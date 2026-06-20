//! Forward-secure audit log: a seized node cannot rewrite its own past.
//!
//! ```text
//! cargo run --release --bin forwardlogsim
//! ```

use std::process::ExitCode;

use picklejar::authmem::sha256;
use picklejar::captoken::hmac_sha256;
use picklejar::forwardlog::{verify, ForwardSecureLog};

fn signed(seq: u64, msg: &[u8]) -> Vec<u8> {
    let mut b = seq.to_be_bytes().to_vec();
    b.extend_from_slice(msg);
    b
}

fn main() -> ExitCode {
    println!("\n=============== FORWARD-SECURE AUDIT LOG ===============");
    println!("a seized node cannot forge what it logged before the seizure\n");

    let initial = [0xA7u8; 32]; // the verifier pins this
    let mut log = ForwardSecureLog::new(initial);
    for m in [
        "boot",
        "tenant acme attached",
        "key rotated",
        "tenant globex attached",
        "scrub ok",
    ] {
        log.append(m.as_bytes());
    }
    println!(
        "logged {} entries. an auditor pinned the initial key.",
        log.entries().len()
    );
    println!(
        "  honest verification: {}",
        if verify(initial, log.entries()).is_ok() {
            "PASS"
        } else {
            "FAIL"
        }
    );

    // The node is physically seized after entry 5. The attacker learns only the
    // live key (every earlier key was ratcheted away).
    let stolen = log.current_key();
    println!(
        "\nnode seized. attacker holds only the current key: {:02x}{:02x}...",
        stolen[0], stolen[1]
    );
    println!("they try to rewrite entry 2 (\"key rotated\") to hide an exfiltration...");

    let mut entries = log.entries().to_vec();
    entries[2].message = b"nothing happened here".to_vec();

    // The attacker can only ratchet the stolen key forward, never backward.
    let mut forge_key = stolen;
    let mut forged = false;
    for _ in 0..1000 {
        entries[2].tag = hmac_sha256(&forge_key, &signed(2, &entries[2].message));
        if verify(initial, &entries).is_ok() {
            forged = true;
            break;
        }
        forge_key = sha256::hash(&forge_key);
    }

    println!("  tried 1000 forward-derived keys -> forged a valid past entry: {forged}");
    println!(
        "  auditor re-verifies the tampered log: {}",
        if verify(initial, &entries).is_ok() {
            "PASS"
        } else {
            "REJECTED at the edited entry"
        }
    );

    println!("\n==================================================");
    if forged {
        println!("VERDICT: a past entry was forged; forward security is broken.");
        return ExitCode::FAILURE;
    }
    println!("VERDICT: the attacker who seized the live key could not forge a single");
    println!("pre-seizure entry, because each entry's key was hashed forward and erased,");
    println!("and the hash cannot be run backward. the past is sealed even after capture.");
    println!("==================================================\n");
    ExitCode::SUCCESS
}
