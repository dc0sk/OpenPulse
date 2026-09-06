//! KissBridge: shared state coordinating the TCP server and modem worker.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock as StdRwLock};

use tokio::sync::broadcast;

use openpulse_core::handshake::InMemoryTrustStore;
use openpulse_core::relay::RelayForwarder;
use openpulse_modem::ModemEngine;
use openpulse_radio::shared_ptt::{SharedPtt, DEFAULT_PTT_MAX};
use openpulse_radio::PttController;

/// KissBridge configuration.
#[derive(Debug, Clone)]
pub struct KissConfig {
    /// Bind address (default `127.0.0.1`).
    pub bind_addr: String,
    /// TCP port (KISS default: 8100).
    pub port: u16,
    /// Default modem mode.
    pub mode: String,
    /// When `true`, TX data is echoed as RX without going through the modem engine.
    pub loopback: bool,
}

impl Default for KissConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1".into(),
            port: 8100,
            mode: "BPSK250".into(),
            loopback: false,
        }
    }
}

/// Shared state coordinating the per-client handlers and the modem worker.
pub struct KissBridge {
    pub engine: Arc<std::sync::Mutex<ModemEngine>>,
    /// Active modem mode string; changeable at runtime via `set_mode()`.
    pub mode: Arc<StdRwLock<String>>,
    /// Raw payloads (AX.25 frames) pushed from the worker to all connected clients.
    pub rx_data_tx: broadcast::Sender<Vec<u8>>,
    /// Raw payloads queued by clients for transmission.
    pub tx_data_tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    /// Pending TX byte count (mirrors ARDOP BUFFER tracking).
    pub tx_pending: Arc<AtomicUsize>,
    pub loopback: bool,
    /// Loaded from `trust.store_path`; empty if no path is configured.
    pub trust_store: Arc<InMemoryTrustStore>,
    /// Present when relay is enabled in the config; enforces hop-limit and dedup.
    pub relay_forwarder: Option<Arc<std::sync::Mutex<RelayForwarder>>>,
    /// Keys the rig for every emission (#1259).
    ///
    /// This crate declared `openpulse-radio` and never used it: it built no `PttController` at all,
    /// never read `[modem] ptt_backend`, and logged nothing to say so — so an operator running the
    /// APRS path with `ptt_backend = "rigctld"` played audio into an unkeyed transceiver. Only VOX
    /// worked, silently, while `[modem]` is captioned "shared by all TNC binaries".
    pub ptt: SharedPtt,
}

impl KissBridge {
    pub fn new(
        engine: ModemEngine,
        mode: String,
        loopback: bool,
    ) -> (Arc<Self>, std::sync::mpsc::Receiver<Vec<u8>>) {
        Self::with_trust_and_relay(engine, mode, loopback, Default::default(), None)
    }

    pub fn with_trust_and_relay(
        engine: ModemEngine,
        mode: String,
        loopback: bool,
        trust_store: InMemoryTrustStore,
        relay_forwarder: Option<RelayForwarder>,
    ) -> (Arc<Self>, std::sync::mpsc::Receiver<Vec<u8>>) {
        Self::with_ptt(engine, mode, loopback, trust_store, relay_forwarder, None)
    }

    /// As [`Self::with_trust_and_relay`], with the PTT controller the binary built (#1259).
    ///
    /// `None` means no PTT — VOX or a manually keyed rig. It is NOT the same as "the configured
    /// backend failed", which the binary reports separately; see #1285.
    pub fn with_ptt(
        engine: ModemEngine,
        mode: String,
        loopback: bool,
        trust_store: InMemoryTrustStore,
        relay_forwarder: Option<RelayForwarder>,
        ptt: Option<Box<dyn PttController + Send>>,
    ) -> (Arc<Self>, std::sync::mpsc::Receiver<Vec<u8>>) {
        let (rx_data_tx, _) = broadcast::channel(32);
        let (tx_data_tx, tx_data_rx) = std::sync::mpsc::sync_channel(64);
        let bridge = Arc::new(Self {
            engine: Arc::new(std::sync::Mutex::new(engine)),
            mode: Arc::new(StdRwLock::new(mode)),
            rx_data_tx,
            tx_data_tx,
            tx_pending: Arc::new(AtomicUsize::new(0)),
            loopback,
            trust_store: Arc::new(trust_store),
            relay_forwarder: relay_forwarder.map(|f| Arc::new(std::sync::Mutex::new(f))),
            ptt: SharedPtt::new(ptt, DEFAULT_PTT_MAX),
        });
        (bridge, tx_data_rx)
    }
}

