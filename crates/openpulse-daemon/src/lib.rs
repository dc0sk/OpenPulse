//! NDJSON-over-TCP control server for the OpenPulse daemon.
//!
//! [`ControlServer::spawn`] binds a TCP listener and accepts one or more
//! concurrent client connections.  Each client receives the full unsolicited
//! [`ControlEvent`] stream and may send [`ControlCommand`] lines which are
//! dispatched back to the caller via an `mpsc` channel.
//!
//! Clients that send [`ControlCommand::SubscribeSpectrum`] receive binary
//! spectrum frames interleaved with the NDJSON event stream on the same
//! connection.  See [`protocol::encode_spectrum_frame`] for the wire format.

pub mod audit;
pub mod logbook;
pub mod monitor;
pub mod protocol;
pub mod ptt;

#[cfg(not(target_arch = "wasm32"))]
pub mod filexfer;

/// WebSocket control endpoint — native server builds only.
#[cfg(not(target_arch = "wasm32"))]
pub mod ws;

/// Daemon run loop (extracted from the `openpulse-server` binary) — native only.
#[cfg(not(target_arch = "wasm32"))]
pub mod server;

/// Twin-station rig: two real daemons bridged through a channel — native only.
#[cfg(not(target_arch = "wasm32"))]
pub mod twin;

#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::net::SocketAddr;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(not(target_arch = "wasm32"))]
use openpulse_channel::dsp::PowerSpectrum;
#[cfg(not(target_arch = "wasm32"))]
use openpulse_core::handshake::{
    verify_conack, verify_conreq, ConAck, ConAckParams, ConReq, ConReqParams, Freshness,
    InMemoryTrustStore, TrustStore,
};
#[cfg(not(target_arch = "wasm32"))]
use openpulse_core::session_key::{derive_ack_key, generate_kex_ephemeral};

/// Maximum clock skew tolerated when verifying a handshake's signed timestamp (replay-freshness).
/// Generous relative to typical HF turnaround (handshakes complete in seconds) while bounding the
/// capture-replay window; assumes both stations keep roughly correct wall-clock time.
#[cfg(not(target_arch = "wasm32"))]
const HANDSHAKE_MAX_SKEW_MS: u64 = 120_000;

/// Current wall-clock time in Unix milliseconds.
#[cfg(not(target_arch = "wasm32"))]
fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
#[cfg(not(target_arch = "wasm32"))]
use openpulse_core::relay::RelayForwarder;
#[cfg(not(target_arch = "wasm32"))]
use openpulse_core::sar::{sar_encode, SarReassembler};
use openpulse_core::trust::{
    classify_connection_trust, CertificateSource, PolicyProfile, PublicKeyTrustLevel, SigningMode,
};
#[cfg(not(target_arch = "wasm32"))]
use openpulse_modem::engine::SecureSessionParams;
#[cfg(not(target_arch = "wasm32"))]
use openpulse_modem::ModemEngine;
#[cfg(not(target_arch = "wasm32"))]
use openpulse_qsy::frame::{
    decode_signed as decode_qsy_frame, encode_signed as encode_qsy_frame, QsyFrame,
};
#[cfg(not(target_arch = "wasm32"))]
use openpulse_qsy::session::{QsyAction, QsyPolicy, QsySession};
#[cfg(not(target_arch = "wasm32"))]
use openpulse_qsy::ConnectionTrustLevel;
#[cfg(not(target_arch = "wasm32"))]
use openpulse_radio::CatController;
#[cfg(not(target_arch = "wasm32"))]
use openpulse_repeater::CrossBandRepeater;
#[cfg(not(target_arch = "wasm32"))]
use protocol::{
    encode_spectrum_frame, CommandResponse, ControlCommand, ControlEvent, MessageSummary,
};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(not(target_arch = "wasm32"))]
use tokio::net::{TcpListener, TcpStream};
#[cfg(not(target_arch = "wasm32"))]
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};

pub use protocol::ControlCommand as Command;
pub use protocol::ControlEvent as Event;

/// Live engine metrics shared between the main loop and the periodic metrics task.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub struct MetricsSnapshot {
    pub afc_correction_hz: f32,
    /// Cumulative bytes decoded from the RF receive path.
    pub total_rx_bytes: u64,
    /// Smoothed (EWMA) modem receive-path decode latency in milliseconds.
    pub decode_latency_ms: f32,
    /// Cumulative raw bytes of decoded RX payloads measured for compressibility.
    pub raw_payload_bytes: u64,
    /// Cumulative best-effort compressed size of those payloads (the session LZ4/zstd compressor,
    /// including its framing overhead; never larger than raw). The ratio `compressed / raw` is the
    /// live compression figure reported in `ControlEvent::Metrics`.
    pub compressed_payload_bytes: u64,
    /// Front-end toggle state, refreshed from the engine by the main loop (#1276).
    ///
    /// This struct is daemon-internal and `Default`-constructed in one place, so extending it is
    /// safe — unlike the wire `ControlEvent::Metrics`, which the panel destructures exhaustively
    /// and two other sites construct. The state has to travel through here because the periodic
    /// metrics task holds no engine; the main loop does, and already writes this snapshot.
    pub front_end: FrontEndState,
}

/// The five front-end toggles, as last read FROM THE ENGINE — not a shadow of the commands sent.
///
/// `dcd_squelch` is a threshold rather than a toggle, but it shares the write-only defect and the
/// same fix, so it travels with them.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Default, Clone, Copy, PartialEq)]
pub struct FrontEndState {
    pub notch: bool,
    pub agc: bool,
    pub cessb: bool,
    pub logbook: bool,
    pub dcd_squelch: f32,
}

/// Compression ratio (compressed / raw) of the measured payload stream, or `None` before any payload
/// has been seen. Matches the `ControlEvent::Metrics.compress_ratio` convention (compressed / raw).
fn compression_ratio(raw: u64, compressed: u64) -> Option<f32> {
    (raw > 0).then(|| compressed as f32 / raw as f32)
}

#[cfg(not(target_arch = "wasm32"))]
type SharedMetrics = Arc<Mutex<MetricsSnapshot>>;

/// Sample the daemon process's CPU and memory load. Returns
/// `(cpu_percent, ram_mib, ram_percent_of_total)`, where CPU is the conventional process
/// reading (100% = one core fully used; may exceed 100% for a multi-threaded process).
#[cfg(not(target_arch = "wasm32"))]
fn sample_process_resources(sys: &mut sysinfo::System, pid: sysinfo::Pid) -> (f32, f32, f32) {
    sys.refresh_memory();
    sys.refresh_process(pid);
    let total = sys.total_memory().max(1) as f32;
    match sys.process(pid) {
        Some(p) => {
            let rss = p.memory() as f32;
            (
                p.cpu_usage().max(0.0),
                rss / (1024.0 * 1024.0),
                (rss / total * 100.0).clamp(0.0, 100.0),
            )
        }
        None => (0.0, 0.0, 0.0),
    }
}

/// Best-effort system GPU utilisation (0–100). Queries NVIDIA via `nvidia-smi`; on a host with
/// no such tool (or a non-NVIDIA GPU) it marks `available` false so subsequent ticks don't keep
/// spawning a failing process, and returns `None`.
#[cfg(not(target_arch = "wasm32"))]
fn read_gpu_utilization(available: &mut bool) -> Option<f32> {
    if !*available {
        return None;
    }
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=utilization.gpu",
            "--format=csv,noheader,nounits",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .next()
            .and_then(|l| l.trim().parse::<f32>().ok()),
        _ => {
            *available = false;
            None
        }
    }
}

/// Mutable daemon runtime state touched by side-effectful control commands.
#[cfg(not(target_arch = "wasm32"))]
pub struct RuntimeControlState {
    pub repeater_enabled: bool,
    pub qsy_decisions: HashMap<String, bool>,
    pub qsy_pending_token: Option<String>,
    /// Active QSY negotiation session (present after operator accepts a pending token).
    pub qsy_session: Option<QsySession>,
    /// When the current QSY session was created, for TTL expiry. `None` whenever `qsy_session` is.
    pub qsy_session_started: Option<Instant>,
    /// QSY lines refused for failing authentication or freshness — a tripwire, so a build where the
    /// verification silently stopped running is visible rather than merely quiet.
    pub qsy_lines_refused: u64,
    /// Ed25519 key of the peer this QSY negotiation is bound to, pinned when the session is created
    /// (#1252).
    ///
    /// Pinned rather than resolved per line. `handle_inbound_conreq` runs a permissive profile and
    /// answers any CONREQ addressed to our (public) callsign, and `record_verified_peer` then
    /// overwrites `last_verified_callsign` — so a third station can displace the link peer
    /// mid-negotiation with one CONREQ, and a per-line lookup would start accepting the displacer's
    /// signatures on an in-flight session.
    pub qsy_peer_pubkey: Option<[u8; 32]>,
    /// Candidate frequencies (Hz) supplied from config for QSY scanning.
    pub qsy_candidate_freqs: Vec<u64>,
    /// QSY policy parsed from config; governs which requests are accepted.
    pub qsy_policy: QsyPolicy,
    /// Dwell time per frequency during a QSY scan (milliseconds).
    pub qsy_scan_dwell_ms: u64,
    /// Switchover offset (seconds) encoded in outgoing QSY_ACK frames.
    pub qsy_switchover_offset_s: u32,
    /// Pre-built cross-band repeater; taken and moved into a thread by EnableRepeater.
    pub repeater: Option<CrossBandRepeater>,
    /// Stop flag for the running repeater thread.
    pub repeater_stop: Option<Arc<AtomicBool>>,
    /// Handle for the running repeater thread.
    pub repeater_thread: Option<std::thread::JoinHandle<()>>,
    /// PTT hardware + watchdog deadline behind a shared lock, so an independent watchdog thread can
    /// force-release the transmitter even while the async command loop is blocked in a long handler
    /// (issue #863). Keyed ⇔ the deadline is armed; the default max keyed duration is 180 s (Part 97).
    pub ptt: crate::ptt::SharedPtt,
    /// Loaded trust store for verifying incoming peer handshakes.
    pub trust_store: InMemoryTrustStore,
    /// Optional relay forwarder; `Some` when `[relay] enabled = true` in config.
    pub relay_forwarder: Option<RelayForwarder>,
    /// Fallback DCD/squelch RMS threshold when no per-band override matches.
    pub dcd_squelch_default: f32,
    /// Per-band DCD/squelch overrides (band label → threshold), applied on retune.
    pub dcd_squelch_bands: std::collections::BTreeMap<String, f32>,
    /// Global TX attenuation (dB) applied when no per-band override matches the current band.
    pub tx_attenuation_default: f32,
    /// Per-band TX attenuation overrides (band label → dB), set by `SetTxAttenuation { band }` and
    /// re-applied on retune.
    pub tx_attenuation_bands: std::collections::BTreeMap<String, f32>,
    /// Automatic ADIF logbook (opt-in); records one QSO per connect→disconnect.
    pub logbook: crate::logbook::Logbook,
    /// Most recent CAT frequency (Hz) set via `SetFreq`, stamped into the logbook QSO.
    pub last_freq_hz: Option<u64>,
    /// 32-byte Ed25519 seed identifying this station; signs outgoing CONREQ/CONACK frames.
    pub station_seed: [u8; 32],
    /// Local callsign advertised as the handshake `station_id`.
    pub local_callsign: String,
    /// Local Maidenhead grid advertised in the handshake (empty = not advertised).
    pub local_grid: String,
    /// Outstanding CONREQ awaiting a CONACK (initiator role); `None` when idle.
    pub pending_handshake: Option<PendingHandshake>,
    /// Callsign of the most recently verified peer this session — a pointer into
    /// [`verified_peers`](Self::verified_peers), not a second copy of the identity. Used by the
    /// OTA-suppress and QSY-trust reads, which operate on the current link (the QSY frame carries no
    /// callsign or signature, so there is no per-requester identity to bind — this is the best
    /// available signal). Per-sender identity binding (file-transfer offer verification) looks the
    /// *offer's own sender* up in the map directly.
    pub last_verified_callsign: Option<String>,
    /// All peers verified this session, keyed by callsign — the authoritative verified-identity store,
    /// so a signed file-transfer offer is checked against *its own sender's* key rather than whoever
    /// handshook most recently (audit E5).
    pub verified_peers: std::collections::HashMap<String, VerifiedPeer>,
    /// Reassembles inbound SAR-fragmented handshake frames (CONREQ/CONACK exceed one modem frame).
    pub handshake_sar: SarReassembler,
    /// Our active OTA rate-ladder identity `(profile_name, fingerprint)`, set at OTA startup. Used to
    /// advertise our ladder in the handshake and to detect a diverged peer ladder. `None` = no OTA.
    pub local_ota_ladder: Option<(String, u64)>,
    /// Compress fixed-mode `SendMessage` payloads before transmission (`[compression] enabled`). The OTA
    /// path is packed in `server::run`; this covers the non-OTA transmit inside `apply_command_to_engine`.
    pub compress_tx: bool,
    /// Reassembles inbound `OPFX` file-transfer control frames (segment-id `0xFFFF`); block-data
    /// fragments (segment-id `block_index + 1`) are reassembled inside the active receive session.
    pub filexfer_sar: SarReassembler,
    /// Tripwire: number of inbound `OPFX` frames routed to the file-transfer path. Stays 0 unless a
    /// file frame actually reaches the seam on the production receive path (seam-gap discipline).
    pub filexfer_frames_routed: u64,
    /// Active inbound file-transfer session (at most one per link in v1).
    pub file_rx: Option<crate::filexfer::FxRxState>,
    /// Received files this session, newest last — served by `ListFiles` so a late-connecting client
    /// sees transfers that completed before it attached (not just live `FileReceived` events).
    pub received_files: Vec<crate::protocol::FileSummary>,
    /// Active outbound file-transfer session (at most one per link in v1).
    pub file_tx: Option<crate::filexfer::FxTxState>,
    /// Storage + acceptance policy from `[file_transfer]` config.
    pub filexfer_policy: crate::filexfer::FileTransferPolicy,
    /// Frames the file-transfer sessions want on air, `(sar_fragment, mode)`. `server::run` drains this
    /// with a single PTT keying per burst; queueing (not transmitting inline) keeps the module I/O-free
    /// while the PTT controller — which lives in `server::run` — sequences the half-duplex TX.
    pub filexfer_tx_queue: Vec<(Vec<u8>, String)>,
    /// JS8 station-discovery runtime (FF-15), present when `[discovery]` is configured. `enabled`
    /// gates activity; `server::run` feeds it captured audio + the idle predicate and executes its
    /// retune outcomes. `None` when discovery is not built for this daemon.
    pub discovery: Option<openpulse_discovery::DiscoveryRuntime>,
    /// Simultaneous multi-mode receive (REQ-RX-01); `None` when the monitor is off or has no modes.
    pub monitor: Option<crate::monitor::MonitorRuntime>,
    /// Home frequency (Hz) saved when discovery QSYed to the JS8 calling channel, restored on stand-down;
    /// `None` when not dwelling. The home frequency itself comes from `last_freq_hz`.
    pub discovery_home_freq_hz: Option<u64>,
    /// JS8 calling frequency (Hz) per band label (from `[discovery]` config). Discovery dwells on the
    /// entry for the operator's current home band; empty when discovery is not configured.
    pub discovery_calling_freqs_hz: std::collections::BTreeMap<String, u64>,
    /// Rendezvous working channels (Hz) per band label (from `[discovery]` config). A rendezvous agrees
    /// a channel **index** into the current band's list; the daemon resolves it to Hz for the QSY.
    pub discovery_rendezvous_channels_hz: std::collections::BTreeMap<String, Vec<u64>>,
    /// A scheduled post-rendezvous QSY: `(peer, freq_hz, due_at_ms)`. Set when a rendezvous is agreed; the
    /// QSY + CONREQ handoff fire once the `switch_in_slots` delay elapses (both stations retune together
    /// and the Accept has time to be heard first).
    pub rendezvous_qsy_due: Option<(String, u64, u64)>,
    /// Set by `discovery_tick` the tick a rendezvous QSY completes: `(peer, freq_hz)`. `server::run`
    /// takes it and runs the `ConnectPeer` handshake handoff (needs `&mut engine`), then clears it.
    pub rendezvous_connect_ready: Option<(String, u64)>,
    /// Shared peer cache (§5.2): recognized OpenPulse peers mapped from discovery's hinted stations,
    /// queryable by capability/quality/trust for rendezvous, relay routing, and peer queries.
    pub peer_cache: openpulse_core::peer_cache::PeerCache,
}

/// How long a QSY negotiation may sit without advancing before it is abandoned.
///
/// The negotiation is a handful of frames over RF; minutes is generous. The bound exists because a
/// session that never completes blocks auto-QSY entirely, and an unsigned inbound QSY_REQ is enough
/// to create one (audit 2026-07-19, #4).
pub const QSY_SESSION_TTL: Duration = Duration::from_secs(300);

impl RuntimeControlState {
    /// Drop the QSY session if it has finished, or has sat longer than `ttl` without completing.
    ///
    /// Called from the daemon tick. Without it, one unsigned QSY_REQ from any station — or a
    /// negotiation whose peer simply walked away — disables the anti-jam response for the process
    /// lifetime, because `maybe_qsy_on_interference` returns early whenever a session exists.
    pub fn expire_stale_qsy_session(&mut self, ttl: Duration) {
        let Some(session) = self.qsy_session.as_ref() else {
            self.qsy_session_started = None;
            return;
        };
        let terminal = session.is_terminal();
        let stale = self.qsy_session_started.is_none_or(|t| t.elapsed() >= ttl);
        if terminal || stale {
            tracing::debug!(terminal, stale, "clearing QSY session");
            self.qsy_session = None;
            self.qsy_session_started = None;
            self.qsy_peer_pubkey = None;
        }
    }

    /// True when a verified peer's OTA ladder is known to differ from ours — OTA rate-stepping must
    /// be suppressed (fixed-mode fallback) so a `recommended_level` can't mean different modes.
    /// OTA without a handshake, or with a compatible/undetermined peer, is unaffected.
    pub fn ota_suppressed_by_peer(&self) -> bool {
        matches!(self.last_verified_peer(), Some(p) if p.profile_compatible == Some(false))
    }

    /// The most recently verified peer this session, resolved through the authoritative per-callsign map.
    pub fn last_verified_peer(&self) -> Option<&VerifiedPeer> {
        self.last_verified_callsign
            .as_ref()
            .and_then(|c| self.verified_peers.get(c))
    }

    /// True when the station has a real callsign to transmit under. §97.119 forbids keying the
    /// transmitter without a station ID, and periodic auto-ID is disabled for an empty/`N0CALL`
    /// callsign — so an *autonomous* responder (CONACK, relay, QSY) that merely heard a frame must
    /// not key up at all without a valid MYID, or it would transmit unidentified.
    pub fn local_callsign_valid(&self) -> bool {
        let c = self.local_callsign.trim();
        !c.is_empty() && !c.eq_ignore_ascii_case("N0CALL")
    }

    /// Over-air trust level of the last peer we verified this session, for gating the QSY responder.
    /// RF certificates are `OverAir` without PSK, so this tops out at `Reduced` (a trust-store key)
    /// or `Low` (first-seen) — never `Verified`, which requires an out-of-band cert.
    ///
    /// Read once, at session creation, alongside the key pinned into `qsy_peer_pubkey` (#1252) — so
    /// the level and the key describe the same station and a later CONREQ cannot displace either.
    /// Before #1252 the QSY frame carried no signature at all and this was a "only after some
    /// handshake this session" check rather than a per-requester one. The remaining gap is the
    /// INITIATOR role, whose `QsySession::new_initiator()` still carries an empty policy: the key is
    /// pinned there too, so a stranger cannot steer the negotiation, but no trust *level* is
    /// consulted on that side.
    pub fn rf_peer_trust(&self) -> ConnectionTrustLevel {
        match self.last_verified_peer() {
            Some(p) => {
                let key_trust = self.trust_store.trust_level(&p.callsign);
                classify_connection_trust(key_trust, CertificateSource::OverAir, false).decision
            }
            None => ConnectionTrustLevel::Unverified,
        }
    }
}

/// An in-flight CONREQ the daemon sent and is awaiting a CONACK for.
#[derive(Clone, Debug)]
pub struct PendingHandshake {
    /// Session id of the in-flight handshake.
    pub session_id: u64,
    /// Callsign the operator asked to connect to.
    pub peer_callsign: String,
    /// When the CONREQ went out, for timeout expiry.
    pub started_at: Instant,
    /// Ephemeral X25519 secret for OTA-ACK key agreement (E7); combined with the peer's CONACK
    /// `kex_pubkey` to derive the session ACK-MAC key. All-zero when key agreement is not in use.
    pub kex_secret: [u8; 32],
    /// The CONREQ **exactly as transmitted**, so the CONACK can be bound to it by hash (#1147).
    ///
    /// Kept as bytes rather than as a precomputed hash so the binding is derived from the same
    /// artifact the peer hashed, not from a value this side computed a second time and could
    /// compute differently.
    pub conreq_bytes: Vec<u8>,
    /// Signing modes our CONREQ offered, so a CONACK selecting an unoffered one is refused
    /// (F-1147-05).
    pub offered_modes: Vec<SigningMode>,
}

/// A peer identity proven by a verified Ed25519 handshake signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedPeer {
    /// Peer station id (callsign) from the signed frame.
    pub callsign: String,
    /// Peer Maidenhead grid from the signed frame (empty = not advertised).
    pub grid: String,
    /// Peer Ed25519 verifying-key bytes.
    pub pubkey: Vec<u8>,
    /// Whether the peer's advertised OTA rate ladder matches ours: `Some(true)` = compatible,
    /// `Some(false)` = the ladders diverged (OTA adaptation is unsafe → suppressed), `None` = not
    /// determinable (we or the peer advertised no OTA ladder). See `docs/dev/design/ladder-versioning.md`.
    pub profile_compatible: Option<bool>,
}

/// Timeout after which an unanswered CONREQ is abandoned.
#[cfg(not(target_arch = "wasm32"))]
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Reassembly timeout for inbound file-transfer control fragments.
#[cfg(not(target_arch = "wasm32"))]
pub const FILEXFER_SAR_TIMEOUT: Duration = Duration::from_secs(300);

#[cfg(not(target_arch = "wasm32"))]
impl Default for RuntimeControlState {
    fn default() -> Self {
        Self {
            repeater_enabled: false,
            qsy_decisions: HashMap::new(),
            qsy_pending_token: None,
            qsy_session: None,
            qsy_session_started: None,
            qsy_lines_refused: 0,
            qsy_peer_pubkey: None,
            qsy_candidate_freqs: Vec::new(),
            qsy_policy: QsyPolicy::default(),
            qsy_scan_dwell_ms: 500,
            qsy_switchover_offset_s: 5,
            repeater: None,
            repeater_stop: None,
            repeater_thread: None,
            ptt: crate::ptt::SharedPtt::default(),
            trust_store: InMemoryTrustStore::default(),
            relay_forwarder: None,
            dcd_squelch_default: 0.01,
            dcd_squelch_bands: std::collections::BTreeMap::new(),
            tx_attenuation_default: 0.0,
            tx_attenuation_bands: std::collections::BTreeMap::new(),
            logbook: crate::logbook::Logbook::default(),
            last_freq_hz: None,
            station_seed: [0u8; 32],
            local_callsign: String::new(),
            local_grid: String::new(),
            pending_handshake: None,
            last_verified_callsign: None,
            verified_peers: std::collections::HashMap::new(),
            handshake_sar: SarReassembler::new(HANDSHAKE_TIMEOUT),
            local_ota_ladder: None,
            compress_tx: false,
            filexfer_sar: SarReassembler::new(FILEXFER_SAR_TIMEOUT),
            filexfer_frames_routed: 0,
            file_rx: None,
            received_files: Vec::new(),
            file_tx: None,
            filexfer_policy: crate::filexfer::FileTransferPolicy::default(),
            filexfer_tx_queue: Vec::new(),
            discovery: None,
            monitor: None,
            discovery_home_freq_hz: None,
            discovery_calling_freqs_hz: std::collections::BTreeMap::new(),
            discovery_rendezvous_channels_hz: std::collections::BTreeMap::new(),
            rendezvous_qsy_due: None,
            rendezvous_connect_ready: None,
            peer_cache: openpulse_core::peer_cache::PeerCache::new(256, 3_600_000),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Debug for RuntimeControlState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeControlState")
            .field("repeater_enabled", &self.repeater_enabled)
            .field("qsy_decisions", &self.qsy_decisions)
            .field("qsy_pending_token", &self.qsy_pending_token)
            .field("qsy_session", &self.qsy_session.is_some())
            .field("qsy_candidate_freqs", &self.qsy_candidate_freqs)
            .field("qsy_switchover_offset_s", &self.qsy_switchover_offset_s)
            .field("repeater", &self.repeater.is_some())
            .field("repeater_stop", &self.repeater_stop.is_some())
            .field("repeater_thread", &self.repeater_thread.is_some())
            .field("ptt", &self.ptt)
            .field("trust_store_entries", &"<opaque>")
            .field("relay_forwarder", &self.relay_forwarder.is_some())
            .finish()
    }
}

/// Shared mutable mode string, written by `set_mode` commands.
#[cfg(not(target_arch = "wasm32"))]
pub type SharedMode = Arc<Mutex<String>>;
/// Read-only set of mode names the engine's registered plugins support, captured at startup so the
/// control-command dispatcher can reject an unknown `SetMode`/`SetConfig` *before* writing shared state.
#[cfg(not(target_arch = "wasm32"))]
pub type ValidModes = Arc<std::collections::HashSet<String>>;
/// Shared mutable TX attenuation (dB), written by `set_tx_attenuation` commands.
#[cfg(not(target_arch = "wasm32"))]
pub type SharedAttenuation = Arc<Mutex<f32>>;
/// Shared QSY enabled flag, toggled by `set_config` commands.
#[cfg(not(target_arch = "wasm32"))]
pub type SharedQsyEnabled = Arc<Mutex<bool>>;
/// Shared bandplan mode string (`"unrestricted"`, `"ham-iaru-r1"`, etc.).
#[cfg(not(target_arch = "wasm32"))]
pub type SharedBandplanMode = Arc<Mutex<String>>;
/// Shared flag: allow integrated tuner operation when SWR is high.
#[cfg(not(target_arch = "wasm32"))]
pub type SharedTunerOnHighSWR = Arc<Mutex<bool>>;
/// Shared audio sample tap for spectrum computation (most-recent 1024 samples).
#[cfg(not(target_arch = "wasm32"))]
pub type SpectrumTap = Arc<RwLock<Vec<f32>>>;
/// Shared station identity strings (callsign + grid square), set at startup.
#[cfg(not(target_arch = "wasm32"))]
pub type SharedStationId = Arc<Mutex<(String, String)>>;
/// Shared in-memory message store (sent and received messages).
#[cfg(not(target_arch = "wasm32"))]
pub type SharedMessageStore = Arc<Mutex<MessageStore>>;

/// Initial state used when starting the TCP control server.
#[cfg(not(target_arch = "wasm32"))]
pub struct ControlServerConfig {
    pub initial_mode: String,
    pub initial_station_id: (String, String),
    pub initial_qsy_enabled: bool,
    pub initial_bandplan_mode: String,
    pub initial_allow_tuner_on_high_swr: bool,
    /// Control-channel PSK: `Some` requires each client to complete a Noise handshake
    /// (REQ-CTL-01/02); `None` runs the plaintext path (loopback default).
    pub control_psk: Option<[u8; openpulse_linksec::PSK_LEN]>,
}

/// Maximum number of messages kept in memory; oldest are evicted when full.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) const MAX_MESSAGES: usize = 500;

/// A single stored message (sent or received).
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub id: u64,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub timestamp_secs: u64,
}

/// In-memory inbox with a monotonically increasing ID counter.
#[cfg(not(target_arch = "wasm32"))]
pub struct MessageStore {
    next_id: u64,
    pub messages: std::collections::VecDeque<StoredMessage>,
}

#[cfg(not(target_arch = "wasm32"))]
impl MessageStore {
    fn new() -> Self {
        Self {
            next_id: 1,
            messages: std::collections::VecDeque::new(),
        }
    }

