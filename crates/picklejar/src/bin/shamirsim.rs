//! Shamir secret sharing: split a memory key across nodes so any 3 of 5
//! reconstruct it and any 2 learn nothing.
//!
//! ```text
//! cargo run --release --bin shamirsim
//! ```

use std::process::ExitCode;

use picklejar::shamir::{combine, split, Share};

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for b in &bytes[..bytes.len().min(8)] {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn main() -> ExitCode {
    println!("\n=============== SHAMIR SECRET SHARING ===============");
    println!("no single node holds the memory key\n");

    let key: Vec<u8> = (0u8..32)
        .map(|i| i.wrapping_mul(7).wrapping_add(11))
        .collect();
    println!("the tenant's 32-byte memory key: {}...\n", hex(&key));

    let (k, n) = (3u8, 5u8);
    let shares = split(&key, k, n, 0x5EED_CAFE);
    println!("split {k}-of-{n} across nodes (each share is just random-looking bytes):");
    for s in &shares {
        println!("  node x={}  share {}...", s.x, hex(&s.y));
    }

    // Any 3 nodes come online and reconstruct.
    let three: Vec<Share> = vec![shares[0].clone(), shares[2].clone(), shares[4].clone()];
    let recovered = combine(&three);
    let ok = recovered == key;
    println!(
        "\nnodes 1, 3, 5 cooperate ({k} shares) -> recovered {}...  match: {ok}",
        hex(&recovered)
    );

    // Two nodes try, and get noise.
    let two: Vec<Share> = vec![shares[0].clone(), shares[1].clone()];
    let guessed = combine(&two);
    let leaked = guessed == key;
    println!(
        "nodes 1, 2 alone ({} shares) -> got {}...  match: {leaked}",
        two.len(),
        hex(&guessed)
    );

    println!("\n==================================================");
    if ok && !leaked {
        println!("VERDICT: any {k} of {n} nodes rebuild the key exactly; fewer recover only");
        println!(
            "noise. compromise or loss of up to {} nodes is survivable, and no single",
            n - k
        );
        println!("node ever held the whole secret.");
    } else {
        println!("VERDICT: something is wrong (recovered={ok}, leaked={leaked}).");
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
