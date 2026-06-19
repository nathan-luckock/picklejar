//! The picklejar scorecard: one reproducible page that proves the whole claim.
//!
//! It measures live throughput on the real engine, then runs the full reliability
//! certificate (recall, the five exhaustive model checks, fault detection, and
//! radiation survival) and prints them together. The verification numbers are
//! deterministic and content-hashed, so the same commit always produces the same
//! proof; only the throughput line varies with the machine it ran on.
//!
//! ```text
//! cargo run --release --bin scorecard
//! ```

use std::fmt::Write as _;
use std::process::ExitCode;
use std::time::Instant;

use picklejar::certify::Certificate;
use picklejar::{quantize, Database};

/// Measured engine throughput, in operations per second.
struct Throughput {
    inserts: f64,
    lookups: f64,
    knn: f64,
    rows: usize,
}

/// A tiny deterministic PRNG so the workload (not the timing) replays the same.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        let v = (self.next() >> 40) as f32 / 16_777_216.0;
        v
    }
}

/// A `VECTOR(DIMS)` literal of pseudo-random components, e.g. `[0.12, 0.84, ...]`.
fn vector_literal(rng: &mut Rng, dims: usize) -> String {
    let mut s = String::from("[");
    for d in 0..dims {
        if d > 0 {
            s.push_str(", ");
        }
        let _ = write!(s, "{:.4}", rng.unit());
    }
    s.push(']');
    s
}

/// Drive the real engine and time inserts, point lookups, and indexed
/// nearest-neighbor queries.
// The operation counts are small constants, so the `usize as f64` rate divisions
// are exact; the casts are not a precision risk.
#[allow(clippy::cast_precision_loss)]
fn measure_throughput() -> Throughput {
    const DIMS: usize = 16;
    const ROWS: usize = 3000;
    const LOOKUPS: usize = 2000;
    const KNN: usize = 300;
    const K: usize = 10;

    // A process-unique scratch path under the OS temp directory (binaries cannot
    // use the dev-only tempfile crate).
    let base = std::env::temp_dir().join(format!("pj_scorecard_{}.db", std::process::id()));
    let mut db = Database::open(&base).expect("open");
    db.execute("CREATE TABLE mem (id INT PRIMARY KEY, tenant TEXT, e VECTOR(16))")
        .unwrap();

    let mut rng = Rng(0x5151_C0DE_2026);

    // All three loops run inside one transaction so a single fsync is amortized
    // over the batch, the way a real bulk workload commits. (Per-statement
    // auto-commit fsyncs once per row, the durability worst case, which measures
    // disk latency, not the engine.)

    // Inserts.
    db.execute("BEGIN").unwrap();
    let t = Instant::now();
    for id in 0..ROWS {
        let v = vector_literal(&mut rng, DIMS);
        db.execute(&format!(
            "INSERT INTO mem VALUES ({id}, 't{}', '{v}')",
            id % 8
        ))
        .unwrap();
    }
    let inserts = ROWS as f64 / t.elapsed().as_secs_f64();
    db.execute("COMMIT").unwrap();

    // Point lookups by primary key.
    db.execute("BEGIN").unwrap();
    let t = Instant::now();
    for _ in 0..LOOKUPS {
        let id = usize::try_from(rng.next() % ROWS as u64).expect("in range");
        db.execute(&format!("SELECT tenant FROM mem WHERE id = {id}"))
            .unwrap();
    }
    let lookups = LOOKUPS as f64 / t.elapsed().as_secs_f64();
    db.execute("COMMIT").unwrap();

    // Indexed nearest-neighbor queries through the HNSW path. The warm-up query
    // must run inside the transaction: BEGIN (a non-read statement) clears the
    // index cache, so warming before it would be undone and the first timed query
    // would pay to rebuild the HNSW graph.
    db.execute("SET vector_index = on").unwrap();
    db.execute("BEGIN").unwrap();
    let warm = vector_literal(&mut rng, DIMS);
    let _ = db.execute(&format!(
        "SELECT id FROM mem ORDER BY e <-> '{warm}' LIMIT {K}"
    ));
    let t = Instant::now();
    for _ in 0..KNN {
        let q = vector_literal(&mut rng, DIMS);
        db.execute(&format!(
            "SELECT id FROM mem ORDER BY e <-> '{q}' LIMIT {K}"
        ))
        .unwrap();
    }
    let knn = KNN as f64 / t.elapsed().as_secs_f64();
    db.execute("COMMIT").unwrap();

    Throughput {
        inserts,
        lookups,
        knn,
        rows: ROWS,
    }
}

fn main() -> ExitCode {
    println!("\n================ PICKLEJAR SCORECARD ================");
    println!("a from-scratch, Postgres-wire AI-memory engine, proven\n");

    // 1. Live throughput on the real engine.
    let tp = measure_throughput();
    println!("THROUGHPUT (measured live on this machine)");
    println!(
        "  inserts            {:>10.0} rows/sec      durable, one fsync per row",
        tp.inserts
    );
    println!(
        "  point lookups      {:>10.0} queries/sec   by primary key",
        tp.lookups
    );
    println!(
        "  vector KNN (HNSW)  {:>10.0} queries/sec   k=10 over {} embeddings",
        tp.knn, tp.rows
    );

    // 2. Recall under embedding drift (the benchmarked research contribution).
    let drift = quantize::run_drift_benchmark(0x00CE_27A1);
    println!("\nRECALL UNDER DRIFT (4x-compressed index)");
    println!(
        "  drift-adaptive     {:>10.3}  vs static {:.3}  ({} recalibrations)",
        drift.adaptive_recall, drift.static_recall, drift.recalibrations
    );

    // 3. The full reliability certificate: every invariant, content-hashed.
    let cert = Certificate::generate();
    println!("\nPROVEN INVARIANTS (deterministic, content-hashed)");
    for c in &cert.checks {
        let mark = if c.passed { "PASS" } else { "FAIL" };
        println!("  [{mark}] {}", c.name);
    }
    println!("\n  certificate hash {:08x}", cert.content_hash());

    let ok = cert.passed();
    println!("\n----------------------------------------------------");
    if ok {
        println!(
            "VERDICT: {} invariants proven, recall held under drift, \
             throughput measured live. Reproduce from this commit.",
            cert.checks.len()
        );
        println!("====================================================\n");
        ExitCode::SUCCESS
    } else {
        println!("VERDICT: an invariant did not hold. See the FAIL line above.");
        ExitCode::FAILURE
    }
}
