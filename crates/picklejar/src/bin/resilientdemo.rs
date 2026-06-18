//! A corruption drill for the self-healing erasure-coded store, and the mass
//! argument it exists to make.
//!
//! Stores a batch of AI-memory blobs, then irradiates them: flips random shards,
//! up to the parity limit per blob, the way single-event upsets would. It then
//! reads everything back and shows that every blob came back byte-for-byte
//! correct, every fault was logged, and every fault was repaired from redundancy,
//! with no human and no spare node. The header prints the mass overhead of this
//! protection against the hardware alternative (extra redundant copies).
//!
//! ```text
//! cargo run --release --bin resilientdemo                # k=10, m=4, 200 blobs
//! cargo run --release --bin resilientdemo -- 16 4 500    # k, m, blob count
//! ```

use std::process::ExitCode;

use picklejar_storage::resilient::ResilientStore;

/// A small deterministic PRNG so a drill replays exactly.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next_u64() % n as u64).expect("fits")
    }
}

/// A deterministic embedding blob for `key`: a length-prefixed run of bytes that
/// is easy to regenerate and compare.
fn blob(key: u64) -> Vec<u8> {
    let len = 256 + usize::try_from(key % 7).expect("0..7") * 64;
    (0..len)
        .map(|i| u8::try_from((i as u64 ^ key) & 0xFF).expect("masked"))
        .collect()
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let k: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    let m: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4);
    let blobs: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(200);

    let Ok(mut store) = ResilientStore::new(k, m) else {
        eprintln!("invalid shape: k={k}, m={m} (need k>=1, k+m<=256)");
        return ExitCode::FAILURE;
    };

    // The mass argument. To survive m simultaneous failures, the hardware way is
    // m extra full copies (replication / N-way mirroring): +m*100% storage. The
    // software way is m parity shards over k data shards: +m/k.
    #[allow(clippy::cast_precision_loss)]
    let (kf, mf) = (k as f64, m as f64);
    let erasure_overhead = mf / kf * 100.0;
    let replication_overhead = mf * 100.0;
    let saved = (1.0 - (1.0 + mf / kf) / (1.0 + mf)) * 100.0;
    println!("self-healing erasure-coded store: k={k} data + m={m} parity shards");
    println!("survives any {m} simultaneous shard failures");
    println!(
        "  storage overhead: +{erasure_overhead:.0}% (erasure) vs +{replication_overhead:.0}% \
         ({m} redundant copies) for the same fault tolerance"
    );
    println!("  that is {saved:.0}% less stored (and launched) mass than redundant copies\n");

    // Store the blobs.
    for key in 0..blobs {
        if let Err(e) = store.put(key, &blob(key)) {
            eprintln!("put {key} failed: {e}");
            return ExitCode::FAILURE;
        }
    }

    // Irradiate: flip up to m random shards in each blob.
    let mut rng = Rng(0x5EED_1A71);
    let mut injected = 0usize;
    for key in 0..blobs {
        let bad = rng.below(m + 1);
        let mut hit = std::collections::HashSet::new();
        while hit.len() < bad {
            hit.insert(rng.below(k + m));
        }
        for &shard in &hit {
            store.corrupt_shard(key, shard, b"single-event upset");
            injected += 1;
        }
    }

    // Read everything back and verify it is exactly what was stored.
    let mut wrong = 0usize;
    for key in 0..blobs {
        match store.get(key) {
            Ok(got) if got == blob(key) => {}
            Ok(_) => wrong += 1,
            Err(e) => {
                eprintln!("blob {key} was lost: {e}");
                wrong += 1;
            }
        }
    }

    let log = store.fault_log();
    let repaired = log.iter().filter(|e| e.repaired).count();
    println!("{blobs} blobs stored, {injected} upsets injected across their shards");
    println!(
        "  {} faults detected, {repaired} repaired from redundancy, {wrong} blobs served wrong",
        log.len()
    );

    if wrong == 0 {
        println!("result: every blob healed and returned correct; nothing was lost");
        ExitCode::SUCCESS
    } else {
        eprintln!("result: {wrong} blobs were served wrong or lost");
        ExitCode::FAILURE
    }
}
