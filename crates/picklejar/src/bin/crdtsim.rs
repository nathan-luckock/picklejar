//! Conflict-free replicated memory: two partitioned nodes edit offline and merge
//! to the identical state, with a concurrent conflict resolved the same way on
//! both, without coordination.
//!
//! ```text
//! cargo run --release --bin crdtsim
//! ```

use std::process::ExitCode;

use picklejar::crdtmem::Replica;

fn text(value: Option<&[u8]>) -> String {
    value.map_or_else(
        || "<deleted>".to_string(),
        |b| format!("\"{}\"", String::from_utf8_lossy(b)),
    )
}

fn dump(label: &str, r: &Replica) {
    println!("  {label}:");
    for (id, slot) in r.slots() {
        println!("    memory {id}  ->  {}", text(slot.value.as_deref()));
    }
}

fn main() -> ExitCode {
    println!("\n=============== REPLICATED MEMORY (CRDT) ===============");
    println!("two partitioned nodes edit offline, then merge without conflict\n");

    // Two orbital nodes that cannot reach each other right now.
    let mut node_a = Replica::new(1);
    let mut node_b = Replica::new(2);

    // Both start from a shared memory they synced earlier.
    node_a.set(1, b"ground pass: 14:30Z");
    node_b.merge(&node_a);
    println!("both nodes share one memory, then the link drops.\n");

    // Offline edits on each side, including a concurrent conflict on memory 2.
    node_a.set(2, b"fuel reserve: 80%");
    node_a.set(3, b"antenna: nominal");

    node_b.set(2, b"fuel reserve: 76%"); // concurrent conflict on memory 2
    node_b.set(4, b"thermal anomaly logged");
    node_b.remove(1); // B retires the shared memory

    println!("while partitioned, each node edits locally:");
    dump("node A", &node_a);
    dump("node B", &node_b);
    let diverged = !node_a.converged_with(&node_b);
    println!("\n  diverged: {diverged}\n");

    // The link returns. Each node merges the other, in opposite orders.
    println!("the link returns. each node merges the other (in opposite orders)...\n");
    let mut a_then_b = node_a.clone();
    a_then_b.merge(&node_b);
    let mut b_then_a = node_b.clone();
    b_then_a.merge(&node_a);

    dump("node A after merge", &a_then_b);
    dump("node B after merge", &b_then_a);

    let converged = a_then_b.converged_with(&b_then_a);
    let conflict = text(a_then_b.get(2));

    println!("\n==================================================");
    if converged {
        println!("VERDICT: both nodes converged to the identical state, despite merging");
        println!("in opposite orders. the concurrent conflict on memory 2 resolved to");
        println!("{conflict} on both, by the same rule, with no coordination.");
    } else {
        println!("VERDICT: the nodes did not converge; something is wrong.");
        return ExitCode::FAILURE;
    }
    println!("merge is a semilattice join: commutative, associative, idempotent, so");
    println!("any replicas that saw the same edits agree no matter how they gossiped.");
    println!("==================================================\n");
    ExitCode::SUCCESS
}