    /// Allocate the next unique message ID.
    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

/// All shared state passed to each per-client TCP handler.
#[cfg(not(target_arch = "wasm32"))]
struct ClientCtx {
    ev_tx: Arc<broadcast::Sender<ControlEvent>>,
    cmd_tx: mpsc::Sender<ControlCommand>,
    active_mode: SharedMode,
    tx_attenuation_db: SharedAttenuation,
    qsy_enabled: SharedQsyEnabled,
    bandplan_mode: SharedBandplanMode,
    allow_tuner_on_high_swr: SharedTunerOnHighSWR,
    spectrum_tap: SpectrumTap,
    station_id: SharedStationId,
    message_store: SharedMessageStore,
    valid_modes: ValidModes,
}

/// Handle returned by [`ControlServer::spawn`].
///
/// Dropping this handle does *not* stop the server — use [`ControlServerHandle::shutdown`]
/// for a clean stop (or just let the process exit).
#[cfg(not(target_arch = "wasm32"))]
pub struct ControlServerHandle {
    /// Receives every [`ControlCommand`] dispatched from any connected client.
    pub commands: mpsc::Receiver<ControlCommand>,
    /// Sender for the shared event broadcast (pass to [`ws::spawn_ws`] to
    /// share state between the TCP and WebSocket control endpoints).
    pub event_tx: Arc<broadcast::Sender<ControlEvent>>,
    /// mpsc sender for injecting commands programmatically (used by WebSocket endpoint).
    pub command_tx: mpsc::Sender<ControlCommand>,
    /// Current active mode string (also updated by the command handler).
    pub active_mode: SharedMode,
    /// Current TX attenuation in dB (also updated by the command handler).
    pub tx_attenuation_db: SharedAttenuation,
    /// Whether QSY frequency-agility is enabled.
    pub qsy_enabled: SharedQsyEnabled,
    /// Active bandplan guardrail mode string.
    pub bandplan_mode: SharedBandplanMode,
    /// Whether tuner-on-high-SWR behavior is allowed.
    pub allow_tuner_on_high_swr: SharedTunerOnHighSWR,
    /// Audio sample tap; caller may write recent RX samples here.
    pub spectrum_tap: SpectrumTap,
    /// Station callsign and grid square loaded from config at startup.
    pub station_id: SharedStationId,
    /// In-memory message store shared across all control endpoints.
    pub message_store: SharedMessageStore,
    /// Live engine metrics written by the main loop; read by the periodic metrics task.
    pub shared_metrics: SharedMetrics,
    /// Mode names the registered plugins support (captured at startup), for pre-write validation of
    /// `SetMode`/`SetConfig` on both the TCP and WebSocket dispatch paths.
    pub valid_modes: ValidModes,
}

/// NDJSON-over-TCP control server.
#[cfg(not(target_arch = "wasm32"))]
pub struct ControlServer;

#[cfg(not(target_arch = "wasm32"))]
impl ControlServer {
    /// Spawn the control server on `addr`.
    ///
    /// `engine` is used to subscribe to the event broadcast channel.
    /// The bound address is written to `bound_addr` if provided (useful in
    /// tests that bind on port 0 and need the ephemeral port).
    pub async fn spawn(
        addr: SocketAddr,
        engine: &ModemEngine,
        config: ControlServerConfig,
        bound_addr: Option<&mut SocketAddr>,
    ) -> Result<ControlServerHandle, std::io::Error> {
        let listener = TcpListener::bind(addr).await?;
        if let Some(out) = bound_addr {
            *out = listener.local_addr()?;
        }

        let (ev_tx, _) = broadcast::channel::<ControlEvent>(256);
        let ev_tx = Arc::new(ev_tx);
        let (cmd_tx, cmd_rx) = mpsc::channel::<ControlCommand>(64);

        let active_mode = Arc::new(Mutex::new(config.initial_mode));
        let tx_attenuation_db: SharedAttenuation = Arc::new(Mutex::new(0.0f32));
        let qsy_enabled: SharedQsyEnabled = Arc::new(Mutex::new(config.initial_qsy_enabled));
        let bandplan_mode: SharedBandplanMode = Arc::new(Mutex::new(config.initial_bandplan_mode));
        let allow_tuner_on_high_swr: SharedTunerOnHighSWR =
            Arc::new(Mutex::new(config.initial_allow_tuner_on_high_swr));
        let spectrum_tap: SpectrumTap = Arc::new(RwLock::new(vec![0.0f32; 1024]));
        let station_id: SharedStationId = Arc::new(Mutex::new(config.initial_station_id));
        let message_store: SharedMessageStore = Arc::new(Mutex::new(MessageStore::new()));
        let shared_metrics: SharedMetrics = Arc::new(Mutex::new(MetricsSnapshot::default()));
        // Capture the registered mode names once, so a bad SetMode is rejected before it mutates state.
        let valid_modes: ValidModes = Arc::new(
            engine
                .plugins()
                .list()
                .iter()
                .flat_map(|info| info.supported_modes.iter().cloned())
                .collect(),
        );

        // Background task: forward EngineEvents into the ControlEvent broadcast.
        let mut eng_rx = engine.subscribe();
        let ev_fwd = Arc::clone(&ev_tx);
        tokio::spawn(async move {
            loop {
                match eng_rx.recv().await {
                    Ok(ev) => {
                        let _ = ev_fwd.send(ControlEvent::EngineEvent { event: ev });
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lost = n, "engine event receiver lagged; events dropped");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Background task: periodic Metrics + SystemMetrics events at 1 Hz.
        let ev_metrics = Arc::clone(&ev_tx);
        let metrics_snap = Arc::clone(&shared_metrics);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            let mut last_bytes: u64 = 0;
            // Host-resource sampling state (daemon process).
            let mut sys = sysinfo::System::new();
            let pid = sysinfo::Pid::from_u32(std::process::id());
            // Prime the CPU baseline so the first emit reports a real delta, not 0.
            sys.refresh_process(pid);
            let mut gpu_available = true;
            // Our-kernel GPU-busy tracking (only meaningful with the `gpu` feature).
            #[cfg(feature = "gpu")]
            let mut last_gpu_busy = openpulse_gpu::gpu_busy_nanos();
            #[cfg(feature = "gpu")]
            let mut last_gpu_instant = std::time::Instant::now();
            loop {
                interval.tick().await;
                let (afc, new_bytes, decode_latency_ms, raw_bytes, compressed_bytes, front_end) = {
                    let m = metrics_snap.lock().await;
                    (
                        m.afc_correction_hz,
                        m.total_rx_bytes,
                        m.decode_latency_ms,
                        m.raw_payload_bytes,
                        m.compressed_payload_bytes,
                        m.front_end,
                    )
                };
                let effective_bps = (new_bytes.saturating_sub(last_bytes) * 8) as f32;
                last_bytes = new_bytes;
                let _ = ev_metrics.send(ControlEvent::Metrics {
                    effective_bps,
                    ecc_rate: None,
                    compress_ratio: compression_ratio(raw_bytes, compressed_bytes),
                    afc_correction_hz: afc,
                    signal_strength_dbm: None,
                });
                // #1276: push the front-end state alongside the metrics, so a client that connects
                // or reconnects is correct within a second without having to ask. Sent every tick
                // rather than on change, for the same reason `Metrics` is: a client that missed the
                // change event would otherwise stay wrong forever, and a bool costs nothing.
                let _ = ev_metrics.send(ControlEvent::FrontEndState {
                    notch: front_end.notch,
                    agc: front_end.agc,
                    cessb: front_end.cessb,
                    logbook: front_end.logbook,
                    dcd_squelch: front_end.dcd_squelch,
                });

                let (cpu_percent, ram_mb, ram_percent) = sample_process_resources(&mut sys, pid);

                // GPU load: prefer the time our wgpu kernels actually spent on the GPU this
                // interval; fall back to a best-effort system source when the gpu feature is
                // off or no kernels ran (CPU path / no adapter).
                #[cfg(feature = "gpu")]
                let gpu_percent = {
                    let now_busy = openpulse_gpu::gpu_busy_nanos();
                    let now = std::time::Instant::now();
                    let busy = now_busy.saturating_sub(last_gpu_busy) as f64;
                    let elapsed = now.duration_since(last_gpu_instant).as_nanos() as f64;
                    last_gpu_busy = now_busy;
                    last_gpu_instant = now;
                    let kernel_pct = if elapsed > 0.0 {
                        (busy / elapsed * 100.0).clamp(0.0, 100.0) as f32
                    } else {
                        0.0
                    };
                    if kernel_pct > 0.05 {
                        Some(kernel_pct)
                    } else {
                        tokio::task::block_in_place(|| read_gpu_utilization(&mut gpu_available))
                    }
                };
                #[cfg(not(feature = "gpu"))]
                let gpu_percent =
                    tokio::task::block_in_place(|| read_gpu_utilization(&mut gpu_available));
                let _ = ev_metrics.send(ControlEvent::SystemMetrics {
                    cpu_percent,
                    ram_mb,
                    ram_percent,
                    gpu_percent,
                    decode_latency_ms,
                });
            }
        });

        // Acceptor task.
        let ev_tx_a = Arc::clone(&ev_tx);
        let cmd_tx_a = cmd_tx.clone();
        let mode_a = Arc::clone(&active_mode);
        let atten_a = Arc::clone(&tx_attenuation_db);
        let qsy_a = Arc::clone(&qsy_enabled);
        let bp_a = Arc::clone(&bandplan_mode);
        let tuner_a = Arc::clone(&allow_tuner_on_high_swr);
        let tap_a = Arc::clone(&spectrum_tap);
        let sid_a = Arc::clone(&station_id);
        let store_a = Arc::clone(&message_store);
        let modes_a = Arc::clone(&valid_modes);
        let control_psk = config.control_psk;
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        tracing::info!(%peer, "control port: client connected");
                        let ctx = ClientCtx {
                            ev_tx: Arc::clone(&ev_tx_a),
                            cmd_tx: cmd_tx_a.clone(),
                            active_mode: Arc::clone(&mode_a),
                            tx_attenuation_db: Arc::clone(&atten_a),
                            qsy_enabled: Arc::clone(&qsy_a),
                            bandplan_mode: Arc::clone(&bp_a),
                            allow_tuner_on_high_swr: Arc::clone(&tuner_a),
                            spectrum_tap: Arc::clone(&tap_a),
                            station_id: Arc::clone(&sid_a),
                            message_store: Arc::clone(&store_a),
                            valid_modes: Arc::clone(&modes_a),
                        };
                        let rx = ev_tx_a.subscribe();
                        tokio::spawn(handle_client(stream, rx, ctx, control_psk));
                    }
                    Err(e) => tracing::warn!("control port accept error: {e}"),
                }
            }
        });

        Ok(ControlServerHandle {
            commands: cmd_rx,
            event_tx: ev_tx,
            command_tx: cmd_tx,
            active_mode,
            tx_attenuation_db,
            qsy_enabled,
            bandplan_mode,
            allow_tuner_on_high_swr,
            spectrum_tap,
            station_id,
            message_store,
            shared_metrics,
            valid_modes,
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// Mode-aware control-connection writer: plaintext (loopback) or a PSK-authenticated Noise channel.
enum ClientWriter {
    Plain(tokio::net::tcp::OwnedWriteHalf),
    Noise(openpulse_linksec::async_channel::NoiseWriteHalf<tokio::io::WriteHalf<TcpStream>>),
}

impl ClientWriter {
    /// Write one JSON value as a protocol message (a `\n`-terminated line on the plaintext path;
    /// a length-framed Noise message on the authenticated path).
    async fn write_json<T: serde::Serialize>(&mut self, v: &T) -> Result<(), ()> {
        let s = serde_json::to_string(v).map_err(|_| ())?;
        match self {
            ClientWriter::Plain(w) => {
                let mut line = s;
                line.push('\n');
                w.write_all(line.as_bytes()).await.map_err(|_| ())
            }
            ClientWriter::Noise(w) => w.send(s.as_bytes()).await.map_err(|_| ()),
        }
    }

    /// Write one raw binary frame (e.g. a spectrum frame).
    async fn write_frame(&mut self, bytes: &[u8]) -> Result<(), ()> {
        match self {
            ClientWriter::Plain(w) => w.write_all(bytes).await.map_err(|_| ()),
            ClientWriter::Noise(w) => w.send(bytes).await.map_err(|_| ()),
        }
    }
}

/// Mode-aware control-connection reader yielding one NDJSON command per message.
enum ClientReader {
    Plain(tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>),
    Noise(openpulse_linksec::async_channel::NoiseReadHalf<tokio::io::ReadHalf<TcpStream>>),
}

impl ClientReader {
    /// Next command line: `Ok(Some(line))`, `Ok(None)` on clean close, `Err(())` on error.
    async fn next_command(&mut self) -> Result<Option<String>, ()> {
        match self {
            ClientReader::Plain(l) => l.next_line().await.map_err(|_| ()),
            ClientReader::Noise(r) => match r.recv().await {
                Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| ()),
                Err(_) => Ok(None),
            },
        }
    }
}

async fn handle_client(
    stream: TcpStream,
    mut ev_rx: broadcast::Receiver<ControlEvent>,
    ctx: ClientCtx,
    control_psk: Option<[u8; openpulse_linksec::PSK_LEN]>,
) {
    // When a PSK is configured (non-loopback bind or require_auth), the client must complete the
    // Noise handshake; a wrong/absent PSK drops the connection before any command is processed
    // (fail closed, REQ-CTL-02). Otherwise the channel is plaintext (loopback default).
    let (mut reader, mut write_half) = match control_psk {
        Some(psk) => {
            match openpulse_linksec::async_channel::AsyncNoise::responder(stream, &psk).await {
                Ok(ch) => {
                    let (w, r) = ch.into_split();
                    (ClientReader::Noise(r), ClientWriter::Noise(w))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "control client failed the PSK handshake; dropping (fail closed)");
                    return;
                }
            }
        }
        None => {
            let (read_half, write_half) = stream.into_split();
            (
                ClientReader::Plain(BufReader::new(read_half).lines()),
                ClientWriter::Plain(write_half),
            )
        }
    };

    let (spec_frame_tx, mut spec_frame_rx) = mpsc::channel::<Vec<u8>>(4);
    let mut spectrum_task: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        tokio::select! {
            Some(frame) = spec_frame_rx.recv() => {
                if write_half.write_frame(&frame).await.is_err() { break; }
            }
            result = ev_rx.recv() => {
                match result {
                    Ok(ev) => {
                        if write_half.write_json(&ev).await.is_err() { break; }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lost = n, "TCP client event receiver lagged; events dropped");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            result = reader.next_command() => {
                match result {
                    Ok(Some(line)) if !line.trim().is_empty() => {
                        let cmd: ControlCommand = match serde_json::from_str(line.trim()) {
                            Ok(c) => c,
                            Err(e) => {
                                let resp = CommandResponse::err(format!("parse error: {e}"));
                                let _ = send_json(&mut write_half, &resp).await;
                                continue;
                            }
                        };
                        if handle_command(cmd, &mut write_half, &spec_frame_tx, &mut spectrum_task, &ctx).await {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Ok(Some(_)) => {}
                    Err(_) => break,
                }
            }
        }
    }

    if let Some(h) = spectrum_task {
        h.abort();
    }
}

/// Dispatch one command; returns `true` when the write failed and the loop should exit.
#[cfg(not(target_arch = "wasm32"))]
// TCP control-port command handler. The request-response commands below (those that return data,
// not just an ok) are handled inline; everything else falls to `dispatch_command`. The WebSocket
// path in `ws.rs` mirrors this exact inline set — KEEP THE TWO IN SYNC: a request-response command
// added here but not in `ws.rs` (or vice versa) silently falls to `dispatch_command` on the other
// transport and returns no data. (Audited 2026-06-27: both transports are at parity.)
async fn handle_command(
    cmd: ControlCommand,
    write_half: &mut ClientWriter,
    spec_frame_tx: &mpsc::Sender<Vec<u8>>,
    spectrum_task: &mut Option<tokio::task::JoinHandle<()>>,
    ctx: &ClientCtx,
) -> bool {
    match &cmd {
        ControlCommand::SubscribeSpectrum { fps } => {
            let fps = (*fps).clamp(1, 100);
            if let Some(h) = spectrum_task.take() {
                h.abort();
            }
            let tap = Arc::clone(&ctx.spectrum_tap);
            let tx = spec_frame_tx.clone();
            let period = Duration::from_millis(1000 / fps as u64);
            *spectrum_task = Some(tokio::spawn(async move {
                let mut interval = tokio::time::interval(period);
                let mut ps = PowerSpectrum::new();
                loop {
                    interval.tick().await;
                    let bins = ps.compute(&tap.read().await);
                    let frame = encode_spectrum_frame(8000, &bins);
                    if tx.send(frame).await.is_err() {
                        break;
                    }
                }
            }));
            send_json(write_half, &CommandResponse::ok()).await.is_err()
        }

        ControlCommand::GetConfig => {
            let (cs, gs) = ctx.station_id.lock().await.clone();
            // Hold all locks simultaneously so the snapshot is consistent with SetConfig.
            let mode_guard = ctx.active_mode.lock().await;
            let atten_guard = ctx.tx_attenuation_db.lock().await;
            let qsy_guard = ctx.qsy_enabled.lock().await;
            let bp_guard = ctx.bandplan_mode.lock().await;
            let tuner_guard = ctx.allow_tuner_on_high_swr.lock().await;
            let config = protocol::DaemonConfig {
                callsign: cs,
                grid_square: gs,
                mode: mode_guard.clone(),
                tx_attenuation_db: *atten_guard,
                qsy_enabled: *qsy_guard,
                bandplan_mode: bp_guard.clone(),
                allow_tuner_on_high_swr: *tuner_guard,
            };
            drop(mode_guard);
            drop(atten_guard);
            drop(qsy_guard);
            drop(bp_guard);
            drop(tuner_guard);
            if send_json(write_half, &ControlEvent::ConfigData { config })
                .await
                .is_err()
            {
                return true;
            }
            send_json(write_half, &CommandResponse::ok()).await.is_err()
        }

        ControlCommand::ListMessages => {
            let messages: Vec<MessageSummary> = ctx
                .message_store
                .lock()
                .await
                .messages
                .iter()
                .map(|m| MessageSummary {
                    id: m.id,
                    from: m.from.clone(),
                    to: m.to.clone(),
                    subject: m.subject.clone(),
                    timestamp_secs: m.timestamp_secs,
                })
                .collect();
            if send_json(write_half, &ControlEvent::MessageList { messages })
                .await
                .is_err()
            {
                return true;
            }
            send_json(write_half, &CommandResponse::ok()).await.is_err()
        }

        ControlCommand::GetMessage { id } => {
            let found = ctx
                .message_store
                .lock()
                .await
                .messages
                .iter()
                .find(|m| m.id == *id)
                .cloned();
            match found {
                None => send_json(
                    write_half,
                    &CommandResponse::err(format!("unknown id {id}")),
                )
                .await
                .is_err(),
                Some(m) => {
                    let ev = ControlEvent::MessageData {
                        id: m.id,
                        from: m.from,
                        to: m.to,
                        subject: m.subject,
                        body: m.body,
                    };
                    if send_json(write_half, &ev).await.is_err() {
                        return true;
                    }
                    send_json(write_half, &CommandResponse::ok()).await.is_err()
                }
            }
        }

        ControlCommand::SendMessage { to, subject, body } => {
            let from = ctx.station_id.lock().await.0.clone();
            let timestamp_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let id = {
                let mut store = ctx.message_store.lock().await;
                let id = store.alloc_id();
                store.messages.push_back(StoredMessage {
                    id,
                    from: from.clone(),
                    to: to.clone(),
                    subject: subject.clone(),
                    body: body.clone(),
                    timestamp_secs,
                });
                if store.messages.len() > MAX_MESSAGES {
                    let _ = store.messages.pop_front();
                }
                id
            };
            let preview: String = body.chars().take(120).collect();
            let ev = ControlEvent::MessageReceived {
                id,
                from,
                to: to.clone(),
                subject: subject.clone(),
                preview,
                timestamp_secs,
            };
            // Broadcast to all connected clients.
            let _ = ctx.ev_tx.send(ev);
            // Forward to daemon main for RF dispatch.
            let _ = ctx.cmd_tx.send(cmd.clone()).await;
            send_json(write_half, &CommandResponse::ok()).await.is_err()
        }

        ControlCommand::DeleteMessage { id } => {
            ctx.message_store
                .lock()
                .await
                .messages
                .retain(|m| m.id != *id);
            send_json(write_half, &CommandResponse::ok()).await.is_err()
        }

        _ => {
            let resp = dispatch_command(
                &cmd,
                &ctx.cmd_tx,
                &ctx.active_mode,
                &ctx.tx_attenuation_db,
                &ctx.qsy_enabled,
                &ctx.bandplan_mode,
                &ctx.allow_tuner_on_high_swr,
                &ctx.valid_modes,
            )
            .await;
            send_json(write_half, &resp).await.is_err()
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn send_json<T: serde::Serialize>(writer: &mut ClientWriter, value: &T) -> Result<(), ()> {
    writer.write_json(value).await
}

/// Apply state-mutating commands and forward all commands to the caller.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_command(
    cmd: &ControlCommand,
    cmd_tx: &mpsc::Sender<ControlCommand>,
    active_mode: &SharedMode,
    tx_attenuation_db: &SharedAttenuation,
    qsy_enabled: &SharedQsyEnabled,
    bandplan_mode: &SharedBandplanMode,
    allow_tuner_on_high_swr: &SharedTunerOnHighSWR,
    valid_modes: &ValidModes,
) -> CommandResponse {
    // Reject an unknown mode BEFORE writing shared state, so a typo can't silently deafen RX +
    // station-ID while the client is told "ok" (the later engine-side validation only logs). An empty
    // set means the caller supplied no registry (tests) — skip validation to preserve their behaviour.
    let requested_mode = match cmd {
        ControlCommand::SetMode { mode } => Some(mode),
        ControlCommand::SetConfig { config } => Some(&config.mode),
        _ => None,
    };
    if let Some(mode) = requested_mode {
        if !valid_modes.is_empty() && !valid_modes.contains(mode) {
            return CommandResponse::err(format!("unsupported mode '{mode}'"));
        }
    }
    if let ControlCommand::SetMode { ref mode } = cmd {
        *active_mode.lock().await = mode.clone();
    }
    if let ControlCommand::SetTxAttenuation { db, band } = cmd {
        // The shared value is the reported global default; a per-band override does not change it (its
        // effect is tracked engine-side and applied on the matching band). See apply_command_to_engine.
        if band.is_none() {
            *tx_attenuation_db.lock().await = *db;
        }
    }
    if let ControlCommand::SetConfig { ref config } = cmd {
        // Hold all locks simultaneously so GetConfig cannot observe a mixed state.
        let (new_qsy, new_bp, new_allow_tuner) = {
            let mut mode = active_mode.lock().await;
            let mut atten = tx_attenuation_db.lock().await;
            let mut qsy = qsy_enabled.lock().await;
            let mut bp = bandplan_mode.lock().await;
            let mut tuner = allow_tuner_on_high_swr.lock().await;
            *mode = config.mode.clone();
            *atten = config.tx_attenuation_db;
            *qsy = config.qsy_enabled;
            *bp = config.bandplan_mode.clone();
            *tuner = config.allow_tuner_on_high_swr;
            (*qsy, bp.clone(), *tuner)
        };
        // Persist QSY settings so they survive a daemon restart.
        if let Err(e) = openpulse_config::save_qsy_config(new_qsy, &new_bp, new_allow_tuner) {
            tracing::warn!("could not persist QSY config: {e}");
        }
    }

    if cmd_tx.send(cmd.clone()).await.is_err() {
        return CommandResponse::err("server shutting down");
    }

    CommandResponse::ok()
}

/// Dispatch a list of [`QsyAction`]s produced by a [`QsySession`].
///
/// Used by both the initiator (`accept_qsy`) and responder (`process_received_bytes`) paths.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
async fn execute_qsy_actions(
    actions: Vec<QsyAction>,
    session: &mut QsySession,
    engine: &mut ModemEngine,
    mut rig_controller: Option<&mut (dyn CatController + Send)>,
    event_tx: &Arc<broadcast::Sender<ControlEvent>>,
    // Cloned, not borrowed out of `runtime_state`: at the QSY call sites the session is a live
    // `&mut` borrow out of the same struct. `SharedPtt` is an `Arc` clone, so this is free.
    ptt: &crate::ptt::SharedPtt,
    // Station signing seed — every emitted QSY line is signed (#1252).
    station_seed: &[u8; 32],
    mode: &str,
    scan_dwell_ms: u64,
) {
    // NOTE: eight parameters. The alternative — passing `runtime_state` and reading `ptt`/dwell from
    // it — does not compile: at two of the three call sites the QSY session is a live `&mut` borrow
    // out of that same struct across this call.
    let mut scan_freqs: Option<Vec<u64>> = None;

    for action in actions {
        match action {
            QsyAction::SendFrame(ref frame) => {
                match encode_qsy_frame(frame, now_ms(), station_seed) {
                    Ok(line) => {
                        let _ = keyed_transmit(ptt, Some(event_tx.as_ref()), "qsy", || {
                            engine.transmit(line.as_bytes(), mode, None)
                        });
                    }
                    // Over the single-frame budget, or the seed could not sign. Both used to surface at
                    // transmit — AFTER the REQ had gone out — wedging the peer until its session TTL.
                    Err(e) => {
                        tracing::warn!(error = %e, "qsy: not transmitting an unsignable frame")
                    }
                }
            }
            QsyAction::StartScan { candidates } => {
                scan_freqs = Some(candidates);
            }
            QsyAction::QsyNow { freq_hz } => {
                if let Some(ref mut rig) = rig_controller {
                    if let Err(e) = rig.set_frequency(freq_hz) {
                        tracing::warn!(freq_hz, error = %e, "qsy: set_frequency failed");
                    }
                }
            }
            QsyAction::Reject { reason } => {
                let _ = event_tx.send(ControlEvent::CommandError {
                    command: "qsy".to_string(),
                    reason: format!("QSY rejected: {reason}"),
                });
            }
        }
    }

    if let Some(freqs) = scan_freqs {
        let results: Vec<(u64, f32)> = if let Some(ref mut rig) = rig_controller {
            // Hop to each candidate, dwell briefly, and read the measured SNR.
            // Save and restore the original frequency so the radio is left on-channel.
            // Rig calls are synchronous TCP I/O — run them in block_in_place so they
            // don't stall the Tokio runtime during the scan.
            let original_freq = match tokio::task::block_in_place(|| rig.get_frequency()) {
                Ok(f) => Some(f),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "qsy scan: failed to read current frequency; will not restore after scan"
                    );
                    None
                }
            };
            let mut scan_results = Vec::with_capacity(freqs.len());
            for &freq in &freqs {
                if let Err(e) = tokio::task::block_in_place(|| rig.set_frequency(freq)) {
                    tracing::warn!(freq, error = %e, "qsy scan: set_frequency failed; using last SNR");
                    scan_results.push((freq, engine.last_rx_snr_db().unwrap_or(0.0)));
                    continue;
                }
                // Dwell per config to let the audio buffer refresh before sampling SNR.
                tokio::time::sleep(Duration::from_millis(scan_dwell_ms)).await;
                match tokio::task::block_in_place(|| engine.receive(mode, None)) {
                    Ok(_) => {}
                    Err(e) => tracing::warn!(freq, error = %e, "qsy scan: receive failed"),
                }
                scan_results.push((freq, engine.last_rx_snr_db().unwrap_or(0.0)));
            }
            if let Some(orig) = original_freq {
                if let Err(e) = tokio::task::block_in_place(|| rig.set_frequency(orig)) {
                    tracing::warn!(freq = orig, error = %e, "qsy scan: failed to restore frequency");
                }
            }
            scan_results
        } else {
            // No rig controller: fall back to uniform SNR from the most recent receive.
            let observed_snr = engine.last_rx_snr_db().unwrap_or(0.0);
            freqs.iter().map(|&f| (f, observed_snr)).collect()
        };
        match session.scan_complete(results) {
            Ok(follow_up) => {
                // scan_complete never returns another StartScan; iterate directly.
                for action in follow_up {
                    match action {
                        QsyAction::SendFrame(ref frame) => {
                            match encode_qsy_frame(frame, now_ms(), station_seed) {
                                Ok(line) => {
                                    let _ = keyed_transmit(
                                        ptt,
                                        Some(event_tx.as_ref()),
                                        "qsy-post-scan",
                                        || engine.transmit(line.as_bytes(), mode, None),
                                    );
                                }
                                Err(e) => tracing::warn!(
                                    error = %e,
                                    "qsy: not transmitting an unsignable post-scan frame"
                                ),
                            }
                        }
                        QsyAction::QsyNow { freq_hz } => {
                            if let Some(ref mut rig) = rig_controller {
                                if let Err(e) = rig.set_frequency(freq_hz) {
                                    tracing::warn!(freq_hz, error = %e, "qsy: post-scan set_frequency failed");
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "qsy: scan_complete failed"),
        }
    }
}

/// Release PTT if the watchdog deadline has elapsed since `PttAssert`.
///
/// Returns `true` if the watchdog fired (PTT was forcibly released). The hardware release now happens
/// inside [`ptt::SharedPtt::force_release_if_expired`], so callers no longer need to propagate it —
/// this is a thin delegate kept for the async command loop's cooperative poll. The independent
/// watchdog thread ([`ptt::SharedPtt::spawn_watchdog`]) fires the same path when the loop is blocked.
#[cfg(not(target_arch = "wasm32"))]
pub fn check_ptt_watchdog(
    runtime_state: &mut RuntimeControlState,
    event_tx: &Arc<broadcast::Sender<ControlEvent>>,
) -> bool {
    runtime_state.ptt.force_release_if_expired(event_tx)
}

/// Process raw bytes received from the modem engine and drive QSY responder logic.
///
/// Called from the main daemon loop after each receive tick. Non-QSY payloads are
/// silently discarded; only valid [`QsyFrame`] lines advance the session.
#[cfg(not(target_arch = "wasm32"))]
pub async fn process_received_bytes(
    bytes: &[u8],
    runtime_state: &mut RuntimeControlState,
    rig_controller: Option<&mut (dyn CatController + Send)>,
    event_tx: &Arc<broadcast::Sender<ControlEvent>>,
    active_mode: &SharedMode,
    engine: &mut ModemEngine,
) {
    if bytes.is_empty() {
        return;
    }
    let mode = active_mode.lock().await.clone();

    // Attempt relay forwarding on the raw bytes before QSY parsing: WireEnvelope frames
    // are binary and would be dropped by the UTF-8 early return below.
    maybe_relay_forward(bytes, &mode, runtime_state, engine, event_tx);

    // QSY frames are ASCII; a non-QSY, non-relay binary frame is a candidate handshake SAR
    // fragment (CONREQ/CONACK exceed one 255-byte modem frame, so they arrive fragmented).
    let text = std::str::from_utf8(bytes).ok().map(str::trim);

    // A line that IDENTIFIES ITSELF as QSY never falls through to SAR (#1162). Before the format
    // had a magic there was nothing to test, so any undecodable text dropped into the reassembly
    // router below — where `sar_segment_id` of ASCII routes a non-zero first byte into FILEXFER
    // fragment reassembly. That was survivable while QSY was unrecognisable; with a magic it would
    // mean a future-epoch QSY line silently feeding the file assembler. Dispatch, not session
    // semantics — #1162's scope note says leave the session alone, and this leaves it alone.
    // Which key must have signed this line (#1252). An in-flight session is bound to the peer it was
    // created with; the first frame of a new one is bound to the peer verified by the handshake.
    // No verified peer means no key, so the line cannot be authenticated and is refused — the
    // signature is mandatory, which is also what stops `allow_trustlevels`' documented
    // "empty list accepts any level" from admitting an unsigned stranger.
    let qsy_expected_key: Option<[u8; 32]> = runtime_state.qsy_peer_pubkey.or_else(|| {
        runtime_state
            .last_verified_peer()
            .and_then(|p| <[u8; 32]>::try_from(p.pubkey.as_slice()).ok())
    });
    let qsy_freshness = openpulse_core::handshake::Freshness {
        now_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        max_skew_ms: HANDSHAKE_MAX_SKEW_MS,
    };
    let qsy_decoded: Option<QsyFrame> = text.and_then(|t| {
        let key = qsy_expected_key.as_ref()?;
        decode_qsy_frame(t, key, qsy_freshness).ok()
    });

    if let Some(t) = text {
        if t.starts_with(qsy_wire_magic()) && qsy_decoded.is_none() {
            runtime_state.qsy_lines_refused = runtime_state.qsy_lines_refused.saturating_add(1);
            tracing::warn!(
                line = %t.chars().take(24).collect::<String>(),
                had_key = qsy_expected_key.is_some(),
                "refusing a QSY line: unsigned, forged, stale, or a version this build cannot decode"
            );
            return;
        }
    }

    let Some(frame) = qsy_decoded else {
        // Route the reassembly by SAR segment-id (the 4-byte header is public layout): 0 = handshake
        // (unchanged, bit-for-bit), any other id = file transfer. A malformed sub-header frame stays on
        // the handshake path, which ignores it exactly as before.
        match sar_segment_id(bytes) {
            Some(0) | None => {
                try_reassemble_handshake(bytes, runtime_state, event_tx, &mode, engine)
            }
            Some(segment_id) => {
                filexfer::route_inbound_fragment(bytes, segment_id, runtime_state, event_tx, &mode)
            }
        }
        return;
    };

    // Audit F6 (§97.119): the QSY responder keys the transmitter (even a Reject reply is an on-air
    // frame), so an autonomous responder that merely heard a QSY frame must not engage without a
    // valid MYID, or it would transmit unidentified.
    if !runtime_state.local_callsign_valid() {
        tracing::warn!(
            "qsy: ignoring inbound frame — no valid station callsign to transmit an identified reply"
        );
        return;
    }

    // Audit F4: classify the QSY requester's trust from the peer we verified this session (over-air,
    // no PSK → at most `Reduced`) instead of a hardcoded `Unverified`, so `qsy.allow_trustlevels`
    // is an enforceable gate rather than a control that rejects every peer.
    let qsy_policy = runtime_state.qsy_policy.clone();
    let peer_trust = runtime_state.rf_peer_trust();
    let is_new_session = runtime_state.qsy_session.is_none();
    if is_new_session {
        // Stamp on creation so an abandoned negotiation can time out. Without this a single
        // unsigned inbound REQ blocks auto-QSY forever (audit 2026-07-19, #4).
        runtime_state.qsy_session_started = Some(Instant::now());
        // Bind the negotiation to the key that signed its first frame — see `qsy_peer_pubkey`.
        runtime_state.qsy_peer_pubkey = qsy_expected_key;
    }
    let session = runtime_state
        .qsy_session
        .get_or_insert_with(|| QsySession::new_responder(qsy_policy, peer_trust));

    // Notify connected clients that a remote station initiated QSY.
    if is_new_session {
        if let QsyFrame::Req {
            ref token,
            n_candidates,
        } = frame
        {
            let _ = event_tx.send(ControlEvent::QsyIncoming {
                token: token.clone(),
                n_candidates,
            });
        }
    }

    match session.apply(frame) {
        Ok(actions) => {
            // Clone before the call: `runtime_state` is borrowed for the session across it.
            let qsy_ptt = runtime_state.ptt.clone();
            let qsy_dwell = runtime_state.qsy_scan_dwell_ms;
            let qsy_seed = runtime_state.station_seed;
            execute_qsy_actions(
                actions,
                session,
                engine,
                rig_controller,
                event_tx,
                &qsy_ptt,
                &qsy_seed,
                &mode,
                qsy_dwell,
            )
            .await;
        }
        Err(e) => tracing::warn!(error = %e, "qsy responder: apply frame failed"),
    }
}

/// Session key for the handshake SAR reassembler. One handshake is in flight per peer connection,
/// and a node only ever receives one frame type at a time (initiator→CONACK, responder→CONREQ).
#[cfg(not(target_arch = "wasm32"))]
const HANDSHAKE_SAR_SESSION: &str = "handshake";

/// SAR-fragment a signed handshake frame and transmit it. The receiver reassembles in
/// [`try_reassemble_handshake`].
///
/// Since #1147 a classical CONREQ/CONACK is ONE fragment (241/244 B against 251 B), so this is a
/// single frame in practice — the path stays SAR-based because the PQ frames (~5 kB) still need it,
/// and because "one fragment" is a property of the caps rather than something this function should
/// assume.
#[cfg(not(target_arch = "wasm32"))]
/// Run one keyed emission: key the transmitter, run `emit`, release on drop.
///
/// **Every** `engine.transmit*` in this crate goes through here — enforced by
/// `every_daemon_transmit_is_keyed` in `tests/ptt_keys_every_daemon_transmit.rs`, which fails on a
/// bare call and is validated against a planted one.
///
/// Three rules the shape enforces rather than documents (#1262):
///
/// * **One key per BURST, not per frame.** A multi-fragment handshake or filexfer burst is one
///   keying; the peer cannot reply between fragments. Keying per frame would key/unkey ~20 times for
///   one PQ CONREQ.
/// * **The guard never crosses an `.await`.** It is owned by this synchronous frame and cannot
///   escape it, so an async caller must finish its awaits (mode locks, etc.) *before* calling in.
/// * **No `block_in_place` in here.** `lib.rs` has 49 `#[tokio::test]`s on the default
///   current-thread flavor, where `block_in_place` panics. Callers that need it wrap the call.
///
/// Unix milliseconds now — the stamp on every signed QSY line.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Reports WHICH half failed. Several call sites log the two differently on purpose — a refused
/// assert on the station ID is a §97.119 obligation not met (`error`), while a transmit error is a
/// modem fault (`warn`) — so collapsing them into one `Option` would delete audit-derived
/// diagnostics (audit 2026-07-19, #8).
pub(crate) enum KeyedTxError {
    /// The transmitter could not be keyed by a HARDWARE fault; nothing was emitted. The underlying
    /// `PttError` is logged at the failure site rather than carried — no caller branches on which
    /// PTT fault occurred, and carrying it made the variant a dead field.
    Assert,
    /// Somebody else holds a live key; nothing was emitted, and nothing is wrong (#1263).
    ///
    /// **A third variant is mandatory, not cosmetic.** Callers must be able to tell "the rig is
    /// busy" from "the rig is broken", because the station ID treats them oppositely: it DEFERS on
    /// this (keeps its due flag, sends after the holder releases) and MARKS on `Assert`. Overloading
    /// `Assert` would put the hardware-fault path onto the defer path, and a faulted rig would then
    /// get a key attempt every 50 ms tick for the whole 180 s watchdog window.
    AlreadyKeyed,
    /// Keyed successfully, but the emission itself failed.
    Transmit(openpulse_core::error::ModemError),
}

/// Clear repeater state whose thread has already exited (#1298).
///
/// The `EnableRepeater` thread OWNS the `CrossBandRepeater` — it is `take()`n out of the runtime
/// state — so when that thread exits the repeater is gone for good. Nothing observed that:
/// `repeater_enabled` stayed `true`, `EnableRepeater` answered "already enabled", and a
/// `DisableRepeater` + `EnableRepeater` pair reported success while starting nothing.
///
/// Called at the top of both repeater commands rather than from a periodic tick: the events emitted
/// by the thread itself are what notify clients promptly, and this only has to make the *next*
/// command see the truth. A tick poll would need `runtime_state` on the tick path for no added
/// signal.
fn reap_finished_repeater(
    runtime_state: &mut RuntimeControlState,
    event_tx: &Arc<broadcast::Sender<ControlEvent>>,
) {
    let finished = runtime_state
        .repeater_thread
        .as_ref()
        .is_some_and(|t| t.is_finished());
    if !finished {
        return;
    }
    if let Some(t) = runtime_state.repeater_thread.take() {
        let _ = t.join();
    }
    runtime_state.repeater_stop = None;
    if runtime_state.repeater_enabled {
        runtime_state.repeater_enabled = false;
        tracing::warn!("cross-band repeater thread has exited; marking the repeater disabled");
        let _ = event_tx.send(ControlEvent::RepeaterChanged { enabled: false });
    }
}

pub(crate) fn keyed_transmit<T>(
    ptt: &crate::ptt::SharedPtt,
    event_tx: Option<&broadcast::Sender<ControlEvent>>,
    what: &str,
    emit: impl FnOnce() -> Result<T, openpulse_core::error::ModemError>,
) -> Result<T, KeyedTxError> {
    let _guard = match ptt.keyed(event_tx) {
        Ok(g) => g,
        // Busy is not broken (#1263): log it at debug and hand back a variant the caller can defer
        // on, rather than the hardware-fault variant it must mark on.
        Err(openpulse_radio::PttError::AlreadyKeyed { held_by }) => {
            tracing::debug!(what, held_by, "PTT held by another emission; skipped");
            return Err(KeyedTxError::AlreadyKeyed);
        }
        Err(e) => {
            tracing::warn!(what, error = %e, "PTT assert failed; emission skipped");
            return Err(KeyedTxError::Assert);
        }
    };
    emit().map_err(|e| {
        tracing::warn!(what, error = %e, "transmit failed");
        KeyedTxError::Transmit(e)
    })
}

/// Transmit a handshake frame as ONE keyed burst over its SAR fragments.
///
/// Returns `false` when nothing went out. The caller must not record a verified peer on a CONACK
/// that was never transmitted — see the F5 note at the call site.
fn transmit_handshake_frame(
    engine: &mut ModemEngine,
    ptt: &crate::ptt::SharedPtt,
    event_tx: Option<&broadcast::Sender<ControlEvent>>,
    mode: &str,
    frame: &[u8],
) -> bool {
    let fragments = match sar_encode(0, frame) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "handshake: SAR encode failed");
            return false;
        }
    };
    // ONE keying for the whole fragment burst: a PQ frame is ~20 contiguous fragments and the peer
    // cannot reply mid-burst.
    keyed_transmit(ptt, event_tx, "handshake", || {
        for frag in fragments {
            engine.transmit(&frag, mode, None)?;
        }
        Ok(())
    })
    .is_ok()
}

/// Feed a non-QSY, non-relay frame into the handshake SAR reassembler; on a completed segment,
/// dispatch the reassembled CONREQ/CONACK (confirmed by its HSCQ/HSAK magic). Stray frames create
/// at most a short-lived reassembly slot that the periodic [`expire_pending_handshake`] clears.
#[cfg(not(target_arch = "wasm32"))]
/// The SAR `segment_id` (big-endian bytes 0–1) of a fragment, or `None` if it's too short to be a
/// well-formed SAR fragment. Used to route reassembly (handshake = 0, file transfer ≠ 0).
#[cfg(not(target_arch = "wasm32"))]
fn sar_segment_id(bytes: &[u8]) -> Option<u16> {
    (bytes.len() >= openpulse_core::sar::SAR_HEADER_SIZE)
        .then(|| ((bytes[0] as u16) << 8) | bytes[1] as u16)
}

#[cfg(not(target_arch = "wasm32"))]
fn try_reassemble_handshake(
    bytes: &[u8],
    runtime_state: &mut RuntimeControlState,
    event_tx: &Arc<broadcast::Sender<ControlEvent>>,
    mode: &str,
    engine: &mut ModemEngine,
) {
    // A fragment may complete more than one candidate when a poisoned/interleaved stream shares the
    // constant handshake key; dispatch every completed frame and let verification drop the bogus ones.
    let completed = match runtime_state
        .handshake_sar
        .ingest(HANDSHAKE_SAR_SESSION, bytes)
    {
        Ok(frames) => frames,
        Err(_) => return, // not a well-formed SAR fragment; ignore
    };
    for assembled in completed {
        if assembled.starts_with(b"HSCQ") {
            handle_inbound_conreq(&assembled, runtime_state, event_tx, mode, engine);
        } else if assembled.starts_with(b"HSAK") {
            handle_inbound_conack(&assembled, runtime_state, event_tx, engine);
        } else {
            tracing::debug!("handshake: reassembled segment has no CONREQ/CONACK magic; dropping");
        }
    }
}

/// Responder side of the signed handshake: verify an inbound CONREQ, reply with a signed
/// CONACK over RF, and record the proven peer identity (callsign + grid + pubkey). Verification
/// failures are logged and dropped (no reply), so an unverifiable frame can't open a session.
#[cfg(not(target_arch = "wasm32"))]
fn handle_inbound_conreq(
    bytes: &[u8],
    runtime_state: &mut RuntimeControlState,
    event_tx: &Arc<broadcast::Sender<ControlEvent>>,
    mode: &str,
    engine: &mut ModemEngine,
) {
    // Permissive policy: the signature proves key possession; trust classification is recorded
    // but an unknown (first-seen) peer is still allowed to connect, mirroring `ConnectPeer`.
    // Freshness (replay protection): reject a captured/replayed CONREQ outside the clock-skew window.
    //
    // Verified from the received BYTES: the signature covers the transmitted prefix, so re-encoding
    // a decoded struct to recover the signed span would reintroduce the two-representation drift
    // this format removes.
    let now_ms = unix_now_ms();
    let req = match verify_conreq(
        bytes,
        &runtime_state.trust_store,
        PolicyProfile::Permissive,
        SigningMode::Normal,
        Some(Freshness {
            now_ms,
            max_skew_ms: HANDSHAKE_MAX_SKEW_MS,
        }),
    ) {
        Ok((req, _decision)) => req,
        Err(e) => {
            tracing::warn!(error = %e, "handshake: CONREQ verification rejected");
            return;
        }
    };

    // #1178: answer only what is addressed to us. Without this a CONREQ had no destination, so
    // EVERY daemon in range replied and spent RF before the initiator filtered the answers. Checked
    // BEFORE the callsign gate below, because the cheapest refusal is the one that never considers
    // keying the transmitter at all.
    if !req.is_addressed_to(&runtime_state.local_callsign) {
        tracing::debug!(
            peer = %req.station_id,
            dst = %req.dst_station,
            local = %runtime_state.local_callsign,
            "handshake: CONREQ addressed elsewhere; not answering"
        );
        return;
    }

    // Audit F6 (§97.119): replying with a CONACK keys the transmitter. Auto-ID is disabled without
    // a valid callsign, so an autonomous responder must not answer a CONREQ unidentified — refuse to
    // key up (and don't record a half-handshake the peer never sees completed).
    if !runtime_state.local_callsign_valid() {
        tracing::warn!(
            peer = %req.station_id,
            "handshake: heard a CONREQ but no valid station callsign is set; not transmitting a CONACK"
        );
        return;
    }

    // OTA-ACK key agreement (E7): if the peer advertised an ephemeral X25519 key, generate ours,
    // derive the shared ACK-MAC key, and arm the engine's ACK authentication. We advertise our
    // ephemeral public key back in the CONACK (signed) so the initiator derives the same key.
    let kex_public = if let Ok(peer_kex) = <[u8; 32]>::try_from(req.kex_pubkey.as_slice()) {
        let (kex_secret, kex_public) = generate_kex_ephemeral();
        engine.set_ack_mac_key(Some(derive_ack_key(&kex_secret, &peer_kex)));
        kex_public.to_vec()
    } else {
        Vec::new()
    };

    // Reply with a signed CONACK echoing the session id and advertising our grid + OTA ladder.
    let (ota_name, ota_fp) = runtime_state
        .local_ota_ladder
        .clone()
        .unwrap_or_else(|| (String::new(), 0));
    match ConAck::create(
        &ConAckParams {
            station_id: &runtime_state.local_callsign,
            selected_mode: SigningMode::Normal,
            // Transcript binding over the CONREQ exactly as received, signature included.
            conreq_hash: openpulse_core::handshake::conreq_hash(bytes),
            station_grid: &runtime_state.local_grid,
            profile_name: &ota_name,
            profile_fingerprint: ota_fp,
            timestamp_ms: now_ms,
            kex_pubkey: &kex_public,
        },
        &runtime_state.station_seed,
    ) {
        Ok(frame) => {
            // F5, third route to the same situation (#1262): a refused PTT assert means the CONACK
            // never went out, so recording the peer would treat it as verified while it never
            // received our reply and will never consider the session established.
            if !transmit_handshake_frame(engine, &runtime_state.ptt, Some(event_tx), mode, &frame) {
                tracing::warn!("handshake: CONACK not transmitted; not recording the peer");
                return;
            }
        }
        Err(e) => {
            // F5: do NOT fall through to `record_verified_peer`. The §97.119 guard above refuses to
            // record a half-handshake when we cannot reply — this arm is the same situation arrived
            // at differently, and recording here would leave us treating a peer as verified while
            // it never received a CONACK and will never consider the session established.
            tracing::warn!(error = %e, "handshake: CONACK create failed; not recording the peer");
            return;
        }
    }

    record_verified_peer(
        runtime_state,
        event_tx,
        &req.station_id,
        &req.station_grid,
        &req.pubkey,
        &req.profile_name,
        req.profile_fingerprint,
    );
}

/// Initiator side of the signed handshake: verify the peer's CONACK against the in-flight CONREQ,
/// then record the proven peer identity and clear the pending handshake.
#[cfg(not(target_arch = "wasm32"))]
fn handle_inbound_conack(
    bytes: &[u8],
    runtime_state: &mut RuntimeControlState,
    event_tx: &Arc<broadcast::Sender<ControlEvent>>,
    engine: &mut ModemEngine,
) {
    let ack = match ConAck::decode(bytes) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "handshake: CONACK decode failed");
            return;
        }
    };
    let Some(pending) = runtime_state.pending_handshake.clone() else {
        tracing::debug!("handshake: CONACK received with no pending CONREQ; ignoring");
        return;
    };
    // The CONACK must be bound to the CONREQ we actually sent. `verify_conack` re-checks this, but
    // gating here avoids tearing down a pending handshake on an unrelated peer's CONACK.
    //
    // v2 binds by HASH OVER THE TRANSMITTED CONREQ rather than echoing the session id: the id is
    // cleartext and time-based, so guessable inside the handshake window, while the hash covers the
    // whole frame including our own `kex_pubkey`.
    if ack.conreq_hash != openpulse_core::handshake::conreq_hash(&pending.conreq_bytes) {
        tracing::debug!(
            peer = %ack.station_id,
            "handshake: CONACK is not bound to our CONREQ; ignoring"
        );
        return;
    }
    // The CONACK must come from the station we actually dialed (audit F2). The session id is cleartext and
    // time-based (guessable within the handshake window), so without this an attacker who races a CONACK
    // echoing it — under their own callsign — would be recorded as the peer the operator meant to reach.
    if ack.station_id != pending.peer_callsign {
        tracing::warn!(
            got = %ack.station_id,
            dialed = %pending.peer_callsign,
            "handshake: CONACK from a different station than dialed; ignoring"
        );
        return;
    }
    if let Err(e) = verify_conack(
        bytes,
        &pending.conreq_bytes,
        &pending.offered_modes,
        &runtime_state.trust_store,
        PolicyProfile::Permissive,
        SigningMode::Normal,
        Some(Freshness {
            now_ms: unix_now_ms(),
            max_skew_ms: HANDSHAKE_MAX_SKEW_MS,
        }),
    ) {
        tracing::warn!(peer = %ack.station_id, error = %e, "handshake: CONACK verification rejected");
        runtime_state.pending_handshake = None;
        return;
    }
    // OTA-ACK key agreement (E7): derive the shared ACK-MAC key from our stored ephemeral secret and the
    // peer's CONACK ephemeral public key (both were covered by the verified signatures), and arm the
    // engine. `verify_conack` above bound `ack.pubkey` to the dialed identity, so the ECDH is authenticated.
    if let Ok(peer_kex) = <[u8; 32]>::try_from(ack.kex_pubkey.as_slice()) {
        engine.set_ack_mac_key(Some(derive_ack_key(&pending.kex_secret, &peer_kex)));
    }
    record_verified_peer(
        runtime_state,
        event_tx,
        &ack.station_id,
        &ack.station_grid,
        &ack.pubkey,
        &ack.profile_name,
        ack.profile_fingerprint,
    );
    runtime_state.pending_handshake = None;
}

/// Store a freshly-verified peer identity, stamp the verified grid onto the in-flight logbook QSO,
/// and emit a `PeerVerified` event for clients.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
fn record_verified_peer(
    runtime_state: &mut RuntimeControlState,
    event_tx: &Arc<broadcast::Sender<ControlEvent>>,
    callsign: &str,
    grid: &str,
    pubkey: &[u8],
    peer_profile_name: &str,
    peer_profile_fingerprint: u64,
) {
    // Ladder-compatibility guard: compare the peer's advertised OTA ladder identity to ours. Only a
    // definite mismatch (both sides advertised, fingerprints differ) suppresses OTA — an unadvertised
    // side leaves it undetermined (None), so OTA-without-handshake keeps working.
    let profile_compatible = match (&runtime_state.local_ota_ladder, peer_profile_fingerprint) {
        (Some((_, local_fp)), peer_fp) if peer_fp != 0 => Some(*local_fp == peer_fp),
        _ => None,
    };
    if profile_compatible == Some(false) {
        let local_fp = runtime_state
            .local_ota_ladder
            .as_ref()
            .map(|(_, fp)| *fp)
            .unwrap_or(0);
        tracing::warn!(
            peer = %callsign,
            peer_profile = %peer_profile_name,
            peer_fingerprint = format!("{peer_profile_fingerprint:016x}"),
            local_fingerprint = format!("{local_fp:016x}"),
            "handshake: peer OTA rate ladder differs from ours; disabling adaptive OTA (fixed mode)"
        );
    }
    let peer = VerifiedPeer {
        callsign: callsign.to_string(),
        grid: grid.to_string(),
        pubkey: pubkey.to_vec(),
        profile_compatible,
    };
    // The per-callsign map is the authoritative store (audit E5) so file-transfer offer verification
    // binds to the true sender; `last_verified_callsign` just points at this most-recent entry for the
    // OTA/QSY reads, which act on the current link.
    runtime_state
        .verified_peers
        .insert(callsign.to_string(), peer);
    runtime_state.last_verified_callsign = Some(callsign.to_string());
    // Prefer the on-air verified grid over the config peer_grids fallback for this QSO.
    if !grid.is_empty() {
        runtime_state.logbook.set_pending_peer_grid(grid);
    }
    tracing::info!(peer = %callsign, grid = %grid, "handshake: peer identity verified");
    let _ = event_tx.send(ControlEvent::PeerVerified {
        callsign: callsign.to_string(),
        grid: grid.to_string(),
    });
}

/// Abandon an unanswered CONREQ once [`HANDSHAKE_TIMEOUT`] elapses. Called from the daemon
/// receive loop each tick; emits a `CommandError` so the operator sees the handshake gave up.
#[cfg(not(target_arch = "wasm32"))]
pub fn expire_pending_handshake(
    runtime_state: &mut RuntimeControlState,
    event_tx: &Arc<broadcast::Sender<ControlEvent>>,
) {
    // Drop stale partial reassemblies (e.g. a handshake that lost a fragment).
    runtime_state.handshake_sar.expire();
    if let Some(p) = &runtime_state.pending_handshake {
        if p.started_at.elapsed() >= HANDSHAKE_TIMEOUT {
            let peer = p.peer_callsign.clone();
            runtime_state.pending_handshake = None;
            tracing::warn!(peer = %peer, "handshake: CONACK timed out; no verified identity");
            let _ = event_tx.send(ControlEvent::CommandError {
                command: "connect_peer".to_string(),
                reason: format!("handshake timed out awaiting CONACK from {peer}"),
            });
        }
    }
}

/// Auto-initiate a QSY when the receiver notch confirms a persistent **in-band** interferer — one
/// a notch can't remove. Called from the main loop after each receive tick. No-op unless
/// `auto_enabled`, the engine reports an in-band interferer, candidate frequencies are configured,
/// and no QSY negotiation is already in flight. Reuses the standard initiator path
/// ([`QsySession::initiate`] + [`execute_qsy_actions`]), so the peer responds over RF as usual.
#[cfg(not(target_arch = "wasm32"))]
pub async fn maybe_qsy_on_interference(
    auto_enabled: bool,
    runtime_state: &mut RuntimeControlState,
    rig_controller: Option<&mut (dyn CatController + Send)>,
    event_tx: &Arc<broadcast::Sender<ControlEvent>>,
    active_mode: &SharedMode,
    engine: &mut ModemEngine,
) {
    if !auto_enabled || runtime_state.qsy_session.is_some() {
        return;
    }
    if engine.in_band_interferers().is_empty() {
        return;
    }
    let interferers: Vec<f32> = engine.in_band_interferers().to_vec();
    let candidates = runtime_state.qsy_candidate_freqs.clone();
    if candidates.is_empty() {
        tracing::warn!(
            ?interferers,
            "in-band interference confirmed but no QSY candidates configured (qsy.candidate_freqs_hz); cannot auto-QSY"
        );
        // Clear so the warning doesn't repeat every tick until the tracker decays.
        engine.clear_in_band_interferers();
        return;
    }

    // Audit F6 (§97.119): auto-QSY keys the transmitter to send the QSY request; without a valid
    // MYID the daemon can't auto-ID, so refuse to initiate rather than transmit unidentified.
    if !runtime_state.local_callsign_valid() {
        tracing::warn!(
            ?interferers,
            "in-band interference confirmed but no valid station callsign is set; not auto-initiating QSY"
        );
        engine.clear_in_band_interferers();
        return;
    }

    tracing::warn!(
        ?interferers,
        "in-band interference confirmed — auto-initiating QSY"
    );
    let mut session =
        QsySession::new_initiator().with_switchover_offset_s(runtime_state.qsy_switchover_offset_s);
    let actions = match session.initiate(candidates) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "auto-QSY initiate failed");
            return;
        }
    };
    let mode = active_mode.lock().await.clone();
    // Clone before the call: `runtime_state` is borrowed for the session across it.
    let qsy_ptt = runtime_state.ptt.clone();
    let qsy_dwell = runtime_state.qsy_scan_dwell_ms;
    let qsy_seed = runtime_state.station_seed;
    execute_qsy_actions(
        actions,
        &mut session,
        engine,
        rig_controller,
        event_tx,
        &qsy_ptt,
        &qsy_seed,
        &mode,
        qsy_dwell,
    )
    .await;
    // #1252: pin the key on the INITIATOR side too. Without this the per-line fallback to
    // `last_verified_peer()` lets a stranger's CONREQ displace the link peer mid-negotiation, after
    // which their signed REJECT aborts our move and their signed VOTE steers it.
    runtime_state.qsy_peer_pubkey = runtime_state
        .last_verified_peer()
        .and_then(|p| <[u8; 32]>::try_from(p.pubkey.as_slice()).ok());
    runtime_state.qsy_session = Some(session);
    runtime_state.qsy_session_started = Some(Instant::now());
    // The old interferer no longer applies once we move; start fresh so we don't re-trigger.
    engine.clear_in_band_interferers();
}

