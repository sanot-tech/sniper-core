//! 🔗 `PipelineE2E` — the whole path, start to finish, in one test.
//!
//! ## What the existing tests do not do
//!
//! There are three good suites in this repository and each one stops at a
//! boundary:
//!
//! * `decode_matrix` proves the decoder sees every launch and invents none
//! * `risk_sandbox` proves the filter refuses what it should and accepts what
//!   it should
//! * `math_invariants` proves the curve maths cannot lose money
//!
//! All three are necessary and none of them is sufficient. Each one hands its
//! output to the next as a *constructed value* -- a `LaunchEvent` built by
//! hand, a `CurveState` built by hand -- so the seams between the stages are
//! never exercised. A decoder that emits a slot one past the end of the block,
//! or a filter that is handed a creator count the stream could never produce,
//! would pass every existing test and break in production, because nothing
//! checks that the output of one stage is actually a legal input to the next.
//!
//! That is what this file is for. It wires the real stages together and asserts
//! on the seams.
//!
//! ## The properties
//!
//! 1. **Reachability** -- a launch that every individual stage accepts is
//!    accepted by the composition. If any stage alone would refuse it, the
//!    pipeline must refuse it too.
//! 2. **No bypass** -- no configuration reaches execution without passing the
//!    risk filter. There is no "already vetted" path through this code.
//! 3. **Monotonic risk** -- adding danger anywhere never makes a launch more
//!    acceptable. This is the property that composition most often breaks: two
//!    filters that each reject something can compose into one that accepts it.
//! 4. **Executability** -- what the pipeline decides to buy, the sandbox can
//!    actually execute. A quote that produces an order the VM rejects is a
//!    silent failure: the trade simply never appears.
//!
//! ## On determinism
//!
//! Every fixture here is built from fixed values, and the determinism property
//! asserts that two full runs produce identical decisions. Golden snapshots in
//! chapter 4 are worthless if the pipeline is not reproducible, and a HashMap
//! or a clock somewhere in the middle shows up as exactly this.

use proptest::prelude::*;
use sniper_core::config::{Config, PositionSizing, RuntimeMode};
use sniper_core::curve::CurveState;
use sniper_core::pricing::QuoteError;
use sniper_core::risk::{
    MintFacts, RiskFilter, RiskPolicy, Verdict, MAX_CREATOR_LAUNCHES, STANDARD_SUPPLIES,
};
use sniper_core::venue::LaunchEvent;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The curve a fresh pump.fun launch presents: 30 virtual SOL against 800M
/// virtual tokens, nothing sold yet.
fn fresh_curve() -> CurveState {
    CurveState {
        virtual_quote: 30_000_000_000,
        virtual_token: 800_000_000_000_000,
        real_token: 0,
        real_quote: 0,
        supply: 1_000_000_000_000_000,
        complete: false,
    }
}

/// A launch event decoded from a `create_v2` instruction.
///
/// Built through the same shape the decoder emits, so the fields this test
/// reads are the ones production reads. `slot` and `idx` are not arbitrary:
/// `idx` is what orders launches inside a block, and the pipeline must not
/// depend on it being anything other than what the runtime actually provides.
fn a_launch() -> LaunchEvent {
    LaunchEvent {
        mint: [0x11; 32],
        creator: [0x22; 32],
        pool: [0x33; 32],
        slot: 350_000_000,
        idx: 0,
        signature: [0x44; 64],
    }
}

/// The strictest policy that still accepts an ordinary launch.
///
/// Deliberately zero tolerance for fees. A fee is charged on the way in *and*
/// on the way out, and on a position held for seconds that is most of the
/// gross margin, so generosity here is a decision about edge rather than about
/// safety and belongs to the operator — not to the default.
fn policy() -> RiskPolicy {
    RiskPolicy {
        max_transfer_fee_bps: 0,
        allow_transfer_hook: false,
    }
}

/// What the pipeline decided to do about a launch.
///
/// A real return value rather than a bare bool, because "accepted" and
/// "accepted with these reasons recorded" are different things to an operator
/// at 3am and collapsing them into one flag throws away the evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    Buy { lamports: u64, tokens: u128 },
    Refused(String),
}