/// Key the rig, emit, and release — the only way this TNC transmits (#1259).
///
/// Modelled on ARDOP's `keyed_transmit`. Two properties, both load-bearing:
///
/// * The **engine lock is taken by the caller and passed in**, so the guard drops before the caller
///   releases the mutex and the RX poll that follows a data emission never runs against a keyed rig.
///   KISS wrote its `engine.lock()` as a statement temporary inside the `transmit` expression, so a
///   `let _guard` beside it would have outlived the lock instead.
/// * A PTT assert failure **skips the emission**. Transmitting anyway is the defect this closes:
///   audio into an unkeyed transceiver.
///
/// `transmit` blocks to end-of-audio, so a guard dropped when it returns releases after the last
/// sample.
fn keyed_transmit(
    ptt: &SharedPtt,
    engine: &mut ModemEngine,
    what: &str,
    data: &[u8],
    mode: &str,
) -> bool {
    let _guard = match ptt.keyed(None) {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(what, error = %e, "PTT assert failed; skipping transmission");
            return false;
        }
    };
    match engine.transmit(data, mode, None) {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(what, error = %e, "modem TX error");
            false
        }
    }
}

impl KissBridge {
    /// Change the active modem mode at runtime.
    pub fn set_mode(&self, mode: String) {
        *self.mode.write().unwrap_or_else(|e| e.into_inner()) = mode;
    }

