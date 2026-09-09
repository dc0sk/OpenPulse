//! Structured TOML configuration for OpenPulse TNC binaries.
//!
//! Reads `~/.config/openpulse/config.toml` and returns an [`OpenpulseConfig`]
//! with built-in defaults applied for any missing fields.
//!
//! Precedence: CLI flag overrides > config file > built-in defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod logging;
pub mod secret_file;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("identity key file has wrong length (expected 32 bytes)")]
    IdentityKeyLength,
    #[error(
        "insecure permissions on secret file {path}: {mode:o} (expected owner-only, e.g. 600)"
    )]
    InsecureSecretPermissions { path: String, mode: u32 },
}

/// Top-level configuration.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct OpenpulseConfig {
    pub station: StationConfig,
    pub audio: AudioConfig,
    pub modem: ModemConfig,
    pub radio: RadioConfig,
    pub repeater: RepeaterConfig,
    pub ardop: ArdopConfig,
    pub kiss: KissConfig,
    pub logging: LoggingConfig,
    pub relay: RelayConfig,
    pub trust: TrustConfig,
    pub mesh: MeshConfig,
    pub qsy: QsyConfig,
    pub daemon: DaemonConfig,
    pub logbook: LogbookConfig,
    pub observability: ObservabilityConfig,
    pub control_security: ControlSecurityConfig,
    pub compression: CompressionConfig,
    pub file_transfer: FileTransferConfig,
    pub discovery: DiscoveryConfig,
    pub monitor: MonitorConfig,
}

/// Simultaneous multi-mode receive (REQ-RX-01): decode extra modes from the capture stream for a
/// monitor/discovery role, independent of the active RX session's single mode.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct MonitorConfig {
    /// Master switch. Default `false`.
    pub enabled: bool,
    /// Modes to decode concurrently (e.g. `["BPSK250", "QPSK500"]`). Empty disables the monitor.
    pub modes: Vec<String>,
}

/// JS8-based station discovery (FF-15; opt-in, RX-only by default). See
/// `docs/dev/design/js8-discovery-rendezvous-plan.md`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DiscoveryConfig {
    /// Master switch. Default `false`.
    pub enabled: bool,
    /// `"rx_only"` (default, no TX) | `"beacon"` (periodic `@HB` heartbeats + `@OPULSE` hints) |
    /// `"full"` (beacon + future directed queries). Beacon/full transmit only with a configured
    /// callsign and the clock within ±2 s of UTC; the operator is responsible for §97.221
    /// automatic-control compliance (see `docs/regulatory.md`).
    pub mode: String,
    /// JS8 submode for the calling channel (MVP: `"normal"` only).
    pub submode: String,
    /// Seconds the idle predicate must hold before auto-QSY to the JS8 frequency.
    pub idle_grace_secs: u64,
    /// Maximum dwell before returning to the home frequency (`0` = until preempted).
    pub dwell_secs: u64,
    /// Heartbeat every N slots (`N × 15 s` for NORMAL). TX modes only.
    pub heartbeat_interval_slots: u32,
    /// Send the `@OPULSE` hint every Nth beacon. TX modes only.
    pub hint_interval_beacons: u32,
    /// Actively query newly-heard stations with `INFO?` (`mode = "full"` only).
    pub query_new_stations: bool,
    /// Global query budget per 10 minutes.
    pub max_queries_per_10min: u32,
    /// Seconds a heard station is retained in the table before a TTL sweep.
    pub station_ttl_secs: u64,
    /// Refuse TX when the estimated `|UTC offset|` exceeds this many ms (RX-only degrade).
    pub max_clock_skew_ms: u64,
    /// RESERVED — accepted but not yet wired. The `@OPULSE` group is currently baked into the beacon
    /// frame packing (validated against JS8Call ground-truth vectors) and the RX hint filter, so
    /// setting this has no effect. Making it configurable is a future feature (a custom group changes
    /// on-air addressing). A non-default value logs a warning at daemon startup.
    pub group: String,
    /// JS8 calling frequency (Hz) per band label (`"20m"` → 14 078 000). Dwell uses the entry for the
    /// current home band.
    pub calling_freqs_hz: BTreeMap<String, u64>,
    /// Rendezvous working-channel table (Hz) per band label. A rendezvous `Propose` carries **indices**
    /// into the current band's list (position `0` = first entry), so both stations resolve the same Hz
    /// without spelling out frequencies on-air. The daemon validates each entry against the bandplan at
    /// startup. See `docs/dev/design/js8-discovery-rendezvous-plan.md` §5.3.
    pub rendezvous_channels_hz: BTreeMap<String, Vec<u64>>,
}

impl DiscoveryConfig {
    /// Resolve a rendezvous channel **index** for `band` to its Hz frequency, or `None` if the band is
    /// absent or the index is out of range.
    pub fn rendezvous_channel_hz(&self, band: &str, index: u8) -> Option<u64> {
        self.rendezvous_channels_hz
            .get(band)
            .and_then(|chans| chans.get(index as usize))
            .copied()
    }
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        let calling_freqs_hz = [
            ("160m", 1_842_000),
            ("80m", 3_578_000),
            ("40m", 7_078_000),
            ("30m", 10_130_000),
            ("20m", 14_078_000),
            ("17m", 18_104_000),
            ("15m", 21_078_000),
            ("12m", 24_922_000),
            ("10m", 28_078_000),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        // Working channels for the post-rendezvous QSY, in each band's data/ARQ segment and clear of
        // the JS8 calling frequency. Operators tune these to local usage; the bandplan gate warns on
        // anything questionable at startup.
        let rendezvous_channels_hz = [
            ("160m", vec![1_843_000, 1_844_000, 1_845_000]),
            ("80m", vec![3_583_000, 3_585_000, 3_587_000]),
            ("40m", vec![7_101_000, 7_103_000, 7_105_000]),
            ("30m", vec![10_142_000, 10_143_000, 10_144_000]),
            ("20m", vec![14_101_000, 14_103_000, 14_105_000]),
            ("17m", vec![18_106_000, 18_107_000, 18_108_000]),
            ("15m", vec![21_101_000, 21_103_000, 21_105_000]),
            ("12m", vec![24_926_000, 24_927_000, 24_928_000]),
            ("10m", vec![28_121_000, 28_123_000, 28_125_000]),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        Self {
            enabled: false,
            mode: "rx_only".into(),
            submode: "normal".into(),
            idle_grace_secs: 120,
            dwell_secs: 900,
            heartbeat_interval_slots: 8,
            hint_interval_beacons: 3,
            query_new_stations: false,
            max_queries_per_10min: 2,
            station_ttl_secs: 3600,
            max_clock_skew_ms: 2000,
            group: "OPULSE".into(),
            calling_freqs_hz,
            rendezvous_channels_hz,
        }
    }
}

/// Control-channel link security (REQ-CTL-01/02). Auth is always required on a non-loopback
/// daemon bind; `require_auth` forces it on loopback too.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ControlSecurityConfig {
    /// Force PSK auth + encryption even on a loopback bind. On a non-loopback bind auth is
    /// mandatory regardless (see `openpulse_linksec::auth_required`).
    pub require_auth: bool,
    /// Keystore key id intended to hold the 32-byte control-channel PSK.
    ///
    /// **Currently inert (#1234)**: nothing reads it, and `openpulse-keystore` has no consumer.
    /// The daemon warns at startup if it is set to a non-default value; the PSK comes from
    /// `OPENPULSE_CONTROL_PSK`.
    pub psk_key_id: String,
}

impl Default for ControlSecurityConfig {
    fn default() -> Self {
        Self {
            require_auth: false,
            psk_key_id: "control-psk".into(),
        }
    }
}

/// Observability / audit-mode settings (REQ-OBS-01). Opt-in; off by default.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ObservabilityConfig {
    /// Enable audit mode: the daemon records its control-event stream (and, in later
    /// slices, per-session diagnostics + a startup snapshot) under `archive_dir`.
    pub audit_mode: bool,
    /// Directory for audit artifacts (`events.ndjson`, …). `~` is expanded.
    pub archive_dir: String,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            audit_mode: false,
            archive_dir: "~/.local/share/openpulse/audit".into(),
        }
    }
}

/// End-to-end session compression settings (opt-in; changes the on-air payload).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct CompressionConfig {
    /// Compress OTA session data payloads before transmission (self-describing LZ4/zstd frame).
    /// Default `false`. The receive side always decompresses a packed frame regardless of this flag,
    /// so it is safe to enable on one station independently.
    pub enabled: bool,
}

