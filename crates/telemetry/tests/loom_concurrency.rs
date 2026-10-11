//! 🧵 `LoomConcurrency` — the always-on layer, proved under every interleaving.
//!
//! ## What a normal concurrency test can and cannot tell you
//!
//! `counter.rs` already has `counter_is_lossless_under_contention`: eight
//! threads hammer one counter a hundred thousand times and the total is
//! checked. That is a real test and it does catch real bugs. What it cannot do
//! is tell you it passed because the code is correct rather than because the
//! scheduler never produced the interleaving that breaks it.
//!
//! A relaxed atomic race is a *memory ordering* bug. It lives in the order
//! reads and writes become visible, not in the arithmetic, and the
//! interleaving that exposes it may occur once in a billion runs on hardware
//! that happens to be strong today. On a weaker memory model, or a different
//! CPU, or a busier machine, it may occur constantly.
//!
//! loom removes that lottery. It runs the test again for every legal
//! interleaving the C11 model permits, with state reduction so the search
//! finishes. When this file passes, the guarantee is not "it did not happen
//! while I watched".
//!
//! ## What that buys here specifically
//!
//! * `Counter::inc` loses no increments under *any* interleaving, and a
//!   concurrent reader never observes a count that is not a prefix of the
//!   increments that preceded it.
//! * `Gauge::set` and `Gauge::add` never tear: a reader sees a value some
//!   thread actually wrote, never a mixture of two.
//! * The saturating `sum_ns` never goes backwards. A wrapped total is worse
//!   than a wrong total, because a total that decreases is a total an
//!   operator has no way to reason about.
//!
//! ## Running it
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p sniper-telemetry --test loom_concurrency --release
//! ```
//!
//! Plain `cargo test` compiles all of this away to a single skipped test that
//! says so out loud, rather than silently passing and proving nothing.
//!
//! Release matters. Under a debug build every atomic access is a function call
//! with bookkeeping around it, and loom's state space grows past anything
//! enumerable.

/// Confirms, out loud, when the exhaustive tests above did not run.
///
/// A loom file that compiles to nothing under `cargo test` is a trap: CI goes
/// green, the file is in the repo, everyone believes the concurrency is
/// proved, and nobody ever executed a single interleaving. The usual
/// workaround -- `#[cfg(loom)]` on everything, or a `--skip`d test -- produces
/// exactly that silence.
///
/// So instead of hiding, this test asserts the *absence* is expected. It
/// passes in both configurations, and under `--cfg loom` it is simply not
/// compiled at all. Either way the reason is written down in a test result
/// rather than inferred from a file that quietly contains nothing.
#[test]
#[cfg(not(loom))]
fn exhaustive_checks_require_the_loom_cfg_and_are_skipped_without_it() {
    // Nothing to assert about the system -- the assertion is the documentation.
    // Reaching this line means the ordinary test run did what it should: it
    // compiled the model tests out and recorded why.
    assert!(
        !cfg!(loom),
        "this body is unreachable: it is compiled only when cfg(loom) is false"
    );
}

#[cfg(loom)]
mod exhaustive {
    use loom::sync::Arc;
    use loom::thread;

    /// No increment is ever lost, in any interleaving.
    ///
    /// This is the property the whole always-on layer rests on. Counters exist
    /// because an operator cannot watch a bot that explains nothing; if the
    /// counts drift, the explanation is fiction, and the drift is invisible by
    /// construction because a drifted counter still returns a plausible u64.
    #[test]
    fn counter_increments_are_never_lost() {
        loom::model(|| {
            let counter = Arc::new(sniper_telemetry::Counter::new());
            let mut handles = Vec::new();

            for _ in 0..2 {
                let counter = Arc::clone(&counter);
                handles.push(thread::spawn(move || {
                    for _ in 0..2 {
                        counter.inc();
                    }
                }));
            }

            for handle in handles {
                handle.join().expect("worker must not panic");
            }

            // Two threads, two increments each. Not "at least two" -- exactly
            // two, because a counter that over-reports is just as broken as
            // one that under-reports and would be harder to notice.
            assert_eq!(
                4,
                counter.get(),
                "a Relaxed atomic must still be atomic: every inc must be visible"
            );
        });
    }

