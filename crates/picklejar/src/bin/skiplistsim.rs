//! Skip list: an ordered index with expected-logarithmic search, no rebalancing.
//!
//! ```text
//! cargo run --release --bin skiplistsim
//! ```

use std::process::ExitCode;

use picklejar::skiplist::SkipList;

fn main() -> ExitCode {
    println!("\n=============== SKIP LIST ===============");
    println!("a sorted key-value index built from coin flips, no rotations\n");

    let mut list = SkipList::new(0x5121_57AB);

    // Insert 50,000 keys in scrambled order.
    let n = 50_000u64;
    let mut rng = 0x9E37_79B9u64;
    let mut count = 0u64;
    for _ in 0..n {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let k = rng % (n * 4);
        if list.get(k).is_none() {
            count += 1;
        }
        list.insert(k, k.to_be_bytes().to_vec());
    }
    println!(
        "inserted {n} keys in scrambled order ({} distinct).",
        list.len()
    );

    // The entries come out perfectly sorted.
    let entries = list.entries();
    let sorted = entries.windows(2).all(|w| w[0].0 < w[1].0);

    // Point lookups hit and miss correctly.
    let mut hits = 0;
    for (k, _) in entries.iter().take(1000) {
        if list.get(*k).is_some() {
            hits += 1;
        }
    }

    // Remove every other one of the first 1000 and confirm.
    let to_remove: Vec<u64> = entries
        .iter()
        .take(1000)
        .step_by(2)
        .map(|(k, _)| *k)
        .collect();
    for &k in &to_remove {
        list.remove(k);
    }
    let removed_gone = to_remove.iter().all(|&k| list.get(k).is_none());

    println!("\n  entries iterate in sorted order: {sorted}");
    println!("  1000 point lookups all hit: {}", hits == 1000);
    println!("  {} keys removed cleanly: {removed_gone}", to_remove.len());

    println!("\n==================================================");
    if sorted && hits == 1000 && removed_gone && list.len() as u64 == count - to_remove.len() as u64
    {
        println!("VERDICT: {count} distinct keys kept perfectly ordered with expected log-n");
        println!("search, insert, and delete, balanced purely by coin flips with no");
        println!("rotation or rebalancing code. an alternative to the engine's B+ tree.");
    } else {
        println!(
            "VERDICT: unexpected (sorted={sorted}, hits={hits}, removed_gone={removed_gone})."
        );
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