fn maybe_relay_forward(
    payload: &[u8],
    mode: &str,
    runtime_state: &mut RuntimeControlState,
    engine: &mut ModemEngine,
    event_tx: &broadcast::Sender<ControlEvent>,
) {
    use openpulse_core::wire_query::WireEnvelope;

    // Audit F6 (§97.119): retransmitting is an on-air emission of THIS station. Without a valid MYID
    // the daemon can't auto-ID, so a relay must not forward unidentified.
    //
    // It now also keys the transmitter, which this comment claimed before #1262 and the code did not
    // do: the forward reached the sound card unkeyed, so on a VOX rig it went on air outside the
    // watchdog with no `PttChanged` edge.
    if !runtime_state.local_callsign_valid() {
        return;
    }
    // Clone before the `&mut runtime_state.relay_forwarder` borrow below (Arc clone, free).
    let ptt = runtime_state.ptt.clone();
    let Some(ref mut fwd) = runtime_state.relay_forwarder else {
        return;
    };
    let Ok(envelope) = WireEnvelope::decode(payload) else {
        return;
    };
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let forwarded = fwd.forward(&envelope, now_ms);
    match forwarded {
        Ok(out_envelope) => match out_envelope.encode() {
            Ok(out_bytes) => {
                tracing::info!(
                    session_id = ?out_envelope.session_id,
                    hop_index = out_envelope.hop_index,
                    bytes = out_bytes.len(),
                    "relay: forwarding envelope"
                );
                let _ = keyed_transmit(&ptt, Some(event_tx), "relay", || {
                    engine.transmit(&out_bytes, mode, None)
                });
            }
            Err(e) => tracing::warn!(error = %e, "relay: envelope encode failed"),
        },
        Err(e) => tracing::info!(reason = ?e, "relay: dropping envelope"),
    }
}
///
/// This complements [`dispatch_command`], which updates shared daemon state and
/// forwards commands to the caller. Commands without runtime support emit a
/// [`ControlEvent::CommandError`] instead of failing silently.
#[cfg(not(target_arch = "wasm32"))]
/// The QSY wire magic, taken from the signing registry rather than typed here (#1162).
fn qsy_wire_magic() -> &'static str {
    std::str::from_utf8(openpulse_core::signing_domain::SigningDomain::QsyLine.tag())
        .unwrap_or("OPQS")
}