/// Direct P2P file-transfer settings (opt-in; on-air TX). See `docs/dev/design/file-transfer-plan.md`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct FileTransferConfig {
    /// Master switch. When `false`, inbound offers are rejected on air with `feature-disabled`.
    pub enabled: bool,
    /// Directory received files are written under (per-peer subdirectories are created beneath it).
    pub download_dir: String,
    /// Auto-accept inbound offers at or below this many bytes; `0` = always ask the operator.
    pub auto_accept_max_bytes: u64,
    /// Hard per-file cap in bytes, both directions (offers above it are refused).
    pub max_file_bytes: u64,
    /// Total bytes retained per peer under `download_dir` before new offers are rejected (0 = no limit).
    pub per_peer_quota_bytes: u64,
    /// Require a signature-verified peer (CONREQ/CONACK) before accepting offers.
    pub require_verified_peer: bool,
    /// Optional callsign allowlist; empty = any peer passing the trust policy above.
    pub allowed_peers: Vec<String>,
    /// Seconds an unanswered offer stays pending before automatic rejection.
    pub offer_timeout_secs: u64,
    /// Hours a partially-received transfer's blocks are kept on disk for resume before being purged
    /// (`0` = keep indefinitely). Partials live under `download_dir/<peer>/.partial/<sha256>/`.
    pub partial_ttl_hours: u64,
    /// Maximum estimated on-air seconds per keyed TX burst. Queued fragments are split into bursts no
    /// longer than this so a large transfer never holds PTT past the radio's watchdog and yields the
    /// channel between bursts. Keep it well under any PTT time-out (typically 180 s).
    pub burst_max_secs: f64,
}

impl Default for FileTransferConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            download_dir: "~/.local/share/openpulse/downloads".into(),
            auto_accept_max_bytes: 0,
            max_file_bytes: 1024 * 1024,
            per_peer_quota_bytes: 0,
            require_verified_peer: true,
            allowed_peers: Vec::new(),
            offer_timeout_secs: 120,
            partial_ttl_hours: 72,
            burst_max_secs: 20.0,
        }
    }
}

/// Automatic ADIF logbook settings (opt-in; one record per completed contact).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct LogbookConfig {
    /// Append an ADIF record per QSO (a connect→disconnect session). Default `false`.
    pub enabled: bool,
    /// Path to the `.adi` logbook file (created with a header on first write).
    pub adif_path: String,
    /// Optional callsign → Maidenhead grid lookup, used to fill the worked station's `GRIDSQUARE`
    /// when the peer's grid isn't exchanged on air. Keys are matched case-insensitively.
    pub peer_grids: std::collections::BTreeMap<String, String>,
}

impl Default for LogbookConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            adif_path: "~/.local/share/openpulse/openpulse.adi".into(),
            peer_grids: std::collections::BTreeMap::new(),
        }
    }
}

/// `openpulse-daemon` runtime settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Bind address for the TCP control port.
    pub tcp_bind_addr: String,
    /// TCP control port (default 9000).
    pub tcp_port: u16,
    /// Bind address for the WebSocket control port.
    pub websocket_bind_addr: String,
    /// WebSocket control port (default 9001).
    pub websocket_port: u16,
    /// Modem-engine receive ticker interval (ms). Lower = more responsive QSY, higher CPU.
    pub receive_tick_ms: u64,
}

/// QSY frequency-agility settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct QsyConfig {
    /// When false, all incoming QSY_REQ frames are rejected.
    pub enabled: bool,
    /// Trust levels whose QSY_REQ frames are accepted.
    /// Accepted values: "rejected", "low", "unverified", "reduced", "psk_verified", "verified"
    /// (kebab-case variants are also accepted).
    pub allow_trustlevels: Vec<String>,
    /// Bandplan mode for QSY and operating-mode guardrails.
    pub bandplan_mode: String,
    /// Enable bandplan awareness checks before selecting QSY frequencies.
    pub bandplan_awareness_enabled: bool,
    /// Enforce per-segment maximum occupied channel width.
    pub enforce_max_channel_width: bool,
    /// Enforce convention-bound digital/data segments.
    pub enforce_segment_conventions: bool,
    /// Candidate frequencies to scan during QSY negotiation (Hz).
    pub candidate_freqs_hz: Vec<u64>,
    /// Time to dwell on each candidate frequency while reading the S-meter (ms).
    pub scan_dwell_ms: u64,
    /// Seconds after QSY_ACK before both stations switch frequency.
    pub switchover_offset_s: u64,
    /// Allow invoking the rig's integrated tuner when SWR is high.
    pub allow_integrated_tuner_on_high_swr: bool,
    /// Auto-initiate a QSY when the receiver notch confirms a persistent **in-band** interferer
    /// (one a notch can't remove). Requires `[modem] notch_enabled` + `notch_persistence > 0` and
    /// `candidate_freqs_hz`. Default false.
    pub auto_qsy_on_interference: bool,
}

/// Station identity.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct StationConfig {
    pub callsign: String,
    pub grid_square: String,
    /// Path to the 32-byte Ed25519 identity seed used to sign handshake (CONREQ/CONACK) frames.
    /// Empty = the platform default (`~/.config/openpulse/identity.key`); set an explicit path to
    /// give co-located stations (e.g. the twin-station rig) distinct identities.
    pub identity_key_path: String,
    /// Periodic station-ID interval in seconds (REQ-REG-10). While transmitting, the daemon keys up
    /// and sends the callsign at least this often. `600` = every 10 minutes (US Part 97 default);
    /// `0` disables auto-ID entirely (operator IDs manually). A pure-receive station never keys up.
    pub auto_id_interval_secs: u64,
    /// End-of-exchange (sign-off) ID: seconds of TX quiet after the station has transmitted before a
    /// final ID is sent (REQ-REG-10 "at the end of a communication"). `10` = ID ~10 s after the last
    /// transmission of an exchange; `0` disables the sign-off ID (interval ID only). Only active when
    /// `auto_id_interval_secs > 0`.
    pub auto_id_signoff_idle_secs: u64,
    /// Transmitter output power in watts, recorded in the §97 regulatory TX-metadata log. The modem
    /// cannot measure PA output, so this is the operator-declared value; `0.0` = unspecified.
    pub tx_power_watts: f32,
    /// Where the §97 TX-metadata record is appended, one JSON object per transmitted frame. `~` is
    /// expanded. Empty disables the on-disk record entirely.
    ///
    /// Deliberately NOT under `[observability]`: that section is opt-in audit tooling and defaults
    /// off, whereas a regulatory record that only exists when someone remembered to enable it is
    /// not a regulatory record. The in-memory log is a bounded window (#1110), so this file is the
    /// only copy that survives a restart.
    pub tx_log_path: String,
}

/// Audio backend selection.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Audio backend: `cpal` (real hardware), `loopback` (testing), or
    /// `default` (use cpal if compiled in, loopback otherwise).
    pub backend: String,
    /// Audio device selector (cpal backend). Empty = the system default device.
    /// Resolved hotplug-safely: an exact name first, then the stable ALSA `CARD=`
    /// token, then a case-insensitive substring — so a device that the OS renames or
    /// reorders (e.g. gains a `(2)` suffix) still resolves. Pin a specific full-duplex
    /// device here to target an `snd-aloop` PCM — the real-audio twin-station rig sets
    /// station A and B to crossed PCMs.
    pub device: String,
    /// Soft-limiter threshold applied to TX audio before the output backend.
    /// Each sample `s` becomes `threshold * tanh(s / threshold)`.
    /// Set to `0.0` (default) to disable. Typical value: `1.5 * RMS`.
    pub tx_limiter_threshold: f32,
}

