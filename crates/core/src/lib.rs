//! 🦅 sniper-core — the trading brain.
//!
//! The crate is organised along the latency pipeline so that a slow stage is
//! always attributable to one module:
//!
//! ```text
//!   stream  ── detect ──►  decode  ──►  risk  ──►  exec  ──►  position
//!   gRPC bytes            venue::decode   filters   fan-out   TP/SL/trailing
//!   (Chapter 7)           (Chapter 2)     (Ch. 6)   (Ch. 5)   (Chapter 8)
//!                                    │
//!                              venue::price / build
//!                                  (Ch. 3, 4)
//! ```

pub mod config;
pub mod curve;
pub mod pricing;
pub mod risk;
pub mod stream;
pub mod venue;

pub use config::{Config, ConfigError, PositionSizing, RuntimeMode, LIVE_POSITION_CAP_LAMPORTS};
pub use curve::CurveState;
pub use pricing::{CurveReserves, FeeBps, QuoteError};
pub use risk::{MintFacts, RiskFilter, Verdict};
pub use stream::logs::{PumpActivity, PUMP_PROGRAM_ID};
pub use venue::pumpfun::{PumpFunDecoder, RawInstruction, RawTransaction};
pub use venue::{LaunchEvent, RiskFlag, Venue, VenueKind};

pub use sniper_telemetry::{Metrics, Stage};
