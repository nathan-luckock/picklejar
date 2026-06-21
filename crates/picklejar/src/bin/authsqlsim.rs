//! Authenticated SQL: verify a `WHERE` query result without trusting the server.
//!
//! ```text
//! cargo run --release --bin authsqlsim
//! ```

use std::process::ExitCode;

use picklejar::authsql::{verify_complete, verify_sound, Cmp, Predicate, Record, Rejected, Table};

fn main() -> ExitCode {
    println!("\n=============== AUTHENTICATED SQL ===============");
    println!("PROVE SELECT: a verifiable query result for an untrusted node\n");

    // The committed table: (rowid, [salary, dept]).
    let table = Table::new(vec![
        Record {
            rowid: 1,
            fields: vec![50_000, 1],
        },
        Record {
            rowid: 2,
            fields: vec![120_000, 2],
        },
        Record {
            rowid: 3,
            fields: vec![90_000, 1],
        },
        Record {
            rowid: 4,
            fields: vec![60_000, 2],
        },
        Record {
            rowid: 5,
            fields: vec![200_000, 1],
        },
    ]);
    let commit = table.commit();
    let pred = Predicate {
        field: 0,
        op: Cmp::Gt,
        value: 80_000,
    };

    println!("committed 5 rows; client pins one root and forgets the table.");
    println!("query: SELECT rowid WHERE salary > 80000\n");

    let rows = table.query(&pred);
    let ids: Vec<u64> = rows.iter().map(|r| r.record.rowid).collect();
    println!("server returns rows {ids:?} with inclusion proofs.");
    let sound = verify_sound(commit, &pred, &rows).is_ok();
    println!(
        "  client soundness check (authentic + matches): {}\n",
        if sound { "PASS" } else { "FAIL" }
    );

    let mut caught = 0;

    // Attack 1: alter a returned row's salary.
    {
        let mut tampered = rows;
        tampered[0].record.fields[0] = 999_999;
        if matches!(
            verify_sound(commit, &pred, &tampered),
            Err(Rejected::Forged { .. })
        ) {
            caught += 1;
            println!("[CAUGHT] fabricated row -> inclusion proof failed");
        }
    }

    // Attack 2: return a real committed row that does not match (rowid 1).
    {
        let sneaked: Vec<_> = table
            .full()
            .into_iter()
            .filter(|r| r.record.rowid == 1)
            .collect();
        if matches!(
            verify_sound(commit, &pred, &sneaked),
            Err(Rejected::NotMatching { .. })
        ) {
            caught += 1;
            println!("[CAUGHT] padded a non-matching row -> predicate re-check failed");
        }
    }

    // Attack 3: omit a matching row. Caught only by the completeness check.
    {
        let mut all = table.full();
        all.retain(|r| r.record.rowid != 3);
        if verify_complete(commit, &pred, &all) == Err(Rejected::Incomplete) {
            caught += 1;
            println!("[CAUGHT] withheld a matching row -> disclosed rows do not rebuild the root");
        }
    }

    // The honest completeness path returns the true, verified-complete result.
    let complete = verify_complete(commit, &pred, &table.full())
        .map(|r| r.iter().map(|x| x.rowid).collect::<Vec<_>>());

    println!("\n  completeness-by-disclosure result: {complete:?}");

    println!("\n==================================================");
    if sound && caught == 3 && complete == Ok(vec![2, 3, 5]) {
        println!("VERDICT: the query result verified against a pinned root with no trust in");
        println!("the server. fabrication and padding fail soundness; omission fails the");
        println!("completeness check. authenticated KNN, generalized to SQL.");
    } else {
        println!("VERDICT: unexpected (sound={sound}, caught={caught}, complete={complete:?}).");
        return ExitCode::FAILURE;
    }
    println!("note: soundness is succinct (proof per returned row); completeness here is");
    println!("by full disclosure, the honest price without heavier cryptography.");
    println!("==================================================\n");
    ExitCode::SUCCESS
}
