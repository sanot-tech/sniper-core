//! 🦅 sniper-telemetry — the development archetype.
//!
//! **High performance with minimal overhead and permanent debug presence.**
//!
//! A sniper bot lives or dies on microseconds, but a bot you cannot see is a
//! bot you cannot trust. This crate resolves that tension with three rules:
//!
//! 1. **Measurement is a build flag, not a runtime switch.**
//!    [`Probe`] compiles to literally nothing unless the `debug-trace`
//!    feature is on. Production never pays for instrumentation.
//!
//! 2. **The always-on layer must be cheaper than a branch misprediction.**
//!    [`Counter`] and [`Gauge`] are `Relaxed` atomics: ~1 ns, no locks, no
//!    allocation, no syscalls. They stay enabled in every build because they
//!    are what an operator actually watches.
//!
//! 3. **Nothing in the hot path may allocate or block.**
//!    [`TraceRing`] has a fixed capacity assigned at construction and
//!    overwrites the oldest record instead of growing. Draining happens on
//!    another thread, so the producer never waits on a lock it owns.
//!
//! ```no_run
//! use sniper_telemetry::{Metrics, Probe, Stage};
//!
//! let m = Metrics::new();
//! let p = Probe::start(&m, Stage::Decode);
//! // ... hot-path work: no allocation, no locks ...
//! p.finish();
//! assert_eq!(m.stage(Stage::Decode).count(), 1);
//! ```

pub mod counter;
pub mod histogram;
pub mod probe;
pub mod ring;

pub use counter::{Counter, Gauge};
pub use histogram::Histogram;
pub use probe::Probe;
pub use ring::TraceRing;

/// Every hot-path stage, in execution order.
///
/// The ordinal order is meaningful: [`Stage::RECEIVE`] happens on the gRPC
/// reader task, everything from [`Stage::DECODE`] onwards happens on the
/// trading task. A latency budget per stage turns "the bot is slow" into
/// "the bot is slow in `Stage::BUILD`", which is actionable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Stage {
    /// Bytes arrived from the Yellowstone gRPC stream.
    Receive = 0,
    /// Protobuf decoded and launch-event discrimination done.
    Decode = 1,
    /// Venue selected, PDA/account resolution completed (cache hits only).
    Resolve = 2,
    /// Quote computed and instructions assembled into a transaction.
    Build = 3,
    /// Ed25519 signature applied.
    Sign = 4,
    /// Handed to the transport fan-out (Jito / Helius Sender / RPC).
    Send = 5,
    /// Landed on chain and confirmed at the requested commitment.
    Land = 6,
}

impl Stage {
    /// All stages in pipeline order.
    pub const ALL: [Stage; 7] = [
        Stage::Receive,
        Stage::Decode,
        Stage::Resolve,
        Stage::Build,
        Stage::Sign,
        Stage::Send,
        Stage::Land,
    ];

    /// Stable index into fixed-size arrays.
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Lowercase identifier for logs and metric exports.
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Receive => "receive",
            Self::Decode => "decode",
            Self::Resolve => "resolve",
            Self::Build => "build",
            Self::Sign => "sign",
            Self::Send => "send",
            Self::Land => "land",
        }
    }
}

/// Aggregated telemetry for one stage: how many times, and how long.
///
/// `Counters` are always on. The [`Histogram`] only records when the
/// `debug-trace` feature is enabled.
#[derive(Debug, Default)]
pub struct StageTimings {
    count: Counter,
    errors: Counter,
    histogram: Histogram,
}

impl StageTimings {
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn record(&self, nanos: u64) {
        self.count.inc();
        self.histogram.record(nanos);
    }

    #[inline]
    pub fn count(&self) -> u64 {
        self.count.get()
    }

    #[inline]
    pub fn errors(&self) -> u64 {
        self.errors.get()
    }

    #[inline]
    pub fn incr_errors(&self) {
        self.errors.inc();
    }

    #[inline]
    #[must_use]
    pub fn histogram(&self) -> &Histogram {
        &self.histogram
    }
}

/// The root telemetry object. Cheap to create once, then share by reference.
#[derive(Debug, Default)]
pub struct Metrics {
    stages: [StageTimings; 7],
}

impl Metrics {
    /// Not `const`: [`Histogram`] allocates its bucket table, and a const
    /// constructor would force every caller into a lazy static for no gain —
    /// one `Metrics` is built once at startup and shared by reference.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self {
            stages: [
                StageTimings::new(),
                StageTimings::new(),
                StageTimings::new(),
                StageTimings::new(),
                StageTimings::new(),
                StageTimings::new(),
                StageTimings::new(),
            ],
        }
    }

    #[inline]
    #[must_use]
    pub fn stage(&self, stage: Stage) -> &StageTimings {
        &self.stages[stage.index()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_ordinals_match_pipeline_order() {
        // Guards against reordering the enum and silently moving budgets.
        assert_eq!(Stage::Receive.index(), 0);
        assert_eq!(Stage::Land.index(), 6);
        assert_eq!(Stage::ALL.len(), 7);
    }

    #[test]
    fn stage_names_are_stable() {
        // These strings end up in dashboards; changing them is a breaking change.
        assert_eq!(Stage::Receive.as_str(), "receive");
        assert_eq!(Stage::Land.as_str(), "land");
    }

    #[test]
    fn metrics_is_sync_and_shareable() {
        // If this stops compiling, someone introduced a non-Sync field into the
        // hot path, which would forbid sharing Metrics across tokio tasks.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Metrics>();
        assert_send_sync::<TraceRing<u64>>();
    }

    #[test]
    fn stage_timings_count_and_error() {
        let m = Metrics::new();
        m.stage(Stage::Decode).record(1_000);
        m.stage(Stage::Decode).record(2_000);
        m.stage(Stage::Decode).incr_errors();
        assert_eq!(m.stage(Stage::Decode).count(), 2);
        assert_eq!(m.stage(Stage::Decode).errors(), 1);
    }
}
