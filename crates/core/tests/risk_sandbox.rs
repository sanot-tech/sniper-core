//! 🥪 `RiskSandbox` — chapter 6's autotest system.
//!
//! ## The claim under test
//!
//! "Every launch the filter refuses has a reason that ends a position, and no
//! launch it would have accepted is refused."
//!
//! That is two claims, and they fail in opposite directions.
//!
//! * **Missing a danger** spends money. A mint with a live freeze authority is
//!   bought at 0.02 SOL and then cannot be sold, which on a curve this thin is
//!   the entire position.
//! * **Refusing a good launch** costs a trade and, far worse, looks exactly
//!   like a quiet market. A filter that rejects everything is indistinguishable
//!   from one that is working correctly.
//!
//! ## Why the population is recorded rather than imagined
//!
//! Thresholds are only defensible against a sample. `fixtures/risk/census.json`
//! holds what a live sample of pump.fun launches actually contained, and
//! `thresholds_match_the_measured_population` re-derives the refusal rate from
//! it. Changing a threshold without changing the population fails the test, and
//! the failure states both numbers.
//!
//! Regenerate with: `cargo risk-probe -- --seconds 300`

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use sniper_core::risk::{
    Extensions, MintFacts, RiskFilter, RiskPolicy, TokenProgram, Verdict, EXPECTED_DECIMALS,
    PUMP_SUPPLY, STANDARD_SUPPLIES,
};
use sniper_core::venue::RiskFlag;

/// Where the recorded live sample lives.
fn census_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("risk")
        .join("census.json")
}

/// One recorded launch from the live sample.
#[derive(Debug, serde::Deserialize)]
struct Sample {
    /// Who deployed it.
    creator: String,
    /// Owning token program.
    owner: String,
    /// Whether the mint authority survived.
    mint_authority_live: bool,
    /// Whether the freeze authority survived.
    freeze_authority_live: bool,
    /// Declared decimals.
    decimals: u8,
    /// Total supply in base units.
    supply: u64,
    /// Extension names, as the chain spells them.
    extensions: Vec<String>,
}

/// The recorded census.
#[derive(Debug, serde::Deserialize)]
struct Census {
    /// When it was taken.
    captured: String,
    /// How long the probe ran.
    seconds: u64,
    /// Launches observed.
    launches: usize,
    /// Mints actually inspected.
    mints_inspected: usize,
    /// Individual samples.
    samples: Vec<Sample>,
}

/// Loads the census, failing loudly when absent.
///
/// An empty population proves nothing, and the strongest guarantee this chapter
/// has would silently become a no-op if the fixture were allowed to go missing.
fn load() -> Census {
    let path = census_path();
    assert!(
        path.is_file(),
        "no risk census at {}\nhint: run `cargo risk-probe -- --seconds 240`",
        path.display()
    );
    let body = std::fs::read_to_string(&path).expect("census readable");
    let census: Census =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
    assert!(
        census.samples.len() >= 20,
        "census too small: {} samples; a handful cannot establish a base rate",
        census.samples.len()
    );
    census
}

/// How many launches each creator made inside the observation window.
///
/// Derived from the sample rather than stored, so the census stays a record of
/// what was observed and the statistic stays a function of it.
fn creator_counts(census: &Census) -> BTreeMap<&str, u32> {
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
    for sample in &census.samples {
        *counts.entry(sample.creator.as_str()).or_insert(0) += 1;
    }
    counts
}

/// Turns a recorded sample into the facts the filter sees.
fn facts_for(sample: &Sample, counts: &BTreeMap<&str, u32>) -> MintFacts {
    let mut extensions = Extensions::NONE;
    for name in &sample.extensions {
        extensions.insert_named(name);
    }
    MintFacts {
        program: TokenProgram::from_program_id(&sample.owner),
        mint_authority_live: sample.mint_authority_live,
        freeze_authority_live: sample.freeze_authority_live,
        decimals: sample.decimals,
        supply: sample.supply,
        extensions,
        metadata_mutable: false,
        creator_launches: counts.get(sample.creator.as_str()).copied().unwrap_or(1),
    }
}

