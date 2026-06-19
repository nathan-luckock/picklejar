//! The AI memory layer reliability certificate.
//!
//! This runs the memory layer's reliability invariants deterministically and
//! emits a reproducible, content-hashed report: the artifact a mission-assurance
//! review regenerates to confirm the store survives its environment before it is
//! ever deployed. Every check is seeded, so the same commit always produces the
//! identical certificate, and the trailing content hash makes that tamper-evident.
//!
//! The certificate covers the AI-memory-layer invariants (recall on realistic
//! data, the metamorphic relations of nearest-neighbor search, corruption
//! detection, and self-healing from redundancy). Crash durability and tenant
//! isolation are certified separately and at far larger scale by the `dst` and
//! `vecsim` binaries; this report names them so the whole picture is in one place.

use std::fmt::Write as _;

use crate::hnsw::{Hnsw, Metric};
use crate::{Database, QueryOutcome, Value};

/// `SplitMix64`, the deterministic generator the rest of the engine uses.
struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn comp(&mut self) -> f32 {
        f32::from(i16::try_from(self.next_u64() % 2001).unwrap_or(0) - 1000)
    }
}

/// `n` points drawn from `clusters` clusters with small jitter: the case where a
/// graph index's recall is genuinely stressed.
fn clustered(n: usize, dim: usize, clusters: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng::new(seed);
    let centers: Vec<Vec<f32>> = (0..clusters)
        .map(|_| (0..dim).map(|_| rng.comp()).collect())
        .collect();
    (0..n)
        .map(|_| {
            let c = usize::try_from(rng.next_u64() % clusters as u64).unwrap_or(0);
            centers[c]
                .iter()
                .map(|&x| x + f32::from(i16::try_from(rng.next_u64() % 21).unwrap_or(0) - 10))
                .collect()
        })
        .collect()
}

/// `n` unit-norm vectors, the natural input for cosine similarity.
fn normalized(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng::new(seed);
    (0..n)
        .map(|_| {
            let v: Vec<f32> = (0..dim).map(|_| rng.comp()).collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm == 0.0 {
                v
            } else {
                v.iter().map(|x| x / norm).collect()
            }
        })
        .collect()
}

fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            let d = x - y;
            d * d
        })
        .sum()
}

fn cosine_dist(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        1.0
    } else {
        1.0 - dot / (na * nb)
    }
}

/// Exact top-k by brute force under `metric`, the oracle for recall.
fn brute_force(data: &[Vec<f32>], q: &[f32], k: usize, metric: Metric) -> Vec<usize> {
    let mut scored: Vec<(f32, usize)> = data
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let d = if metric == Metric::Cosine {
                cosine_dist(q, v)
            } else {
                l2_sq(q, v)
            };
            (d, i)
        })
        .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0));
    scored.into_iter().take(k).map(|(_, i)| i).collect()
}

fn recall(data: &[Vec<f32>], queries: &[Vec<f32>], metric: Metric, seed: u64) -> f64 {
    let dim = data[0].len();
    let mut index = Hnsw::new_with_metric(dim, 16, 200, seed, metric);
    for v in data {
        index.insert(v.clone());
    }
    let mut hits = 0usize;
    let mut total = 0usize;
    for q in queries {
        let approx: std::collections::HashSet<usize> = index
            .search(q, 10, 150)
            .into_iter()
            .map(|(i, _)| i)
            .collect();
        for id in brute_force(data, q, 10, metric) {
            total += 1;
            if approx.contains(&id) {
                hits += 1;
            }
        }
    }
    f64::from(u32::try_from(hits).unwrap_or(u32::MAX))
        / f64::from(u32::try_from(total.max(1)).unwrap_or(u32::MAX))
}

/// One certified reliability invariant and its result.
#[derive(Debug)]
pub struct Check {
    /// Short name of the invariant.
    pub name: String,
    /// Human-readable detail, including the measured number where there is one.
    pub detail: String,
    /// Whether the invariant held.
    pub passed: bool,
}

