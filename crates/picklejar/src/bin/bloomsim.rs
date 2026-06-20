//! Bloom filter: a duplicate pre-check in a few bits per memory.
//!
//! ```text
//! cargo run --release --bin bloomsim
//! ```

use std::process::ExitCode;

use picklejar::bloom::BloomFilter;

fn main() -> ExitCode {
    println!("\n=============== BLOOM FILTER ===============");
    println!("\"have I already stored this memory?\" in a handful of bits\n");

    let n = 50_000u64;
    let target = 0.01;
    let mut seen = BloomFilter::with_capacity(usize::try_from(n).expect("fits"), target);
    for i in 0..n {
        seen.insert(&i.to_be_bytes());
    }

    #[allow(clippy::cast_precision_loss)]
    let bits_per_item = seen.bit_len() as f64 / n as f64;
    println!(
        "stored {n} memory ids using {} bits ({bits_per_item:.1} bits/item, {} probes each)",
        seen.bit_len(),
        seen.hash_count()
    );

    // No false negatives: everything inserted reads as present.
    let present = (0..n).filter(|i| seen.contains(&i.to_be_bytes())).count();

    // Measure false positives on ids that were never inserted.
    let mut fps = 0u64;
    for i in n..(2 * n) {
        if seen.contains(&i.to_be_bytes()) {
            fps += 1;
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let rate = fps as f64 / n as f64;

    println!("\n  inserted ids reported present: {present}/{n}");
    println!(
        "  never-seen ids falsely reported present: {fps}/{n} ({:.2}%)",
        rate * 100.0
    );

    println!("\n==================================================");
    if present as u64 == n && rate < target * 3.0 {
        println!("VERDICT: every stored memory was recognized (zero false negatives), and");
        println!(
            "the false-positive rate {:.2}% is near the {:.0}% target. a \"no\" is always",
            rate * 100.0,
            target * 100.0
        );
        println!("trusted, so a duplicate check skips the real store on the common case.");
    } else {
        println!("VERDICT: unexpected (present={present}, fp_rate={rate:.4}).");
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
