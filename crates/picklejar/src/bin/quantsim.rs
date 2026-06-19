//! Benchmark drift-adaptive vector quantization: a stream of embeddings whose
//! magnitude drifts over time is fed into two quantized indexes calibrated on the
//! same early sample, one static and one drift-adaptive, and both are scored
//! against the exact full-precision oracle. The adaptive index holds recall near
//! the ceiling by recalibrating rarely; the static one clips the drifted vectors
//! and collapses.
//!
//! ```text
//! cargo run --release --bin quantsim          # a few seeds
//! cargo run --release --bin quantsim -- 20    # twenty seeds
//! ```

use std::process::ExitCode;

use picklejar::quantize::run_drift_benchmark;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let seeds: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5);

    println!("drift-adaptive vector quantization: recall under distribution drift");
    println!("(scale drift over a 1500-vector stream, recall@10, 4x compression)\n");

    let mut worst_adaptive = f32::INFINITY;
    let mut best_static = 0.0f32;
    for seed in 0..seeds {
        let b = run_drift_benchmark(seed);
        worst_adaptive = worst_adaptive.min(b.adaptive_recall);
        best_static = best_static.max(b.static_recall);
        println!(
            "  seed {seed:>2}: adaptive recall {:.3}  static recall {:.3}  \
             ({} recalibrations over {} inserts)",
            b.adaptive_recall, b.static_recall, b.recalibrations, b.vectors
        );
    }

    println!(
        "\nworst-case adaptive recall {worst_adaptive:.3}, best-case static recall {best_static:.3}"
    );
    if worst_adaptive > 0.85 && best_static < 0.20 {
        println!(
            "result: drift-adaptive quantization holds recall flat where the static \
             quantizer collapses, at 4x compression"
        );
        ExitCode::SUCCESS
    } else {
        eprintln!("result: the drift-adaptation margin did not hold");
        ExitCode::FAILURE
    }
}
