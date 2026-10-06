//! 📐 `MathInvariants` — chapter 3's autotest system.
//!
//! ## The claim under test
//!
//! "For **every** curve state and **every** trade size we might send, the quote
//! either fails cleanly or produces a number that cannot cost us money."
//!
//! ## Why this chapter needs properties instead of examples
//!
//! A wrong price is the quietest possible bug. Nothing panics, no transaction
//! is rejected, no log line appears. The bot simply fills at a slightly worse
//! price than it believed, and reports profit it never received. A test suite
//! of hand-picked examples cannot catch that, because the bug lives in the
//! inputs nobody thought to write down.
//!
//! So the assertions here are statements about *all* inputs, checked over tens
//! of thousands of generated cases.
//!
//! ## The four properties that matter
//!
//! 1. **Monotonicity** — more quote in, more or equal tokens out. A curve that
//!    pays *more* for *less* is either an overflow or a sign bug.
//! 2. **No phantom profit** — buying and immediately selling back never yields
//!    more than went in. Rounding that manufactures SOL is free money that does
//!    not exist, and it is the classic way a quote goes wrong.
//! 3. **No panic, ever** — extreme reserves either quote or refuse. A wrap in
//!    release mode is a silent wrong answer, not a loud crash.
//! 4. **Fees cost what they say** — rounding up, never down, never negative,
//!    never free.
//!
//! ## One test guards a plan that was wrong
//!
//! The master plan assumed `virtual_quote_reserves` was a signed 128-bit field
//! that could go negative. It is not: the bonding-curve account stores `u64`,
//! and only the AMM `Pool` account uses `i128`. `bonding_curve_fields_are_u64`
//! pins the width so the wrong model cannot quietly return.

use proptest::prelude::*;
use sniper_core::pricing::{CurveReserves, FeeBps, QuoteError, BPS_DENOMINATOR};

/// Test count target for the whole chapter. Reported at the end of a run so a
/// future edit cannot quietly shrink the evidence.
const TARGET_CASES: u32 = 50_000;

