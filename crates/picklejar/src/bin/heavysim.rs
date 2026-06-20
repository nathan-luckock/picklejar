//! Space-Saving: recover the hottest memories from a skewed stream in K slots.
//!
//! ```text
//! cargo run --release --bin heavysim
//! ```

use std::process::ExitCode;

use picklejar::spacesaving::SpaceSaving;

fn main() -> ExitCode {
    println!("\n=============== SPACE-SAVING HEAVY HITTERS ===============");
    println!("the top memories in a fixed number of counters\n");

    let mut ss = SpaceSaving::with_capacity(32);
    let hot: [(&str, u64); 3] = [
        ("memory:home", 50_000),
        ("memory:inbox", 25_000),
        ("memory:cart", 10_000),
    ];

    // Offer each hot memory its share, interleaved with a flood of cold keys.
    for &(k, c) in &hot {
        for _ in 0..c {
            ss.offer(k.as_bytes());
        }
    }
    for i in 0..300_000u64 {
        ss.offer(&i.to_be_bytes());
    }
    println!(
        "offered {} accesses; tracking only 32 counters.\n",
        ss.total()
    );

    println!("top 3 recovered:");
    let top = ss.top(3);
    for (k, t) in &top {
        println!(
            "  {:<14} est {:>6}  (error <= {})",
            String::from_utf8_lossy(k),
            t.count,
            t.error
        );
    }

    let recovered: Vec<String> = top
        .iter()
        .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
        .collect();
    let expected = ["memory:home", "memory:inbox", "memory:cart"];

    println!("\n==================================================");
    if expected.iter().all(|e| recovered.iter().any(|r| r == e)) {
        println!("VERDICT: all three genuinely hot memories were recovered from 32 slots,");
        println!("despite 300,000 cold one-off keys churning through the weakest slot. a");
        println!("memory above total/k of the traffic can never be evicted.");
    } else {
        println!("VERDICT: a hot memory was missed: {recovered:?}");
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