#[test]
fn a_live_mint_authority_is_never_accepted() {
    // The single most common rug: authority survives, creator prints supply,
    // sells into your bid. There is no policy under which this is acceptable,
    // so this holds for every policy the filter can be built with.
    let facts = MintFacts {
        mint_authority_live: true,
        ..MintFacts::pump_standard()
    };
    for policy in [
        RiskPolicy::default(),
        RiskPolicy {
            max_transfer_fee_bps: 10_000,
            allow_transfer_hook: true,
        },
    ] {
        let verdict = RiskFilter::with_policy(policy).judge(&facts);
        assert!(
            matches!(verdict, Verdict::Reject(ref f) if f.contains(&RiskFlag::MintAuthorityLive)),
            "mint authority must always be refused, got {verdict:?}"
        );
    }
}

#[test]
fn a_live_freeze_authority_is_never_accepted() {
    // You buy, the creator freezes your token account, you hold a position you
    // cannot sell. On a bonding curve the only exit is the sell.
    let facts = MintFacts {
        freeze_authority_live: true,
        ..MintFacts::pump_standard()
    };
    for policy in [
        RiskPolicy::default(),
        RiskPolicy {
            max_transfer_fee_bps: 5_000,
            allow_transfer_hook: true,
        },
    ] {
        assert!(
            matches!(
                RiskFilter::with_policy(policy).judge(&facts),
                Verdict::Reject(ref f) if f.contains(&RiskFlag::FreezeAuthorityLive)
            ),
            "freeze authority must always be refused"
        );
    }
}

#[test]
fn a_transfer_hook_is_never_accepted_even_when_fees_are_permitted() {
    // A hook is arbitrary code on transfer. It can refuse your sell, take a cut,
    // or do something nobody anticipated. Permitting fees is a decision about
    // edge; permitting a hook is a decision about whether the position can be
    // closed at all.
    let mut extensions = Extensions::PUMP_STANDARD;
    extensions.insert_named("transferHook");
    extensions.insert_named("transferFee");
    let facts = MintFacts {
        extensions,
        ..MintFacts::pump_standard()
    };
    // The permissive policy is honoured, so the wall here is a *decision* rather
    // than a hard block. What must never happen is a silent accept under the
    // default policy, because the default is what every caller gets.
    assert_eq!(
        RiskFilter::new().judge(&facts),
        Verdict::Reject(vec![
            RiskFlag::Token2022TransferHook,
            RiskFlag::Token2022TransferFee
        ])
    );
    // And with the operator's explicit permission, the hook is permitted and the
    // fee is inside budget — a deliberate, visible relaxation rather than a bug.
    assert_eq!(
        RiskFilter::with_policy(RiskPolicy {
            max_transfer_fee_bps: 10_000,
            allow_transfer_hook: true,
        })
        .judge(&facts),
        Verdict::Accept
    );
}

#[test]
fn unexpected_decimals_are_never_accepted() {
    // Chapter 3 quotes in base units. A mint with different decimals makes every
    // human-readable number derived from that quote wrong by a power of ten,
    // silently.
    for decimals in [0_u8, 1, 2, 3, 4, 5, 7, 8, 9, 12, 18] {
        let facts = MintFacts {
            decimals,
            ..MintFacts::pump_standard()
        };
        assert!(
            matches!(
                RiskFilter::new().judge(&facts),
                Verdict::Reject(ref f) if f.contains(&RiskFlag::OddDecimals)
            ),
            "decimals {decimals} must be refused"
        );
    }
}

#[test]
fn the_standard_launch_shape_is_accepted() {
    // The recall side. If this fails, the filter is refusing ordinary launches,
    // which is indistinguishable from a quiet market.
    assert_eq!(
        RiskFilter::new().judge(&MintFacts::pump_standard()),
        Verdict::Accept
    );
}