    /// Read the active modem mode.
    pub fn current_mode(&self) -> String {
        self.mode.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Apply a KISS control frame (any non-`DATA` type byte). Most are advisory no-ops here: TXDELAY and
/// TXtail are host-specified keying delays that this TNC does not honour (it keys and releases around
/// each emission itself — see [`KissBridge::ptt`]), P/SlotTime are CSMA-persistence/slot hints, and
/// SetHardware is TNC-specific.
///
/// This comment used to say "This TNC manages PTT and channel access itself", which was **false for
/// PTT** until #1259 — there was no PTT layer at all. The CSMA half was always true (`main.rs`
/// enables carrier sense, the only shipping front-end that does). The one
/// that maps to this TNC's real channel access is **FullDuplex** (0x05): a non-zero value selects full
/// duplex (no carrier-sense deferral → engine CSMA off); a zero value re-enables CSMA.
pub(crate) fn apply_kiss_control_frame(cmd: u8, value: &[u8], bridge: &KissBridge) {
    if cmd == crate::kiss::KISS_FULLDUPLEX {
        let full_duplex = value.first().copied().unwrap_or(0) != 0;
        let mut engine = bridge.engine.lock().unwrap_or_else(|e| e.into_inner());
        if full_duplex {
            engine.disable_csma();
        } else {
            engine.enable_csma();
        }
        tracing::debug!(full_duplex, "KISS FullDuplex applied to CSMA");
    } else {
        tracing::debug!(
            kiss_cmd = format!("0x{cmd:02x}"),
            "ignoring KISS control frame (not applied by this TNC)"
        );
    }
}

/// The AX.25 source callsign of `frame` iff it is valid for on-air TX (§97.119): a decodable address
/// header whose source is non-empty and not `N0CALL`. Returns `None` otherwise so the worker can refuse
/// unidentified frames uniformly. The address header format is common to all AX.25 frame types, so this
/// does not restrict connected-mode traffic.
fn tx_source_callsign(frame: &[u8]) -> Option<String> {
    let call = crate::ax25::Ax25Addr::source_from_frame(frame)?.callsign_str();
    openpulse_core::station_id::callsign_is_valid(&call).then_some(call)
}

/// Spawn the background worker thread that drives TX/RX via the modem engine.
pub fn spawn_worker(bridge: Arc<KissBridge>, tx_data_rx: std::sync::mpsc::Receiver<Vec<u8>>) {
    std::thread::Builder::new()
        .name("kiss-modem-worker".into())
        .spawn(move || worker_loop(bridge, tx_data_rx))
        .expect("failed to spawn kiss-modem-worker thread");
}

fn worker_loop(bridge: Arc<KissBridge>, tx_data_rx: std::sync::mpsc::Receiver<Vec<u8>>) {
    loop {
        let mode = bridge
            .mode
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        while let Ok(data) = tx_data_rx.try_recv() {
            let len = data.len();
            if bridge.loopback {
                if bridge.rx_data_tx.send(data).is_err() {
                    tracing::debug!("KISS loopback RX: no subscribers, frame dropped");
                }
            } else if tx_source_callsign(&data).is_none() {
                // §97.119: a packet station identifies via the AX.25 *source* address in each frame.
                // Refuse to key the transmitter for a frame whose source is absent, `N0CALL`, or not a
                // decodable AX.25 address header — an unidentified emission. (KISS is a bare frame pipe
                // with no host response channel, so this is logged; the frame is dropped.)
                tracing::warn!(
                    "refusing on-air TX: AX.25 source callsign missing or invalid (§97.119)"
                );
            } else {
                {
                    // Scoped so the guard inside `keyed_transmit` — and this lock — are both
                    // released before the RX poll below.
                    let mut engine = bridge.engine.lock().unwrap_or_else(|e| e.into_inner());
                    keyed_transmit(&bridge.ptt, &mut engine, "data", &data, &mode);
                }
                if let Ok(received) = bridge
                    .engine
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .receive(&mode, None)
                {
                    maybe_relay_forward(&bridge, &received, &mode);
                    if !received.is_empty() {
                        let _ = bridge.rx_data_tx.send(received);
                    }
                }
            }
            bridge.tx_pending.fetch_sub(
                len.min(bridge.tx_pending.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
        }

        if !bridge.loopback {
            if let Ok(received) = bridge
                .engine
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .receive(&mode, None)
            {
                maybe_relay_forward(&bridge, &received, &mode);
                if !received.is_empty() {
                    let _ = bridge.rx_data_tx.send(received);
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Attempt to forward `payload` as a relay `WireEnvelope` when relay is enabled.
///
/// ## Layering contract
/// `engine.receive()` returns the decoded HPX frame *payload*.  A relay sender
/// must therefore call `engine.transmit(&envelope.encode()?, …)` so that the
/// `WireEnvelope` bytes land in the HPX payload slot.  The relay receiver then
/// gets those bytes here and probes them with `WireEnvelope::decode`.
///
/// The probe is cheap: `decode` checks the 4-byte `OPHF` magic first and
/// returns `Err(InvalidMagic)` immediately for ordinary user-data payloads.
/// When the magic matches the forwarder increments `hop_index` and re-transmits.
fn maybe_relay_forward(bridge: &KissBridge, payload: &[u8], mode: &str) {
    use openpulse_core::wire_query::WireEnvelope;

    let Some(ref fwd_arc) = bridge.relay_forwarder else {
        return;
    };

    let Ok(envelope) = WireEnvelope::decode(payload) else {
        return;
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let forwarded = {
        let mut fwd = fwd_arc.lock().unwrap_or_else(|e| e.into_inner());
        fwd.forward(&envelope, now_ms)
    };

    match forwarded {
        Ok(out_envelope) => {
            if let Ok(out_bytes) = out_envelope.encode() {
                {
                    let mut engine = bridge.engine.lock().unwrap_or_else(|e| e.into_inner());
                    keyed_transmit(&bridge.ptt, &mut engine, "relay", &out_bytes, mode);
                }
                tracing::debug!(
                    session_id = out_envelope.session_id,
                    hop_index = out_envelope.hop_index,
                    "relay: forwarded envelope"
                );
            }
        }
        Err(e) => {
            tracing::debug!("relay: envelope not forwarded: {e:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ax25::{Ax25Addr, Ax25UiFrame};
    use openpulse_audio::LoopbackBackend;
    use std::time::Duration;

    /// An AX.25 UI frame with source callsign `src`.
    fn frame_from(src: &str) -> Vec<u8> {
        Ax25UiFrame {
            dest: Ax25Addr::parse("APRS").unwrap(),
            src: Ax25Addr::parse(src).unwrap(),
            info: b"hello".to_vec(),
        }
        .encode()
        .unwrap()
    }

    /// A non-loopback bridge (so TX goes through the modem, exercising the §97.119 gate).
    fn onair_bridge() -> (Arc<KissBridge>, std::sync::mpsc::Receiver<Vec<u8>>) {
        let mut engine = ModemEngine::new(Box::new(LoopbackBackend::default()));
        engine
            .register_plugin(Box::new(bpsk_plugin::BpskPlugin::new()))
            .expect("register BPSK plugin");
        KissBridge::new(engine, "BPSK250".into(), false)
    }

    #[test]
    fn kiss_fullduplex_control_frame_toggles_csma() {
        let (bridge, _rx) = onair_bridge();
        // Mirror the running TNC, which enables CSMA at startup.
        bridge
            .engine
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enable_csma();
        let csma_on = |b: &KissBridge| {
            b.engine
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_csma_enabled()
        };
        assert!(csma_on(&bridge));

        // FullDuplex with a non-zero value → full duplex → CSMA off.
        apply_kiss_control_frame(crate::kiss::KISS_FULLDUPLEX, &[1], &bridge);
        assert!(
            !csma_on(&bridge),
            "full-duplex disables carrier-sense deferral"
        );

        // FullDuplex value 0 → half duplex → CSMA back on.
        apply_kiss_control_frame(crate::kiss::KISS_FULLDUPLEX, &[0], &bridge);
        assert!(csma_on(&bridge), "half-duplex re-enables CSMA");

        // Other control frames (e.g. TXDELAY 0x01) are no-ops that leave CSMA untouched.
        apply_kiss_control_frame(0x01, &[50], &bridge);
        assert!(csma_on(&bridge), "TXDELAY does not affect CSMA");
    }

    #[test]
    fn tx_source_callsign_gates_on_the_ax25_source() {
        assert_eq!(
            tx_source_callsign(&frame_from("W1AW-9")).as_deref(),
            Some("W1AW"),
            "a valid source call is usable for TX"
        );
        assert!(
            tx_source_callsign(&frame_from("N0CALL")).is_none(),
            "placeholder source is refused"
        );
        assert!(
            tx_source_callsign(&frame_from("")).is_none(),
            "empty source is refused"
        );
        assert!(
            tx_source_callsign(&[0u8; 8]).is_none(),
            "a frame too short for an address header is refused"
        );
    }

    #[test]
    fn worker_refuses_invalid_source_but_passes_a_valid_one() {
        let (bridge, rx) = onair_bridge();
        let tx = bridge.tx_data_tx.clone();
        spawn_worker(bridge.clone(), rx);

        // Queued in order: the N0CALL frame must be refused, the valid one transmitted. Waiting for the
        // valid frame to go out proves *both* were processed (in order), so `frames == 1` shows the
        // N0CALL frame was dropped rather than merely not-yet-reached.
        tx.send(frame_from("N0CALL")).expect("queue invalid frame");
        tx.send(frame_from("W1AW-9")).expect("queue valid frame");

        let mut frames = 0;
        for _ in 0..200 {
            frames = bridge
                .engine
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .frames_transmitted();
            if frames > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            frames, 1,
            "exactly the valid-source frame is transmitted; the N0CALL frame is refused (§97.119)"
        );
    }
}