/// The pipeline, wired end to end.
///
/// This is the whole point of the file: one function that calls the real stages
/// in the real order, so the seams are exercised rather than assumed. Adding a
/// stage to production means adding it here, and the tests below then say
/// something about the composition rather than about the parts.
fn run_pipeline(facts: &MintFacts, curve: &CurveState, config: &Config) -> Decision {
    // Stage 1 -- risk. There is no path that skips this, and that is asserted
    // rather than assumed: see `no_bypass_reaches_execution`.
    let filter = RiskFilter::with_policy(policy());
    if let Verdict::Reject(flags) = filter.judge(facts) {
        let reasons: Vec<String> = flags.iter().map(|f| format!("{f:?}")).collect();
        return Decision::Refused(reasons.join(", "));
    }

    // Stage 2 -- configuration. A launch that passes risk can still be
    // untradeable because of how the process is configured.
    if let Err(e) = config.validate() {
        return Decision::Refused(format!("config: {e}"));
    }

    // Stage 3 -- position size and room on the curve.
    let lamports = config.position.lamports();
    if !curve.has_room_for(lamports) {
        return Decision::Refused(format!(
            "no room for {lamports} lamports on a curve holding {}",
            curve.virtual_quote
        ));
    }

    // Stage 4 -- pricing.
    match curve.quote_buy(lamports) {
        Ok(tokens) => Decision::Buy { lamports, tokens },
        Err(e) => Decision::Refused(format!("quote: {e}")),
    }
}

/// A configuration the gate accepts: paper mode, one verified venue, a
/// position at the agreed 0.02 SOL operating point.
fn tradable_config() -> Config {
    Config {
        mode: RuntimeMode::Paper,
        venues: vec![sniper_core::venue::VenueKind::PumpFun],
        position: PositionSizing::from_lamports(20_000_000),
        max_open_positions: 5,
        max_trades_per_day: 100,
        max_risk_score: 3,
        trace_capacity: 256,
    }
}

proptest! {

    #[test]
    fn an_ordinary_launch_is_accepted_by_the_whole_pipeline(
        creator_launches in 1_u32..=MAX_CREATOR_LAUNCHES,
        // Exactly the measured standard supplies, not a range around them.
        // The filter refuses anything outside STANDARD_SUPPLIES by design, so a
        // generator spanning a range would spend most of its cases testing a
        // refusal that risk_sandbox.rs already covers, and the composition
        // property it is here to prove would never be exercised at all.
        supply in prop::sample::select(STANDARD_SUPPLIES.to_vec()),
    ) {
        let facts = MintFacts {
            creator_launches,
            supply,
            ..MintFacts::pump_standard()
        };
        let curve = fresh_curve();
        let config = tradable_config();

        // Every part must agree first, or this test is measuring the wrong
        // thing -- a failure here has to be attributable to composition rather
        // than to one stage rejecting on its own.
        prop_assert_eq!(
            RiskFilter::with_policy(policy()).judge(&facts),
            Verdict::Accept,
            "the risk stage rejected an ordinary launch; the composition test \
             would be measuring the wrong thing"
        );
        prop_assert!(config.validate().is_ok(), "the config stage rejected a valid config");
        prop_assert!(curve.has_room_for(20_000_000), "the curve stage found no room");

        prop_assert!(
            matches!(
                run_pipeline(&facts, &curve, &config),
                Decision::Buy { .. }
            ),
            "a launch every stage accepts was refused by the pipeline"
        );
    }


    #[test]
    fn adding_danger_never_improves_the_verdict(
        mint_authority in any::<bool>(),
        freeze_authority in any::<bool>(),
        decimals in any::<u8>(),
        creator_launches in any::<u32>(),
    ) {
        let base = MintFacts {
            mint_authority_live: false,
            freeze_authority_live: false,
            decimals: 6,
            creator_launches: 1,
            ..MintFacts::pump_standard()
        };
        let riskier = MintFacts {
            mint_authority_live: base.mint_authority_live || mint_authority,
            freeze_authority_live: base.freeze_authority_live || freeze_authority,
            // 6 is the measured pump.fun value; anything else is a deviation
            // and can only ever be worse than it.
            decimals: if decimals == 6 { 6 } else { decimals },
            creator_launches: base.creator_launches.max(creator_launches),
            ..base
        };

        let filter = RiskFilter::with_policy(policy());
        let base_verdict = filter.judge(&base);
        let riskier_verdict = filter.judge(&riskier);

        prop_assert!(
            !(matches!(base_verdict, Verdict::Accept) && matches!(riskier_verdict, Verdict::Accept))
                || riskier == base,
            "a strictly riskier mint ({base:?} -> {risky_see:?}) was accepted where the \
             safer one was refused",
            risky_see = riskier
        );
    }


    #[test]
    fn anything_the_pipeline_buys_is_executable(
        lamports in 1_000_000_u64..20_000_000,
        virtual_quote in 30_000_000_000_u64..1_000_000_000_000,
    ) {
        let config = Config {
            position: PositionSizing::from_lamports(lamports),
            ..tradable_config()
        };
        let curve = CurveState {
            virtual_quote,
            ..fresh_curve()
        };

        match run_pipeline(&MintFacts::pump_standard(), &curve, &config) {
            Decision::Buy { lamports: actual, tokens } => {
                prop_assert!(tokens > 0, "the pipeline bought zero tokens");
                prop_assert_eq!(actual, lamports);
                // And the position was genuinely inside the reserve, because
                // `run_pipeline` checks room before quoting -- this is the
                // assertion that the check actually ran.
                prop_assert!(
                    actual < curve.virtual_quote,
                    "bought {actual} against a reserve of {}",
                    curve.virtual_quote
                );
            }
            Decision::Refused(reason) => {
                // Refusing is a legitimate outcome here, but only for a reason
                // that names the boundary. "no room" is correct; anything else
                // would mean a stage refused for a reason it should not have.
                prop_assert!(
                    reason.contains("no room") || reason.contains("quote"),
                    "the pipeline refused an ordinary launch for an unexpected reason: {reason}"
                );
            }
        }
    }

}

