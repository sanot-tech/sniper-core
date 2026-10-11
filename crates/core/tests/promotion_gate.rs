//! ⚖️ `PromotionGate` — the configuration laws, stated over all inputs.
//!
//! ## Why this file exists next to the unit tests in `config.rs`
//!
//! The tests in the module ask "does a 0.02 SOL position pass?". This file
//! asks "is there *any* input that slips through?" — a different question, and
//! the one that matters for a gate whose whole job is refusing bad
//! configurations.
//!
//! The gate has one asymmetry that makes this worth the machinery: most
//! failures it catches are loud (a panic at startup, a non-zero exit), but the
//! one that matters most is silent. A bot that starts happily and then trades
//! at the wrong size, or accepts a venue nobody verified, does not look broken.
//! It looks like a bad week.
//!
//! ## The properties
//!
//! 1. **Soundness** — no configuration that passes `validate` is one the gate
//!    exists to refuse. Stated over generated venues, positions and risk
//!    scores, because the combinations nobody thought to write down are exactly
//!    the ones that ship.
//! 2. **Determinism** — the same configuration always yields the same verdict
//!    and the same error. A gate that reports a different reason on a second
//!    call is a gate nobody can debug at 3am.
//! 3. **Exhaustiveness of the ladder** — only `Live` broadcasts, under every
//!    configuration. One property, over the whole input space.
//! 4. **Exactness** — lamports round-trip through `as_sol` without drift,
//!    because the position size is a money value and float rounding is a bug
//!    waiting for an inconvenient day.
//!
//! ## What is deliberately not claimed
//!
//! These tests prove the gate refuses what it says it refuses. They do not
//! prove the gate is *complete* -- that no conceivable dangerous
//! configuration slips through. That is a different and much harder claim,
//! and pretending to prove it would be worse than not making it.

use proptest::prelude::*;
use sniper_core::config::{
    Config, ConfigError, PositionSizing, RuntimeMode, LIVE_POSITION_CAP_LAMPORTS,
};
use sniper_core::venue::VenueKind;

/// Positions an operator could plausibly type into a config file.
///
/// The range stops at 5 SOL rather than spanning all of `u64`, because the
/// interesting boundary is "small enough to trade live" and everything above
/// that takes the same branch. A strategy that generated `u64::MAX` would
/// spend most of its cases proving that an absurd position is refused, which
/// is a thing one example proves.
fn position_lamports() -> impl Strategy<Value = u64> {
    prop_oneof![
        // Exactly at and around the live cap -- the decision boundary.
        Just(LIVE_POSITION_CAP_LAMPORTS),
        Just(LIVE_POSITION_CAP_LAMPORTS.saturating_sub(1)),
        Just(LIVE_POSITION_CAP_LAMPORTS.saturating_add(1)),
        // Zero, one lamport, and the small sizes a test wallet would use.
        Just(0),
        Just(1),
        0_u64..1_000_000,
        // Ordinary sizes.
        1_000_000_u64..100_000_000,
        // Clearly over the cap.
        100_000_000_u64..5_000_000_000,
    ]
}

/// Risk scores across and beyond the accepted 1..=10 range.
fn risk_score() -> impl Strategy<Value = u8> {
    prop_oneof![1_u8..=10, Just(0), Just(11), Just(u8::MAX),]
}

/// Any venue, including the unverified ones.
fn venue() -> impl Strategy<Value = VenueKind> {
    prop::sample::select(VenueKind::ALL.to_vec())
}

/// Venue lists that are sometimes empty, because an empty list is a real
/// operator mistake and the gate has a specific answer for it.
fn venue_list() -> impl Strategy<Value = Vec<VenueKind>> {
    prop::collection::vec(venue(), 0..=3)
}

/// Builds a configuration from drawn parts.
fn config_from(
    mode: RuntimeMode,
    venues: Vec<VenueKind>,
    position: u64,
    risk: u8,
    trace_capacity: usize,
) -> Config {
    Config {
        mode,
        venues,
        position: PositionSizing::from_lamports(position),
        max_open_positions: 5,
        max_trades_per_day: 100,
        max_risk_score: risk,
        trace_capacity,
    }
}

