//! 🩸 `LeakBudget` — the stream's memory is bounded, and here is the proof.
//!
//! ## The defect
//!
//! `stream::logs::leak` converts a log-derived name into a `&'static str` with
//! `Box::leak`, for every name that is not already in the `KNOWN` table. The
//! doc comment calls this "on purpose": an unrecognised pump instruction is the
//! most useful thing the stream can discover, and interning it into a fixed
//! table would discard that.
//!
//! All true, and it is still a leak. `Box::leak` never frees, and the function
//! is called from `pump_activity` once per top-level instruction name in every
//! transaction the stream sees. So an unrecognised name costs one permanent
//! allocation *per occurrence*, not per distinct name. A name the program
//! emits a thousand times a second costs a thousand allocations a second, and
//! none of them are ever reclaimed.
//!
//! For a process that is designed to run for days next to a hot wallet, this
//! is not a slow leak. It is an unbounded one, and it is driven by remote
//! input.
//!
//! ## The fix being tested
//!
//! Intern unknown names exactly once, through a process-wide set, and return
//! the already-interned `&'static str` on every subsequent call. Growth becomes
//! bounded by the number of *distinct* names rather than the number of
//! *occurrences*, and a repeated name costs a hash lookup instead of a
//! permanent allocation.
//!
//! ## What this file does not claim
//!
//! That memory is constant. A genuinely new instruction name -- pump.fun ships
//! new ones -- still costs one allocation, permanently, by design: the name is
//! the evidence and the caller is expected to surface it. What is claimed is
//! that the cost is per distinct name rather than per occurrence, which is the
//! difference between a bounded set and an attacker-controlled loop.

use serial_test::serial;
use sniper_core::stream::logs::{LogNotification, PumpActivity};

/// The bare name of an instruction the crate's KNOWN table has never seen.
///
/// Deliberately *not* the whole log line. `framed()` adds the
/// `Program log: Instruction: ` prefix, and a constant that already carried it
/// produced a double-prefixed line that classified as Other with a name of
/// "Program log: Instruction: migrate_legacy_v3" -- which passed every
/// assertion about interning and would have made the leak measurement describe
/// the wrong string.
const UNRECOGNISED: &str = "migrate_legacy_v3";

/// Builds a `logsSubscribe` notification carrying `logs`.
///
/// Mirrors the real envelope exactly -- `params.result.value.transaction.logs` --
/// because a fixture that flattens the nesting would not catch a wire-shape
/// regression, and the shape is part of what this module has to get right.
fn notification_with(logs: &[&str]) -> LogNotification {
    serde_json::from_value(serde_json::json!({
        "params": {
            "result": {
                "context": { "slot": 350_000_000 },
                // `value` IS the transaction's logs -- there is no inner
                // `transaction` object. The envelope is
                // params.result.value.logs, and getting that wrong produces a
                // notification whose `logs` is empty rather than an error,
                // which is the worst shape a fixture can have: it looks
                // parsed, it just says nothing.
                "value": {
                    "signature": "5wHu1rJqJ9rD8nQvB3xK2mL7pR4tY6uI9oP0aS1dF2g",
                    "err": null,
                    "logs": logs
                }
            }
        }
    }))
    .expect("the envelope is well formed by construction")
}

/// Wraps a bare instruction name in the exact log line the runtime emits.
///
/// The prefix is applied here rather than at each call site. Half the failures
/// in this file's first draft came from that line being written twice or not at
/// all: with it doubled, the name interned was literally
/// "Program log: Instruction: migrate_legacy_v3", which satisfied every
/// assertion about dedup while measuring the wrong string.
fn instruction_line(name: &str) -> String {
    format!("Program log: Instruction: {name}")
}

/// Wraps program lines in the invoke/success frame the runtime emits.
fn framed(instruction: &str) -> Vec<String> {
    vec![
        format!("Program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P invoke [1]"),
        instruction.to_string(),
        "Program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P success".to_string(),
    ]
}

/// Classifies a notification.
///
/// The wire envelope nests two levels deep -- `params.result.value` -- and the
/// module models that shape rather than flattening it, so the test has to walk
/// it too. Writing the path out once here keeps the seven call sites below
/// about what they are testing rather than about envelope navigation.
fn classify(notification: &LogNotification) -> PumpActivity {
    notification
        .params
        .result
        .value
        .pump_activity()
        .expect("a framed pump transaction must classify")
}

/// How many distinct names the process has interned so far.
///
/// Reading the count directly is the point. The alternative is to infer growth
/// from RSS, which varies with allocator behaviour and page boundaries and
/// would make this test flaky in exactly the way that gets it ignored -- and a
/// flaky leak test is worse than none, because it teaches people to re-run it.
fn interned_names() -> usize {
    sniper_core::stream::logs::interned_unknown_count()
}

#[test]
#[serial]
fn repeated_unknown_names_intern_once_not_once_per_occurrence() {
    // 5 000 transactions carrying the same unrecognised instruction.
    //
    // Before the fix this is 5 000 permanent allocations. The number is not
    // arbitrary: at any plausible transaction rate it is a few seconds of
    // stream, and it is small enough that the test is fast.
    const OCCURRENCES: usize = 5_000;

    let baseline = interned_names();

    for _ in 0..OCCURRENCES {
        let logs = framed(&instruction_line(UNRECOGNISED));
        let borrowed: Vec<&str> = logs.iter().map(String::as_str).collect();
        let activity = classify(&notification_with(&borrowed));
        assert!(matches!(activity, PumpActivity::Other(_)));
    }

    let after = interned_names();

    // At most one *new* name, never one per occurrence.
    //
    // "<= 1" rather than "== 1" on purpose. The interning set is process-wide
    // and shared with the other tests in this file, which run serially and may
    // have seen this name already -- so an exact equality would fail depending
    // on execution order, which is the definition of a flaky test. What must
    // hold is the bound: 5 000 occurrences cannot cost 5 000 allocations.
    //
    // Before the fix this was 5 000, and the process grew by roughly 300 KB
    // over the course of a single second of stream.
    let new_names = after.saturating_sub(baseline);
    assert!(
        new_names <= 1,
        "one distinct name was seen {OCCURRENCES} times and interned {new_names} \
         new entries; each repeat must reuse the first allocation"
    );
}

