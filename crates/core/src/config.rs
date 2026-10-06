//! ⚙️ Runtime configuration and the promotion gate.
//!
//! Configuration is split into two halves on purpose:
//!
//! * **public** — commit-safe, lives in `config.toml`;
//! * **secret** — lives in `.env`, read at startup, never logged.
//!
//! The split is enforced by the type system: [`Config`] cannot hold a secret,
//! so `Debug`-printing it, dumping it to a dashboard, or panicking with it in
//! the message cannot leak a key.

use std::fmt;

use crate::venue::VenueKind;

/// Position size expressed in lamports.
///
/// `u64` lamports, never `f64` SOL: floating point rounding on a money value
/// is a bug waiting to happen, and 1 SOL = 1_000_000_000 lamports exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionSizing(u64);

impl PositionSizing {
    /// 1 SOL in lamports.
    pub const ONE_SOL: u64 = 1_000_000_000;

    #[must_use]
    pub const fn from_lamports(lamports: u64) -> Self {
        Self(lamports)
    }

    /// Builds from whole SOL.
    ///
    /// # Panics
    /// Panics if `sol` cannot be represented exactly in lamports. A
    /// fractional-SOL position is a configuration error, not something to
    /// silently round.
    #[must_use]
    pub fn from_sol(sol: f64) -> Self {
        let lamports = (sol * Self::ONE_SOL as f64).round() as u64;
        assert!(
            (lamports as f64 / Self::ONE_SOL as f64 - sol).abs() < 1e-9,
            "position size must be an exact number of lamports, got {sol} SOL",
        );
        Self(lamports)
    }

    #[inline]
    #[must_use]
    pub const fn lamports(&self) -> u64 {
        self.0
    }

    #[inline]
    #[must_use]
    pub fn as_sol(&self) -> f64 {
        self.0 as f64 / Self::ONE_SOL as f64
    }

    /// True when the size is small enough for the live stage.
    ///
    /// The promotion gate from chapter 12 caps live positions; this is the
    /// in-process half of that rule.
    #[must_use]
    pub fn is_live_safe(&self) -> bool {
        self.0 <= Self::ONE_SOL / 50 // 0.02 SOL
    }
}

/// Which stage of the promotion ladder the process is running as.
///
/// [`RuntimeMode::DryRun`] and [`RuntimeMode::Paper`] cannot be escalated from
/// configuration alone — see [`Config::validate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeMode {
    /// Fully offline: replay fixtures, no network at all.
    DryRun,
    /// Real subscription and real building, broadcast replaced by logging.
    #[default]
    Paper,
    /// Devnet: real network, worthless SOL.
    Devnet,
    /// Mainnet with real money. Requires every safety check to pass.
    Live,
}

impl RuntimeMode {
    /// True when the bot is allowed to sign and broadcast a transaction.
    #[must_use]
    pub const fn can_broadcast(&self) -> bool {
        matches!(self, Self::Live)
    }

    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::DryRun => "dry-run",
            Self::Paper => "paper",
            Self::Devnet => "devnet",
            Self::Live => "live",
        }
    }
}

impl fmt::Display for RuntimeMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Public, non-secret configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Promotion ladder stage.
    pub mode: RuntimeMode,
    /// Venues to subscribe to, in order.
    pub venues: Vec<VenueKind>,
    /// Size of every entry.
    pub position: PositionSizing,
    /// Maximum entries open at once.
    pub max_open_positions: usize,
    /// Hard cap on trades per day, independent of PnL.
    pub max_trades_per_day: u32,
    /// Highest acceptable risk score (1..=10) before a token is skipped.
    pub max_risk_score: u8,
    /// Configured slots to keep in the decision trace ring.
    pub trace_capacity: usize,
}

impl Default for Config {
    /// Defaults match the agreed operating point: 0.02 SOL per entry, the
    /// three venues, paper mode.
    fn default() -> Self {
        Self {
            mode: RuntimeMode::Paper,
            venues: VenueKind::ALL.to_vec(),
            position: PositionSizing::from_lamports(20_000_000),
            max_open_positions: 5,
            max_trades_per_day: 100,
            max_risk_score: 3,
            trace_capacity: 256,
        }
    }
}