pub async fn apply_command_to_engine(
    cmd: &ControlCommand,
    engine: &mut ModemEngine,
    active_mode: &SharedMode,
    event_tx: &Arc<broadcast::Sender<ControlEvent>>,
    rig_controller: Option<&mut (dyn CatController + Send)>,
    runtime_state: &mut RuntimeControlState,
) {
    match cmd {
        ControlCommand::SetMode { mode } => {
            if engine.plugins().get(mode).is_some() {
                *active_mode.lock().await = mode.clone();
            } else {
                let _ = event_tx.send(ControlEvent::CommandError {
                    command: "set_mode".to_string(),
                    reason: format!("unsupported mode '{mode}'"),
                });
            }
        }
        ControlCommand::SetTxAttenuation { db, band } => match band {
            // Per-band override: remember it, and apply immediately only when it is the current band.
            Some(label) => {
                runtime_state
                    .tx_attenuation_bands
                    .insert(label.clone(), *db);
                let on_this_band = runtime_state
                    .last_freq_hz
                    .and_then(openpulse_qsy::bandplan::band_label_for_hz)
                    == Some(label.as_str());
                if on_this_band {
                    engine.set_tx_attenuation_db(*db);
                }
            }
            // Global default: takes effect now, unless a per-band override matches the current band.
            None => {
                runtime_state.tx_attenuation_default = *db;
                match runtime_state.last_freq_hz {
                    Some(hz) => apply_band_attenuation(engine, runtime_state, hz),
                    None => engine.set_tx_attenuation_db(*db),
                }
            }
        },
        ControlCommand::SetConfig { config } => {
            if engine.plugins().get(&config.mode).is_some() {
                *active_mode.lock().await = config.mode.clone();
            } else {
                let _ = event_tx.send(ControlEvent::CommandError {
                    command: "set_config".to_string(),
                    reason: format!("unsupported mode '{}'", config.mode),
                });
            }
            // Sets the global default; per-band overrides persist and re-apply on retune.
            runtime_state.tx_attenuation_default = config.tx_attenuation_db;
            match runtime_state.last_freq_hz {
                Some(hz) => apply_band_attenuation(engine, runtime_state, hz),
                None => engine.set_tx_attenuation_db(config.tx_attenuation_db),
            }
        }
        ControlCommand::PttAssert => {
            // #1263: `handle_ptt_command` now takes an OWNED key (`key_as_manual`), which arms the
            // watchdog itself — so the separate `arm()` that used to live here is gone. Re-arming
            // would also have defeated the idempotence the assert path needs: a second `ptt-assert`
            // must not push the 180 s deadline out, or re-asserting every 170 s never expires.
            //
            // This arm is reached only when the key was actually taken (a hard failure sets
            // `ptt_hard_failed` and skips the dispatch), so the edge is announced here as before.
            let _ = event_tx.send(ControlEvent::PttChanged { active: true });
        }
        ControlCommand::PttRelease => {
            // The hardware release and disarm both happen in `force_release_manual` — the operator's
            // hard override, which drops the transmitter whoever holds it (#1263).
            let _ = event_tx.send(ControlEvent::PttChanged { active: false });
        }
        ControlCommand::ConnectPeer { callsign } => {
            // REFUSE THE WILDCARD DIAL (#1203). `"*"` is legal in `dst_station` — it is the CONREQ's
            // deliberate broadcast form, and `ConReq::is_addressed_to` matches every station — so
            // without this guard one operator command broadcasts a CONREQ that EVERY daemon in
            // range answers, which is the exact RF cost #1178 added `dst_station` to prevent.
            //
            // And the dial could never succeed anyway: the CONACK filter compares `ack.station_id`
            // against the literal `pending.peer_callsign`, so every reply to a `"*"` dial is
            // rejected and the operator waits out a 30 s timeout having already spent the channel.
            // Unreachable by construction AND maximally expensive, so refusing removes nothing that
            // works — verified: no production site sets `dst_station` to `"*"`; the wildcard is a
            // receiver-side concept with no sender.
            //
            // A real broadcast/discovery dial is issue #1203 option (b) and a separate design pass:
            // it would accept the first responder whose CONACK verifies, rather than matching a
            // literal callsign. Refusing here does not foreclose it.
            if callsign == "*" {
                tracing::warn!("connect_peer: refusing the wildcard dial; not transmitting");
                let _ = event_tx.send(ControlEvent::CommandError {
                    command: "connect_peer".to_string(),
                    reason: "cannot dial \"*\": a wildcard CONREQ is answered by every station in                              range and no reply can ever match it. Dial a specific callsign."
                        .to_string(),
                });
                return;
            }
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            // Use the configured trust level when the peer is in the trust store.
            // Unknown peers (not yet in store) get Full so the session proceeds and
            // trust is established at handshake time.  Revoked peers are rejected.
            let stored_trust = runtime_state.trust_store.trust_level(callsign);
            let key_trust = if stored_trust == PublicKeyTrustLevel::Unknown {
                PublicKeyTrustLevel::Full
            } else {
                stored_trust
            };
            let params = SecureSessionParams {
                local_minimum_mode: SigningMode::Normal,
                peer_supported_modes: vec![SigningMode::Normal, SigningMode::Psk],
                key_trust,
                certificate_source: CertificateSource::OutOfBand,
                psk_validated: false,
            };

            // BUILD THE CONREQ BEFORE ANNOUNCING ANYTHING (#1199).
            //
            // This used to run last, after `begin_secure_session`, `RfConnectionChanged`, the
            // logbook QSO and the QSY token — and its failure arm only logged. A station whose
            // callsign exceeds `caps::STATION_ID`, a peer callsign that exceeds it, or an empty
            // `dst_station` therefore produced a session that announced itself connected, opened a
            // QSO, and could still key the transmitter, while NO CONREQ was ever sent. The peer
            // never learned the session existed, and because `pending_handshake` was never set the
            // handshake-timeout `CommandError` could not fire either: a lost CONACK reported
            // "timed out" after 30 s, a create failure reported nothing, ever.
            //
            // Ordering is the fix, not a louder log: an error at the old position would still leave
            // the announcement, the QSO and the token behind. Nothing is transmitted on failure, so
            // the peer cannot participate — a "connection" the peer will never hear about is not a
            // degraded mode worth keeping.
            //
            // A fixed-width session id, NOT `"{callsign}-{now_ms}"`: that format coupled the id's
            // length to the callsign's and overflowed a cap sized from a 6-character callsign (F1).
            // The callsign is already in the frame as `station_id`.
            let session_id = now_ms;
            let (ota_name_pre, ota_fp_pre) = runtime_state
                .local_ota_ladder
                .clone()
                .unwrap_or_else(|| (String::new(), 0));
            // Ephemeral X25519 for OTA-ACK key agreement (E7): advertise the public key in the
            // signed CONREQ, keep the secret to derive the ACK-MAC key from the peer's CONACK.
            let (kex_secret, kex_public) = generate_kex_ephemeral();
            let offered_modes = vec![SigningMode::Normal];
            let conreq_frame = match ConReq::create(
                &ConReqParams {
                    station_id: &runtime_state.local_callsign,
                    // #1178: address the station we are dialling, so no other daemon in range
                    // spends RF answering a request that was never for it.
                    dst_station: callsign,
                    signing_modes: offered_modes.clone(),
                    session_id,
                    station_grid: &runtime_state.local_grid,
                    profile_name: &ota_name_pre,
                    profile_fingerprint: ota_fp_pre,
                    timestamp_ms: now_ms,
                    kex_pubkey: &kex_public,
                },
                &runtime_state.station_seed,
            ) {
                Ok(frame) => frame,
                Err(e) => {
                    tracing::error!(error = %e, "connect_peer: CONREQ could not be built; not connecting");
                    let _ = event_tx.send(ControlEvent::CommandError {
                        command: "connect_peer".to_string(),
                        reason: format!(
                            "cannot build the signed handshake, so no connection was opened: {e}"
                        ),
                    });
                    return;
                }
            };

            match engine.begin_secure_session(params, now_ms) {
                Ok(_) => {
                    let _ = event_tx.send(ControlEvent::RfConnectionChanged {
                        connected: true,
                        peer: Some(callsign.clone()),
                    });

                    // Open a logbook QSO (finalized + appended on disconnect).
                    let mode = active_mode.lock().await.clone();
                    let freq = runtime_state.last_freq_hz;
                    runtime_state
                        .logbook
                        .begin_qso(callsign, &mode, freq, now_ms);

                    let token = format!("qsy-{now_ms}");
                    runtime_state.qsy_pending_token = Some(token.clone());
                    let _ = event_tx.send(ControlEvent::QsyPending { token });

                    // The frame was built above; transmit it and arm the handshake timeout.
                    transmit_handshake_frame(
                        engine,
                        &runtime_state.ptt,
                        Some(event_tx),
                        &mode,
                        &conreq_frame,
                    );
                    runtime_state.pending_handshake = Some(PendingHandshake {
                        session_id,
                        peer_callsign: callsign.clone(),
                        started_at: Instant::now(),
                        kex_secret,
                        conreq_bytes: conreq_frame,
                        offered_modes,
                    });
                }
                Err(err) => {
                    let _ = event_tx.send(ControlEvent::CommandError {
                        command: "connect_peer".to_string(),
                        reason: format!("secure session start failed: {err}"),
                    });
                }
            }
        }
        ControlCommand::DisconnectPeer => {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            match engine.end_secure_session(now_ms) {
                Ok(()) => {
                    runtime_state.qsy_pending_token = None;
                    // Finalize + append the logbook QSO (opt-in; failures don't affect the session).
                    let rx_snr = engine.last_rx_snr_db();
                    if let Err(e) = runtime_state.logbook.end_qso(now_ms, rx_snr) {
                        tracing::warn!(error = %e, "logbook: failed to append ADIF record");
                    }
                    let _ = event_tx.send(ControlEvent::RfConnectionChanged {
                        connected: false,
                        peer: None,
                    });
                }
                Err(err) => {
                    let _ = event_tx.send(ControlEvent::CommandError {
                        command: "disconnect_peer".to_string(),
                        reason: format!("secure session end failed: {err}"),
                    });
                }
            }
        }
        ControlCommand::SendMessage { body, .. } => {
            // Plain fixed-mode transmit. When an OTA session is active, the daemon's
            // run loop (`server::run`) intercepts SendMessage upstream to drive the
            // receiver-led OTA send with the real-radio PTT turnaround, so this branch
            // only runs for the non-OTA case.
            let mode = active_mode.lock().await.clone();
            // Compress on the wire when enabled; the peer's rx tick unpacks the self-describing frame.
            let payload = if runtime_state.compress_tx {
                openpulse_core::compression::pack(body.as_bytes())
            } else {
                body.as_bytes().to_vec()
            };
            // The mode lock above is awaited BEFORE the guard is taken: the guard must not cross an
            // await, and this shape makes that unwriteable rather than merely documented.
            if keyed_transmit(&runtime_state.ptt, Some(event_tx), "send-message", || {
                engine.transmit(&payload, &mode, None)
            })
            .is_err()
            {
                let _ = event_tx.send(ControlEvent::CommandError {
                    command: "send_message".to_string(),
                    reason: format!("rf dispatch failed in mode '{mode}'"),
                });
            }
        }
        ControlCommand::SetFreq { rig, freq_hz } => {
            if rig != "rigctld" {
                let _ = event_tx.send(ControlEvent::CommandError {
                    command: "set_freq".to_string(),
                    reason: format!("unsupported rig target '{rig}'"),
                });
                return;
            }

            let Some(controller) = rig_controller else {
                let _ = event_tx.send(ControlEvent::CommandError {
                    command: "set_freq".to_string(),
                    reason: "no rigctld controller configured".to_string(),
                });
                return;
            };

            match controller.set_frequency(*freq_hz) {
                Ok(()) => {
                    runtime_state.last_freq_hz = Some(*freq_hz);
                    // Restore the per-band DCD squelch + TX attenuation for the new frequency.
                    apply_band_squelch(engine, runtime_state, *freq_hz);
                    apply_band_attenuation(engine, runtime_state, *freq_hz);
                    let _ = event_tx.send(ControlEvent::RigStatus {
                        rig: rig.clone(),
                        freq_hz: *freq_hz,
                        mode: "CAT".to_string(),
                        power_w: None,
                        alc: None,
                        swr: None,
                    });
                }
                Err(err) => {
                    let _ = event_tx.send(ControlEvent::CommandError {
                        command: "set_freq".to_string(),
                        reason: format!("rigctld set_frequency failed: {err}"),
                    });
                }
            }
        }
        ControlCommand::AcceptQsy { token } => {
            let token = token.trim();
            if token.is_empty() {
                let _ = event_tx.send(ControlEvent::CommandError {
                    command: "accept_qsy".to_string(),
                    reason: "empty token".to_string(),
                });
                return;
            }

            if runtime_state.qsy_pending_token.as_deref() != Some(token) {
                let _ = event_tx.send(ControlEvent::CommandError {
                    command: "accept_qsy".to_string(),
                    reason: format!("unknown pending token '{token}'"),
                });
                return;
            }

            match runtime_state.qsy_decisions.get(token) {
                Some(true) => {
                    let _ = event_tx.send(ControlEvent::CommandError {
                        command: "accept_qsy".to_string(),
                        reason: format!("token '{token}' already accepted"),
                    });
                }
                Some(false) => {
                    let _ = event_tx.send(ControlEvent::CommandError {
                        command: "accept_qsy".to_string(),
                        reason: format!("token '{token}' already rejected"),
                    });
                }
                None => {
                    runtime_state.qsy_decisions.insert(token.to_string(), true);
                    runtime_state.qsy_pending_token = None;
                    let _ = event_tx.send(ControlEvent::QsyDecision {
                        token: token.to_string(),
                        accepted: true,
                    });

                    let candidates = runtime_state.qsy_candidate_freqs.clone();
                    if candidates.is_empty() {
                        let _ = event_tx.send(ControlEvent::CommandError {
                            command: "accept_qsy".to_string(),
                            reason: "no candidate frequencies configured (qsy.candidate_freqs_hz)"
                                .to_string(),
                        });
                        return;
                    }

                    let mut session = QsySession::new_initiator()
                        .with_switchover_offset_s(runtime_state.qsy_switchover_offset_s);
                    let actions = match session.initiate(candidates) {
                        Ok(a) => a,
                        Err(e) => {
                            let _ = event_tx.send(ControlEvent::CommandError {
                                command: "accept_qsy".to_string(),
                                reason: format!("QSY session initiate failed: {e}"),
                            });
                            return;
                        }
                    };

                    let mode = active_mode.lock().await.clone();
                    // Clone before the call: `runtime_state` is borrowed for the session across it.
                    let qsy_ptt = runtime_state.ptt.clone();
                    let qsy_dwell = runtime_state.qsy_scan_dwell_ms;
                    let qsy_seed = runtime_state.station_seed;
                    execute_qsy_actions(
                        actions,
                        &mut session,
                        engine,
                        rig_controller,
                        event_tx,
                        &qsy_ptt,
                        &qsy_seed,
                        &mode,
                        qsy_dwell,
                    )
                    .await;

                    runtime_state.qsy_peer_pubkey = runtime_state
                        .last_verified_peer()
                        .and_then(|p| <[u8; 32]>::try_from(p.pubkey.as_slice()).ok());
                    runtime_state.qsy_session = Some(session);
                    runtime_state.qsy_session_started = Some(Instant::now());
                }
            }
        }
        ControlCommand::RejectQsy { token } => {
            let token = token.trim();
            if token.is_empty() {
                let _ = event_tx.send(ControlEvent::CommandError {
                    command: "reject_qsy".to_string(),
                    reason: "empty token".to_string(),
                });
                return;
            }

            if runtime_state.qsy_pending_token.as_deref() != Some(token) {
                let _ = event_tx.send(ControlEvent::CommandError {
                    command: "reject_qsy".to_string(),
                    reason: format!("unknown pending token '{token}'"),
                });
                return;
            }

            match runtime_state.qsy_decisions.get(token) {
                Some(true) => {
                    let _ = event_tx.send(ControlEvent::CommandError {
                        command: "reject_qsy".to_string(),
                        reason: format!("token '{token}' already accepted"),
                    });
                }
                Some(false) => {
                    let _ = event_tx.send(ControlEvent::CommandError {
                        command: "reject_qsy".to_string(),
                        reason: format!("token '{token}' already rejected"),
                    });
                }
                None => {
                    runtime_state.qsy_decisions.insert(token.to_string(), false);
                    runtime_state.qsy_pending_token = None;
                    let _ = event_tx.send(ControlEvent::QsyDecision {
                        token: token.to_string(),
                        accepted: false,
                    });
                }
            }
        }
        ControlCommand::EnableRepeater => {
            reap_finished_repeater(runtime_state, event_tx);
            if runtime_state.repeater_enabled {
                let _ = event_tx.send(ControlEvent::CommandError {
                    command: "enable_repeater".to_string(),
                    reason: "repeater already enabled".to_string(),
                });
                return;
            }

            // No repeater to run is a FAILED enable, not a quiet one (#1298). This arm used to warn
            // at `tracing::warn!` and then set `repeater_enabled = true` and emit
            // `RepeaterChanged { enabled: true }` anyway — so a daemon with no repeater reported one
            // as running, and no command sequence could get back to a truthful state.
            let Some(mut repeater) = runtime_state.repeater.take() else {
                let _ = event_tx.send(ControlEvent::CommandError {
                    command: "enable_repeater".to_string(),
                    reason: "no repeater is available — it was not built at startup (see the \
                             startup log for why), or a previous session ended and consumed it"
                        .to_string(),
                });
                return;
            };

            let stop = Arc::new(AtomicBool::new(false));
            let stop_clone = Arc::clone(&stop);
            // The thread owns the repeater, so when it exits the repeater is GONE. Report that:
            // without it, "relaying nothing because the band is quiet" and "the thread died" are
            // the same observation from outside (#1298).
            let thread_tx = Arc::clone(event_tx);
            let thread = std::thread::spawn(move || {
                if let Err(e) = repeater.run_full_duplex(stop_clone) {
                    tracing::warn!(error = %e, "cross-band repeater exited with error");
                    let _ = thread_tx.send(ControlEvent::CommandError {
                        command: "repeater".to_string(),
                        reason: format!("cross-band repeater stopped: {e}"),
                    });
                    // Only on the error path: a clean stop is already reported by DisableRepeater,
                    // and emitting there too would put two `false` edges on one transition.
                    let _ = thread_tx.send(ControlEvent::RepeaterChanged { enabled: false });
                }
            });
            runtime_state.repeater_stop = Some(stop);
            runtime_state.repeater_thread = Some(thread);

            runtime_state.repeater_enabled = true;
            let _ = event_tx.send(ControlEvent::RepeaterChanged { enabled: true });
        }
        ControlCommand::DisableRepeater => {
            reap_finished_repeater(runtime_state, event_tx);
            if !runtime_state.repeater_enabled {
                let _ = event_tx.send(ControlEvent::CommandError {
                    command: "disable_repeater".to_string(),
                    reason: "repeater already disabled".to_string(),
                });
                return;
            }

            if let Some(stop) = runtime_state.repeater_stop.take() {
                stop.store(true, Ordering::Relaxed);
            }
            if let Some(thread) = runtime_state.repeater_thread.take() {
                let _ = thread.join();
            }

            runtime_state.repeater_enabled = false;
            let _ = event_tx.send(ControlEvent::RepeaterChanged { enabled: false });
        }
        ControlCommand::StartOtaSession { profile } => {
            match openpulse_core::profile::SessionProfile::by_name(profile) {
                Some(p) => {
                    engine.start_ota_session(p);
                    let _ = event_tx.send(ota_status_event(engine));
                }
                None => {
                    let _ = event_tx.send(ControlEvent::CommandError {
                        command: "start_ota_session".to_string(),
                        reason: format!("unknown profile '{profile}'"),
                    });
                }
            }
        }
        ControlCommand::StopOtaSession => {
            engine.stop_ota_session();
            let _ = event_tx.send(ota_status_event(engine));
        }
        ControlCommand::OtaSetLevelBounds {
            min_level,
            max_level,
        } => {
            let parse = |o: &Option<String>| {
                o.as_deref()
                    .filter(|s| !s.is_empty())
                    .and_then(openpulse_core::rate::SpeedLevel::from_name)
            };
            engine.ota_set_level_bounds(parse(min_level), parse(max_level));
            let _ = event_tx.send(ota_status_event(engine));
        }
        ControlCommand::OtaLockLevel { level } => {
            match openpulse_core::rate::SpeedLevel::from_name(level) {
                Some(l) => {
                    engine.ota_lock_level(l);
                    let _ = event_tx.send(ota_status_event(engine));
                }
                None => {
                    let _ = event_tx.send(ControlEvent::CommandError {
                        command: "ota_lock_level".to_string(),
                        reason: format!("invalid level '{level}'"),
                    });
                }
            }
        }
        ControlCommand::OtaUnlock => {
            engine.ota_unlock();
            let _ = event_tx.send(ota_status_event(engine));
        }
        ControlCommand::OtaSetHysteresis {
            min_backlog,
            upgrade_hold_frames,
        } => {
            if let Some(b) = min_backlog {
                engine.set_min_backlog_for_upgrade(*b);
            }
            if let Some(f) = upgrade_hold_frames {
                engine.set_upgrade_hold_frames(*f);
            }
            // These configure RateAdaptationPolicy, which the daemon's receiver-led OTA ladder
            // (OtaRateController) does not use — so they have no effect here (audit #2).
            tracing::warn!(
                "ota_set_hysteresis has no effect on the daemon's receiver-led OTA ladder \
                 (backlog/hold gating belongs to the sender-led policy the daemon does not run)"
            );
        }
        ControlCommand::OtaSetAggressiveness { preset } => {
            match openpulse_core::rate::OtaAggressiveness::from_name(preset) {
                Some(p) => {
                    engine.set_ota_aggressiveness(p);
                    tracing::warn!(
                        preset = p.name(),
                        "ota_set_aggressiveness has no effect on the daemon's receiver-led OTA ladder \
                         (it configures the sender-led policy the daemon does not run)"
                    );
                }
                None => {
                    let _ = event_tx.send(ControlEvent::CommandError {
                        command: "ota_set_aggressiveness".to_string(),
                        reason: format!(
                            "unknown preset '{preset}' (conservative|balanced|aggressive)"
                        ),
                    });
                }
            }
        }
        ControlCommand::SetDcdSquelch { threshold } => {
            if threshold.is_finite() && *threshold >= 0.0 {
                engine.set_dcd_squelch(*threshold);
            } else {
                let _ = event_tx.send(ControlEvent::CommandError {
                    command: "set_dcd_squelch".to_string(),
                    reason: format!("invalid threshold {threshold}"),
                });
            }
        }
        ControlCommand::SetCessb { enabled } => {
            engine.set_cessb_enabled(*enabled);
        }
        ControlCommand::SetNotch { enabled } => {
            if *enabled {
                engine.enable_notch();
            } else {
                engine.disable_notch();
            }
        }
        ControlCommand::SetAgc { enabled } => {
            if *enabled {
                engine.enable_agc();
            } else {
                engine.disable_agc();
            }
        }
        ControlCommand::SetLogbook { enabled } => {
            runtime_state.logbook.set_enabled(*enabled);
        }
        // Receive-side file-transfer decisions act on the active session (engine + event_tx are here).
        ControlCommand::AcceptFile { transfer_id } => {
            let mode = active_mode.lock().await.clone();
            filexfer::accept_offer(*transfer_id, runtime_state, event_tx, &mode);
        }
        ControlCommand::RejectFile { transfer_id } => {
            let mode = active_mode.lock().await.clone();
            filexfer::reject_offer(*transfer_id, runtime_state, event_tx, &mode);
        }
        ControlCommand::CancelFile { transfer_id } => {
            let mode = active_mode.lock().await.clone();
            filexfer::cancel_transfer(*transfer_id, runtime_state, event_tx, &mode);
        }
        ControlCommand::SendFile { to, path } => {
            let mode = active_mode.lock().await.clone();
            filexfer::send_file(to, path, runtime_state, event_tx, &mode);
        }
        ControlCommand::EnableDiscovery => set_discovery_enabled(true, runtime_state, event_tx),
        ControlCommand::DisableDiscovery => set_discovery_enabled(false, runtime_state, event_tx),
        ControlCommand::RendezvousWith { callsign } => {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            start_rendezvous_cmd(callsign, runtime_state, event_tx, now_ms);
        }
        ControlCommand::ListStations => emit_station_list(runtime_state, event_tx),
        ControlCommand::ListPeers => {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            emit_peer_list(runtime_state, event_tx, now_ms);
        }
        ControlCommand::ListFiles => emit_file_list(runtime_state, event_tx),
        ControlCommand::GetPttState => {
            // Re-emit the current PTT state so a client that missed the edge can resync. PTT is keyed
            // exactly when the watchdog deadline is armed (`arm()` on every key path, cleared on
            // release), so it is the single source of truth for the logical PTT state.
            let active = runtime_state.ptt.is_keyed();
            let _ = event_tx.send(ControlEvent::PttChanged { active });
        }
        // No live-modem side effects for these commands in the engine path.
        ControlCommand::SubscribeSpectrum { .. }
        | ControlCommand::GetConfig
        | ControlCommand::ListMessages
        | ControlCommand::GetMessage { .. }
        | ControlCommand::DeleteMessage { .. } => {}
    }
}

/// JS8 discovery lifecycle-state label for [`ControlEvent::DiscoveryStatus`].
pub(crate) fn discovery_state_label(state: openpulse_discovery::DiscoveryState) -> &'static str {
    use openpulse_discovery::DiscoveryState::*;
    match state {
        Inactive => "inactive",
        Activating => "activating",
        Dwelling => "dwelling",
    }
}

/// Emit a [`ControlEvent::DiscoveryStatus`] reflecting the runtime's current state (no-op when
/// discovery is not configured).
pub(crate) fn emit_discovery_status(
    runtime_state: &RuntimeControlState,
    event_tx: &Arc<broadcast::Sender<ControlEvent>>,
) {
    if let Some(rt) = runtime_state.discovery.as_ref() {
        let _ = event_tx.send(ControlEvent::DiscoveryStatus {
            state: discovery_state_label(rt.state()).to_string(),
            dial_freq_hz: rt.dial_freq_hz(),
            drift_bias_ms: rt.drift_bias_ms(),
        });
    }
}

