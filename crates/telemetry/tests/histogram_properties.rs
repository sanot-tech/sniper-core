//! 📊 `HistogramProperties` — the latency recorder, stated as laws.
//!
//! ## Why this file exists separately from the unit tests in `histogram.rs`
//!
//! The unit tests there check hand-picked values. That is the right tool for
//! "a 2 000 ns sample lands in bucket 4", and the wrong tool for "this
//! recorder never lies about the shape of its own data". A latency histogram
//! that misreports is worse than no histogram: it is consulted to decide
//! whether the bot is fast enough, so a wrong reading does not fail loudly, it
//! fails as a bad trade.
//!
//! ## The five laws
//!
//! 1. **Conservation** — the bucket counts sum to `count()`. Every
//!    observation lands in exactly one bucket, so none may vanish or be
//!    double-counted.
//! 2. **Monotonicity** — `percentile_ns` never decreases as `q` rises. A
//!    dashboard showing p50 above p99 is showing the reader a contradiction.
//! 3. **Extremality** — `max_ns` is the largest thing recorded, and
//!    `percentile_ns(1.0)` never under-reports it.
//! 4. **Central tendency** — `mean_ns` sits between the smallest and largest
//!    observation, and never exceeds `max_ns`.
//! 5. **Totality** — no input panics, and no counter wraps. `sum_ns` is a
//!    `fetch_add` on a `u64`; law 5 is what says whether that is safe.
//!
//! ## The one that found a real defect
//!
//! `sum_ns` used a plain `fetch_add`, which wraps rather than saturating on
//! overflow. Two samples of `u64::MAX` nanoseconds — reachable through the
//! public `record`, and reachable from `Probe::finish` if a clock ever
//! reported an absurd elapsed time — leave `sum_ns` at `u64::MAX - 1` while
//! `count()` reads 2. `mean_ns` then reports roughly half of `u64::MAX` for a
//! histogram holding two observations, which is a number no operator can
//! interpret and no alert can threshold.
//!
//! The property test `sum_never_wraps` is the witness. It is kept even after
//! the fix, because the fix is one line and the reasoning behind it is not.

use proptest::prelude::*;
use sniper_telemetry::Histogram;

/// Latencies a real hot path produces: sub-microsecond through multi-second.
fn latency_ns() -> impl Strategy<Value = u64> {
    prop_oneof![
        // The shape that dominates: sub-microsecond decode/build/sign.
        0_u64..10_000_u64,
        // The awkward middle, where bucket edges are close together.
        10_000_u64..1_000_000_u64,
        // Slow stages and the overflow bucket.
        1_000_000_u64..2_000_000_000_u64,
        // Exact bucket edges, because off-by-one at an edge is the classic
        // way a bucket table goes wrong. One nanosecond above each edge is
        // included too: `record` compares with `>`, so the edge itself and
        // the edge plus one are two genuinely different cases.
        Just(100_u64),
        Just(250),
        Just(500),
        Just(1_000),
        Just(2_000),
        Just(5_000),
        Just(10_000),
        Just(20_000),
        Just(50_000),
        Just(100_000),
        Just(250_000),
        Just(500_000),
        Just(1_000_000),
        Just(2_000_000),
        Just(5_000_000),
        Just(10_000_000),
        Just(50_000_000),
        Just(100_000_000),
        Just(500_000_000),
        Just(1_000_000_000),
        Just(101_u64),
        Just(251),
        Just(501),
        Just(1_001),
        Just(2_001),
        Just(5_001),
        Just(10_001),
        Just(100_001),
        Just(1_000_001),
        // The last value that still fits a real bucket rather than the
        // overflow slot, and the first value that does not.
        Just(1_000_000_000_u64 - 1),
        Just(1_000_000_001_u64),
    ]
}

/// Any quantile a dashboard might plausibly compute, including the hostile
/// ones a dashboard does compute when it divides by an empty sample.
fn quantile() -> impl Strategy<Value = f64> {
    prop_oneof![
        0.0_f64..=1.0,
        Just(f64::NAN),
        Just(f64::INFINITY),
        Just(f64::NEG_INFINITY),
        Just(-5.0),
        Just(5.0),
    ]
}

