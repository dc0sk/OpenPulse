//! KISS-over-TCP TNC server for APRS and AX.25 applications.
//!
//! Exposes a single TCP port (default 8100) that APRS clients can connect to.
//! Uses KISS byte-stuffed framing with AX.25 UI frames as the payload.
//!
//! **Station identification (audit G-5):** unlike the daemon and ARDOP TX paths, the KISS TNC runs no
//! separate periodic auto-ID timer (`StationIdTimer`). This is by design — every AX.25 frame carries
//! the source callsign in its address field ([`ax25::Ax25Addr`]), so a station transmitting AX.25/APRS
//! is self-identifying on every transmission and satisfies §97.119 without a distinct ID cycle. The
//! operator remains responsible for configuring a valid source callsign.

mod bridge;
mod error;
mod server;

pub mod ax25;
pub mod kiss;

pub use bridge::{spawn_worker, KissBridge, KissConfig};
pub use error::KissTncError;

use std::sync::Arc;

use tokio::net::TcpListener;

use openpulse_modem::ModemEngine;

/// KISS TNC server.
pub struct KissServer {
    bridge: Arc<KissBridge>,
    /// Kept here until `run_with_listener` is called so the worker is only
    /// spawned when the server actually starts, avoiding a thread leak on
    /// construction.
    tx_data_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    config: KissConfig,
}

impl KissServer {
    pub fn new(engine: ModemEngine, config: KissConfig) -> Self {
        Self::with_trust_and_relay(engine, config, Default::default(), None)
    }

    /// Create a server with a pre-loaded trust store and optional relay forwarder.
    pub fn with_trust_and_relay(
        engine: ModemEngine,
        config: KissConfig,
        trust_store: openpulse_core::handshake::InMemoryTrustStore,
        relay_forwarder: Option<openpulse_core::relay::RelayForwarder>,
    ) -> Self {
        Self::with_ptt(engine, config, trust_store, relay_forwarder, None)
    }

    /// As [`Self::with_trust_and_relay`], with the PTT controller the binary built (#1259).
    pub fn with_ptt(
        engine: ModemEngine,
        config: KissConfig,
        trust_store: openpulse_core::handshake::InMemoryTrustStore,
        relay_forwarder: Option<openpulse_core::relay::RelayForwarder>,
        ptt: Option<Box<dyn openpulse_radio::PttController + Send>>,
    ) -> Self {
        let (bridge, tx_data_rx) = KissBridge::with_ptt(
            engine,
            config.mode.clone(),
            config.loopback,
            trust_store,
            relay_forwarder,
            ptt,
        );
        Self {
            bridge,
            tx_data_rx: Some(tx_data_rx),
            config,
        }
    }

    /// Returns a handle to the shared bridge (useful for testing).
    pub fn bridge(&self) -> Arc<KissBridge> {
        self.bridge.clone()
    }

    /// Run the KISS TCP listener until it stops.
    pub async fn run(self) -> Result<(), KissTncError> {
        let addr = format!("{}:{}", self.config.bind_addr, self.config.port);
        let listener = TcpListener::bind(&addr).await?;
        self.run_with_listener(listener).await
    }

    /// Run using a pre-bound listener (useful for tests that need port 0).
    pub async fn run_with_listener(mut self, listener: TcpListener) -> Result<(), KissTncError> {
        spawn_worker(
            self.bridge.clone(),
            self.tx_data_rx.take().expect("tx_data_rx already consumed"),
        );
        server::serve(listener, self.bridge).await
    }
}
