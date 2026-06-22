//! Deterministic simulation of AP replication under partition.
//!
//! Drives random writes through random coordinators while randomly partitioning
//! and healing the network, then proves every node converges. A divergence is a
//! single `u64` seed you replay exactly. The cluster-level counterpart of the
//! single-node crash simulator.
//!
//! ```text
//! cargo run --release --bin repsim -- 10000 5 400
//! ```

use picklejar_replication::run_seed;

fn parse<T: std::str::FromStr>(args: &[String], i: usize, default: T) -> T {
    args.get(i).and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let seeds: u64 = parse(&args, 1, 10_000);
    let nodes: usize = parse(&args, 2, 5);
    let ops: usize = parse(&args, 3, 400);

    println!("replication DST: {seeds} seeds, {nodes} nodes, {ops} ops/seed");
    println!("(random writes under random partitions, then heal + anti-entropy)\n");

    let mut writes = 0usize;
    let mut partitions = 0usize;
    let mut transfers = 0usize;
    let mut diverged: Vec<u64> = Vec::new();
    for seed in 0..seeds {
        let report = run_seed(seed, nodes, ops);
        writes += report.writes;
        partitions += report.partitions;
        transfers += report.transfers;
        if !report.converged {
            diverged.push(seed);
        }
    }

    println!("writes applied:        {writes}");
    println!("partitions induced:    {partitions}");
    println!("slot transfers (Merkle): {transfers}");
    if diverged.is_empty() {
        println!("\nVERDICT: all {seeds} seeds converged after partition + heal");
    } else {
        let shown = &diverged[..diverged.len().min(10)];
        println!(
            "\nVERDICT: {} of {seeds} seed(s) DIVERGED: {shown:?}",
            diverged.len()
        );
        std::process::exit(1);
    }
}