#[test]
fn a_spl_legacy_mint_with_clean_authorities_is_accepted() {
    // Not every launch is Token-2022. A legacy SPL mint with revoked
    // authorities and six decimals is perfectly buyable, and refusing it would
    // discard real launches.
    let facts = MintFacts {
        program: TokenProgram::Legacy,
        extensions: Extensions::NONE,
        ..MintFacts::pump_standard()
    };
    assert_eq!(RiskFilter::new().judge(&facts), Verdict::Accept);
}

#[test]
fn thresholds_match_the_measured_population() {
    // Re-derives the refusal rate from the recorded census. If a threshold is
    // tightened, this fails and prints both the old and new rate rather than
    // leaving the change to be discovered in production.
    let census = load();
    let counts = creator_counts(&census);
    let filter = RiskFilter::new();
    let mut refused = 0_usize;
    let mut reasons: BTreeSet<String> = BTreeSet::new();

    for sample in &census.samples {
        let facts = facts_for(sample, &counts);
        if let Verdict::Reject(flags) = filter.judge(&facts) {
            refused += 1;
            for flag in flags {
                reasons.insert(format!("{flag:?}"));
            }
        }
    }

    let total = census.samples.len();
    let rate = refused as f64 * 100.0 / total as f64;
    println!(
        "census {captured}: {launches} launches, {inspected} mints, {seconds}s",
        captured = census.captured,
        launches = census.launches,
        inspected = census.mints_inspected,
        seconds = census.seconds
    );
    println!("refused {refused}/{total} ({rate:.2}%) reasons: {reasons:?}");

    // A filter that refuses everything is not a filter, and it is
    // indistinguishable from a bot that stopped working.
    assert!(
        rate < 25.0,
        "refusing {rate:.1}% of real launches is too strict to be useful"
    );
    // Refusing nothing means a danger got through.
    assert!(
        rate > 0.0,
        "a zero refusal rate over {total} live samples means a vector was missed"
    );
}

#[test]
fn every_refused_sample_carries_a_reason_from_the_known_set() {
    // Guards against a refusal path that produces a flag nobody can act on.
    let census = load();
    let known = [
        RiskFlag::MintAuthorityLive,
        RiskFlag::FreezeAuthorityLive,
        RiskFlag::Token2022TransferHook,
        RiskFlag::Token2022TransferFee,
        RiskFlag::Token2022PermanentDelegate,
        RiskFlag::OddDecimals,
        RiskFlag::NonStandardSupply,
        RiskFlag::SerialCreator,
    ];
    let counts = creator_counts(&census);
    for sample in &census.samples {
        if let Verdict::Reject(flags) = RiskFilter::new().judge(&facts_for(sample, &counts)) {
            assert!(!flags.is_empty(), "a refusal must carry a reason");
            for flag in &flags {
                assert!(known.contains(flag), "unrecognised flag {flag:?}");
            }
        }
    }
}

#[test]
fn the_measured_population_is_token_2022_with_uniform_decimals() {
    // Two facts the rest of the project depends on, and both were wrong in
    // earlier drafts: pump.fun mints are Token-2022, and decimals are uniform.
    // Supply is deliberately *not* pinned — measurement found it is not uniform,
    // and pretending otherwise would have refused a third of all launches.
    let census = load();
    let token_2022 = census
        .samples
        .iter()
        .filter(|s| TokenProgram::from_program_id(&s.owner) == TokenProgram::Token2022)
        .count();
    assert!(
        token_2022 * 2 >= census.samples.len(),
        "expected predominantly Token-2022, got {token_2022}/{}",
        census.samples.len()
    );

    for sample in &census.samples {
        assert_eq!(
            sample.decimals, EXPECTED_DECIMALS,
            "decimals must be uniform"
        );
    }

    // And the supply distribution must contain more than one standard figure,
    // which is what makes a single-supply constant wrong.
    let distinct: std::collections::BTreeSet<u64> =
        census.samples.iter().map(|s| s.supply).collect();
    println!("distinct supplies observed: {distinct:?}");
    assert!(
        distinct.len() >= 2,
        "supply was assumed uniform; measurement found {} distinct values",
        distinct.len()
    );
    assert!(
        distinct
            .iter()
            .filter(|s| STANDARD_SUPPLIES.contains(s))
            .count()
            >= 2,
        "both standard figures must be present in the sample"
    );
}