/// Handle `EnableDiscovery`/`DisableDiscovery`: toggle the runtime and emit a `DiscoveryStatus`, or a
/// `CommandError` when discovery is not configured for this daemon.
fn set_discovery_enabled(
    on: bool,
    runtime_state: &mut RuntimeControlState,
    event_tx: &Arc<broadcast::Sender<ControlEvent>>,
) {
    if runtime_state.discovery.is_none() {
        let _ = event_tx.send(ControlEvent::CommandError {
            command: if on {
                "enable_discovery"
            } else {
                "disable_discovery"
            }
            .to_string(),
            reason: "JS8 discovery is not configured ([discovery] enabled = false)".to_string(),
        });
        return;
    }
    if let Some(rt) = runtime_state.discovery.as_mut() {
        let _ = rt.set_enabled(on); // outcome execution (retune) happens in the rx-tick loop
    }
    emit_discovery_status(runtime_state, event_tx);
}

/// Slots to wait for a rendezvous reply *after* our Propose has fully transmitted, before timing out
/// (`N × 15 s` for NORMAL; 16 ≈ 4 min). Must exceed the peer's receive + Accept-over round-trip
/// (~8–10 slots) so a well-behaved exchange never times out.
const RENDEZVOUS_TIMEOUT_SLOTS: u64 = 16;