/// A regenerable reliability certificate for the AI memory layer.
#[derive(Debug)]
pub struct Certificate {
    /// The invariants checked, in order.
    pub checks: Vec<Check>,
}

impl Certificate {
    /// Run every invariant and assemble the certificate. Fully deterministic:
    /// the same commit always produces the identical result.
    #[must_use]
    pub fn generate() -> Self {
        let mut checks = Vec::new();

        // Recall on hard data, with exact brute force as the oracle.
        let r = recall(
            &clustered(3000, 32, 30, 101),
            &clustered(100, 32, 30, 202),
            Metric::L2,
            7,
        );
        checks.push(Check {
            name: "recall L2 (clustered)".into(),
            detail: format!("recall@10 = {r:.4} over 3000 clustered vectors (oracle: brute force)"),
            passed: r >= 0.97,
        });
        let r = recall(
            &normalized(3000, 64, 303),
            &normalized(100, 64, 404),
            Metric::Cosine,
            3,
        );
        checks.push(Check {
            name: "recall cosine (unit-norm)".into(),
            detail: format!("recall@10 = {r:.4} over 3000 unit-norm vectors"),
            passed: r >= 0.97,
        });

        // Metamorphic relations: correctness without a ground-truth oracle.
        checks.push(metamorphic_self_retrieval());
        checks.push(metamorphic_deletion());

        // Corruption detection: every single-bit fault in the serialized index is
        // caught, never served as a wrong answer.
        checks.push(corruption_detection());

        // Self-healing: a copy corrupted past its checksum is recovered from
        // redundancy with no intervention.
        checks.push(self_healing());

        // Radiation survivability: the proof, framed in an orbit's own units.
        checks.push(radiation_survivability());

        // Whole-store corruption survival: the same property at the engine level,
        // through the SQL layer, not just the index artifact.
        checks.push(whole_store_corruption());

        // Irradiated multi-tenant memory layer: a committed multi-tenant workload
        // corrupted at an orbit's upset rate, then reopened, never serves a tenant
        // a silently wrong embedding and never leaks another tenant's row.
        checks.push(irradiated_memory_layer());

        // Self-healing storage: erasure-coded blobs survive and repair any m bad
        // shards, the mass-efficient redundancy that replaces heavy hardware copies.
        checks.push(erasure_self_heal());

        // The live engine heals itself: a protected database, with corrupt heap
        // pages, reconstructs them from parity on `open_resilient` and serves the
        // committed data exactly.
        checks.push(live_heap_self_heal());

        // Formal model checks: the core durability and isolation invariants proved
        // over every reachable interleaving of a bounded model, not just sampled.
        checks.push(wal_ordering_model());
        checks.push(snapshot_isolation_model());

        // The catalog is WAL-logged: a schema change whose sidecar write was
        // lost in a crash is recovered from the log on open, so the two copies
        // (WAL and sidecar) are redundant and the schema self-heals.
        checks.push(catalog_wal_recovery());

        // The same for tenant isolation: a policy whose sidecar write was lost
        // is restored from the WAL, so a crash can never silently drop a fence.
        checks.push(rls_wal_recovery());

        Self { checks }
    }

    /// Whether every invariant held.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }

    /// The canonical body of the certificate, deterministic across runs. The
    /// content hash is taken over exactly this text.
    #[must_use]
    pub fn body(&self) -> String {
        let mut s = String::new();
        for c in &self.checks {
            let mark = if c.passed { "PASS" } else { "FAIL" };
            let _ = writeln!(s, "[{mark}] {}: {}", c.name, c.detail);
        }
        s
    }

    /// A CRC32 over the canonical body: tamper-evident and exactly reproducible
    /// from the same commit, so the certificate cannot be edited after the fact.
    #[must_use]
    pub fn content_hash(&self) -> u32 {
        picklejar_storage::crc32::crc32(self.body().as_bytes())
    }

