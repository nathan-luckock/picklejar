//! Consistent hashing: a node joins the fleet and steals only its fair share.
//!
//! ```text
//! cargo run --release --bin ringsim
//! ```

use std::process::ExitCode;

use picklejar::consistenthash::HashRing;

fn route_all(ring: &HashRing, n: u64) -> Vec<String> {
    (0..n)
        .map(|i| ring.route(&i.to_be_bytes()).unwrap_or("?").to_string())
        .collect()
}

#[allow(clippy::cast_precision_loss)]
fn main() -> ExitCode {
    println!("\n=============== CONSISTENT HASHING ===============");
    println!("grow the fleet without reshuffling every memory\n");

    let n = 100_000u64;
    let mut ring = HashRing::new(200);
    for name in ["node-1", "node-2", "node-3"] {
        ring.add_node(name);
    }
    let before = route_all(&ring, n);
    println!("placed {n} memories across 3 nodes.");

    ring.add_node("node-4");
    let after = route_all(&ring, n);
    let moved = before.iter().zip(&after).filter(|(b, a)| b != a).count();
    let pct = moved as f64 / n as f64 * 100.0;

    // A naive hash-mod-n would remap almost everything; estimate it for contrast.
    let naive_moved = before
        .iter()
        .enumerate()
        .filter(|(i, _)| (*i as u64 % 3) != (*i as u64 % 4))
        .count();

    println!("\nadded node-4. memories that moved:");
    println!("  consistent hashing: {moved}/{n} ({pct:.1}%)  -- all onto node-4");
    println!(
        "  naive hash mod n:   ~{naive_moved}/{n} (~{:.0}%)",
        naive_moved as f64 / n as f64 * 100.0
    );

    let all_to_new = before
        .iter()
        .zip(&after)
        .filter(|(b, a)| b != a)
        .all(|(_, a)| a == "node-4");

    println!("\n==================================================");
    if pct < 35.0 && all_to_new {
        println!("VERDICT: adding a 4th node moved only ~{pct:.0}% of memories, all onto the");
        println!(
            "new node, versus the ~{:.0}% a naive hash-mod-n would shuffle. on",
            naive_moved as f64 / n as f64 * 100.0
        );
        println!("unreachable hardware, that is the difference between a rebalance and an outage.");
    } else {
        println!("VERDICT: unexpected (moved {pct:.1}%, all_to_new={all_to_new}).");
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