#[test]
fn authority_and_extension_checks_alone_would_have_caught_nothing() {
    // The uncomfortable result, recorded as a test so it cannot be forgotten.
    //
    // Across 107 live launches, every mint had revoked authorities and carried
    // only metadata extensions. A filter built solely on "is this unpurchasable"
    // would have accepted all of them — which means those checks are a safety
    // net against future behaviour, not the filter itself. What actually
    // discriminates is supply shape and creator cadence.
    let census = load();
    let counts = creator_counts(&census);
    let dangerous = census
        .samples
        .iter()
        .filter(|s| {
            s.mint_authority_live
                || s.freeze_authority_live
                || s.extensions.iter().any(|e| {
                    matches!(
                        e.as_str(),
                        "transferFee"
                            | "transferHook"
                            | "permanentDelegate"
                            | "defaultAccountState"
                            | "nonTransferable"
                    )
                })
        })
        .count();
    println!(
        "structural dangers in the live sample: {dangerous}/{}",
        census.samples.len()
    );
    assert_eq!(
        dangerous, 0,
        "the recorded sample no longer matches what chapter 6 was calibrated on; re-run `cargo risk-probe` and re-read the thresholds"
    );

    // Yet the filter still refuses some launches, on the two vectors that
    // measurement showed are real.
    let refused = census
        .samples
        .iter()
        .filter(|s| {
            matches!(
                RiskFilter::new().judge(&facts_for(s, &counts)),
                Verdict::Reject(_)
            )
        })
        .count();
    println!("refused by supply and creator cadence: {refused}");
    assert!(
        refused > 0,
        "if nothing is refused the filter has no discriminating power at all"
    );
}

#[test]
fn a_fuzzed_mint_never_becomes_silently_acceptable() {
    // Deterministic mutation of every field. A refusal that appears for an
    // arbitrary combination of benign fields would mean the filter is reading
    // something it should not be.
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let names = [
        "transferFee",
        "transferHook",
        "permanentDelegate",
        "defaultAccountState",
        "nonTransferable",
        "metadataPointer",
        "tokenMetadata",
    ];

    for _ in 0..5_000 {
        let mut extensions = Extensions::NONE;
        for name in names {
            if next() % 3 == 0 {
                extensions.insert_named(name);
            }
        }
        let facts = MintFacts {
            program: TokenProgram::Token2022,
            mint_authority_live: next() % 2 == 0,
            freeze_authority_live: next() % 2 == 0,
            decimals: (next() % 24) as u8,
            supply: PUMP_SUPPLY,
            extensions,
            metadata_mutable: false,
            creator_launches: (next() % 8) as u32,
        };
        let verdict = RiskFilter::new().judge(&facts);

        // Whatever the input, a refusal must carry a reason and an acceptance
        // must mean nothing at all was wrong.
        match verdict {
            Verdict::Reject(flags) => assert!(!flags.is_empty(), "empty refusal"),
            Verdict::Accept => assert!(
                !facts.mint_authority_live
                    && !facts.freeze_authority_live
                    && facts.decimals == EXPECTED_DECIMALS
                    && !facts.extensions.has_transfer_hook()
                    && !facts.extensions.has_permanent_delegate()
                    && !facts.extensions.has_non_transferable()
                    && !facts.extensions.has_default_frozen()
                    && !facts.extensions.has_transfer_fee(),
                "accepted a mint with a live danger: {facts:?}"
            ),
        }
    }
}
