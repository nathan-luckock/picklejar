//! Verifiable history: watch a tamper-evident ledger catch three forgeries,
//! including a sophisticated one that re-chains the whole history.
//!
//! ```text
//! cargo run --release --bin ledgersim
//! ```

use std::process::ExitCode;

use picklejar::ledger::{entry_hash, value_hash, Ledger, Op};

/// A short hex prefix of a hash, for display.
fn short(hash: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for b in &hash[..6] {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn build() -> Ledger {
    let mut l = Ledger::new();
    l.record(Op::Insert, 1, b"ada's address: 12 Oak St");
    l.record(Op::Update, 1, b"ada's address: 9 Elm Ave");
    l.record(Op::Insert, 2, b"bob's note: call dentist");
    l.record(Op::Delete, 1, b"");
    l
}

fn main() -> ExitCode {
    println!("\n=============== VERIFIABLE HISTORY ===============");
    println!("a tamper-evident ledger of memory operations\n");

    let honest = build();
    let pinned = honest.head();
    println!(
        "recorded {} operations. an auditor pins the head:",
        honest.len()
    );
    println!("  pinned head: {}...\n", short(&pinned));
    for e in honest.entries() {
        println!(
            "  v{}  {:?} row {}  value {}...",
            e.seq,
            e.op,
            e.rowid,
            short(&e.value_hash)
        );
    }
    println!("\nan honest audit of the untouched history:");
    println!("  {}", audit_line(&honest));
    println!(
        "  matches pinned head: {}\n",
        honest.matches_pinned(&pinned)
    );

    let mut caught = 0;

    // Attack 1: edit a past value in place, without re-chaining.
    println!("attack 1: secretly rewrite the value at version 1 (in place)");
    {
        let genesis = honest.genesis();
        let mut entries = honest.entries().to_vec();
        entries[1].value_hash = value_hash(b"ada's address: SECRET BUNKER");
        let forged = Ledger::from_entries(genesis, entries);
        match forged.audit() {
            Err(f) => {
                caught += 1;
                println!("  [CAUGHT by audit] {f}");
            }
            Ok(()) => println!("  [MISSED]"),
        }
    }

    // Attack 2: drop an entry from the middle of the history.
    println!("\nattack 2: delete version 2 from the history entirely");
    {
        let genesis = honest.genesis();
        let mut entries = honest.entries().to_vec();
        entries.remove(2);
        let forged = Ledger::from_entries(genesis, entries);
        match forged.audit() {
            Err(f) => {
                caught += 1;
                println!("  [CAUGHT by audit] {f}");
            }
            Ok(()) => println!("  [MISSED]"),
        }
    }

    // Attack 3: the sophisticated forger. Edit version 1, then recompute every
    // later hash so the chain is internally perfect.
    println!("\nattack 3: rewrite version 1 AND re-chain every later entry to match");
    {
        let genesis = honest.genesis();
        let mut entries = honest.entries().to_vec();
        entries[1].value_hash = value_hash(b"ada's address: SECRET BUNKER");
        let mut prev = entries[0].hash;
        for e in entries.iter_mut().skip(1) {
            e.prev = prev;
            e.hash = entry_hash(e.seq, e.op, e.rowid, &e.value_hash, &e.prev);
            prev = e.hash;
        }
        let forged = Ledger::from_entries(genesis, entries);
        let audit_ok = forged.audit().is_ok();
        let head_ok = forged.matches_pinned(&pinned);
        println!("  internal audit passes: {audit_ok}  (the forgery is self-consistent)");
        println!(
            "  matches pinned head:   {head_ok}  (forged head: {}...)",
            short(&forged.head())
        );
        if audit_ok && !head_ok {
            caught += 1;
            println!("  [CAUGHT by pinned head] the rewrite cannot reproduce the pinned head");
        } else {
            println!("  [MISSED]");
        }
    }

    println!("\n=============== Entry as_of a version ===============");
    if let Some(e) = honest.entry_at(2) {
        println!(
            "at version 2, row {} was {:?} (value {}...)",
            e.rowid,
            e.op,
            short(&e.value_hash)
        );
    }

    println!("\n==================================================");
    if caught == 3 {
        println!("VERDICT: all 3 forgeries caught. lazy edits are pinpointed by audit;");
        println!("a fully re-chained rewrite is self-consistent but cannot reproduce the");
        println!("pinned head. internal consistency is not enough; pin the head.");
    } else {
        println!("VERDICT: only {caught}/3 forgeries caught. something is wrong.");
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}

/// Render the audit outcome as a line.
fn audit_line(l: &Ledger) -> String {
    match l.audit() {
        Ok(()) => "audit: intact".to_string(),
        Err(f) => format!("audit: {f}"),
    }
}
