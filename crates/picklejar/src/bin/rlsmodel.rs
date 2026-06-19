//! Exhaustively model-check the row-level-security retrieval invariant and
//! report: a tenant's query, accelerated by the approximate index or not, can
//! never return another tenant's row. The complement to the `vecsim` random
//! isolation sweep, this proves it over every reachable interleaving of a
//! bounded model.
//!
//! ```text
//! cargo run --release --bin rlsmodel        # sweep bounds 1..=4
//! cargo run --release --bin rlsmodel -- 6   # sweep bounds 1..=6
//! ```

use std::process::ExitCode;

use picklejar::isolation_model::{check, reachable_states};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let max: u8 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(4);

    println!(
        "model-checking the RLS retrieval invariant (a tenant query never returns another \
         tenant's row)..."
    );
    for bound in 1..=max {
        if let Some(cx) = check(bound, true) {
            eprintln!("VIOLATION at bound {bound}: {cx:?}");
            return ExitCode::FAILURE;
        }
        println!(
            "  bound {bound}: invariant holds over {} reachable states",
            reachable_states(bound, true)
        );
    }

    // Confirm the check has teeth: a buggy engine that serves the approximate
    // index under an active policy must be caught, or the proof above is vacuous.
    if let Some(cx) = check(2, false) {
        println!("  teeth check: an index path taken under a policy is caught ({cx:?})");
    } else {
        eprintln!("teeth check failed: a known-buggy dispatch was not caught");
        return ExitCode::FAILURE;
    }

    println!("result: RLS retrieval isolation proved over every interleaving up to bound {max}");
    ExitCode::SUCCESS
}
