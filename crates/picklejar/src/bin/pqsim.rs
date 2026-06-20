//! Product quantization: 16x-smaller embeddings that still rank correctly.
//!
//! ```text
//! cargo run --release --bin pqsim
//! ```

#![allow(clippy::cast_precision_loss)]

use std::process::ExitCode;

use picklejar::pq::ProductQuantizer;

struct Rng(u64);
impl Rng {
    fn unit(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 40) as f32 / 16_777_216.0
    }
}

fn l2(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

fn main() -> ExitCode {
    println!("\n=============== PRODUCT QUANTIZATION ===============");
    println!("16x-smaller embeddings that still surface the right memory\n");

    let dims = 32;
    let n = 2000;
    let mut rng = Rng(0xBEEF);
    let centers: Vec<Vec<f32>> = (0..16)
        .map(|_| (0..dims).map(|_| rng.unit()).collect())
        .collect();
    let data: Vec<Vec<f32>> = (0..n)
        .map(|i| {
            let c = &centers[i % centers.len()];
            c.iter()
                .map(|x| (rng.unit() - 0.5).mul_add(0.03, *x))
                .collect()
        })
        .collect();

    let pq = ProductQuantizer::train(&data, 8, 256, 99);
    let codes: Vec<Vec<u8>> = data.iter().map(|v| pq.encode(v)).collect();

    let orig_bytes = dims * 4;
    let code_bytes = pq.code_len();
    println!("trained on {n} vectors. each {dims}-dim embedding:");
    println!(
        "  raw: {orig_bytes} bytes   ->   PQ code: {code_bytes} bytes   ({}x smaller)",
        orig_bytes / code_bytes
    );

    // Recall@10: how often the exact nearest is in the PQ top-10.
    let mut hits = 0;
    let trials = 100;
    for (qi, q) in data.iter().enumerate().take(trials) {
        let exact = data
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != qi)
            .min_by(|a, b| l2(q, a.1).total_cmp(&l2(q, b.1)))
            .map_or(0, |(i, _)| i);
        let mut ranked: Vec<(usize, f32)> = codes
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != qi)
            .map(|(i, c)| (i, l2(q, &pq.decode(c))))
            .collect();
        ranked.sort_by(|a, b| a.1.total_cmp(&b.1));
        if ranked.iter().take(10).any(|(i, _)| *i == exact) {
            hits += 1;
        }
    }
    let recall = f64::from(hits) / trials as f64;
    println!(
        "  recall@10 (exact nearest found in PQ top-10): {:.0}%",
        recall * 100.0
    );

    println!("\n==================================================");
    if orig_bytes / code_bytes >= 16 && recall > 0.85 {
        println!(
            "VERDICT: each embedding shrank {}x, from {orig_bytes} to {code_bytes} bytes,",
            orig_bytes / code_bytes
        );
        println!("yet the exact nearest memory still lands in the top 10 ranked on the");
        println!(
            "compressed codes {:.0}% of the time. far more vectors fit in the same RAM.",
            recall * 100.0
        );
    } else {
        println!(
            "VERDICT: unexpected (ratio {}, recall {recall:.2}).",
            orig_bytes / code_bytes
        );
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
