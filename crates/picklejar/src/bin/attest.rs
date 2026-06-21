//! The grand attestation: one content-hashed page proving every guarantee at once.
//!
//! ```text
//! cargo run --release --bin attest
//! ```

use std::process::ExitCode;

use picklejar::grandcert::attest;

fn main() -> ExitCode {
    let attestation = attest();
    print!("\n{}", attestation.render());
    if attestation.all_passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
