//! Deterministic simulation testing (DST) runner for crash recovery.
//!
//! Each seed drives one fully reproducible crash-and-recover scenario (see
//! [`picklejar_wal::sim`]). This binary sweeps a range of seeds and reports the
//! first that breaks an invariant, so it can be replayed exactly.
//!
//! ```text
//! cargo run --release --bin dst                 # 1000 seeds from 0
//! cargo run --release --bin dst -- 100000       # 100k seeds from 0
//! cargo run --release --bin dst -- --seed 42    # replay one seed, verbose
//! ```

use std::process::ExitCode;

use picklejar_wal::run_seed;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    if args.get(1).map(String::as_str) == Some("--seed") {
        let seed: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
        return match run_seed(seed) {
            Ok(outcome) => {
                println!(
                    "seed {seed}: OK  committed={} rolled_back={} winners={} redone={} undone={}",
                    outcome.committed,
                    outcome.rolled_back,
                    outcome.winners,
                    outcome.redone,
                    outcome.undone,
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("FAIL {e}");
                ExitCode::FAILURE
            }
        };
    }

    // `dst <count> [start]` sweeps the half-open seed range `start..start+count`.
    // The optional start makes it shardable: several processes can each cover a
    // disjoint slice of one large run in parallel, then their union is the whole.
    let count: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let start: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let end = start.saturating_add(count);
    if start == 0 {
        println!("running {count} deterministic crash-recovery simulations...");
    } else {
        println!("running {count} simulations over seeds {start}..{end}...");
    }
    let mut committed_total = 0u64;
    let mut done = 0u64;
    for seed in start..end {
        match run_seed(seed) {
            Ok(outcome) => committed_total += outcome.committed as u64,
            Err(e) => {
                eprintln!("FAIL {e}");
                eprintln!("reproduce with: cargo run --bin dst -- --seed {seed}");
                return ExitCode::FAILURE;
            }
        }
        done += 1;
        if count >= 1000 && done % (count / 10).max(1) == 0 {
            println!("  {done}/{count} seeds passed (through seed {seed})");
        }
    }
    println!("all {count} seeds recovered correctly ({committed_total} committed rows verified)");
    ExitCode::SUCCESS
}