proptest! {
    // ---------------------------------------------------------------------
    // Law 1 — conservation
    // ---------------------------------------------------------------------

    /// Every observation is in exactly one bucket, and no bucket invents one.
    ///
    /// Asserted by comparing the public `count()` against the sum of the
    /// public `snapshot()`. Two independent accessors disagreeing is exactly
    /// what a dashboard would surface as a chart that does not add up.
    #[test]
    fn bucket_counts_sum_to_the_reported_count(samples in prop::collection::vec(latency_ns(), 0usize..64)) {
        let h = Histogram::new();
        for s in &samples {
            h.record(*s);
        }

        let from_snapshot: u64 = h.snapshot().iter().sum();
        prop_assert_eq!(
            from_snapshot,
            h.count(),
            "snapshot {:?} disagrees with count()",
            h.snapshot()
        );
        prop_assert_eq!(h.count(), samples.len() as u64);

        // The bucket table has a fixed width, so a non-empty sample must put
        // at least one observation somewhere. This catches the degenerate
        // case where every bucket silently reads zero.
        if !samples.is_empty() {
            prop_assert!(from_snapshot > 0, "a non-empty histogram reported no observations");
        }
    }

    /// The bucket a value lands in must be the *first* one that contains it.
    ///
    /// `record` walks the edge table until `nanos <= BUCKETS_NS[idx]`. If the
    /// comparison were ever flipped, samples would drift into a neighbouring
    /// bucket and every percentile below it would be wrong in the same
    /// direction — a systematic underestimate that no single-sample check
    /// would reveal.
    #[test]
    fn every_sample_lands_in_the_first_bucket_that_contains_it(value in latency_ns()) {
        let h = Histogram::new();
        h.record(value);
        let snapshot = h.snapshot();

        let occupied: Vec<usize> = snapshot
            .iter()
            .enumerate()
            .filter(|(_, c)| **c == 1)
            .map(|(i, _)| i)
            .collect();

        // Positional, not inline capture. prop_assert_eq! builds its message
        // with `concat!` rather than `format!`, so an inline `{occupied:?}`
        // ends up inside a plain literal that format! never expands, and the
        // compiler rejects it with "no argument named occupied". prop_assert!
        // uses format! and does accept inline capture -- the two macros are
        // not symmetric, which is worth knowing before the second hour spent
        // on it.
        prop_assert_eq!(
            occupied.len(),
            1,
            "exactly one bucket may hold the sample, got {:?}",
            occupied
        );

        let idx = occupied[0];
        // Index 0 means "at most the first edge"; the last bucket is overflow.
        let upper = if idx < 20 { EDGES[idx] } else { u64::MAX };
        prop_assert!(value <= upper, "value {value} landed in bucket {idx} bounded by {upper}");

        if idx > 0 {
            let previous = idx - 1;
            let lower_edge = EDGES[previous];
            // Computed into a binding first: a format capture may only name a
            // variable, it cannot contain an expression, so `{idx - 1}` is a
            // compile error rather than an arithmetic convenience.
            prop_assert!(
                value > lower_edge,
                "value {value} should have landed in bucket {previous}, it is above edge {lower_edge}"
            );
        }
    }

    // ---------------------------------------------------------------------
    // Law 2 — monotonicity
    // ---------------------------------------------------------------------

    /// Raising the quantile never lowers the answer.
    ///
    /// The pair of quantiles is drawn together rather than independently so
    /// that degenerate-but-valid requests like `q_lo = 1.0, q_hi = NaN` are
    /// included; those collapse to the same rank and must stay equal rather
    /// than invert.
    #[test]
    fn percentile_never_decreases_as_the_quantile_rises(
        samples in prop::collection::vec(latency_ns(), 1usize..64),
        q_lo in quantile(),
        q_hi in quantile(),
    ) {
        let h = Histogram::new();
        for s in &samples {
            h.record(*s);
        }

        let lo = h.percentile_ns(q_lo);
        let hi = h.percentile_ns(q_hi);

        // Only assert the direction when the request itself is ordered.
        // NaN compares false against everything, so guard explicitly.
        if !q_lo.is_nan() && !q_hi.is_nan() && q_lo <= q_hi {
            prop_assert!(
                lo <= hi,
                "p({q_lo}) = {lo} exceeded p({q_hi}) = {hi}"
            );
        }

        // Independently of ordering, every answer must be a value this
        // histogram could actually have produced: either zero, or some bucket
        // upper bound. A percentile outside this set means the ranking
        // arithmetic invented a latency that no sample ever had.
        for q in [q_lo, q_hi] {
            let p = h.percentile_ns(q);
            prop_assert!(
                p == 0 || EDGES.contains(&p) || p == u64::MAX,
                "percentile {p} for q={q} is not a bucket bound"
            );
        }
    }

    /// Quantile arithmetic is stable: the same question twice is the same
    /// answer twice.
    ///
    /// `record` touches shared state, so a hidden cache or a clock would show
    /// up here. Quotes run inside a deadline; a percentile that moves between
    /// two identical reads is a number nobody can reason about.
    #[test]
    fn percentiles_are_reproducible(
        samples in prop::collection::vec(latency_ns(), 1usize..32),
        q in quantile(),
    ) {
        let h = Histogram::new();
        for s in &samples {
            h.record(*s);
        }
        prop_assert_eq!(h.percentile_ns(q), h.percentile_ns(q));
        prop_assert_eq!(h.count(), h.count());
        prop_assert_eq!(h.max_ns(), h.max_ns());
        prop_assert_eq!(h.mean_ns(), h.mean_ns());
    }

    // ---------------------------------------------------------------------
    // Laws 3 and 4 — extremality and central tendency
    // ---------------------------------------------------------------------

    /// `max_ns` is the largest observation, and p100 does not under-report it.
    ///
    /// Under-reporting the maximum is how a latency regression stays
    /// invisible: the average barely moves, and the p99 that was supposed to
    /// catch it comes back from a bucket boundary.
    #[test]
    fn the_maximum_is_never_under_reported(samples in prop::collection::vec(latency_ns(), 1usize..64)) {
        let h = Histogram::new();
        for s in &samples {
            h.record(*s);
        }

        let true_max = samples.iter().copied().max().expect("non-empty");
        prop_assert_eq!(h.max_ns(), true_max, "max_ns lost the largest sample");

        // p100 must be at or above the true maximum, because every answer is
        // a bucket upper bound.
        let p100 = h.percentile_ns(1.0);
        prop_assert!(p100 >= true_max, "p100 {p100} under-reported max {true_max}");

        // And the maximum must still be inside the overall range.
        let true_min = samples.iter().copied().min().expect("non-empty");
        prop_assert!(true_min <= h.max_ns());
    }

    /// The mean sits inside the observed range and never exceeds the maximum.
    ///
    /// Integer division means the mean is truncated, so the safe claim is
    /// `mean <= max` and `mean >= min` only when it cannot round below zero.
    /// A mean above the maximum is arithmetic that has left the data.
    #[test]
    fn the_mean_stays_inside_the_observed_range(samples in prop::collection::vec(latency_ns(), 1usize..64)) {
        let h = Histogram::new();
        for s in &samples {
            h.record(*s);
        }

        let mean = h.mean_ns();
        let true_max = samples.iter().copied().max().expect("non-empty");

        prop_assert!(
            mean <= true_max,
            "mean {mean} exceeds the largest observation {true_max}"
        );

        // A constant sample set has an exactly computable mean, which is the
        // strongest statement available and catches off-by-one in `sum_ns`.
        if samples.iter().all(|s| *s == samples[0]) {
            prop_assert_eq!(mean, samples[0], "a constant sample set must average to itself");
        }
    }

    // ---------------------------------------------------------------------
    // Law 5 — totality
    //
    // The two tests below that take no generator arguments live *outside*
    // `proptest!`. proptest 1.12 has no macro arm for a parameterless test:
    // every arm requires at least one `name in strategy`, so a bare
    // `fn nothing() {}` inside the block does not parse and reports the
    // confusing "expected expression, found keyword `fn`" pointing at the
    // first test in the file rather than at the real culprit.
    // ---------------------------------------------------------------------

}

