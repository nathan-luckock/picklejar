//! Deterministic simulation runner for the vector memory layer.
//!
//! Each seed drives one fully reproducible crash-and-recover scenario through
//! the real engine (see [`picklejar::vecsim`]), proving that committed
//! embeddings survive intact (durability) and that each tenant sees only its own
//! after recovery (isolation). This binary sweeps a range of seeds and reports
//! the first that breaks an invariant, so it can be replayed exactly.
//!
//! ```text
//! cargo run --release --bin vecsim                 # 1000 seeds from 0
//! cargo run --release --bin vecsim -- 100000       # 100k seeds from 0
//! cargo run --release --bin vecsim -- --seed 42    # replay one seed, verbose
//! cargo run --release --bin vecsim -- --irradiate 10000 365 geo
//!     # 10k workloads, each irradiated at the geostationary upset rate for a
//!     # year of dwell, proving none is ever served silently corrupted
//! ```

use std::process::ExitCode;

use picklejar::radiation::Orbit;
use picklejar::vecsim::{run_seed, run_seed_irradiated};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    if args.get(1).map(String::as_str) == Some("--irradiate") {
        return irradiate_sweep(&args);
    }

    if args.get(1).map(String::as_str) == Some("--seed") {
        let seed: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
        return match run_seed(seed) {
            Ok(o) => {
                println!(
                    "seed {seed}: OK  tenants={} committed={} rolled_back={} live={}",
                    o.tenants, o.committed, o.rolled_back, o.live
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("FAIL {e}");
                ExitCode::FAILURE
            }
        };
    }

    let count: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1000);
    println!("running {count} deterministic vector durability+isolation simulations...");
    let mut live_total = 0u64;
    for seed in 0..count {
        match run_seed(seed) {
            Ok(o) => live_total += o.live as u64,
            Err(e) => {
                eprintln!("FAIL {e}");
                eprintln!("reproduce with: cargo run --bin vecsim -- --seed {seed}");
                return ExitCode::FAILURE;
            }
        }
        if count >= 1000 && (seed + 1) % (count / 10).max(1) == 0 {
            println!("  {}/{count} seeds passed", seed + 1);
        }
    }
    println!(
        "all {count} seeds recovered with isolation intact ({live_total} live embeddings verified)"
    );
    ExitCode::SUCCESS
}

/// `--irradiate <count> [orbit_days] [leo|geo]`: run `count` workloads, irradiate
/// each at the orbit's single-event-upset rate for `orbit_days` of dwell, and
/// prove none is ever served silently corrupted after reopen.
fn irradiate_sweep(args: &[String]) -> ExitCode {
    let count: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let orbit_days: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(365.0);
    let orbit = match args.get(4).map(String::as_str) {
        Some("geo" | "GEO") => Orbit::Geo,
        _ => Orbit::Leo,
    };

    println!(
        "irradiating {count} workloads at {} for {orbit_days} orbit-days each...",
        orbit.name()
    );
    let mut detected = 0u64;
    let mut flips_total = 0u64;
    for seed in 0..count {
        match run_seed_irradiated(seed, orbit, orbit_days) {
            Ok(o) => {
                flips_total += o.flips as u64;
                if o.detected {
                    detected += 1;
                }
            }
            Err(e) => {
                eprintln!("FAIL {e}");
                eprintln!(
                    "reproduce with: cargo run --bin vecsim -- --irradiate 1 {orbit_days} {}",
                    orbit_name(orbit)
                );
                return ExitCode::FAILURE;
            }
        }
        if count >= 1000 && (seed + 1) % (count / 10).max(1) == 0 {
            println!("  {}/{count} workloads upheld the invariant", seed + 1);
        }
    }
    println!(
        "all {count} irradiated workloads upheld the no-silent-corruption invariant\n  \
         {flips_total} total upsets injected at the {} rate; {detected} workloads detected \
         corruption, the rest were unaffected, none was ever served wrong",
        orbit.name()
    );
    ExitCode::SUCCESS
}

/// The lower-case CLI token for an orbit, for the reproduce hint.
const fn orbit_name(orbit: Orbit) -> &'static str {
    match orbit {
        Orbit::Leo => "leo",
        Orbit::Geo => "geo",
    }
}