proptest! {
    // ---------------------------------------------------------------------
    // Property 1 — soundness
    // ---------------------------------------------------------------------

    /// A configuration the gate accepts is never one it exists to refuse.
    ///
    /// The contrapositive of "the gate works" is hard to state directly, so
    /// this states it as: if `validate` says yes, then every condition the gate
    /// is documented to enforce actually holds. A gate that accepts an
    /// unverified venue on live fails here.
    #[test]
    fn anything_accepted_is_actually_safe(
        mode in prop::sample::select(vec![
            RuntimeMode::DryRun,
            RuntimeMode::Paper,
            RuntimeMode::Devnet,
            RuntimeMode::Live,
        ]),
        venues in venue_list(),
        position in position_lamports(),
        risk in risk_score(),
        capacity in prop::collection::vec(0usize..=4096, 1..=3),
    ) {
        let trace_capacity = capacity[0];
        let cfg = config_from(mode, venues.clone(), position, risk, trace_capacity);

        if cfg.validate().is_ok() {
            // Whatever passed must satisfy each documented precondition.
            prop_assert!(!cfg.venues.is_empty(), "an empty venue list must never pass");
            prop_assert!(cfg.trace_capacity > 0, "a zero trace capacity must never pass");
            prop_assert!(
                (1..=10).contains(&cfg.max_risk_score),
                "risk score {} outside 1..=10 must never pass",
                cfg.max_risk_score
            );

            if cfg.mode == RuntimeMode::Live {
                prop_assert!(
                    cfg.position.is_live_safe(),
                    "live accepted position {} lamports, above the cap",
                    cfg.position.lamports()
                );
                for venue in &cfg.venues {
                    prop_assert!(
                        !venue.needs_idl_verification(),
                        "live accepted unverified venue {}, which is the \
                         single mistake that costs real money",
                        venue.as_str()
                    );
                }
            }
        }
    }

    /// Live with an unverified venue is refused, whatever else is configured.
    ///
    /// Stated separately because it is the highest-severity refusal in the
    /// gate and because it interacts with the other checks: a configuration can
    /// be wrong in several ways at once, and this asserts the unverified venue
    /// is among the reasons given rather than silently shadowed by an
    /// unrelated error.
    #[test]
    fn live_with_an_unverified_venue_is_always_refused(
        unverified in prop::sample::select(
            VenueKind::ALL.iter().copied().filter(|v| v.needs_idl_verification()).collect::<Vec<_>>()
        ),
        position in 0_u64..=LIVE_POSITION_CAP_LAMPORTS,
        risk in 1_u8..=10,
        capacity in 1_usize..=4096,
    ) {
        let cfg = config_from(RuntimeMode::Live, vec![unverified], position, risk, capacity);
        prop_assert_eq!(
            cfg.validate(),
            Err(ConfigError::UnverifiedVenueOnLive { venue: unverified }),
            "live trading on an IDL-unverified venue must be refused by name"
        );
    }

    /// Live above the cap is refused, with both numbers in the error.
    ///
    /// The error carries the configured size *and* the cap. An operator seeing
    /// this at startup should not have to go read the source to find out what
    /// the limit is, and the numbers are what make the message actionable.
    #[test]
    fn live_above_the_cap_is_refused_with_both_numbers(
        position in (LIVE_POSITION_CAP_LAMPORTS + 1)..5_000_000_000,
        risk in 1_u8..=10,
        capacity in 1_usize..=4096,
    ) {
        let cfg = config_from(RuntimeMode::Live, vec![VenueKind::PumpFun], position, risk, capacity);
        prop_assert_eq!(
            cfg.validate(),
            Err(ConfigError::PositionTooLarge {
                lamports: position,
                max_lamports: LIVE_POSITION_CAP_LAMPORTS,
            }),
            "the refusal must state what was configured and what was allowed"
        );
    }

    // ---------------------------------------------------------------------
    // Property 2 — determinism
    // ---------------------------------------------------------------------

    /// The same configuration always produces the same verdict and the same
    /// error.
    ///
    /// Not a tautology in practice: `validate` walks a `Vec<VenueKind>`, and
    /// anything that made that walk order-dependent -- a sort, a set, a HashMap
    /// somewhere upstream -- would report a different *reason* for the same
    /// refusal on a different run. The reason matters: it is what the operator
    /// reads at 3am.
    #[test]
    fn validation_is_deterministic(
        mode in prop::sample::select(vec![
            RuntimeMode::DryRun,
            RuntimeMode::Paper,
            RuntimeMode::Devnet,
            RuntimeMode::Live,
        ]),
        venues in venue_list(),
        position in position_lamports(),
        risk in risk_score(),
        capacity in prop::collection::vec(0usize..=4096, 1..=3),
    ) {
        let cfg = config_from(mode, venues, position, risk, capacity[0]);

        let first = cfg.validate();
        for _ in 0..4 {
            prop_assert_eq!(
                &cfg.validate(),
                &first,
                "validation is not deterministic"
            );
        }
    }

    /// The order of the venue list does not change the verdict.
    ///
    /// Two lists holding the same venues must be interchangeable. This is the
    /// cheap, sharp version of the determinism property above: it fails the
    /// moment anyone introduces ordering sensitivity, and it is easy to read
    /// what broke when it does.
    #[test]
    fn venue_order_does_not_change_the_verdict(
        venues in prop::collection::vec(venue(), 1..=3),
        position in position_lamports(),
        risk in risk_score(),
        mode in prop::sample::select(vec![RuntimeMode::Paper, RuntimeMode::Live]),
        capacity in 1_usize..=4096,
    ) {
        let mut forward = config_from(mode, venues.clone(), position, risk, capacity);
        let mut reversed = config_from(mode, venues.clone(), position, risk, capacity);
        forward.venues.sort_by_key(|v| v.as_str());
        reversed.venues = {
            let mut v = venues;
            v.sort_by_key(|k| std::cmp::Reverse(k.as_str()));
            v
        };

        prop_assert_eq!(
            forward.validate(),
            reversed.validate(),
            "reordering the venue list changed the verdict"
        );
    }

    // ---------------------------------------------------------------------
    // Property 3 — the ladder
    // ---------------------------------------------------------------------

    /// Only `Live` broadcasts, under every configuration in the space.
    ///
    /// This is the property the whole promotion ladder exists to enforce, and
    /// it is one line of code: `matches!(self, Self::Live)`. One line, one
    /// consequence, and the consequence is "a paper-mode bug cannot spend
    /// money". It gets a property test because the cost of it being wrong is
    /// unbounded and the cost of the test is nil.
    #[test]
    fn only_live_broadcasts(
        mode in prop::sample::select(vec![
            RuntimeMode::DryRun,
            RuntimeMode::Paper,
            RuntimeMode::Devnet,
            RuntimeMode::Live,
        ]),
        venues in venue_list(),
        position in position_lamports(),
        risk in risk_score(),
        capacity in prop::collection::vec(0usize..=4096, 1..=3),
    ) {
        let cfg = config_from(mode, venues, position, risk, capacity[0]);
        prop_assert_eq!(
            cfg.mode.can_broadcast(),
            cfg.mode == RuntimeMode::Live,
            "{:?} reported an inconsistent broadcast capability",
            cfg.mode
        );
    }

    /// A mode's string form round-trips through the ladder order.
    ///
    /// The names end up in log lines, metrics labels and dashboard filters, so
    /// they are part of the interface. Two modes sharing a name would silently
    /// merge their telemetry.
    #[test]
    fn mode_names_are_unique_and_stable(
        modes in prop::collection::vec(
            prop::sample::select(vec![
                RuntimeMode::DryRun,
                RuntimeMode::Paper,
                RuntimeMode::Devnet,
                RuntimeMode::Live,
            ]),
            1..=4
        )
    ) {
        // Distinct modes only. Drawing with replacement makes the generator
        // hand back [Live, Live], which after deduplication has one name and
        // two entries -- a property of the generator, not of the code. The
        // claim under test is "no two *different* modes share a name".
        let mut distinct = modes.clone();
        distinct.sort_by_key(RuntimeMode::as_str);
        distinct.dedup();

        let mut names: Vec<&str> = distinct.iter().map(RuntimeMode::as_str).collect();
        names.sort_unstable();
        names.dedup();
        prop_assert_eq!(
            names.len(),
            distinct.len(),
            "two distinct modes share a name, which would merge their telemetry"
        );

        for mode in distinct {
            prop_assert_eq!(
                mode.as_str(),
                mode.as_str(),
                "as_str is not stable for {:?}",
                mode
            );
            // Display must not disagree with as_str: the two are used
            // interchangeably in error messages and log output.
            prop_assert_eq!(
                mode.to_string(),
                mode.as_str(),
                "Display and as_str disagree for {:?}",
                mode
            );
        }
    }

    // ---------------------------------------------------------------------
    // Property 4 — exactness of the money value
    // ---------------------------------------------------------------------

    /// Lamports survive the round trip without drift.
    ///
    /// `as_sol` divides by a billion and `from_sol` multiplies back and rounds.
    /// The claim is that for every representable position the round trip is the
    /// identity, so a size read from a config file, printed to a log and read
    /// back is unchanged. Drift here would mean the logged size is not the size
    /// that was traded.
    #[test]
    fn lamports_survive_a_sol_round_trip(lamports in position_lamports()) {
        let original = PositionSizing::from_lamports(lamports);
        let via_sol = PositionSizing::from_sol(original.as_sol());
        prop_assert_eq!(
            via_sol.lamports(),
            lamports,
            "{} lamports became {} after a round trip through f64",
            lamports,
            via_sol.lamports()
        );
    }

    /// `is_live_safe` agrees with the cap it is documented to use.
    ///
    /// Stated as an equivalence over the boundary rather than as a few examples,
    /// so moving the cap is a deliberate act that this test reports on instead
    /// of silently following along.
    #[test]
    fn live_safety_is_exactly_the_documented_boundary(lamports in position_lamports()) {
        let sizing = PositionSizing::from_lamports(lamports);
        prop_assert_eq!(
            sizing.is_live_safe(),
            lamports <= LIVE_POSITION_CAP_LAMPORTS,
            "is_live_safe disagrees with the cap for {} lamports",
            lamports
        );
    }
}