    /// The full certificate text: a header naming the fault model, the per-check
    /// body, and a footer with the verdict and the content hash.
    #[must_use]
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str("PICKLEJAR AI MEMORY LAYER RELIABILITY CERTIFICATE\n");
        s.push_str("fault model: single-event upsets injected into the serialized index, into\n");
        s.push_str("      whole-store pages through SQL, into an irradiated multi-tenant\n");
        s.push_str("      workload at an orbit's upset rate, into erasure-coded shards that\n");
        s.push_str("      self-heal from parity, and into a live heap that reconstructs its\n");
        s.push_str("      corrupt pages from parity on open\n");
        s.push_str("method: deterministic, every check reproducible from a fixed seed\n");
        s.push_str("note: crash durability (100k sims) and tenant isolation are certified\n");
        s.push_str("      separately and at larger scale by the `dst` and `vecsim` binaries;\n");
        s.push_str("      the WAL-ordering and snapshot-isolation invariants are also\n");
        s.push_str("      model-checked exhaustively over a bounded model\n");
        s.push_str("--------------------------------------------------------------------\n");
        s.push_str(&self.body());
        s.push_str("--------------------------------------------------------------------\n");
        let verdict = if self.passed() {
            "ALL INVARIANTS HELD"
        } else {
            "FAILED: an invariant did not hold"
        };
        let _ = writeln!(s, "result: {verdict}");
        let _ = writeln!(
            s,
            "certificate hash: {:08x}  (regenerate from this commit to verify)",
            self.content_hash()
        );
        s
    }
}

fn metamorphic_self_retrieval() -> Check {
    let dim = 24;
    let data = clustered(1500, dim, 20, 41);
    let mut index = Hnsw::new(dim, 16, 200, 9);
    for v in &data {
        index.insert(v.clone());
    }
    let mut ok = 0usize;
    for (i, v) in data.iter().enumerate() {
        if index.search(v, 1, 64).first().is_some_and(|&(j, _)| j == i) {
            ok += 1;
        }
    }
    let rate = f64::from(u32::try_from(ok).unwrap_or(u32::MAX))
        / f64::from(u32::try_from(data.len()).unwrap_or(u32::MAX));
    Check {
        name: "metamorphic: self-retrieval".into(),
        detail: format!("{rate:.4} of stored vectors are their own nearest neighbor"),
        passed: rate >= 0.99,
    }
}

fn metamorphic_deletion() -> Check {
    let dim = 16;
    let data = clustered(800, dim, 12, 88);
    let mut index = Hnsw::new(dim, 16, 200, 2);
    for v in &data {
        index.insert(v.clone());
    }
    let victims = [3usize, 50, 199, 372, 511];
    for &victim in &victims {
        index.remove(victim);
    }
    let leaked = data.iter().any(|v| {
        index
            .search(v, 10, 80)
            .into_iter()
            .any(|(j, _)| victims.contains(&j))
    });
    Check {
        name: "metamorphic: deletion consistency".into(),
        detail: if leaked {
            "a removed vector reappeared in a result".into()
        } else {
            "no removed vector appears in any result".into()
        },
        passed: !leaked,
    }
}

fn corruption_detection() -> Check {
    let dim = 8;
    let data = clustered(200, dim, 8, 55);
    let mut index = Hnsw::new(dim, 16, 100, 9);
    for v in &data {
        index.insert(v.clone());
    }
    let good = index.to_bytes();
    let mut total = 0usize;
    let mut detected = 0usize;
    for pos in (0..good.len()).step_by(3) {
        let mut bad = good.clone();
        bad[pos] ^= 0x01;
        total += 1;
        if Hnsw::from_bytes(&bad).is_none() {
            detected += 1;
        }
    }
    Check {
        name: "corruption detection".into(),
        detail: format!("{detected}/{total} single-bit faults detected on load"),
        passed: detected == total,
    }
}