/// A short (2-char base-36) rendezvous session token derived from the current time.
fn rendezvous_token(now_ms: u64) -> String {
    const B36: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let v = (now_ms % (36 * 36)) as usize;
    let bytes = [B36[v / 36], B36[v % 36]];
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Handle `RendezvousWith`: propose the current band's working channels to `peer` over JS8. Errors when
/// discovery is not configured, has no channels for the band, or is not in a TX-capable mode.
fn start_rendezvous_cmd(
    peer: &str,
    runtime_state: &mut RuntimeControlState,
    event_tx: &Arc<broadcast::Sender<ControlEvent>>,
    now_ms: u64,
) {
    let err = |reason: &str| {
        let _ = event_tx.send(ControlEvent::CommandError {
            command: "rendezvous_with".to_string(),
            reason: reason.to_string(),
        });
    };
    let Some(dial) = runtime_state.discovery.as_ref().map(|d| d.dial_freq_hz()) else {
        return err("JS8 discovery is not configured ([discovery] enabled = false)");
    };
    let channels: Vec<u8> = openpulse_qsy::bandplan::band_label_for_hz(dial)
        .and_then(|label| runtime_state.discovery_rendezvous_channels_hz.get(label))
        .map(|v| (0..v.len().min(u8::MAX as usize) as u8).collect())
        .unwrap_or_default();
    if channels.is_empty() {
        return err("no rendezvous channels configured for the current band");
    }
    let token = rendezvous_token(now_ms);
    if let Some(rt) = runtime_state.discovery.as_mut() {
        rt.start_rendezvous(peer, &token, channels, RENDEZVOUS_TIMEOUT_SLOTS);
        if !rt.rendezvous_active() {
            err("rendezvous requires a configured callsign and beacon/full discovery mode");
        }
    }
}

/// Handle `ListStations`: emit a `StationList` from the discovery table (empty when unconfigured).
fn emit_station_list(
    runtime_state: &RuntimeControlState,
    event_tx: &Arc<broadcast::Sender<ControlEvent>>,
) {
    let stations = runtime_state
        .discovery
        .as_ref()
        .map(|rt| {
            rt.stations()
                .iter()
                .map(|s| crate::protocol::StationSummary {
                    callsign: s.callsign.clone(),
                    grid: s.grid.clone().unwrap_or_default(),
                    snr_db: s.snr_db,
                    heard_count: s.heard_count,
                    last_heard_ms: s.last_heard_ms,
                    is_opulse: s.hint.is_some(),
                })
                .collect()
        })
        .unwrap_or_default();
    let _ = event_tx.send(ControlEvent::StationList { stations });
}

/// Emit this session's received-file list to the requesting client (`ListFiles`).
pub(crate) fn emit_file_list(
    runtime_state: &RuntimeControlState,
    event_tx: &Arc<broadcast::Sender<ControlEvent>>,
) {
    let _ = event_tx.send(ControlEvent::FileList {
        files: runtime_state.received_files.clone(),
    });
}

/// Upsert discovery's hinted (OpenPulse-marked) stations into the shared [`PeerCache`] via
/// `station_to_peer_record`. Plain JS8 stations map to `None` and are skipped. Idempotent — safe to
/// call whenever a station is (re)heard.
pub(crate) fn sync_discovered_peers(runtime_state: &mut RuntimeControlState, now_ms: u64) {
    let records: Vec<_> = runtime_state
        .discovery
        .as_ref()
        .map(|rt| {
            rt.stations()
                .iter()
                .filter_map(openpulse_discovery::station_to_peer_record)
                .collect()
        })
        .unwrap_or_default();
    for r in records {
        runtime_state.peer_cache.upsert(r, now_ms);
    }
}

/// Emit the shared cache's recognized OpenPulse peers (sorted by quality) to the requesting client.
pub(crate) fn emit_peer_list(
    runtime_state: &mut RuntimeControlState,
    event_tx: &Arc<broadcast::Sender<ControlEvent>>,
    now_ms: u64,
) {
    use openpulse_core::peer_cache::{TrustFilter, TrustLevel};
    let peers = runtime_state
        .peer_cache
        .query(0, 0, TrustFilter::Any, 256, now_ms)
        .into_iter()
        .map(|r| crate::protocol::PeerSummary {
            peer_id: r.peer_id,
            capability_mask: r.capability_mask,
            route_quality: r.route_quality,
            trust_level: match r.trust_level {
                TrustLevel::Unknown => "unknown",
                TrustLevel::Reduced => "reduced",
                TrustLevel::PskVerified => "psk_verified",
                TrustLevel::Verified => "verified",
            }
            .to_string(),
        })
        .collect();
    let _ = event_tx.send(ControlEvent::PeerList { peers });
}

/// Resolve the DCD squelch for `freq_hz` (per-band override → default) and apply it.
pub fn apply_band_squelch(
    engine: &mut ModemEngine,
    runtime_state: &RuntimeControlState,
    freq_hz: u64,
) {
    let threshold = openpulse_qsy::bandplan::band_label_for_hz(freq_hz)
        .and_then(|label| runtime_state.dcd_squelch_bands.get(label).copied())
        .unwrap_or(runtime_state.dcd_squelch_default);
    engine.set_dcd_squelch(threshold);
}

/// Resolve the TX attenuation for `freq_hz` (per-band override → global default) and apply it.
pub fn apply_band_attenuation(
    engine: &mut ModemEngine,
    runtime_state: &RuntimeControlState,
    freq_hz: u64,
) {
    let atten = openpulse_qsy::bandplan::band_label_for_hz(freq_hz)
        .and_then(|label| runtime_state.tx_attenuation_bands.get(label).copied())
        .unwrap_or(runtime_state.tx_attenuation_default);
    engine.set_tx_attenuation_db(atten);
}

/// Build an [`ControlEvent::OtaStatus`] snapshot from the engine's current OTA state.
pub fn ota_status_event(engine: &ModemEngine) -> ControlEvent {
    ControlEvent::OtaStatus {
        active: engine.ota_active(),
        tx_mode: engine.ota_tx_mode().map(|s| s.to_string()),
        tx_level: engine.ota_tx_level().map(|l| l.name()),
        tx_fec: format!("{:?}", engine.ota_tx_fec()).to_lowercase(),
        rx_recommended_level: engine.ota_rx_recommended_level().map(|l| l.name()),
        rx_confirmed_level: engine.ota_rx_confirmed_level().map(|l| l.name()),
        is_locked: engine.ota_is_locked(),
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[allow(clippy::field_reassign_with_default)]
mod command_apply_tests {
    use super::*;
    use bpsk_plugin::BpskPlugin;
    use openpulse_audio::LoopbackBackend;

    #[test]
    fn compression_ratio_is_none_before_any_payload() {
        assert_eq!(compression_ratio(0, 0), None);
    }

    #[test]
    fn compression_ratio_reports_compressed_over_raw() {
        // 1000 raw → 200 compressed = 0.20 (a 5:1 reduction).
        let r = compression_ratio(1000, 200).unwrap();
        assert!((r - 0.20).abs() < 1e-6, "got {r}");
    }

    #[test]
    fn compression_ratio_tracks_the_real_compressor_on_compressible_data() {
        // Highly compressible payload → the session compressor beats raw, so ratio < 1.
        let payload = vec![0x5Au8; 4096];
        let (compressed, _algo) = openpulse_core::compression::compress_if_smaller(&payload);
        let r = compression_ratio(payload.len() as u64, compressed.len() as u64).unwrap();
        assert!(
            r < 0.5,
            "repeated-byte payload should compress well, got {r}"
        );
    }

    /// Seed used by tests that play the QSY initiator.
    pub(super) const INITIATOR_SEED: [u8; 32] = [21u8; 32];

    /// A runtime state whose verified peer is `seed`'s public key, so a QSY line signed by that
    /// seed authenticates (#1252).
    pub(super) fn state_with_qsy_peer(seed: &[u8; 32]) -> RuntimeControlState {
        let pubkey = ed25519_dalek::SigningKey::from_bytes(seed)
            .verifying_key()
            .to_bytes()
            .to_vec();
        let mut rs = RuntimeControlState {
            local_callsign: "W1AW".into(), // valid MYID so the responder may key up (audit F6)
            ..RuntimeControlState::default()
        };
        rs.verified_peers.insert(
            "K1PEER".into(),
            VerifiedPeer {
                callsign: "K1PEER".into(),
                grid: String::new(),
                pubkey,
                profile_compatible: None,
            },
        );
        rs.last_verified_callsign = Some("K1PEER".into());
        rs
    }

    /// A signed QSY line stamped now, as the peer would send it.
    pub(super) fn signed_qsy_line(frame: &QsyFrame, seed: &[u8; 32]) -> String {
        encode_qsy_frame(frame, now_ms(), seed).expect("sign a QSY line")
    }

    fn test_engine() -> ModemEngine {
        let mut engine = ModemEngine::new(Box::new(LoopbackBackend::new()));
        engine.register_plugin(Box::new(BpskPlugin::new())).unwrap();
        engine
    }

    #[test]
    fn list_files_reports_this_sessions_received_files() {
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(8);
        let ev_tx = Arc::new(tx);
        let mut rs = RuntimeControlState::default();
        rs.received_files.push(crate::protocol::FileSummary {
            name: "report.txt".into(),
            from: "W1AW".into(),
            size: 100,
            verified: true,
            path: "/tmp/report.txt".into(),
            timestamp_secs: 1_700_000_000,
        });

        emit_file_list(&rs, &ev_tx);

        match rx.try_recv().expect("FileList emitted") {
            ControlEvent::FileList { files } => {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].name, "report.txt");
                assert!(files[0].verified);
            }
            other => panic!("expected FileList, got {other:?}"),
        }
    }

    async fn apply(
        cmd: ControlCommand,
        engine: &mut ModemEngine,
        rs: &mut RuntimeControlState,
        ev: &Arc<broadcast::Sender<ControlEvent>>,
    ) {
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        apply_command_to_engine(&cmd, engine, &active_mode, ev, None, rs).await;
    }

    #[tokio::test]
    async fn front_end_toggle_commands_reach_the_engine() {
        // SetNotch/SetAgc/SetCessb are the cross-cutting RX/TX front-end toggles (audit H1) — assert
        // each dispatch actually flips the engine state, not just serde-parses.
        let mut engine = test_engine();
        let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);
        let mut rs = RuntimeControlState::default();

        apply(
            ControlCommand::SetNotch { enabled: true },
            &mut engine,
            &mut rs,
            &ev,
        )
        .await;
        assert!(engine.is_notch_enabled());
        apply(
            ControlCommand::SetNotch { enabled: false },
            &mut engine,
            &mut rs,
            &ev,
        )
        .await;
        assert!(!engine.is_notch_enabled());

        apply(
            ControlCommand::SetAgc { enabled: true },
            &mut engine,
            &mut rs,
            &ev,
        )
        .await;
        assert!(engine.is_agc_enabled());
        apply(
            ControlCommand::SetAgc { enabled: false },
            &mut engine,
            &mut rs,
            &ev,
        )
        .await;
        assert!(!engine.is_agc_enabled());

        apply(
            ControlCommand::SetCessb { enabled: true },
            &mut engine,
            &mut rs,
            &ev,
        )
        .await;
        assert!(engine.cessb_enabled());
        apply(
            ControlCommand::SetCessb { enabled: false },
            &mut engine,
            &mut rs,
            &ev,
        )
        .await;
        assert!(!engine.cessb_enabled());
    }

    #[tokio::test]
    async fn set_dcd_squelch_rejects_invalid_threshold() {
        let mut engine = test_engine();
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);
        let mut rs = RuntimeControlState::default();

        apply(
            ControlCommand::SetDcdSquelch { threshold: 0.05 },
            &mut engine,
            &mut rs,
            &ev,
        )
        .await;
        apply(
            ControlCommand::SetDcdSquelch { threshold: -1.0 },
            &mut engine,
            &mut rs,
            &ev,
        )
        .await;

        let mut error_for = None;
        while let Ok(e) = rx.try_recv() {
            if let ControlEvent::CommandError { command, .. } = e {
                error_for = Some(command);
            }
        }
        assert_eq!(error_for.as_deref(), Some("set_dcd_squelch"));
    }

    #[tokio::test]
    async fn ptt_commands_track_state_and_emit_changed() {
        let mut engine = test_engine();
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);
        let mut rs = RuntimeControlState::default();

        // The manual path is split across two functions in production, and #1263 moved the arming
        // from this one to the other: `handle_ptt_command` (in `server.rs`) takes the OWNED key —
        // hardware assert plus watchdog arm — and `apply_command_to_engine` announces the edge. This
        // test used to call `apply` alone and assert `is_keyed()`, which meant it was exercising a
        // half-path that armed the watchdog with no hardware behind it. Driving both halves here is
        // more faithful; the hardware/refusal half has its own test in `server.rs`.
        rs.ptt.key_as_manual().expect("manual key");
        apply(ControlCommand::PttAssert, &mut engine, &mut rs, &ev).await;
        assert!(rs.ptt.is_keyed());
        assert_eq!(rs.ptt.held_by(), "manual");

        rs.ptt.force_release_manual();
        apply(ControlCommand::PttRelease, &mut engine, &mut rs, &ev).await;
        assert!(!rs.ptt.is_keyed());

        let mut states = Vec::new();
        while let Ok(e) = rx.try_recv() {
            if let ControlEvent::PttChanged { active } = e {
                states.push(active);
            }
        }
        assert_eq!(states, vec![true, false]);
    }

    #[tokio::test]
    async fn ota_set_level_bounds_emits_status() {
        let mut engine = test_engine();
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);
        let mut rs = RuntimeControlState::default();

        apply(
            ControlCommand::StartOtaSession {
                profile: "hpx500".into(),
            },
            &mut engine,
            &mut rs,
            &ev,
        )
        .await;
        while rx.try_recv().is_ok() {} // drain the start status

        apply(
            ControlCommand::OtaSetLevelBounds {
                min_level: Some("SL3".into()),
                max_level: Some("SL8".into()),
            },
            &mut engine,
            &mut rs,
            &ev,
        )
        .await;

        let mut got_status = false;
        while let Ok(e) = rx.try_recv() {
            if matches!(e, ControlEvent::OtaStatus { .. }) {
                got_status = true;
            }
        }
        assert!(got_status, "OtaSetLevelBounds must emit an OtaStatus");
    }

    #[tokio::test]
    async fn ota_commands_start_lock_and_report_status() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(32);
        let ev_tx = Arc::new(tx);
        let mut rs = RuntimeControlState::default();

        // Start an OTA session.
        apply_command_to_engine(
            &ControlCommand::StartOtaSession {
                profile: "hpx500".into(),
            },
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut rs,
        )
        .await;
        assert!(engine.ota_active());
        assert_eq!(engine.ota_tx_level().map(|l| l.name()), Some("SL2".into()));

        // Lock to SL4 → status reflects the lock.
        apply_command_to_engine(
            &ControlCommand::OtaLockLevel {
                level: "SL4".into(),
            },
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut rs,
        )
        .await;
        assert!(engine.ota_is_locked());
        assert_eq!(engine.ota_tx_level().map(|l| l.name()), Some("SL4".into()));

        // An OtaStatus event was emitted with the locked state.
        let mut saw_locked_status = false;
        while let Ok(ev) = rx.try_recv() {
            if let ControlEvent::OtaStatus {
                is_locked,
                tx_level,
                ..
            } = ev
            {
                if is_locked && tx_level.as_deref() == Some("SL4") {
                    saw_locked_status = true;
                }
            }
        }
        assert!(
            saw_locked_status,
            "expected an OtaStatus event with the SL4 lock"
        );

        // Unlock + stop.
        apply_command_to_engine(
            &ControlCommand::OtaUnlock,
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut rs,
        )
        .await;
        assert!(!engine.ota_is_locked());
        apply_command_to_engine(
            &ControlCommand::StopOtaSession,
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut rs,
        )
        .await;
        assert!(!engine.ota_active());
    }

    #[test]
    fn ladder_fingerprint_mismatch_suppresses_ota() {
        let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        let mut rs = RuntimeControlState {
            local_ota_ladder: Some(("hpx_hf".into(), 0xAAAA_AAAA_AAAA_AAAA)),
            ..RuntimeControlState::default()
        };
        let key = [7u8; 32];

        // Matching fingerprint → compatible, OTA not suppressed.
        record_verified_peer(
            &mut rs,
            &ev_tx,
            "W1AW",
            "",
            &key,
            "hpx_hf",
            0xAAAA_AAAA_AAAA_AAAA,
        );
        assert_eq!(
            rs.last_verified_peer().unwrap().profile_compatible,
            Some(true)
        );
        assert!(!rs.ota_suppressed_by_peer());

        // Diverged ladder (different fingerprint) → incompatible, OTA suppressed.
        record_verified_peer(
            &mut rs,
            &ev_tx,
            "W1AW",
            "",
            &key,
            "hpx_hf",
            0xBBBB_BBBB_BBBB_BBBB,
        );
        assert_eq!(
            rs.last_verified_peer().unwrap().profile_compatible,
            Some(false)
        );
        assert!(
            rs.ota_suppressed_by_peer(),
            "diverged ladder must suppress OTA"
        );

        // Peer advertised no ladder (fp=0) → undetermined, NOT suppressed (OTA-without-handshake case).
        record_verified_peer(&mut rs, &ev_tx, "W1AW", "", &key, "", 0);
        assert_eq!(rs.last_verified_peer().unwrap().profile_compatible, None);
        assert!(!rs.ota_suppressed_by_peer());

        // We have no local OTA ladder → undetermined even if the peer advertises one.
        rs.local_ota_ladder = None;
        record_verified_peer(
            &mut rs,
            &ev_tx,
            "W1AW",
            "",
            &key,
            "hpx_hf",
            0xAAAA_AAAA_AAAA_AAAA,
        );
        assert_eq!(rs.last_verified_peer().unwrap().profile_compatible, None);
        assert!(!rs.ota_suppressed_by_peer());
    }

    #[tokio::test]
    async fn ota_set_hysteresis_dispatches_without_disturbing_session() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, _) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        let mut rs = RuntimeControlState::default();

        apply_command_to_engine(
            &ControlCommand::StartOtaSession {
                profile: "hpx500".into(),
            },
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut rs,
        )
        .await;
        let level_before = engine.ota_tx_level();

        // Tuning the anti-oscillation gates must not touch the level or the session.
        apply_command_to_engine(
            &ControlCommand::OtaSetHysteresis {
                min_backlog: Some(256),
                upgrade_hold_frames: Some(4),
            },
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut rs,
        )
        .await;
        assert!(engine.ota_active());
        assert_eq!(engine.ota_tx_level(), level_before);
    }

    #[test]
    fn apply_band_squelch_uses_per_band_override_else_default() {
        let mut engine = test_engine();
        let mut rs = RuntimeControlState {
            dcd_squelch_default: 0.01,
            ..RuntimeControlState::default()
        };
        rs.dcd_squelch_bands.insert("40m".into(), 0.05);

        // 40m is in the map → its override applies.
        apply_band_squelch(&mut engine, &rs, 7_040_000);
        assert!((engine.dcd_squelch() - 0.05).abs() < 1e-6);

        // 20m is not in the map → fall back to the default.
        apply_band_squelch(&mut engine, &rs, 14_070_000);
        assert!((engine.dcd_squelch() - 0.01).abs() < 1e-6);

        // Out-of-band frequency → default.
        apply_band_squelch(&mut engine, &rs, 5_000_000);
        assert!((engine.dcd_squelch() - 0.01).abs() < 1e-6);
    }

    #[test]
    fn apply_band_attenuation_uses_per_band_override_else_default() {
        let mut engine = test_engine();
        let mut rs = RuntimeControlState {
            tx_attenuation_default: -3.0,
            ..RuntimeControlState::default()
        };
        rs.tx_attenuation_bands.insert("40m".into(), -6.0);

        // 40m is in the map → its override applies.
        apply_band_attenuation(&mut engine, &rs, 7_040_000);
        assert!((engine.tx_attenuation_db() - (-6.0)).abs() < 1e-6);

        // 20m is not in the map → fall back to the global default.
        apply_band_attenuation(&mut engine, &rs, 14_070_000);
        assert!((engine.tx_attenuation_db() - (-3.0)).abs() < 1e-6);

        // Out-of-band frequency → default.
        apply_band_attenuation(&mut engine, &rs, 5_000_000);
        assert!((engine.tx_attenuation_db() - (-3.0)).abs() < 1e-6);
    }

    #[tokio::test]
    async fn set_tx_attenuation_per_band_stores_and_applies_on_the_matching_band() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, _) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        let mut rs = RuntimeControlState::default();

        // Per-band override while not on that band: stored, not applied (engine stays at 0).
        apply_command_to_engine(
            &ControlCommand::SetTxAttenuation {
                db: -6.0,
                band: Some("20m".into()),
            },
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut rs,
        )
        .await;
        assert_eq!(rs.tx_attenuation_bands.get("20m").copied(), Some(-6.0));
        assert!(
            (engine.tx_attenuation_db() - 0.0).abs() < 1e-6,
            "override not applied off-band"
        );

        // Now on 20m: a retune applies the stored override.
        rs.last_freq_hz = Some(14_070_000);
        apply_band_attenuation(&mut engine, &rs, 14_070_000);
        assert!((engine.tx_attenuation_db() - (-6.0)).abs() < 1e-6);

        // A global default set while on 20m: the per-band override still wins.
        apply_command_to_engine(
            &ControlCommand::SetTxAttenuation {
                db: -3.0,
                band: None,
            },
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut rs,
        )
        .await;
        assert!((rs.tx_attenuation_default - (-3.0)).abs() < 1e-6);
        assert!(
            (engine.tx_attenuation_db() - (-6.0)).abs() < 1e-6,
            "the 20m override wins over the global default while on 20m"
        );

        // Move to a band with no override → the global default applies.
        rs.last_freq_hz = Some(7_040_000);
        apply_band_attenuation(&mut engine, &rs, 7_040_000);
        assert!((engine.tx_attenuation_db() - (-3.0)).abs() < 1e-6);
    }

    #[tokio::test]
    async fn ota_set_aggressiveness_valid_and_invalid() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        let mut rs = RuntimeControlState::default();

        // A valid preset dispatches cleanly (no CommandError).
        apply_command_to_engine(
            &ControlCommand::OtaSetAggressiveness {
                preset: "aggressive".into(),
            },
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut rs,
        )
        .await;
        assert!(
            rx.try_recv().is_err(),
            "valid preset must not emit an event"
        );

        // An unknown preset emits a CommandError.
        apply_command_to_engine(
            &ControlCommand::OtaSetAggressiveness {
                preset: "turbo".into(),
            },
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut rs,
        )
        .await;
        match rx.try_recv() {
            Ok(ControlEvent::CommandError { command, reason }) => {
                assert_eq!(command, "ota_set_aggressiveness");
                assert!(reason.contains("turbo"));
            }
            other => panic!("expected CommandError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_set_config_updates_mode_and_tx_attenuation() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, _) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);

        let cmd = ControlCommand::SetConfig {
            config: protocol::DaemonConfig {
                callsign: "N0CALL".into(),
                grid_square: "AA00".into(),
                mode: "BPSK250".into(),
                tx_attenuation_db: -6.0,
                qsy_enabled: false,
                bandplan_mode: "unrestricted".into(),
                allow_tuner_on_high_swr: false,
            },
        };

        let mut runtime_state = RuntimeControlState::default();
        apply_command_to_engine(
            &cmd,
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;

        assert_eq!(*active_mode.lock().await, "BPSK250");
        assert!((engine.tx_attenuation_db() - (-6.0)).abs() < 1e-6);
    }

    #[tokio::test]
    async fn received_bytes_route_opfx_to_filexfer_and_handshake_stays_untouched() {
        use openpulse_core::sar::sar_encode;
        use openpulse_filexfer::{FxFrame, Reason};

        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, _) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        let mut rs = RuntimeControlState::default();

        // A handshake fragment (segment-id 0) must stay on the handshake path — filexfer untouched.
        let hs = sar_encode(0, b"HSCQ not a real conreq").unwrap();
        process_received_bytes(&hs[0], &mut rs, None, &ev_tx, &active_mode, &mut engine).await;
        assert_eq!(
            rs.filexfer_frames_routed, 0,
            "handshake must not reach the file seam"
        );

        // An OPFX control fragment (segment-id 0xFFFF) must route to the file-transfer seam.
        let frame = FxFrame::FileReject {
            transfer_id: 1,
            reason: Reason::Busy,
        }
        .encode();
        let ctrl = sar_encode(filexfer::FX_CONTROL_SEGMENT_ID, &frame).unwrap();
        process_received_bytes(&ctrl[0], &mut rs, None, &ev_tx, &active_mode, &mut engine).await;
        assert_eq!(
            rs.filexfer_frames_routed, 1,
            "OPFX control frame must reach the file seam"
        );

        // A block-data fragment (segment-id block_index+1) also routes to the file seam.
        let block = sar_encode(3, b"OPFX block-ish").unwrap();
        process_received_bytes(&block[0], &mut rs, None, &ev_tx, &active_mode, &mut engine).await;
        assert_eq!(
            rs.filexfer_frames_routed, 2,
            "OPFX block fragment must reach the file seam"
        );
    }

    /// Audit 2026-07-19 #11: an un-accepted transfer must not have peer bytes acknowledged on air.
    /// The state check used to run AFTER `persist_block` and the on-air `BlockAck`, so a peer that
    /// sent data while the operator was still deciding got both.
    #[tokio::test]
    async fn blocks_are_not_acked_while_awaiting_the_operators_decision() {
        use crate::filexfer::{FileTransferPolicy, FX_CONTROL_SEGMENT_ID};
        use ed25519_dalek::SigningKey;
        use openpulse_core::manifest::TransferManifest;
        use openpulse_core::sar::sar_encode;
        use openpulse_filexfer::{encode_block, split_blocks, FileOffer, FxFrame};

        let (tx, _rx) = broadcast::channel::<ControlEvent>(128);
        let ev_tx = Arc::new(tx);

        let mut seed = [0u8; 32];
        seed[0] = 42;
        let pubkey = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
        let file = b"awaiting decision payload. ".repeat(80).to_vec();
        let manifest = TransferManifest::sign(&file, "W1AW", &seed).unwrap();
        let transfer_id = 0x0A17_F00Du32;
        let block_size = 1024u32;
        let offer = FileOffer::from_manifest(
            transfer_id,
            &manifest,
            "wait.txt",
            "text/plain",
            block_size,
            &seed,
        )
        .unwrap();

        let dir = std::env::temp_dir().join(format!("opfx_await_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // auto_accept_max_bytes = 0 → the offer PROMPTS instead of auto-accepting, so the session
        // sits in AwaitingDecision, which is the state under test.
        let policy = FileTransferPolicy::from_config(&openpulse_config::FileTransferConfig {
            enabled: true,
            download_dir: dir.to_string_lossy().into_owned(),
            auto_accept_max_bytes: 0,
            max_file_bytes: 10 * 1024 * 1024,
            per_peer_quota_bytes: 0,
            require_verified_peer: true,
            allowed_peers: vec![],
            offer_timeout_secs: 120,
            partial_ttl_hours: 72,
            burst_max_secs: 20.0,
        });
        let vp = VerifiedPeer {
            callsign: "W1AW".into(),
            grid: String::new(),
            pubkey: pubkey.to_vec(),
            profile_compatible: None,
        };
        let mut rs = RuntimeControlState {
            local_callsign: "W1AW".into(),
            verified_peers: std::iter::once(("W1AW".to_string(), vp)).collect(),
            filexfer_policy: policy,
            ..RuntimeControlState::default()
        };

        let frame = FxFrame::FileOffer(offer.clone()).encode();
        for frag in sar_encode(FX_CONTROL_SEGMENT_ID, &frame).expect("sar") {
            crate::filexfer::route_inbound_fragment(
                &frag,
                FX_CONTROL_SEGMENT_ID,
                &mut rs,
                &ev_tx,
                "BPSK250",
            );
        }
        assert!(
            rs.file_rx.is_some(),
            "precondition: the offer must create a session awaiting the operator's decision"
        );
        rs.filexfer_tx_queue.clear(); // ignore anything the offer handling itself queued

        // The peer sends data anyway, before any accept. `encode_block` already returns SAR
        // fragments, and its segment id is the transfer's data segment.
        let seg = (transfer_id as u16) | 0x8000;
        for (k, block) in split_blocks(&file, block_size).iter().enumerate() {
            for frag in encode_block(transfer_id, k as u16, block, None).unwrap() {
                crate::filexfer::route_inbound_fragment(&frag, seg, &mut rs, &ev_tx, "BPSK250");
            }
        }

        assert!(
            rs.filexfer_tx_queue.is_empty(),
            "an un-accepted transfer must not put a BlockAck on the air — the operator has not \
             agreed to receive this file yet"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn inbound_offer_and_blocks_write_verified_file() {
        use crate::filexfer::{FileTransferPolicy, FX_CONTROL_SEGMENT_ID};
        use ed25519_dalek::SigningKey;
        use openpulse_core::manifest::TransferManifest;
        use openpulse_core::sar::sar_encode;
        use openpulse_filexfer::{encode_block, split_blocks, FileOffer, FxFrame};

        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(128);
        let ev_tx = Arc::new(tx);

        // A signed offer from a known sender.
        let mut seed = [0u8; 32];
        seed[0] = 42;
        let pubkey = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
        let file = b"file transfer receive test payload. ".repeat(80).to_vec();
        let manifest = TransferManifest::sign(&file, "W1AW", &seed).unwrap();
        let transfer_id = 0x0BAD_F00D;
        let block_size = 1024u32;
        let offer = FileOffer::from_manifest(
            transfer_id,
            &manifest,
            "recv.txt",
            "text/plain",
            block_size,
            &seed,
        )
        .unwrap();
        assert!(offer.block_count >= 2, "want a multi-block file");

        let dir = std::env::temp_dir().join(format!("opfx_recv_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let policy = FileTransferPolicy::from_config(&openpulse_config::FileTransferConfig {
            enabled: true,
            download_dir: dir.to_string_lossy().into_owned(),
            auto_accept_max_bytes: u64::MAX,
            max_file_bytes: 10 * 1024 * 1024,
            per_peer_quota_bytes: 0,
            require_verified_peer: true,
            allowed_peers: vec![],
            offer_timeout_secs: 120,
            partial_ttl_hours: 72,
            burst_max_secs: 20.0,
        });
        let vp = VerifiedPeer {
            callsign: "W1AW".into(),
            grid: String::new(),
            pubkey: pubkey.to_vec(),
            profile_compatible: None,
        };
        let mut rs = RuntimeControlState {
            // §97.119: this station answers an inbound offer on the air, so it needs a valid
            // MYID. Without one the seam gate refuses to engage (audit 2026-07-19, #6).
            local_callsign: "W1AW".into(),
            verified_peers: std::iter::once(("W1AW".to_string(), vp.clone())).collect(),
            filexfer_policy: policy,
            ..RuntimeControlState::default()
        };

        // Offer → auto-accepted → receive session started.
        let offer_frag =
            sar_encode(FX_CONTROL_SEGMENT_ID, &FxFrame::FileOffer(offer).encode()).unwrap();
        process_received_bytes(
            &offer_frag[0],
            &mut rs,
            None,
            &ev_tx,
            &active_mode,
            &mut engine,
        )
        .await;
        assert!(
            rs.file_rx.is_some(),
            "offer should auto-accept and start a session"
        );

        // Deliver every block's fragments.
        for (k, block) in split_blocks(&file, block_size).iter().enumerate() {
            for frag in encode_block(transfer_id, k as u16, block, None).unwrap() {
                process_received_bytes(&frag, &mut rs, None, &ev_tx, &active_mode, &mut engine)
                    .await;
            }
        }

        // The file must have been verified and written; find the FileReceived event + read it back.
        let mut path = None;
        while let Ok(ev) = rx.try_recv() {
            if let ControlEvent::FileReceived {
                verified, path: p, ..
            } = ev
            {
                assert!(verified, "a signed file must verify");
                path = Some(p);
            }
        }
        let path = path.expect("FileReceived emitted");
        assert_eq!(std::fs::read(&path).expect("file on disk"), file);
        assert!(rs.file_rx.is_none(), "session cleared after completion");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit E5: a signed offer from W1AW is verified against W1AW's key even though a *different*
    /// peer (K2XYZ) handshook more recently and holds the single `verified_peer` slot. Before the
    /// fix, `on_offer` verified against the slot's key, so W1AW's legitimate offer would fail.
    #[tokio::test]
    async fn offer_is_verified_against_its_senders_key_not_the_last_handshook_peer() {
        use crate::filexfer::{FileTransferPolicy, FX_CONTROL_SEGMENT_ID};
        use ed25519_dalek::SigningKey;
        use openpulse_core::manifest::TransferManifest;
        use openpulse_core::sar::sar_encode;
        use openpulse_filexfer::{FileOffer, FxFrame};

        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(128);
        let ev_tx = Arc::new(tx);

        // W1AW signs an offer with its own key.
        let mut w1aw_seed = [0u8; 32];
        w1aw_seed[0] = 11;
        let w1aw_key = SigningKey::from_bytes(&w1aw_seed)
            .verifying_key()
            .to_bytes();
        let file = b"e5 sender-binding payload ".repeat(60).to_vec();
        let manifest = TransferManifest::sign(&file, "W1AW", &w1aw_seed).unwrap();
        let offer =
            FileOffer::from_manifest(0x0E5, &manifest, "e5.txt", "text/plain", 1024, &w1aw_seed)
                .unwrap();

        let dir = std::env::temp_dir().join(format!("opfx_e5_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let policy = FileTransferPolicy::from_config(&openpulse_config::FileTransferConfig {
            enabled: true,
            download_dir: dir.to_string_lossy().into_owned(),
            auto_accept_max_bytes: u64::MAX,
            max_file_bytes: 10 * 1024 * 1024,
            per_peer_quota_bytes: 0,
            require_verified_peer: true,
            allowed_peers: vec![],
            offer_timeout_secs: 120,
            partial_ttl_hours: 72,
            burst_max_secs: 20.0,
        });

        // The map holds both W1AW (correct) and K2XYZ; the single slot points at K2XYZ (a different
        // key) as the most-recently-handshook peer.
        let w1aw = VerifiedPeer {
            callsign: "W1AW".into(),
            grid: String::new(),
            pubkey: w1aw_key.to_vec(),
            profile_compatible: None,
        };
        let k2xyz = VerifiedPeer {
            callsign: "K2XYZ".into(),
            grid: String::new(),
            pubkey: vec![9u8; 32], // a different key
            profile_compatible: None,
        };
        let mut verified_peers = std::collections::HashMap::new();
        verified_peers.insert("W1AW".to_string(), w1aw);
        verified_peers.insert("K2XYZ".to_string(), k2xyz.clone());
        let mut rs = RuntimeControlState {
            // §97.119: this station answers an inbound offer on the air, so it needs a valid
            // MYID. Without one the seam gate refuses to engage (audit 2026-07-19, #6).
            local_callsign: "W1AW".into(),
            verified_peers,
            filexfer_policy: policy,
            ..RuntimeControlState::default()
        };

        let offer_frag =
            sar_encode(FX_CONTROL_SEGMENT_ID, &FxFrame::FileOffer(offer).encode()).unwrap();
        process_received_bytes(
            &offer_frag[0],
            &mut rs,
            None,
            &ev_tx,
            &active_mode,
            &mut engine,
        )
        .await;

        let mut saw_valid_from_w1aw = false;
        while let Ok(ev) = rx.try_recv() {
            if let ControlEvent::FileOffered {
                from,
                signature_valid,
                ..
            } = ev
            {
                assert_eq!(
                    from, "W1AW",
                    "the offer must be attributed to its true sender"
                );
                assert!(
                    signature_valid,
                    "the offer must verify against W1AW's key, not the K2XYZ slot"
                );
                saw_valid_from_w1aw = true;
            }
        }
        assert!(saw_valid_from_w1aw, "a FileOffered event should be emitted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn send_file_offers_then_completes_on_receiver_frames() {
        use crate::filexfer::{FileTransferPolicy, FX_CONTROL_SEGMENT_ID};
        use openpulse_core::sar::sar_encode;
        use openpulse_filexfer::{CompleteStatus, FxFrame};

        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(128);
        let ev_tx = Arc::new(tx);

        let dir = std::env::temp_dir().join(format!("opfx_send_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("outbound.txt");
        let contents = b"send side file transfer test ".repeat(4);
        std::fs::write(&file_path, &contents).unwrap();

        let policy = FileTransferPolicy::from_config(&openpulse_config::FileTransferConfig {
            enabled: true,
            download_dir: dir.to_string_lossy().into_owned(),
            auto_accept_max_bytes: 0,
            max_file_bytes: 1 << 20,
            per_peer_quota_bytes: 0,
            require_verified_peer: false,
            allowed_peers: vec![],
            offer_timeout_secs: 120,
            partial_ttl_hours: 72,
            burst_max_secs: 20.0,
        });
        let mut rs = RuntimeControlState {
            local_callsign: "W1AW".into(),
            filexfer_policy: policy,
            ..RuntimeControlState::default()
        };

        // SendFile → transmit the offer + open the send session.
        let cmd = ControlCommand::SendFile {
            to: "W1AW".into(),
            path: file_path.to_string_lossy().into_owned(),
        };
        apply_command_to_engine(&cmd, &mut engine, &active_mode, &ev_tx, None, &mut rs).await;
        let fx = rs.file_tx.as_ref().expect("send session started");
        let transfer_id = fx.transfer_id();
        assert_eq!(fx.block_count(), 1);

        let feed = |frame: Vec<u8>| sar_encode(FX_CONTROL_SEGMENT_ID, &frame).unwrap()[0].clone();
        // Receiver accepts → send block 0.
        process_received_bytes(
            &feed(
                FxFrame::FileAccept {
                    transfer_id,
                    have_bitmap: vec![],
                }
                .encode(),
            ),
            &mut rs,
            None,
            &ev_tx,
            &active_mode,
            &mut engine,
        )
        .await;
        // Receiver acks block 0 → awaiting verify.
        process_received_bytes(
            &feed(
                FxFrame::BlockAck {
                    transfer_id,
                    block_index: 0,
                    complete: true,
                    missing_frag_bitmap: vec![],
                }
                .encode(),
            ),
            &mut rs,
            None,
            &ev_tx,
            &active_mode,
            &mut engine,
        )
        .await;
        // Receiver confirms verified → FileSent.
        process_received_bytes(
            &feed(
                FxFrame::FileComplete {
                    transfer_id,
                    status: CompleteStatus::VerifiedOk,
                    countersignature: [0u8; 64],
                }
                .encode(),
            ),
            &mut rs,
            None,
            &ev_tx,
            &active_mode,
            &mut engine,
        )
        .await;

        let mut sent = false;
        while let Ok(ev) = rx.try_recv() {
            if let ControlEvent::FileSent {
                receipt_valid, to, ..
            } = ev
            {
                assert_eq!(receipt_valid, Some(true));
                assert_eq!(to, "W1AW");
                sent = true;
            }
        }
        assert!(sent, "FileSent emitted");
        assert!(
            rs.file_tx.is_none(),
            "send session cleared after completion"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Build a runtime state with an active outbound send session (an offer sent, awaiting a reply).
    #[cfg(test)]
    async fn rs_with_active_send(tag: &str) -> (RuntimeControlState, u32, std::path::PathBuf) {
        use crate::filexfer::FileTransferPolicy;
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, _rx) = broadcast::channel::<ControlEvent>(64);
        let ev_tx = Arc::new(tx);
        let dir = std::env::temp_dir().join(format!("opfx_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("outbound.txt");
        std::fs::write(&file_path, b"send lifecycle test payload ".repeat(4)).unwrap();
        let policy = FileTransferPolicy::from_config(&openpulse_config::FileTransferConfig {
            enabled: true,
            download_dir: dir.to_string_lossy().into_owned(),
            auto_accept_max_bytes: 0,
            max_file_bytes: 1 << 20,
            per_peer_quota_bytes: 0,
            require_verified_peer: false,
            allowed_peers: vec![],
            offer_timeout_secs: 120,
            partial_ttl_hours: 72,
            burst_max_secs: 20.0,
        });
        let mut rs = RuntimeControlState {
            local_callsign: "N0CALL".into(),
            filexfer_policy: policy,
            ..RuntimeControlState::default()
        };
        let cmd = ControlCommand::SendFile {
            to: "W1AW".into(),
            path: file_path.to_string_lossy().into_owned(),
        };
        apply_command_to_engine(&cmd, &mut engine, &active_mode, &ev_tx, None, &mut rs).await;
        let transfer_id = rs
            .file_tx
            .as_ref()
            .expect("send session started")
            .transfer_id();
        (rs, transfer_id, dir)
    }

    /// Audit F-5: a send whose peer never answers times out and clears `file_tx`, so the subsystem
    /// isn't pinned forever (which would make every later `SendFile` fail "already active").
    #[tokio::test]
    async fn a_stuck_send_clears_on_offer_timeout() {
        let (mut rs, transfer_id, dir) = rs_with_active_send("timeout").await;
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        assert!(rs.file_tx.is_some(), "precondition: a send is active");

        // Poll with a clock far past the offer deadline → the session fails and is cleared.
        crate::filexfer::poll_timeouts(&mut rs, &ev_tx, "BPSK250", u64::MAX);

        assert!(
            rs.file_tx.is_none(),
            "a timed-out send must release file_tx"
        );
        let mut failed = false;
        while let Ok(ev) = rx.try_recv() {
            if let ControlEvent::FileFailed {
                transfer_id: t,
                direction,
                ..
            } = ev
            {
                if t == transfer_id && direction == "tx" {
                    failed = true;
                }
            }
        }
        assert!(failed, "a FileFailed(tx) event must be emitted on timeout");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit F-5: `CancelFile` on an outbound transfer clears `file_tx` (previously it only ever
    /// touched `file_rx`, leaving a send with no manual recovery path).
    #[tokio::test]
    async fn cancel_clears_an_outbound_send() {
        let (mut rs, transfer_id, dir) = rs_with_active_send("cancel").await;
        let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        assert!(rs.file_tx.is_some(), "precondition: a send is active");

        crate::filexfer::cancel_transfer(transfer_id, &mut rs, &ev_tx, "BPSK250");

        assert!(
            rs.file_tx.is_none(),
            "CancelFile must clear an outbound send session"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn apply_send_message_transmits_payload_over_active_mode() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, _) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);

        let cmd = ControlCommand::SendMessage {
            to: "W1AW".into(),
            subject: "status".into(),
            body: "rf body payload".into(),
        };

        let mut runtime_state = RuntimeControlState::default();
        apply_command_to_engine(
            &cmd,
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;

        let rx = engine.receive("BPSK250", None).unwrap();
        assert_eq!(rx, b"rf body payload");
    }

    #[tokio::test]
    async fn apply_send_message_compresses_the_wire_when_enabled() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, _) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);

        let body = "status ok ".repeat(20); // compressible
        let cmd = ControlCommand::SendMessage {
            to: "W1AW".into(),
            subject: "status".into(),
            body: body.clone(),
        };

        let mut runtime_state = RuntimeControlState::default();
        runtime_state.compress_tx = true;
        apply_command_to_engine(
            &cmd,
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;

        // The wire carries a packed (smaller) frame; unpack recovers the original body.
        let rx = engine.receive("BPSK250", None).unwrap();
        assert!(
            rx.len() < body.len(),
            "packed wire frame {} should be smaller than body {}",
            rx.len(),
            body.len()
        );
        assert_eq!(
            openpulse_core::compression::unpack(&rx).unwrap(),
            body.as_bytes()
        );
    }

    #[tokio::test]
    async fn apply_send_message_invalid_mode_emits_command_error() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("NO_SUCH_MODE".to_string()));
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);

        let cmd = ControlCommand::SendMessage {
            to: "W1AW".into(),
            subject: "status".into(),
            body: "rf body payload".into(),
        };

        let mut runtime_state = RuntimeControlState::default();
        apply_command_to_engine(
            &cmd,
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;

        let mut saw_error = false;
        while let Ok(ev) = rx.try_recv() {
            if let ControlEvent::CommandError { command, reason } = ev {
                assert_eq!(command, "send_message");
                assert!(reason.contains("NO_SUCH_MODE"));
                saw_error = true;
                break;
            }
        }
        assert!(saw_error, "expected command_error event");
    }

    #[tokio::test]
    async fn apply_unimplemented_runtime_commands_emit_command_error() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);

        let cases = vec![
            (
                ControlCommand::SetFreq {
                    rig: "rigctld".into(),
                    freq_hz: 7_100_000,
                },
                "set_freq",
                "no rigctld controller configured",
            ),
            (
                ControlCommand::AcceptQsy {
                    token: "tok-1".into(),
                },
                "accept_qsy",
                "unknown pending token",
            ),
            (
                ControlCommand::RejectQsy {
                    token: "tok-1".into(),
                },
                "reject_qsy",
                "unknown pending token",
            ),
        ];

        let mut runtime_state = RuntimeControlState::default();
        for (cmd, expected_command, expected_reason_substr) in cases {
            apply_command_to_engine(
                &cmd,
                &mut engine,
                &active_mode,
                &ev_tx,
                None,
                &mut runtime_state,
            )
            .await;
            let ev = rx.recv().await.expect("expected command_error event");
            match ev {
                ControlEvent::CommandError { command, reason } => {
                    assert_eq!(command, expected_command);
                    assert!(reason.contains(expected_reason_substr));
                }
                other => panic!("expected command_error event, got {other:?}"),
            }
        }
    }

    /// Build a real, runnable repeater so `EnableRepeater` has something to start.
    fn test_repeater(enabled: bool) -> openpulse_repeater::CrossBandRepeater {
        let mk = || {
            let mut e = ModemEngine::new(Box::new(openpulse_audio::LoopbackBackend::new()));
            let _ = e.register_plugin(Box::new(bpsk_plugin::BpskPlugin::new()));
            e
        };
        openpulse_repeater::CrossBandRepeater::new(
            Box::new(openpulse_radio::NoOpPtt::new()),
            mk(),
            mk(),
            openpulse_repeater::RepeaterConfig {
                enabled,
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn apply_repeater_enable_disable_emits_state_changes() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        let mut runtime_state = RuntimeControlState::default();
        // #1298: this test used to run with `repeater: None` and assert that enabling SUCCEEDED —
        // it was pinning the defect. A daemon with nothing to run must not report a running repeater.
        runtime_state.repeater = Some(test_repeater(true));

        apply_command_to_engine(
            &ControlCommand::EnableRepeater,
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;
        assert!(runtime_state.repeater_enabled);
        match rx.recv().await.expect("expected repeater event") {
            ControlEvent::RepeaterChanged { enabled } => assert!(enabled),
            other => panic!("expected RepeaterChanged, got {other:?}"),
        }

        apply_command_to_engine(
            &ControlCommand::DisableRepeater,
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;
        assert!(!runtime_state.repeater_enabled);
        match rx.recv().await.expect("expected repeater event") {
            ControlEvent::RepeaterChanged { enabled } => assert!(!enabled),
            other => panic!("expected RepeaterChanged, got {other:?}"),
        }
    }

    /// THE #1298 GATE (enable half): with nothing to run, enabling must FAIL and say so.
    ///
    /// The old arm logged a `warn!`, then set `repeater_enabled = true` and emitted
    /// `RepeaterChanged { enabled: true }` regardless — so a daemon with no repeater reported one as
    /// running, and no command sequence could get back to a truthful state.
    #[tokio::test]
    async fn enabling_a_repeater_that_does_not_exist_fails_instead_of_claiming_success() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        let mut runtime_state = RuntimeControlState::default();
        assert!(runtime_state.repeater.is_none(), "premise: nothing to run");

        apply_command_to_engine(
            &ControlCommand::EnableRepeater,
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;

        assert!(
            !runtime_state.repeater_enabled,
            "the daemon reports a repeater as enabled while none exists"
        );
        match rx.recv().await.expect("expected an event") {
            ControlEvent::CommandError { command, reason } => {
                assert_eq!(command, "enable_repeater");
                assert!(
                    reason.contains("no repeater is available"),
                    "unhelpful reason: {reason}"
                );
            }
            other => {
                panic!("expected CommandError, got {other:?} — a RepeaterChanged here is the bug")
            }
        }
    }

    /// THE #1298 GATE (reap half): a thread that has exited must not leave the repeater "enabled".
    ///
    /// The thread OWNS the `CrossBandRepeater`, so its exit means the repeater is gone. Nothing
    /// observed that: `EnableRepeater` answered "already enabled" forever after.
    #[tokio::test]
    async fn a_repeater_thread_that_exited_is_reaped_rather_than_reported_enabled() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(64);
        let ev_tx = Arc::new(tx);
        let mut runtime_state = RuntimeControlState::default();
        // A DISABLED repeater's `run_full_duplex` returns Ok(0) at once, so the thread exits
        // immediately — standing in for any exit the daemon did not ask for.
        runtime_state.repeater = Some(test_repeater(false));

        apply_command_to_engine(
            &ControlCommand::EnableRepeater,
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;
        assert!(runtime_state.repeater_enabled, "enable succeeded");

        // Let the thread finish.
        for _ in 0..200 {
            if runtime_state
                .repeater_thread
                .as_ref()
                .is_some_and(|t| t.is_finished())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // The next command must see the truth rather than "already enabled".
        apply_command_to_engine(
            &ControlCommand::EnableRepeater,
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;

        let mut saw_disabled_edge = false;
        let mut saw_already_enabled = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                ControlEvent::RepeaterChanged { enabled: false } => saw_disabled_edge = true,
                ControlEvent::CommandError { ref reason, .. }
                    if reason.contains("already enabled") =>
                {
                    saw_already_enabled = true
                }
                _ => {}
            }
        }
        assert!(
            saw_disabled_edge,
            "the dead thread was never reported: clients still believe a repeater is running"
        );
        assert!(
            !saw_already_enabled,
            "the daemon answered 'repeater already enabled' about a thread that had exited — this \
             is the state no command sequence could escape"
        );
    }

    #[tokio::test]
    async fn apply_qsy_accept_reject_record_and_emit_decisions() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        let mut runtime_state = RuntimeControlState::default();
        runtime_state.qsy_pending_token = Some("tok-accept".to_string());
        runtime_state.qsy_candidate_freqs = vec![14_070_000, 14_077_000];

        apply_command_to_engine(
            &ControlCommand::AcceptQsy {
                token: "tok-accept".into(),
            },
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;
        assert_eq!(runtime_state.qsy_decisions.get("tok-accept"), Some(&true));
        assert!(runtime_state.qsy_pending_token.is_none());
        match rx.recv().await.expect("expected qsy event") {
            ControlEvent::QsyDecision { token, accepted } => {
                assert_eq!(token, "tok-accept");
                assert!(accepted);
            }
            other => panic!("expected QsyDecision, got {other:?}"),
        }

        runtime_state.qsy_pending_token = Some("tok-reject".to_string());
        apply_command_to_engine(
            &ControlCommand::RejectQsy {
                token: "tok-reject".into(),
            },
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;
        assert_eq!(runtime_state.qsy_decisions.get("tok-reject"), Some(&false));
        assert!(runtime_state.qsy_pending_token.is_none());
        match next_non_ptt(&mut rx).await {
            ControlEvent::QsyDecision { token, accepted } => {
                assert_eq!(token, "tok-reject");
                assert!(!accepted);
            }
            other => panic!("expected QsyDecision, got {other:?}"),
        }
    }

    /// Next event that is not a PTT edge.
    ///
    /// Since #1262 every keyed emission reports `PttChanged`, including the QSY and handshake paths
    /// that previously transmitted without keying at all. Tests asserting on the NEXT event have to
    /// step over those edges — they are the fix working, not noise.
    async fn next_non_ptt(rx: &mut broadcast::Receiver<ControlEvent>) -> ControlEvent {
        loop {
            match rx.recv().await.expect("expected an event") {
                ControlEvent::PttChanged { .. } => continue,
                other => return other,
            }
        }
    }

    #[tokio::test]
    async fn connect_then_disconnect_writes_an_adif_logbook_record() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("QPSK500".to_string()));
        let (tx, _rx) = broadcast::channel::<ControlEvent>(32);
        let ev_tx = Arc::new(tx);

        let path = std::env::temp_dir().join(format!("opadif-it-{}.adi", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut runtime_state = RuntimeControlState {
            // A real callsign: production cannot reach the default's empty one (`main.rs` refuses
            // to start without one), and since #1199 an unbuildable CONREQ opens nothing — so an
            // empty local callsign now correctly writes no logbook record at all.
            local_callsign: "DL0XYZ".into(),
            logbook: crate::logbook::Logbook::new(
                true,
                path.to_str().unwrap(),
                "DL0XYZ",
                "AA00aa",
                &std::collections::BTreeMap::from([("dl1abc".to_string(), "JO31aa".to_string())]),
            ),
            last_freq_hz: Some(14_070_000),
            ..RuntimeControlState::default()
        };

        for cmd in [
            ControlCommand::ConnectPeer {
                callsign: "DL1ABC".to_string(),
            },
            ControlCommand::DisconnectPeer,
        ] {
            apply_command_to_engine(
                &cmd,
                &mut engine,
                &active_mode,
                &ev_tx,
                None,
                &mut runtime_state,
            )
            .await;
        }

        let body = std::fs::read_to_string(&path).expect("logbook file written");
        assert!(body.contains("<CALL:6>DL1ABC"));
        assert!(body.contains("<BAND:3>20m"));
        assert!(body.contains("<SUBMODE:7>QPSK500"));
        assert!(body.contains("<STATION_CALLSIGN:6>DL0XYZ"));
        assert!(body.contains("<GRIDSQUARE:6>JO31aa")); // worked station's grid from peer_grids
        assert_eq!(body.matches("<EOR>").count(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn apply_connect_disconnect_drive_secure_session_and_pending_qsy() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(32);
        let ev_tx = Arc::new(tx);
        let mut runtime_state = RuntimeControlState {
            // A real callsign, because production cannot reach the default's empty one: `main.rs`
            // refuses to start without one, and since #1199 an unbuildable CONREQ opens nothing at
            // all. The empty default used to connect-and-not-handshake, which is the defect.
            local_callsign: "K2XYZ".into(),
            ..RuntimeControlState::default()
        };

        apply_command_to_engine(
            &ControlCommand::ConnectPeer {
                callsign: "W1AW".to_string(),
            },
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;

        assert!(engine.hpx_session_id().is_some());
        let token = runtime_state
            .qsy_pending_token
            .clone()
            .expect("connect should create pending qsy token");

        let first = rx.recv().await.expect("expected event");
        let second = rx.recv().await.expect("expected event");
        let saw_connected = matches!(
            (&first, &second),
            (
                ControlEvent::RfConnectionChanged {
                    connected: true,
                    peer: Some(_)
                },
                ControlEvent::QsyPending { .. }
            ) | (
                ControlEvent::QsyPending { .. },
                ControlEvent::RfConnectionChanged {
                    connected: true,
                    peer: Some(_)
                }
            )
        );
        assert!(
            saw_connected,
            "expected rf connected and qsy pending events"
        );

        apply_command_to_engine(
            &ControlCommand::AcceptQsy { token },
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;

        apply_command_to_engine(
            &ControlCommand::DisconnectPeer,
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;

        assert!(runtime_state.qsy_pending_token.is_none());
        assert!(engine.hpx_session_id().is_none());
    }

    #[tokio::test]
    async fn get_ptt_state_rebroadcasts_the_current_keyed_state() {
        // A client that missed a PttChanged edge can resync: GetPttState re-emits the current state,
        // which is keyed exactly when the watchdog deadline is armed.
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        let mut runtime_state = RuntimeControlState::default();

        // Currently keyed → GetPttState reports active: true.
        runtime_state.ptt.arm();
        apply_command_to_engine(
            &ControlCommand::GetPttState,
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;
        assert!(
            matches!(rx.try_recv(), Ok(ControlEvent::PttChanged { active: true })),
            "GetPttState must re-broadcast active: true while keyed"
        );

        // Not keyed → active: false.
        runtime_state.ptt.disarm();
        apply_command_to_engine(
            &ControlCommand::GetPttState,
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;
        assert!(
            matches!(
                rx.try_recv(),
                Ok(ControlEvent::PttChanged { active: false })
            ),
            "GetPttState must re-broadcast active: false while unkeyed"
        );
    }

    #[tokio::test]
    async fn accept_qsy_with_candidates_initiates_session_and_transmits_req() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        let mut runtime_state = RuntimeControlState::default();
        runtime_state.qsy_pending_token = Some("tok-qsy".to_string());
        runtime_state.qsy_candidate_freqs = vec![14_070_000, 14_077_000];

        apply_command_to_engine(
            &ControlCommand::AcceptQsy {
                token: "tok-qsy".into(),
            },
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;

        assert_eq!(runtime_state.qsy_decisions.get("tok-qsy"), Some(&true));
        assert!(runtime_state.qsy_pending_token.is_none());
        assert!(
            runtime_state.qsy_session.is_some(),
            "QsySession should be stored in runtime_state"
        );

        // QsyDecision event must be first
        match rx.recv().await.expect("expected QsyDecision event") {
            ControlEvent::QsyDecision { token, accepted } => {
                assert_eq!(token, "tok-qsy");
                assert!(accepted);
            }
            other => panic!("expected QsyDecision, got {other:?}"),
        }

        // QSY_REQ and QSY_LIST frames were transmitted; verify loopback receive contains QSY text
        let bytes = engine.receive("BPSK250", None).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("QSY_REQ") || text.contains("QSY_LIST"),
            "expected QSY frame in modem output, got: {text:?}"
        );
    }

    #[tokio::test]
    async fn accept_qsy_without_candidates_emits_command_error() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        let mut runtime_state = RuntimeControlState::default();
        runtime_state.qsy_pending_token = Some("tok-nocand".to_string());
        // qsy_candidate_freqs is empty (default)

        apply_command_to_engine(
            &ControlCommand::AcceptQsy {
                token: "tok-nocand".into(),
            },
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;

        // QsyDecision event first, then CommandError for no candidates
        let ev1 = rx.recv().await.expect("expected first event");
        assert!(
            matches!(ev1, ControlEvent::QsyDecision { .. }),
            "expected QsyDecision"
        );
        let ev2 = rx.recv().await.expect("expected CommandError event");
        match ev2 {
            ControlEvent::CommandError { command, reason } => {
                assert_eq!(command, "accept_qsy");
                assert!(reason.contains("candidate"), "reason: {reason}");
            }
            other => panic!("expected CommandError, got {other:?}"),
        }
    }

    /// Drive a persistent in-band CW tone through the streaming capture path until the notch
    /// persistence tracker confirms it, leaving `in_band_interferers()` populated.
    #[cfg(not(target_arch = "wasm32"))]
    fn engine_with_confirmed_in_band_interferer(min_hits: u32) -> ModemEngine {
        let mut engine = test_engine();
        engine.enable_notch();
        engine.set_notch_persistence(min_hits);
        // 1500 Hz = engine centre; BPSK250 occupied 500 Hz → protected band ~1250–1750.
        let tone: Vec<f32> = (0..8192)
            .map(|i| 0.2 * (2.0 * std::f32::consts::PI * 1500.0 * i as f32 / 8000.0).sin())
            .collect();
        for _ in 0..(min_hits + 1) {
            let _ = engine.accumulate_capture(Some("BPSK250"), tone.clone());
        }
        // This subsumes the `notch_blocks_processed() > 0` tripwire that used to follow it (#1271):
        // the counter increments on the line immediately before `apply_rx_notch` is called
        // (engine.rs), and `notch_in_band_interferers` is populated only INSIDE that function — so a
        // non-empty list strictly implies the counter moved, and the second assertion could not
        // fail when this one passed.
        assert!(
            !engine.in_band_interferers().is_empty(),
            "persistence should confirm the in-band tone — which also proves the notch ran on the \
             daemon's streaming (accumulate_capture) path"
        );
        engine
    }

    #[tokio::test]
    async fn auto_qsy_on_interference_initiates_session_and_transmits_req() {
        let mut engine = engine_with_confirmed_in_band_interferer(3);
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        let mut runtime_state = RuntimeControlState::default();
        runtime_state.local_callsign = "W1AW".into(); // valid MYID so auto-QSY may key up (audit F6)
        runtime_state.qsy_candidate_freqs = vec![14_070_000, 14_077_000];

        maybe_qsy_on_interference(
            true,
            &mut runtime_state,
            None,
            &ev_tx,
            &active_mode,
            &mut engine,
        )
        .await;

        assert!(
            runtime_state.qsy_session.is_some(),
            "a confirmed in-band interferer should auto-initiate a QSY session"
        );
        assert!(
            engine.in_band_interferers().is_empty(),
            "the hint should be cleared so it does not re-trigger every tick"
        );
        let bytes = engine.receive("BPSK250", None).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("QSY_REQ") || text.contains("QSY_LIST"),
            "expected a QSY frame in the modem output, got: {text:?}"
        );
    }

    #[tokio::test]
    async fn auto_qsy_end_to_end_initiator_to_responder_over_rf() {
        use bpsk_plugin::BpskPlugin;
        use openpulse_modem::channel_sim::ChannelSimHarness;

        // Station A (tx_engine) detects the interferer and auto-initiates QSY; Station B (rx_engine)
        // receives the QSY_REQ over the (clean) channel and opens a responder session — the full
        // notch → in-band-interferer → auto-QSY → RF handoff loop, deterministically.
        let mut h = ChannelSimHarness::new();
        h.tx_engine
            .register_plugin(Box::new(BpskPlugin::new()))
            .unwrap();
        h.rx_engine
            .register_plugin(Box::new(BpskPlugin::new()))
            .unwrap();

        // A: confirm a persistent in-band tone via the streaming capture path.
        h.tx_engine.enable_notch();
        h.tx_engine.set_notch_persistence(3);
        let tone: Vec<f32> = (0..8192)
            .map(|i| 0.2 * (2.0 * std::f32::consts::PI * 1500.0 * i as f32 / 8000.0).sin())
            .collect();
        for _ in 0..4 {
            let _ = h
                .tx_engine
                .accumulate_capture(Some("BPSK250"), tone.clone());
        }
        assert!(!h.tx_engine.in_band_interferers().is_empty());

        // A: auto-QSY → transmits QSY_REQ into the TX loopback.
        let mode_a: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx_a, _rx_a) = broadcast::channel::<ControlEvent>(16);
        let ev_a = Arc::new(tx_a);
        let mut rs_a = RuntimeControlState {
            local_callsign: "W1AW".into(), // valid MYID so auto-QSY may key up (audit F6)
            qsy_candidate_freqs: vec![14_070_000, 14_077_000],
            // A signs its QSY lines with this seed (#1252); B is bound to the matching key below.
            station_seed: INITIATOR_SEED,
            ..RuntimeControlState::default()
        };
        maybe_qsy_on_interference(true, &mut rs_a, None, &ev_a, &mode_a, &mut h.tx_engine).await;
        assert!(rs_a.qsy_session.is_some(), "A should have initiated QSY");

        // Carry A's transmitted QSY_REQ across the (clean) channel to B and decode it.
        h.route_clean();
        let bytes = h.rx_engine.receive("BPSK250", None).unwrap_or_default();
        assert!(
            String::from_utf8_lossy(&bytes).contains("QSY_REQ"),
            "B should receive A's QSY_REQ, got {:?}",
            String::from_utf8_lossy(&bytes)
        );

        // B: the decoded QSY_REQ drives a responder session.
        let mode_b: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx_b, mut rx_b) = broadcast::channel::<ControlEvent>(16);
        let ev_b = Arc::new(tx_b);
        // B has verified A through the handshake, so it holds A's key — which is the only key that
        // can open a responder session on B since #1252.
        let mut rs_b = state_with_qsy_peer(&INITIATOR_SEED);
        rs_b.local_callsign = "K2XYZ".into(); // valid MYID so the responder may key up (audit F6)
        process_received_bytes(&bytes, &mut rs_b, None, &ev_b, &mode_b, &mut h.rx_engine).await;
        assert!(
            rs_b.qsy_session.is_some(),
            "B should open a responder session from A's auto-QSY QSY_REQ"
        );
        assert!(
            matches!(rx_b.try_recv(), Ok(ControlEvent::QsyIncoming { .. })),
            "B should emit QsyIncoming"
        );
    }

    #[tokio::test]
    async fn auto_qsy_noop_when_disabled_or_session_in_flight() {
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);

        // Disabled → no session even with a confirmed interferer and candidates.
        let mut engine = engine_with_confirmed_in_band_interferer(2);
        let mut rs = RuntimeControlState::default();
        rs.qsy_candidate_freqs = vec![14_070_000];
        maybe_qsy_on_interference(false, &mut rs, None, &ev_tx, &active_mode, &mut engine).await;
        assert!(rs.qsy_session.is_none(), "disabled must not initiate");

        // A negotiation already in flight → don't start another.
        let mut engine2 = engine_with_confirmed_in_band_interferer(2);
        let mut rs2 = RuntimeControlState::default();
        rs2.qsy_candidate_freqs = vec![14_070_000];
        rs2.qsy_session = Some(QsySession::new_initiator());
        maybe_qsy_on_interference(true, &mut rs2, None, &ev_tx, &active_mode, &mut engine2).await;
        assert!(
            !engine2.in_band_interferers().is_empty(),
            "an in-flight session must leave the hint untouched"
        );
    }

    #[tokio::test]
    async fn repeater_enable_with_prebuilt_spawns_thread_and_disable_joins_it() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        let mut runtime_state = RuntimeControlState::default();

        // Inject a pre-built repeater using LoopbackBackend engines.
        let rep_rx = ModemEngine::new(Box::new(LoopbackBackend::new()));
        let rep_tx = ModemEngine::new(Box::new(LoopbackBackend::new()));
        let rep_cfg = openpulse_repeater::RepeaterConfig {
            enabled: true,
            mode: "BPSK250".to_string(),
            tx_hang_ms: 0,
            full_duplex: false,
            ..Default::default()
        };
        runtime_state.repeater = Some(openpulse_repeater::CrossBandRepeater::new(
            Box::new(openpulse_radio::NoOpPtt::new()),
            rep_rx,
            rep_tx,
            rep_cfg,
        ));

        apply_command_to_engine(
            &ControlCommand::EnableRepeater,
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;

        assert!(runtime_state.repeater_enabled);
        assert!(
            runtime_state.repeater_thread.is_some(),
            "repeater thread should be spawned"
        );
        match rx.recv().await.expect("expected RepeaterChanged event") {
            ControlEvent::RepeaterChanged { enabled } => assert!(enabled),
            other => panic!("expected RepeaterChanged, got {other:?}"),
        }

        // Disable the repeater — sets stop flag, joins the thread.
        apply_command_to_engine(
            &ControlCommand::DisableRepeater,
            &mut engine,
            &active_mode,
            &ev_tx,
            None,
            &mut runtime_state,
        )
        .await;

        assert!(!runtime_state.repeater_enabled);
        assert!(
            runtime_state.repeater_thread.is_none(),
            "thread handle should be cleared after join"
        );
        match rx.recv().await.expect("expected RepeaterChanged event") {
            ControlEvent::RepeaterChanged { enabled } => assert!(!enabled),
            other => panic!("expected RepeaterChanged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn process_received_bytes_with_qsy_req_creates_responder_session() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);

        // ADAPTED at #1252, not inverted — the assertion below is unchanged. Before #1252 this
        // passed on an UNSIGNED REQ, which was the defect: any station in earshot could open a
        // responder session. The fixture is now signed by the peer this station verified, because
        // that is the only line that may. Note what this test therefore is and is not: it exercises
        // responder-session creation, and it is NOT a guard on the authentication — a build that
        // stopped verifying would still pass it. The guards are in
        // `tests/qsy_lines_are_authenticated.rs`, each with its own negative control.
        let seed = [7u8; 32];
        let mut runtime_state = state_with_qsy_peer(&seed);
        let qsy_req_line = signed_qsy_line(
            &QsyFrame::Req {
                token: "tok-resp".into(),
                n_candidates: 2,
            },
            &seed,
        );
        let qsy_req = qsy_req_line.as_bytes();
        process_received_bytes(
            qsy_req,
            &mut runtime_state,
            None,
            &ev_tx,
            &active_mode,
            &mut engine,
        )
        .await;

        assert!(
            runtime_state.qsy_session.is_some(),
            "responder session should be created on first QSY_REQ"
        );
    }

    /// Audit 2026-07-19 #4: a single spoofed, unsigned QSY_REQ created a responder session that was
    /// never cleared, and `maybe_qsy_on_interference` returns early whenever a session exists — so
    /// one frame permanently disabled the anti-jam response for the daemon's lifetime.
    #[tokio::test]
    async fn a_stale_qsy_session_does_not_permanently_block_auto_qsy() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);

        // One spoofed inbound REQ. Nothing else happens: the "peer" never sends QSY_LIST, which is
        // exactly what an attacker who only wants to jam the anti-jam response would do.
        // Signed since #1252 — an unsigned REQ no longer creates a session at all, so the
        // precondition below would fail for the wrong reason. The point of this test is the TTL, not
        // the authentication: a negotiation whose peer walks away must still time out.
        let seed = [9u8; 32];
        let mut runtime_state = state_with_qsy_peer(&seed);
        process_received_bytes(
            signed_qsy_line(
                &QsyFrame::Req {
                    token: "spoofed".into(),
                    n_candidates: 2,
                },
                &seed,
            )
            .as_bytes(),
            &mut runtime_state,
            None,
            &ev_tx,
            &active_mode,
            &mut engine,
        )
        .await;
        assert!(
            runtime_state.qsy_session.is_some(),
            "precondition: the REQ must create a responder session, else this proves nothing"
        );

        // Simulate the TTL having elapsed with the negotiation never advancing. A zero TTL means
        // "anything not created this instant is stale", which is the same condition the daemon tick
        // reaches after QSY_SESSION_TTL of silence.
        runtime_state.expire_stale_qsy_session(Duration::ZERO);

        assert!(
            runtime_state.qsy_session.is_none(),
            "an abandoned QSY session must expire — otherwise one unsigned frame disables auto-QSY \
             for the daemon's lifetime"
        );
    }

    /// Control: a session that is still within its TTL must NOT be expired, or an in-progress
    /// negotiation would be torn down mid-flight.
    #[tokio::test]
    async fn a_live_qsy_session_is_not_expired() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);

        // Signed since #1252; see `state_with_qsy_peer`.
        let seed = [11u8; 32];
        let mut runtime_state = state_with_qsy_peer(&seed);
        process_received_bytes(
            signed_qsy_line(
                &QsyFrame::Req {
                    token: "live".into(),
                    n_candidates: 2,
                },
                &seed,
            )
            .as_bytes(),
            &mut runtime_state,
            None,
            &ev_tx,
            &active_mode,
            &mut engine,
        )
        .await;

        // The real TTL: the session was created moments ago, so it must survive.
        runtime_state.expire_stale_qsy_session(QSY_SESSION_TTL);
        assert!(
            runtime_state.qsy_session.is_some(),
            "a session well inside its TTL must survive — expiring it would tear down an \
             in-progress negotiation mid-flight"
        );
    }

    #[tokio::test]
    async fn process_received_bytes_ignores_non_qsy_text() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        let mut runtime_state = RuntimeControlState::default();

        process_received_bytes(
            b"hello world",
            &mut runtime_state,
            None,
            &ev_tx,
            &active_mode,
            &mut engine,
        )
        .await;

        assert!(
            runtime_state.qsy_session.is_none(),
            "no session for non-QSY text"
        );
    }

    #[tokio::test]
    async fn process_received_bytes_ignores_non_utf8() {
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        let mut runtime_state = RuntimeControlState::default();

        process_received_bytes(
            &[0xff, 0xfe, 0x00],
            &mut runtime_state,
            None,
            &ev_tx,
            &active_mode,
            &mut engine,
        )
        .await;

        assert!(
            runtime_state.qsy_session.is_none(),
            "no session for non-UTF-8 bytes"
        );
    }

    /// End-to-end test: an initiator session generates a QSY_REQ frame; that
    /// frame is fed to the responder path, which must create a session and emit
    /// `QsyIncoming`.
    #[tokio::test]
    async fn qsy_initiator_req_drives_responder_session_and_emits_event() {
        // ── Initiator: build the QSY_REQ frame ─────────────────────────────
        let candidates: Vec<u64> = vec![14074000, 14070000, 7074000];
        let mut init_session = QsySession::new_initiator();
        let actions = init_session
            .initiate(candidates)
            .expect("initiator initiate should succeed");

        // The first action must be SendFrame(Req).
        let req_text = actions
            .iter()
            .find_map(|a| {
                if let QsyAction::SendFrame(frame @ QsyFrame::Req { .. }) = a {
                    // Signed as the initiator would send it (#1252): an unsigned line is refused
                    // before the responder looks at it, so feeding one would test nothing.
                    Some(signed_qsy_line(frame, &INITIATOR_SEED))
                } else {
                    None
                }
            })
            .expect("initiator must produce SendFrame(Req) action");

        // Extract the token from the encoded line for assertion.
        // Decoded, not counted. This used to be `split_whitespace().nth(1)`, which read the token
        // by POSITION — so #1162's wire token shifted every field by one and the assertion compared
        // the verb against the token. Parsing through the codec cannot drift with the format.
        // Read it back the way a receiver does — `decode_signed`, which verifies against the
        // expected key and checks freshness in one step. Using the unsigned codec here would have
        // asserted on a line no receiver would accept.
        let decoded = openpulse_qsy::frame::decode_signed(
            req_text.trim(),
            &ed25519_dalek::SigningKey::from_bytes(&INITIATOR_SEED)
                .verifying_key()
                .to_bytes(),
            openpulse_core::handshake::Freshness {
                now_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                max_skew_ms: HANDSHAKE_MAX_SKEW_MS,
            },
        )
        .expect("our own signed REQ verifies and is fresh");
        let token_from_line = match decoded {
            QsyFrame::Req { token, .. } => token,
            other => panic!("expected a QSY_REQ, got {other:?}"),
        };

        // ── Responder: feed the frame ───────────────────────────────────────
        let mut engine = test_engine();
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".to_string()));
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(32);
        let ev_tx = Arc::new(tx);
        // Bound to the initiator's key: since #1252 a responder opens a session only for a line
        // signed by the peer it verified.
        let mut runtime_state = state_with_qsy_peer(&INITIATOR_SEED);

        process_received_bytes(
            req_text.as_bytes(),
            &mut runtime_state,
            None,
            &ev_tx,
            &active_mode,
            &mut engine,
        )
        .await;

        // Responder must have a session.
        assert!(
            runtime_state.qsy_session.is_some(),
            "responder session should be created on receiving QSY_REQ"
        );

        // A QsyIncoming event must have been broadcast with matching token.
        let ev = rx.recv().await.expect("expected QsyIncoming event");
        match ev {
            ControlEvent::QsyIncoming {
                token,
                n_candidates,
            } => {
                assert_eq!(token, token_from_line, "QsyIncoming token must match frame");
                assert_eq!(
                    n_candidates, 3,
                    "n_candidates must match initiator's candidate list length"
                );
            }
            other => panic!("expected QsyIncoming, got {other:?}"),
        }
    }

    #[test]
    fn ptt_watchdog_fires_after_deadline() {
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        // A zero max-duration makes any armed deadline immediately expired — fully deterministic,
        // and avoids subtracting past the process uptime on freshly booted CI containers.
        let mut state = RuntimeControlState::default();
        state.ptt.set_max_duration(Duration::ZERO);
        state.ptt.arm();
        let fired = check_ptt_watchdog(&mut state, &ev_tx);
        assert!(fired, "watchdog must fire when deadline is exceeded");
        assert!(!state.ptt.is_keyed(), "PTT deadline must be cleared");
        let ev = rx.try_recv().expect("PttChanged event must be emitted");
        assert!(
            matches!(ev, ControlEvent::PttChanged { active: false }),
            "event must be PttChanged {{active: false}}"
        );
    }

    #[test]
    fn ptt_watchdog_silent_before_deadline() {
        let (tx, _) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        let mut state = RuntimeControlState::default();
        state.ptt.arm();
        let fired = check_ptt_watchdog(&mut state, &ev_tx);
        assert!(!fired, "watchdog must not fire before deadline");
        assert!(state.ptt.is_keyed(), "PTT deadline must remain armed");
    }

    #[test]
    fn ptt_watchdog_silent_when_ptt_not_active() {
        let (tx, _) = broadcast::channel::<ControlEvent>(16);
        let ev_tx = Arc::new(tx);
        let mut state = RuntimeControlState::default();
        let fired = check_ptt_watchdog(&mut state, &ev_tx);
        assert!(!fired, "watchdog must not fire when PTT is not active");
    }

    #[test]
    fn rendezvous_token_is_two_base36_chars() {
        let t = rendezvous_token(1_700_000_123_456);
        assert_eq!(t.len(), 2);
        assert!(t
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()));
    }

    /// A discovery runtime in `tx_mode` with `callsign`, dwelling on the 20 m calling channel.
    fn discovery_rs(tx_mode: openpulse_discovery::TxMode, callsign: &str) -> RuntimeControlState {
        use openpulse_discovery::{DiscoveryParams, DiscoveryRuntime, Submode};
        RuntimeControlState {
            discovery_rendezvous_channels_hz: [(
                "20m".to_string(),
                vec![14_101_000, 14_103_000, 14_105_000],
            )]
            .into_iter()
            .collect(),
            discovery: Some(DiscoveryRuntime::new(DiscoveryParams {
                enabled: true,
                idle_grace_ms: 0,
                dwell_ms: 0,
                station_ttl_ms: 3_600_000,
                submode: Submode::Normal,
                calling_freq_hz: 14_078_000, // 20 m
                tx_mode,
                callsign: callsign.into(),
                grid: "JN58".into(),
                hint: None,
                heartbeat_interval_slots: 8,
                hint_interval_beacons: 0,
                tx_offset_hz: 1500.0,
                max_clock_skew_ms: 2000,
            })),
            ..RuntimeControlState::default()
        }
    }

    fn drain_command_errors(rx: &mut broadcast::Receiver<ControlEvent>) -> Vec<String> {
        let mut errs = Vec::new();
        while let Ok(e) = rx.try_recv() {
            if let ControlEvent::CommandError { command, .. } = e {
                errs.push(command);
            }
        }
        errs
    }

    #[tokio::test]
    async fn dispatch_rejects_unknown_mode_without_mutating_state() {
        // Audit #14: a bad SetMode must be rejected before it writes active_mode, so a typo can't
        // silently deafen RX/station-ID while the client is told "ok".
        let (cmd_tx, _cmd_rx) = mpsc::channel::<ControlCommand>(4);
        let active_mode: SharedMode = Arc::new(Mutex::new("BPSK250".into()));
        let atten: SharedAttenuation = Arc::new(Mutex::new(0.0));
        let qsy: SharedQsyEnabled = Arc::new(Mutex::new(false));
        let bp: SharedBandplanMode = Arc::new(Mutex::new("ham_iaru_region1".into()));
        let tuner: SharedTunerOnHighSWR = Arc::new(Mutex::new(false));
        let valid: ValidModes = Arc::new(["BPSK250".to_string(), "QPSK500".to_string()].into());

        // Unknown mode → error response, state untouched, command NOT forwarded.
        let resp = dispatch_command(
            &ControlCommand::SetMode {
                mode: "NONSENSE999".into(),
            },
            &cmd_tx,
            &active_mode,
            &atten,
            &qsy,
            &bp,
            &tuner,
            &valid,
        )
        .await;
        assert!(!resp.ok, "unknown mode must be rejected: {resp:?}");
        assert_eq!(*active_mode.lock().await, "BPSK250", "state unchanged");

        // Valid mode → applied.
        let resp = dispatch_command(
            &ControlCommand::SetMode {
                mode: "QPSK500".into(),
            },
            &cmd_tx,
            &active_mode,
            &atten,
            &qsy,
            &bp,
            &tuner,
            &valid,
        )
        .await;
        assert!(resp.ok, "valid mode accepted: {resp:?}");
        assert_eq!(*active_mode.lock().await, "QPSK500");
    }

    #[test]
    fn rendezvous_with_starts_a_proposal_when_configured() {
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);
        let mut rs = discovery_rs(openpulse_discovery::TxMode::Full, "DC0SK");
        start_rendezvous_cmd("KN4CRD", &mut rs, &ev, 1_700_000_000_000);
        assert!(
            rs.discovery.as_ref().unwrap().rendezvous_active(),
            "a proposal is in flight"
        );
        assert!(
            drain_command_errors(&mut rx).is_empty(),
            "no error on the happy path"
        );
    }

    #[test]
    fn rendezvous_with_errors_when_discovery_is_unconfigured() {
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);
        let mut rs = RuntimeControlState::default(); // discovery: None
        start_rendezvous_cmd("KN4CRD", &mut rs, &ev, 1_700_000_000_000);
        assert_eq!(drain_command_errors(&mut rx), vec!["rendezvous_with"]);
    }

    #[test]
    fn rendezvous_with_errors_without_channels_for_the_band() {
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);
        let mut rs = discovery_rs(openpulse_discovery::TxMode::Full, "DC0SK");
        rs.discovery_rendezvous_channels_hz.clear(); // no table for 20 m
        start_rendezvous_cmd("KN4CRD", &mut rs, &ev, 1_700_000_000_000);
        assert_eq!(drain_command_errors(&mut rx), vec!["rendezvous_with"]);
        assert!(!rs.discovery.as_ref().unwrap().rendezvous_active());
    }

    #[test]
    fn rendezvous_with_errors_without_a_callsign() {
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);
        let mut rs = discovery_rs(openpulse_discovery::TxMode::Full, ""); // no callsign → TX gated
        start_rendezvous_cmd("KN4CRD", &mut rs, &ev, 1_700_000_000_000);
        assert_eq!(drain_command_errors(&mut rx), vec!["rendezvous_with"]);
        assert!(!rs.discovery.as_ref().unwrap().rendezvous_active());
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod handshake_rf_tests {
    use super::command_apply_tests::{signed_qsy_line, state_with_qsy_peer};
    use super::*;
    use bpsk_plugin::BpskPlugin;
    use openpulse_audio::LoopbackBackend;
    use tokio::sync::Mutex;

    fn bpsk_engine() -> ModemEngine {
        let mut e = ModemEngine::new(Box::new(LoopbackBackend::new()));
        e.register_plugin(Box::new(BpskPlugin::new())).unwrap();
        e
    }

    /// A signed CONREQ addressed to `dst`, as transmitted.
    fn test_conreq(dst: &str) -> Vec<u8> {
        ConReq::create(
            &ConReqParams {
                station_id: "W1AW",
                dst_station: dst,
                signing_modes: vec![SigningMode::Normal],
                session_id: 1_700_000_000_000,
                station_grid: "FN31pr",
                profile_name: "",
                profile_fingerprint: 0,
                timestamp_ms: unix_now_ms(),
                kex_pubkey: &[0u8; 32],
            },
            &[1u8; 32],
        )
        .unwrap()
    }

    /// A signed CONACK from `K2XYZ` bound to `conreq`.
    fn test_conack(conreq: &[u8], grid: &str) -> Vec<u8> {
        ConAck::create(
            &ConAckParams {
                station_id: "K2XYZ",
                selected_mode: SigningMode::Normal,
                conreq_hash: openpulse_core::handshake::conreq_hash(conreq),
                station_grid: grid,
                profile_name: "",
                profile_fingerprint: 0,
                timestamp_ms: unix_now_ms(),
                kex_pubkey: &[0u8; 32],
            },
            &[2u8; 32],
        )
        .unwrap()
    }

    thread_local! {
        /// One CONREQ per test thread, so a CONACK can be bound to the exact bytes "we sent".
        static CONREQ_FOR_INIT: Vec<u8> = test_conreq("K2XYZ");
    }

    fn pending_for(conreq: &[u8]) -> PendingHandshake {
        PendingHandshake {
            session_id: 1_700_000_000_000,
            peer_callsign: "K2XYZ".into(),
            started_at: Instant::now(),
            kex_secret: [0u8; 32],
            conreq_bytes: conreq.to_vec(),
            offered_modes: vec![SigningMode::Normal],
        }
    }

    fn mode() -> SharedMode {
        Arc::new(Mutex::new("BPSK250".to_string()))
    }

    /// The responder reassembles a fragmented, signed CONREQ from RF, records the proven peer
    /// identity (callsign + grid + pubkey), and emits `PeerVerified`.
    #[tokio::test]
    async fn responder_verifies_a_single_fragment_conreq_and_records_peer() {
        // This test used to assert `frags.len() > 1` — "a CONREQ exceeds one modem frame". #1147
        // INVERTS that premise, and the inversion is the point of the change: a v1 CONREQ was 752 B
        // = 3 SAR fragments = three preambles and three acquisitions, decoding at ~p^3 on a fading
        // channel. The assertion is flipped rather than deleted so the property is pinned, not
        // merely no longer contradicted.
        let conreq = test_conreq("K2XYZ");
        let frags = sar_encode(0, &conreq).unwrap();
        assert_eq!(
            frags.len(),
            1,
            "a v2 CONREQ must fit ONE modem frame; it took {} — the fragment budget has regressed",
            frags.len()
        );

        let mut eng = bpsk_engine();
        let mode = mode();
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);
        let mut rs = RuntimeControlState {
            local_callsign: "K2XYZ".into(),
            local_grid: "EM69".into(),
            station_seed: [2u8; 32],
            ..RuntimeControlState::default()
        };

        for frag in &frags {
            process_received_bytes(frag, &mut rs, None, &ev, &mode, &mut eng).await;
        }

        let vp = rs
            .last_verified_peer()
            .cloned()
            .expect("peer verified from the single fragment");
        assert_eq!(vp.callsign, "W1AW");
        assert_eq!(vp.grid, "FN31pr");
        assert_eq!(vp.pubkey, ConReq::decode(&conreq).unwrap().pubkey);
        // Step over the PTT edges the CONACK's keying now emits (#1262).
        let mut ev = rx.try_recv();
        while matches!(ev, Ok(ControlEvent::PttChanged { .. })) {
            ev = rx.try_recv();
        }
        assert!(
            matches!(ev, Ok(ControlEvent::PeerVerified { callsign, grid })
                if callsign == "W1AW" && grid == "FN31pr"),
            "PeerVerified event should be emitted"
        );
    }

    /// SAR-poison resilience: a crafted fragment sharing the constant handshake key must not block
    /// a legitimate CONREQ from reassembling and verifying.
    ///
    /// **#1147 shrinks this attack surface rather than removing the test.** A v1 CONREQ spanned
    /// three fragments, so a poisoner had a window in which the real frame was incomplete and could
    /// be blocked by a conflicting stream on the same key. A v2 CONREQ is ONE fragment and
    /// reassembles on arrival, so there is no incomplete window to occupy — but poison on the same
    /// key is still possible, and must still fail to verify a peer on its own.
    #[tokio::test]
    async fn poison_fragment_does_not_block_conreq_verification() {
        let conreq = test_conreq("K2XYZ");
        let frags = sar_encode(0, &conreq).unwrap();

        let mut eng = bpsk_engine();
        let mode = mode();
        let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);
        let mut rs = RuntimeControlState {
            local_callsign: "K2XYZ".into(),
            local_grid: "EM69".into(),
            station_seed: [2u8; 32],
            ..RuntimeControlState::default()
        };

        // A poison fragment on the same handshake key (segment_id 0): a self-contained garbage
        // "frame" that reassembles immediately but carries no HSCQ magic, plus a conflicting
        // index-0 claim of a multi-fragment stream.
        let poison_solo = vec![0u8, 0, 0, 1, 0xDE, 0xAD, 0xBE, 0xEF];
        let mut poison_conflict = vec![0u8, 0, 0, 3];
        poison_conflict.extend(vec![0x99u8; 100]);
        process_received_bytes(&poison_solo, &mut rs, None, &ev, &mode, &mut eng).await;
        process_received_bytes(&poison_conflict, &mut rs, None, &ev, &mode, &mut eng).await;
        assert!(
            rs.last_verified_peer().is_none(),
            "poison must not verify a peer on its own"
        );

        for frag in &frags {
            process_received_bytes(frag, &mut rs, None, &ev, &mode, &mut eng).await;
        }
        let vp = rs
            .last_verified_peer()
            .cloned()
            .expect("peer verified despite the poison fragments");
        assert_eq!(vp.callsign, "W1AW");
    }

    /// Audit F6/F4 unit coverage: `local_callsign_valid` rejects the empty/`N0CALL` sentinels, and
    /// `rf_peer_trust` maps a verified peer to its over-air trust (never `Verified` over RF).
    #[test]
    fn callsign_validity_and_rf_peer_trust() {
        let mut rs = RuntimeControlState {
            local_callsign: String::new(),
            ..Default::default()
        };
        assert!(!rs.local_callsign_valid(), "empty callsign is not valid");
        rs.local_callsign = "n0call".into();
        assert!(!rs.local_callsign_valid(), "N0CALL sentinel is not valid");
        rs.local_callsign = "W1AW".into();
        assert!(rs.local_callsign_valid(), "a real callsign is valid");

        // No verified peer this session → Unverified.
        assert_eq!(rs.rf_peer_trust(), ConnectionTrustLevel::Unverified);

        rs.verified_peers.insert(
            "K2XYZ".into(),
            VerifiedPeer {
                callsign: "K2XYZ".into(),
                grid: "EM69".into(),
                pubkey: vec![2u8; 32],
                profile_compatible: None,
            },
        );
        rs.last_verified_callsign = Some("K2XYZ".into());
        // First-seen (unknown) key over air → Low.
        assert_eq!(rs.rf_peer_trust(), ConnectionTrustLevel::Low);

        // A trust-store (Full) key, but still over-air without PSK → Reduced, never Verified.
        rs.trust_store
            .add_entry("K2XYZ", [2u8; 32], PublicKeyTrustLevel::Full);
        assert_eq!(rs.rf_peer_trust(), ConnectionTrustLevel::Reduced);
    }

    /// Audit F6 (§97.119): a responder with no valid callsign that hears a full CONREQ must not key
    /// the transmitter to answer with a CONACK, and must not record a half-handshake the peer never
    /// sees completed.
    #[tokio::test]
    async fn responder_without_callsign_does_not_transmit_conack() {
        let frags = sar_encode(0, &test_conreq("*")).unwrap();

        let mut eng = bpsk_engine();
        let mode = mode();
        let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);
        let mut rs = RuntimeControlState {
            local_callsign: String::new(), // no MYID
            station_seed: [2u8; 32],
            ..RuntimeControlState::default()
        };

        let before = eng.frames_transmitted();
        for frag in &frags {
            process_received_bytes(frag, &mut rs, None, &ev, &mode, &mut eng).await;
        }
        assert_eq!(
            eng.frames_transmitted(),
            before,
            "no CONACK may be transmitted without a valid callsign"
        );
        assert!(
            rs.last_verified_peer().is_none(),
            "a half-handshake must not be recorded when we cannot reply"
        );
    }

    /// #1178 THROUGH THE TX PATH: a CONREQ addressed to another station is not answered.
    ///
    /// Asserted on the engine's transmit counter, not on the filter function, because the defect
    /// being fixed is *spent RF* — a unit check on `is_addressed_to` would pass even if the daemon
    /// went on to key up anyway. The positive control in the same test is what makes the negative
    /// meaningful: the identical setup DOES transmit when the request is addressed to us.
    /// #1203: dialling the wildcard `"*"` must refuse, and must NOT key the transmitter.
    ///
    /// Asserted on the engine's transmit counter, like #1178, because the defect is *spent RF*: a
    /// wildcard CONREQ is answered by EVERY daemon in range. Checking only that a `CommandError`
    /// is emitted would pass on code that errored *after* transmitting, which is the failure this
    /// pins. The positive control — an ordinary callsign, identical setup — is what makes the zero
    /// meaningful; without it the assertion would also pass if `ConnectPeer` transmitted nothing
    /// at all.
    #[tokio::test]
    async fn a_wildcard_connect_peer_refuses_and_does_not_key_the_transmitter() {
        async fn dial(peer: &str) -> (u64, Vec<ControlEvent>, bool, bool) {
            let mut eng = bpsk_engine();
            let mode = mode();
            let (tx, mut rx) = broadcast::channel::<ControlEvent>(32);
            let ev = Arc::new(tx);
            let mut rs = RuntimeControlState {
                local_callsign: "K2XYZ".into(),
                local_grid: "EM69".into(),
                station_seed: [3u8; 32],
                ..RuntimeControlState::default()
            };
            let before = eng.frames_transmitted();
            apply_command_to_engine(
                &ControlCommand::ConnectPeer {
                    callsign: peer.to_string(),
                },
                &mut eng,
                &mode,
                &ev,
                None,
                &mut rs,
            )
            .await;
            let sent = eng.frames_transmitted() - before;
            let mut events = Vec::new();
            while let Ok(e) = rx.try_recv() {
                events.push(e);
            }
            (
                sent,
                events,
                rs.pending_handshake.is_some(),
                rs.qsy_pending_token.is_some(),
            )
        }

        // POSITIVE CONTROL: an ordinary dial DOES key the transmitter.
        let (ok_sent, _, ok_pending, _) = dial("W1AW").await;
        assert!(
            ok_sent > 0,
            "control: an ordinary ConnectPeer must transmit a CONREQ"
        );
        assert!(ok_pending, "control: an ordinary dial arms the handshake");

        // THE DEFECT: the wildcard dial spends no RF and announces nothing.
        let (sent, events, pending, token) = dial("*").await;
        assert_eq!(
            sent, 0,
            "a wildcard dial must not key the transmitter — every station in range answers it"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                ControlEvent::CommandError { command, .. } if command == "connect_peer"
            )),
            "a refused wildcard dial must say so"
        );
        assert!(
            !events.iter().any(|e| matches!(
                e,
                ControlEvent::RfConnectionChanged {
                    connected: true,
                    ..
                }
            )),
            "a refused dial must not announce a connection"
        );
        assert!(!pending, "a refused dial must not arm the handshake");
        assert!(!token, "a refused dial must not set a QSY token");
    }

    /// #1199: a `ConnectPeer` whose CONREQ cannot be built must announce NOTHING and say so.
    ///
    /// The defect this pins is not a missing log line — it is ordering. The handler used to run
    /// `begin_secure_session`, emit `RfConnectionChanged { connected: true }`, open a logbook QSO
    /// and set a QSY token BEFORE building the CONREQ, then only `warn!` if the build failed. The
    /// result was a session that reported itself connected and could still key the transmitter
    /// while no CONREQ was ever transmitted, so the peer never learned it existed — and since
    /// `pending_handshake` was never set, the 30 s handshake-timeout `CommandError` could not fire
    /// either. A lost CONACK reported a timeout; a build failure reported nothing, ever.
    ///
    /// Asserting only "a CommandError is emitted" would pass on the OLD code with a one-line
    /// change, so this asserts the whole property: an error IS emitted, `RfConnectionChanged` is
    /// NOT, no QSO is open, and no handshake is pending. The positive control in the same test —
    /// an ordinary callsign, identical setup — is what makes the negative meaningful.
    #[tokio::test]
    async fn connect_peer_with_an_unbuildable_conreq_announces_nothing() {
        async fn connect(local: &str, peer: &str) -> (Vec<ControlEvent>, bool, bool) {
            let mut eng = bpsk_engine();
            let mode = mode();
            let (tx, mut rx) = broadcast::channel::<ControlEvent>(32);
            let ev = Arc::new(tx);
            let mut rs = RuntimeControlState {
                local_callsign: local.into(),
                local_grid: "EM69".into(),
                station_seed: [3u8; 32],
                ..RuntimeControlState::default()
            };
            apply_command_to_engine(
                &ControlCommand::ConnectPeer {
                    callsign: peer.to_string(),
                },
                &mut eng,
                &mode,
                &ev,
                None,
                &mut rs,
            )
            .await;
            let mut events = Vec::new();
            while let Ok(e) = rx.try_recv() {
                events.push(e);
            }
            (
                events,
                rs.pending_handshake.is_some(),
                rs.qsy_pending_token.is_some(),
            )
        }

        // POSITIVE CONTROL: ordinary callsigns — the connection is announced and armed.
        let (ok_events, ok_pending, ok_token) = connect("K2XYZ", "W1AW").await;
        assert!(
            ok_events.iter().any(|e| matches!(
                e,
                ControlEvent::RfConnectionChanged {
                    connected: true,
                    ..
                }
            )),
            "control: an ordinary ConnectPeer must announce the connection"
        );
        assert!(
            ok_pending,
            "control: an ordinary ConnectPeer must arm the handshake"
        );
        assert!(
            ok_token,
            "control: an ordinary ConnectPeer must set the QSY token"
        );

        // Every route to an unbuildable CONREQ, per #1199.
        //
        // The over-cap callsign is DERIVED from the cap, not written as a literal. This used to
        // hard-code `SV5/DL1ABCD/P` (13 chars), which was over the cap at 12 and became LEGAL at 18
        // (#1191) — the frame then encoded fine and this test failed for the right reason: there
        // was nothing left to refuse. A fixture pinned to a constant's VALUE goes stale the moment
        // the value moves; one derived from the constant cannot.
        let over_cap = "A".repeat(openpulse_core::handshake_wire::caps::STATION_ID + 1);
        for (local, peer, why) in [
            (
                over_cap.as_str(),
                "W1AW",
                "local callsign over caps::STATION_ID",
            ),
            (
                "K2XYZ",
                over_cap.as_str(),
                "PEER callsign over the cap — config validation cannot catch this",
            ),
            ("K2XYZ", "", "empty dst_station"),
        ] {
            let (events, pending, token) = connect(local, peer).await;
            assert!(
                events.iter().any(|e| matches!(e, ControlEvent::CommandError { command, .. } if command == "connect_peer")),
                "{why}: must emit a CommandError, not fail silently"
            );
            assert!(
                !events.iter().any(|e| matches!(
                    e,
                    ControlEvent::RfConnectionChanged {
                        connected: true,
                        ..
                    }
                )),
                "{why}: must NOT announce a connection it cannot transmit"
            );
            assert!(!pending, "{why}: must not arm a handshake");
            assert!(!token, "{why}: must not set a QSY token");
        }
    }

    #[tokio::test]
    async fn a_conreq_addressed_elsewhere_does_not_key_the_transmitter() {
        async fn transmits_for(dst: &str) -> u64 {
            let mut eng = bpsk_engine();
            let mode = mode();
            let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
            let ev = Arc::new(tx);
            let mut rs = RuntimeControlState {
                local_callsign: "K2XYZ".into(),
                local_grid: "EM69".into(),
                station_seed: [2u8; 32],
                ..RuntimeControlState::default()
            };
            let before = eng.frames_transmitted();
            for frag in &sar_encode(0, &test_conreq(dst)).unwrap() {
                process_received_bytes(frag, &mut rs, None, &ev, &mode, &mut eng).await;
            }
            eng.frames_transmitted() - before
        }

        assert!(
            transmits_for("K2XYZ").await > 0,
            "POSITIVE CONTROL FAILED: a CONREQ addressed to us produced no transmission, so the \
             zero below would prove nothing about addressing"
        );
        assert!(
            transmits_for("*").await > 0,
            "a broadcast CONREQ must still be answered"
        );
        assert_eq!(
            transmits_for("DL9ZZZ").await,
            0,
            "a CONREQ addressed to another station keyed the transmitter — every daemon in range \
             spends RF on a request that was never for it (#1178)"
        );
    }

    /// Audit F6 (§97.119): a responder with no valid callsign that hears a QSY_REQ must not engage
    /// the QSY responder (which would key the transmitter for a reply, even a Reject).
    #[tokio::test]
    async fn responder_without_callsign_ignores_qsy_req() {
        // The line must be genuinely VALID, or this test passes for the wrong reason: since #1252 an
        // unsigned line is refused before the callsign gate is ever reached, so an unsigned fixture
        // would assert nothing about §97.119. Signed by a verified peer, the only thing left to stop
        // the reply is the missing MYID — which is what this test is for.
        let seed = [13u8; 32];
        let req = signed_qsy_line(
            &QsyFrame::Req {
                token: "TOK123".into(),
                n_candidates: 3,
            },
            &seed,
        );

        let mut eng = bpsk_engine();
        let mode = mode();
        let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);
        let mut rs = state_with_qsy_peer(&seed);
        rs.local_callsign = "N0CALL".into();

        let before = eng.frames_transmitted();
        process_received_bytes(req.as_bytes(), &mut rs, None, &ev, &mode, &mut eng).await;
        assert_eq!(
            eng.frames_transmitted(),
            before,
            "no QSY reply may be transmitted without a valid callsign"
        );
        assert!(
            rs.qsy_session.is_none(),
            "no QSY responder session may be created without a valid callsign"
        );
    }

    /// The initiator verifies the peer's CONACK against its in-flight CONREQ, records the verified
    /// peer, clears the pending handshake, and stamps the verified grid onto the logbook QSO.
    #[tokio::test]
    async fn initiator_verifies_conack_and_stamps_logbook_grid() {
        let tmp = std::env::temp_dir().join(format!("ophs-init-{}.adi", std::process::id()));
        let _ = std::fs::remove_file(&tmp);

        let mut rs = RuntimeControlState {
            local_callsign: "W1AW".into(),
            local_grid: "FN31".into(),
            station_seed: [1u8; 32],
            pending_handshake: Some(pending_for(&CONREQ_FOR_INIT.with(|c| c.clone()))),
            ..RuntimeControlState::default()
        };
        rs.logbook = crate::logbook::Logbook::new(
            true,
            tmp.to_str().unwrap(),
            "W1AW",
            "FN31",
            &Default::default(),
        );
        rs.logbook
            .begin_qso("K2XYZ", "BPSK250", Some(14_070_000), 1_700_000_000_000);

        let conack = test_conack(&CONREQ_FOR_INIT.with(|c| c.clone()), "EM69");
        let frags = sar_encode(0, &conack).unwrap();

        let mut eng = bpsk_engine();
        let mode = mode();
        let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);
        for frag in &frags {
            process_received_bytes(frag, &mut rs, None, &ev, &mode, &mut eng).await;
        }

        let vp = rs.last_verified_peer().cloned().expect("verified");
        assert_eq!(vp.callsign, "K2XYZ");
        assert_eq!(vp.grid, "EM69");
        assert!(
            rs.pending_handshake.is_none(),
            "pending handshake cleared on verified CONACK"
        );

        // The verified on-air grid (EM69) must land in the ADIF QSO record.
        rs.logbook.end_qso(1_700_000_300_000, Some(10.0)).unwrap();
        let body = std::fs::read_to_string(&tmp).unwrap();
        assert!(
            body.contains("<GRIDSQUARE:4>EM69"),
            "ADIF should carry the verified grid; got: {body}"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    /// A CONACK bound to a DIFFERENT CONREQ is ignored: no peer is recorded and the pending
    /// handshake is preserved, so the real CONACK can still complete it.
    ///
    /// v1 matched on the session id echo. v2 matches on a hash over the whole transmitted CONREQ,
    /// which is strictly harder to forge: the daemon's own comment conceded the session id is
    /// cleartext and time-based, hence guessable inside the handshake window.
    #[tokio::test]
    async fn conack_bound_to_another_conreq_is_ignored() {
        let ours = test_conreq("K2XYZ");
        let mut rs = RuntimeControlState {
            local_callsign: "W1AW".into(),
            station_seed: [1u8; 32],
            pending_handshake: Some(pending_for(&ours)),
            ..RuntimeControlState::default()
        };
        // A CONACK bound to some other CONREQ entirely.
        let mut other = test_conreq("K2XYZ");
        other[7] ^= 0xFF;
        let conack = test_conack(&other, "");
        let frags = sar_encode(0, &conack).unwrap();

        let mut eng = bpsk_engine();
        let mode = mode();
        let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);
        for frag in &frags {
            process_received_bytes(frag, &mut rs, None, &ev, &mode, &mut eng).await;
        }
        assert!(
            rs.last_verified_peer().is_none(),
            "mismatched session must not verify"
        );
        assert!(
            rs.pending_handshake.is_some(),
            "mismatched CONACK must not clear the pending handshake"
        );
    }

    /// Audit F2: a CONACK that echoes the correct (cleartext, guessable) session id but comes from a
    /// station other than the one we dialed is ignored — an attacker cannot race a self-signed CONACK
    /// under their own callsign and be recorded as the dialed peer. The pending handshake is preserved.
    #[tokio::test]
    async fn conack_from_undialed_station_is_ignored() {
        let mut rs = RuntimeControlState {
            local_callsign: "W1AW".into(),
            station_seed: [1u8; 32],
            pending_handshake: Some(pending_for(&test_conreq("K2XYZ"))),
            ..RuntimeControlState::default()
        };
        // Attacker "N0EVL" signs a CONACK with its own key, binding it CORRECTLY to our CONREQ —
        // the hash is public, so binding is not a secret. What stops it is the dialed-station check.
        let ours = test_conreq("K2XYZ");
        let conack = ConAck::create(
            &ConAckParams {
                station_id: "N0EVL",
                selected_mode: SigningMode::Normal,
                conreq_hash: openpulse_core::handshake::conreq_hash(&ours),
                station_grid: "",
                profile_name: "",
                profile_fingerprint: 0,
                timestamp_ms: unix_now_ms(),
                kex_pubkey: &[0u8; 32],
            },
            &[9u8; 32],
        )
        .unwrap();
        let frags = sar_encode(0, &conack).unwrap();

        let mut eng = bpsk_engine();
        let mode = mode();
        let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);
        for frag in &frags {
            process_received_bytes(frag, &mut rs, None, &ev, &mode, &mut eng).await;
        }
        assert!(
            rs.last_verified_peer().is_none(),
            "CONACK from an undialed station must not verify"
        );
        assert!(
            rs.pending_handshake.is_some(),
            "CONACK from an undialed station must not clear the pending handshake"
        );
    }

    /// `ConnectPeer` initiates the signed handshake: it records a pending handshake keyed on a
    /// session id derived from the local callsign.
    #[tokio::test]
    async fn connect_peer_initiates_signed_handshake() {
        let mut eng = bpsk_engine();
        let mode = mode();
        let (tx, _rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);
        let mut rs = RuntimeControlState {
            local_callsign: "W1AW".into(),
            local_grid: "FN31".into(),
            station_seed: [1u8; 32],
            ..RuntimeControlState::default()
        };
        apply_command_to_engine(
            &ControlCommand::ConnectPeer {
                callsign: "K2XYZ".into(),
            },
            &mut eng,
            &mode,
            &ev,
            None,
            &mut rs,
        )
        .await;
        let p = rs
            .pending_handshake
            .expect("pending handshake set by ConnectPeer");
        assert_eq!(p.peer_callsign, "K2XYZ");
        // Previously: `assert!(p.session_id.starts_with("W1AW-"))`. That pinned the very coupling
        // that caused the silent-downgrade defect — the id embedded the local callsign, so its
        // length tracked the callsign's and overflowed the wire cap for a legal 11-character
        // compound call. The id is now a fixed u64 and carries no callsign; the callsign is already
        // in the frame as `station_id`. What matters is that it is SET and non-degenerate.
        assert_ne!(
            p.session_id, 0,
            "a pending handshake must carry a real session id"
        );
        assert!(
            !p.conreq_bytes.is_empty(),
            "the CONREQ must be retained for CONACK binding"
        );
    }

    /// A CONREQ survives a real BPSK250 round trip — as ONE fragment.
    ///
    /// This asserted a full 255-byte fragment, because a v1 CONREQ was large enough that its first
    /// fragment was always maximal. A v2 CONREQ is 241 B, so it is one sub-maximal fragment. Both
    /// facts are asserted: the single-fragment property (the #1147 win) and that the frame actually
    /// crosses the modem, which is the thing the test was for.
    #[test]
    fn a_conreq_survives_a_bpsk_round_trip_as_one_fragment() {
        use openpulse_modem::channel_sim::ChannelSimHarness;
        let mut h = ChannelSimHarness::new();
        h.tx_engine
            .register_plugin(Box::new(BpskPlugin::new()))
            .unwrap();
        h.rx_engine
            .register_plugin(Box::new(BpskPlugin::new()))
            .unwrap();
        let conreq = test_conreq("K2XYZ");
        let mut frags = sar_encode(0, &conreq).unwrap();
        assert_eq!(
            frags.len(),
            1,
            "a v2 CONREQ must be one SAR fragment; it took {}",
            frags.len()
        );
        let frag = frags.remove(0);
        assert!(
            frag.len() <= 255,
            "a fragment cannot exceed one modem frame; got {}",
            frag.len()
        );
        h.tx_engine.transmit(&frag, "BPSK250", None).unwrap();
        h.route_clean();
        let rx = h.rx_engine.receive("BPSK250", None).unwrap_or_default();
        assert_eq!(
            rx, frag,
            "a CONREQ SAR fragment must survive BPSK250 transport"
        );
    }

    /// An unanswered CONREQ is abandoned after the timeout, emitting a `CommandError`.
    #[test]
    fn pending_handshake_expires_after_timeout() {
        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);
        let mut rs = RuntimeControlState {
            pending_handshake: Some(PendingHandshake {
                started_at: Instant::now() - HANDSHAKE_TIMEOUT - Duration::from_secs(1),
                ..pending_for(&test_conreq("K2XYZ"))
            }),
            ..RuntimeControlState::default()
        };
        expire_pending_handshake(&mut rs, &ev);
        assert!(
            rs.pending_handshake.is_none(),
            "stale handshake must be dropped"
        );
        assert!(
            matches!(rx.try_recv(), Ok(ControlEvent::CommandError { command, .. })
                if command == "connect_peer"),
            "expiry should emit a connect_peer CommandError"
        );
    }

    #[test]
    fn discovery_commands_toggle_the_runtime_and_list_stations() {
        use openpulse_discovery::{DiscoveryParams, DiscoveryRuntime, Submode};

        let (tx, mut rx) = broadcast::channel::<ControlEvent>(16);
        let ev = Arc::new(tx);

        // Unconfigured: EnableDiscovery reports an error.
        let mut rs = RuntimeControlState::default();
        set_discovery_enabled(true, &mut rs, &ev);
        assert!(matches!(
            rx.try_recv(),
            Ok(ControlEvent::CommandError { command, .. }) if command == "enable_discovery"
        ));

        // Configured: EnableDiscovery emits DiscoveryStatus; ListStations emits an (empty) StationList.
        rs.discovery = Some(DiscoveryRuntime::new(DiscoveryParams {
            enabled: false,
            idle_grace_ms: 0,
            dwell_ms: 0,
            station_ttl_ms: 3_600_000,
            submode: Submode::Normal,
            calling_freq_hz: 14_078_000,
            tx_mode: openpulse_discovery::TxMode::RxOnly,
            callsign: String::new(),
            grid: String::new(),
            hint: None,
            heartbeat_interval_slots: 8,
            hint_interval_beacons: 3,
            tx_offset_hz: 1500.0,
            max_clock_skew_ms: 2000,
        }));
        set_discovery_enabled(true, &mut rs, &ev);
        assert!(matches!(
            rx.try_recv(),
            Ok(ControlEvent::DiscoveryStatus { dial_freq_hz, .. }) if dial_freq_hz == 14_078_000
        ));
        emit_station_list(&rs, &ev);
        assert!(matches!(
            rx.try_recv(),
            Ok(ControlEvent::StationList { stations }) if stations.is_empty()
        ));
    }
}

