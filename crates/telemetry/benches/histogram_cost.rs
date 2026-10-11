//! ⏱ `HistogramCost` — the archetype's central claim, measured.
//!
//! ## What is being claimed
//!
//! `sniper-telemetry` describes itself as "high performance with minimal
//! per-decision cost", and `histogram.rs` now uses `fetch_update` with
//! `saturating_add` rather than a plain `fetch_add` in order to stop an
//! overflow from inverting `mean_ns`.
//!
//! That is a trade, and the honest way to take a trade is to measure it. The
//! claim under test is therefore:
//!
//! > saturating `sum_ns` costs nothing that a latency budget can see.
//!
//! ## How to read these numbers
//!
//! `record/uncontended` is the number that matters. It is the cost of one
//! observation on the hot path with no other thread involved, which is the
//! situation the pipeline is designed to produce.
//!
//! `record/contended` exists to bound the downside rather than to be a target.
//! `fetch_update` can spin where `fetch_add` cannot, so the contended case is
//! where a regression would show up first. The comment in `histogram.rs`
//! claims the pipeline keeps each stage on a single task; this benchmark is
//! what would falsify that claim if it stopped being true.
//!
//! Criterion prints a change against the previous run, so the comparison that
//! matters is `cargo bench`, then `cargo bench` again after a change -- not a
//! single absolute number, which is machine-specific and says nothing on its
//! own.

use criterion::{criterion_group, criterion_main, Criterion};
use sniper_telemetry::Histogram;
use std::sync::Arc;
use std::thread;

/// Nanoseconds a sub-microsecond stage actually reports. Decode, build and
/// sign all land here, so this is the value the hot path records.
const TYPICAL_LATENCY_NS: u64 = 850;

fn record_cost(c: &mut Criterion) {
    // ---------------------------------------------------------------------
    // Uncontended: the case the pipeline is built around.
    // ---------------------------------------------------------------------

    let mut solo = c.benchmark_group("record");
    // A histogram with no reads between writes, so this is the pure store.
    solo.bench_function("uncontended", |b| {
        let h = Histogram::new();
        b.iter(|| h.record(TYPICAL_LATENCY_NS));
    });

    // Reading between writes. This is the honest end-to-end number: an
    // operator watching a dashboard reads the histogram far more often than
    // the hot path writes to it, and `count()` walks all 21 buckets.
    solo.bench_function("uncontended_then_read", |b| {
        let h = Histogram::new();
        b.iter(|| {
            h.record(TYPICAL_LATENCY_NS);
            h.count()
        });
    });
    solo.finish();

    // ---------------------------------------------------------------------
    // Contended: the downside, bounded.
    // ---------------------------------------------------------------------
    //
    // Four threads on one histogram. This is not a configuration the bot
    // should ever reach -- Stage::DECODE onward runs on the single trading
    // task -- but if that ever changes, this number is what moves first, and
    // finding out here is much cheaper than finding out in a latency incident.

    let mut contended = c.benchmark_group("record_contended");
    contended.bench_function("four_threads_one_histogram", |b| {
        b.iter(|| {
            let h = Arc::new(Histogram::new());
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let h = Arc::clone(&h);
                    thread::spawn(move || {
                        for _ in 0..1_000 {
                            h.record(TYPICAL_LATENCY_NS);
                        }
                    })
                })
                .collect();
            for handle in handles {
                handle.join().expect("recorder thread must not panic");
            }
            h.count()
        });
    });
    contended.finish();
}

/// What the saturating sum actually cost.
///
/// The fix in `histogram.rs` replaced `fetch_add` with a saturating
/// `fetch_update`. This benchmark isolates the two operations on a bare
/// `AtomicU64`, away from the bucket walk, so the comparison is between the
/// two atomic primitives and nothing else.
///
/// This is the measurement that the commit message for that fix promised.
/// Without it the claim "costs nothing a latency budget can see" is just an
/// assertion, and an assertion about performance is worth exactly as much as
/// the person who wrote it believed.
///
/// Expected reading: both in the same single-digit nanosecond band, with
/// `fetch_add` perhaps a fraction lower. A result where they differ by 2x or
/// more would mean the trade needs revisiting -- in which case the bucket
/// counts are where to give ground, since `count()` is read far more often
/// than it is written and a weaker invariant there costs less than a slower
/// write on every single observation.
fn saturation_cost(c: &mut Criterion) {
    use std::sync::atomic::{AtomicU64, Ordering};

    let mut group = c.benchmark_group("sum_ns_primitive");

    // The operation as it was before the fix: unconditional wrapping add.
    group.bench_function("fetch_add_wrapping", |b| {
        let total = AtomicU64::new(0);
        b.iter(|| total.fetch_add(TYPICAL_LATENCY_NS, Ordering::Relaxed));
    });

    // The operation as it is now: CAS loop with saturating arithmetic.
    group.bench_function("fetch_update_saturating", |b| {
        let total = AtomicU64::new(0);
        b.iter(|| {
            let _ = total.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_add(TYPICAL_LATENCY_NS))
            });
        });
    });

    // Control. This is the floor: a relaxed load with no write at all. Any
    // difference between the two entries above that is not explained by the
    // gap between them and this number is measurement noise.
    group.bench_function("load_only_control", |b| {
        let total = AtomicU64::new(0);
        b.iter(|| total.load(Ordering::Relaxed));
    });

    group.finish();
}

/// The crate's own headline claim, checked.
///
/// `counter.rs` says "Relaxed atomics: ~1 ns, no locks, no allocation, no
/// syscalls" and the crate root says the always-on layer must be "cheaper than
/// a branch misprediction". A mispredicted branch costs roughly 15-20 cycles,
/// so the bar is meaningful rather than aspirational.
///
/// `gauge_set` is included next to `counter_inc` on purpose. Both are Relaxed
/// atomics on the same machine, and they are not the same price: a plain
/// `store` compiles to a single move, while `fetch_add` is a locked
/// read-modify-write. Grouping them under one "Relaxed atomics are ~1 ns"
/// claim would hide that the cheap one is cheap and the expensive one is what
/// the hot path actually calls.
fn counter_cost(c: &mut Criterion) {
    let mut counters = c.benchmark_group("counter");
    counters.bench_function("counter_inc", |b| {
        let counter = sniper_telemetry::Counter::new();
        b.iter(|| counter.inc());
    });
    counters.bench_function("gauge_set", |b| {
        let gauge = sniper_telemetry::Gauge::new();
        b.iter(|| gauge.set(42));
    });
    counters.finish();
}

/// The probe with instrumentation compiled out.
///
/// The crate promises that `debug-trace` off means the probe costs *nothing*,
/// not "a little". A probe should measure the same as an empty loop body. If
/// this group ever separates from the empty-loop group, `#[inline(always)]`
/// has stopped doing its job and production is paying for a measurement it was
/// promised it would not pay for.
fn probe_cost_when_disabled(c: &mut Criterion) {
    use sniper_telemetry::{Metrics, Probe, Stage};

    let mut probes = c.benchmark_group("probe");
    probes.bench_function("start_finish", |b| {
        let m = Metrics::new();
        b.iter(|| Probe::start(&m, Stage::Decode).finish());
    });

    // Control. If a probe costs measurably more than this, it is not free.
    probes.bench_function("empty_loop_control", |b| {
        let m = Metrics::new();
        b.iter(|| {
            let _ = &m;
            core::hint::black_box(());
        });
    });
    probes.finish();
}

criterion_group!(
    benches,
    record_cost,
    saturation_cost,
    counter_cost,
    probe_cost_when_disabled
);
criterion_main!(benches);
