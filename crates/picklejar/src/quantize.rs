//! Drift-adaptive scalar quantization for the vector memory layer.
//!
//! A quantized index trades recall for memory: each `f32` component is stored as
//! one `u8` (a 4x smaller index) under a per-dimension affine map calibrated to
//! the data's range. The catch every production vector index hits is *drift*: the
//! embedding distribution shifts over time, so a quantizer calibrated once on the
//! early data clips and loses resolution on the new data, and recall decays. The
//! usual fix is a full reindex.
//!
//! This module holds recall flat under drift instead, at a fixed memory budget,
//! by watching the live distribution and recalibrating only when it has outgrown
//! the calibrated range. The codes stay one byte per dimension throughout; the
//! full-precision rows the engine already stores are the source of truth a
//! recalibration re-quantizes from, so the *index* never grows. Holding recall
//! flat under drift at a fixed memory budget, rather than reindexing, is the one
//! place this engine makes a benchmarked contribution rather than re-implementing
//! solved art; see `bin/quantsim` and the certificate.

use crate::hnsw::Metric;

/// A per-dimension affine scalar quantizer.
///
/// Each dimension has a calibrated `[min, max]` range. A component is mapped onto
/// that range, scaled to a byte (`0..=255`), and dequantized back by the inverse
/// affine map. A degenerate dimension (`max == min`) stores zero and dequantizes
/// to `min`.
#[derive(Clone, Debug, PartialEq)]
pub struct ScalarQuantizer {
    /// Per-dimension lower bound of the calibrated range.
    min: Vec<f32>,
    /// Per-dimension upper bound of the calibrated range.
    max: Vec<f32>,
}

impl ScalarQuantizer {
    /// Calibrate to the per-dimension range of `sample`. An empty sample, or a
    /// sample of a single point, yields a degenerate (all-`min`) quantizer for the
    /// dimensions with no spread, which is exact for constants and clamps the rest.
    ///
    /// # Panics
    ///
    /// Panics if `sample` is empty (there is no dimensionality to calibrate to);
    /// callers calibrate on a non-empty initial sample.
    #[must_use]
    pub fn calibrate(sample: &[Vec<f32>]) -> Self {
        assert!(!sample.is_empty(), "calibrate needs a non-empty sample");
        let dims = sample[0].len();
        let mut min = vec![f32::INFINITY; dims];
        let mut max = vec![f32::NEG_INFINITY; dims];
        for v in sample {
            for ((mn, mx), &x) in min.iter_mut().zip(max.iter_mut()).zip(v) {
                *mn = mn.min(x);
                *mx = mx.max(x);
            }
        }
        Self { min, max }
    }

    /// The dimensionality this quantizer was calibrated for.
    #[must_use]
    pub fn dims(&self) -> usize {
        self.min.len()
    }

    /// Quantize `v` to one byte per dimension under the calibrated range.
    #[must_use]
    // The cast is bounded: `t.clamp(0, 1) * 255` rounded is in `0..=255`, so it
    // neither truncates meaningfully nor loses a sign.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn quantize(&self, v: &[f32]) -> Vec<u8> {
        self.min
            .iter()
            .zip(&self.max)
            .zip(v)
            .map(|((&mn, &mx), &x)| {
                let span = mx - mn;
                if span <= 0.0 {
                    return 0;
                }
                // Clamp to [0, 1] so out-of-range components saturate rather than
                // wrap, then map to the byte range.
                (((x - mn) / span).clamp(0.0, 1.0) * 255.0).round() as u8
            })
            .collect()
    }

    /// Dequantize a code back to an approximate vector under the calibrated range.
    #[must_use]
    pub fn dequantize(&self, code: &[u8]) -> Vec<f32> {
        self.min
            .iter()
            .zip(&self.max)
            .zip(code)
            .map(|((&mn, &mx), &c)| (f32::from(c) / 255.0).mul_add(mx - mn, mn))
            .collect()
    }
}