/// Modem defaults shared by all TNC binaries.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ModemConfig {
    /// Default modulation mode (e.g. `"BPSK250"`).
    pub mode: String,
    /// Adaptive session profile (e.g. `"hpx_hf"`, `"hpx_ofdm_hf"`).
    ///
    /// Selects the SpeedLevel→mode ladder the rate controller and mode advisor use.
    /// See `SessionProfile::PROFILE_NAMES` in `openpulse-core`.
    pub profile: String,
    /// PTT backend: `none`, `rts`, `dtr`, `vox`, `rigctld`, `cm108`, or `gpio`.
    ///
    /// `gpio` was missing from this list until #1258: #876 added the backend and touched 15 files
    /// without touching this one. `rts`/`dtr` need `--features serial` and `gpio` needs
    /// `--features gpio` in the binary being built.
    pub ptt_backend: String,
    /// PTT device path for device-based backends. For `cm108`, a `/dev/hidrawN` path (empty =
    /// auto-detect the first CM108-family device); for `rts`/`dtr`, the serial port path; for
    /// `gpio`, a `chip:line` spec (e.g. `gpiochip0:17`).
    pub ptt_device: String,
    /// CM108 GPIO pin driving PTT (1..=8); GPIO 3 is the near-universal default.
    pub ptt_gpio: u8,
    /// Receiver-led OTA adaptive rate-stepping. When `true`, the daemon starts an
    /// OTA session at launch and drives it on the RX path.
    pub ota_enabled: bool,
    /// OTA session profile; falls back to [`profile`](Self::profile) when empty.
    pub ota_profile: String,
    /// Lowest OTA speed level (e.g. `"SL3"`); empty = the profile's natural floor.
    pub ota_min_level: String,
    /// Highest OTA speed level (e.g. `"SL10"`); empty = the profile's natural cap.
    pub ota_max_level: String,
    /// Lock OTA to a fixed speed level (e.g. `"SL6"`); empty = adapt normally.
    pub ota_lock_level: String,
    /// A2 gate: minimum queued TX bytes before an upgrade is acted on (0 = off).
    pub ota_min_backlog: usize,
    /// A3 gate: suppress this many upgrades after a downgrade (0 = off).
    pub ota_upgrade_hold_frames: u32,
    /// Aggressiveness preset (`conservative`/`balanced`/`aggressive`) that sets the
    /// A2/A3 gates together; empty = use the individual `ota_min_backlog` /
    /// `ota_upgrade_hold_frames` values above. The preset, when set, takes precedence.
    pub ota_aggressiveness: String,
    /// Default DCD/squelch RMS threshold (carrier-present level for channel-busy
    /// detection, CSMA, and burst-capture flush). Raise it above a band's noise
    /// floor. Applied at startup and as the fallback when no per-band value matches.
    pub dcd_squelch: f32,
    /// Per-band DCD/squelch override, keyed by band label (`"20m"`, `"2m"`, …).
    /// When the rig tunes into a listed band, that threshold is applied; otherwise
    /// `dcd_squelch` is used. Empty (default) = always use `dcd_squelch`.
    pub dcd_squelch_bands: std::collections::BTreeMap<String, f32>,
    /// CE-SSB TX envelope conditioning (raises average power at a fixed peak on
    /// high-PAPR multicarrier modes). Default `true`; it acts only on QPSK-subcarrier OFDM
    /// (`OFDM16`/`OFDM52`) — a no-op for single-carrier modes, dense OFDM-HOM, and all SC-FDMA,
    /// whose tighter decision regions the clip's EVM breaks. See `ModemEngine::cessb_benefits`.
    pub cessb_enabled: bool,
    /// Receiver-side automatic notch: removes out-of-band CW interference (QRM) before demod.
    /// **Default `true` since 2026-08-01, on measurement.** The protected band tracks the active
    /// mode so the signal is never notched; an in-band interferer still can't be removed (that is a
    /// QSY case).
    ///
    /// It was opt-in and therefore off in every recorded on-air failure — built and never enabled,
    /// which is why "we already harden against interference" and "the station could not decode" were
    /// both true at once. Measured on the recorded hot floor with a 2200 Hz interferer outside
    /// BPSK250's occupied band, the decode **fails with the notch off and succeeds with it on** —
    /// but not at every level, and the levels are deliberately not quoted here: re-derived
    /// 2026-08-22, the rescue set is **not an interval**, because a louder tone is easier for the
    /// detector to see and harder for the demodulator to survive at the same time. REQ-QRM-01 in
    /// `docs/dev/requirements.md` records the measured amplitudes — the one place they live;
    /// `notch_rescues_interferer` is the gate and its `probe_band_sweep` the instrument to re-run.
    ///
    /// The band-tracking sentence above is now true; it was NOT true for the whole life of this
    /// default. `receive_with_timeout_fec_inner` did not record the receive mode, so the protected
    /// band fell back to 2000 Hz — 4x BPSK250's real width — and any interferer between 500 and
    /// 2500 Hz was structurally un-notchable. That path never enabled the notch, so no shipping
    /// station was affected, but the evidence recorded for this default was measured through it.
    pub notch_enabled: bool,
    /// Max simultaneous notches.
    pub notch_max: usize,
    /// Notch sharpness (BW ≈ f0 / q).
    pub notch_q: f32,
    /// Notch persistence: a tone must appear in this many signal-absent (silence) blocks before
    /// it counts as a confirmed external interferer. `0` (default) disables persistence tracking.
    /// When on, the notch nulls confirmed externals robustly and logs in-band ones as QSY hints.
    pub notch_persistence: u32,
    /// Receiver-side streaming AGC (input-level normalisation before demod). Default `false`. Decode is
    /// already amplitude-ratio-based (level-invariant above the DCD squelch), so the AGC's value is
    /// stabilising the level through deep QSB and providing a metering readout — not decode rescue. It is
    /// active-span gated (adapts only on carrier-present blocks) and gain-locked per burst.
    pub agc_enabled: bool,
    /// AGC target output RMS (0..1). Default `0.3`.
    pub agc_target_rms: f32,
    /// AGC adaptation rate α in (0, 1]; smaller = slower/steadier. Default `0.02`.
    pub agc_bandwidth: f32,
    /// AGC symmetric gain clamp in dB. Default `40.0`.
    pub agc_max_gain_db: f32,
}

/// Per-rig CAT settings (used in `[radio.rig_a]` / `[radio.rig_b]` sections).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct RigConfig {
    /// rigctld TCP address for this rig (default `"127.0.0.1:4532"`).
    ///
    /// Under `[radio.rig_b]` it drives the cross-band repeater's TX PTT. It must NOT equal the
    /// top-level `[radio] rigctld_addr` when the main rig reaches that rigctld for CAT or PTT — the
    /// default value collides with it, so an empty `[radio.rig_b]` section is itself the collision,
    /// and the daemon refuses to start an enabled repeater on one (#1260).
    pub rigctld_addr: String,
    /// Per-rig PTT/CAT backend selector. Read for `[radio.rig_b]` since #1260 (it selects the
    /// repeater's TX PTT backend through the shared `ptt_builder`); for `[radio.rig_a]` the daemon
    /// still reads the *top-level* `[radio] cat_backend`.
    pub backend: String,
    /// Serial port for this rig's backend. Read for `[radio.rig_b]` since #1260 (the `rts`/`dtr`/
    /// `cm108`/`gpio` device path for the repeater's TX PTT; note there is no per-rig GPIO pin, so
    /// `backend = "gpio"` on rig_b means pin 0). For `[radio.rig_a]` the active generic-backend port
    /// is still the *top-level* `[radio] serial_port`.
    pub serial_port: String,
    /// **Reserved (multi-rig).** The active generic-backend rig file is the *top-level*
    /// `[radio] rig_file`; this per-rig copy is unread until the multi-rig refactor.
    pub rig_file: String,
}

/// Rig CAT control settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct RadioConfig {
    /// CAT (frequency/mode) backend: `"rigctld"` (default), `"generic"`, or `"none"`.
    ///
    /// `"none"` runs with no CAT control at all — no connection is attempted — for a TRX that
    /// Hamlib/rigctld does not support. `"generic"` drives a TOML-scripted serial rig (the
    /// `serial_port` and `rig_file` below); it requires the daemon to be built with the
    /// `generic-serial` feature and is Unix-only. The operator sets frequency manually if CAT is
    /// unavailable; PTT still works via the `[modem] ptt_backend` selection (`vox`/`rts`/`dtr`).
    pub cat_backend: String,
    /// rigctld TCP address for single-rig PTT-only use (default `"127.0.0.1:4532"`).
    pub rigctld_addr: String,
    /// Serial device for the `"generic"` CAT backend (e.g. `/dev/ttyUSB0`). Unused for rigctld/none.
    pub serial_port: String,
    /// Rig-definition TOML for the `"generic"` CAT backend (command templates + serial params).
    /// See `docs/config/rig-*.toml`. Unused for rigctld/none.
    pub rig_file: String,
    /// Rig meter (ALC/power-out/SWR) poll interval in milliseconds, emitted as
    /// `RigStatus` events for live operator drive-tuning. `0` disables polling.
    /// Default `500` (2 Hz). Uses a dedicated rigctld connection, so it never
    /// contends with PTT/frequency commands. Raise it for rigs with slow CAT.
    pub meter_poll_ms: u64,
    /// **Currently unused.** The primary rig is configured via the top-level `[radio] rigctld_addr`
    /// above (what the daemon's CAT/PTT and the repeater's RX side actually read); `[radio.rig_a]`
    /// is never consumed. Kept for the planned multi-rig refactor — see roadmap "Config/feature gaps".
    pub rig_a: RigConfig,
    /// Secondary rig (TX for cross-band relay).  `None` if not configured.
    pub rig_b: Option<RigConfig>,
}

