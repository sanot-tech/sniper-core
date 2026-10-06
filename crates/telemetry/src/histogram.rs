//! Fixed-bucket latency histogram.
//!
//! Why not `hdrhistogram`? Two reasons tied to the archetype:
//!
//! * it needs a calibration pass and an allocation, both unacceptable in a
//!   crate that promises "no dependencies";
//! * a sniper's latency distribution is extremely narrow and known in advance
//!   (sub-microsecond decode, single-digit-millisecond land). A hand-written
//!   bucket table over exactly that range is both faster and more readable.

use std::sync::atomic::{AtomicU64, Ordering};

/// Upper bounds in nanoseconds. The last entry is the overflow bucket.
const BUCKETS_NS: [u64; 20] = [
    100,           // 100 ns
    250,           //
    500,           //
    1_000,         // 1 µs
    2_000,         //
    5_000,         //
    10_000,        // 10 µs
    20_000,        //
    50_000,        //
    100_000,       // 100 µs
    250_000,       //
    500_000,       //
    1_000_000,     // 1 ms
    2_000_000,     //
    5_000_000,     //
    10_000_000,    // 10 ms
    50_000_000,    //
    100_000_000,   // 100 ms
    500_000_000,   //
    1_000_000_000, // 1 s — anything above lands in the overflow bucket
];

/// Number of buckets = edges + 1 overflow slot.
pub const BUCKET_COUNT: usize = BUCKETS_NS.len() + 1;

/// Lock-free latency histogram.
#[derive(Debug)]
pub struct Histogram {
    counts: [AtomicU64; BUCKET_COUNT],
    sum_ns: AtomicU64,
    max_ns: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    #[must_use]
    pub fn new() -> Self {
        Self {
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
        }
    }

    /// Records one observation. Branch-predictable for the common small case,
    /// and free of floating point so it can never be a source of jitter.
    #[inline]
    pub fn record(&self, nanos: u64) {
        let mut idx = 0;
        // Hot path exits within 1-2 iterations because sub-microsecond
        // values dominate (decode, build, sign).
        while idx < BUCKETS_NS.len() && nanos > BUCKETS_NS[idx] {
            idx += 1;
        }
        self.counts[idx].fetch_add(1, Ordering::Relaxed);
        self.sum_ns.fetch_add(nanos, Ordering::Relaxed);
        // Monotonic max without a CAS loop: a lost race here costs one
        // slightly smaller reading, never a panic or a stall.
        self.max_ns.fetch_max(nanos, Ordering::Relaxed);
    }

    #[inline]
    #[must_use]
    pub fn count(&self) -> u64 {
        self.counts.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }

    #[inline]
    #[must_use]
    pub fn sum_ns(&self) -> u64 {
        self.sum_ns.load(Ordering::Relaxed)
    }

    #[inline]
    #[must_use]
    pub fn max_ns(&self) -> u64 {
        self.max_ns.load(Ordering::Relaxed)
    }

    #[inline]
    #[must_use]
    pub fn mean_ns(&self) -> u64 {
        self.sum_ns().checked_div(self.count()).unwrap_or(0)
    }

    /// Approximate percentile from bucket upper bounds.
    ///
    /// Returns the upper bound of the bucket containing the requested
    /// quantile, which is a deliberate over-estimate: for a latency budget you
    /// want the pessimistic edge of the interval, not a flattering midpoint.
    #[must_use]
    pub fn percentile_ns(&self, q: f64) -> u64 {
        let total = self.count();
        if total == 0 {
            return 0;
        }
        // q arrives as a fraction for ergonomics, but the ranking only needs a
        // count. Converting once, with clamping, keeps a NaN or an
        // out-of-range quantile from turning into a wild index.
        let target = quantile_to_rank(total, q);
        let mut seen = 0_u64;
        for (i, c) in self.counts.iter().enumerate() {
            seen += c.load(Ordering::Relaxed);
            if seen >= target {
                return bucket_upper_ns(i);
            }
        }
        bucket_upper_ns(BUCKET_COUNT - 1)
    }

    /// Raw bucket counts, for export to a dashboard.
    #[must_use]
    pub fn snapshot(&self) -> Vec<u64> {
        self.counts
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect()
    }

    pub fn reset(&self) {
        for c in &self.counts {
            c.store(0, Ordering::Relaxed);
        }
        self.sum_ns.store(0, Ordering::Relaxed);
        self.max_ns.store(0, Ordering::Relaxed);
    }
}

#[inline]
#[must_use]
fn bucket_upper_ns(idx: usize) -> u64 {
    BUCKETS_NS.get(idx).copied().unwrap_or(u64::MAX)
}

