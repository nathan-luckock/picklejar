//! Proof of retrievability: challenge an unreachable node to prove it still holds
//! your memories, then watch a node that quietly lost some get caught.
//!
//! ```text
//! cargo run --release --bin retrievesim
//! ```

use std::process::ExitCode;

use picklejar::retrieval::{challenge, detection_probability, verify, Store};

fn main() -> ExitCode {
    println!("\n=============== PROOF OF RETRIEVABILITY ===============");
    println!("make the node you cannot reach prove it still has your data\n");

    // The client uploads memory chunks and keeps only the 32-byte commitment.
    let total = 32;
    let chunks: Vec<Vec<u8>> = (0..total)
        .map(|i| format!("memory chunk {i}: ...embedding and payload...").into_bytes())
        .collect();
    let node = Store::new(chunks.clone());
    let commitment = node.commit();
    println!("uploaded {total} memory chunks. the client keeps only the commitment:");
    println!("  pinned commitment, then forgets the data.\n");

    // Round 1: the honest node answers a random spot-check.
    let q = 6;
    let ch = challenge(0x1111, node.len(), q);
    let answer = node.answer(&ch);
    println!("client spot-checks {q} random chunks {ch:?} (never downloading all {total}):");
    match verify(commitment, &ch, &answer) {
        Ok(()) => {
            println!("  honest node: PASS, every challenged chunk proved against the commitment\n");
        }
        Err(e) => {
            println!("  honest node unexpectedly failed: {e}");
            return ExitCode::FAILURE;
        }
    }

    // A second node has silently lost some chunks to bit-rot.
    let lost = 6;
    let mut rotted = Store::new(chunks);
    for i in [3, 9, 14, 21, 27, 30] {
        rotted.corrupt(i, b"<rotted>");
    }
    let p = detection_probability(lost, total, q);
    println!("a second node has silently rotted {lost} of {total} chunks.");
    println!(
        "one {q}-chunk challenge catches it with probability {:.1}%:",
        p * 100.0
    );

    let mut caught_round = None;
    for round in 1..=5u64 {
        let ch = challenge(round.wrapping_mul(0x9E37_79B9), rotted.len(), q);
        let answer = rotted.answer(&ch);
        match verify(commitment, &ch, &answer) {
            Ok(()) => {
                println!("  round {round}: challenge {ch:?} -> passed (missed the rot this round)");
            }
            Err(e) => {
                println!("  round {round}: challenge {ch:?} -> CAUGHT: {e}");
                caught_round = Some(round);
                break;
            }
        }
    }

    println!("\n==================================================");
    if let Some(round) = caught_round {
        let cumulative = 1.0 - (1.0 - p).powi(i32::try_from(round).unwrap_or(1));
        println!("VERDICT: the rotted node was caught in round {round}. across {round} rounds the");
        println!(
            "client's chance of catching it was {:.1}%, while it only ever downloaded a",
            cumulative * 100.0
        );
        println!("handful of chunks. you never touched the disk; you made it prove itself.");
    } else {
        println!(
            "VERDICT: not caught in 5 rounds (unlucky); more rounds drive detection to near 1."
        );
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