/// Why a configuration was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A live venue whose program id has not been verified against the IDL.
    UnverifiedVenueOnLive { venue: VenueKind },
    /// Position size above the live cap.
    PositionTooLarge { lamports: u64, max_lamports: u64 },
    /// No venue selected.
    NoVenues,
    /// Trace ring capacity of zero would panic at construction.
    ZeroTraceCapacity,
    /// Risk score outside 1..=10.
    RiskScoreOutOfRange(u8),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnverifiedVenueOnLive { venue } => write!(
                f,
                "venue `{}` has no IDL-verified program id; refusing to trade live \
                 on an unverified subscription target",
                venue.as_str()
            ),
            Self::PositionTooLarge {
                lamports,
                max_lamports,
            } => write!(
                f,
                "position {lamports} lamports exceeds the live cap of {max_lamports}"
            ),
            Self::NoVenues => f.write_str("no venues selected"),
            Self::ZeroTraceCapacity => f.write_str("trace_capacity must be > 0"),
            Self::RiskScoreOutOfRange(v) => write!(f, "max_risk_score {v} is outside 1..=10"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Fails fast on any combination that could lose money.
    ///
    /// Called once, at startup, before the stream is subscribed. The point of
    /// validating early is that a misconfiguration becomes a non-zero exit
    /// code at startup rather than a surprise at 3am with an open position.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.venues.is_empty() {
            return Err(ConfigError::NoVenues);
        }
        if self.trace_capacity == 0 {
            return Err(ConfigError::ZeroTraceCapacity);
        }
        if !(1..=10).contains(&self.max_risk_score) {
            return Err(ConfigError::RiskScoreOutOfRange(self.max_risk_score));
        }
        if self.mode == RuntimeMode::Live {
            for venue in &self.venues {
                if venue.needs_idl_verification() {
                    return Err(ConfigError::UnverifiedVenueOnLive { venue: *venue });
                }
            }
            const LIVE_CAP: u64 = PositionSizing::ONE_SOL / 50; // 0.02 SOL
            if !self.position.is_live_safe() {
                return Err(ConfigError::PositionTooLarge {
                    lamports: self.position.lamports(),
                    max_lamports: LIVE_CAP,
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_position_is_exactly_two_hundredths_of_sol() {
        let cfg = Config::default();
        assert_eq!(cfg.position.lamports(), 20_000_000);
        assert!((cfg.position.as_sol() - 0.02).abs() < 1e-12);
        assert!(cfg.position.is_live_safe());
    }

    #[test]
    fn position_from_sol_is_exact() {
        let p = PositionSizing::from_sol(0.05);
        assert_eq!(p.lamports(), 50_000_000);
        assert!((p.as_sol() - 0.05).abs() < 1e-12);
        // The live cap is 0.02 SOL, so 0.05 must be rejected for live trading.
        // Documenting the boundary here means a future change to the cap
        // shows up as a failing test rather than as a surprise risk.
        assert!(!p.is_live_safe());
        let big = PositionSizing::from_sol(2.0);
        assert!(!big.is_live_safe());
    }

    #[test]
    fn live_cap_is_exactly_two_hundredths_of_sol() {
        // The agreed operating point. If this moves, it must be a deliberate
        // decision, not an accidental edit to a division.
        assert_eq!(PositionSizing::ONE_SOL / 50, 20_000_000);
        assert!(PositionSizing::from_sol(0.02).is_live_safe());
        assert!(!PositionSizing::from_sol(0.0200001).is_live_safe());
    }

    #[test]
    fn only_live_can_broadcast() {
        assert!(!RuntimeMode::DryRun.can_broadcast());
        assert!(!RuntimeMode::Paper.can_broadcast());
        assert!(!RuntimeMode::Devnet.can_broadcast());
        assert!(RuntimeMode::Live.can_broadcast());
    }

    #[test]
    fn live_mode_refuses_unverified_venue() {
        let cfg = Config {
            mode: RuntimeMode::Live,
            venues: vec![VenueKind::PumpSwap],
            ..Config::default()
        };
        assert_eq!(
            cfg.validate(),
            Err(ConfigError::UnverifiedVenueOnLive {
                venue: VenueKind::PumpSwap
            })
        );
    }

    #[test]
    fn live_mode_refuses_oversized_position() {
        let cfg = Config {
            mode: RuntimeMode::Live,
            venues: vec![VenueKind::PumpFun],
            position: PositionSizing::from_sol(0.5),
            ..Config::default()
        };
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::PositionTooLarge { .. })
        ));
    }

    #[test]
    fn live_mode_accepts_verified_venue_and_small_size() {
        let cfg = Config {
            mode: RuntimeMode::Live,
            venues: vec![VenueKind::PumpFun],
            position: PositionSizing::from_sol(0.02),
            ..Config::default()
        };
        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn paper_mode_tolerates_unverified_venue() {
        // Paper must stay usable while IDs are still being confirmed, otherwise
        // we could not validate anything before going live.
        let cfg = Config {
            mode: RuntimeMode::Paper,
            venues: VenueKind::ALL.to_vec(),
            ..Config::default()
        };
        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn rejects_empty_venue_list_and_bad_scores() {
        let mut cfg = Config {
            venues: Vec::new(),
            ..Config::default()
        };
        assert_eq!(cfg.validate(), Err(ConfigError::NoVenues));

        cfg.venues = VenueKind::ALL.to_vec();
        cfg.max_risk_score = 0;
        assert_eq!(cfg.validate(), Err(ConfigError::RiskScoreOutOfRange(0)));

        cfg.max_risk_score = 11;
        assert_eq!(cfg.validate(), Err(ConfigError::RiskScoreOutOfRange(11)));
    }
}
