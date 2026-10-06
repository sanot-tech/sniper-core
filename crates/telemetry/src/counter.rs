//! Lock-free counters. These are the always-on half of the archetype.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// A monotonically increasing counter.
///
/// `Relaxed` is deliberate and correct here: we only ever need a value that
/// is *eventually* accurate, never a happens-before relationship with other
/// memory. That is what allows `SeqCst` fences to be skipped, which is the
/// difference between ~1 ns and ~20 ns per update.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    #[inline]
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    #[inline]
    #[must_use]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn reset(&self) {
        self.0.store(0, Ordering::Relaxed);
    }
}

/// A signed gauge for values that move both ways (price, queue depth, balance).
#[derive(Debug, Default)]
pub struct Gauge(AtomicI64);

impl Gauge {
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicI64::new(0))
    }

    #[inline]
    pub fn set(&self, v: i64) {
        self.0.store(v, Ordering::Relaxed);
    }

    #[inline]
    pub fn add(&self, d: i64) {
        self.0.fetch_add(d, Ordering::Relaxed);
    }

    #[inline]
    #[must_use]
    pub fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn counter_counts() {
        let c = Counter::new();
        c.inc();
        c.add(41);
        assert_eq!(c.get(), 42);
        c.reset();
        assert_eq!(c.get(), 0);
    }

    #[test]
    fn gauge_tracks_signed_values() {
        let g = Gauge::new();
        g.set(-5);
        g.add(3);
        assert_eq!(g.get(), -2);
    }

    #[test]
    fn counter_is_lossless_under_contention() {
        // Relaxed atomics must still be atomic: no lost updates even when many
        // threads hammer the same counter. This is the property that lets the
        // always-on layer be trusted in production.
        let c = Arc::new(Counter::new());
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let c = Arc::clone(&c);
                thread::spawn(move || {
                    for _ in 0..10_000 {
                        c.inc();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(c.get(), 80_000);
    }
}