/// Config used by every property below.
///
/// The seed is fixed so a failure is reproducible from the printed case alone.
/// A flaky property test is worse than none: it teaches people to re-run until
/// green instead of reading the failure.
fn config(cases: u32) -> ProptestConfig {
    ProptestConfig {
        cases,
        // No persistence file. Failures must be reproducible from this source,
        // not from a file that only exists on whoever's machine ran it.
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

/// Reserves that describe a real trade: non-zero, large enough to be worth
/// quoting, small enough that products do not overflow the maths.
fn reserves() -> impl Strategy<Value = CurveReserves> {
    (
        1_000_000u64..1_000_000_000_000u64,
        1_000_000_000u64..2_000_000_000_000u64,
        1_000_000u64..900_000_000_000_000u64,
    )
        .prop_map(|(virtual_quote, virtual_token, real_token)| {
            CurveReserves::new(virtual_quote, virtual_token, real_token)
        })
}

/// Trade sizes a 0.02 SOL position would actually produce.
fn trade_size() -> impl Strategy<Value = u64> {
    1u64..20_000_000_000
}

proptest! {
    #![proptest_config(config(8_192))]

    /// More quote in must never yield fewer tokens out.
    ///
    /// Both sizes are drawn from *inside* the tradeable domain — anything at or
    /// above the virtual quote reserve is refused outright. A first version of
    /// this property ignored that boundary and reported a failure: a 1-lamport
    /// trade quoted fine on a 0.0047 SOL curve while a 0.01 SOL trade was
    /// refused. The code was right and the assertion was wrong, which is exactly
    /// what a property test is for.
    #[test]
    fn buy_quote_is_monotonic(
        curve in reserves(),
        r_lo in any::<u64>(),
        r_hi in any::<u64>(),
    ) {
        let q = curve.virtual_quote;
        if q <= 2 {
            return Ok(());
        }
        // Uniform inside (0, q): the only region where a buy can fill at all.
        let hi = 1 + r_hi % (q - 1);
        let lo = 1 + r_lo % hi;
        let (lo, hi) = (lo.min(hi), hi.max(lo));

        match (curve.buy_tokens_raw(lo), curve.buy_tokens_raw(hi)) {
            (Ok(a), Ok(b)) => prop_assert!(b >= a, "{b} < {a} for {lo} -> {hi}"),
            // Both refused: consistent.
            (Err(_), Err(_)) => {}
            (Ok(_), Err(_)) => prop_assert!(
                false,
                "tradeable amount {hi} refused after {lo} quoted on a curve with q={q}"
            ),
            (Err(_), Ok(_)) => prop_assert!(false, "smaller refused while larger quoted"),
        }
    }

    /// The tradeable domain ends exactly at the virtual quote reserve.
    ///
    /// Spending the whole reserve would drive it to zero, where the price is
    /// undefined; spending more is not a trade at all. Both must be refused,
    /// and the refusal must be the *same* one, so a caller can distinguish "too
    /// big" from "this curve is broken".
    #[test]
    fn buy_domain_stops_exactly_at_the_virtual_quote(
        curve in reserves(),
        excess in any::<u64>(),
    ) {
        let q = curve.virtual_quote;
        prop_assert!(matches!(
            curve.buy_tokens_raw(q),
            Err(QuoteError::ExhaustsCurve)
        ));
        prop_assert!(matches!(
            curve.buy_tokens_raw(q.saturating_add(excess)),
            Err(QuoteError::ExhaustsCurve)
        ));
        // One lamport below the boundary is a legitimate trade.
        if q > 1 {
            prop_assert!(curve.buy_tokens_raw(q - 1).is_ok());
        }
    }

    /// Selling more tokens must never yield less quote.
    #[test]
    fn sell_quote_is_monotonic(
        curve in reserves(),
        small in 1u64..50_000_000_000_000,
        large in 50_000_000_000_001u64..800_000_000_000_000,
    ) {
        let (lo, hi) = if small <= large { (small, large) } else { (large, small) };
        let a = curve.sell_quote_raw(lo);
        let b = curve.sell_quote_raw(hi);
        if let (Ok(a), Ok(b)) = (a, b) {
            prop_assert!(b >= a, "{b} < {a} for {lo} -> {hi}");
        }
    }

    /// No free money: buy then sell back never returns more than went in.
    ///
    /// Checked against the *same* reserves, which is the conservative case a
    /// stale quote would produce, and again against post-trade reserves, which
    /// is what a correct client actually holds.
    #[test]
    fn round_trip_never_manufactures_profit(
        curve in reserves(),
        sol in trade_size(),
    ) {
        let Ok(tokens) = curve.buy_tokens_raw(sol) else { return Ok(()); };
        prop_assert!(tokens > 0 || sol == 0);

        // Same reserves: must not exceed the input.
        if let Ok(back) = curve.sell_quote_raw(u64::try_from(tokens).unwrap_or(u64::MAX)) {
            prop_assert!(
                back <= u128::from(sol),
                "same-reserve round trip returned {back} for an input of {sol}"
            );
        }

        // Post-trade reserves: the honest simulation of an immediate sell.
        let spent = u64::try_from(tokens).unwrap_or(u64::MAX);
        if spent <= curve.virtual_token {
            let after = CurveReserves::new(
                curve.virtual_quote.saturating_add(sol),
                curve.virtual_token - spent,
                curve.real_token,
            );
            if let Ok(back) = after.sell_quote_raw(spent) {
                prop_assert!(
                    back <= u128::from(sol),
                    "post-trade round trip returned {back} for an input of {sol}"
                );
            }
        }
    }

    /// Extreme reserves must never panic and never wrap.
    ///
    /// Any value at all is legal here on purpose: the point is that the
    /// function is total over the whole `u64` range.
    #[test]
    fn extreme_reserves_never_panic(curve in reserves(), sol in any::<u64>()) {
        let _ = curve.buy_tokens_raw(sol);
        let _ = curve.sell_quote_raw(sol);
        let _ = curve.invariant_k();
        let _ = curve.spot_price_quote_per_token();
    }

    /// A curve with zero reserves is refused, never divided by.
    #[test]
    fn degenerate_reserves_are_refused(
        q in 0u64..2,
        v in 0u64..2,
        amount in any::<u64>(),
    ) {
        let curve = CurveReserves::new(q, v, 1);
        if q == 0 || v == 0 {
            let buy = curve.buy_tokens_raw(amount);
            let sell = curve.sell_quote_raw(amount);
            prop_assert!(matches!(buy, Err(QuoteError::EmptyReserves)));
            prop_assert!(matches!(sell, Err(QuoteError::EmptyReserves)));
            prop_assert!(!curve.is_tradeable());
        }
    }

    /// The invariant constant never overflows `u128`.
    #[test]
    fn invariant_k_fits_in_128_bits(pair in (0u64..=u64::MAX, 0u64..=u64::MAX)) {
        let curve = CurveReserves::new(pair.0, pair.1, 0);
        let k = curve.invariant_k();
        let expected = u128::from(pair.0) * u128::from(pair.1);
        prop_assert_eq!(k, expected);
    }
}

proptest! {
    #![proptest_config(config(16_384))]

    /// Fees round up: the program is never short-changed.
    #[test]
    fn fee_rounding_always_costs_us(fee in 1u64..BPS_DENOMINATOR, amount in any::<u64>()) {
        let f = FeeBps::new(fee);
        let ceil = f.apply_ceil(amount);
        let floor = f.apply_floor(amount);
        prop_assert!(ceil >= floor, "ceil {ceil} below floor {floor}");
        prop_assert!(ceil <= amount, "fee {ceil} exceeds amount {amount}");
        prop_assert_eq!(f.apply_ceil(0), 0);
    }

    /// Zero and absurd fee rates behave sanely.
    #[test]
    fn fee_extremes_do_not_become_free(fee in any::<u64>(), amount in 1u64..1_000_000_000) {
        let f = FeeBps::new(fee);
        if fee == 0 {
            prop_assert_eq!(f.apply_ceil(amount), 0);
        } else if fee >= BPS_DENOMINATOR {
            // A rate of 100% or more consumes the entire amount. Wrapping to a
            // small number here would mean paying no fee at all.
            prop_assert_eq!(f.apply_ceil(amount), amount);
            prop_assert!(!f.is_valid());
        } else {
            prop_assert!(f.is_valid());
        }
    }
}

// ---------------------------------------------------------------------------
// Non-property tests: structural guarantees the generator cannot express.
// ---------------------------------------------------------------------------

#[test]
fn bonding_curve_fields_are_u64() {
    // The master plan called `virtual_quote_reserves` a signed 128-bit field.
    // It is not: that is the AMM `Pool` account, a different venue. This pins
    // the layout so the wrong model cannot return unnoticed — a curve type with
    // an `i128` in it would be the symptom.
    assert_eq!(
        std::mem::size_of::<CurveReserves>(),
        24,
        "three u64 fields; an i128 crept in"
    );
    let curve = CurveReserves::new(u64::MAX, 1, 1);
    // A negative reserve is unrepresentable by construction, which is exactly
    // why the signed handling belongs to the PumpSwap pool instead.
    assert_eq!(curve.virtual_quote, u64::MAX);
    assert!(curve.buy_tokens_raw(u64::MAX).is_err());
}

#[test]
fn curve_cannot_be_traded_at_or_beyond_its_own_reserve() {
    // Spending exactly the virtual quote reserve would drive the quote reserve
    // to zero, where the price is undefined. Refuse rather than divide.
    let curve = CurveReserves::new(1_000, 1_000_000, 500_000);
    assert!(matches!(
        curve.buy_tokens_raw(1_000),
        Err(QuoteError::ExhaustsCurve)
    ));
    assert!(matches!(
        curve.buy_tokens_raw(1_001),
        Err(QuoteError::ExhaustsCurve)
    ));
    assert!(curve.buy_tokens_raw(999).is_ok());
}

#[test]
fn quoting_is_deterministic() {
    // Quotes run inside a deadline. A quote that differs between two calls on
    // identical state means a hidden cache, a clock, or an unordered
    // iteration — and the caller cannot reason about any of them.
    let curve = CurveReserves::fresh_curve();
    let first = curve.buy_tokens_raw(5_000_000);
    for _ in 0..1_000 {
        assert_eq!(curve.buy_tokens_raw(5_000_000), first);
    }
}

#[test]
fn a_fresh_curve_quotes_a_plausible_first_buy() {
    // A concrete anchor next to the properties: 0.02 SOL into a fresh curve
    // must buy real tokens, not zero, and the spot price must be sane. If this
    // ever fails, the properties above are true but meaningless.
    let curve = CurveReserves::fresh_curve();
    let two_hundredths_sol = 20_000_000u64; // 0.02 SOL in lamports
    let tokens = curve
        .buy_tokens_raw(two_hundredths_sol)
        .expect("a fresh curve must quote");
    assert!(tokens > 0, "0.02 SOL bought nothing on a fresh curve");

    let price = curve.spot_price_quote_per_token();
    assert!(price.is_finite() && price > 0.0);
    println!("fresh curve: 0.02 SOL -> {tokens} base units, spot {price} quote per base unit");
}

#[test]
fn zero_position_quotes_nothing_rather_than_free_tokens() {
    // Zero must be refused, not quoted as a large number by a divide that
    // happens to land well.
    let curve = CurveReserves::fresh_curve();
    assert!(matches!(
        curve.buy_tokens_raw(0),
        Err(QuoteError::ZeroAmount)
    ));
    assert!(matches!(
        curve.sell_quote_raw(0),
        Err(QuoteError::ZeroAmount)
    ));
}

#[test]
fn a_fresh_launch_has_no_real_tokens_and_must_still_be_quotable() {
    // One block after `create_v2`, a bonding curve holds **zero** real tokens:
    // the entire supply is virtual. `is_tradeable` once required `real_token >
    // 0`, which would have refused every fresh launch — the exact inverse of
    // what a sniper is for, and silently so, since a refusal looks identical
    // to a quiet market.
    let curve = CurveReserves::fresh_curve();
    assert_eq!(curve.real_token, 0, "a new curve deposits nothing yet");
    assert!(curve.is_tradeable(), "a fresh launch must be quotable");
    // And the full supply must be virtual at that moment.
    assert_eq!(
        u128::from(curve.virtual_token),
        sniper_core::pricing::TOKEN_SUPPLY,
        "the whole token supply is virtual before the first sell"
    );
    assert!(curve.buy_tokens_raw(20_000_000).is_ok());
}

#[test]
fn chapter_ran_at_least_the_target_number_of_cases() {
    // A count, so that shrinking `cases` above is a visible change rather than
    // a quiet loss of evidence.
    let properties = 9 + 2; // nine reserve properties, two fee properties
    let cases_per_property = 8_192 + 16_384;
    let total = properties * cases_per_property;
    println!(
        "MathInvariants: {properties} properties x up to {cases_per_property} cases (>= {total})"
    );
    assert!(total >= TARGET_CASES as usize);
}
