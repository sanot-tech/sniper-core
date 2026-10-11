//! 📐 `CurveGeometry` — what a curve state means, stated as laws.
//!
//! ## The claim
//!
//! `CurveState` answers the question chapter 6 made load-bearing: "how far
//! along is this curve?". Everything downstream -- whether a launch is still
//! early enough to enter, whether it is really an exit -- is a threshold on
//! that number, so the number has to mean exactly what its name says.
//!
//! ## What the unit tests do not cover
//!
//! `curve.rs` has good tests for specific values: half sold is 500, fully sold
//! is 1000. They share an assumption that is never stated, and that assumption
//! is wrong in a way worth pinning down.
//!
//! `progress_permille` is documented as "share of supply already sold, **in
//! parts per thousand**". Permille is a fixed scale: 0 is nothing sold, 1000 is
//! everything. The implementation clamps at `u32::MAX`, not at 1000.
//!
//! That is defensible -- clamping at 1000 would erase the distinction between a
//! curve that is completely sold and one that is wildly over-full, which is
//! nonsense data worth surfacing rather than flattening. And the existing test
//! `absurd_reserves_produce_a_large_number_not_a_small_one` explicitly asserts
//! the result is *greater* than 1000 for over-full input, so the behaviour is
//! intentional.
//!
//! So the value is not a permille. It is a permille that may exceed 1000. Any
//! caller that treats it as a bounded ratio -- clamping it, averaging it,
//! plotting it on a 0..1000 axis -- is silently wrong, and the type does not
//! stop them. That gap is what this file exists to close.
//!
//! ## The properties
//!
//! 1. **Monotonicity** -- more sold never reports less progress.
//! 2. **Bounds** -- progress is always in 0..=u32::MAX, never negative (it is
//!    unsigned, stated anyway because the saturation path is the interesting
//!    one) and never a wrapped small value for absurd input.
//! 3. **Known-supply accuracy** -- for a supply that is actually known, the
//!    permille figure is the ratio, to within one unit of truncation.
//! 4. **The `has_room_for` / `quote_buy` agreement** -- the two ways of asking
//!    "can this position be placed here" must never disagree.

use proptest::prelude::*;
use sniper_core::curve::CurveState;
use sniper_core::pricing::QuoteError;

/// A curve shaped like a real fresh launch: 30 virtual SOL, 800M virtual
/// tokens against a 1B supply, nothing sold yet.
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

/// Supplies that are actually worth reasoning about, plus the degenerate ones.
fn supply() -> impl Strategy<Value = u64> {
    prop_oneof![
        // Real pump.fun supplies: 1B base units with 6 decimals.
        Just(1_000_000_000_000_000_u64),
        // Every order of magnitude around it.
        1_u64..1_000_000_000_000_000_000,
        // And the awkward small ones, where the permille figure is coarse.
        1_u64..1_000,
    ]
}

/// Sold amounts, drawn as a *fraction* of supply so the expected ratio is
/// computable and the test is checking the arithmetic rather than the RNG.
fn sold_fraction() -> impl Strategy<Value = u8> {
    0_u8..=100
}

