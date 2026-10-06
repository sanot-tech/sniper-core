//! 📡 Live feeds.
//!
//! Currently one: the WebSocket log subscription, which is the cheapest feed
//! that can identify a launch without a paid endpoint. See [`logs`].

pub mod logs;

pub use logs::{LogNotification, PumpActivity, TxLogs, PUMP_PROGRAM_ID};
