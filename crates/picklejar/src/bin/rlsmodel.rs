//! Exhaustively model-check that the approximate index cache never serves a
//! wrong row, two ways: it never returns another tenant's row (isolation) and it
//! never returns a deleted row (freshness). The complement to the random `vecsim`
//! sweep, this proves both over every reachable interleaving of a bounded model.
//!
//! ```text
//! cargo run --release --bin rlsmodel        # sweep bounds 1..=4
//! cargo run --release --bin rlsmodel -- 6   # sweep bounds 1..=6
//! ```

use std::process::ExitCode;

use picklejar::{freshness_model, isolation_model, valid_time_model};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let max: u8 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(4);

    println!("model-checking the index cache: a query never returns a row it should not...");

    println!("isolation (a tenant query never returns another tenant's row):");
    for bound in 1..=max {
        if let Some(cx) = isolation_model::check(bound, true) {
            eprintln!("VIOLATION at bound {bound}: {cx:?}");
            return ExitCode::FAILURE;
        }
        println!(
            "  bound {bound}: holds over {} reachable states",
            isolation_model::reachable_states(bound, true)
        );
    }
    if let Some(cx) = isolation_model::check(2, false) {
        println!("  teeth: an index path taken under a policy is caught ({cx:?})");
    } else {
        eprintln!("teeth check failed: a known-buggy dispatch was not caught");
        return ExitCode::FAILURE;
    }

    println!("freshness (a query never returns a deleted row):");
    for rows in 1..=max {
        if let Some(cx) = freshness_model::check(rows, true) {
            eprintln!("VIOLATION at {rows} rows: {cx:?}");
            return ExitCode::FAILURE;
        }
        println!(
            "  {rows} rows: holds over {} reachable states",
            freshness_model::reachable_states(rows, true)
        );
    }
    if let Some(cx) = freshness_model::check(2, false) {
        println!("  teeth: a delete that leaves the cache stale is caught ({cx:?})");
    } else {
        eprintln!("teeth check failed: a stale-cache delete was not caught");
        return ExitCode::FAILURE;
    }

    println!("valid-time travel (a read returns a row exactly when it is valid then):");
    for domain in 1..=max {
        if let Some(cx) = valid_time_model::check(domain, true) {
            eprintln!("VIOLATION over domain {domain}: {cx:?}");
            return ExitCode::FAILURE;
        }
        println!(
            "  domain {domain}: holds over {} checked cases",
            valid_time_model::reachable_states(domain, true)
        );
    }
    if let Some(cx) = valid_time_model::check(3, false) {
        println!("  teeth: a closed upper bound that serves a superseded row is caught ({cx:?})");
    } else {
        eprintln!("teeth check failed: a closed upper bound was not caught");
        return ExitCode::FAILURE;
    }

    println!(
        "result: index-cache isolation and freshness, and valid-time travel, proved exhaustively up to bound {max}"
    );
    ExitCode::SUCCESS
}
