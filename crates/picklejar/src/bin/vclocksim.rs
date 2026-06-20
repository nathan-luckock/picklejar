//! Vector clocks: distinguish a causal memory update from a concurrent conflict.
//!
//! ```text
//! cargo run --release --bin vclocksim
//! ```

use std::process::ExitCode;

use picklejar::vclock::{Causality, VectorClock};

fn main() -> ExitCode {
    println!("\n=============== VECTOR CLOCKS ===============");
    println!("causal update, or concurrent conflict?\n");

    let mut correct = 0;

    // Case 1: a causal chain. Node 1 writes, node 2 sees it and writes.
    {
        let mut n1 = VectorClock::new();
        n1.increment(1);
        let mut n2 = n1.clone();
        n2.merge(&n1);
        n2.increment(2);
        let rel = n2.compare(&n1);
        println!("causal chain  (n1 writes, n2 reads it then writes):");
        println!("  n2 vs n1 -> {rel:?}");
        if rel == Causality::After {
            correct += 1;
            println!("  => n2's write supersedes n1's; no conflict.");
        }
    }

    // Case 2: concurrent writes during a partition.
    {
        let mut shared = VectorClock::new();
        shared.increment(1);
        let mut a = shared.clone();
        a.increment(1);
        let mut b = shared.clone();
        b.increment(2);
        let rel = a.compare(&b);
        println!("\nconcurrent   (n1 and n2 both write without seeing each other):");
        println!("  a vs b -> {rel:?}");
        if rel == Causality::Concurrent {
            correct += 1;
            println!("  => a true conflict: hand both versions to the merge rule.");
        }
    }

    println!("\n==================================================");
    if correct == 2 {
        println!("VERDICT: the causal write was recognized as superseding, and the");
        println!("concurrent writes were flagged as a genuine conflict. last-writer-wins");
        println!("alone cannot tell these apart; the vector clock can.");
    } else {
        println!("VERDICT: only {correct}/2 classified correctly; something is wrong.");
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
