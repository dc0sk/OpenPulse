//! OpenPulse modem engine.
//!
//! The [`ModemEngine`] ties a [`PluginRegistry`] and an [`AudioBackend`]
//! together to provide simple `transmit` and `receive` operations.

pub mod benchmark;
pub mod capture_replay;
pub mod capture_ticker;
pub mod channel_sim;
pub mod diagnostics;
pub mod engine;
pub mod envelope_codec;
pub mod event;
pub mod harq;
pub mod ofdm_sim;
pub mod pipeline;
mod rate_policy;
pub mod rho_calibration;

pub use engine::{ModemEngine, OtaRxResult};
pub use event::EngineEvent;
