//! 🪙 Curve state — what a launch looks like at the moment it is seen.
//!
//! ## Why this exists
//!
//! Chapter 6 measured 107 live launches and found that not one of them was a
//! structural trap: authorities revoked, no dangerous Token-2022 extension,
//! ordinary supply. A filter built on "can this token be sold" therefore accepts
//! everything, and accepts it while looking busy.
//!
//! If the danger is not structural it has to be positional: a curve that is
//! already 80% sold is not a launch, it is an exit. Nothing in chapter 2's
//! decoder knows that, because the decoder's job is to recognise the
//! instruction, not to judge the trade.
//!
//! ## What is measured rather than assumed
//!
//! `supply` is read from the curve account itself. Chapter 3 spent two
//! corrections guessing it — one billion from documentation, then two billion
//! from a single inspected mint — while the true figure was sitting in an
//! account field the whole time, per token.
//!
//! ## Progress is the number that decides
//!
//! [`CurveState::progress_permille`] answers "how much of this token has
//! already been sold", which is the only shape-independent measure of whether
//! an entry is early or late. It is deliberately a ratio rather than an
//! absolute reserve count, because the reserves themselves depend on supply and
//! therefore on the very thing that was being guessed wrong.

use crate::pricing::{CurveReserves, QuoteError};

/// Largest permille value representable, as a `u128` for the comparison below.
const CEILING_PERMILLE: u128 = u32::MAX as u128;

/// A bonding curve as read at one instant.
///
/// A snapshot, not a live view: by the time a trade is built the curve has
/// moved, and pretending otherwise is what slippage is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurveState {
    /// Virtual quote reserve — the SOL side of the price.
    pub virtual_quote: u64,
    /// Virtual token reserve.
    pub virtual_token: u64,
    /// Tokens already sold out of the curve.
    pub real_token: u64,
    /// SOL already in the curve.
    pub real_quote: u64,
    /// Total supply, read from the account.
    pub supply: u64,
    /// Whether the curve has graduated and left the bonding-curve venue.
    pub complete: bool,
}

impl CurveState {
    /// Converts to the pricing type.
    ///
    /// `real_token` is deliberately *not* folded into the quote reserves: the
    /// constant-product maths runs on the virtual pair, and the real balance is
    /// what the cap is checked against.
    #[must_use]
    pub const fn reserves(&self) -> CurveReserves {
        CurveReserves::new(self.virtual_quote, self.virtual_token, self.real_token)
    }

    /// Share of supply already sold, in parts per thousand.
    ///
    /// Parts per thousand rather than a float because this is a comparison
    /// against a threshold, and an integer comparison cannot be off by one part
    /// in a million. Returns 0 when supply is unknown, which reads as "nothing
    /// sold" — the optimistic direction. That is a deliberate asymmetry: a
    /// missing supply should not block a trade, and `supply == 0` also means
    /// there is no cap to enforce.
    #[must_use]
    pub const fn progress_permille(&self) -> u32 {
        if self.supply == 0 {
            return 0;
        }
        // `saturating_mul` rather than `*`: `real_token * 1000` overflows u64 at
        // roughly 1.8e16 base units, and a wrap into a small number would read
        // as "almost nothing sold" for a curve that has sold everything. The
        // product is done in u128, which cannot overflow for any u64 input.
        //
        // The clamp is applied to the *ratio*, not to the numerator. An earlier
        // version clamped the numerator against `u32::MAX`, which is smaller
        // than the numerator for every realistic supply — so the division then
        // produced zero, and every progress report came back as "nothing sold"
        // regardless of the truth.
        let scaled = (self.real_token as u128).saturating_mul(1_000);
        let ratio = scaled / self.supply as u128;
        if ratio > CEILING_PERMILLE {
            u32::MAX
        } else {
            ratio as u32
        }
    }

    /// Whether the curve has room for a position of `lamports` at all.
    ///
    /// Spending at or above the virtual quote reserve would drive it to zero,
    /// where the price is undefined — the same boundary chapter 3 draws.
    #[must_use]
    pub const fn has_room_for(&self, lamports: u64) -> bool {
        !self.complete && lamports > 0 && self.virtual_quote > lamports
    }

