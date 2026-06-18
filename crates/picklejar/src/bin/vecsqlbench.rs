//! End-to-end benchmark of the cached SQL index path: how much faster is a
//! repeated `ORDER BY col <-> :q LIMIT k` once the engine's HNSW index is warm,
//! compared with the exact scan the same query runs without the index.
//!
//! ```text
//! cargo run --release --bin vecsqlbench                 # 2000 rows, dim 64
//! cargo run --release --bin vecsqlbench -- 20000 128    # 20k rows, dim 128
//! ```
//!
//! Reports the exact-scan latency, the cold first query (which builds and caches
//! the index), the warm cached-query latency, the speedup, and how often the warm
//! top result matched the exact one. Timing is wall-clock; run it in release on an
//! idle machine for meaningful numbers.

use std::time::Instant;

use picklejar::{Database, QueryOutcome, Value};

/// A small deterministic PRNG so a run is reproducible across machines.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A pseudo-random `f32` in `[-1, 1)`.
    fn unit(&mut self) -> f32 {
        let bits = self.next_u64() >> 40; // 24 bits
        #[allow(clippy::cast_precision_loss)]
        let frac = bits as f32 / f32::from(1u16 << 12) / f32::from(1u16 << 12);
        frac.mul_add(2.0, -1.0)
    }
}

/// Format a vector as the SQL literal the engine parses, with enough precision to
/// round-trip the timing-relevant values.
fn literal(v: &[f32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::from("[");
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        let _ = write!(s, "{x:.6}");
    }
    s.push(']');
    s
}

/// Run `SELECT id ... ORDER BY e <-> :q LIMIT k` and return the first id, or -1
/// for an empty result.
fn top_id(db: &mut Database, query: &[f32], k: usize) -> i64 {
    let sql = format!(
        "SELECT id FROM items ORDER BY e <-> '{}' LIMIT {k}",
        literal(query)
    );
    match db.execute(&sql).expect("query") {
        QueryOutcome::Rows { rows, .. } => match rows.first().and_then(|r| r.first()) {
            Some(Value::Int(n)) => *n,
            _ => -1,
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2000);
    let dim: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(64);
    let queries = 200usize;
    let k = 10usize;

    let dir = std::env::temp_dir().join(format!("pj-vecsqlbench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    let path = dir.join("bench.db");
    let mut db = Database::open(&path).expect("open");

    println!("loading {n} rows of dim {dim} through the engine...");
    db.execute(&format!("CREATE TABLE items (id INT, e VECTOR({dim}))"))
        .expect("create");
    let mut rng = Rng(1);
    for i in 0..n {
        let v: Vec<f32> = (0..dim).map(|_| rng.unit()).collect();
        db.execute(&format!(
            "INSERT INTO items VALUES ({i}, '{}')",
            literal(&v)
        ))
        .expect("insert");
    }
    let probes: Vec<Vec<f32>> = (0..queries)
        .map(|_| (0..dim).map(|_| rng.unit()).collect())
        .collect();

    // Exact scan: the index path off, every query a full brute-force pass.
    db.set_vector_index(false);
    let exact_start = Instant::now();
    let exact_ids: Vec<i64> = probes.iter().map(|q| top_id(&mut db, q, k)).collect();
    let exact = exact_start.elapsed();

    // Cached index path on. The first query is cold: it builds and caches the
    // index. Every query after is warm and reuses it.
    db.set_vector_index(true);
    let cold_start = Instant::now();
    let _ = top_id(&mut db, &probes[0], k);
    let cold = cold_start.elapsed();

    let warm_start = Instant::now();
    let mut matched = 0usize;
    for (i, q) in probes.iter().enumerate() {
        let id = top_id(&mut db, q, k);
        if exact_ids.get(i) == Some(&id) {
            matched += 1;
        }
    }
    let warm = warm_start.elapsed();

    let _ = std::fs::remove_dir_all(&dir);

    let exact_us = avg_us(exact, queries);
    let warm_us = avg_us(warm, queries);
    let speedup = exact.as_secs_f64() / warm.as_secs_f64().max(f64::MIN_POSITIVE);
    println!("exact scan:      {exact_us:.1} us/query");
    println!(
        "cold query:      {:.1} ms (builds + caches the index)",
        cold.as_secs_f64() * 1e3
    );
    println!("warm cached:     {warm_us:.1} us/query");
    println!("speedup (warm):  {speedup:.1}x over the exact scan");
    println!("top-1 agreement: {matched}/{queries} warm queries matched the exact nearest");
}

/// Average microseconds per query for a total `elapsed` over `queries`.
#[allow(clippy::cast_precision_loss)]
fn avg_us(elapsed: std::time::Duration, queries: usize) -> f64 {
    elapsed.as_secs_f64() * 1e6 / queries as f64
}
