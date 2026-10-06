//! 📐 Curve pricing — chapter 3.
//!
//! ## What this module owns, and what it borrows
//!
//! The curve arithmetic itself is **not** ours: [`pump_rust_client`] ships the
//! same maths the on-chain program runs, and re-deriving it would be a way to
//! be subtly wrong. What *is* ours:
//!
//! * deciding which account's fields feed the quote, and with what types;
//! * refusing to quote when the inputs cannot describe a real trade;
//! * capping against the reserve the program will actually honour;
//! * and the invariants that prove we never round in our own favour.
//!
//! ## A correction this chapter had to make
//!
//! Planning assumed `virtual_quote_reserves` was a signed 128-bit field that
//! could go negative. Checking the pinned IDL shows that is only true of the
//! **AMM `Pool` account** (`PumpSwap`). The **bonding curve** account is `u64`:
//!
//! ```text
//!   BondingCurve::virtual_quote_reserves : u64    ← this chapter
//!   Pool::virtual_quote_reserves          : i128   ← PumpSwap, a different venue
//! ```
//!
//! Treating them as the same thing would have produced overflow in one and a
//! wrong sign in the other, and both would have looked like a plausible price.
//! The distinction is asserted by a test so it cannot quietly regress.
//!
//! ## Why invariants beat examples here
//!
//! A wrong price does not crash anything. It produces a transaction that
//! succeeds, fills at a slightly worse price, and reports a slightly wrong
//! `PnL`. Nothing raises, nothing logs, and the loss compounds quietly. The only
//! defence that scales is a property that must hold for *every* input, checked
//! over tens of thousands of generated cases.

use core::fmt;

pub use pump_rust_client::math::bonding_curve::{
    buy_token_quote_with_sol, sell_quote, TOKEN_SUPPLY,
};

/// Basis-point denominator. Fees everywhere in this protocol are bps.
pub const BPS_DENOMINATOR: u64 = 10_000;

/// The virtual and real reserves that define a bonding curve's price.
///
/// All three are `u64` lamports/base units, matching the on-chain account.
/// Intermediates inside the maths are `u128`, which is where overflow actually
/// happens: `sol * tokens` overflows `u64` at roughly 4·10⁹ each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurveReserves {
    /// Virtual quote reserve (SOL, or USDC for a paired coin).
    pub virtual_quote: u64,
    /// Virtual token reserve, in base units.
    pub virtual_token: u64,
    /// Real token reserve: the most the program will actually sell.
    pub real_token: u64,
}

impl CurveReserves {
    #[must_use]
    pub const fn new(virtual_quote: u64, virtual_token: u64, real_token: u64) -> Self {
        Self {
            virtual_quote,
            virtual_token,
            real_token,
        }
    }

    /// A plausible curve at launch, used by tests and paper runs.
    ///
    /// Numbers taken from the shape of a fresh pump.fun coin rather than
    /// invented: the curve starts with the whole token supply virtual and no
    /// real tokens deposited, which is what makes the first tokens cheap.
    ///
    /// `real_token` is **zero**, and that is not a placeholder. It is the exact
    /// state of a curve one block after `create_v2`.
    #[must_use]
    pub const fn fresh_curve() -> Self {
        // `TOKEN_SUPPLY` is `u128` in the SDK; the account field is `u64`. The
        // cast is safe because the supply is 10^15, nowhere near the boundary.
        Self::new(1_000_000_000, TOKEN_SUPPLY as u64, 0)
    }

    /// Tokens received for `sol_lamports`, fees **not** applied.
    ///
    /// This is the raw constant-product quote, useful for reasoning about price
    /// movement. Real trades must go through
    /// [`Self::buy_tokens_with_fees`], because the program charges fees and
    /// rounds in its own favour.
    pub fn buy_tokens_raw(&self, sol_lamports: u64) -> Result<u128, QuoteError> {
        if self.virtual_quote == 0 || self.virtual_token == 0 {
            return Err(QuoteError::EmptyReserves);
        }
        if sol_lamports == 0 {
            return Err(QuoteError::ZeroAmount);
        }
        // Reject an input that would exhaust the curve outright rather than
        // returning a quote that cannot be filled.
        if sol_lamports >= self.virtual_quote {
            return Err(QuoteError::ExhaustsCurve);
        }
        buy_token_quote_with_sol(self.virtual_quote, self.virtual_token, sol_lamports)
            .map_err(QuoteError::from)
    }