/// Converts a quantile in [0, 1] into a 1-based rank within `total` samples.
///
/// Two deliberate decisions:
///
/// * **NaN and out-of-range quantiles collapse to the extremes** rather than
///   producing a nonsensical index. A dashboard passing `NaN` should show the
///   median-ish bucket, not panic a trading process.
/// * **`u64 -> f64` loses precision above 2^53 samples.** Irrelevant here: the
///   result is only ever compared against a bucket boundary, and the error is
///   far below one bucket width. Using integer math instead would cost a public
///   API change to avoid an inaccuracy that cannot be observed.
fn quantile_to_rank(total: u64, q: f64) -> u64 {
    const MILLION: f64 = 1_000_000.0;
    let clamped = if q.is_nan() { 0.5 } else { q.clamp(0.0, 1.0) };
    // Parts-per-million keeps the rest of the arithmetic in integers.
    //
    // The cast is safe by construction rather than by luck: `clamped` is
    // bounded to [0, 1] and multiplied by 1e6, so the value is in [0, 1e6].
    // std has no `TryFrom<f64> for u64`, so the range has to be established
    // here rather than checked by the compiler.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ppm = (clamped * MILLION).round() as u64;
    let rank = total.saturating_mul(ppm) / 1_000_000;
    // A rank of 0 would match the very first observation and quietly turn
    // "median" into "minimum"; clamp it to 1 instead.
    rank.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_histogram_is_safe() {
        let h = Histogram::new();
        assert_eq!(h.count(), 0);
        assert_eq!(h.mean_ns(), 0);
        assert_eq!(h.percentile_ns(0.99), 0);
        assert_eq!(h.max_ns(), 0);
    }

    #[test]
    fn records_land_in_expected_buckets() {
        let h = Histogram::new();
        h.record(50); // -> bucket 0 (<=100)
        h.record(90); // -> bucket 0
        h.record(120); // -> bucket 1 (<=250)
        h.record(2_000); // -> bucket 4 (<=2000)
        assert_eq!(h.count(), 4);
        assert_eq!(h.sum_ns(), 50 + 90 + 120 + 2_000);
        assert_eq!(h.mean_ns(), (50 + 90 + 120 + 2_000) / 4);
        assert_eq!(h.max_ns(), 2_000);
    }

    #[test]
    fn overflow_bucket_absorbs_huge_values() {
        let h = Histogram::new();
        h.record(60_000_000_000);
        assert_eq!(h.count(), 1);
        assert_eq!(h.snapshot().last().copied(), Some(1));
    }

    #[test]
    fn percentiles_are_monotonic_and_pessimistic() {
        let h = Histogram::new();
        for _ in 0..90 {
            h.record(50); // fast: bucket 0
        }
        for _ in 0..10 {
            h.record(6_000_000); // slow: bucket 15 (<=10ms)
        }
        let p50 = h.percentile_ns(0.50);
        let p99 = h.percentile_ns(0.99);
        assert!(p50 <= 100, "p50 should be in the <=100ns bucket, got {p50}");
        assert!(
            p99 >= 5_000_000,
            "p99 should reach the ms bucket, got {p99}"
        );
        assert!(p50 <= p99);
    }

    #[test]
    fn hostile_quantiles_do_not_panic_or_invert() {
        let h = Histogram::new();
        for _ in 0..100 {
            h.record(50);
        }
        // NaN, negative and above-one inputs must all stay inside the range the
        // histogram can answer rather than panicking or wrapping around.
        let lo = h.percentile_ns(f64::NAN);
        let mid = h.percentile_ns(0.5);
        let hi = h.percentile_ns(5.0);
        let neg = h.percentile_ns(-1.0);
        assert!(
            lo <= hi,
            "NaN must not exceed an extreme quantile: {lo} vs {hi}"
        );
        assert_eq!(lo, neg, "NaN is treated as the median, like a negative q");
        assert!(mid >= lo && mid <= hi);
        assert!(h.percentile_ns(0.0) <= 100);
    }

    #[test]
    fn single_sample_is_reported_for_every_quantile() {
        let h = Histogram::new();
        h.record(1_200);
        // Guards the rank clamp: with one sample every quantile must land on
        // that sample, including q = 0 which would otherwise rank zero and
        // silently mean "minimum".
        assert!(h.percentile_ns(0.0) >= 1_200);
        assert!(h.percentile_ns(0.5) >= 1_200);
        assert!(h.percentile_ns(1.0) >= 1_200);
    }

    #[test]
    fn reset_clears_everything() {
        let h = Histogram::new();
        h.record(1_234);
        h.reset();
        assert_eq!(h.count(), 0);
        assert_eq!(h.max_ns(), 0);
    }
}
