//! Engine-level deterministic simulation for the vector memory layer.
//!
//! The storage-level simulator in `picklejar-wal` (the `dst` binary) proves that
//! committed rows survive a crash. This simulator runs one level up, through the
//! real [`Database`], and proves the two properties the AI memory layer must hold
//! *together*:
//!
//! - **Durability.** After a crash and reopen, every committed embedding is
//!   present and byte-for-byte intact, every updated embedding reflects its last
//!   committed value, every deleted embedding is gone, and every rolled-back
//!   change left no trace.
//! - **Isolation.** After recovery, each tenant still sees exactly its own
//!   embeddings and never another tenant's, on both ordinary reads and
//!   nearest-neighbor ranking, enforced by row-level security in the engine
//!   rather than by application code.
//!
//! Every run is driven entirely by one `u64` seed, so any failure replays
//! exactly. The on-disk location is process-unique (it does not affect the
//! simulated logic, only where the bytes live), and is removed when the run ends.
//!
//! The crash model is the engine's real reopen path: the [`Database`] is dropped
//! and reopened, which runs WAL recovery. This is the same model the engine's own
//! in-process recovery tests use. It is complementary to, not a replacement for,
//! the stricter fault-disk model behind the storage-level `dst` simulator.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::hnsw::Metric;
use crate::radiation::{expected_upsets_per_day, Orbit};
use crate::{ast, Database, QueryOutcome, Value};

/// `SplitMix64`: a small, fast, fully deterministic PRNG, seeded once per run.
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

    /// A value in `0..n` (`n` must be non-zero).
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// What one simulated run built and verified.
#[derive(Debug, Clone, Copy)]
pub struct Outcome {
    /// Number of tenants in the run.
    pub tenants: usize,
    /// Committed operations (inserts, updates, and deletes) that had to take
    /// effect.
    pub committed: usize,
    /// Rolled-back operations that had to leave no trace.
    pub rolled_back: usize,
    /// Embeddings still live after the workload, all verified intact and
    /// correctly isolated after the crash.
    pub live: usize,
}

/// The live embedding for each id, per tenant: the oracle of what a correct
/// database must hold after recovery.
type Oracle = Vec<BTreeMap<i64, Vec<f32>>>;

/// The oracle effect of one committed operation.
enum Effect {
    /// Insert or update an id to a new embedding.
    Set(i64, Vec<f32>),
    /// Remove an id.
    Remove(i64),
}