/// A drift-adaptive quantized index: it stores one-byte-per-dimension codes,
/// tracks the live distribution, and recalibrates only when that distribution has
/// outgrown the calibrated range past a threshold.
#[derive(Clone, Debug)]
pub struct AdaptiveIndex {
    quantizer: ScalarQuantizer,
    /// One quantized code per inserted vector, in insertion order.
    codes: Vec<Vec<u8>>,
    /// Running per-dimension minimum observed across every inserted vector.
    obs_min: Vec<f32>,
    /// Running per-dimension maximum observed across every inserted vector.
    obs_max: Vec<f32>,
    /// The drift fraction at which `needs_recalibration` trips.
    threshold: f32,
    /// How many times the index has recalibrated.
    recalibrations: usize,
}

impl AdaptiveIndex {
    /// Calibrate on `sample` and start an empty index that recalibrates when drift
    /// exceeds `threshold` (the observed range overflowing the calibrated range by
    /// that fraction). The sample is calibration only; it is not inserted.
    ///
    /// # Panics
    ///
    /// Panics if `sample` is empty.
    #[must_use]
    pub fn new(sample: &[Vec<f32>], threshold: f32) -> Self {
        let quantizer = ScalarQuantizer::calibrate(sample);
        let dims = quantizer.dims();
        Self {
            quantizer,
            codes: Vec::new(),
            obs_min: vec![f32::INFINITY; dims],
            obs_max: vec![f32::NEG_INFINITY; dims],
            threshold,
            recalibrations: 0,
        }
    }

    /// Insert `v`: store its code and fold it into the observed distribution.
    pub fn insert(&mut self, v: &[f32]) {
        for ((omn, omx), &x) in self.obs_min.iter_mut().zip(self.obs_max.iter_mut()).zip(v) {
            *omn = omn.min(x);
            *omx = omx.max(x);
        }
        self.codes.push(self.quantizer.quantize(v));
    }

    /// The current drift fraction: the largest per-dimension overflow of the
    /// observed range beyond the calibrated range, relative to the calibrated
    /// span. Zero when every inserted component has stayed inside the calibrated
    /// range; it grows as new data clips harder.
    #[must_use]
    pub fn drift(&self) -> f32 {
        let mut worst = 0.0f32;
        for (((&mn, &mx), &omn), &omx) in self
            .quantizer
            .min
            .iter()
            .zip(&self.quantizer.max)
            .zip(&self.obs_min)
            .zip(&self.obs_max)
        {
            let span = mx - mn;
            if span <= 0.0 {
                continue;
            }
            let below = (mn - omn).max(0.0);
            let above = (omx - mx).max(0.0);
            worst = worst.max((below + above) / span);
        }
        worst
    }

    /// Whether drift has crossed the threshold and a recalibration is due.
    #[must_use]
    pub fn needs_recalibration(&self) -> bool {
        self.drift() > self.threshold
    }

    /// Recalibrate to the observed range and re-quantize every code from `source`,
    /// the full-precision rows (one per inserted vector, in insertion order) the
    /// engine already stores. The index footprint is unchanged: codes stay one
    /// byte per dimension. After this, drift is zero.
    ///
    /// # Panics
    ///
    /// Panics if `source` does not have exactly one row per stored code.
    pub fn recalibrate(&mut self, source: &[Vec<f32>]) {
        assert_eq!(
            source.len(),
            self.codes.len(),
            "recalibrate needs one source row per stored code"
        );
        // The new calibrated range is the full observed range, so nothing inserted
        // so far clips. Fall back to the old bound for any dimension never seen.
        for ((mn, mx), (&omn, &omx)) in self
            .quantizer
            .min
            .iter_mut()
            .zip(self.quantizer.max.iter_mut())
            .zip(self.obs_min.iter().zip(&self.obs_max))
        {
            if omn.is_finite() {
                *mn = omn;
            }
            if omx.is_finite() {
                *mx = omx;
            }
        }
        for (code, row) in self.codes.iter_mut().zip(source) {
            *code = self.quantizer.quantize(row);
        }
        self.recalibrations += 1;
    }

