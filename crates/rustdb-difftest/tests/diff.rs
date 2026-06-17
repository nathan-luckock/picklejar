//! Differential testing: rustdb must agree with SQLite across many seeds.
//!
//! Each seed generates random SQL in a dialect-shared subset and runs it
//! through both engines, comparing results as a sorted multiset. A failure
//! prints the seed and full SQL; `cargo run --bin difftest -- --seed <n>`
//! replays it. The `difftest` binary sweeps far more for deeper exploration.

use rustdb_difftest::{run_seed, Outcome};

#[test]
fn rustdb_agrees_with_sqlite() {
    let mut compared = 0u32;
    for seed in 0..300u64 {
        match run_seed(seed) {
            Ok(Outcome::Match { .. }) => compared += 1,
            Ok(Outcome::Skipped) => {}
            Err(e) => panic!("differential failure:\n{e}"),
        }
    }
    assert!(
        compared > 0,
        "every seed was skipped; the generator is broken"
    );
}
