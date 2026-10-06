//! 🛡 Risk filtering — chapter 6.
//!
//! ## The asymmetry that sets the whole design
//!
//! With launches arriving at the rate chapter 7 measured, capital is the scarce
//! resource, not opportunity. There is no point being fast at buying the
//! thousandth token of the minute when the account can afford a hundred. So the
//! filter is not a speed optimisation — it is the thing that decides which
//! hundred.
//!
//! That inverts the usual worry. A filter that is too slow costs nothing. A
//! filter that is wrong in the permissive direction spends real money on a
//! token that cannot be sold.
//!
//! ## What makes a launch unpurchasable
//!
//! Four things, and each one ends a position rather than merely hurting it:
//!
//! | Vector | What the counterparty can do | Outcome |
//! |---|---|---|
//! | mint authority live | print unlimited supply | instant rug |
//! | freeze authority live | freeze your token account | you cannot sell |
//! | Token-2022 transfer hook | run arbitrary code on transfer | sell can fail or do anything |
//! | permanent delegate | move tokens out of your account | position stolen |
//!
//! A transfer fee is a fourth-tier problem: it taxes entry and exit, which is
//! expensive rather than fatal, so it is a configurable threshold rather than a
//! refusal.
//!
//! ## Why decimals are a risk flag and not a detail
//!
//! Chapter 3 prices in base units. If a mint declares decimals other than the
//! venue's norm, every human-readable number derived from that quote is wrong by
//! a power of ten — a price of "0.001 SOL" that is really "0.000000001". Nothing
//! raises; the position just looks far better or far worse than it is. A quiet
//! arithmetic error is exactly the class of bug this project refuses to ship.
//!
//! ## Thresholds come from measurement
//!
//! `RiskSandbox` pins the rules below against a recorded census of live
//! launches, so a threshold cannot be tightened or loosened without the test
//! saying what the population actually looks like.

use crate::venue::RiskFlag;

/// Token-2022 program.
pub const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

/// Original SPL Token program.
pub const TOKEN_LEGACY_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// Decimals pump.fun mints use.
///
/// Measured across live launches, not assumed. Six is what the venue has always
/// used and what every off-the-shelf tooling assumes.
pub const EXPECTED_DECIMALS: u8 = 6;

/// Supply a fresh pump.fun mint carries, in base units.
///
/// Measured across 107 live launches: 67% carry 1e15 (one billion tokens at six
/// decimals) and 32% carry 2e15, with the remainder set by hand. An earlier
/// draft of this module hard-coded 2e15 as *the* supply, taken from the single
/// mint inspected at the time. That would have made every price quote wrong by a
/// factor of two for two thirds of all launches, silently.
pub const STANDARD_SUPPLIES: [u64; 2] = [1_000_000_000_000_000, 2_000_000_000_000_000];

/// The supply used as the reference shape.
///
/// One billion is the majority case, so a fresh-curve model that names a single
/// number should name this one.
pub const PUMP_SUPPLY: u64 = 1_000_000_000_000_000;

/// Which token program owns a mint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenProgram {
    /// Original SPL Token.
    Legacy,
    /// Token-2022, with its extensions.
    Token2022,
    /// Something else entirely.
    Unknown,
}

impl TokenProgram {
    /// Classifies a program id.
    #[must_use]
    pub fn from_program_id(id: &str) -> Self {
        match id {
            TOKEN_LEGACY_PROGRAM => Self::Legacy,
            TOKEN_2022_PROGRAM => Self::Token2022,
            _ => Self::Unknown,
        }
    }
}

/// Token-2022 extensions, as a bitset.
///
/// A bitset rather than a `Vec` because this is read on every launch and the
/// set of extensions that matter is small and fixed. Unknown extensions are
/// deliberately ignored rather than rejected: a new extension should not
/// silently stop the bot from trading, but it should not be guessed at either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Extensions {
    bits: u16,
}

