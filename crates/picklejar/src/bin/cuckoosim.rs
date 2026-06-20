//! Cuckoo filter: deletable membership with a short fingerprint per memory.
//!
//! ```text
//! cargo run --release --bin cuckoosim
//! ```

use std::process::ExitCode;

use picklejar::cuckoo::CuckooFilter;

fn main() -> ExitCode {
    println!("\n=============== CUCKOO FILTER ===============");
    println!("deletable membership, one byte of fingerprint per memory\n");

    let mut cf = CuckooFilter::with_capacity(20_000);
    for i in 0..10_000u64 {
        cf.insert(&i.to_be_bytes());
    }
    println!(
        "stored {} memories (one byte of fingerprint each).",
        cf.len()
    );

    // Forget a few by removing their fingerprints.
    let forget = [1u64, 50, 9999];
    for &id in &forget {
        cf.remove(&id.to_be_bytes());
    }
    let gone = forget
        .iter()
        .filter(|&&id| !cf.contains(&id.to_be_bytes()))
        .count();
    println!("forgot {forget:?}: {gone}/{} now absent.", forget.len());

    // No false negatives for what remains.
    let present = (0..10_000u64)
        .filter(|&i| !forget.contains(&i))
        .filter(|&i| cf.contains(&i.to_be_bytes()))
        .count();

    // False positives on never-seen ids.
    let mut fps = 0u64;
    for i in 5_000_000u64..5_010_000 {
        if cf.contains(&i.to_be_bytes()) {
            fps += 1;
        }
    }
    let rate = f64::from(u32::try_from(fps).unwrap_or(0)) / 10_000.0;
    println!(
        "remaining present: {present}/{}, false-positive rate {:.2}%",
        10_000 - forget.len(),
        rate * 100.0
    );

    println!("\n==================================================");
    if gone == forget.len() && present == 10_000 - forget.len() && rate < 0.05 {
        println!("VERDICT: memories deleted cleanly, survivors all present (no false");
        println!("negatives), false-positive rate a few percent. deletable like the");
        println!("counting filter, but a fingerprint per item instead of byte counters.");
    } else {
        println!("VERDICT: unexpected (gone={gone}, present={present}, rate={rate:.4}).");
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