/// Cross-band repeater settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct RepeaterConfig {
    /// Start the cross-band repeater at daemon startup. Requires `[radio.rig_b]`; the daemon
    /// refuses to start when this is set and rig_b is missing or unusable.
    ///
    /// `true` means RUNNING, not merely "available to enable" — the same contract the JS8 discovery
    /// beacon has. `DisableRepeater` / `EnableRepeater` are a runtime switch on top. (`[radio.rig_a]`
    /// is NOT required despite older wording: it is unread; the primary rig is the top-level
    /// `[radio]` section.)
    pub enabled: bool,
    /// Modulation mode used for both RX (rig_a) and TX (rig_b).
    pub mode: String,
    /// Milliseconds to hold PTT after the last byte is transmitted (half-duplex only).
    pub tx_hang_ms: u64,
    /// When true, PTT is held for the entire relay session.  `tx_hang_ms` is ignored.
    pub full_duplex: bool,
    /// Audio device for rig_b, the repeater's TRANSMIT rig. Empty = the OS default, which on a
    /// multi-card host is whatever card the OS happens to prefer — the #1311 defect, and a
    /// cross-band repeater is by definition a two-card station, so leaving this empty is very
    /// likely wrong.
    ///
    /// It names the device for BOTH directions: `ModemEngine::set_default_device` is consulted by
    /// `open_output` and `open_input` alike, so this is also where rig_b's receive audio comes from
    /// when carrier sense lands (#1325). It must not equal `[audio] device` — that would put two
    /// capture streams on one device, which is #1007's rule.
    pub tx_device: String,
    /// Carrier-sense rig_b's band before keying it (#1325). Default `true`.
    ///
    /// A cross-band relay is an unattended §97.221 station transmitting on a band it never listens
    /// to, so without this it doubles with whatever QSO is already there, repeatedly, for as long
    /// as traffic keeps arriving on its input. Turn it off only for an assignment where this
    /// station is entitled to transmit regardless; the daemon refuses to start an enabled repeater
    /// that can neither sense its output band nor has been told not to.
    pub carrier_sense: bool,
}

/// ARDOP TNC service settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ArdopConfig {
    pub bind_addr: String,
    pub cmd_port: u16,
    pub data_port: u16,
    /// Opt-in: run an adaptive ARQ session so the rate ladder, ARQBW, and ARQTIMEOUT take effect.
    /// Default false (fixed-mode operation, the historical behaviour).
    pub enable_adaptive_arq: bool,
    /// Session profile name for the adaptive ARQ ladder (e.g. `hpx500`, `hpx_hf`). Empty falls
    /// back to `hpx500`.
    pub adaptive_profile: String,
}

/// KISS TNC service settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct KissConfig {
    pub bind_addr: String,
    pub port: u16,
}

/// Logging configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// `tracing` level filter: `error`, `warn`, `info`, `debug`, or `trace`.
    pub level: String,
    /// Optional log file path (REQ-OBS-02). When set, logs are also appended to a
    /// daily-rolled file next to this path, in addition to stdout. `~` is expanded.
    #[serde(default)]
    pub file: Option<String>,
}

/// Multi-hop relay settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct RelayConfig {
    pub enabled: bool,
    pub max_hops: u8,
    /// Store-and-forward frame TTL in seconds. Read by `openpulse-daemon`'s relay forwarder.
    pub store_forward_ttl_s: u64,
    /// Peer IDs (lower-hex, 64 chars each) whose frames are dropped at the first relay hop.
    pub deny_list: Vec<String>,
    /// Optional allow-list of originator peer IDs (lower-hex, 64 chars each). When non-empty, the relay
    /// forwards *only* frames from these originators (in addition to the deny-list) — scoping a relay to
    /// a known set of stations. Empty = no restriction. Note: the originator id is not cryptographically
    /// authenticated at the relay, so this is a defense-in-depth control, not strong authentication.
    #[serde(default)]
    pub allow_list: Vec<String>,
}

/// Trust store settings.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct TrustConfig {
    /// Path to the local trust store. Empty string uses the platform default.
    pub store_path: String,
}

/// Mesh daemon settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct MeshConfig {
    /// Enable the mesh relay daemon.
    pub enabled: bool,
    /// Maximum relay hop count (envelope dropped when hop_index reaches this).
    pub max_hops: u8,
    /// Relay trust policy string: `"strict"`, `"balanced"`, or `"permissive"`.
    /// Reserved for future trust-level filtering; `RelayTrustPolicy` currently
    /// models a deny-list of peer IDs and does not yet enforce this value.
    pub relay_policy: String,
    /// Store-and-forward frame TTL in seconds. Read by `openpulse-mesh` daemon.
    pub store_forward_ttl_s: u64,
    /// Peer discovery beacon interval in seconds; 0 disables beacons.
    pub beacon_interval_s: u64,
    /// Maximum entries in the local peer cache.
    pub peer_cache_capacity: usize,
    /// Peer cache entry TTL in seconds.
    pub peer_cache_ttl_s: u64,
}

// ── Defaults ──────────────────────────────────────────────────────────────────

impl Default for StationConfig {
    fn default() -> Self {
        Self {
            callsign: "N0CALL".into(),
            grid_square: "AA00".into(),
            identity_key_path: String::new(),
            auto_id_interval_secs: 600,
            auto_id_signoff_idle_secs: 10,
            tx_power_watts: 0.0,
            tx_log_path: "~/.local/share/openpulse/tx-log.ndjson".into(),
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            backend: "default".into(),
            device: String::new(),
            tx_limiter_threshold: 0.0,
        }
    }
}

impl Default for ModemConfig {
    fn default() -> Self {
        Self {
            mode: "BPSK250".into(),
            profile: "hpx_hf".into(),
            ptt_backend: "none".into(),
            ptt_device: String::new(),
            ptt_gpio: 3,
            ota_enabled: false,
            ota_profile: String::new(),
            ota_min_level: String::new(),
            ota_max_level: String::new(),
            ota_lock_level: String::new(),
            ota_min_backlog: 0,
            ota_upgrade_hold_frames: 0,
            ota_aggressiveness: String::new(),
            dcd_squelch: 0.01, // matches the engine's built-in DcdState default
            dcd_squelch_bands: std::collections::BTreeMap::new(),
            cessb_enabled: true,
            notch_enabled: true,
            notch_max: 10,
            notch_q: 25.0,
            notch_persistence: 0,
            agc_enabled: false,
            agc_target_rms: 0.3,
            agc_bandwidth: 0.02,
            agc_max_gain_db: 40.0,
        }
    }
}

impl Default for RigConfig {
    fn default() -> Self {
        Self {
            rigctld_addr: "127.0.0.1:4532".into(),
            backend: "rigctld".into(),
            serial_port: String::new(),
            rig_file: String::new(),
        }
    }
}

impl Default for RadioConfig {
    fn default() -> Self {
        Self {
            cat_backend: "rigctld".into(),
            rigctld_addr: "127.0.0.1:4532".into(),
            serial_port: String::new(),
            rig_file: String::new(),
            meter_poll_ms: 500,
            rig_a: RigConfig::default(),
            rig_b: None,
        }
    }
}

impl Default for RepeaterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: "BPSK250".into(),
            tx_hang_ms: 500,
            full_duplex: false,
            tx_device: String::new(),
            carrier_sense: true,
        }
    }
}

impl Default for ArdopConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1".into(),
            cmd_port: 8515,
            data_port: 8516,
            enable_adaptive_arq: false,
            adaptive_profile: "hpx500".into(),
        }
    }
}

impl Default for KissConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1".into(),
            port: 8100,
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            file: None,
        }
    }
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_hops: 3,
            store_forward_ttl_s: 300,
            deny_list: Vec::new(),
            allow_list: Vec::new(),
        }
    }
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_hops: 3,
            relay_policy: "balanced".into(),
            store_forward_ttl_s: 300,
            beacon_interval_s: 60,
            peer_cache_capacity: 256,
            peer_cache_ttl_s: 3600,
        }
    }
}

impl Default for QsyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            // "reduced" included since #1252 (maintainer decision). Without it inbound QSY was
            // DEAD rather than gated: the over-air path tops out at `Reduced`
            // (`classify_connection_trust(.., OverAir, false)`), while `Verified` needs an
            // out-of-band certificate and `PskVerified` needs `psk_validated`, which the RF path
            // never sets — so a station that enabled QSY and left this list rejected every request.
            // Admitting `Reduced` is safe now that lines are signed: it means a key in the local
            // trust store, proven over air, which is exactly what the signature demonstrates.
            allow_trustlevels: vec!["verified".into(), "psk_verified".into(), "reduced".into()],
            bandplan_mode: "ham-iaru-r1".into(),
            bandplan_awareness_enabled: true,
            enforce_max_channel_width: true,
            enforce_segment_conventions: true,
            candidate_freqs_hz: vec![],
            scan_dwell_ms: 500,
            switchover_offset_s: 5,
            allow_integrated_tuner_on_high_swr: false,
            auto_qsy_on_interference: false,
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            tcp_bind_addr: "127.0.0.1".into(),
            tcp_port: 9000,
            websocket_bind_addr: "127.0.0.1".into(),
            websocket_port: 9001,
            receive_tick_ms: 50,
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the platform-standard config file path.
///
/// On Linux: `~/.config/openpulse/config.toml`
/// On macOS: `~/Library/Application Support/openpulse/config.toml`
/// On Windows: `%APPDATA%\openpulse\config.toml`
pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("openpulse").join("config.toml"))
}

/// Load config from the platform-standard path. Returns `OpenpulseConfig::default()`
/// if the file does not exist.
pub fn load() -> Result<OpenpulseConfig, ConfigError> {
    match default_config_path() {
        Some(path) => load_from(&path),
        None => Ok(OpenpulseConfig::default()),
    }
}