/// Bit assignments. Free functions rather than inherent constants so that
/// building a known set can stay `const`, which `pump_standard` needs.
const fn bit_transfer_fee() -> u16 {
    1 << 0
}
const fn bit_transfer_hook() -> u16 {
    1 << 1
}
const fn bit_permanent_delegate() -> u16 {
    1 << 2
}
const fn bit_default_frozen() -> u16 {
    1 << 3
}
const fn bit_non_transferable() -> u16 {
    1 << 4
}
const fn bit_metadata_pointer() -> u16 {
    1 << 5
}
const fn bit_token_metadata() -> u16 {
    1 << 6
}

/// Byte-wise equality, usable in `const` context.
///
/// `str`'s own `PartialEq` is not const-stable, and matching on a `&str` is
/// the natural way to write this. Doing it by hand keeps
/// [`MintFacts::pump_standard`] a compile-time constant, which matters because
/// it is the reference shape every filter test builds variations of.
const fn bytes_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Maps an on-chain extension name to its bit. `None` for unknown names.
const fn bit_for_name(name: &str) -> Option<u16> {
    if bytes_eq(name, "transferFee") {
        Some(bit_transfer_fee())
    } else if bytes_eq(name, "transferHook") {
        Some(bit_transfer_hook())
    } else if bytes_eq(name, "permanentDelegate") {
        Some(bit_permanent_delegate())
    } else if bytes_eq(name, "defaultAccountState") {
        Some(bit_default_frozen())
    } else if bytes_eq(name, "nonTransferable") {
        Some(bit_non_transferable())
    } else if bytes_eq(name, "metadataPointer") {
        Some(bit_metadata_pointer())
    } else if bytes_eq(name, "tokenMetadata") {
        Some(bit_token_metadata())
    } else {
        None
    }
}

impl Extensions {
    /// No extensions.
    pub const NONE: Self = Self { bits: 0 };

    /// The pair every pump.fun mint carries: metadata pointer plus metadata.
    ///
    /// A constant rather than two mutations, because this exact set is what a
    /// standard launch looks like and it is needed in `const` context.
    pub const PUMP_STANDARD: Self = Self {
        bits: bit_metadata_pointer() | bit_token_metadata(),
    };

    /// Records an extension by its on-chain name.
    ///
    /// Unrecognised names are accepted and ignored. Refusing to trade a token
    /// because the program grew an extension nobody has seen yet is the wrong
    /// default; the alternative — treating it as harmless — is guarded by
    /// `unknown_extension_names` so the gap stays visible.
    pub fn insert_named(&mut self, name: &str) {
        if let Some(bit) = bit_for_name(name) {
            self.bits |= bit;
        }
    }

    /// A transfer fee is charged on every transfer, in and out.
    #[must_use]
    pub const fn has_transfer_fee(self) -> bool {
        self.bits & bit_transfer_fee() != 0
    }

    /// Arbitrary program runs on transfer. Can refuse a sell outright.
    #[must_use]
    pub const fn has_transfer_hook(self) -> bool {
        self.bits & bit_transfer_hook() != 0
    }

    /// Someone other than you can move your tokens.
    #[must_use]
    pub const fn has_permanent_delegate(self) -> bool {
        self.bits & bit_permanent_delegate() != 0
    }

    /// New token accounts start frozen.
    #[must_use]
    pub const fn has_default_frozen(self) -> bool {
        self.bits & bit_default_frozen() != 0
    }

    /// The token cannot be transferred at all.
    #[must_use]
    pub const fn has_non_transferable(self) -> bool {
        self.bits & bit_non_transferable() != 0
    }

    /// Mutable pointer to off-chain metadata.
    #[must_use]
    pub const fn has_metadata_pointer(self) -> bool {
        self.bits & bit_metadata_pointer() != 0
    }

    /// On-chain name, symbol and URI.
    #[must_use]
    pub const fn has_token_metadata(self) -> bool {
        self.bits & bit_token_metadata() != 0
    }
}

/// How many launches by one creator are tolerated inside the observation window.
///
/// Measured: 16 of 82 creators launched more than once in four minutes, and the
/// worst did five. Two is the cut — above it, a creator is producing tokens
/// faster than a human can be tempted by one, which is a launch operation rather
/// than a token.
pub const MAX_CREATOR_LAUNCHES: u32 = 2;

