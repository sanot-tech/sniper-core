//! 🧷 `TestProfileInvariants` — the build configuration is itself under test.
//!
//! ## The question
//!
//! `[profile.release]` sets `panic = "abort"`. The obvious worry is that this
//! silently breaks the test suite, because `panic = "abort"` removes unwinding,
//! and unwinding is what `#[should_panic]` and `proptest` are built on. Both
//! would go quiet rather than loud: a `#[should_panic]` test would abort the
//! whole harness, and a `proptest` case that panicked would take the process
//! with it instead of shrinking to a minimal counter-example.
//!
//! That is the exact failure mode where a test suite stops testing. So it is
//! worth an answer, and the answer should be checked by CI rather than trusted.
//!
//! ## What was actually measured
//!
//! Both halves were run, not reasoned about:
//!
//! * `cargo test --release --test <probe>` with a `#[should_panic]` test —
//!   **passes**. Cargo forces `panic = "unwind"` onto the `bench` profile that
//!   `--release` tests use, overriding the `release` setting for test targets.
//! * `proptest!` with a generator that panics on one specific input — the run
//!   completes and reports `minimal failing input: x = 5`, which is only
//!   possible if unwinding survived into the test harness.
//!
//! So `panic = "abort"` costs nothing here and buys real size and speed in the
//! shipped binary. The one legitimate trade is stated explicitly below.
//!
//! ## What this file does
//!
//! It does not re-derive the answer. It makes the answer a **regression
//! guard**: if a future edit moves `panic = "abort"` somewhere it starts
//! applying to test targets, the test below dies in a way that is impossible
//! to mistake for an ordinary assertion failure.

use std::panic::{catch_unwind, AssertUnwindSafe};

/// Unwinding must reach the test harness.
///
/// This is the direct probe. `catch_unwind` only returns if unwinding exists;
/// under `panic = "abort"` it never gets the chance, and the whole harness
/// dies instead. That distinction is the entire point — an aborted harness
/// prints one line and leaves you guessing which test did it, whereas a
/// passing assertion is unambiguous.
///
/// Panics are unwound rather than aborted on purpose: `panic = "abort"` in
/// `[profile.release]` is not supposed to apply to test targets, and this is
/// what proves it does not.
#[test]
fn unwinding_reaches_the_test_harness() {
    let caught = catch_unwind(AssertUnwindSafe(|| {
        panic!("deliberate probe: unwinding is available to the test harness");
    }));

    assert!(
        caught.is_err(),
        "catch_unwind returned Ok, which means the panic never happened — \
         the probe is not testing what it claims to test"
    );

    // The payload must survive too. Losing it would mean the unwind happened
    // but the information about *what* failed did not, which is still a silent
    // failure mode for anyone reading CI logs.
    let payload = caught.expect_err("just asserted it is Err");
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .expect("panic payload must be a string");
    assert!(
        message.contains("unwinding is available"),
        "payload survived but lost its contents: {message}"
    );
}

/// `panic = "abort"` is only defensible while something restarts the process.
///
/// A bot supervised by systemd restarts within seconds, so a fast abort is
/// better than a slow unwind: the abort path is shorter and the supervisor
/// brings it back with a fresh, known-good address space.
///
/// The cost is that a panic anywhere loses in-flight state, including the
/// `TraceRing` contents that exist to explain *why* it crashed. That is the
/// real argument for the tracing layer being always-on and for its drain being
/// cheap to call from a signal path.
///
/// This test cannot observe a restart policy. It pins the invariant that makes
/// the policy workable instead: the tracing primitives stay `Send + Sync` and
/// keep working with no other state in the process at all.
#[test]
fn telemetry_survives_the_abort_it_is_built_to_explain() {
    use std::sync::Arc;
    use std::thread;

    let ring = Arc::new(sniper_telemetry::TraceRing::new(16));
    let producer = Arc::clone(&ring);

    let handle = thread::spawn(move || {
        for i in 0..16_u64 {
            producer.push(i);
        }
    });
    handle.join().expect("producer thread must not panic");

    // Nothing else in this test allocates or keeps state. If a TraceRing ever
    // grew a dependency on some crate-level cache, this is the first place it
    // would show up: the ring is used with the process otherwise empty.
    assert_eq!(ring.len(), 16);
    assert_eq!(ring.snapshot(), (0..16).collect::<Vec<_>>());
    assert_eq!(ring.evicted(), 0, "a fresh ring evicts nothing");
}
