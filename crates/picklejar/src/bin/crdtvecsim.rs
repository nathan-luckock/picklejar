//! CRDT vector index: two partitioned nodes merge their similarity indexes and
//! agree on every nearest-neighbor result.
//!
//! ```text
//! cargo run --release --bin crdtvecsim
//! ```

use std::process::ExitCode;

use picklejar::crdtvec::CrdtVectorIndex;

fn ids(hits: &[(u64, f64)]) -> Vec<u64> {
    hits.iter().map(|(id, _)| *id).collect()
}

fn main() -> ExitCode {
    println!("\n=============== CRDT VECTOR INDEX ===============");
    println!("two disconnected nodes merge their similarity indexes, no coordinator\n");

    let mut node_a = CrdtVectorIndex::new(1);
    let mut node_b = CrdtVectorIndex::new(2);

    // While partitioned, each node indexes its own embeddings, with a conflict
    // on memory 5 (both re-embed it differently).
    node_a.insert(1, vec![0.0, 0.0]);
    node_a.insert(2, vec![1.0, 0.5]);
    node_a.insert(5, vec![9.0, 9.0]);

    node_b.insert(3, vec![0.2, 0.1]);
    node_b.insert(4, vec![8.0, 8.0]);
    node_b.insert(5, vec![0.3, 0.3]); // concurrent re-embed of memory 5

    println!("node A indexed memories {{1,2,5}}, node B indexed {{3,4,5}} (5 conflicts).");

    // The link returns; each merges the other, in opposite orders.
    let mut a_then_b = node_a.clone();
    a_then_b.merge(&node_b);
    let mut b_then_a = node_b.clone();
    b_then_a.merge(&node_a);

    let query = [0.1_f32, 0.1];
    let knn_ab = a_then_b.knn(&query, 3);
    let knn_ba = b_then_a.knn(&query, 3);

    println!("\nafter merging in opposite orders, query nearest-3 to [0.1, 0.1]:");
    println!("  node A: {:?}", ids(&knn_ab));
    println!("  node B: {:?}", ids(&knn_ba));

    let converged = a_then_b.converged_with(&b_then_a);
    let agree = knn_ab == knn_ba;

    println!("\n==================================================");
    if converged && agree {
        println!("VERDICT: both nodes converged to the identical index and return the same");
        println!(
            "nearest neighbors {:?}, despite merging in opposite orders and a concurrent",
            ids(&knn_ab)
        );
        println!("re-embedding of memory 5. a replicated similarity index, conflict-free.");
    } else {
        println!("VERDICT: divergence (converged={converged}, agree={agree}).");
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