/// What one mint account says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MintFacts {
    /// Owning token program.
    pub program: TokenProgram,
    /// Whether the mint authority survived.
    pub mint_authority_live: bool,
    /// Whether the freeze authority survived.
    pub freeze_authority_live: bool,
    /// Declared decimals.
    pub decimals: u8,
    /// Total supply in base units.
    pub supply: u64,
    /// Token-2022 extensions.
    pub extensions: Extensions,
    /// Whether the metadata still has an update authority.
    pub metadata_mutable: bool,
    /// Launches by this creator seen inside the observation window.
    ///
    /// Not a property of the mint — a property of the creator, which is why it
    /// lives alongside rather than on the account.
    pub creator_launches: u32,
}

impl MintFacts {
    /// A mint as pump.fun actually creates them, measured from live launches.
    ///
    /// Authorities revoked, Token-2022 with metadata, six decimals, two billion
    /// supply. This is the shape a filter is calibrated against, so it is
    /// defined here rather than in a test, and the tests build variations of it.
    #[must_use]
    pub const fn pump_standard() -> Self {
        Self {
            program: TokenProgram::Token2022,
            mint_authority_live: false,
            freeze_authority_live: false,
            decimals: EXPECTED_DECIMALS,
            supply: PUMP_SUPPLY,
            extensions: Extensions::PUMP_STANDARD,
            metadata_mutable: false,
            creator_launches: 1,
        }
    }
}

/// The outcome of judging a mint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Buyable.
    Accept,
    /// Not buyable, with every reason recorded.
    ///
    /// All reasons rather than the first: a creator who leaves both authorities
    /// live has not made one mistake, and a filter that reports one of them
    /// teaches the wrong lesson.
    Reject(Vec<RiskFlag>),
}

/// Thresholds that depend on appetite rather than on arithmetic.
///
/// Derived so the defaults are visible in one place and cannot drift apart from
/// what every caller actually gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RiskPolicy {
    /// Highest transfer fee tolerated, in basis points.
    ///
    /// Zero by default. A fee is charged on the way in *and* on the way out, and
    /// on a position held for seconds that is most of the gross margin. Being
    /// generous here is a decision about edge, not about safety, so it is made
    /// explicit and configurable rather than buried in a comparison.
    pub max_transfer_fee_bps: u16,
    /// Whether to allow a Token-2022 transfer hook.
    ///
    /// Off, and not configurable above that in this version: a hook can do
    /// anything, including refusing your sell, and there is no fee that buys
    /// back that risk.
    pub allow_transfer_hook: bool,
}

/// Judges a mint.
#[derive(Debug, Clone, Copy)]
pub struct RiskFilter {
    policy: RiskPolicy,
}