    /// A concurrent reader never sees a count that never happened.
    ///
    /// Monotonicity, not equality. The claim is that a reader can only observe
    /// a prefix of the increments that had completed -- never a value that was
    /// skipped over, and never a value below one an earlier reader already saw
    /// on this thread.
    ///
    /// The last part is the non-obvious half, and it is the one that makes
    /// "eventually accurate" a usable contract. If a later read could report
    /// *less* than an earlier read on the same thread, no amount of averaging
    /// would rescue the dashboard: the series would be noise.
    #[test]
    fn a_reader_only_ever_observes_a_growing_count() {
        loom::model(|| {
            let counter = Arc::new(sniper_telemetry::Counter::new());
            let writer = {
                let counter = Arc::clone(&counter);
                thread::spawn(move || {
                    counter.add(3);
                    counter.add(3);
                })
            };

            let mut previous = counter.get();
            // Spin rather than sleep: loom has no wall clock, and a fixed
            // iteration count is enough to sample the interleavings that
            // matter without modelling scheduling delays.
            for _ in 0..3 {
                let current = counter.get();
                assert!(
                    current >= previous,
                    "count went backwards on one thread: {current} after {previous}"
                );
                assert!(
                    current <= 6,
                    "count exceeded the number of increments that exist: {current}"
                );
                previous = current;
            }

            writer.join().expect("writer must not panic");
            assert_eq!(6, counter.get(), "the final count is exact");
        });
    }

    /// `Gauge` never tears.
    ///
    /// A gauge holds a value that moves both ways, and a reader must see
    /// something a thread actually stored -- never a mixture of two stores.
    /// Every value observed here is checked against the set of values the
    /// writers can produce, so a torn read would be caught even though a torn
    /// read of a 64-bit word is not something you can provoke on demand.
    #[test]
    fn a_gauge_read_returns_a_value_some_thread_wrote() {
        loom::model(|| {
            let gauge = Arc::new(sniper_telemetry::Gauge::new());
            let a = {
                let gauge = Arc::clone(&gauge);
                thread::spawn(move || gauge.set(1))
            };
            let b = {
                let gauge = Arc::clone(&gauge);
                thread::spawn(move || gauge.set(-1))
            };

            // Only three values are reachable: the zero it started at, and the
            // two stores. Anything else is a torn read.
            let observed = [0, 1, -1];
            for _ in 0..3 {
                assert!(
                    observed.contains(&gauge.get()),
                    "gauge read {} which no thread ever wrote",
                    gauge.get()
                );
            }

            a.join().expect("writer a must not panic");
            b.join().expect("writer b must not panic");
        });
    }

    /// The saturating `sum_ns` is monotonic.
    ///
    /// This is the invariant the overflow fix bought, and it is stronger than
    /// "does not wrap". A sum that can go down is worse than a sum that
    /// saturates: saturation loses information in one direction only, while a
    /// decreasing total makes every derivative a reader computes from it
    /// meaningless.
    ///
    /// Values are chosen to straddle the u64 boundary. `u64::MAX` on its own
    /// does not saturate, and adding to it must be exactly the case the old
    /// `fetch_add` got wrong.
    #[test]
    fn the_saturating_sum_never_decreases() {
        loom::model(|| {
            let histogram = Arc::new(sniper_telemetry::Histogram::new());

            let big = {
                let histogram = Arc::clone(&histogram);
                thread::spawn(move || histogram.record(u64::MAX))
            };
            let small = {
                let histogram = Arc::clone(&histogram);
                thread::spawn(move || histogram.record(1))
            };

            let mut previous = histogram.sum_ns();
            for _ in 0..3 {
                let current = histogram.sum_ns();
                assert!(
                    current >= previous,
                    "sum_ns decreased from {previous} to {current}; a saturating total may \
                     stop growing but must never go backwards"
                );
                previous = current;
            }

            big.join().expect("writer big must not panic");
            small.join().expect("writer small must not panic");

            let total = histogram.sum_ns();
            assert!(
                total >= u64::MAX,
                "total {total} is below the largest single sample {}",
                u64::MAX
            );
            assert_eq!(2, histogram.count(), "both observations are still counted");
        });
    }