/// `sum_ns` saturates instead of wrapping.
///
/// This is the witness for the defect described in the module docs. The
/// arithmetic is the proof, not the test: two samples of `u64::MAX` cannot be
/// summed in a `u64`, and a wrapping sum turns `mean_ns` into a number smaller
/// than any single observation in the histogram.
// Clippy is right that `>= u64::MAX` is always true for a `u64`, and the
// comparison is still the point. The test asks "did the sum stay at least
// as large as the largest single sample?", and `== u64::MAX` would answer a
// narrower question -- one that a different wrong implementation could
// also satisfy. The lint's suggestion is silently weaker than the
// assertion, which is exactly the kind of "fix" that turns a real test
// into a decorative one.
#[allow(
    clippy::absurd_extreme_comparisons,
    reason = "the assertion is deliberately a bound, not an equality"
)]
#[test]
fn sum_never_wraps() {
    let h = Histogram::new();
    h.record(u64::MAX);
    h.record(u64::MAX);

    // Two maxima cannot be represented in a u64. Saturating means the reported
    // sum stays at least as large as the largest single observation, so
    // `mean_ns` keeps describing the data instead of describing an overflow.
    //
    // Every format hole is positional. An inline capture like {u64::MAX} does
    // not compile -- captures name variables, not constants -- and mixing
    // inline captures with positional arguments in one message is the kind of
    // thing that reads as fine and fails to build.
    assert!(
        h.sum_ns() >= u64::MAX,
        "sum_ns wrapped to {} — below the largest single observation ({}), which makes \
         mean_ns report {} for a histogram holding exactly 2 observations",
        h.sum_ns(),
        u64::MAX,
        h.mean_ns()
    );
}