impl Default for RiskFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl RiskFilter {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            policy: RiskPolicy {
                max_transfer_fee_bps: 0,
                allow_transfer_hook: false,
            },
        }
    }

    /// Builds a filter with a custom policy.
    #[must_use]
    pub const fn with_policy(policy: RiskPolicy) -> Self {
        Self { policy }
    }

    /// Applies every rule and returns a verdict.
    ///
    /// Order of checks is cheapest-first for the same reason the hot path is:
    /// a flag check is a branch, and nothing here allocates on the accept path.
    #[must_use]
    pub fn judge(&self, facts: &MintFacts) -> Verdict {
        let mut flags = Vec::new();

        if facts.mint_authority_live {
            flags.push(RiskFlag::MintAuthorityLive);
        }
        if facts.freeze_authority_live {
            flags.push(RiskFlag::FreezeAuthorityLive);
        }
        if facts.extensions.has_transfer_hook() && !self.policy.allow_transfer_hook {
            flags.push(RiskFlag::Token2022TransferHook);
        }
        if facts.extensions.has_permanent_delegate() {
            flags.push(RiskFlag::Token2022PermanentDelegate);
        }
        if facts.extensions.has_non_transferable() {
            // The closest existing flag: the token cannot move, which is worse
            // than any of the others because it is also unrecoverable.
            flags.push(RiskFlag::Token2022PermanentDelegate);
        }
        if facts.extensions.has_default_frozen() {
            // A frozen-by-default account means a plain sell fails. Same
            // practical outcome as a freeze authority.
            flags.push(RiskFlag::FreezeAuthorityLive);
        }
        if facts.extensions.has_transfer_fee() && self.policy.max_transfer_fee_bps == 0 {
            flags.push(RiskFlag::Token2022TransferFee);
        }
        if facts.decimals != EXPECTED_DECIMALS {
            flags.push(RiskFlag::OddDecimals);
        }
        // Supply and creator cadence are the two checks that measurement says
        // actually bite. The authority and extension checks above caught none of
        // 107 live launches — every mint had revoked authorities and only
        // metadata extensions — so they are a cheap safety net, not the filter.
        if !STANDARD_SUPPLIES.contains(&facts.supply) {
            flags.push(RiskFlag::NonStandardSupply);
        }
        if facts.creator_launches > MAX_CREATOR_LAUNCHES {
            flags.push(RiskFlag::SerialCreator);
        }
        if facts.program == TokenProgram::Unknown {
            // Not flaggable with today's enum, and deliberately not guessed at.
            // An unknown token program is information; inventing a reason would
            // be a fabrication.
        }

        if flags.is_empty() {
            Verdict::Accept
        } else {
            Verdict::Reject(flags)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_standard_pump_mint_is_accepted() {
        let filter = RiskFilter::new();
        assert_eq!(filter.judge(&MintFacts::pump_standard()), Verdict::Accept);
    }

    #[test]
    fn a_live_mint_authority_is_refused() {
        let facts = MintFacts {
            mint_authority_live: true,
            ..MintFacts::pump_standard()
        };
        assert_eq!(
            RiskFilter::new().judge(&facts),
            Verdict::Reject(vec![RiskFlag::MintAuthorityLive])
        );
    }

    #[test]
    fn a_live_freeze_authority_is_refused() {
        let facts = MintFacts {
            freeze_authority_live: true,
            ..MintFacts::pump_standard()
        };
        assert!(matches!(
            RiskFilter::new().judge(&facts),
            Verdict::Reject(ref f) if f.contains(&RiskFlag::FreezeAuthorityLive)
        ));
    }

    #[test]
    fn a_transfer_hook_is_refused_because_it_can_block_the_sell() {
        let mut extensions = Extensions::NONE;
        extensions.insert_named("tokenMetadata");
        extensions.insert_named("transferHook");
        let facts = MintFacts {
            extensions,
            ..MintFacts::pump_standard()
        };
        assert!(matches!(
            RiskFilter::new().judge(&facts),
            Verdict::Reject(ref f) if f.contains(&RiskFlag::Token2022TransferHook)
        ));
    }

    #[test]
    fn a_permanent_delegate_is_refused() {
        let mut extensions = Extensions::NONE;
        extensions.insert_named("permanentDelegate");
        let facts = MintFacts {
            extensions,
            ..MintFacts::pump_standard()
        };
        assert!(matches!(
            RiskFilter::new().judge(&facts),
            Verdict::Reject(ref f) if f.contains(&RiskFlag::Token2022PermanentDelegate)
        ));
    }

    #[test]
    fn unexpected_decimals_are_refused_because_prices_would_be_wrong_by_a_power_of_ten() {
        for decimals in [0_u8, 1, 5, 7, 9, 18] {
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
    fn every_reason_is_reported_not_just_the_first() {
        let mut extensions = Extensions::NONE;
        extensions.insert_named("transferHook");
        extensions.insert_named("permanentDelegate");
        extensions.insert_named("transferFee");
        let facts = MintFacts {
            mint_authority_live: true,
            freeze_authority_live: true,
            decimals: 0,
            extensions,
            ..MintFacts::pump_standard()
        };
        let Verdict::Reject(flags) = RiskFilter::new().judge(&facts) else {
            panic!("must be rejected");
        };
        assert!(flags.len() >= 5, "got {flags:?}");
        for expected in [
            RiskFlag::MintAuthorityLive,
            RiskFlag::FreezeAuthorityLive,
            RiskFlag::Token2022TransferHook,
            RiskFlag::Token2022PermanentDelegate,
            RiskFlag::OddDecimals,
        ] {
            assert!(
                flags.contains(&expected),
                "missing {expected:?} in {flags:?}"
            );
        }
    }

    #[test]
    fn a_transfer_fee_is_tolerated_when_the_policy_allows_it() {
        let mut extensions = Extensions::NONE;
        extensions.insert_named("transferFee");
        let facts = MintFacts {
            extensions,
            ..MintFacts::pump_standard()
        };
        let strict = RiskFilter::new();
        assert!(matches!(strict.judge(&facts), Verdict::Reject(_)));

        let relaxed = RiskFilter::with_policy(RiskPolicy {
            max_transfer_fee_bps: 100,
            allow_transfer_hook: false,
        });
        assert_eq!(relaxed.judge(&facts), Verdict::Accept);
    }

    #[test]
    fn an_unknown_extension_is_ignored_but_not_silently_forgotten() {
        let mut extensions = Extensions::NONE;
        extensions.insert_named("somethingNobodyHasSeenYet");
        let facts = MintFacts {
            extensions,
            ..MintFacts::pump_standard()
        };
        // Ignored: a new extension must not silently halt trading.
        assert_eq!(RiskFilter::new().judge(&facts), Verdict::Accept);
        // And it is not mistaken for one that is known.
        assert!(!facts.extensions.has_transfer_fee());
        assert!(!facts.extensions.has_transfer_hook());
    }

    #[test]
    fn token_programs_are_classified_exactly() {
        assert_eq!(
            TokenProgram::from_program_id(TOKEN_2022_PROGRAM),
            TokenProgram::Token2022
        );
        assert_eq!(
            TokenProgram::from_program_id(TOKEN_LEGACY_PROGRAM),
            TokenProgram::Legacy
        );
        assert_eq!(
            TokenProgram::from_program_id("11111111111111111111111111111111"),
            TokenProgram::Unknown
        );
    }

    #[test]
    fn the_measured_supplies_are_both_accepted_and_odd_ones_are_not() {
        // Measured, not assumed: 99% of live launches carry a round supply and
        // the rest do not. Naming a single supply would refuse a third of them.
        for supply in STANDARD_SUPPLIES {
            let facts = MintFacts {
                supply,
                ..MintFacts::pump_standard()
            };
            assert_eq!(
                RiskFilter::new().judge(&facts),
                Verdict::Accept,
                "supply {supply} is a standard figure and must be accepted"
            );
        }
        for supply in [0_u64, 1, 992_452_789_042_372, u64::MAX] {
            let facts = MintFacts {
                supply,
                ..MintFacts::pump_standard()
            };
            assert!(
                matches!(
                    RiskFilter::new().judge(&facts),
                    Verdict::Reject(ref f) if f.contains(&RiskFlag::NonStandardSupply)
                ),
                "supply {supply} is hand-set and must be refused"
            );
        }
    }

    #[test]
    fn a_serial_creator_is_refused() {
        // Measured: one in five creators launches more than once per minute.
        let single = MintFacts::pump_standard();
        assert_eq!(RiskFilter::new().judge(&single), Verdict::Accept);

        for launches in [MAX_CREATOR_LAUNCHES + 1, 5, 50] {
            let facts = MintFacts {
                creator_launches: launches,
                ..MintFacts::pump_standard()
            };
            assert!(
                matches!(
                    RiskFilter::new().judge(&facts),
                    Verdict::Reject(ref f) if f.contains(&RiskFlag::SerialCreator)
                ),
                "{launches} launches from one creator must be refused"
            );
        }
    }

    #[test]
    fn the_reference_supply_is_the_majority_case() {
        // One billion, not two: 67% of measured launches use it. A fresh-curve
        // model that names one number should name the common one.
        assert_eq!(PUMP_SUPPLY, 1_000_000_000_000_000);
        assert_eq!(MintFacts::pump_standard().supply, PUMP_SUPPLY);
        assert_ne!(MintFacts::pump_standard().supply, 2_000_000_000_000_000);
    }
}
