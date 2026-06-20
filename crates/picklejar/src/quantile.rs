//! Streaming quantile sketch: latency percentiles in fixed space.
//!
//! To know whether a memory node is healthy you want its access-latency
//! percentiles (the p50, p95, p99), not just an average that a few slow outliers
//! quietly poison. Keeping every sample to sort is impossible over a long run. A
//! log-bucketed histogram, the idea behind HdrHistogram, records each value into
//! a bucket whose width grows with the value, so it covers nanoseconds to seconds
//! in about a thousand buckets at a fixed relative precision. A percentile is then
//! a single pass accumulating counts to the target rank. The error is bounded by a
//! bucket's relative width, a few percent, regardless of how many samples arrive.

#![allow(clippy::doc_markdown)] // "HdrHistogram" reads as prose

/// Sub-buckets per power-of-two octave: 16 gives roughly 6% relative precision.
const SUB_BITS: u32 = 4;
const SUB: u64 = 1 << SUB_BITS;

/// The bucket index for a value (contiguous and monotonic in `v`).
#[allow(clippy::cast_possible_truncation)] // small intermediate values fit usize
const fn bucket_index(v: u64) -> usize {
    if v < SUB {
        return v as usize;
    }
    let octave = v.ilog2();
    let shift = octave - SUB_BITS;
    let mantissa = (v >> shift) - SUB;
    (octave - SUB_BITS + 1) as usize * SUB as usize + mantissa as usize
}

/// The lower bound of the value range a bucket covers.
#[allow(clippy::cast_possible_truncation)] // shift fits u32 by construction
const fn bucket_value(index: usize) -> u64 {
    let idx = index as u64;
    if idx < SUB {
        return idx;
    }
    let octave_offset = idx / SUB;
    let mantissa = idx % SUB;
    let shift = (octave_offset - 1) as u32;
    (SUB + mantissa) << shift
}

/// A streaming quantile sketch over `u64` values (for example, microseconds).
#[derive(Clone, Debug, Default)]
pub struct QuantileSketch {
    counts: Vec<u64>,
    total: u64,
}

impl QuantileSketch {
    /// An empty sketch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one observed value.
    pub fn record(&mut self, value: u64) {
        let b = bucket_index(value);
        if b >= self.counts.len() {
            self.counts.resize(b + 1, 0);
        }
        self.counts[b] += 1;
        self.total += 1;
    }

    /// The number of recorded values.
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.total
    }

    /// Estimate the value at quantile `q` in `[0, 1]`.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )]
    pub fn quantile(&self, q: f64) -> u64 {
        if self.total == 0 {
            return 0;
        }
        let target = (q.clamp(0.0, 1.0) * self.total as f64).ceil() as u64;
        let target = target.max(1);
        let mut cumulative = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            cumulative += c;
            if cumulative >= target {
                return bucket_value(i);
            }
        }
        bucket_value(self.counts.len().saturating_sub(1))
    }

    /// The number of buckets in use (the memory footprint).
    #[must_use]
    pub fn bucket_count(&self) -> usize {
        self.counts.len()
    }

    /// Merge another sketch (sum the histograms). Lets nodes combine percentiles.
    pub fn merge(&mut self, other: &Self) {
        if other.counts.len() > self.counts.len() {
            self.counts.resize(other.counts.len(), 0);
        }
        for (a, &b) in self.counts.iter_mut().zip(&other.counts) {
            *a += b;
        }
        self.total += other.total;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )]
    fn within(est: u64, truth: u64, rel: f64) -> bool {
        let diff = (est as f64 - truth as f64).abs();
        diff <= rel * truth as f64
    }

    #[test]
    fn bucket_index_and_value_round_trip_monotonically() {
        let mut last = 0;
        for v in [0u64, 1, 15, 16, 17, 100, 1000, 1_000_000, 1_000_000_000] {
            let b = bucket_index(v);
            assert!(b >= last || v <= 16, "indices must be monotonic in v");
            last = b;
            // The bucket's lower bound never exceeds the value.
            assert!(
                bucket_value(b) <= v,
                "value {v} in bucket {b} with lb {}",
                bucket_value(b)
            );
        }
    }

    #[test]
    fn percentiles_track_a_uniform_stream() {
        let mut q = QuantileSketch::new();
        for v in 1..=100_000u64 {
            q.record(v);
        }
        // p50/p95/p99 within the histogram's relative precision.
        assert!(
            within(q.quantile(0.50), 50_000, 0.07),
            "p50 was {}",
            q.quantile(0.50)
        );
        assert!(
            within(q.quantile(0.95), 95_000, 0.07),
            "p95 was {}",
            q.quantile(0.95)
        );
        assert!(
            within(q.quantile(0.99), 99_000, 0.07),
            "p99 was {}",
            q.quantile(0.99)
        );
    }

    #[test]
    fn outliers_do_not_move_the_median() {
        let mut q = QuantileSketch::new();
        for _ in 0..100_000 {
            q.record(100); // a tight mode at 100
        }
        for _ in 0..1000 {
            q.record(10_000_000); // a slug of huge outliers (~1% of the stream)
        }
        // The mean would be dragged up; the median stays at the mode.
        assert!(
            within(q.quantile(0.5), 100, 0.07),
            "median was {}",
            q.quantile(0.5)
        );
        // But the tail reflects the outliers.
        assert!(
            q.quantile(0.999) > 1_000_000,
            "p99.9 should see the outliers"
        );
    }

    #[test]
    fn memory_is_bounded_regardless_of_count() {
        let mut q = QuantileSketch::new();
        for v in 0..1_000_000u64 {
            q.record(v % 1_000_000);
        }
        // A million samples over a wide range still fit in ~1000 buckets.
        assert!(
            q.bucket_count() < 2000,
            "bucket count {} should stay small",
            q.bucket_count()
        );
    }

    #[test]
    fn merge_combines_two_streams() {
        let mut a = QuantileSketch::new();
        let mut b = QuantileSketch::new();
        for v in 1..=50_000u64 {
            a.record(v);
        }
        for v in 50_001..=100_000u64 {
            b.record(v);
        }
        a.merge(&b);
        assert_eq!(a.count(), 100_000);
        assert!(
            within(a.quantile(0.5), 50_000, 0.07),
            "merged p50 was {}",
            a.quantile(0.5)
        );
    }
}