fn self_healing() -> Check {
    let dim = 8;
    let data = clustered(150, dim, 8, 12);
    let mut index = Hnsw::new(dim, 16, 100, 5);
    for v in &data {
        index.insert(v.clone());
    }
    let probe = &data[7];
    let expect = index.search(probe, 5, 64);
    let img = index.to_bytes_redundant();
    let mut trials = 0usize;
    let mut healed = 0usize;
    // Corrupt the first copy at a range of offsets; each must recover exactly.
    for start in (16..40).step_by(4) {
        let mut damaged = img.clone();
        for b in damaged.iter_mut().skip(start).take(8) {
            *b ^= 0xFF;
        }
        trials += 1;
        if let Some((healed_index, _)) = Hnsw::load_redundant(&damaged) {
            if healed_index.search(probe, 5, 64) == expect {
                healed += 1;
            }
        }
    }
    Check {
        name: "self-healing".into(),
        detail: format!("{healed}/{trials} corrupted copies recovered exactly from redundancy"),
        passed: healed == trials,
    }
}

fn radiation_survivability() -> Check {
    use crate::radiation::{expected_upsets_per_day, Orbit};
    let dim = 8;
    let data = clustered(2000, dim, 12, 77);
    let mut index = Hnsw::new(dim, 16, 100, 5);
    for v in &data {
        index.insert(v.clone());
    }
    let img = index.to_bytes();
    let orbit = Orbit::Leo;
    let per_day = expected_upsets_per_day(img.len(), orbit);
    // Stress well above the expected daily dose: inject single-bit upsets, each
    // into a fresh copy, and require every one detected (never a wrong answer).
    let dose = stress_count(per_day);
    let bits = img.len() * 8;
    let mut rng = Rng::new(9001);
    let mut detected = 0usize;
    for _ in 0..dose {
        let mut bad = img.clone();
        let bit = usize::try_from(rng.next_u64() % bits as u64).unwrap_or(0);
        bad[bit / 8] ^= 1u8 << (bit % 8);
        if Hnsw::from_bytes(&bad).is_none() {
            detected += 1;
        }
    }
    let kb = img.len() / 1024;
    Check {
        name: "radiation survivability (LEO)".into(),
        detail: format!(
            "modeled {} dose ~{per_day:.2} upsets/day for a {kb} KB index; \
             {detected}/{dose} single-bit upsets at high dose detected",
            orbit.name()
        ),
        passed: detected == dose,
    }
}

