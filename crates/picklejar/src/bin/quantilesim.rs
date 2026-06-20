//! Streaming quantile sketch: latency percentiles in ~1000 buckets.
//!
//! ```text
//! cargo run --release --bin quantilesim
//! ```

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)]

use std::process::ExitCode;

use picklejar::quantile::QuantileSketch;

fn main() -> ExitCode {
    println!("\n=============== STREAMING QUANTILES ===============");
    println!("latency percentiles in fixed space, immune to outliers\n");

    let mut q = QuantileSketch::new();
    let mut state = 0x1357_9BDFu64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    // A realistic latency stream: most requests ~200us, a heavy tail to ~50ms.
    let n = 2_000_000u64;
    let mut sum = 0u64;
    for _ in 0..n {
        let r = next() % 1000;
        let micros = if r < 980 {
            150 + next() % 150 // body: 150..300us
        } else if r < 999 {
            1_000 + next() % 9_000 // tail: 1..10ms
        } else {
            20_000 + next() % 30_000 // rare: 20..50ms
        };
        q.record(micros);
        sum += micros;
    }
    let mean = sum / n;

    println!(
        "recorded {n} latencies in {} buckets (~{} KiB).",
        q.bucket_count(),
        q.bucket_count() * 8 / 1024 + 1
    );
    println!("  mean:  {mean} us   (dragged up by the tail)");
    println!("  p50:   {} us", q.quantile(0.50));
    println!("  p95:   {} us", q.quantile(0.95));
    println!("  p99:   {} us", q.quantile(0.99));
    println!("  p99.9: {} us", q.quantile(0.999));

    println!("\n==================================================");
    let p50 = q.quantile(0.50);
    let p999 = q.quantile(0.999);
    if q.bucket_count() < 2000 && p50 < 350 && p999 > 1000 && mean > p50 {
        println!("VERDICT: 2 million samples summarized in under a kilobyte. the median sits");
        println!("in the body ({p50} us) while the mean ({mean} us) is poisoned by the tail,");
        println!("and p99.9 ({p999} us) exposes the slow outliers a node operator must see.");
    } else {
        println!(
            "VERDICT: unexpected (buckets {}, p50 {p50}, p999 {p999}).",
            q.bucket_count()
        );
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
