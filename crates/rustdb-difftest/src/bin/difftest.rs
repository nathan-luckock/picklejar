//! Differential testing runner: rustdb vs SQLite.
//!
//! ```text
//! cargo run --release --bin difftest                # 2000 seeds from 0
//! cargo run --release --bin difftest -- 100000      # 100k seeds
//! cargo run --bin difftest -- --seed 42             # replay one, verbose
//! ```

use std::process::ExitCode;

use rustdb_difftest::{run_seed, Outcome};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    if args.get(1).map(String::as_str) == Some("--seed") {
        let seed: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
        return match run_seed(seed) {
            Ok(Outcome::Match { rows }) => {
                println!("seed {seed}: MATCH ({rows} rows)");
                ExitCode::SUCCESS
            }
            Ok(Outcome::Skipped) => {
                println!("seed {seed}: skipped (SQLite rejected the SQL)");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        };
    }

    let count: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2000);
    println!("running {count} differential comparisons against SQLite...");
    let (mut matched, mut skipped) = (0u64, 0u64);
    for seed in 0..count {
        match run_seed(seed) {
            Ok(Outcome::Match { .. }) => matched += 1,
            Ok(Outcome::Skipped) => skipped += 1,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        }
        if count >= 1000 && (seed + 1) % (count / 10).max(1) == 0 {
            println!("  {}/{count} seeds done", seed + 1);
        }
    }
    println!("all {count} seeds agreed with SQLite ({matched} compared, {skipped} skipped)");
    ExitCode::SUCCESS
}