/// Run one seeded crash-and-recover simulation of the vector memory layer.
///
/// Returns the verified [`Outcome`], or an error string naming the first
/// violated invariant (a missing, altered, or resurrected embedding, or a tenant
/// seeing a row that is not its own).
///
/// # Errors
///
/// Returns `Err` if the temporary working directory cannot be created, if a
/// setup statement fails, or if any durability or isolation invariant is
/// violated after recovery.
pub fn run_seed(seed: u64) -> Result<Outcome, String> {
    let dir = std::env::temp_dir().join(format!("pj-vecsim-{}-{seed}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create_dir_all: {e}"))?;
    let result = simulate(seed, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// What one irradiated run injected and observed.
#[derive(Debug, Clone, Copy)]
pub struct IrradiatedOutcome {
    /// The orbit whose single-event-upset rate set the dose.
    pub orbit: Orbit,
    /// The simulated dwell time in orbit, in days.
    pub orbit_days: f64,
    /// The size of the irradiated artifact (the on-disk database file).
    pub bytes: usize,
    /// The number of bit flips actually injected (a seeded draw whose expected
    /// value is the orbit dose over `orbit_days`).
    pub flips: usize,
    /// Whether the engine detected the corruption (an error surfaced on reopen or
    /// on a query) rather than the flips landing where they changed no answer.
    pub detected: bool,
    /// Live embeddings the run was built around.
    pub live: usize,
}

/// Run one seeded workload, then irradiate its on-disk bytes and reopen.
///
/// The bytes are corrupted at the single-event upset rate of `orbit` for
/// `orbit_days` of dwell time, then the store is reopened and checked against the
/// one invariant that matters in an unreachable environment: the engine either
/// detects the corruption or it changed no committed answer, but it never serves
/// a tenant a silently wrong embedding and never leaks another tenant's row.
///
/// The dose is drawn from the orbit model with expected value
/// `expected_upsets_per_day(bytes, orbit) * orbit_days`, so a run injects the
/// radiation the artifact would actually accumulate, not an arbitrary fault count.
/// Only the heap file is irradiated (the durable served surface); WAL-stream
/// corruption is a separate model. Every run replays exactly from `seed`.
///
/// # Errors
///
/// Returns `Err` if the working directory cannot be created, if a setup statement
/// fails, or if the irradiated engine served data that differs from what was
/// committed without raising an error (a silent-corruption invariant violation).
pub fn run_seed_irradiated(
    seed: u64,
    orbit: Orbit,
    orbit_days: f64,
) -> Result<IrradiatedOutcome, String> {
    // A process-unique suffix so concurrent runs of the same seed never share a
    // directory. The location does not affect the simulated logic, only where the
    // bytes live, so this keeps each run reproducible while making it isolated.
    static RUN_SEQ: AtomicU64 = AtomicU64::new(0);
    let uniq = RUN_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("pj-vecrad-{}-{seed}-{uniq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create_dir_all: {e}"))?;
    let result = simulate_irradiated(seed, &dir, orbit, orbit_days);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// The slotted-page size; flips land strictly inside the checksum-covered region
/// of a page (past the header) so a corrupted heap byte is always detectable.
const PAGE: u64 = 8192;
/// Bytes at the start of each page that the page checksum does not cover.
const HEADER: u64 = 12;

fn simulate_irradiated(
    seed: u64,
    dir: &Path,
    orbit: Orbit,
    orbit_days: f64,
) -> Result<IrradiatedOutcome, String> {
    let workload = build_workload(seed, dir)?;
    let live: usize = workload.live.iter().map(BTreeMap::len).sum();

    let bytes = usize::try_from(
        std::fs::metadata(&workload.path)
            .map_err(|e| format!("metadata: {e}"))?
            .len(),
    )
    .unwrap_or(usize::MAX);

    // Draw the number of upsets with expected value equal to the orbit dose. A
    // dedicated RNG keeps the draw independent of the workload's own RNG stream.
    let dose = expected_upsets_per_day(bytes, orbit) * orbit_days;
    let mut rng = Rng::new(seed ^ 0x52AD_1A71_0000_0001);
    let flips = draw_dose(dose, &mut rng);
    inject_flips(&workload.path, flips, &mut rng).map_err(|e| format!("inject: {e}"))?;

    let detected = verify_irradiated(seed, &workload)?;
    Ok(IrradiatedOutcome {
        orbit,
        orbit_days,
        bytes,
        flips,
        detected,
        live,
    })
}

/// A seeded draw with expected value `dose`: the integer part, plus one more with
/// probability equal to the fractional part. Over a sweep this lands the true
/// expected dose without ever injecting a non-integer number of flips.
fn draw_dose(dose: f64, rng: &mut Rng) -> usize {
    let whole = dose.floor();
    let frac = dose - whole;
    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    #[allow(clippy::cast_possible_truncation)]
    let mut flips = whole.max(0.0) as usize;
    // u64 -> [0,1) without bias that matters at this resolution.
    #[allow(clippy::cast_precision_loss)]
    let roll = rng.next_u64() as f64 / u64::MAX as f64;
    if roll < frac {
        flips += 1;
    }
    flips
}

/// Flip `flips` individual bits, each in the checksum-covered region of a random
/// page of the file, so every flip is a detectable heap corruption.
fn inject_flips(path: &Path, flips: usize, rng: &mut Rng) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    if flips == 0 {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let len = file.metadata()?.len();
    if len <= PAGE {
        return Ok(());
    }
    let pages = len / PAGE;
    for _ in 0..flips {
        let page = rng.below(pages);
        let off = HEADER + rng.below(PAGE - HEADER);
        let pos = page * PAGE + off;
        if pos >= len {
            continue;
        }
        file.seek(SeekFrom::Start(pos))?;
        let mut b = [0u8; 1];
        file.read_exact(&mut b)?;
        let bit = u32::try_from(rng.below(8)).expect("0..8 fits u32");
        b[0] ^= 1u8 << bit;
        file.seek(SeekFrom::Start(pos))?;
        file.write_all(&b)?;
    }
    file.flush()
}

/// Reopen the irradiated store and check the silent-corruption invariant: every
/// query either errors (the corruption was detected) or returns exactly the
/// committed, correctly-isolated data. Returns whether any detection occurred, or
/// an error naming the first silently-wrong answer.
fn verify_irradiated(seed: u64, w: &Workload) -> Result<bool, String> {
    // Recovery refusing to open the corrupted store counts as detection, not a
    // silent failure.
    let Ok(mut db) = Database::open(&w.path) else {
        return Ok(true);
    };

    let live_total: usize = w.live.iter().map(BTreeMap::len).sum();
    match db.execute("SELECT id FROM memories") {
        Err(_) => return Ok(true),
        Ok(QueryOutcome::Rows { rows, .. }) => {
            if rows.len() != live_total {
                return Err(format!(
                    "seed {seed}: irradiated: silent row-count change, expected {live_total}, found {}",
                    rows.len()
                ));
            }
        }
        Ok(_) => {}
    }

    let probe = ast::format_vector(&vec![0.0f32; w.dim]);
    let mut detected = false;
    for (t, want) in w.live.iter().enumerate() {
        db.set_session_user(&format!("t{t}"));
        match db.execute("SELECT id, e FROM memories ORDER BY id") {
            Err(_) => {
                detected = true;
                continue;
            }
            Ok(QueryOutcome::Rows { rows, .. }) => {
                let expected: Vec<Vec<Value>> = want
                    .iter()
                    .map(|(id, v)| vec![Value::Int(*id), Value::Vector(v.clone())])
                    .collect();
                if rows != expected {
                    return Err(format!(
                        "seed {seed}: tenant t{t}: irradiated silent corruption, served data differs from committed without an error"
                    ));
                }
            }
            Ok(_) => {}
        }
        match db.execute(&format!(
            "SELECT tenant FROM memories ORDER BY e <-> '{probe}' LIMIT 1000"
        )) {
            Err(_) => detected = true,
            Ok(QueryOutcome::Rows { rows, .. }) => {
                let mine = Value::Text(format!("t{t}"));
                if rows.iter().any(|row| row.first() != Some(&mine)) {
                    return Err(format!(
                        "seed {seed}: tenant t{t}: irradiated KNN leaked another tenant's row"
                    ));
                }
            }
            Ok(_) => {}
        }
    }
    Ok(detected)
}

/// The committed state of one finished workload, with the engine closed and its
/// bytes resting on disk. Both the clean reopen check and the irradiated check
/// start from here, so the two share an identical workload for a given seed.
struct Workload {
    path: PathBuf,
    live: Oracle,
    dim: usize,
    tenants: usize,
    committed: usize,
    rolled_back: usize,
}

/// Build and commit one seeded multi-tenant workload through the real engine,
/// then close it (the crash). Returns the oracle of what a correct database must
/// hold, alongside the on-disk path the bytes now live at.
#[allow(clippy::too_many_lines)]
fn build_workload(seed: u64, dir: &Path) -> Result<Workload, String> {
    let mut rng = Rng::new(seed);
    let dim = usize::try_from(2 + rng.below(6)).expect("small");
    let tenants = usize::try_from(2 + rng.below(3)).expect("small");
    let ops = 30 + rng.below(90);

    let path = dir.join("mem.db");
    let mut db = Database::open(&path).map_err(|e| format!("open: {e}"))?;

    // Schema: a multi-tenant embedding table fenced by a per-tenant RLS policy.
    exec(
        &mut db,
        &format!("CREATE TABLE memories (id INT, tenant TEXT, e VECTOR({dim}))"),
    )?;
    exec(
        &mut db,
        "GRANT SELECT, INSERT, UPDATE, DELETE ON memories TO PUBLIC",
    )?;
    for t in 0..tenants {
        exec(&mut db, &format!("CREATE ROLE t{t} LOGIN"))?;
    }
    // USING fences reads and the affected scope of writes; WITH CHECK fences the
    // values written, so a tenant can neither see nor create another's rows.
    exec(
        &mut db,
        "CREATE POLICY tenant ON memories \
         USING ((tenant = current_user())) WITH CHECK ((tenant = current_user()))",
    )?;
    exec(&mut db, "ALTER TABLE memories ENABLE ROW LEVEL SECURITY")?;

    // Workload: a mix of inserts, updates, and deletes, a fifth of them rolled
    // back. Each operation runs AS the owning tenant, so every write passes
    // through the RLS fence exactly as a real tenant's would; the post-crash
    // checks then confirm both durability and that the fence held.
    let mut live: Oracle = vec![BTreeMap::new(); tenants];
    let mut committed = 0usize;
    let mut rolled_back = 0usize;

    for i in 0..ops {
        let t = usize::try_from(rng.below(tenants as u64)).expect("small");
        db.set_session_user(&format!("t{t}"));
        let (stmt, effect) = build_op(&mut rng, t, i, dim, &live[t]);
        if rng.below(5) == 0 {
            // A rolled-back transaction: its change must leave no trace.
            exec(&mut db, "BEGIN")?;
            exec(&mut db, &stmt)?;
            exec(&mut db, "ROLLBACK")?;
            rolled_back += 1;
        } else {
            exec(&mut db, &stmt)?;
            match effect {
                Effect::Set(id, v) => {
                    live[t].insert(id, v);
                }
                Effect::Remove(id) => {
                    live[t].remove(&id);
                }
            }
            committed += 1;
        }
    }

    // The crash: drop the engine, leaving the committed bytes on disk.
    drop(db);
    Ok(Workload {
        path,
        live,
        dim,
        tenants,
        committed,
        rolled_back,
    })
}

/// Run one seeded crash-and-recover simulation: build the workload, reopen
/// (running WAL recovery), and verify durability and isolation.
fn simulate(seed: u64, dir: &Path) -> Result<Outcome, String> {
    let Workload {
        path,
        live,
        dim,
        tenants,
        committed,
        rolled_back,
    } = build_workload(seed, dir)?;
    let mut db = Database::open(&path).map_err(|e| format!("reopen: {e}"))?;

    // Durability, from the superuser's unfenced view: exactly the live rows
    // survive, no rolled-back or deleted row reappears.
    let live_total: usize = live.iter().map(BTreeMap::len).sum();
    let total = rows(&mut db, "SELECT id FROM memories")?.len();
    if total != live_total {
        return Err(format!(
            "seed {seed}: durability: expected {live_total} live rows, found {total}"
        ));
    }

    // A zero-vector probe of the right width for the nearest-neighbor checks.
    let probe = ast::format_vector(&vec![0.0f32; dim]);

    // Isolation plus durability, per tenant: each tenant sees exactly its own
    // live embeddings, intact, and nothing belonging to anyone else.
    for (t, want) in live.iter().enumerate() {
        db.set_session_user(&format!("t{t}"));
        let got = rows(&mut db, "SELECT id, e FROM memories ORDER BY id")?;
        let expected: Vec<Vec<Value>> = want
            .iter()
            .map(|(id, v)| vec![Value::Int(*id), Value::Vector(v.clone())])
            .collect();
        if got != expected {
            return Err(format!(
                "seed {seed}: tenant t{t}: expected {} rows, found {} (isolation or durability violated)",
                expected.len(),
                got.len()
            ));
        }
        // A nearest-neighbor query must also stay inside the tenant's own rows:
        // every row a tenant's KNN ranks must carry that tenant's own label.
        let knn = rows(
            &mut db,
            &format!("SELECT tenant FROM memories ORDER BY e <-> '{probe}' LIMIT 1000"),
        )?;
        let mine = Value::Text(format!("t{t}"));
        if knn.iter().any(|row| row.first() != Some(&mine)) {
            return Err(format!(
                "seed {seed}: tenant t{t}: a KNN result leaked another tenant's row"
            ));
        }

        // The approximate (HNSW index) path is fenced too: a tenant's index
        // search returns only its own rows. This fault-tests the index path's
        // isolation after a crash, not just the exact brute-force path.
        let zero = vec![0.0f32; dim];
        let approx = db
            .knn("memories", "e", &zero, 8, Metric::L2)
            .map_err(|e| format!("seed {seed}: tenant t{t}: knn: {e}"))?;
        if approx.iter().any(|row| row.get(1) != Some(&mine)) {
            return Err(format!(
                "seed {seed}: tenant t{t}: the index path leaked another tenant's row"
            ));
        }
    }

    Ok(Outcome {
        tenants,
        committed,
        rolled_back,
        live: live_total,
    })
}

/// Build one random operation for tenant `t` and the oracle effect it has if
/// committed. Inserts use a fresh id derived from the op index `i`; updates and
/// deletes target one of the tenant's currently-live ids, falling back to an
/// insert when the tenant has no rows yet.
fn build_op(
    rng: &mut Rng,
    t: usize,
    i: u64,
    dim: usize,
    tenant_live: &BTreeMap<i64, Vec<f32>>,
) -> (String, Effect) {
    let fresh_id = i64::try_from(i).expect("op count fits i64") + 1;
    // 0,1 -> insert; 2 -> update; 3 -> delete. Without live rows, always insert.
    let choice = if tenant_live.is_empty() {
        0
    } else {
        rng.below(4)
    };
    match choice {
        2 => {
            let target = nth_key(tenant_live, rng);
            let v = random_vector(rng, dim);
            let stmt = format!(
                "UPDATE memories SET e = '{}' WHERE id = {target}",
                ast::format_vector(&v)
            );
            (stmt, Effect::Set(target, v))
        }
        3 => {
            let target = nth_key(tenant_live, rng);
            (
                format!("DELETE FROM memories WHERE id = {target}"),
                Effect::Remove(target),
            )
        }
        _ => {
            let v = random_vector(rng, dim);
            let stmt = format!(
                "INSERT INTO memories VALUES ({fresh_id}, 't{t}', '{}')",
                ast::format_vector(&v)
            );
            (stmt, Effect::Set(fresh_id, v))
        }
    }
}

/// Pick a pseudo-random key from a non-empty map. The map's keys iterate in
/// sorted order, so the choice is fully determined by the seed.
fn nth_key(map: &BTreeMap<i64, Vec<f32>>, rng: &mut Rng) -> i64 {
    let k = usize::try_from(rng.below(map.len() as u64)).expect("nonempty map");
    *map.keys().nth(k).expect("k is below len")
}

/// A random embedding of `dim` integer-valued components in `[-1000, 1000]`.
/// Integer-valued `f32`s in that range format and parse back exactly, so the
/// stored embedding round-trips the SQL literal path with no float-formatting
/// ambiguity to muddy the durability check.
fn random_vector(rng: &mut Rng, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|_| {
            let raw = i16::try_from(rng.below(2001)).expect("0..=2000 fits i16");
            f32::from(raw - 1000)
        })
        .collect()
}

/// Run a non-query statement, mapping any failure to a descriptive string.
fn exec(db: &mut Database, sql: &str) -> Result<(), String> {
    db.execute(sql)
        .map(|_| ())
        .map_err(|e| format!("exec `{sql}`: {e}"))
}

/// Run a query and return its rows.
fn rows(db: &mut Database, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    match db.execute(sql).map_err(|e| format!("exec `{sql}`: {e}"))? {
        QueryOutcome::Rows { rows, .. } => Ok(rows),
        other => Err(format!("expected rows from `{sql}`, got {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{run_seed, run_seed_irradiated, Orbit};

    #[test]
    fn irradiated_runs_never_serve_silent_corruption() {
        // A heavy dose so the flips reliably land on live heap pages across the
        // sweep. Every run must uphold the invariant (an Ok return), and at least
        // one must have actually detected corruption, proving the fault path is
        // exercised rather than always missing the data.
        let mut detected = 0usize;
        let mut flips_total = 0usize;
        for seed in 0..16 {
            let outcome = run_seed_irradiated(seed, Orbit::Geo, 500.0).unwrap_or_else(|e| {
                panic!("seed {seed}: silent-corruption invariant violated: {e}")
            });
            if outcome.detected {
                detected += 1;
            }
            flips_total += outcome.flips;
        }
        assert!(
            flips_total > 0,
            "the dose never produced a single upset; the test exercised nothing"
        );
        assert!(
            detected > 0,
            "no irradiated run detected corruption; the dose never landed on data"
        );
    }

    #[test]
    fn a_light_dose_mostly_passes_cleanly() {
        // A realistic low-Earth-orbit dose over a short dwell: the invariant must
        // still hold for every seed, whether or not any flip happened to land.
        for seed in 0..16 {
            run_seed_irradiated(seed, Orbit::Leo, 1.0)
                .unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        }
    }

    #[test]
    fn the_same_irradiated_seed_is_reproducible() {
        let a = run_seed_irradiated(7, Orbit::Geo, 250.0).expect("seed 7");
        let b = run_seed_irradiated(7, Orbit::Geo, 250.0).expect("seed 7 again");
        assert_eq!(a.bytes, b.bytes);
        assert_eq!(a.flips, b.flips);
        assert_eq!(a.detected, b.detected);
        assert_eq!(a.live, b.live);
    }

    #[test]
    fn a_sweep_of_seeds_holds_durability_and_isolation() {
        // A small sweep keeps the routine test suite fast; the `vecsim` binary
        // sweeps tens of thousands of seeds on demand in release.
        for seed in 0..8 {
            run_seed(seed).unwrap_or_else(|e| panic!("seed {seed} failed: {e}"));
        }
    }

    #[test]
    fn the_same_seed_is_reproducible() {
        let a = run_seed(7).expect("seed 7");
        let b = run_seed(7).expect("seed 7 again");
        assert_eq!(a.committed, b.committed);
        assert_eq!(a.rolled_back, b.rolled_back);
        assert_eq!(a.live, b.live);
        assert_eq!(a.tenants, b.tenants);
    }
}
