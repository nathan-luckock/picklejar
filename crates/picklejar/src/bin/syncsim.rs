//! Merkle anti-entropy: two replicas reconcile by exchanging hashes along only
//! the divergent paths, not the whole data set.
//!
//! ```text
//! cargo run --release --bin syncsim
//! ```

use std::process::ExitCode;

use picklejar::antientropy::MerkleSet;

fn main() -> ExitCode {
    println!("\n=============== MERKLE ANTI-ENTROPY ===============");
    println!("find what two replicas disagree on without shipping it all\n");

    let depth = 12; // 4096 buckets
    let n = 3000u64;
    let base: Vec<(u64, Vec<u8>)> = (0..n)
        .map(|k| (k, format!("memory {k}").into_bytes()))
        .collect();

    let node_a = MerkleSet::from_entries(depth, &base);
    let mut node_b = MerkleSet::from_entries(depth, &base);

    // While partitioned, node B changed a few memories and learned a new one.
    node_b.insert(42, b"memory 42 (revised during partition)");
    node_b.insert(1337, b"memory 1337 (revised)");
    node_b.insert(n + 7, b"a memory only B has");

    println!(
        "each replica holds ~{n} memories. roots match: {}",
        node_a.root() == node_b.root()
    );

    let (keys, compares) = node_a.diff(&node_b);
    let leaves = 1usize << depth;
    println!("\nreconciling by walking the trees top-down...");
    println!("  divergent memories found: {keys:?}");
    println!("  tree-node hashes compared: {compares}  (out of {leaves} leaf buckets)");

    println!("\n==================================================");
    let expected = vec![42, 1337, n + 7];
    if keys == expected && compares < leaves / 10 {
        println!(
            "VERDICT: the exact {} divergent memories were found by comparing {compares}",
            keys.len()
        );
        println!("hashes, a tiny fraction of the {leaves} buckets. identical subtrees were");
        println!("skipped after a single hash check. the link only carries the differences.");
    } else {
        println!("VERDICT: unexpected result (keys={keys:?}, compares={compares}).");
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
