//! Stage probes — the "debug presence" mechanism.
//!
//! With `debug-trace` **off** (the default, and what production runs):
//! `Probe::start` and `Probe::finish` are `#[inline(always)]` empty functions.
//! LLVM removes them entirely, so a release build pays exactly zero.
//!
//! With `debug-trace` **on**: each probe reads the clock twice and pushes one
//! observation into the stage histogram.

// Only needed when instrumentation is actually compiled in. Importing it
// unconditionally would leave an unused-import warning in production builds.
#[cfg(feature = "debug-trace")]
use std::time::Instant;

use crate::{Metrics, Stage};

/// Times one stage of the hot path.
///
/// Deliberately `Drop`-free: implementing `Drop` would add a landing pad and
/// a panic-unwind edge to every call site. Forgetting to call `finish()` is a
/// lost measurement, not a correctness bug, and the compiler keeps that
/// mistake cheap.
pub struct Probe<'a> {
    #[cfg(feature = "debug-trace")]
    metrics: &'a Metrics,
    #[cfg(feature = "debug-trace")]
    stage: Stage,
    #[cfg(feature = "debug-trace")]
    start: Instant,
    #[cfg(not(feature = "debug-trace"))]
    _marker: core::marker::PhantomData<&'a ()>,
}

impl<'a> Probe<'a> {
    /// Begins timing `stage`.
    #[inline(always)]
    #[must_use]
    pub fn start(metrics: &'a Metrics, stage: Stage) -> Self {
        #[cfg(feature = "debug-trace")]
        {
            Self {
                metrics,
                stage,
                start: Instant::now(),
            }
        }
        #[cfg(not(feature = "debug-trace"))]
        {
            let _ = (metrics, stage);
            Self {
                _marker: core::marker::PhantomData,
            }
        }
    }

    /// Ends timing and records the observation.
    #[inline(always)]
    pub fn finish(self) {
        #[cfg(feature = "debug-trace")]
        {
            let elapsed = self.start.elapsed().as_nanos();
            // Saturating cast: a broken clock must not panic a trading bot.
            self.metrics
                .stage(self.stage)
                .record(u64::try_from(elapsed).unwrap_or(u64::MAX));
        }
    }

    /// Marks the operation as failed without changing the timing.
    #[inline(always)]
    pub fn fail(self) {
        #[cfg(feature = "debug-trace")]
        {
            self.metrics.stage(self.stage).incr_errors();
            self.finish();
        }
        #[cfg(not(feature = "debug-trace"))]
        {
            self.finish();
        }
    }
}

/// Convenience macro so call sites read like labelled blocks and cannot
/// accidentally measure the wrong expression.
#[macro_export]
macro_rules! probe {
    ($metrics:expr, $stage:expr, $body:expr) => {{
        let __probe = $crate::Probe::start($metrics, $stage);
        let __result = $body;
        __probe.finish();
        __result
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Metrics;

    #[test]
    fn probe_compiles_in_both_configurations() {
        // The archetype's central promise: a probe is always *callable* and
        // the call site is byte-identical in both builds. Only the cost
        // changes. This test therefore runs in every configuration.
        let m = Metrics::new();
        let p = Probe::start(&m, Stage::Decode);
        let mut acc = 0u64;
        for i in 0..50_000 {
            acc = acc.wrapping_add(i);
        }
        assert!(acc > 0);
        p.finish();

        // Without the feature the probe is a no-op, so nothing is recorded.
        // That is the intended zero-cost behaviour, asserted explicitly
        // rather than left as an untested assumption.
        #[cfg(feature = "debug-trace")]
        assert_eq!(m.stage(Stage::Decode).count(), 1, "probe must record once");
        #[cfg(not(feature = "debug-trace"))]
        assert_eq!(
            m.stage(Stage::Decode).count(),
            0,
            "probes must cost nothing in production"
        );
    }

    #[test]
    fn macro_form_is_callable_in_both_builds() {
        // The macro must never be feature-gated away, otherwise debug and
        // release builds would need different call sites.
        let m = Metrics::new();
        let out = probe!(&m, Stage::Resolve, 6 * 7);
        assert_eq!(out, 42);
    }

    #[test]
    #[cfg(feature = "debug-trace")]
    fn probe_fail_marks_error_and_still_records() {
        let m = Metrics::new();
        Probe::start(&m, Stage::Build).fail();
        assert_eq!(m.stage(Stage::Build).count(), 1);
        assert_eq!(m.stage(Stage::Build).errors(), 1);
    }

    #[test]
    #[cfg(feature = "debug-trace")]
    fn macro_records_when_instrumented() {
        let m = Metrics::new();
        let out = probe!(&m, Stage::Resolve, 6 * 7);
        assert_eq!(out, 42);
        assert_eq!(m.stage(Stage::Resolve).count(), 1);
    }

    #[cfg(feature = "debug-trace")]
    #[test]
    fn debug_trace_reports_nonzero_duration() {
        let m = Metrics::new();
        Probe::start(&m, Stage::Sign).finish();
        let hist = m.stage(Stage::Sign).histogram();
        // Even an instant probe reads the clock twice, so ns must be > 0.
        assert!(hist.sum_ns() > 0, "clock readings should register");
    }
}