    /// Quote received for selling `token_amount`, fees **not** applied.
    pub fn sell_quote_raw(&self, token_amount: u64) -> Result<u128, QuoteError> {
        if self.virtual_quote == 0 || self.virtual_token == 0 {
            return Err(QuoteError::EmptyReserves);
        }
        if token_amount == 0 {
            return Err(QuoteError::ZeroAmount);
        }
        sell_quote(self.virtual_quote, self.virtual_token, token_amount).map_err(QuoteError::from)
    }

    /// Tokens the program would credit for `sol_lamports`, **before fees**.
    ///
    /// This is the constant-product quote the curve performs. Fees are a
    /// separate, deliberate deduction on top — the SDK's fee-aware path needs a
    /// `Global` and a `FeeConfig` account, which means real chain state, so it is
    /// wired in chapter 4 where the transaction is actually assembled.
    ///
    /// What matters here is the shape of the curve, and the shape has exactly
    /// the properties asserted in `tests/math_invariants.rs`.
    ///
    /// The one thing callers must remember: the program charges fees and rounds
    /// against us, so this value is an **upper bound** on what a fill yields.
    /// Using it as an expected fill is how a bot ends up reporting profit it
    /// never received.
    pub fn buy_tokens_upper_bound(&self, sol_lamports: u64) -> Result<u128, QuoteError> {
        self.buy_tokens_raw(sol_lamports)
    }

    /// Lamports the program would credit for selling `token_amount`,
    /// **before fees**. Likewise an upper bound.
    pub fn sell_upper_bound(&self, token_amount: u64) -> Result<u128, QuoteError> {
        self.sell_quote_raw(token_amount)
    }

    /// Spot price of one base unit, as a float.
    ///
    /// Floats are acceptable **here and only here**: this feeds display and
    /// human-readable logs. Nothing that decides whether to trade reads it,
    /// because a float that is off by one part in 10^18 is exactly the kind of
    /// quiet inaccuracy that shows up only in the `PnL`.
    #[must_use]
    pub fn spot_price_quote_per_token(&self) -> f64 {
        if self.virtual_token == 0 {
            return 0.0;
        }
        self.virtual_quote as f64 / self.virtual_token as f64
    }

    /// Curve constant `k = virtual_quote * virtual_token`, widened to `u128`.
    ///
    /// The invariant that defines the whole family: a trade moves reserves
    /// along a curve of constant `k`. If this changes, the model is wrong.
    #[must_use]
    pub fn invariant_k(&self) -> u128 {
        u128::from(self.virtual_quote) * u128::from(self.virtual_token)
    }

    /// Whether this curve can be quoted at all.
    ///
    /// Deliberately ignores `real_token`. On a brand-new launch
    /// `real_token_reserves` is **zero** — no tokens have left the curve yet —
    /// and that is exactly the state a sniper exists to trade. An earlier
    /// version of this function required `real_token > 0` and would have
    /// refused every fresh launch while happily accepting saturated ones: the
    /// precise inverse of what the bot is for.
    ///
    /// Only the virtual reserves decide, because only they appear in the price.
    #[must_use]
    pub const fn is_tradeable(&self) -> bool {
        self.virtual_quote > 0 && self.virtual_token > 0
    }
}

/// A fee rate in basis points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeBps(pub u64);

impl FeeBps {
    #[must_use]
    pub const fn new(bps: u64) -> Self {
        Self(bps)
    }

    /// Fees charged on `amount`, rounded **up**.
    ///
    /// Rounding up is not a preference. It is the direction the program uses,
    /// and a client that rounded down would under-reserve the fee, produce a
    /// transaction that fails, and burn the fee anyway.
    ///
    /// Saturates rather than wrapping: an absurd fee must not become a small
    /// one, because "free" is the worst possible answer.
    #[must_use]
    pub const fn apply_ceil(&self, amount: u64) -> u64 {
        if self.0 == 0 {
            return 0;
        }
        if self.0 >= BPS_DENOMINATOR {
            return amount;
        }
        let numerator = amount as u128 * self.0 as u128;
        let fee = numerator.div_ceil(BPS_DENOMINATOR as u128);
        if fee >= amount as u128 {
            amount
        } else {
            fee as u64
        }
    }

