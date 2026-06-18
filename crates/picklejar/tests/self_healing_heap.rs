//! The live database heap heals itself: protect a committed database with
//! Reed-Solomon parity, corrupt heap pages the way radiation would, reopen
//! through `open_resilient`, and confirm every row and embedding comes back
//! exactly. This is the engine-level payoff of the erasure-coding work, the
//! database repairing its own storage with no human and no spare node.

use std::io::{Read, Seek, SeekFrom, Write};

use picklejar::{Database, QueryOutcome, Value};
use tempfile::tempdir;

const PAGE: u64 = 8192;

/// `SplitMix64`, so the corruption sites replay exactly.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Flip one byte in the checksum-covered region of page `page` of `path`.
fn corrupt_page(path: &std::path::Path, page: u64, seed: &mut u64) {
    let off = 12 + next(seed) % (PAGE - 12);
    let pos = page * PAGE + off;
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open heap");
    if pos >= f.metadata().expect("meta").len() {
        return;
    }
    f.seek(SeekFrom::Start(pos)).unwrap();
    let mut b = [0u8; 1];
    f.read_exact(&mut b).unwrap();
    b[0] ^= 0xFF;
    f.seek(SeekFrom::Start(pos)).unwrap();
    f.write_all(&b).unwrap();
}

/// An exact `f32` for a small integer, dodging lossy `as` casts in the oracle.
fn f(x: i64) -> f32 {
    f32::from(i16::try_from(x).expect("fits i16"))
}

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute(sql).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        QueryOutcome::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn the_heap_heals_corrupt_pages_from_parity_on_open() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("mem.db");

    // The committed memory layer: rows with embeddings, plus an isolation policy,
    // so the snapshot covers ordinary data, vectors, and metadata-bearing pages.
    let n = 3000i64;
    let expected: Vec<Vec<Value>> = (1..=n)
        .map(|i| vec![Value::Int(i), Value::Vector(vec![f(i), f(i * 2), f(i * 3)])])
        .collect();
    {
        let mut db = Database::open(&path).expect("open");
        db.execute("CREATE TABLE memories (id INT, e VECTOR(3))")
            .unwrap();
        for i in 1..=n {
            db.execute(&format!(
                "INSERT INTO memories VALUES ({i}, '[{i}, {}, {}]')",
                i * 2,
                i * 3
            ))
            .unwrap();
        }
        // Take the parity snapshot of the committed heap: k=6, m=3 (heals any
        // three corrupt pages per stripe at +50% parity for this stripe size).
        let report = db.protect(6, 3).expect("protect");
        assert!(report.protected_pages > 6, "should protect several stripes");
        assert!(db.parity_path().exists(), "parity sidecar written");
    }

    // Irradiate the heap: corrupt one page in each six-page stripe. Each stripe
    // tolerates three bad pages, so one per stripe is comfortably recoverable.
    let pages = std::fs::metadata(&path).expect("meta").len() / PAGE;
    assert!(pages >= 6, "the store should span multiple stripes");
    let mut seed = 0xBADC_0FFE_E0DDu64;
    let stripes = pages.div_ceil(6);
    for s in 0..stripes {
        let page = s * 6 + 1; // the second page of each stripe
        if page < pages {
            corrupt_page(&path, page, &mut seed);
        }
    }

    // A plain open would now hit a checksum error on the corrupt pages. Opening
    // through the self-healing path reconstructs them from parity first.
    let mut db = Database::open_resilient(&path).expect("resilient open");
    let got = rows(&mut db, "SELECT id, e FROM memories ORDER BY id");
    assert_eq!(
        got, expected,
        "healed heap did not return the committed data"
    );
}

#[test]
fn protect_statement_writes_parity_and_heals() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("p.db");
    let n = 1500i64;
    {
        let mut db = Database::open(&path).expect("open");
        db.execute("CREATE TABLE m (id INT, e VECTOR(3))").unwrap();
        for i in 1..=n {
            db.execute(&format!(
                "INSERT INTO m VALUES ({i}, '[{i}, {}, {}]')",
                i * 2,
                i * 3
            ))
            .unwrap();
        }
        // PROTECT through SQL, with explicit shard counts, returns a report row.
        let report = rows(&mut db, "PROTECT WITH (k = 6, m = 3)");
        assert_eq!(report.len(), 1, "PROTECT returns one report row");
        match report[0].first() {
            Some(Value::Int(pages)) => assert!(*pages > 1, "should protect pages"),
            other => panic!("expected protected_pages int, got {other:?}"),
        }
        assert!(
            db.parity_path().exists(),
            "PROTECT wrote the parity sidecar"
        );
    }

    // Corrupt one page per stripe and confirm open_resilient heals it.
    let pages = std::fs::metadata(&path).expect("meta").len() / PAGE;
    let mut seed = 0xF00Du64;
    let mut s = 0u64;
    while s * 6 + 1 < pages {
        corrupt_page(&path, s * 6 + 1, &mut seed);
        s += 1;
    }
    let mut db = Database::open_resilient(&path).expect("resilient open");
    let got = rows(&mut db, "SELECT id FROM m ORDER BY id LIMIT 3");
    assert_eq!(
        got,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)]
        ]
    );
}

#[test]
fn protect_requires_a_superuser() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("perm.db");
    let mut db = Database::open(&path).expect("open");
    db.execute("CREATE TABLE t (id INT)").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    db.execute("CREATE ROLE bob LOGIN").unwrap();

    // The bootstrap superuser may protect.
    assert!(db.execute("PROTECT").is_ok());
    // An ordinary role may not: PROTECT reads the whole heap.
    db.set_session_user("bob");
    assert!(
        db.execute("PROTECT").is_err(),
        "a non-superuser must not be able to PROTECT"
    );
}

#[test]
fn open_resilient_without_a_parity_file_is_just_open() {
    // No protect, no parity sidecar: open_resilient behaves exactly like open.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("plain.db");
    {
        let mut db = Database::open(&path).expect("open");
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    }
    let mut db = Database::open_resilient(&path).expect("resilient open");
    assert_eq!(
        rows(&mut db, "SELECT id FROM t ORDER BY id"),
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)],
        ]
    );
}

#[test]
fn corruption_beyond_parity_is_detected_not_served_wrong() {
    // Corrupt more pages in one stripe than parity can heal: the data is lost, but
    // it must surface as an error, never as a silently wrong answer.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("over.db");
    {
        let mut db = Database::open(&path).expect("open");
        db.execute("CREATE TABLE t (id INT, e VECTOR(2))").unwrap();
        for i in 1..=120i64 {
            db.execute(&format!("INSERT INTO t VALUES ({i}, '[{i}, {i}]')"))
                .unwrap();
        }
        db.protect(6, 1).expect("protect"); // only one parity per stripe
    }
    // Corrupt the first three pages: stripe 0 now has more bad pages than parity.
    let mut seed = 1u64;
    for page in 0..3 {
        corrupt_page(&path, page, &mut seed);
    }
    // open_resilient heals what it can; the unrecoverable pages stay corrupt and
    // the engine refuses them (at open or on the query) rather than returning
    // wrong data. If a query does return rows, they must be exactly correct.
    if let Ok(mut db) = Database::open_resilient(&path) {
        if let Ok(QueryOutcome::Rows { rows, .. }) = db.execute("SELECT id, e FROM t ORDER BY id") {
            let expected: Vec<Vec<Value>> = (1..=120i64)
                .map(|i| vec![Value::Int(i), Value::Vector(vec![f(i), f(i)])])
                .collect();
            assert_eq!(
                rows, expected,
                "served data must be exactly correct if no error"
            );
        }
    }
}
