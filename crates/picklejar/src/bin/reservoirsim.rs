//! Reservoir sampling: a uniform sample of a memory stream in one pass.
//!
//! ```text
//! cargo run --release --bin reservoirsim
//! ```

use std::process::ExitCode;

use picklejar::reservoir::Reservoir;

#[allow(clippy::cast_precision_loss)]
fn main() -> ExitCode {
    println!("\n=============== RESERVOIR SAMPLING ===============");
    println!("a uniform sample of a stream of unknown length, in one pass\n");

    let k = 20;
    let n = 1_000_000u64;
    let mut r = Reservoir::with_capacity(k, 0x00C0_FFEE);
    for i in 0..n {
        r.offer(i);
    }
    println!("streamed {n} memories, kept a uniform sample of {k} in {k} slots.");
    println!("  sample (memory ids): {:?}", r.sample());

    // Check the sample spreads across the stream rather than clustering.
    let mut sorted = r.sample().to_vec();
    sorted.sort_unstable();
    let spread = sorted.last().unwrap_or(&0) - sorted.first().unwrap_or(&0);
    let mean = sorted.iter().sum::<u64>() as f64 / k as f64;

    println!(
        "  sample spans ids {}..={} (range {spread} of {n})",
        sorted[0],
        sorted[k - 1]
    );
    println!("  sample mean {:.0}, stream midpoint {}", mean, n / 2);

    println!("\n==================================================");
    // A uniform sample of 0..n should span most of the range and average near n/2.
    if spread > n / 2 && (mean - n as f64 / 2.0).abs() < n as f64 / 4.0 {
        println!("VERDICT: the {k}-item sample spreads across the whole {n}-item stream and");
        println!("averages near its midpoint, as a uniform sample should, using {k} slots");
        println!("and a single pass with no knowledge of the length in advance.");
    } else {
        println!("VERDICT: the sample looks skewed (range {spread}, mean {mean:.0}).");
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
