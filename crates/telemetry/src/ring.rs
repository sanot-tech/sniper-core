//! Bounded trace ring — fixed memory, oldest-first eviction.
//!
//! A sniper bot in production must be able to say "show me the last 50
//! decisions" without unbounded memory growth. This ring allocates once and
//! then only moves a head index.
//!
//! Concurrency: the producer side takes the lock only to append (a pointer
//! bump plus one store); the consumer side takes it to drain. Draining runs on
//! a separate thread, so the hot path never waits behind a slow reader for
//! long. If that ever matters, swap the mutex for a lock-free sequence — the
//! public API does not change.

use std::collections::VecDeque;
use std::sync::Mutex;

#[derive(Debug)]
pub struct TraceRing<T> {
    inner: Mutex<VecDeque<T>>,
    capacity: usize,
    evicted: crate::Counter,
}

impl<T> TraceRing<T> {
    /// Creates a ring holding at most `capacity` records.
    ///
    /// # Panics
    /// Panics if `capacity` is 0.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "trace ring capacity must be non-zero");
        Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            evicted: crate::Counter::new(),
        }
    }

    /// Appends a record, evicting the oldest when full.
    pub fn push(&self, record: T) {
        let Ok(mut q) = self.inner.lock() else {
            // A poisoned mutex means a panic while holding it. Dropping the
            // record is the correct behaviour for telemetry: never propagate.
            return;
        };
        if q.len() == self.capacity {
            q.pop_front();
            self.evicted.inc();
        }
        q.push_back(record);
    }

    /// Number of records currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().map_or(0, |q| q.len())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Total records dropped because the ring was full.
    ///
    /// Non-zero means the consumer is not draining fast enough — that is a
    /// signal, not a bug to hide.
    #[must_use]
    pub fn evicted(&self) -> u64 {
        self.evicted.get()
    }

    /// Removes and returns every held record, oldest first.
    pub fn drain(&self) -> Vec<T> {
        match self.inner.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Reads a snapshot without consuming, oldest first.
    #[must_use]
    pub fn snapshot(&self) -> Vec<T>
    where
        T: Clone,
    {
        match self.inner.lock() {
            Ok(q) => q.iter().cloned().collect(),
            Err(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_order_and_respects_capacity() {
        let ring = TraceRing::new(3);
        for i in 0..3 {
            ring.push(i);
        }
        assert_eq!(ring.snapshot(), vec![0, 1, 2]);
        assert_eq!(ring.len(), 3);
    }

    #[test]
    fn evicts_oldest_when_full() {
        let ring = TraceRing::new(3);
        for i in 0..10 {
            ring.push(i);
        }
        // Newest three survive, oldest seven are gone.
        assert_eq!(ring.snapshot(), vec![7, 8, 9]);
        assert_eq!(ring.evicted(), 7, "every dropped record must be counted");
        assert_eq!(ring.capacity(), 3);
    }

    #[test]
    fn drain_empties_the_ring() {
        let ring = TraceRing::new(4);
        ring.push(10);
        ring.push(20);
        assert_eq!(ring.drain(), vec![10, 20]);
        assert!(ring.is_empty());
        assert_eq!(ring.drain(), Vec::<i32>::new());
    }

    #[test]
    fn concurrent_producers_stay_within_capacity() {
        use std::sync::Arc;
        use std::thread;

        let ring = Arc::new(TraceRing::new(64));
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let ring = Arc::clone(&ring);
                thread::spawn(move || {
                    for i in 0..500 {
                        ring.push(t * 1_000 + i);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        // The whole point: bounded memory no matter the input volume.
        assert!(ring.len() <= 64);
        // Counted plus evicted must account for every single push. Saturating
        // rather than casting, so the assertion cannot be satisfied by a
        // truncated count on a 32-bit target.
        let evicted = usize::try_from(ring.evicted()).unwrap_or(usize::MAX);
        assert_eq!(ring.len() + evicted, 2_000);
    }

    #[test]
    #[should_panic(expected = "capacity must be non-zero")]
    fn zero_capacity_is_rejected_early() {
        let _ = TraceRing::<u8>::new(0);
    }
}
