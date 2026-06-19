//! Measure the engine's detection coverage across the four storage-write fault
//! classes: bit flip, torn write, lost write, and misdirected write. Each is
//! injected into well-formed pages and run through the engine's layered
//! page-integrity check (the payload checksum, then the LSN-versus-log guard).
//!
//! ```text
//! cargo run --release --bin faultsim          # 2000 trials per class
//! cargo run --release --bin faultsim -- 50000 # 50k trials per class
//! ```

use std::process::ExitCode;

use picklejar::faults::run_fault_coverage;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let per_class: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2000);

    let cov = run_fault_coverage(0xFA17, per_class);
    println!("storage-fault detection coverage ({per_class} trials per class)\n");
    println!(
        "  bit flip          {:>6.1}%  (payload checksum)",
        cov.bit_flip * 100.0
    );
    println!(
        "  torn write        {:>6.1}%  (payload checksum)",
        cov.torn_write * 100.0
    );
    println!(
        "  lost write        {:>6.1}%  (LSN-versus-log guard)",
        cov.lost_write * 100.0
    );
    println!(
        "  misdirected write {:>6.1}%  (partial: LSN guard only; needs a page-id guard)",
        cov.misdirected_write * 100.0
    );

    // Bit flip, torn, and lost writes must be caught completely; the misdirected
    // residual is reported, not asserted, because the page format has no
    // self-identifying id yet (recorded on the roadmap).
    if cov.bit_flip >= 1.0 && cov.torn_write >= 1.0 && cov.lost_write >= 1.0 {
        println!(
            "\nresult: bit flip, torn write, and lost write fully detected; \
             misdirected write {:.1}% (the page-id-guard residual)",
            cov.misdirected_write * 100.0
        );
        ExitCode::SUCCESS
    } else {
        eprintln!("\nresult: a fault class regressed below full detection");
        ExitCode::FAILURE
    }
}