    /// The `k` nearest stored vectors to `query` under `metric`, by index, ranking
    /// each stored code by its dequantized approximation (asymmetric distance: the
    /// query stays full precision). Fewer than `k` are returned only if the index
    /// holds fewer than `k` vectors.
    #[must_use]
    pub fn search(&self, query: &[f32], k: usize, metric: Metric) -> Vec<usize> {
        let mut scored: Vec<(usize, f32)> = self
            .codes
            .iter()
            .enumerate()
            .map(|(i, code)| {
                (
                    i,
                    crate::hnsw::rank(metric, query, &self.quantizer.dequantize(code)),
                )
            })
            .collect();
        // Partial-sort would do, but the index is small in the benchmarked regime;
        // a full sort keeps the ranking obviously correct.
        scored.sort_by(|a, b| a.1.total_cmp(&b.1));
        scored.into_iter().take(k).map(|(i, _)| i).collect()
    }

    /// Number of vectors stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.codes.len()
    }

    /// Whether the index is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    /// How many times the index has recalibrated.
    #[must_use]
    pub const fn recalibrations(&self) -> usize {
        self.recalibrations
    }

    /// The in-memory footprint of the codes, in bytes: one byte per dimension per
    /// vector. This is what the quantizer buys against the `4 * dims` bytes a
    /// full-precision index would hold per vector.
    #[must_use]
    pub fn code_bytes(&self) -> usize {
        self.codes.iter().map(Vec::len).sum()
    }
}

/// `SplitMix64`: the same small deterministic PRNG the rest of the simulators use,
/// so a benchmark replays exactly from its seed.
struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform `f32` in `[0, 1)`.
    // The top 24 bits fit `f32`'s mantissa exactly, so the division is precise.
    #[allow(clippy::cast_precision_loss)]
    fn unit(&mut self) -> f32 {
        // 2^24, the number of distinct values the 24 retained bits can take.
        (self.next_u64() >> 40) as f32 / 16_777_216.0
    }
}

/// The result of a drift benchmark.
///
/// The headline is `adaptive_recall` holding high while `static_recall` collapses,
/// at the same `compression` and with far fewer `recalibrations` than reindexing
/// on every insert.
#[derive(Clone, Copy, Debug)]
pub struct DriftBenchmark {
    /// Mean recall@k of the drift-adaptive index against the exact oracle.
    pub adaptive_recall: f32,
    /// Mean recall@k of the static (calibrate-once) index against the oracle.
    pub static_recall: f32,
    /// Times the adaptive index recalibrated over the whole stream.
    pub recalibrations: usize,
    /// Vectors streamed into each index.
    pub vectors: usize,
    /// Embedding dimensionality.
    pub dims: usize,
    /// Index memory saved: full-precision bytes per vector over code bytes (4x for
    /// `f32` to `u8`).
    pub compression: f32,
}

/// Exact `k` nearest by index over full-precision `vectors`, the oracle a quantized
/// index is scored against.
fn exact_topk(vectors: &[Vec<f32>], query: &[f32], k: usize, metric: Metric) -> Vec<usize> {
    let mut scored: Vec<(usize, f32)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| (i, crate::hnsw::rank(metric, query, v)))
        .collect();
    scored.sort_by(|a, b| a.1.total_cmp(&b.1));
    scored.into_iter().take(k).map(|(i, _)| i).collect()
}

/// Fraction of `exact`'s ids the `got` set also found.
// The counts are small (k and the query budget), so the `f32` ratio is exact.
#[allow(clippy::cast_precision_loss)]
fn recall(got: &[usize], exact: &[usize]) -> f32 {
    if exact.is_empty() {
        return 1.0;
    }
    let hits = got.iter().filter(|i| exact.contains(i)).count();
    hits as f32 / exact.len() as f32
}