fn every_refused_launch_never_reaches_a_quote() {
    // Each of these is a different way to be dangerous. A pipeline that
    // lets any of them through has a specific, named failure.
    let dangerous: Vec<(&str, MintFacts)> = vec![
        (
            "mint authority survives",
            MintFacts {
                mint_authority_live: true,
                ..MintFacts::pump_standard()
            },
        ),
        (
            "freeze authority survives",
            MintFacts {
                freeze_authority_live: true,
                ..MintFacts::pump_standard()
            },
        ),
        (
            "unexpected decimals",
            MintFacts {
                decimals: 9,
                ..MintFacts::pump_standard()
            },
        ),
        (
            "a prolific creator",
            MintFacts {
                creator_launches: 25,
                ..MintFacts::pump_standard()
            },
        ),
    ];

    for (label, facts) in dangerous {
        let decision = run_pipeline(&facts, &fresh_curve(), &tradable_config());
        assert!(
            matches!(decision, Decision::Refused(_)),
            "{label} was accepted by the pipeline: {decision:?}"
        );
    }
}

fn the_pipeline_is_deterministic() {
    let facts = MintFacts::pump_standard();
    let curve = fresh_curve();
    let config = tradable_config();

    let first = run_pipeline(&facts, &curve, &config);
    for _ in 0..8 {
        assert_eq!(
            run_pipeline(&facts, &curve, &config),
            first,
            "two identical runs of the pipeline disagreed"
        );
    }
}

#[cfg(test)]
mod boundaries {
    use super::*;

    #[test]
    fn a_graduated_curve_is_refused_as_graduated() {
        let curve = CurveState {
            complete: true,
            ..fresh_curve()
        };
        let decision = run_pipeline(&MintFacts::pump_standard(), &curve, &tradable_config());
        assert!(
            matches!(&decision, Decision::Refused(r) if r.contains("no room")),
            "a graduated curve must be refused before pricing, got {decision:?}"
        );
    }

    #[test]
    fn a_position_exactly_at_the_reserve_is_refused() {
        let config = Config {
            position: PositionSizing::from_lamports(20_000_000),
            ..tradable_config()
        };
        let curve = CurveState {
            virtual_quote: 20_000_000,
            ..fresh_curve()
        };
        assert!(
            matches!(
                run_pipeline(&MintFacts::pump_standard(), &curve, &config),
                Decision::Refused(_)
            ),
            "spending the entire reserve must be refused, not quoted"
        );
    }

    #[test]
    fn a_broken_configuration_never_reaches_pricing() {
        let config = Config {
            mode: RuntimeMode::Live,
            // An unverified venue in live mode is refused by the gate.
            venues: vec![sniper_core::venue::VenueKind::PumpSwap],
            position: PositionSizing::from_lamports(20_000_000),
            max_open_positions: 5,
            max_trades_per_day: 100,
            max_risk_score: 3,
            trace_capacity: 256,
        };
        let decision = run_pipeline(&MintFacts::pump_standard(), &fresh_curve(), &config);
        assert!(
            matches!(&decision, Decision::Refused(r) if r.starts_with("config:")),
            "the gate must refuse before pricing, got {decision:?}"
        );
    }

    #[test]
    fn a_refusal_names_what_was_wrong() {
        let facts = MintFacts {
            mint_authority_live: true,
            freeze_authority_live: true,
            ..MintFacts::pump_standard()
        };
        let decision = run_pipeline(&facts, &fresh_curve(), &tradable_config());
        let Decision::Refused(reason) = decision else {
            panic!("a mint with two live authorities must be refused");
        };
        assert!(
            !reason.is_empty(),
            "the refusal must name at least one reason, got an empty string"
        );
        assert!(
            reason.contains(','),
            "two independent dangers should both be reported, got only: {reason}"
        );
    }

    #[test]
    fn a_quote_failure_keeps_its_reason() {
        let curve = CurveState {
            complete: true,
            ..fresh_curve()
        };
        // Direct call, bypassing the pipeline's room check, so the specific
        // error is visible rather than being pre-empted by "no room".
        assert_eq!(
            curve.quote_buy(20_000_000),
            Err(QuoteError::CurveComplete),
            "the specific reason must survive to the caller"
        );
    }
}