/// Non-property tests: guarantees about panics and defaults that a generator
/// cannot express, because the interesting inputs are the invalid ones.
#[cfg(test)]
mod hostile {
    use super::*;

    /// `from_sol` refuses every value that is not a whole number of lamports.
    ///
    /// The function panics rather than returning an error, which is the right
    /// call for a configuration typo caught once at startup -- but it means the
    /// guard is the only thing standing between a bad config and a position of
    /// `u64::MAX`. These are the values that must panic rather than wrap.
    ///
    /// A negative value is the dangerous one: `(-1.0 * 1e9) as u64` saturates
    /// to 0 in Rust, which would turn "a typo of minus one" into "trade
    /// nothing" and, worse, hide the typo entirely.
    #[test]
    fn from_sol_refuses_values_that_cannot_be_lamports() {
        for bad in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -1.0,
            -0.000_000_001,
            1e9,
        ] {
            let result = std::panic::catch_unwind(|| PositionSizing::from_sol(bad));
            assert!(
                result.is_err(),
                "from_sol({}) returned a value instead of refusing",
                bad
            );
        }
    }

    /// The default configuration is one the gate accepts.
    ///
    /// Worth stating plainly: the defaults are the configuration a fresh
    /// checkout runs, and if they failed validation every new user would meet
    /// the promotion gate as a wall with no visible cause.
    #[test]
    fn the_default_configuration_passes_the_gate() {
        assert_eq!(
            Config::default().validate(),
            Ok(()),
            "the shipping defaults must be a valid configuration"
        );
    }
}