/// Run one deterministic drift benchmark from `seed`.
///
/// A stream of embeddings whose per-dimension magnitude grows over time (scale
/// drift, the covariate shift a long-lived memory store sees) is fed into two
/// quantized indexes calibrated on the same early sample: one static, one
/// drift-adaptive. Queries are drawn from the current (drifted) distribution and
/// scored against the exact full-precision oracle. The adaptive index recalibrates
/// when drift trips its threshold and holds recall high; the static index clips the
/// grown vectors to the same saturated codes and collapses.
#[must_use]
// The counts (stream length, query budget) are small, so the `f32` arithmetic over
// them is exact; the casts are not a precision risk here.
#[allow(clippy::cast_precision_loss)]
pub fn run_drift_benchmark(seed: u64) -> DriftBenchmark {
    const DIMS: usize = 12;
    const K: usize = 10;
    const CALIB: usize = 128;
    const STREAM: usize = 1500;
    const QUERIES: usize = 64;
    const METRIC: Metric = Metric::L2;

    let mut rng = Rng::new(seed);

    // The magnitude a vector's components span at stream position `t`: it grows
    // from 1 to 30 across the stream, so late embeddings sit far outside the range
    // the early sample calibrated.
    let scale = |t: usize| -> f32 { (t as f32 / STREAM as f32).mul_add(29.0, 1.0) };
    let sample_vec =
        |rng: &mut Rng, s: f32| -> Vec<f32> { (0..DIMS).map(|_| rng.unit() * s).collect() };

    // Calibrate both indexes on an early sample (scale ~1).
    let calib: Vec<Vec<f32>> = (0..CALIB).map(|_| sample_vec(&mut rng, 1.0)).collect();
    let mut adaptive = AdaptiveIndex::new(&calib, 0.25);
    let mut fixed = AdaptiveIndex::new(&calib, f32::INFINITY); // never recalibrates

    // Stream the drifting embeddings into both, recalibrating the adaptive one
    // from the full-precision source when its drift trips.
    let mut source: Vec<Vec<f32>> = Vec::with_capacity(STREAM);
    for t in 0..STREAM {
        let v = sample_vec(&mut rng, scale(t));
        source.push(v.clone());
        adaptive.insert(&v);
        fixed.insert(&v);
        if adaptive.needs_recalibration() {
            adaptive.recalibrate(&source);
        }
    }

    // Score both against the exact oracle on queries from the current region.
    let mut adaptive_sum = 0.0f32;
    let mut static_sum = 0.0f32;
    for _ in 0..QUERIES {
        let q = sample_vec(&mut rng, scale(STREAM - 1));
        let oracle = exact_topk(&source, &q, K, METRIC);
        adaptive_sum += recall(&adaptive.search(&q, K, METRIC), &oracle);
        static_sum += recall(&fixed.search(&q, K, METRIC), &oracle);
    }

    DriftBenchmark {
        adaptive_recall: adaptive_sum / QUERIES as f32,
        static_recall: static_sum / QUERIES as f32,
        recalibrations: adaptive.recalibrations(),
        vectors: STREAM,
        dims: DIMS,
        compression: 4.0,
    }
}

#[cfg(test)]
mod tests {
    use super::{run_drift_benchmark, AdaptiveIndex, ScalarQuantizer};
    use crate::hnsw::Metric;

    #[test]
    fn quantize_round_trips_within_resolution() {
        let q = ScalarQuantizer::calibrate(&[vec![0.0, -1.0], vec![10.0, 1.0]]);
        // A value mid-range round-trips to within one quantization step.
        let v = vec![5.0, 0.0];
        let approx = q.dequantize(&q.quantize(&v));
        assert!((approx[0] - 5.0).abs() < 10.0 / 255.0 + 1e-4);
        assert!((approx[1] - 0.0).abs() < 2.0 / 255.0 + 1e-4);
    }