/// Load config from `path`. Returns `OpenpulseConfig::default()` if the file does
/// not exist.
pub fn load_from(path: &Path) -> Result<OpenpulseConfig, ConfigError> {
    if !path.exists() {
        return Ok(OpenpulseConfig::default());
    }
    let content = std::fs::read_to_string(path)?;
    let config: OpenpulseConfig = toml::from_str(&content)?;
    Ok(config)
}

/// Load or generate the node's 32-byte Ed25519 signing key seed.
///
/// Reads `identity.key` from the platform config directory (`~/.config/openpulse/`).
/// If absent, generates a fresh random seed, persists it, then returns it.
/// The caller derives `peer_id = SigningKey::from_bytes(&seed).verifying_key().to_bytes()`.
pub fn load_or_generate_identity() -> Result<[u8; 32], ConfigError> {
    let path = default_identity_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "cannot determine config directory",
        )
    })?;
    load_identity_from(&path)
}

/// Load or generate an identity seed at an explicit path (useful in tests).
pub fn load_identity_from(path: &Path) -> Result<[u8; 32], ConfigError> {
    if path.exists() {
        // Refuse a group/world-readable identity key (REQ-CTL-05).
        secret_file::validate_owner_only(path)?;
        let bytes = std::fs::read(path)?;
        if bytes.len() != 32 {
            return Err(ConfigError::IdentityKeyLength);
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&bytes);
        return Ok(seed);
    }

    use rand::RngCore;
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Create with owner-only permissions atomically on Unix to avoid a window
    // where the file exists with broader umask-derived permissions.
    #[cfg(unix)]
    {
        use std::io::{ErrorKind, Write};
        use std::os::unix::fs::OpenOptionsExt;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(mut f) => f.write_all(&seed)?,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                // Another process won the race and already created the file.
                let bytes = std::fs::read(path)?;
                if bytes.len() != 32 {
                    return Err(ConfigError::IdentityKeyLength);
                }
                seed.copy_from_slice(&bytes);
            }
            Err(e) => return Err(e.into()),
        }
    }
    #[cfg(not(unix))]
    std::fs::write(path, seed)?;
    Ok(seed)
}
/// Returns the platform-standard identity key file path.
///
/// On Linux: `~/.config/openpulse/identity.key`
fn default_identity_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("openpulse").join("identity.key"))
}

/// Persist updated QSY settings to the platform config file.
///
/// Loads the existing config (falling back to defaults), updates the QSY
/// fields, then rewrites the file. Returns an error when the config directory
/// cannot be determined.
pub fn save_qsy_config(
    qsy_enabled: bool,
    bandplan_mode: &str,
    allow_integrated_tuner_on_high_swr: bool,
) -> Result<(), ConfigError> {
    let path = match default_config_path() {
        Some(p) => p,
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "cannot determine config directory",
            )
            .into())
        }
    };
    save_qsy_config_to_path(
        &path,
        qsy_enabled,
        bandplan_mode,
        allow_integrated_tuner_on_high_swr,
    )
}

