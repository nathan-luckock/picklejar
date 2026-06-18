//! Operational snapshot backup: heal the database from parity if it can, then
//! copy a consistent snapshot of it to a destination, then close. The tool a
//! deployment runs from cron to ship snapshots off-node (to a ground station, or
//! a peer) so committed data survives whole-node loss.
//!
//! Restore is just opening the destination: `picklejar --database <dest>`.
//!
//! ```text
//! cargo run --release --bin pjbackup -- mem.db backups/mem-2026.db
//! ```

use std::path::Path;
use std::process::ExitCode;

use picklejar::Database;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (Some(src), Some(dest)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: pjbackup <db_path> <dest_base>");
        return ExitCode::FAILURE;
    };

    // Open through the self-healing path so a corrupt page is repaired from parity
    // before it is copied: the snapshot we ship is a healthy one.
    let mut db = match Database::open_resilient(Path::new(src)) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("open {src}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match db.backup(Path::new(dest)) {
        Ok(report) => {
            println!(
                "backed up {} files ({} bytes) to {dest}; restore with: picklejar --database {dest}",
                report.files, report.bytes
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("backup to {dest}: {e}");
            ExitCode::FAILURE
        }
    }
}