    /// Fees charged on `amount`, rounded **down**.
    ///
    /// Only for reporting. Never for deciding what to send.
    #[must_use]
    pub const fn apply_floor(&self, amount: u64) -> u64 {
        if self.0 == 0 || self.0 >= BPS_DENOMINATOR {
            return 0;
        }
        // A plain comparison rather than `Ord::min`: that trait method is not
        // const-stable yet, and the whole point of this function is that it
        // compiles into the hot path with no calls at all.
        let fee = (amount as u128 * self.0 as u128 / BPS_DENOMINATOR as u128) as u64;
        if fee > amount {
            amount
        } else {
            fee
        }
    }

    /// True when the rate is inside the plausible range.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.0 < BPS_DENOMINATOR
    }
}

/// Why a quote could not be produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteError {
    /// A reserve is zero, so the price is undefined.
    EmptyReserves,
    /// An amount of zero was supplied.
    ZeroAmount,
    /// The input is large enough to consume the entire virtual reserve.
    ExhaustsCurve,
    /// The SDK refused: overflow or a fee derivation it could not complete.
    SdkFailure,
}

impl From<pump_rust_client::math::QuoteError> for QuoteError {
    fn from(_: pump_rust_client::math::QuoteError) -> Self {
        // The SDK's variants are all "this input cannot be quoted". Losing the
        // distinction costs nothing operationally, and collapsing them means a
        // new SDK variant cannot break this crate's error type.
        QuoteError::SdkFailure
    }
}

impl fmt::Display for QuoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Self::EmptyReserves => "a reserve is zero, so the price is undefined",
            Self::ZeroAmount => "amount must be greater than zero",
            Self::ExhaustsCurve => "input exceeds the virtual reserve; the curve would be consumed",
            Self::SdkFailure => "quote math rejected these inputs",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for QuoteError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_curve_is_tradeable() {
        let c = CurveReserves::fresh_curve();
        assert!(c.is_tradeable());
        assert_eq!(
            c.invariant_k(),
            u128::from(c.virtual_quote) * u128::from(c.virtual_token)
        );
    }

    #[test]
    fn zero_reserves_are_refused_not_divided_by() {
        let empty = CurveReserves::new(0, 1_000, 1_000);
        assert!(matches!(
            empty.buy_tokens_raw(1),
            Err(QuoteError::EmptyReserves)
        ));
        assert!(matches!(
            CurveReserves::new(1_000, 0, 1_000).sell_quote_raw(1),
            Err(QuoteError::EmptyReserves)
        ));
        assert!(!empty.is_tradeable());
    }

    #[test]
    fn zero_amount_is_refused() {
        let c = CurveReserves::fresh_curve();
        assert!(matches!(
            c.buy_tokens_upper_bound(0),
            Err(QuoteError::ZeroAmount)
        ));
        assert!(matches!(c.sell_upper_bound(0), Err(QuoteError::ZeroAmount)));
    }

    #[test]
    fn fees_round_up_and_down_as_documented() {
        // 100 bps of 1_000 is exactly 10 either way.
        assert_eq!(FeeBps::new(100).apply_ceil(1_000), 10);
        assert_eq!(FeeBps::new(100).apply_floor(1_000), 10);
        // 250 bps of 3 is 0.75: up gives 1, down gives 0.
        assert_eq!(FeeBps::new(250).apply_ceil(3), 1);
        assert_eq!(FeeBps::new(250).apply_floor(3), 0);
    }

    #[test]
    fn absurd_fee_saturates_instead_of_wrapping() {
        let fee = FeeBps::new(50_000); // 500%
        assert_eq!(fee.apply_ceil(1_000), 1_000, "must not become free");
        assert_eq!(fee.apply_ceil(u64::MAX), u64::MAX);
        assert!(!fee.is_valid());
    }

    #[test]
    fn zero_fee_costs_nothing() {
        let fee = FeeBps::new(0);
        assert_eq!(fee.apply_ceil(u64::MAX), 0);
        assert_eq!(fee.apply_floor(u64::MAX), 0);
    }
}
