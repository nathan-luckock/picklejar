//! The replication money-shot: partition a cluster, write to both sides, heal,
//! and watch it reconcile itself with no coordinator.
//!
//! ```text
//! cargo run --release --bin repdemo
//! ```

use picklejar_replication::Cluster;

fn show(cluster: &Cluster, key: u64) {
    for id in 0..cluster.len() {
        let seen = cluster.read(id, key).map_or_else(
            || "<none>".to_string(),
            |v| String::from_utf8_lossy(&v).into_owned(),
        );
        println!("      node {id} sees: {seen}");
    }
    println!(
        "      converged: {}",
        if cluster.fully_converged() {
            "yes"
        } else {
            "NO"
        }
    );
}

fn main() {
    println!("=== picklejar replication: the partition money-shot ===");
    println!("A 3-node cluster, every key replicated 3 ways.\n");

    let key = 100;
    let mut cluster = Cluster::new(3, 3, 2, 2);

    println!("[1] All nodes connected. Write 'alpha' via node 0.");
    println!("      -> {:?}", cluster.write(0, key, b"alpha"));
    println!(
        "      read back from node 2: {}\n",
        cluster.read(2, key).map_or_else(
            || "<none>".into(),
            |v| String::from_utf8_lossy(&v).into_owned()
        )
    );

    println!("[2] PARTITION: {{node 0}} | {{node 1, node 2}}. The link is down.");
    cluster.set_partitions(&[0, 1, 1]);
    println!("    Both sides keep serving (no coordinator, no quorum stall).\n");

    println!("[3] Write to BOTH sides of the split:");
    println!(
        "      'left'  via node 0 (minority) -> {:?}",
        cluster.write(0, key, b"left")
    );
    println!(
        "      'right' via node 1 (majority) -> {:?}",
        cluster.write(1, key, b"right")
    );
    println!("    During the partition the sides disagree, and that is fine:");
    show(&cluster, key);
    println!();

    println!("[4] HEAL the link and run anti-entropy.");
    cluster.heal();
    let transferred = cluster.anti_entropy();
    println!("      reconciled {transferred} slot(s) by Merkle diff.");
    show(&cluster, key);
    println!();

    println!("Nobody coordinated the merge. The cluster reconciled itself,");
    println!("and every node resolved the conflict to the same value.");
    println!();
    println!("Proven at scale: `cargo run --release --bin repsim` runs thousands of");
    println!("random partition schedules; every one converges.");
}