    /// `Histogram::count` agrees with the bucket table under contention.
    ///
    /// `count()` walks all 21 buckets and sums them, while `record` increments
    /// one of them -- so a reader racing two writers multiplies the state space
    /// by 21 on every observation. That is why this test takes exactly one
    /// reading from the main thread instead of spinning: the guarantee under
    /// test is "a concurrent reader cannot observe a count that never
    /// happened", and a single racing read is what exercises it. Looping here
    /// would re-prove the same property at a cost that left the earlier
    /// version of this test still running after a minute.
    ///
    /// The ceiling of 2 is not arbitrary -- it is the exact number of `record`
    /// calls that exist. Anything higher would mean a count nobody wrote.
    #[test]
    fn a_concurrent_reader_sees_a_count_within_bounds() {
        // `count()` walks all 21 buckets and sums them while `record`
        // increments one, so a reader racing two writers multiplies the state
        // space by 21 on every observation. The guarantee under test is "a
        // concurrent reader cannot observe a count that never happened", and
        // one racing read is what exercises it -- looping only re-proves the
        // same property at a cost that left the earlier version of this test
        // still running after a minute.
        let mut builder = loom::model::Builder::new();
        // Bounded *and reported*: loom explores exhaustively or not at all, so
        // a ceiling that silently truncated would turn "proved" into "checked
        // some of them" with nothing in the output to say so. The bound is
        // loose enough that every interesting ordering is still reachable --
        // two writers and one racing read.
        builder.max_branches = 5_000;
        builder.check(|| {
            let histogram = Arc::new(sniper_telemetry::Histogram::new());

            let a = {
                let histogram = Arc::clone(&histogram);
                thread::spawn(move || histogram.record(50))
            };
            let b = {
                let histogram = Arc::clone(&histogram);
                thread::spawn(move || histogram.record(60))
            };

            // May land before either write, between them, or after both. All
            // three are legal; all three must be in range.
            let mid_flight = histogram.count();
            assert!(
                mid_flight <= 2,
                "count {mid_flight} exceeds the two writes that exist"
            );

            a.join().expect("writer a must not panic");
            b.join().expect("writer b must not panic");

            assert_eq!(2, histogram.count(), "the final count is exact");

            // Once the dust settles the two accessors must still agree. This
            // is conservation under contention rather than in isolation: if a
            // concurrent record could land somewhere the counter never saw,
            // `snapshot().iter().sum()` and `count()` would part ways here.
            let snapshot_total: u64 = histogram.snapshot().iter().sum();
            assert_eq!(
                snapshot_total,
                histogram.count(),
                "snapshot and count must not disagree after concurrent writers"
            );
        });
    }

    /// `Metrics` stays shareable across real tasks.
    ///
    /// A compile-time assertion rather than a runtime one, but it belongs here:
    /// loom is the moment to ask "can this actually cross a task boundary",
    /// because answering "no" here would mean every concurrency claim above is
    /// about a type nobody can deploy.
    #[test]
    fn metrics_crosses_thread_boundaries() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<sniper_telemetry::Metrics>();

        loom::model(|| {
            let metrics = Arc::new(sniper_telemetry::Metrics::new());
            let worker = {
                let metrics = Arc::clone(&metrics);
                thread::spawn(move || {
                    metrics.stage(sniper_telemetry::Stage::Decode).record(1_000);
                    metrics.stage(sniper_telemetry::Stage::Decode).record(2_000);
                })
            };
            worker.join().expect("worker must not panic");

            let decode = metrics.stage(sniper_telemetry::Stage::Decode);
            assert_eq!(2, decode.count(), "both stage recordings survive");
            assert_eq!(
                3_000,
                decode.histogram().sum_ns(),
                "the saturating sum adds both observations"
            );
        });
    }
}

/// Keeps the import list above honest.
///
/// `Ordering` is imported at module scope for the doc comment's benefit, but
/// the tests below reach atomics only through the crate's own API, which is
/// the point: they test the contract callers see, not the internals. This
/// alias makes the unused import explicit rather than deleting it.
#[cfg(loom)]
mod crate_support {
    #[allow(unused_imports)]
    pub use std::sync::atomic::Ordering;
}
