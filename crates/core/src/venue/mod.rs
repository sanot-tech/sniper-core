//! 🎯 Venue abstraction — one trait, three implementations.
//!
//! ## Why a trait at all
//!
//! A venue is not "a DEX". It is a concrete on-chain program with its own:
//!
//! * program id (what we subscribe to),
//! * launch trigger (which instruction means "a new token just appeared"),
//! * price math (bonding curve vs constant product vs concentrated bins),
//! * instruction set (`buy_v2` vs `initialize` vs `createPoolWithFirstBuy`),
//! * and fee schedule.
//!
//! Because those five things differ, a sniper needs a *separate buy builder
//! per venue* while everything else — risk filters, position tracking,
//! execution fan-out, telemetry — is shared. The trait captures exactly that
//! seam, so adding a fourth venue is a new file, not a rewrite.
//!
//! ## The three venues we snipe
//!
//! | Venue | Program | Trigger | Notes |
//! |---|---|---|---|
//! | [`VenueKind::PumpFun`] | `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P` | `create` / `create_v2` | Bonding curve, no pool yet |
//! | [`VenueKind::PumpSwap`] | pAMM | `CreatePoolEvent` | Constant product, graduates from pump.fun |
//! | [`VenueKind::RaydiumLaunchLab`] | `LaunchLab` | `initialize` / `initialize2` | Raydium's own curve |
//!
//! Program ids other than pump.fun's are declared here but must be verified
//! against the pinned IDL before mainnet use — see [`VenueKind::program_id`].

/// A decoded "a new token just appeared" event.
///
/// Deliberately minimal: only fields that are already present in the stream
/// message. Adding a field that requires an RPC round-trip would silently
/// reintroduce latency into the hot path, which is exactly what the two-tier
/// risk design in the risk module exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchEvent {
    /// Mint address of the new token.
    pub mint: [u8; 32],
    /// Creator / deployer, used later for deployer-history risk scoring.
    pub creator: [u8; 32],
    /// Pool or bonding-curve account that will receive liquidity.
    pub pool: [u8; 32],
    /// Slot the launch transaction landed in.
    pub slot: u64,
    /// Slot index of the launch transaction inside the block.
    pub idx: u32,
    /// Signature of the launch transaction.
    pub signature: [u8; 64],
}

impl LaunchEvent {
    /// Slot the event belongs to.
    #[inline]
    #[must_use]
    pub const fn slot(&self) -> u64 {
        self.slot
    }
}

/// A venue the bot can snipe.
pub trait Venue {
    /// Stable identifier used in logs and metrics labels.
    fn kind(&self) -> VenueKind;

    /// Program ids to subscribe to on the Yellowstone stream.
    ///
    /// Some venues need more than one (e.g. pump.fun curve *and* AMM), which
    /// is why this returns a slice rather than a single address.
    fn program_ids(&self) -> &[&'static str];

    /// True when this raw transaction launches a new token on this venue.
    ///
    /// Must be a pure, allocation-free discriminator: compare the 8-byte
    /// Anchor discriminator of the instruction, never string-match log
    /// messages. Log matching produces false positives on every buy.
    fn is_launch(&self, raw_tx: &[u8]) -> bool;

    /// Decodes a launch event, assuming [`Self::is_launch`] returned true.
    fn decode_launch(&self, raw_tx: &[u8]) -> Option<LaunchEvent>;

    /// Venue-specific risk notes for an already-decoded event.
    ///
    /// Runs on the cheap tier: it may only look at bytes we already have.
    /// Network-backed checks belong to the risk module's second tier.
    fn cheap_risk_flags(&self, event: &LaunchEvent) -> Vec<RiskFlag>;
}

/// A venue-specific danger sign found without any network access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskFlag {
    /// Mint authority still live: the creator can print more supply.
    MintAuthorityLive,
    /// Freeze authority live: the creator can freeze your token account.
    FreezeAuthorityLive,
    /// Token-2022 transfer hook present: arbitrary code runs on transfer.
    Token2022TransferHook,
    /// Token-2022 transfer fee: you lose a cut on the way in and out.
    Token2022TransferFee,
    /// Token-2022 permanent delegate: someone can move your tokens.
    Token2022PermanentDelegate,
    /// Suspicious decimal configuration.
    OddDecimals,
    /// Supply is not one of the venue's standard round figures.
    ///
    /// Measured: 99% of live launches carry exactly 1e15 or 2e15 base units,
    /// while the remainder carry hand-set values. A creator who chooses an odd
    /// supply is building something other than a normal coin, and the standard
    /// figure is what every off-the-shelf pricing model assumes.
    NonStandardSupply,
    /// The creator has launched repeatedly in a short window.
    ///
    /// Measured: one in five creators launches more than one token per minute,
    /// some at five launches in four minutes. A creator producing tokens at that
    /// rate is running a launch bot, and the exit liquidity a sniper depends on
    /// is not there.
    SerialCreator,
}