    /// Tokens a position of `lamports` would buy, before fees.
    pub fn quote_buy(&self, lamports: u64) -> Result<u128, QuoteError> {
        if self.complete {
            // A graduated curve no longer trades here. Returning a quote would
            // invite an order against a venue that will reject it.
            return Err(QuoteError::CurveComplete);
        }
        self.reserves().buy_tokens_raw(lamports)
    }

    /// SOL the curve holds, for reference in logs and `PnL` reconciliation.
    #[must_use]
    pub const fn sol_in_curve(&self) -> u64 {
        self.real_quote
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::TOKEN_SUPPLY;

    fn curve() -> CurveState {
        CurveState {
            virtual_quote: 30_000_000_000,
            virtual_token: 800_000_000_000_000,
            real_token: 0,
            real_quote: 0,
            supply: 1_000_000_000_000_000,
            complete: false,
        }
    }

    #[test]
    fn a_fresh_curve_has_sold_nothing() {
        assert_eq!(curve().progress_permille(), 0);
    }

    #[test]
    fn progress_tracks_the_share_already_sold() {
        let mut c = curve();
        c.real_token = c.supply / 2;
        assert_eq!(c.progress_permille(), 500);
        c.real_token = c.supply;
        assert_eq!(c.progress_permille(), 1_000);
        c.real_token = c.supply / 4;
        assert_eq!(c.progress_permille(), 250);
    }

    #[test]
    fn absurd_reserves_produce_a_large_number_not_a_small_one() {
        // The property that matters is not the exact value — it is that a wrap
        // cannot turn an absurdly full curve into one that reads as untouched.
        // A real_token far above supply is nonsense data, and nonsense must be
        // visibly nonsense rather than plausibly empty.
        let c = CurveState {
            real_token: u64::MAX,
            supply: TOKEN_SUPPLY as u64,
            ..curve()
        };
        let progress = c.progress_permille();
        assert!(
            progress > 1_000,
            "an over-full curve reported as {progress} permille — that is a wrap"
        );

        // And at the extreme, the value saturates rather than truncating.
        let tiny_supply = CurveState {
            real_token: u64::MAX,
            supply: 1,
            ..curve()
        };
        assert_eq!(tiny_supply.progress_permille(), u32::MAX);
    }

    #[test]
    fn an_unknown_supply_reads_as_untouched_rather_than_blocking() {
        let mut c = curve();
        c.supply = 0;
        c.real_token = 500;
        assert_eq!(c.progress_permille(), 0);
        // And it still quotes, because with no supply there is no cap to hit.
        assert!(c.quote_buy(20_000_000).is_ok());
    }

    #[test]
    fn a_complete_curve_quotes_nothing() {
        // The curve has graduated to the AMM. Quoting here would produce an
        // order the program rejects.
        let c = CurveState {
            complete: true,
            ..curve()
        };
        assert!(!c.has_room_for(20_000_000));
        assert_eq!(c.quote_buy(20_000_000), Err(QuoteError::CurveComplete));
    }

    #[test]
    fn a_position_must_fit_inside_the_quote_reserve() {
        // A curve whose quote reserve is barely above the position size, so the
        // boundary is the interesting thing rather than a rounding artefact of a
        // large reserve.
        let c = CurveState {
            virtual_quote: 21_000_000,
            ..curve()
        };
        assert!(c.has_room_for(20_000_000));
        assert!(
            !c.has_room_for(21_000_000),
            "exactly the reserve is no room"
        );
        assert!(!c.has_room_for(21_000_001));
        assert!(!c.has_room_for(0));
    }

    #[test]
    fn two_hundredths_of_sol_buys_real_tokens_on_a_fresh_curve() {
        let tokens = curve()
            .quote_buy(20_000_000)
            .expect("a fresh curve must quote");
        assert!(tokens > 0);
    }

    #[test]
    fn reserves_do_not_fold_the_real_balance_into_the_price() {
        // The price runs on the virtual pair. Folding real balances in would
        // make the quote depend on how much had already traded, which is a
        // different model and not the one the program implements.
        let mut c = curve();
        let before = c.reserves();
        c.real_token = 400_000_000_000_000;
        assert_eq!(c.reserves().virtual_token, before.virtual_token);
        assert_eq!(c.reserves().real_token, c.real_token);
    }
}
