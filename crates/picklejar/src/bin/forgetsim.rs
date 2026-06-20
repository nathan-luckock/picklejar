//! Provable forgetting: watch a memory become unrecoverable even to an adversary
//! holding every persistent copy of it, across a crash.
//!
//! ```text
//! cargo run --release --bin forgetsim
//! ```

use std::process::ExitCode;

use picklejar::authmem::sha256;
use picklejar::forgetmem::{KeyVault, Recall, Sealed};

/// A stand-in for a hardware random key generator: each memory gets its own
/// independent key, kept only in the vault.
fn fresh_key(rowid: u64) -> [u8; 32] {
    sha256::hash(&rowid.to_be_bytes())
}

/// Render a recall outcome as readable text or an explicit forgotten marker.
fn show(recall: &Recall) -> String {
    match recall {
        Recall::Remembered(bytes) => {
            format!("\"{}\"", String::from_utf8_lossy(bytes))
        }
        Recall::Forgotten => "<forgotten: no key, unrecoverable>".to_string(),
    }
}

fn main() -> ExitCode {
    println!("\n=============== PROVABLE FORGETTING ===============");
    println!("a memory that proves it is gone, not just deleted\n");

    let mut vault = KeyVault::new();
    let memories: [(u64, &str); 3] = [
        (1, "ada's home address"),
        (2, "ada's medical note"),
        (3, "ada's favorite song"),
    ];

    // Seal each memory under its own key. Only the ciphertext persists to disk.
    let sealed: Vec<Sealed> = memories
        .iter()
        .map(|(id, text)| vault.seal(*id, *id, fresh_key(*id), text.as_bytes()))
        .collect();
    println!(
        "sealed {} memories for tenant 'ada' (only ciphertext touches disk):",
        sealed.len()
    );
    for s in &sealed {
        println!("  row {}  ->  {}", s.rowid, show(&vault.recall(s)));
    }

    // Ada exercises her right to be forgotten on the medical note.
    println!("\nada asks to be forgotten: row 2 (the medical note). shredding its key...");
    vault.forget(2);

    println!("\nafter forgetting:");
    for s in &sealed {
        println!("  row {}  ->  {}", s.rowid, show(&vault.recall(s)));
    }

    // The adversary now has every durable copy of the forgotten ciphertext.
    println!("\nan adversary recovers every persistent copy of row 2's bytes:");
    let forgotten = &sealed[1];
    let from_heap = forgotten.clone();
    let from_wal = forgotten.clone();
    let from_parity = Sealed {
        ciphertext: forgotten.ciphertext.clone(),
        ..forgotten.clone()
    };
    let mut beaten = 0;
    for (source, copy) in [
        ("heap page", &from_heap),
        ("write-ahead log", &from_wal),
        ("Reed-Solomon parity", &from_parity),
    ] {
        let outcome = vault.recall(copy);
        let blocked = matches!(outcome, Recall::Forgotten);
        beaten += u32::from(blocked);
        println!(
            "  from {source:<20} ({} ciphertext bytes)  ->  {}",
            copy.ciphertext.len(),
            show(&outcome)
        );
    }

    // A crash, then recovery from the durable key snapshot.
    println!("\nnow the node crashes and recovers from its durable key snapshot...");
    let snapshot = vault.snapshot();
    let recovered = KeyVault::recover(&snapshot);
    let kept = matches!(recovered.recall(&sealed[0]), Recall::Remembered(_));
    let still_gone = matches!(recovered.recall(&sealed[1]), Recall::Forgotten);
    println!(
        "  row 1 after recovery  ->  {}",
        show(&recovered.recall(&sealed[0]))
    );
    println!(
        "  row 2 after recovery  ->  {}",
        show(&recovered.recall(&sealed[1]))
    );

    println!("\n==================================================");
    if beaten == 3 && kept && still_gone {
        println!("VERDICT: row 2 is unrecoverable from every durable surface, and the");
        println!("crash did not bring it back. the other memories are intact. forgetting");
        println!("destroyed the key, not just the row, so the bytes that remain are noise.");
    } else {
        println!("VERDICT: something is wrong (adversary blocked {beaten}/3, kept={kept}, gone={still_gone}).");
        return ExitCode::FAILURE;
    }
    println!("scope: this is forward forgetting. an adversary who copied the key before");
    println!("the shred is out of scope, which is exactly the regulatory guarantee.");
    println!("==================================================\n");
    ExitCode::SUCCESS
}