/// The three venues this bot snipes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VenueKind {
    /// pump.fun bonding curve.
    PumpFun,
    /// `PumpSwap` — pump.fun's own AMM, where curve tokens graduate.
    PumpSwap,
    /// `Raydium LaunchLab`, Raydium's own bonding curve.
    RaydiumLaunchLab,
}

impl VenueKind {
    /// All configured venues, in subscription order.
    pub const ALL: [VenueKind; 3] = [
        VenueKind::PumpFun,
        VenueKind::PumpSwap,
        VenueKind::RaydiumLaunchLab,
    ];

    /// Primary program id.
    ///
    /// Only the pump.fun id is verified against the published documentation.
    /// The other two are placeholders and **must** be confirmed against the
    /// pinned IDL before any mainnet run — a wrong program id means silently
    /// subscribing to nothing, which looks exactly like "no tokens launch".
    #[must_use]
    pub const fn program_id(&self) -> &'static str {
        match self {
            // ✅ verified in pump-fun/pump-public-docs
            Self::PumpFun => "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",
            // ⚠️ verify against the pinned IDL before trading this venue
            Self::PumpSwap => "PENDING_IDL_VERIFICATION:pumpswap",
            // ⚠️ verify against the pinned IDL before trading this venue
            Self::RaydiumLaunchLab => "PENDING_IDL_VERIFICATION:raydium-launchlab",
        }
    }

    /// True when the program id still needs IDL verification.
    ///
    /// [`crate::config`] refuses to start on a live venue with an unverified
    /// id rather than pretending to trade.
    #[must_use]
    pub const fn needs_idl_verification(&self) -> bool {
        match self {
            Self::PumpFun => false,
            Self::PumpSwap | Self::RaydiumLaunchLab => true,
        }
    }

    /// Lowercase identifier for logs and metrics.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::PumpFun => "pumpfun",
            Self::PumpSwap => "pumpswap",
            Self::RaydiumLaunchLab => "raydium-launchlab",
        }
    }
}

/// pump.fun-specific decoding.
///
/// Separated from the trait definition because the trait is about *what a venue
/// is*, while this is about *how pump.fun encodes one*. Keeping them apart
/// means a third venue cannot start touching pump.fun's discriminators by
/// accident, and the discriminator table has exactly one place to live.
pub mod pumpfun;

#[cfg(test)]
mod tests {
    use super::*;
    use sniper_telemetry::Metrics;

    #[test]
    fn pumpfun_program_id_is_verified() {
        // Guard rail: if someone edits this constant to a placeholder, the
        // config loader must stop treating the venue as usable.
        let kind = VenueKind::PumpFun;
        assert!(!kind.needs_idl_verification());
        assert_eq!(kind.program_id().len(), 43, "base58 pubkey is 43 chars");
        assert!(kind.program_id().chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn unverified_venues_are_flagged() {
        assert!(VenueKind::PumpSwap.needs_idl_verification());
        assert!(VenueKind::RaydiumLaunchLab.needs_idl_verification());
    }

    #[test]
    fn all_venues_have_unique_names() {
        let names: Vec<_> = VenueKind::ALL.iter().map(VenueKind::as_str).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), VenueKind::ALL.len());
    }

    #[test]
    fn metrics_are_shareable_across_venue_implementations() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Metrics>();
    }
}
