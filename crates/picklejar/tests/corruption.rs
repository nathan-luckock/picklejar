//! Engine-level proof that committed data is never served silently corrupted.
//!
//! For many seeds, this writes known rows (including vectors), flips one byte in a
//! checksum-covered region of a random page in the live database file, reopens,
//! and queries. The invariant: the engine either detects the corruption (a
//! checksum error surfaces from open or from the query) or the flip landed where
//! it does not change the answer, but it never returns committed data that
//! differs from what was written without raising an error. This is the
//! whole-store version of the "never serve a silently corrupted value" property,
//! exercised through the SQL layer.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

use picklejar::{Database, QueryOutcome, Value};
use tempfile::tempdir;

const PAGE: u64 = 8192;

/// `SplitMix64`, so the corruption sites are deterministic.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[test]
fn committed_data_is_never_served_silently_corrupted() {
    // The known rows every database in this test holds.
    let f = |n: i64| f32::from(i16::try_from(n).expect("small"));
    let expected: Vec<Vec<Value>> = (1..=20i64)
        .map(|i| vec![Value::Int(i), Value::Vector(vec![f(i), f(i * 2)])])
        .collect();

    let mut seed = 0xABCD_EF12_3456_789Au64;
    let mut detected = 0usize;
    let mut unaffected = 0usize;
    for _ in 0..60 {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("c.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE t (id INT, e VECTOR(2))").unwrap();
            for i in 1..=20i64 {
                db.execute(&format!("INSERT INTO t VALUES ({i}, '[{i}, {}]')", i * 2))
                    .unwrap();
            }
        }

        // Flip one byte in a checksum-covered region of a random page.
        let len = std::fs::metadata(&path).expect("metadata").len();
        let pages = (len / PAGE).max(1);
        let page = next(&mut seed) % pages;
        let off = 12 + next(&mut seed) % (PAGE - 12);
        let pos = page * PAGE + off;
        if pos < len {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .expect("open file");
            file.seek(SeekFrom::Start(pos)).unwrap();
            let mut b = [0u8; 1];
            file.read_exact(&mut b).unwrap();
            b[0] ^= 0xFF;
            file.seek(SeekFrom::Start(pos)).unwrap();
            file.write_all(&b).unwrap();
        }

        // Reopen and query. The corruption is either detected (an error from open
        // or the query) or it did not affect the committed data; it is never
        // returned as a different, wrong answer without an error.
        match Database::open(&path) {
            Err(_) => detected += 1,
            Ok(mut db) => match db.execute("SELECT id, e FROM t ORDER BY id") {
                Err(_) => detected += 1,
                Ok(QueryOutcome::Rows { rows, .. }) => {
                    assert_eq!(
                        rows, expected,
                        "silent corruption: committed data was returned wrong without an error"
                    );
                    unaffected += 1;
                }
                Ok(_) => unaffected += 1,
            },
        }
    }

    // Every iteration upheld the invariant. Both outcomes should occur, which
    // confirms the test actually exercises corrupted reads and not only clean ones.
    assert_eq!(detected + unaffected, 60);
    assert!(detected > 0, "the corruption was never actually exercised");
}