/// A station ID refused because the rig was BUSY defers; one refused by a FAULT does not (#1263).
///
/// This is the property my first #1263 design would have broken. `mark_identified` was called
/// "Advance regardless of PTT success" and clears both `last_id_ms` and `tx_since_id`, so refusing a
/// station ID without distinguishing busy from broken skips a whole §97.119 interval.
///
/// The asymmetry is why `KeyedTxError::AlreadyKeyed` is a third variant rather than an overload of
/// `Assert`: deferring on a hardware fault too would key-attempt a faulted rig at the 50 ms tick
/// rate for the entire 180 s watchdog window.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod station_id_deferral_tests {
    use super::*;
    use openpulse_core::station_id::StationIdTimer;

    const TEN_MIN: u64 = 600_000;

    fn timer() -> StationIdTimer {
        StationIdTimer::new(TEN_MIN, 0)
    }

    /// The decision the rx-tick makes, isolated: mark on success and on fault, defer on busy.
    fn mark_unless_busy(t: &mut StationIdTimer, outcome: Result<(), KeyedTxError>, now_ms: u64) {
        if !matches!(outcome, Err(KeyedTxError::AlreadyKeyed)) {
            t.mark_identified(now_ms);
        }
    }

    #[test]
    fn a_busy_rig_defers_the_id_instead_of_skipping_an_interval() {
        let mut t = timer();
        t.note_tx(0);
        assert!(t.id_due(TEN_MIN), "precondition: an ID is due");

        mark_unless_busy(&mut t, Err(KeyedTxError::AlreadyKeyed), TEN_MIN);
        assert!(
            t.id_due(TEN_MIN),
            "a station ID refused because another emission held the key must stay DUE — marking it \
             skips a full §97.119 interval, which is what the first #1263 design would have done"
        );

        // It goes out on the next tick after the holder releases.
        mark_unless_busy(&mut t, Ok(()), TEN_MIN + 50);
        assert!(
            !t.id_due(TEN_MIN + 50),
            "and is cleared once it actually goes"
        );
    }

    #[test]
    fn a_hardware_fault_still_marks_so_a_faulted_rig_is_not_hammered() {
        let mut t = timer();
        t.note_tx(0);
        assert!(t.id_due(TEN_MIN));

        mark_unless_busy(&mut t, Err(KeyedTxError::Assert), TEN_MIN);
        assert!(
            !t.id_due(TEN_MIN),
            "an assert FAULT must still advance the timer — deferring on it would retry a broken \
             rig every 50 ms tick for the whole 180 s watchdog window"
        );
    }

    #[test]
    fn a_transmit_error_also_marks() {
        let mut t = timer();
        t.note_tx(0);
        mark_unless_busy(
            &mut t,
            Err(KeyedTxError::Transmit(
                openpulse_core::error::ModemError::Frame("probe".into()),
            )),
            TEN_MIN,
        );
        assert!(!t.id_due(TEN_MIN), "a modem fault is not a busy rig either");
    }
}