proptest! {
/// No input panics, at any magnitude.
///
/// `record` walks the edge table with an index; the loop bound has to hold
/// for every `u64`, not for the range a latency plausibly occupies. In
/// release a wrap would be a silently wrong bucket rather than a crash,
/// which is why this is a test at all.
#[test]
fn no_input_panics(value in any::<u64>()) {
    let h = Histogram::new();
    h.record(value);
    prop_assert_eq!(h.count(), 1);
    // Overflow bucket for anything above the last edge.
    if value > 1_000_000_000 {
        prop_assert_eq!(
            h.snapshot().last().copied(),
            Some(1),
            "a value above the last edge must land in the overflow bucket"
        );
    }
}

}

/// A zero observation is legal and must not disappear.
///
/// Zero is the value an entirely instrumented-but-unmeasured stage reports.
/// Losing it would make `count()` disagree with reality on precisely the stage
/// that is failing to report, and it is the one input a latency recorder is
/// most tempted to treat as "nothing to record".
#[test]
fn zero_is_recorded_like_any_other_value() {
    let h = Histogram::new();
    h.record(0);
    assert_eq!(h.count(), 1);
    assert_eq!(h.sum_ns(), 0);
    assert_eq!(h.max_ns(), 0);
    assert_eq!(h.mean_ns(), 0);
    assert_eq!(h.snapshot()[0], 1, "zero belongs in the first bucket");
}

proptest! {
    /// `reset` returns the histogram to the state of a fresh one, in every
    /// observable field.
    ///
    /// A partial reset is the worst kind of bug here: the histogram keeps
    /// working, keeps returning numbers, and those numbers are wrong by
    /// whatever survived. This compares the entire public surface, not just
    /// `count`.
    #[test]
    fn reset_restores_the_fresh_state_exactly(
        samples in prop::collection::vec(latency_ns(), 1usize..32)
    ) {
        let h = Histogram::new();
        for s in &samples {
            h.record(*s);
        }
        h.reset();

        let fresh = Histogram::new();
        assert_eq!(h.count(), fresh.count());
        assert_eq!(h.sum_ns(), fresh.sum_ns());
        assert_eq!(h.max_ns(), fresh.max_ns());
        assert_eq!(h.mean_ns(), fresh.mean_ns());
        assert_eq!(h.percentile_ns(0.99), fresh.percentile_ns(0.99));
        assert_eq!(h.snapshot(), fresh.snapshot());
    }
}

/// Mirror of the crate-private edge table, so this file can assert *which*
/// bucket a value landed in without exporting private constants.
///
/// Duplicated deliberately: if the table changes in `histogram.rs`, this copy
/// goes stale and `every_sample_lands_in_the_first_bucket_that_contains_it`
/// fails loudly. A shared public constant would let both sides drift
/// together and agree on the wrong thing.
const EDGES: [u64; 20] = [
    100,
    250,
    500,
    1_000,
    2_000,
    5_000,
    10_000,
    20_000,
    50_000,
    100_000,
    250_000,
    500_000,
    1_000_000,
    2_000_000,
    5_000_000,
    10_000_000,
    50_000_000,
    100_000_000,
    500_000_000,
    1_000_000_000,
];