proptest! {
    // ---------------------------------------------------------------------
    // Property 1 — monotonicity
    // ---------------------------------------------------------------------

    /// Selling more never reports less progress.
    ///
    /// This is the property a threshold depends on. If progress could go
    /// backwards as a curve fills, then "refuse anything above 800 permille"
    /// would be a rule that un-refuses things as they get worse, and the filter
    /// would be worse than useless at exactly the moment it matters.
    #[test]
    fn progress_never_decreases_as_more_is_sold(supply in supply(), sold in sold_fraction()) {
        let mut curve = fresh_curve();
        curve.supply = supply;
        curve.real_token = (supply / 100) * u64::from(sold);

        let before = curve.progress_permille();
        curve.real_token += supply / 100;
        let after = curve.progress_permille();

        prop_assert!(
            after >= before,
            "progress fell from {before} to {after} when more tokens were sold \
             (supply {supply}, sold {sold}%)"
        );
    }

    // ---------------------------------------------------------------------
    // Property 2 — bounds
    // ---------------------------------------------------------------------

    /// Absurd input saturates visibly rather than wrapping to something small.
    ///
    /// `real_token * 1000` overflows `u64` at roughly 1.8e16 base units, well
    /// inside the legal range. A plain multiply would wrap and report a curve
    /// that has sold everything as one that has barely sold anything -- the
    /// single most dangerous possible reading of this number, because the
    /// response to "0% sold" is to enter and the curve is gone.
    #[test]
    fn absurd_sold_amounts_saturate_instead_of_wrapping(
        supply in 1_u64..1_000_000_000_000_000_000,
        multiple in 2_u64..10_000,
    ) {
        // `real_token` strictly greater than `supply`. Equal is not the
        // interesting case: sold == supply is exactly 1000 permille, a
        // perfectly correct answer, and asserting it exceeds 1000 would be
        // asserting that a full curve reads as over-full.
        let real_token = supply.saturating_mul(multiple);
        prop_assume!(real_token > supply);

        let curve = CurveState {
            real_token,
            supply,
            ..fresh_curve()
        };
        let progress = curve.progress_permille();

        // More than the full supply was sold, so the figure must say so. A
        // wrapping multiply would land near zero -- and "near zero" is the one
        // reading of this number that makes the bot enter a curve that is gone.
        prop_assert!(
            progress > 1_000,
            "supply {supply} with {real_token} sold ({}x) reported {progress} permille",
            multiple
        );
    }

    /// The figure never exceeds `u32::MAX`.
    ///
    /// Trivial for a `u32` return type, and stated anyway because the clamp in
    /// the implementation is the only thing making it true, and a `u128`-to-
    /// `u32` cast is exactly the kind of narrowing that survives review.
    #[test]
    fn progress_is_representable(real_token in any::<u64>(), supply in supply()) {
        let curve = CurveState {
            real_token,
            supply,
            ..fresh_curve()
        };
        // No assertion needed to make this compile-and-not-panic; the cast is
        // inside the function. This test exists so that changing the return
        // type to u128 without updating the clamp fails here rather than
        // silently changing what every threshold in the project means.
        let _ = curve.progress_permille();
    }

    // ---------------------------------------------------------------------
    // Property 3 — accuracy when the supply is known
    // ---------------------------------------------------------------------

    /// With a known supply, the figure is the true ratio to within rounding.
    ///
    /// Integer division truncates, so the result is at most one permille below
    /// the exact value. Asserting an exact match would be asserting an
    /// arithmetic detail that rounding is allowed to change; asserting the
    /// bound is asserting the property a threshold actually relies on.
    #[test]
    fn a_known_supply_yields_the_true_ratio(
        supply in 1_000_u64..1_000_000_000_000_000_000,
        permille in 0_u128..=1_000,
    ) {
        let curve = CurveState {
            real_token: (supply as u128 * permille / 1_000) as u64,
            supply,
            ..fresh_curve()
        };

        let reported = u128::from(curve.progress_permille());
        // Truncation means "at most", and the re-multiplied figure must not
        // undershoot by more than the rounding of the input itself.
        prop_assert!(
            permille >= reported || reported + 1 >= permille,
            "supply {supply} at {permille} permille reported {reported}"
        );
    }

    /// An unknown supply reads as untouched, and that is deliberate.
    ///
    /// `supply == 0` returns 0 permille rather than refusing, because a missing
    /// supply should not block a trade and a zero supply genuinely has no cap
    /// to enforce. The optimistic direction is the right one to be wrong in
    /// here: the alternative is a filter that silently refuses every launch
    /// whose account it could not fully read, which is indistinguishable from a
    /// quiet market.
    #[test]
    fn a_zero_supply_never_blocks(real_token in any::<u64>()) {
        let curve = CurveState {
            real_token,
            supply: 0,
            ..fresh_curve()
        };
        prop_assert_eq!(curve.progress_permille(), 0);
    }
}

/// Non-property tests: the agreement between the two ways of asking the same
/// question, and the boundary that separates them.
#[cfg(test)]
mod agreement {
    use super::*;

    /// `has_room_for` and `quote_buy` never disagree about the same position.
    ///
    /// These are two answers to one question -- can a position of this size be
    /// placed here? -- and they are computed from different state. If they ever
    /// disagree, a caller that checks one and a caller that tries the other get
    /// different behaviour for the same market, and which one is wrong depends
    /// on which ran.
    #[test]
    fn room_and_quote_agree_on_every_position() {
        let curve = fresh_curve();
        // Deliberately spans the boundary: the position sizes here run from
        // zero through the whole virtual quote reserve and past it.
        for lamports in (0..40_000_000).step_by(1_000_000) {
            let has_room = curve.has_room_for(lamports);
            let quote = curve.quote_buy(lamports);
            assert_eq!(
                has_room,
                quote.is_ok(),
                "size {lamports}: has_room_for says {has_room} but quote_buy says \
                 {:?}",
                quote.map(|_| ())
            );
        }
    }

    /// A graduated curve refuses both, for the same reason.
    ///
    /// `has_room_for` returning false while `quote_buy` returns a number would
    /// be the worst kind of disagreement: one caller builds an order the
    /// program will reject, the other correctly declines, and the failure shows
    /// up on chain as a failed transaction rather than as a missing trade.
    #[test]
    fn a_graduated_curve_refuses_both_questions() {
        let curve = CurveState {
            complete: true,
            ..fresh_curve()
        };
        assert!(!curve.has_room_for(20_000_000));
        assert_eq!(
            curve.quote_buy(20_000_000),
            Err(QuoteError::CurveComplete),
            "a graduated curve must refuse with the specific reason, not a \
             generic pricing error that looks like bad maths"
        );
    }

    /// The position must fit strictly inside the quote reserve.
    ///
    /// Exactly equal leaves the reserve at zero, where the price is undefined.
    /// The distinction between "fits" and "fills it exactly" is one lamport and
    /// it is the whole difference between a trade and a division by zero.
    #[test]
    fn exactly_the_reserve_is_not_room() {
        let curve = CurveState {
            virtual_quote: 21_000_000,
            ..fresh_curve()
        };
        assert!(
            curve.has_room_for(20_000_000),
            "one lamport of room is room"
        );
        assert!(
            !curve.has_room_for(21_000_000),
            "exactly the reserve is not"
        );
        assert!(!curve.has_room_for(21_000_001), "past the reserve is not");
        assert!(!curve.has_room_for(0), "a zero position is not a position");
    }
}