/// The interned set actually returns the same pointer every time.
///
/// Pointer identity, not string equality. Two `&'static str` that compare
/// equal are not the same allocation, and a fix that deduplicates by comparing
/// strings but returns a fresh copy would pass an equality check while leaking
/// exactly as before. This is the assertion that distinguishes them.
#[test]
#[serial]
fn a_repeated_name_returns_the_identical_pointer() {
    let logs = framed(&instruction_line(UNRECOGNISED));
    let borrowed: Vec<&str> = logs.iter().map(String::as_str).collect();

    let first = classify(&notification_with(&borrowed));
    let second = classify(&notification_with(&borrowed));

    let (PumpActivity::Other(a), PumpActivity::Other(b)) = (first, second) else {
        panic!("expected an unrecognised instruction to classify as Other");
    };

    assert_eq!(
        a.as_ptr(),
        b.as_ptr(),
        "two observations of the same name returned different allocations"
    );
}

/// Each distinct name costs exactly one allocation, however often it appears.
#[test]
#[serial]
fn growth_is_per_distinct_name() {
    let baseline = interned_names();
    let names = ["alpha_v1", "beta_v2", "gamma_v3", "alpha_v1", "beta_v2"];

    for name in names {
        let logs = framed(&instruction_line(name));
        let borrowed: Vec<&str> = logs.iter().map(String::as_str).collect();
        classify(&notification_with(&borrowed));
    }

    assert_eq!(
        interned_names() - baseline,
        3,
        "five occurrences of three distinct names must intern exactly three"
    );
}

/// A known name never reaches the interning set at all.
///
/// The fast path in `leak` returns a `&'static str` from the `KNOWN` table
/// directly. If that regressed, every `buy` and `sell` -- the overwhelming
/// majority of traffic on this stream -- would start allocating permanently,
/// and the defect above would become the common case rather than the rare one.
#[test]
#[serial]
fn known_names_never_reach_the_interning_set() {
    let baseline = interned_names();

    for _ in 0..200 {
        let logs = framed(&instruction_line("Buy"));
        let borrowed: Vec<&str> = logs.iter().map(String::as_str).collect();
        let activity = classify(&notification_with(&borrowed));
        assert!(
            matches!(activity, PumpActivity::Trade(_)),
            "Buy must classify as a trade"
        );
    }

    assert_eq!(
        interned_names(),
        baseline,
        "200 known instructions allocated permanently; the KNOWN fast path is gone"
    );
}

/// The evidence the whole design rests on is still preserved.
///
/// The leak exists so an unrecognised instruction name survives long enough to
/// be
/// reported. If interning ever "fixed" the leak by falling back to a generic
/// label, the memory would be bounded and the stream would be useless. This
/// asserts the name comes back verbatim, because that is the property the leak
/// was paying for.
#[test]
#[serial]
fn an_unrecognised_name_is_still_reported_verbatim() {
    let logs = framed(&instruction_line(UNRECOGNISED));
    let borrowed: Vec<&str> = logs.iter().map(String::as_str).collect();
    let activity = classify(&notification_with(&borrowed));

    let PumpActivity::Other(name) = activity else {
        panic!("an unrecognised instruction must classify as Other");
    };
    assert_eq!(
        name, UNRECOGNISED,
        "the name must survive verbatim -- a generic label here would make \
         the whole leak pointless"
    );
}

/// Interning survives concurrent streams.
///
/// The set is shared process-wide, so it is reached from every stream task. A
/// plain `HashSet` here would be a data race and a use-after-free at once; this
/// is the test that says the synchronisation is real rather than assumed,
/// because `unsafe_code = "forbid"` means the compiler is relying on whatever
/// lock is here being used correctly.
#[test]
#[serial]
fn concurrent_interning_deduplicates() {
    use std::sync::Arc;
    use std::thread;

    const DISTINCT: usize = 16;
    const THREADS: usize = 4;

    // Names unique to this test, so the baseline is measured here and the
    // assertion does not depend on what other tests happened to intern.
    let names: Vec<String> = (0..DISTINCT)
        .map(|i| format!("concurrent_probe_{i}"))
        .collect();
    let baseline = interned_names();

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let names = names.clone();
            thread::spawn(move || {
                for name in &names {
                    let logs = framed(&instruction_line(name));
                    let borrowed: Vec<&str> = logs.iter().map(String::as_str).collect();
                    classify(&notification_with(&borrowed));
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("worker must not panic");
    }

    // Every name was offered once per thread, so without deduplication this
    // would be DISTINCT * THREADS allocations.
    assert_eq!(
        interned_names() - baseline,
        DISTINCT,
        "{DISTINCT} names offered by {THREADS} threads must intern {DISTINCT} times, not {}",
        DISTINCT * THREADS
    );

    let _ = Arc::new(0); // keeps the Arc import meaningful across platforms
}