    #[test]
    fn out_of_range_components_saturate_not_wrap() {
        let q = ScalarQuantizer::calibrate(&[vec![0.0], vec![1.0]]);
        // Above the calibrated max clamps to the top code, below to the bottom.
        assert_eq!(q.quantize(&[5.0]), vec![255]);
        assert_eq!(q.quantize(&[-5.0]), vec![0]);
    }

    #[test]
    fn a_constant_dimension_is_exact() {
        let q = ScalarQuantizer::calibrate(&[vec![7.0], vec![7.0]]);
        assert_eq!(q.dequantize(&q.quantize(&[7.0])), vec![7.0]);
    }

    #[test]
    fn drift_is_zero_in_range_and_grows_out_of_range() {
        let mut idx = AdaptiveIndex::new(&[vec![0.0], vec![1.0]], 0.5);
        idx.insert(&[0.5]);
        assert!(idx.drift() < 1e-6, "in-range insert does not drift");
        // A value 0.5 above the calibrated max (span 1.0) is a drift of 0.5.
        idx.insert(&[1.5]);
        assert!((idx.drift() - 0.5).abs() < 1e-6, "drift={}", idx.drift());
        assert!(
            !idx.needs_recalibration(),
            "0.5 is not past the 0.5 threshold"
        );
        idx.insert(&[2.0]);
        assert!(idx.needs_recalibration(), "1.0 drift is past the threshold");
    }

    #[test]
    fn recalibration_clears_drift_and_keeps_the_footprint() {
        let source = vec![vec![0.5], vec![1.5], vec![2.0]];
        let mut idx = AdaptiveIndex::new(&[vec![0.0], vec![1.0]], 0.5);
        for v in &source {
            idx.insert(v);
        }
        let before = idx.code_bytes();
        assert!(idx.needs_recalibration());
        idx.recalibrate(&source);
        assert!(
            idx.drift() < 1e-6,
            "recalibration covers the observed range"
        );
        assert_eq!(idx.recalibrations(), 1);
        assert_eq!(
            idx.code_bytes(),
            before,
            "codes stay one byte per dimension"
        );
    }

    #[test]
    fn adaptive_holds_recall_under_drift_where_static_collapses() {
        // The headline: across seeds, the drift-adaptive index stays near the
        // full-precision ceiling while the static one collapses, at the same 4x
        // compression and by recalibrating rarely (not reindexing every insert).
        for seed in [1u64, 42, 0xBEEF, 0xD817, 7] {
            let b = run_drift_benchmark(seed);
            assert!(
                b.adaptive_recall > 0.85,
                "adaptive recall {:.3} too low (seed {seed})",
                b.adaptive_recall
            );
            assert!(
                b.static_recall < 0.20,
                "static recall {:.3} unexpectedly high (seed {seed})",
                b.static_recall
            );
            assert!(
                b.adaptive_recall - b.static_recall > 0.5,
                "adaptive must clearly beat static (seed {seed})"
            );
            assert!(
                (1..b.vectors / 10).contains(&b.recalibrations),
                "expected rare-but-present recalibration, got {} (seed {seed})",
                b.recalibrations
            );
            assert!((b.compression - 4.0).abs() < 1e-6);
        }
    }

    #[test]
    fn search_finds_the_nearest_after_recalibration() {
        // Three well-separated points; after recalibration the quantized search
        // still ranks the true nearest first.
        let source = vec![vec![0.0, 0.0], vec![5.0, 5.0], vec![10.0, 10.0]];
        let mut idx = AdaptiveIndex::new(&[vec![0.0, 0.0], vec![1.0, 1.0]], 0.25);
        for v in &source {
            idx.insert(v);
        }
        idx.recalibrate(&source);
        let near = idx.search(&[9.5, 9.5], 1, Metric::L2);
        assert_eq!(
            near,
            vec![2],
            "the point at (10,10) is nearest to (9.5,9.5)"
        );
    }
}