/// Persist updated QSY settings to an explicit config path.
///
/// This is used by tests and tooling that need deterministic file locations.
pub fn save_qsy_config_to_path(
    path: &Path,
    qsy_enabled: bool,
    bandplan_mode: &str,
    allow_integrated_tuner_on_high_swr: bool,
) -> Result<(), ConfigError> {
    let mut cfg = load_from(path).unwrap_or_default();
    cfg.qsy.enabled = qsy_enabled;
    cfg.qsy.allow_integrated_tuner_on_high_swr = allow_integrated_tuner_on_high_swr;
    if bandplan_mode == "unrestricted" {
        cfg.qsy.bandplan_awareness_enabled = false;
    } else {
        cfg.qsy.bandplan_awareness_enabled = true;
        cfg.qsy.bandplan_mode = bandplan_mode.to_string();
    }
    let toml_str =
        toml::to_string_pretty(&cfg).map_err(|e| std::io::Error::other(e.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, toml_str)?;
    Ok(())
}

/// Returns a commented TOML configuration template for `openpulse config init`.
pub fn init_template() -> String {
    r#"# OpenPulse configuration file
# Generated by: openpulse config init
#
# Place this file at:
#   Linux/BSDs : ~/.config/openpulse/config.toml
#   macOS      : ~/Library/Application Support/openpulse/config.toml
#   Windows    : %APPDATA%\openpulse\config.toml
#
# All fields are optional; built-in defaults are used for any missing values.

[station]
# Your amateur radio callsign.
callsign = "N0CALL"
# Maidenhead grid square locator.
grid_square = "AA00"
# Path to the 32-byte Ed25519 identity seed used to sign handshake (CONREQ/CONACK) frames.
# Empty = the platform default (~/.config/openpulse/identity.key), generated on first run.
# Set an explicit path to give co-located stations (e.g. the twin rig) distinct identities.
identity_key_path = ""
# Periodic station-ID interval in seconds (regulatory: e.g. US Part 97 = every 10 minutes).
# While transmitting, the daemon keys up and sends your callsign at least this often.
# 0 disables auto-ID entirely (you identify manually). A pure-receive station never keys up.
auto_id_interval_secs = 600
# End-of-exchange (sign-off) ID: seconds of transmit quiet before a final ID is sent after you
# have transmitted (regulatory "identify at the end of a communication"). 10 = ID ~10 s after the
# last transmission of an exchange; 0 disables the sign-off ID (interval ID only).
auto_id_signoff_idle_secs = 10
# Transmitter output power in watts, recorded in the regulatory TX-metadata log (the modem cannot
# measure PA output — this is your declared value). 0 = unspecified.
tx_power_watts = 0.0
# Where the §97 TX record is written (one JSON object per transmitted frame).
# Empty disables the on-disk record; the in-memory log is a bounded window and is lost on restart.
tx_log_path = "~/.local/share/openpulse/tx-log.ndjson"

[audio]
# Audio backend: default | cpal | loopback
#   default  — use cpal if compiled in, loopback otherwise (recommended)
#   cpal     — always use the real sound card (error if not compiled in)
#   loopback — software loopback for testing only, no audio hardware required
backend = "default"
# Audio device selector for the cpal backend. Empty = the system default device.
# Resolved hotplug-safely (exact name → ALSA CARD= token → substring), so a device
# the OS renames/reorders still resolves. Pin a specific device (e.g. an snd-aloop
# PCM) to target a fixed full-duplex device.
device = ""
# Soft TX limiter threshold (0.0 = disabled). Typical value: 1.5 × RMS of the
# modulated signal. Prevents ADC clipping and reduces PA non-linearity on peaks.
# tx_limiter_threshold = 0.0

[modem]
# Default modulation mode used when no --mode flag is provided. The registered set spans BPSK/QPSK/8PSK
# (incl. -RRC and -HF variants), 64QAM, OFDM16/52 (incl. OFDM52-{8PSK,16QAM,32QAM,64QAM} higher-order),
# SC-FDMA (incl. SCFDMA52-{16QAM,32QAM,64QAM}), PILOT-{QPSK,8PSK,16QAM,32APSK}{500,1000,2000-RRC}, and
# FSK4-ACK — see docs/mode-fec-ladder.md for the authoritative list per band/bandwidth class.
mode = "BPSK250"
# Adaptive session profile (SpeedLevel ladder) used by the rate controller and
# `openpulse mode-advisor`. Available: hpx500, hpx_hf, hpx_ofdm_hf, hpx_wideband,
# hpx_wideband_hd, hpx_narrowband, hpx_narrowband_hd. hpx_ofdm_hf is the OFDM
# higher-order (high-throughput/high-reliability) HF ladder.
profile = "hpx_hf"
# PTT backend: none | rts | dtr | vox | rigctld | cm108
ptt_backend = "none"
# PTT device path for device-based backends. For cm108, a /dev/hidrawN path
# (empty = auto-detect the first CM108-family device); for rts/dtr, the serial
# port path. Ignored by none/vox/rigctld.
ptt_device = ""
# CM108 GPIO pin driving PTT (1..8); 3 is the near-universal default.
ptt_gpio = 3
# Receiver-led OTA adaptive rate-stepping. When true the daemon starts an OTA
# session at launch and drives it on the RX path (the data receiver leads the
# rate per direction; the sender follows an absolute recommendation in the ACK).
ota_enabled = false
# OTA session profile; empty falls back to `profile` above.
ota_profile = ""
# Clamp the OTA ladder. Empty = the profile's natural floor/cap. e.g. "SL3" / "SL10".
ota_min_level = ""
ota_max_level = ""
# Lock OTA to a fixed level (manual override); empty = adapt normally. e.g. "SL6".
ota_lock_level = ""
# A2 gate: minimum queued TX bytes before an upgrade is acted on (0 = off).
ota_min_backlog = 0
# A3 gate: suppress this many upgrade attempts after a downgrade (0 = off).
ota_upgrade_hold_frames = 0
# Aggressiveness preset: conservative | balanced | aggressive. Sets the A2/A3
# gates together (one knob instead of two). Empty = use the two values above.
# Takes precedence over ota_min_backlog / ota_upgrade_hold_frames when set.
ota_aggressiveness = ""
# DCD/squelch RMS threshold (carrier-present level for channel-busy detection,
# CSMA, and burst-capture flush). Raise above a band's noise floor if the carrier
# never appears to "drop". Applied at startup and as the per-band fallback.
dcd_squelch = 0.01
# Optional per-band squelch overrides (band label → threshold). When the rig tunes
# into a listed band the matching value is applied; otherwise dcd_squelch is used.
# [modem.dcd_squelch_bands]
# "40m" = 0.05
# "20m" = 0.02
# "2m"  = 0.01
# CE-SSB TX envelope conditioning: raises average power at a fixed peak on
# high-PAPR multicarrier modes (OFDM/SC-FDMA). No-op for single-carrier modes.
cessb_enabled = true
# Receiver-side automatic notch: removes out-of-band CW interference (QRM) before
# demod. The protected band tracks the active mode, so the signal is never notched
# (true since 2026-08-22 -- one receive path did not record the mode and protected a
# band 4x too wide); an in-band interferer can't be removed this way (that is a QSY
# case). Default ON: measured against a 2200 Hz interferer outside BPSK250's band on
# a recorded hot floor, the decode fails with it off and succeeds with it on -- but
# not at every level, and the levels are deliberately not quoted (the rescue set is
# not an interval). Set false only if you have a reason -- it costs nothing when
# there is nothing to notch.
notch_enabled = true
# Max simultaneous notches, and notch sharpness (bandwidth ~= f0 / notch_q).
notch_max = 10
notch_q = 25.0
# Persistence: a tone must appear in this many silence (signal-absent) blocks before it
# is treated as a confirmed external interferer. 0 = off. When on, externally-confirmed
# tones are notched robustly and confirmed in-band tones are logged as QSY hints.
notch_persistence = 0
# Receiver-side streaming AGC (input-level normalisation before demod). Default off.
# Decode is already level-invariant above the DCD squelch, so the AGC stabilises the
# level through deep QSB + provides a metering readout, rather than rescuing a decode.
# Active-span gated (adapts only on carrier-present blocks) and gain-locked per burst.
agc_enabled = false
agc_target_rms = 0.3
agc_bandwidth = 0.02
agc_max_gain_db = 40.0

[radio]
# CAT (frequency/mode) backend: "rigctld" (default) or "none".
# "none" runs with no CAT control — no rigctld connection is attempted — for a
# TRX that Hamlib/rigctld does not support. Set frequency manually on the radio;
# PTT still works via [modem] ptt_backend (vox/rts/dtr/cm108/gpio/rigctld). Set-freq and QSY retune
# are rejected while CAT is disabled.
cat_backend = "rigctld"
# rigctld TCP address for single-rig PTT-only use.
rigctld_addr = "127.0.0.1:4532"
# "generic" CAT backend (TOML-scripted serial; requires the `generic-serial` build feature, Unix):
serial_port = ""           # e.g. /dev/ttyUSB0
rig_file = ""              # e.g. docs/config/rig-icom-ic7300.toml
# Rig meter (ALC / power-out / SWR) poll interval in ms, surfaced as live RigStatus
# events for drive tuning. 0 disables. Uses a separate rigctld connection.
meter_poll_ms = 500

# The "generic" serial CAT backend IS wired: set cat_backend = "generic" with the top-level
# serial_port + rig_file above (build with --features generic-serial, Unix only).
# [radio.rig_a] remains UNUSED — the primary rig is the top-level [radio] config above; rig_a is
# reserved for the planned multi-rig refactor (roadmap "Config/feature gaps").

# Secondary rig: the daemon reads ONLY its rigctld_addr, for the cross-band repeater's TX PTT.
# Uncomment to enable the repeater's second rig.
# [radio.rig_b]
# rigctld_addr = "127.0.0.1:4533"

[repeater]
# Enable the cross-band repeater (RX uses the top-level [radio] rig; TX uses [radio.rig_b]).
# `true` STARTS it at daemon startup — config means running, as it does for the JS8 beacon.
enabled = false
# Audio device for rig_b, the TRANSMIT rig. Empty = the OS default, which on a two-card
# cross-band station is very likely the MAIN rig's card. Must differ from [audio] device:
# one card cannot carry two capture streams, and the daemon refuses to start on a collision.
tx_device = ""
# Listen to rig_b's band before keying it. A cross-band relay is an unattended station
# transmitting on a band it never hears, so without this it doubles with whatever QSO is
# already there. Turn off only for an assignment entitled to transmit regardless.
carrier_sense = true
# Modulation mode used for both RX (rig_a) and TX (rig_b).
mode = "BPSK250"
# Milliseconds to hold PTT after the last byte is transmitted (half-duplex only).
tx_hang_ms = 500
# Hold PTT for the entire relay session instead of per-frame assert/release.
# full_duplex = false

[ardop]
# IP address the ARDOP TNC listens on.
bind_addr = "127.0.0.1"
# ARDOP command port.
cmd_port = 8515
# ARDOP data port.
data_port = 8516
# Opt-in: run an adaptive ARQ session so the rate ladder + host ARQBW/ARQTIMEOUT take effect.
# Default false = fixed-mode operation (ARQBW/ARQTIMEOUT are accepted-and-echoed no-ops).
enable_adaptive_arq = false
# Session profile for the adaptive ladder (e.g. hpx500, hpx_hf); empty falls back to hpx500.
adaptive_profile = "hpx500"

[kiss]
# IP address the KISS TNC listens on.
bind_addr = "127.0.0.1"
# KISS TCP port.
port = 8100

[logging]
# Log verbosity: error | warn | info | debug | trace
level = "info"
# Optional persistent log file (REQ-OBS-02). When set, logs are appended to a
# daily-rolled file (<path>.YYYY-MM-DD) in addition to stdout. `~` is expanded.
# Read by openpulse-daemon. RUST_LOG still overrides `level`.
# file = "~/.local/share/openpulse/openpulse.log"

[observability]
# Audit mode (REQ-OBS-01): when enabled, openpulse-daemon records its control-event
# stream to <archive_dir>/events.ndjson for later analysis. Off by default. `~` expanded.
audit_mode = false
archive_dir = "~/.local/share/openpulse/audit"

[control_security]
# Control-channel PSK auth + encryption (REQ-CTL-01/02). Auth is ALWAYS required when the
# daemon binds to a non-loopback address; `require_auth` forces it on loopback too. When auth is
# required the daemon refuses to start without a PSK (fail closed). The 32-byte PSK is currently
# supplied via the OPENPULSE_CONTROL_PSK env var (64 hex chars); keystore-backed loading under
# `psk_key_id` is INERT today (#1234): nothing reads it, and the daemon warns at startup if
# you set it to a non-default value. Keystore-backed loading is the follow-up.
require_auth = false
psk_key_id = "control-psk"

[relay]
# Enable multi-hop relay forwarding (used by openpulse-daemon).
enabled = false
# Maximum relay hop count.
max_hops = 3
# Store-and-forward frame TTL in seconds (read by openpulse-daemon relay forwarder).
store_forward_ttl_s = 300
# Originator peer IDs (lower-hex, 64 chars) whose frames are dropped at the first relay hop.
deny_list = []
# Optional allow-list of originator peer IDs (lower-hex, 64 chars). When non-empty, the relay forwards
# ONLY frames from these originators (plus the deny-list still applies) — scoping the relay to known
# stations. Empty = no restriction. The originator id is not cryptographically authenticated at the
# relay, so this is a defense-in-depth control, not strong authentication.
allow_list = []

[mesh]
# Enable the openpulse-mesh daemon relay stack (used by openpulse-mesh binary).
enabled = false
# Maximum relay hop count before a frame is dropped.
max_hops = 3
# Relay trust policy: strict | balanced | permissive
relay_policy = "balanced"
# Store-and-forward frame TTL in seconds (read by openpulse-mesh binary).
store_forward_ttl_s = 300
# Peer discovery beacon interval in seconds.
beacon_interval_s = 60
# Maximum peer cache entries.
peer_cache_capacity = 256
# Peer cache entry TTL in seconds.
peer_cache_ttl_s = 3600

[trust]
# Path to the local trust store file. Empty = platform default.
store_path = ""

[daemon]
# TCP control port (openpulse-daemon).
tcp_bind_addr = "127.0.0.1"
tcp_port = 9000
# WebSocket control port (openpulse-daemon).
websocket_bind_addr = "127.0.0.1"
websocket_port = 9001
# Modem receive ticker interval (ms). Lower = more responsive RF reception, higher CPU.
receive_tick_ms = 50

# [qsy]
# Enable QSY frequency-agility negotiation.  Requires hamlib rigctld configured in [radio].
# enabled = false
# Trust levels allowed to initiate QSY with this station.
# allow_trustlevels = ["verified", "psk_verified", "reduced"]
# Bandplan-awareness mode: ham-iaru-r1 | ham-iaru-r2 | ham-iaru-r3
# bandplan_mode = "ham-iaru-r1"
# Enforce bandplan guardrails for QSY (enabled by default).
# Set to false only as an explicit responsible-operator compliance override.
# bandplan_awareness_enabled = true
# Enforce per-segment occupied bandwidth limits for the active modem mode.
# enforce_max_channel_width = true
# Enforce convention-bound digital/data segments.
# enforce_segment_conventions = true
# Candidate frequencies to evaluate during a QSY scan (Hz).
# candidate_freqs_hz = [14070000, 14074000, 14077000]
# How long to dwell on each candidate while reading the S-meter (ms).
# scan_dwell_ms = 500
# Seconds between QSY_ACK and the actual frequency switch.
# switchover_offset_s = 5
# Allow integrated tuner operation when high SWR is detected.
# allow_integrated_tuner_on_high_swr = false
# Auto-initiate a QSY when the receiver notch confirms a persistent in-band interferer (one a
# notch can't remove). Requires [modem] notch_enabled + notch_persistence > 0 and candidate_freqs_hz.
# auto_qsy_on_interference = false

[logbook]
# Automatic ADIF logbook: append one record per completed contact (a connect→disconnect
# session) so logs import into standard logging software / LoTW / eQSL. Opt-in.
enabled = false
# Path to the .adi file (a header is written on first record).
adif_path = "~/.local/share/openpulse/openpulse.adi"
# Optional callsign → grid lookup to fill the worked station's GRIDSQUARE (case-insensitive).
# [logbook.peer_grids]
# "DL1ABC" = "JO31aa"

[compression]
# Compress OTA session data payloads before transmission (self-describing LZ4/zstd frame).
# The receiver always decompresses a packed frame regardless of this flag, so it is safe to
# enable on one station independently. Opt-in (changes the on-air payload).
enabled = false

[file_transfer]
# Direct peer-to-peer file transfer over an RF session (offer/accept + signed-manifest verify).
# Opt-in (on-air TX). See docs/dev/design/file-transfer-plan.md.
enabled = false
# Where received files are written (per-peer subdirectories are created beneath it).
download_dir = "~/.local/share/openpulse/downloads"
# Auto-accept inbound offers at or below this many bytes; 0 = always ask the operator.
auto_accept_max_bytes = 0
# Hard per-file cap in bytes, both directions.
max_file_bytes = 1048576
# Per-peer retained-bytes quota under download_dir before new offers are rejected (0 = no limit).
per_peer_quota_bytes = 0
# Require a signature-verified peer (CONREQ/CONACK) before accepting offers.
require_verified_peer = true
# Optional callsign allowlist; empty = any peer passing the trust policy above.
allowed_peers = []
# Seconds an unanswered offer stays pending before automatic rejection.
offer_timeout_secs = 120
# Hours a partially-received transfer's blocks are kept for resume before purge (0 = keep forever).
partial_ttl_hours = 72
# Max estimated on-air seconds per keyed TX burst (splits large transfers so PTT never trips the
# radio watchdog and the channel is yielded between bursts). Keep well under any PTT time-out.
burst_max_secs = 20.0

[discovery]
# JS8-based station discovery (FF-15). Opt-in; RX-only by default (no on-air TX).
# See docs/dev/design/js8-discovery-rendezvous-plan.md.
enabled = false
# "rx_only" (default) | "beacon" (HB + hint TX) | "full" (adds queries + rendezvous responder).
mode = "rx_only"
# JS8 submode for the calling channel (MVP: normal only).
submode = "normal"
# Seconds the idle predicate must hold before auto-QSY to the JS8 frequency.
idle_grace_secs = 120
# Maximum dwell before returning to the home frequency (0 = until preempted).
dwell_secs = 900
# Heartbeat every N slots (N * 15 s for NORMAL); TX modes only.
heartbeat_interval_slots = 8
# Send the @OPULSE hint every Nth beacon; TX modes only.
hint_interval_beacons = 3
# Actively query newly-heard stations with INFO? (mode = "full" only). RESERVED: directed INFO queries
# are not yet implemented, so these two are currently accepted-but-unused (setting them has no effect).
query_new_stations = false
max_queries_per_10min = 2
# Seconds a heard station is retained before a TTL sweep.
station_ttl_secs = 3600
# Refuse TX when the estimated |UTC offset| exceeds this many ms (RX-only degrade).
max_clock_skew_ms = 2000
# JS8 custom group for the OpenPulse hint. RESERVED: the @OPULSE group is baked into the beacon frame
# packing and RX filter, so a non-default value has no effect (and logs a warning at startup).
group = "OPULSE"
# JS8 calling frequency (Hz) per band; dwell uses the entry for the current home band.
[discovery.calling_freqs_hz]
"160m" = 1842000
"80m" = 3578000
"40m" = 7078000
"30m" = 10130000
"20m" = 14078000
"17m" = 18104000
"15m" = 21078000
"12m" = 24922000
"10m" = 28078000
# Rendezvous working channels (Hz) per band, in the data/ARQ segment and clear of the JS8 calling
# frequency. A Propose carries INDICES into the current band's list (0 = first), so both stations
# resolve the same Hz without spelling it on-air. The daemon bandplan-checks these at startup.
[discovery.rendezvous_channels_hz]
"160m" = [1843000, 1844000, 1845000]
"80m" = [3583000, 3585000, 3587000]
"40m" = [7101000, 7103000, 7105000]
"30m" = [10142000, 10143000, 10144000]
"20m" = [14101000, 14103000, 14105000]
"17m" = [18106000, 18107000, 18108000]
"15m" = [21101000, 21103000, 21105000]
"12m" = [24926000, 24927000, 24928000]
"10m" = [28121000, 28123000, 28125000]

# Simultaneous multi-mode receive (REQ-RX-01). When enabled, the daemon decodes
# each listed mode from every capture burst in parallel with the active session,
# emitting a MonitorFrame event per decode — a monitor/discovery role. Off by
# default; an empty `modes` list also disables it.
[monitor]
enabled = false
modes = []
"#
    .to_string()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn unique_tmp(suffix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "openpulse_cfg_{}_{}.toml",
            std::process::id(),
            suffix
        ))
    }

    #[test]
    fn load_defaults_when_no_file() {
        let path = unique_tmp("defaults");
        let _ = std::fs::remove_file(&path);
        let cfg = load_from(&path).unwrap();
        assert_eq!(cfg.station.callsign, "N0CALL");
        assert_eq!(cfg.ardop.cmd_port, 8515);
        assert_eq!(cfg.ardop.data_port, 8516);
        assert_eq!(cfg.kiss.port, 8100);
        assert_eq!(cfg.modem.mode, "BPSK250");
        assert_eq!(cfg.logging.level, "info");
        assert!(!cfg.relay.enabled);
        assert_eq!(cfg.relay.max_hops, 3);
        // A config with no [relay] allow_list parses to an unrestricted (empty) list — backwards
        // compatible with pre-E1 configs.
        assert!(cfg.relay.allow_list.is_empty());
        // CAT defaults to rigctld for backward compatibility.
        assert_eq!(cfg.radio.cat_backend, "rigctld");
        // Rig meter polling defaults to 2 Hz (500 ms).
        assert_eq!(cfg.radio.meter_poll_ms, 500);
        // Audio device defaults to empty (system default device).
        assert_eq!(cfg.audio.backend, "default");
        assert_eq!(cfg.audio.device, "");
        // Receiver auto-notch is ON by default (REQ-QRM-01, 2026-08-01). It was opt-in and
        // therefore off in every recorded on-air failure; measured, it turns a FAIL into a decode
        // against a 2200 Hz out-of-band interferer. No amplitude is quoted: re-derived 2026-08-22,
        // the rescue set is not an interval, so the gate asserts existence + attribution rather
        // than a level. Do not flip this back without re-measuring — `notch_rescues_interferer`.
        assert!(cfg.modem.notch_enabled);
        assert_eq!(cfg.modem.notch_max, 10);
        assert_eq!(cfg.modem.notch_q, 25.0);
        assert_eq!(cfg.modem.notch_persistence, 0);
        // Auto-QSY on interference is opt-in.
        assert!(!cfg.qsy.auto_qsy_on_interference);
        // ADIF logbook is opt-in.
        assert!(!cfg.logbook.enabled);
        assert!(cfg.logbook.adif_path.ends_with(".adi"));
        assert!(cfg.logbook.peer_grids.is_empty());
    }

    #[test]
    fn dcd_squelch_per_band_parses() {
        let path = unique_tmp("dcd-squelch");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "[modem]").unwrap();
            writeln!(f, "dcd_squelch = 0.02").unwrap();
            writeln!(f, "[modem.dcd_squelch_bands]").unwrap();
            writeln!(f, r#""40m" = 0.05"#).unwrap();
            writeln!(f, r#""2m" = 0.015"#).unwrap();
        }
        let cfg = load_from(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!((cfg.modem.dcd_squelch - 0.02).abs() < 1e-6);
        assert_eq!(cfg.modem.dcd_squelch_bands.get("40m").copied(), Some(0.05));
        assert_eq!(cfg.modem.dcd_squelch_bands.get("2m").copied(), Some(0.015));
    }

    #[test]
    fn cat_backend_none_parses() {
        let path = unique_tmp("cat-none");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "[radio]").unwrap();
            writeln!(f, r#"cat_backend = "none""#).unwrap();
        }
        let cfg = load_from(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(cfg.radio.cat_backend, "none");
    }

    #[test]
    fn cli_override_pattern() {
        // CLI flag > config > default: simulate by loading config then applying
        // an Option<T> override, the pattern used by TNC binaries.
        let path = unique_tmp("override");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "[ardop]").unwrap();
            writeln!(f, "cmd_port = 9000").unwrap();
            writeln!(f, "data_port = 9001").unwrap();
        }
        let mut cfg = load_from(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(cfg.ardop.cmd_port, 9000);

        // CLI flag supplied → overrides config value.
        let cli_cmd_port: Option<u16> = Some(7777);
        if let Some(p) = cli_cmd_port {
            cfg.ardop.cmd_port = p;
        }
        assert_eq!(cfg.ardop.cmd_port, 7777);
        // Unset CLI flag → config value retained.
        assert_eq!(cfg.ardop.data_port, 9001);
    }

    #[test]
    fn missing_fields_get_defaults() {
        let path = unique_tmp("partial");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            // Only set callsign; everything else should come from Default.
            writeln!(f, "[station]").unwrap();
            writeln!(f, r#"callsign = "K1ABC""#).unwrap();
        }
        let cfg = load_from(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(cfg.station.callsign, "K1ABC");
        // Fields not in the file use built-in defaults.
        assert_eq!(cfg.station.grid_square, "AA00");
        assert_eq!(cfg.ardop.cmd_port, 8515);
        assert_eq!(cfg.modem.ptt_backend, "none");
        assert_eq!(cfg.modem.profile, "hpx_hf");
        assert_eq!(cfg.modem.ota_aggressiveness, ""); // empty = use individual A2/A3 knobs
        assert!((cfg.modem.dcd_squelch - 0.01).abs() < 1e-6);
        assert!(cfg.modem.dcd_squelch_bands.is_empty());
        // tx_power_watts defaults to 0.0 (unspecified) for the regulatory TX log.
        assert!((cfg.station.tx_power_watts - 0.0).abs() < 1e-6);
    }

    /// The §97 TX record is on by default. If this default ever becomes empty the daemon logs a
    /// warning and keeps only the bounded in-memory window — a silent downgrade from "compliance
    /// record" to "recent frames, lost on restart", which is the state #1110 existed to fix.
    #[test]
    fn the_tx_log_path_defaults_to_enabled() {
        let cfg = StationConfig::default();
        assert!(
            !cfg.tx_log_path.trim().is_empty(),
            "the §97 TX record must not default to disabled"
        );
        assert!(
            cfg.tx_log_path.ends_with(".ndjson"),
            "one JSON object per frame: {}",
            cfg.tx_log_path
        );
    }

    #[test]
    fn tx_power_watts_parses_for_the_regulatory_log() {
        let path = unique_tmp("txpower");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "[station]").unwrap();
            writeln!(f, r#"callsign = "K1ABC""#).unwrap();
            writeln!(f, "tx_power_watts = 100.0").unwrap();
        }
        let cfg = load_from(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!((cfg.station.tx_power_watts - 100.0).abs() < 1e-6);
    }

    #[test]
    fn modem_profile_loads_and_template_parses() {
        // Override the profile in a config file and confirm it round-trips.
        let path = unique_tmp("profile");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "[modem]").unwrap();
            writeln!(f, r#"profile = "hpx_ofdm_hf""#).unwrap();
        }
        let cfg = load_from(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(cfg.modem.profile, "hpx_ofdm_hf");

        // The emitted template must parse and carry the documented default.
        let parsed: OpenpulseConfig = toml::from_str(&init_template()).unwrap();
        assert_eq!(parsed.modem.profile, "hpx_hf");
    }

    #[test]
    fn agc_defaults_are_opt_in() {
        let m = ModemConfig::default();
        assert!(!m.agc_enabled, "AGC is opt-in");
        assert_eq!(m.agc_target_rms, 0.3);
        assert_eq!(m.agc_max_gain_db, 40.0);
        // The template's [modem] AGC keys round-trip to the opt-in default.
        let parsed: OpenpulseConfig = toml::from_str(&init_template()).unwrap();
        assert!(!parsed.modem.agc_enabled);
        assert_eq!(parsed.modem.agc_bandwidth, 0.02);
    }

    #[test]
    fn monitor_defaults_are_opt_in_and_empty() {
        let m = MonitorConfig::default();
        assert!(!m.enabled, "monitor is opt-in");
        assert!(m.modes.is_empty());
        // The template's [monitor] block round-trips to the opt-in default.
        let parsed: OpenpulseConfig = toml::from_str(&init_template()).unwrap();
        assert!(!parsed.monitor.enabled);
        assert!(parsed.monitor.modes.is_empty());
    }

    #[test]
    fn discovery_defaults_are_opt_in_and_rx_only() {
        let d = DiscoveryConfig::default();
        assert!(!d.enabled, "discovery is opt-in");
        assert_eq!(d.mode, "rx_only");
        assert_eq!(d.idle_grace_secs, 120);
        assert_eq!(d.max_clock_skew_ms, 2000);
        assert_eq!(d.group, "OPULSE");
        assert_eq!(d.calling_freqs_hz.get("20m"), Some(&14_078_000));
        // The template's [discovery] section round-trips with the band table.
        let parsed: OpenpulseConfig = toml::from_str(&init_template()).unwrap();
        assert!(!parsed.discovery.enabled);
        assert_eq!(
            parsed.discovery.calling_freqs_hz.get("40m"),
            Some(&7_078_000)
        );
        // Rendezvous channel table: index resolution, and the template carries it too.
        assert_eq!(d.rendezvous_channel_hz("20m", 0), Some(14_101_000));
        assert_eq!(d.rendezvous_channel_hz("20m", 2), Some(14_105_000));
        assert_eq!(
            d.rendezvous_channel_hz("20m", 9),
            None,
            "index out of range"
        );
        assert_eq!(d.rendezvous_channel_hz("nosuch", 0), None, "unknown band");
        assert_eq!(
            parsed.discovery.rendezvous_channel_hz("40m", 1),
            Some(7_103_000)
        );
    }

    #[test]
    fn load_twice_returns_same_seed() {
        // Use a unique temp dir to avoid polluting the real config directory.
        let tmp = std::env::temp_dir().join(format!("openpulse_id_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let key_path = tmp.join("identity.key");
        let _ = std::fs::remove_file(&key_path);

        // First call creates the key file.
        let seed1 = load_identity_from(&key_path).unwrap();
        assert_eq!(seed1.len(), 32);
        // Second call reads the same seed.
        let seed2 = load_identity_from(&key_path).unwrap();
        assert_eq!(seed1, seed2);

        let _ = std::fs::remove_file(&key_path);
        let _ = std::fs::remove_dir(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn load_identity_refuses_group_readable_key() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = std::env::temp_dir().join(format!("openpulse_id_perm_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let key_path = tmp.join("identity.key");
        std::fs::write(&key_path, [7u8; 32]).unwrap();
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o640)).unwrap();

        assert!(
            matches!(
                load_identity_from(&key_path),
                Err(ConfigError::InsecureSecretPermissions { .. })
            ),
            "a group-readable identity key must be refused (REQ-CTL-05)"
        );

        let _ = std::fs::remove_file(&key_path);
        let _ = std::fs::remove_dir(&tmp);
    }

    #[test]
    fn save_qsy_config_to_path_persists_unrestricted_mode() {
        let path = unique_tmp("qsy_unrestricted");
        let _ = std::fs::remove_file(&path);

        save_qsy_config_to_path(&path, true, "unrestricted", true).unwrap();

        let cfg = load_from(&path).unwrap();
        assert!(cfg.qsy.enabled);
        assert!(!cfg.qsy.bandplan_awareness_enabled);
        assert!(cfg.qsy.allow_integrated_tuner_on_high_swr);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_qsy_config_to_path_persists_bandplan_mode() {
        let path = unique_tmp("qsy_bandplan");
        let _ = std::fs::remove_file(&path);

        save_qsy_config_to_path(&path, true, "ham-iaru-r2", false).unwrap();

        let cfg = load_from(&path).unwrap();
        assert!(cfg.qsy.enabled);
        assert!(cfg.qsy.bandplan_awareness_enabled);
        assert_eq!(cfg.qsy.bandplan_mode, "ham-iaru-r2");
        assert!(!cfg.qsy.allow_integrated_tuner_on_high_swr);
        let _ = std::fs::remove_file(&path);
    }
}
