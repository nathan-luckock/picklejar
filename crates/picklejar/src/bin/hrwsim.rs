//! Rendezvous hashing: weighted sharding and minimal movement, no ring state.
//!
//! ```text
//! cargo run --release --bin hrwsim
//! ```

use std::process::ExitCode;

use picklejar::rendezvous::Rendezvous;

fn route_all(r: &Rendezvous, n: u64) -> Vec<String> {
    (0..n)
        .map(|i| r.route(&i.to_be_bytes()).unwrap_or("?").to_string())
        .collect()
}

#[allow(clippy::cast_precision_loss)]
fn main() -> ExitCode {
    println!("\n=============== RENDEZVOUS (HRW) HASHING ===============");
    println!("weighted sharding, minimal movement, no ring to maintain\n");

    let n = 60_000u64;
    let mut r = Rendezvous::new();
    r.add_node("big", 2.0);
    r.add_node("node-1", 1.0);
    r.add_node("node-2", 1.0);

    let before = route_all(&r, n);
    let big = before.iter().filter(|x| *x == "big").count();
    println!("3 nodes (big weighted 2x). of {n} memories:");
    println!(
        "  big   -> {big} ({:.0}%)  -- ~half, as its double weight implies",
        big as f64 / n as f64 * 100.0
    );

    // Lose a node; only its keys should move.
    r.remove_node("node-2");
    let after = route_all(&r, n);
    let moved = before.iter().zip(&after).filter(|(b, a)| b != a).count();
    let only_lost = before
        .iter()
        .zip(&after)
        .filter(|(b, a)| b != a)
        .all(|(b, _)| b == "node-2");
    println!("\nnode-2 fails. memories that moved: {moved} ({:.0}%), all previously on node-2: {only_lost}", moved as f64 / n as f64 * 100.0);

    println!("\n==================================================");
    let weighted_ok = (0.40..=0.60).contains(&(big as f64 / n as f64));
    if weighted_ok && only_lost {
        println!("VERDICT: the 2x-weighted node took ~half the load, and when a node failed");
        println!("only its own memories moved, each to its second choice, all with no shared");
        println!("ring state, just a score per node per key.");
    } else {
        println!(
            "VERDICT: unexpected (big share {:.2}, only_lost={only_lost}).",
            big as f64 / n as f64
        );
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
