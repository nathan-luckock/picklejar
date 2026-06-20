//! Blind vector search: the server ranks your nearest memories without ever
//! seeing a true embedding.
//!
//! ```text
//! cargo run --release --bin blindsim
//! ```

use std::process::ExitCode;

use picklejar::blindvec::{knn, Rotation};

fn vshow(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| format!("{x:+.2}")).collect();
    format!("[{}]", parts.join(", "))
}

fn main() -> ExitCode {
    println!("\n=============== BLIND VECTOR SEARCH ===============");
    println!("the server ranks your neighbors without seeing your embeddings\n");

    // The client's true memories, each with a label only the client knows.
    let memories: [(u64, &str, [f32; 4]); 5] = [
        (1, "dentist appointment", [0.10, 0.05, 0.00, 0.05]),
        (2, "quarterly board deck", [1.00, 1.00, 1.00, 1.00]),
        (3, "vacation photos", [5.00, 5.00, 5.00, 5.00]),
        (4, "call the dentist back", [0.20, 0.10, 0.00, 0.10]),
        (5, "tax documents", [2.00, 2.00, 2.00, 2.00]),
    ];
    let query_label = "schedule a dentist visit";
    let query = [0.12_f32, 0.06, 0.00, 0.06];

    // The client holds a secret rotation. The server never learns it.
    let secret = Rotation::from_seed(4, 0xB11D_5EED);

    println!("the client's true memory (kept secret) vs what the server stores:");
    for (id, label, v) in &memories {
        println!("  row {id}  {:<22} true {}", label, vshow(v));
        println!(
            "          {:<22} server sees {}",
            "",
            vshow(&secret.rotate(v))
        );
    }

    // Plaintext ranking, computed by the client for comparison only.
    let plain_db: Vec<(u64, Vec<f32>)> = memories
        .iter()
        .map(|(id, _, v)| (*id, v.to_vec()))
        .collect();
    let plain = knn(&plain_db, &query, 3);

    // The server's world: only rotated vectors, and a rotated query.
    let blind_db: Vec<(u64, Vec<f32>)> = memories
        .iter()
        .map(|(id, _, v)| (*id, secret.rotate(v)))
        .collect();
    let blind_query = secret.rotate(&query);

    println!("\nclient asks \"{query_label}\". it sends only the rotated query:");
    println!("  rotated query the server sees: {}", vshow(&blind_query));

    let blind = knn(&blind_db, &blind_query, 3);

    let label_of = |id: u64| {
        memories
            .iter()
            .find(|(i, _, _)| *i == id)
            .map_or("?", |(_, l, _)| *l)
    };
    println!("\nthe server returns its ranking (blind to the content):");
    for (rank, (id, dist)) in blind.iter().enumerate() {
        println!(
            "  {}. row {id}  distance {dist:.4}  -> client decodes: \"{}\"",
            rank + 1,
            label_of(*id)
        );
    }

    let plain_ids: Vec<u64> = plain.iter().map(|(id, _)| *id).collect();
    let blind_ids: Vec<u64> = blind.iter().map(|(id, _)| *id).collect();

    println!("\n==================================================");
    if plain_ids == blind_ids {
        println!("VERDICT: the blind ranking {blind_ids:?} matches the plaintext ranking");
        println!("{plain_ids:?} exactly, yet the server never held a single true coordinate.");
    } else {
        println!("VERDICT: rankings differ ({blind_ids:?} vs {plain_ids:?}); something is wrong.");
        return ExitCode::FAILURE;
    }
    println!("scope: the rotation hides the embeddings' content (the axes an inverter");
    println!("would need), not their geometry. the server still learns distances,");
    println!("because ranking needs them. only the client's key maps between spaces.");
    println!("==================================================\n");
    ExitCode::SUCCESS
}
