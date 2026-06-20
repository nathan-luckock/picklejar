//! HyperLogLog: estimate distinct memories in ~16 KiB, at any scale.
//!
//! ```text
//! cargo run --release --bin hllsim
//! ```

#![allow(clippy::doc_markdown)] // "HyperLogLog" reads as prose

use std::process::ExitCode;

use picklejar::hll::HyperLogLog;

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn main() -> ExitCode {
    println!("\n=============== HYPERLOGLOG ===============");
    println!("count distinct memories in kilobytes, at any scale\n");

    let mut hll = HyperLogLog::new();
    let distinct = 1_000_000u64;
    // A stream of 5 million events over 1 million distinct memories (each
    // distinct memory seen five times).
    for _round in 0..5u64 {
        for i in 0..distinct {
            hll.add(&i.to_be_bytes());
        }
    }

    let est = hll.estimate();
    let err = (est - distinct as f64).abs() / distinct as f64;
    println!("observed 5,000,000 events over {distinct} distinct memories");
    println!(
        "  exact set would need ~{} MiB; HyperLogLog used ~16 KiB",
        distinct * 8 / 1_048_576
    );
    println!(
        "  estimate: {:.0}  (relative error {:.2}%)",
        est,
        err * 100.0
    );

    println!("\n==================================================");
    if err < 0.03 {
        println!(
            "VERDICT: estimated {:.0} distinct against a true {distinct}, within {:.2}%,",
            est,
            err * 100.0
        );
        println!("in a fixed ~16 KiB no matter how large the stream grows. two nodes can");
        println!("merge their estimators register-wise to count their union.");
    } else {
        println!(
            "VERDICT: estimate {est:.0} off by {:.2}%; unexpected.",
            err * 100.0
        );
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