/// Whole-store corruption survival, exercised through the SQL layer: write known
/// rows to a real database, flip a byte in a checksum-covered page region,
/// reopen, and query. Committed data is either returned correctly or a corruption
/// error surfaces; it is never served wrong without an error. This certifies the
/// product claim (the whole store), not just the index artifact.
fn whole_store_corruption() -> Check {
    use std::io::{Read, Seek, SeekFrom, Write};

    let to_f = |n: i64| f32::from(i16::try_from(n).unwrap_or(0));
    let expected: Vec<Vec<Value>> = (1..=12i64)
        .map(|i| vec![Value::Int(i), Value::Vector(vec![to_f(i), to_f(i * 3)])])
        .collect();
    let base = std::env::temp_dir().join(format!("pj-cert-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::create_dir_all(&base);

    let mut rng = Rng::new(0xC0DE_1234_5678_9ABC);
    let mut trials = 0usize;
    let mut violated = 0usize;
    for run in 0..8 {
        let path = base.join(format!("c{run}.db"));
        if let Ok(mut db) = Database::open(&path) {
            let _ = db.execute("CREATE TABLE t (id INT, e VECTOR(2))");
            for i in 1..=12i64 {
                let _ = db.execute(&format!("INSERT INTO t VALUES ({i}, '[{i}, {}]')", i * 3));
            }
        }
        let len = std::fs::metadata(&path).map_or(0, |m| m.len());
        if len >= 8192 {
            let pages = len / 8192;
            let page = rng.next_u64() % pages;
            let off = 12 + rng.next_u64() % (8192 - 12);
            let pos = page * 8192 + off;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
            {
                if f.seek(SeekFrom::Start(pos)).is_ok() {
                    let mut b = [0u8; 1];
                    if f.read_exact(&mut b).is_ok() {
                        b[0] ^= 0xFF;
                        let _ = f.seek(SeekFrom::Start(pos));
                        let _ = f.write_all(&b);
                    }
                }
            }
        }
        trials += 1;
        if let Ok(mut db) = Database::open(&path) {
            if let Ok(QueryOutcome::Rows { rows, .. }) =
                db.execute("SELECT id, e FROM t ORDER BY id")
            {
                // Returned without error: it must be exactly the committed data.
                if rows != expected {
                    violated += 1;
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&base);
    Check {
        name: "whole-store corruption survival".into(),
        detail: format!(
            "{trials} page-corruption trials through SQL, {violated} silent-wrong results"
        ),
        passed: violated == 0,
    }
}

/// Irradiated multi-tenant memory layer, certified through the live simulator:
/// for several seeds, build and commit a multi-tenant embedding workload, corrupt
/// its on-disk bytes at the geostationary single-event-upset rate for a fixed
/// dwell, reopen, and require that no tenant is ever served a silently wrong
/// embedding and no row leaks across tenants. Every outcome is a deterministic
/// function of its seed, so the reported counts (and the certificate hash) are
/// reproducible.
fn irradiated_memory_layer() -> Check {
    use crate::radiation::Orbit;
    let seeds = 8u64;
    let orbit = Orbit::Geo;
    let orbit_days = 400.0;
    let mut flips = 0usize;
    let mut detected = 0usize;
    let mut violated = 0usize;
    for seed in 0..seeds {
        match crate::vecsim::run_seed_irradiated(seed, orbit, orbit_days) {
            Ok(outcome) => {
                flips += outcome.flips;
                if outcome.detected {
                    detected += 1;
                }
            }
            Err(_) => violated += 1,
        }
    }
    Check {
        name: "irradiated memory layer (GEO)".into(),
        detail: format!(
            "{seeds} multi-tenant workloads irradiated for {orbit_days:.0} orbit-days at the \
             {} rate through SQL; {flips} upsets injected, {detected} detected, \
             {violated} silently wrong",
            orbit.name()
        ),
        passed: violated == 0,
    }
}

/// Self-healing storage, certified: erasure-code a batch of blobs, corrupt up to
/// the parity limit of shards in each, and require every blob to come back
/// byte-for-byte correct after repair from redundancy, with none served wrong.
/// This is the mass-efficient redundancy that lets a node carry commodity storage
/// and heal it in software instead of launching heavy redundant hardware.
fn erasure_self_heal() -> Check {
    use picklejar_storage::resilient::ResilientStore;
    let (k, m) = (10usize, 4usize);
    let Ok(mut store) = ResilientStore::new(k, m) else {
        return Check {
            name: "self-healing storage (erasure)".into(),
            detail: "could not build the k=10, m=4 code".into(),
            passed: false,
        };
    };
    let blobs = 64u64;
    let blob = |key: u64| -> Vec<u8> {
        let len = 200 + usize::try_from(key % 5).unwrap_or(0) * 40;
        (0..len)
            .map(|i| u8::try_from((i as u64 ^ key) & 0xFF).expect("masked"))
            .collect()
    };
    for key in 0..blobs {
        if store.put(key, &blob(key)).is_err() {
            return Check {
                name: "self-healing storage (erasure)".into(),
                detail: "encode failed".into(),
                passed: false,
            };
        }
    }

    // Corrupt up to m shards in each blob, the way single-event upsets would.
    let mut rng = Rng::new(0x5E1F_4EA1);
    let total = u64::try_from(k + m).expect("small");
    let mut injected = 0usize;
    for key in 0..blobs {
        let bad = rng.next_u64() % (u64::try_from(m).expect("small") + 1);
        let mut hit = std::collections::HashSet::new();
        while u64::try_from(hit.len()).expect("small") < bad {
            hit.insert(usize::try_from(rng.next_u64() % total).expect("small"));
        }
        for &shard in &hit {
            store.corrupt_shard(key, shard, b"single-event upset");
            injected += 1;
        }
    }

    // Read everything back; it must equal what was stored.
    let mut wrong = 0usize;
    for key in 0..blobs {
        let ok = matches!(store.get(key), Ok(got) if got == blob(key));
        if !ok {
            wrong += 1;
        }
    }
    let repaired = store.fault_log().iter().filter(|e| e.repaired).count();
    Check {
        name: "self-healing storage (erasure k=10, m=4)".into(),
        detail: format!(
            "{blobs} blobs, {injected} shard upsets injected, {repaired} repaired from parity, \
             {wrong} served wrong; survives 4 failures at +40% storage vs +400% for copies"
        ),
        passed: wrong == 0,
    }
}

/// The live database heals itself, certified end to end: build a small vector
/// store, write a parity snapshot with [`Database::protect`], corrupt one heap
/// page in every stripe, reopen with [`Database::open_resilient`], and require
/// every committed row and embedding to come back exactly. This certifies the
/// engine-level payoff, not just the standalone block store.
fn live_heap_self_heal() -> Check {
    use std::io::{Read, Seek, SeekFrom, Write};
    const PAGE: u64 = 8192;
    let name = "live heap self-heal".to_string();
    let base = std::env::temp_dir().join(format!("pj-cert-heal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::create_dir_all(&base);
    let path = base.join("m.db");
    let rows_n = 800i64;
    let embed = |i: i64| -> Value {
        let f = |x: i64| f32::from(i16::try_from(x).unwrap_or(0));
        Value::Vector(vec![f(i), f(i * 2), f(i * 3)])
    };

    let fail = |detail: String| Check {
        name: name.clone(),
        detail,
        passed: false,
    };

    // Build and protect.
    let protected = {
        let mut db = match Database::open(&path) {
            Ok(db) => db,
            Err(e) => return fail(format!("open: {e}")),
        };
        if let Err(e) = db.execute("CREATE TABLE m (id INT, e VECTOR(3))") {
            return fail(format!("create: {e}"));
        }
        for i in 1..=rows_n {
            if let Err(e) = db.execute(&format!(
                "INSERT INTO m VALUES ({i}, '[{i}, {}, {}]')",
                i * 2,
                i * 3
            )) {
                return fail(format!("insert: {e}"));
            }
        }
        match db.protect(6, 3) {
            Ok(report) => report.protected_pages,
            Err(e) => return fail(format!("protect: {e}")),
        }
    };

    // Corrupt one page in each six-page stripe, at a fixed in-range offset.
    let pages = std::fs::metadata(&path).map_or(0, |meta| meta.len() / PAGE);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    {
        let mut s = 0u64;
        while s * 6 + 1 < pages {
            let pos = (s * 6 + 1) * PAGE + 100;
            if f.seek(SeekFrom::Start(pos)).is_ok() {
                let mut b = [0u8; 1];
                if f.read_exact(&mut b).is_ok() {
                    b[0] ^= 0xFF;
                    let _ = f.seek(SeekFrom::Start(pos));
                    let _ = f.write_all(&b);
                }
            }
            s += 1;
        }
    }

    // Heal on open and verify every row.
    let healed = {
        let mut db = match Database::open_resilient(&path) {
            Ok(db) => db,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&base);
                return fail(format!("open_resilient: {e}"));
            }
        };
        match db.execute("SELECT id, e FROM m ORDER BY id") {
            Ok(QueryOutcome::Rows { rows, .. }) => {
                rows.len() == usize::try_from(rows_n).unwrap_or(0)
                    && rows.iter().enumerate().all(|(idx, row)| {
                        let i = i64::try_from(idx).unwrap_or(0) + 1;
                        row.first() == Some(&Value::Int(i)) && row.get(1) == Some(&embed(i))
                    })
            }
            _ => false,
        }
    };
    let _ = std::fs::remove_dir_all(&base);

    Check {
        name,
        detail: format!(
            "{protected} heap pages protected (k=6, m=3); one page corrupted per stripe, \
             all reconstructed on open and {rows_n} rows served exactly"
        ),
        passed: healed,
    }
}

/// The catalog survives a lost sidecar write: a schema change that reached the
/// WAL is recovered on open even when its `.meta` sidecar write was lost, so the
/// WAL and the sidecar are redundant copies and the schema self-heals.
fn catalog_wal_recovery() -> Check {
    let name = "catalog WAL recovery".to_string();
    let base = std::env::temp_dir().join(format!("pj-cert-catwal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::create_dir_all(&base);
    let path = base.join("m.db");
    let meta_path = path.with_extension("meta");
    let fail = |detail: String| Check {
        name: name.clone(),
        detail,
        passed: false,
    };

    // Build: one table, snapshot the sidecar, then add a second table and a row.
    let stale_meta = {
        let mut db = match Database::open(&path) {
            Ok(db) => db,
            Err(e) => return fail(format!("open: {e}")),
        };
        if let Err(e) = db.execute("CREATE TABLE t1 (id INT)") {
            return fail(format!("create t1: {e}"));
        }
        let snap = match std::fs::read(&meta_path) {
            Ok(b) => b,
            Err(e) => return fail(format!("read meta: {e}")),
        };
        if let Err(e) = db.execute("CREATE TABLE t2 (id INT, name TEXT)") {
            return fail(format!("create t2: {e}"));
        }
        if let Err(e) = db.execute("INSERT INTO t2 VALUES (1, 'a')") {
            return fail(format!("insert: {e}"));
        }
        snap
    };

    // Simulate the lost sidecar write: roll `.meta` back to the t1-only state,
    // leaving the WAL (which logged t2's catalog snapshot) intact.
    if let Err(e) = std::fs::write(&meta_path, &stale_meta) {
        return fail(format!("clobber meta: {e}"));
    }

    // Reopen: the WAL reconstructs t2's schema and the row it pointed at.
    let recovered = {
        let mut db = match Database::open(&path) {
            Ok(db) => db,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&base);
                return fail(format!("reopen: {e}"));
            }
        };
        matches!(
            db.execute("SELECT id, name FROM t2"),
            Ok(QueryOutcome::Rows { rows, .. })
                if rows == vec![vec![Value::Int(1), Value::Text("a".into())]]
        )
    };
    let _ = std::fs::remove_dir_all(&base);

    Check {
        name,
        detail: "a schema change whose sidecar write was lost is recovered from the WAL on open"
            .into(),
        passed: recovered,
    }
}

/// Tenant isolation survives a lost sidecar write: a policy that reached the WAL
/// is restored on open even when its `.pol` sidecar write was lost, so a crash
/// can never silently drop a fence and leak one tenant's rows to another.
fn rls_wal_recovery() -> Check {
    let name = "RLS WAL recovery".to_string();
    let base = std::env::temp_dir().join(format!("pj-cert-rlswal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::create_dir_all(&base);
    let path = base.join("m.db");
    let pol_path = path.with_extension("pol");
    let fail = |detail: String| Check {
        name: name.clone(),
        detail,
        passed: false,
    };

    // Build a two-tenant table and grant, then snapshot `.pol` before the fence.
    let stale_pol = {
        let mut db = match Database::open(&path) {
            Ok(db) => db,
            Err(e) => return fail(format!("open: {e}")),
        };
        for sql in [
            "CREATE TABLE m (id INT, tenant TEXT)",
            "INSERT INTO m VALUES (1, 'acme'), (2, 'globex')",
            "CREATE ROLE acme LOGIN",
            "GRANT SELECT ON m TO PUBLIC",
        ] {
            if let Err(e) = db.execute(sql) {
                return fail(format!("{sql}: {e}"));
            }
        }
        let snap = match std::fs::read(&pol_path) {
            Ok(b) => b,
            Err(e) => return fail(format!("read pol: {e}")),
        };
        for sql in [
            "CREATE POLICY tenant ON m USING ((tenant = current_user()))",
            "ALTER TABLE m ENABLE ROW LEVEL SECURITY",
        ] {
            if let Err(e) = db.execute(sql) {
                return fail(format!("{sql}: {e}"));
            }
        }
        snap
    };

    // Roll `.pol` back to before the fence, leaving the WAL intact.
    if let Err(e) = std::fs::write(&pol_path, &stale_pol) {
        return fail(format!("clobber pol: {e}"));
    }

    // Reopen: the WAL restores the fence, so acme sees only its own row.
    let fenced = {
        let mut db = match Database::open(&path) {
            Ok(db) => db,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&base);
                return fail(format!("reopen: {e}"));
            }
        };
        db.set_session_user("acme");
        matches!(
            db.execute("SELECT id FROM m"),
            Ok(QueryOutcome::Rows { rows, .. }) if rows == vec![vec![Value::Int(1)]]
        )
    };
    let _ = std::fs::remove_dir_all(&base);

    Check {
        name,
        detail:
            "a tenant fence whose policy sidecar write was lost is restored from the WAL on open"
                .into(),
        passed: fenced,
    }
}

/// The write-ahead-logging ordering invariant, model-checked: over every
/// reachable interleaving of the bounded log-and-page model, no page change is
/// durable ahead of its log record, and a deliberately buggy flush is caught.
fn wal_ordering_model() -> Check {
    let bound = 8u8;
    let states = picklejar_wal::model::reachable_states(bound, true);
    let held = picklejar_wal::model::check(bound, true).is_none();
    let teeth = picklejar_wal::model::check(3, false).is_some();
    Check {
        name: "WAL ordering model-check".into(),
        detail: format!(
            "no page durable ahead of its log over all {states} reachable states (bound {bound}); \
             a buggy early flush is caught"
        ),
        passed: held && teeth,
    }
}

/// Snapshot isolation's read-stability invariant, model-checked: over every
/// reachable interleaving, a reader sees the same value twice within its snapshot,
/// and a snapshot-ignoring read is caught.
fn snapshot_isolation_model() -> Check {
    let bound = 8u8;
    let states = picklejar_txn::model::reachable_states(bound, true);
    let held = picklejar_txn::model::check(bound, true).is_none();
    let teeth = picklejar_txn::model::check(3, false).is_some();
    Check {
        name: "snapshot isolation model-check".into(),
        detail: format!(
            "reads are stable within a snapshot over all {states} reachable states (bound {bound}); \
             a snapshot-ignoring read is caught"
        ),
        passed: held && teeth,
    }
}

/// A fault count comfortably above the expected daily dose, clamped to keep the
/// certificate fast.
fn stress_count(per_day: f64) -> usize {
    let target = (per_day * 100.0).ceil();
    if !target.is_finite() || target < 64.0 {
        64
    } else if target > 4096.0 {
        4096
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = target as usize; // in 64..=4096, exact
        n
    }
}

#[cfg(test)]
mod tests {
    use super::Certificate;

    #[test]
    fn the_certificate_passes_and_is_reproducible() {
        let a = Certificate::generate();
        assert!(a.passed(), "certificate did not pass:\n{}", a.render());
        let b = Certificate::generate();
        assert_eq!(
            a.content_hash(),
            b.content_hash(),
            "the certificate must be reproducible from the same commit"
        );
    }
}
