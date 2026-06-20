//! Hyperplane LSH: similar embeddings land in one bucket, turning a similarity
//! search into a hash lookup over a few candidates.
//!
//! ```text
//! cargo run --release --bin lshsim
//! ```

use std::process::ExitCode;

use picklejar::lsh::{Lsh, LshIndex};

struct Rng(u64);
impl Rng {
    #[allow(clippy::cast_precision_loss)]
    fn unit(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 40) as f32 / 16_777_216.0
    }
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn main() -> ExitCode {
    println!("\n=============== HYPERPLANE LSH ===============");
    println!("similar embeddings collide in a bucket; search becomes a lookup\n");

    let dims = 32;
    let bits = 16;
    let mut idx = LshIndex::new(Lsh::new(dims, bits, 0x1234_5678));
    let mut rng = Rng(0xA5A5);

    // Ten clusters of 100 vectors each, 1000 total.
    let clusters = 10;
    let per = 100;
    let mut centers: Vec<Vec<f32>> = Vec::new();
    for _ in 0..clusters {
        centers.push((0..dims).map(|_| rng.unit().mul_add(2.0, -1.0)).collect());
    }
    let mut id = 0u64;
    for c in &centers {
        for _ in 0..per {
            let v: Vec<f32> = c
                .iter()
                .map(|x| (rng.unit() - 0.5).mul_add(0.05, *x))
                .collect();
            idx.insert(id, &v);
            id += 1;
        }
    }
    let total = clusters * per;
    println!("indexed {total} vectors from {clusters} clusters.");

    // Query near one cluster's center: candidates should be mostly that cluster.
    let target = 3u64; // cluster 3
    let cand = idx.candidates(&centers[target as usize]);
    let from_target = cand.iter().filter(|&&c| c / per as u64 == target).count();
    println!("\nquery near cluster {target}'s center:");
    println!(
        "  candidates returned: {} of {total} ({:.1}% of the data scanned)",
        cand.len(),
        cand.len() as f64 / total as f64 * 100.0
    );
    println!(
        "  of those, from cluster {target}: {from_target}/{}",
        cand.len()
    );

    println!("\n==================================================");
    let purity = if cand.is_empty() {
        0.0
    } else {
        from_target as f64 / cand.len() as f64
    };
    if !cand.is_empty() && cand.len() < total / 2 && purity > 0.8 {
        println!(
            "VERDICT: the query scanned {} candidates instead of {total}, and {:.0}% of",
            cand.len(),
            purity * 100.0
        );
        println!("them belong to the right cluster. the hyperplane code turned a full");
        println!("similarity scan into a single-bucket lookup, a prefilter for exact ranking.");
    } else {
        println!(
            "VERDICT: unexpected (cands={}, purity={purity:.2}).",
            cand.len()
        );
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
