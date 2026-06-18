//! The scrubber a deployment runs on a schedule: heal any corrupt heap pages
//! from parity, then refresh the parity snapshot so the next interval is covered.
//!
//! This is the operational counterpart to `resilientsim`, which models the dose
//! versus scrub cadence. A real node runs `pjscrub` from cron at that cadence so
//! latent corruption is repaired before a second fault on the same stripe makes it
//! unrecoverable. It works on a closed database (the engine must not be running),
//! so it never fights the live buffer pool's checksum-enforcing reads.
//!
//! ```text
//! cargo run --release --bin pjscrub -- mem.db          # heal + refresh (k=8, m=2)
//! cargo run --release --bin pjscrub -- mem.db 10 4     # with explicit k, m
//! ```

use std::path::Path;
use std::process::ExitCode;

use picklejar::Database;
use picklejar_storage::resilience::heal_file;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let Some(db_arg) = args.get(1) else {
        eprintln!("usage: pjscrub <db_path> [k] [m]");
        return ExitCode::FAILURE;
    };
    let db_path = Path::new(db_arg);
    let k: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);
    let m: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2);
    let parity_path = db_path.with_extension("parity");

    // 1. Heal corrupt pages from the existing parity, if any, reporting what it
    //    found. This runs on the raw file before the engine opens it.
    if parity_path.exists() {
        match heal_file(db_path, &parity_path) {
            Ok(report) => {
                println!(
                    "heal: {} pages checked, {} repaired from parity, {} stripes unrecoverable",
                    report.pages_checked, report.pages_repaired, report.stripes_unrecoverable
                );
                if report.stripes_unrecoverable > 0 {
                    eprintln!(
                        "warning: {} stripes had more corruption than parity could repair; \
                         that data is lost (and stays detectably corrupt, never served wrong)",
                        report.stripes_unrecoverable
                    );
                }
            }
            Err(e) => {
                eprintln!("heal failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        println!("heal: no parity sidecar yet; this run creates the first snapshot");
    }

    // 2. Reopen the (now healed) database and refresh the parity snapshot so the
    //    next interval's writes are covered.
    let mut db = match Database::open(db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("open failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    match db.protect(k, m) {
        Ok(report) => {
            println!(
                "protect: {} pages snapshotted (k={k}, m={m}), {} parity bytes written",
                report.protected_pages, report.parity_bytes
            );
            println!("scrub complete");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("protect failed: {e}");
            ExitCode::FAILURE
        }
    }
}
