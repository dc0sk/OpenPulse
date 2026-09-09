//! Daemon run loop, extracted from the `openpulse-server` binary so it can be
//! driven in-process — notably to bridge two real daemons through a loopback
//! channel for full-stack validation (the twin-station rig).
//!
//! [`run`] takes an already-built audio backend so a harness can inject a
//! [`openpulse_audio::LoopbackBackend`] whose sample tap it bridges; the
//! `openpulse-server` binary injects the config-selected backend via
//! [`build_audio_backend`]. The control port (TCP 9000 / WS 9001 by default)
//! comes from `[daemon]` in the config, so two daemons just use distinct ports.

use crate::{
    apply_command_to_engine, expire_pending_handshake, maybe_qsy_on_interference, ota_status_event,
    process_received_bytes, ws, ControlServer, RuntimeControlState,
};
use openpulse_audio::LoopbackBackend;
use openpulse_config::{ControlSecurityConfig, OpenpulseConfig};
use openpulse_core::audio::{AudioBackend, AudioInputStream};
use openpulse_core::relay::{RelayForwarder, RelayTrustPolicy};
use openpulse_core::station_id::StationIdTimer;
use openpulse_core::trust_store_file::load_trust_store_from_file;
use openpulse_modem::ModemEngine;
use openpulse_qsy::session::QsyPolicy;
use openpulse_radio::{CatController, PttController, RigctldController};
use openpulse_repeater::{CrossBandRepeater, RepeaterConfig};

use bpsk_plugin::BpskPlugin;
use fsk4_plugin::Fsk4Plugin;
use mfsk16_plugin::Mfsk16Plugin;
use ofdm_plugin::OfdmPlugin;
use pilot_plugin::PilotPlugin;
use psk8_plugin::Psk8Plugin;
use qam64_plugin::Qam64Plugin;
use qpsk_plugin::QpskPlugin;
use scfdma_plugin::ScFdmaPlugin;

/// Load the station identity seed, failing closed.
///
/// `load_identity_from` already handles first run internally — it generates the key, persists it with
/// owner-only permissions, and returns `Ok` — so an `Err` here never means "no key yet". It means the
/// key exists and is unusable: group/world-readable (REQ-CTL-05), the wrong length, or unreadable.
///
/// The previous behaviour was to warn and substitute a **random ephemeral key**, which is a fail-open
/// on identity: the station keeps transmitting, but signs handshake frames with a key no peer can
/// match to its known identity, so a configuration error silently degrades into an unrecognisable
/// station. Refusing to start matches the trust-store load a few lines below, whose comment states
/// the same principle — a load error "must fail closed rather than silently start empty"
/// (audit 2026-07-19, #7).
fn load_station_seed(identity_key_path: &str) -> Result<[u8; 32], String> {
    let loaded = if identity_key_path.is_empty() {
        openpulse_config::load_or_generate_identity()
    } else {
        openpulse_config::load_identity_from(std::path::Path::new(identity_key_path))
    };
    loaded.map_err(|e| {
        let where_ = if identity_key_path.is_empty() {
            "the default location".to_string()
        } else {
            identity_key_path.to_string()
        };
        format!(
            "station identity key at {where_} failed to load; refusing to start rather than sign \
             with a throwaway key no peer can verify — fix [station] identity_key_path, its \
             permissions (owner-only, 0600), or the file: {e}"
        )
    })
}

/// Run the full daemon stack to completion (the loop never returns on success).
///
/// `modem_backend` is the engine's audio backend, injected by the caller: the
/// binary passes the config-selected backend; a harness passes a
/// [`LoopbackBackend`] whose sample tap it bridges to a second daemon. Returns
/// `Err` only on a fatal startup misconfiguration.
pub async fn run(cfg: OpenpulseConfig, modem_backend: Box<dyn AudioBackend>) -> Result<(), String> {
    let mode = cfg.modem.mode.clone();
    let station_id = (
        cfg.station.callsign.clone(),
        cfg.station.grid_square.clone(),
    );
    let initial_qsy_enabled = cfg.qsy.enabled;
    let initial_allow_tuner_on_high_swr = cfg.qsy.allow_integrated_tuner_on_high_swr;
    // Opt-in end-to-end session compression: pack OTA data payloads before TX. The RX side always
    // unpacks a self-describing frame regardless of this flag (see the rx tick), so it is safe on one end.
    let compress_tx = cfg.compression.enabled;
    let initial_bandplan_mode = if cfg.qsy.bandplan_awareness_enabled {
        cfg.qsy.bandplan_mode.clone()
    } else {
        "unrestricted".to_string()
    };

    let mut engine = ModemEngine::new(modem_backend);
    // Record the operator's identity + declared TX power in the §97 regulatory TX-metadata log; without
    // this the log stamps an empty callsign / 0 W on every frame (set_callsign is otherwise only wired
    // from two CLI subcommands).
    engine.set_callsign(cfg.station.callsign.clone());
    engine.set_max_power_watts(cfg.station.tx_power_watts);
    // Persist that log (#1110). The in-memory `TxSessionLog` is a bounded window, so without this
    // the §97 record is both capped and lost on restart. Not gated on `observability.audit_mode`:
    // a compliance record that has to be switched on is not one.
    if cfg.station.tx_log_path.trim().is_empty() {
        tracing::warn!(
            "station.tx_log_path is empty: the §97 TX record is in-memory only, capped, and lost on restart"
        );
    } else {
        let p = openpulse_config::logging::expand_tilde(&cfg.station.tx_log_path);
        tracing::info!(path = %p.display(), "recording the §97 TX metadata log");
        engine.set_tx_log_path(Some(p));
    }
    // Pin all audio I/O to a named device when configured (e.g. an snd-aloop PCM
    // for the real-audio twin-station rig). Empty = the backend default device.
    if !cfg.audio.device.is_empty() {
        engine.set_default_device(Some(cfg.audio.device.clone()));
    }
    // Declare the repeater's rung so the burst cap covers what IT must receive (#1308). Under that
    // issue's decision the repeater reads the bursts THIS engine flushes rather than capturing its
    // own audio, so a cap sized from `[modem] mode` alone would truncate exactly the frames the
    // repeater exists to forward — the #1249 defect class, one consumer over.
    if cfg.repeater.enabled {
        engine.set_relay_mode(Some(cfg.repeater.mode.clone()));
    }

    // Optional GPU acceleration: with `--features gpu` and a compatible adapter, the GPU-capable
    // plugins share one GpuContext; otherwise (or when no adapter is found) they use the CPU path.
    #[cfg(feature = "gpu")]
    let gpu_ctx = {
        let ctx = openpulse_gpu::GpuContext::init();
        match &ctx {
            Some(_) => tracing::info!("GPU acceleration enabled"),
            None => tracing::warn!(
                "GPU acceleration requested but no compatible adapter found; using CPU path"
            ),
        }
        ctx
    };

    // Register a GPU-capable plugin: `with_gpu` when a context is available, else `new`.
    #[cfg(feature = "gpu")]
    macro_rules! register_gpu_plugin {
        ($Plugin:ident, $msg:expr) => {
            engine
                .register_plugin(match &gpu_ctx {
                    Some(c) => Box::new($Plugin::with_gpu(c.clone())),
                    None => Box::new($Plugin::new()),
                })
                .expect($msg)
        };
    }
    #[cfg(not(feature = "gpu"))]
    macro_rules! register_gpu_plugin {
        ($Plugin:ident, $msg:expr) => {
            engine
                .register_plugin(Box::new($Plugin::new()))
                .expect($msg)
        };
    }

    register_gpu_plugin!(BpskPlugin, "failed to register BPSK plugin");
    engine
        .register_plugin(Box::new(Fsk4Plugin::new()))
        .expect("failed to register FSK4 plugin");
    engine
        .register_plugin(Box::new(Mfsk16Plugin::new()))
        .expect("failed to register MFSK16 plugin");
    engine
        .register_plugin(Box::new(OfdmPlugin::new()))
        .expect("failed to register OFDM plugin");
    register_gpu_plugin!(Psk8Plugin, "failed to register 8PSK plugin");
    register_gpu_plugin!(Qam64Plugin, "failed to register 64QAM plugin");
    register_gpu_plugin!(QpskPlugin, "failed to register QPSK plugin");
    // SC-FDMA uses the CPU path: its small per-frame 256-pt FFTs are measured ~1.2–1.3× slower
    // on the GPU (dispatch+readback overhead exceeds the tiny FFT benefit at HF frame sizes).
    engine
        .register_plugin(Box::new(ScFdmaPlugin::new()))
        .expect("failed to register SC-FDMA plugin");
    engine
        .register_plugin(Box::new(PilotPlugin::new()))
        .expect("failed to register pilot-framed plugin");

    if cfg.audio.tx_limiter_threshold > 0.0 {
        engine.set_tx_limiter_threshold(cfg.audio.tx_limiter_threshold);
        tracing::info!(
            threshold = cfg.audio.tx_limiter_threshold,
            "TX soft-limiter enabled"
        );
    }

    engine.set_cessb_enabled(cfg.modem.cessb_enabled);
    tracing::info!(cessb = cfg.modem.cessb_enabled, "CE-SSB TX conditioning");

    // Receiver-side automatic notch for out-of-band CW interference (opt-in).
    if cfg.modem.notch_enabled {
        engine.configure_notch(cfg.modem.notch_max, cfg.modem.notch_q, 2000.0);
        engine.set_notch_persistence(cfg.modem.notch_persistence);
        engine.enable_notch();
    }
    tracing::info!(
        notch = cfg.modem.notch_enabled,
        max = cfg.modem.notch_max,
        q = cfg.modem.notch_q,
        persistence = cfg.modem.notch_persistence,
        "receiver auto-notch"
    );

    // Receiver-side streaming AGC (opt-in). Off by default: decode is already level-invariant above the
    // squelch, so this is for QSB level-stabilisation + metering, not decode rescue (REQ-AGC-01).
    if cfg.modem.agc_enabled {
        engine.configure_agc(
            cfg.modem.agc_target_rms,
            cfg.modem.agc_bandwidth,
            cfg.modem.agc_max_gain_db,
        );
        engine.enable_agc();
    }
    tracing::info!(
        agc = cfg.modem.agc_enabled,
        target_rms = cfg.modem.agc_target_rms,
        "receiver AGC"
    );

    // Auto-QSY on a confirmed in-band interferer needs notch persistence to populate the hint.
    let qsy_auto_on_interference = cfg.qsy.auto_qsy_on_interference;
    if qsy_auto_on_interference && !(cfg.modem.notch_enabled && cfg.modem.notch_persistence > 0) {
        tracing::warn!(
            "qsy.auto_qsy_on_interference is set but requires [modem] notch_enabled = true and \
             notch_persistence > 0 to detect in-band interferers; it will not trigger"
        );
    }

    // Our active OTA ladder identity `(name, fingerprint)`, advertised in the signed handshake so a
    // peer running a diverged ladder is detected (then OTA is suppressed). `None` when OTA is off.
    let mut ota_ladder_identity: Option<(String, u64)> = None;
    // Receiver-led OTA adaptive rate-stepping (opt-in via [modem] ota_enabled).
    if cfg.modem.ota_enabled {
        let profile_name = if cfg.modem.ota_profile.is_empty() {
            cfg.modem.profile.as_str()
        } else {
            cfg.modem.ota_profile.as_str()
        };
        match openpulse_core::profile::SessionProfile::by_name(profile_name) {
            Some(profile) => {
                // Ladder identity for backward-compat: two stations must run the same (mode, FEC)
                // mapping for `recommended_level` to mean the same thing. The fingerprint captures
                // that mapping (not local floors); operators can diff it across stations, and the
                // handshake guard (follow-up) will negotiate it. See docs/dev/design/ladder-versioning.md.
                let fingerprint = profile.fingerprint();
                ota_ladder_identity = Some((profile_name.to_string(), fingerprint));
                engine.start_ota_session(profile);
                let parse = openpulse_core::rate::SpeedLevel::from_name;
                let min = (!cfg.modem.ota_min_level.is_empty())
                    .then(|| parse(&cfg.modem.ota_min_level))
                    .flatten();
                let max = (!cfg.modem.ota_max_level.is_empty())
                    .then(|| parse(&cfg.modem.ota_max_level))
                    .flatten();
                if min.is_some() || max.is_some() {
                    engine.ota_set_level_bounds(min, max);
                }
                if !cfg.modem.ota_lock_level.is_empty() {
                    match parse(&cfg.modem.ota_lock_level) {
                        Some(l) => engine.ota_lock_level(l),
                        // Don't silently run adaptive while the operator believes the level is pinned.
                        None => tracing::warn!(
                            value = %cfg.modem.ota_lock_level,
                            "unparseable [modem] ota_lock_level; adaptive OTA stays enabled (expected e.g. \"SL1\")"
                        ),
                    }
                }
                // `ota_min_backlog` / `ota_upgrade_hold_frames` / `ota_aggressiveness` configure
                // `RateAdaptationPolicy` (the sender-led, backlog-gated ladder used by the CLI adaptive
                // demo and the test matrix). The daemon's receiver-led OTA ladder is `OtaRateController`,
                // which steps purely on measured SNR + NACK count and has no backlog/hold/aggressiveness
                // concept — so these three knobs have NO effect here. Warn rather than silently apply them
                // to an inactive policy and log a false "applied". The daemon's OTA rate is bounded by
                // `ota_min_level` / `ota_max_level` / `ota_lock_level` (applied above). (audit #2)
                let mut inert = Vec::new();
                if cfg.modem.ota_min_backlog > 0 {
                    inert.push("ota_min_backlog");
                }
                if cfg.modem.ota_upgrade_hold_frames > 0 {
                    inert.push("ota_upgrade_hold_frames");
                }
                if !cfg.modem.ota_aggressiveness.is_empty() {
                    inert.push("ota_aggressiveness");
                }
                if !inert.is_empty() {
                    tracing::warn!(
                        keys = inert.join(", "),
                        "[modem] {} do not affect the daemon's receiver-led OTA ladder \
                         (they configure the sender-led RateAdaptationPolicy, which the daemon does not \
                         run); use ota_min_level / ota_max_level / ota_lock_level to bound the OTA rate",
                        inert.join(" / ")
                    );
                }
                tracing::info!(
                    profile = profile_name,
                    ladder_fingerprint = format!("{fingerprint:016x}"),
                    "OTA adaptive rate-stepping enabled"
                );
            }
            None => tracing::warn!(
                profile = profile_name,
                "OTA enabled but profile unknown; OTA not started"
            ),
        }
    }

    // Pre-build the cross-band repeater so it is ready when EnableRepeater fires.
    let repeater_burst_tx: Option<
        std::sync::mpsc::SyncSender<openpulse_modem::pipeline::AudioSamples>,
    >;
    let repeater = {
        // Pin rig_b's card on both repeater engines. They used to pass NOTHING, so on a multi-card
        // host they took the OS default — #1311's defect, inside the daemon #1311 held up as one of
        // the two surfaces that got this right. A cross-band repeater is by definition a two-card
        // station, so the default is very likely the WRONG rig.
        let rep_device = if cfg.repeater.tx_device.is_empty() {
            if cfg.repeater.enabled {
                tracing::warn!(
                    "[repeater] tx_device is unset, so rig_b's engines fall back to the OS default \
                     audio device — on a two-card cross-band station that is very likely the MAIN \
                     rig's card, i.e. the repeater transmits into the wrong radio"
                );
            }
            None
        } else {
            if let Some(why) =
                repeater_tx_device_config_error(&cfg.repeater.tx_device, &cfg.audio.device)
            {
                if cfg.repeater.enabled {
                    return Err(format!("refusing to start: {why}"));
                }
                tracing::warn!("{why}; the repeater cannot be enabled with this config");
            }
            Some(cfg.repeater.tx_device.clone())
        };
        let mut rx = ModemEngine::new(build_audio_backend(&cfg.audio.backend));
        rx.set_default_device(rep_device.clone());
        for (name, plugin) in [
            (
                "BPSK",
                Box::new(BpskPlugin::new()) as Box<dyn openpulse_core::plugin::ModulationPlugin>,
            ),
            ("QPSK", Box::new(QpskPlugin::new())),
            ("8PSK", Box::new(Psk8Plugin::new())),
        ] {
            if let Err(e) = rx.register_plugin(plugin) {
                tracing::warn!(plugin = name, error = %e, "repeater rx: plugin registration failed");
            }
        }
        let mut tx = ModemEngine::new(build_audio_backend(&cfg.audio.backend));
        tx.set_default_device(rep_device.clone());
        for (name, plugin) in [
            (
                "BPSK",
                Box::new(BpskPlugin::new()) as Box<dyn openpulse_core::plugin::ModulationPlugin>,
            ),
            ("QPSK", Box::new(QpskPlugin::new())),
            ("8PSK", Box::new(Psk8Plugin::new())),
        ] {
            if let Err(e) = tx.register_plugin(plugin) {
                tracing::warn!(plugin = name, error = %e, "repeater tx: plugin registration failed");
            }
        }
        // rig_b is a SECOND transmitter, and until #1260 it was the one place the daemon still had
        // #1285's fail-open: a failed `RigctldPtt::connect` fell back to `NoOpPtt`, logged "repeater
        // TX will be silent" — and it is not silent, it transmits into an unkeyed rig. Route it
        // through the same builder as the main rig so a config error refuses to start and an
        // unreachable rig gets a controller that refuses to key and keeps retrying.
        let rep_ptt: Option<Box<dyn PttController + Send>> = match cfg.radio.rig_b.as_ref() {
            Some(rig_b) => {
                // Both `RigConfig::default()` and `RadioConfig::default()` carry
                // `rigctld_addr = "127.0.0.1:4532"`, and `rig_b` is `#[serde(default)]`. So an
                // operator who writes a `[radio.rig_b]` header without an address — or leaves the
                // template's commented-out line commented — gets rig_b pointed at the SAME rigctld
                // as the main rig. Two `SharedPtt`s would then each be certain they own one
                // transmitter: the daemon's guard drop sends `T 0` under the repeater's frame and
                // vice versa, and neither watchdog can see the other's key. #1263's refusal rule
                // protects only within one `SharedPtt`, so it cannot reach this.
                if let Some(why) = repeater_rig_b_config_error(
                    &rig_b.backend,
                    &rig_b.rigctld_addr,
                    &cfg.modem.ptt_backend,
                    &cfg.radio.cat_backend,
                    &cfg.radio.rigctld_addr,
                ) {
                    if cfg.repeater.enabled {
                        return Err(format!("refusing to start: {why}"));
                    }
                    tracing::warn!("{why}; the repeater cannot be enabled with this config");
                    None
                } else {
                    match build_ptt_controller(
                        &rig_b.backend,
                        &rig_b.rigctld_addr,
                        &rig_b.serial_port,
                        0,
                    ) {
                        Some(ctrl) => {
                            tracing::info!(
                                backend = %rig_b.backend,
                                addr = %rig_b.rigctld_addr,
                                "repeater PTT built for rig_b"
                            );
                            Some(ctrl)
                        }
                        None => {
                            let why = format!(
                                "[radio.rig_b] backend = \"{}\" is not a usable PTT backend",
                                rig_b.backend
                            );
                            if cfg.repeater.enabled {
                                return Err(format!("refusing to start: {why}"));
                            }
                            tracing::warn!(
                                "{why}; the repeater cannot be enabled with this config"
                            );
                            None
                        }
                    }
                }
            }
            None => {
                if cfg.repeater.enabled {
                    return Err(
                        "refusing to start: repeater.enabled = true but [radio.rig_b] is not \
                         configured — the repeater would transmit into an unkeyed rig; add \
                         [radio.rig_b] to config.toml"
                            .to_string(),
                    );
                }
                None
            }
        };
        let rep_cfg = RepeaterConfig {
            mode: cfg.repeater.mode.clone(),
            tx_hang_ms: cfg.repeater.tx_hang_ms,
            full_duplex: cfg.repeater.full_duplex,
            callsign: cfg.station.callsign.clone(),
            id_interval_secs: cfg.station.auto_id_interval_secs,
        };
        // Bounded and lossy on purpose (#1308): the repeater spends rig_b airtime per burst while
        // the daemon keeps hearing, so a slow relay must drop bursts rather than grow a queue. Four
        // is a couple of frames of slack, not a buffer.
        let (burst_tx, burst_rx) = std::sync::mpsc::sync_channel(4);
        repeater_burst_tx = Some(burst_tx);
        rep_ptt.map(|p| CrossBandRepeater::new(p, rx, tx, burst_rx, rep_cfg))
    };

    let tcp_bind: std::net::SocketAddr =
        format!("{}:{}", cfg.daemon.tcp_bind_addr, cfg.daemon.tcp_port)
            .parse()
            .map_err(|e| format!("invalid daemon.tcp_bind_addr/tcp_port: {e}"))?;
    let ws_bind: std::net::SocketAddr = format!(
        "{}:{}",
        cfg.daemon.websocket_bind_addr, cfg.daemon.websocket_port
    )
    .parse()
    .map_err(|e| format!("invalid daemon.websocket_bind_addr/websocket_port: {e}"))?;

    // Control-channel auth (REQ-CTL-01/02): required on a non-loopback bind, or when configured.
    // Fail closed — refuse to start if auth is required but no PSK is provided.
    let require_auth = openpulse_linksec::auth_required(
        &cfg.daemon.tcp_bind_addr,
        cfg.control_security.require_auth,
    );
    let control_psk = match load_control_psk() {
        Ok(psk) => psk,
        Err(e) => return Err(e),
    };
    if let Some(msg) = inert_psk_key_id_warning(&cfg.control_security) {
        tracing::warn!("{msg}");
    }
    if require_auth && control_psk.is_none() {
        return Err(format!(
            "control channel requires authentication (bind {} / require_auth={}) but no PSK is set — \
             set OPENPULSE_CONTROL_PSK to 64 hex chars (32 bytes)",
            cfg.daemon.tcp_bind_addr, cfg.control_security.require_auth
        ));
    }
    let control_psk = if require_auth { control_psk } else { None };
    if control_psk.is_some() {
        tracing::info!("control channel: PSK authentication + encryption enabled (Noise)");
    }

    let mut handle = ControlServer::spawn(
        tcp_bind,
        &engine,
        crate::ControlServerConfig {
            initial_mode: mode,
            initial_station_id: station_id,
            initial_qsy_enabled,
            initial_bandplan_mode,
            initial_allow_tuner_on_high_swr,
            control_psk,
        },
        None,
    )
    .await
    .map_err(|e| format!("failed to bind TCP control port {tcp_bind}: {e}"))?;

    tracing::info!("openpulse-server TCP control port listening on {tcp_bind}");

    // The WebSocket control endpoint carries the *same* command protocol as the TCP port (PttAssert,
    // SendMessage, EnableRepeater, …) but has no authentication path. Fail closed: if auth is required
    // for either bind, do NOT spawn the unauthenticated WS listener — otherwise it would bypass the auth
    // the TCP port enforces (REQ-CTL-02). WS auth (Noise-over-WS) is a documented follow-up.
    let ws_auth_required = ws_disabled_for_auth(require_auth, &cfg.daemon.websocket_bind_addr);
    if ws_auth_required {
        tracing::warn!(
            ws_bind = %ws_bind,
            "WebSocket control port DISABLED: control auth is required but the WS endpoint cannot \
             authenticate. Use the TCP control port (Noise/PSK), or bind both to loopback."
        );
    } else {
        ws::spawn_ws(
            ws_bind,
            ws::WsShared {
                ev_tx: handle.event_tx.clone(),
                cmd_tx: handle.command_tx.clone(),
                active_mode: handle.active_mode.clone(),
                tx_attenuation_db: handle.tx_attenuation_db.clone(),
                qsy_enabled: handle.qsy_enabled.clone(),
                bandplan_mode: handle.bandplan_mode.clone(),
                allow_tuner_on_high_swr: handle.allow_tuner_on_high_swr.clone(),
                spectrum_tap: handle.spectrum_tap.clone(),
                station_id: handle.station_id.clone(),
                message_store: handle.message_store.clone(),
                valid_modes: handle.valid_modes.clone(),
            },
            None,
        )
        .await
        .map_err(|e| format!("failed to bind WebSocket control port {ws_bind}: {e}"))?;
        tracing::info!("openpulse-server WebSocket control port listening on {ws_bind}");
    }

    // Audit mode (REQ-OBS-01): write a startup snapshot, then record the control-event stream to
    // <archive_dir>/events.ndjson, tapping the same broadcast channel clients subscribe to — no
    // live client required.
    if cfg.observability.audit_mode {
        let dir = openpulse_config::logging::expand_tilde(&cfg.observability.archive_dir);
        if let Err(e) = crate::audit::write_startup_snapshot(&dir, &cfg) {
            tracing::warn!(error = %e, "audit: failed to write snapshot.json");
        }
        crate::audit::spawn_event_recorder(dir, handle.event_tx.subscribe());
    }

    // CAT backend selection. "none" runs with no CAT control for a TRX that
    // rigctld/Hamlib does not support — no connection is attempted, the operator
    // tunes manually, and frequency-control commands are rejected. PTT is
    // independent (see build_ptt_controller / [modem] ptt_backend).
    let mut rig_controller = build_cat_controller(&cfg.radio);

    // Live rig-meter poll task (operator drive-tuning aid): a *dedicated* rigctld
    // connection polls ALC / power-out / SWR and emits `RigStatus` events so the
    // panel can show live ALC while the operator sets drive. The separate
    // connection means it never contends with the PTT/frequency command path.
    // `[radio] meter_poll_ms = 0` disables it.
    if !cfg.radio.cat_backend.eq_ignore_ascii_case("none") && cfg.radio.meter_poll_ms > 0 {
        match RigctldController::connect(&cfg.radio.rigctld_addr) {
            Ok(mut poll_rig) => {
                let ev = handle.event_tx.clone();
                let interval = std::time::Duration::from_millis(cfg.radio.meter_poll_ms);
                tokio::task::spawn_blocking(move || {
                    use crate::protocol::ControlEvent;
                    let mut freq = poll_rig.get_frequency().unwrap_or(0);
                    let mut mode = poll_rig
                        .get_mode()
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default();
                    let mut tick: u32 = 0;
                    loop {
                        std::thread::sleep(interval);
                        // Frequency/mode change rarely — refresh ~every 10 cycles;
                        // poll the meters every cycle.
                        if tick.is_multiple_of(10) {
                            if let Ok(f) = poll_rig.get_frequency() {
                                freq = f;
                            }
                            if let Ok(m) = poll_rig.get_mode() {
                                mode = m.as_str().to_string();
                            }
                        }
                        tick = tick.wrapping_add(1);
                        let _ = ev.send(ControlEvent::RigStatus {
                            rig: "rigctld".into(),
                            freq_hz: freq,
                            mode: mode.clone(),
                            power_w: poll_rig.get_power_out().ok(),
                            alc: poll_rig.get_alc().ok(),
                            swr: poll_rig.get_swr().ok(),
                        });
                    }
                });
                tracing::info!(
                    interval_ms = cfg.radio.meter_poll_ms,
                    "rig meter poll task started (live ALC/power/SWR)"
                );
            }
            Err(err) => tracing::warn!(
                addr = %cfg.radio.rigctld_addr,
                error = %err,
                "rig meter poll: second rigctld connection failed; live meters disabled"
            ),
        }
    }

    // #1285: refuse to start on a PTT CONFIG error, matching what this daemon already does for the
    // station key, the [qsy] policy and the trust store. A mistyped `ptt_backend = "rigctl"` used to
    // warn, start, and then transmit into an unkeyed rig — with `ota_enabled` the daemon opens an OTA
    // session at launch. A typo cannot self-heal, so failing fast costs nothing and there is no
    // startup-ordering trap; an unreachable-but-real backend is handled the other way, inside
    // `build_ptt_controller`.
    if !cfg.modem.ptt_backend.is_empty() && cfg.modem.ptt_backend != "none" {
        if let Err(openpulse_radio::PttError::Config(why)) =
            openpulse_radio::ptt_builder::build_ptt(&openpulse_radio::ptt_builder::PttSpec {
                backend: &cfg.modem.ptt_backend,
                rigctld_addr: &cfg.radio.rigctld_addr,
                device: &cfg.modem.ptt_device,
                gpio_pin: cfg.modem.ptt_gpio,
            })
        {
            return Err(format!(
                "[modem] ptt_backend is unusable; refusing to start rather than transmit into an \
                 unkeyed rig — fix [modem] in config: {why}"
            ));
        }
    }

    let ptt_controller: Option<Box<dyn PttController + Send>> = build_ptt_controller(
        &cfg.modem.ptt_backend,
        &cfg.radio.rigctld_addr,
        &cfg.modem.ptt_device,
        cfg.modem.ptt_gpio,
    );

    let qsy_policy = match QsyPolicy::from_config(
        cfg.qsy.enabled,
        &cfg.qsy.allow_trustlevels,
        &cfg.qsy.bandplan_mode,
        cfg.qsy.bandplan_awareness_enabled,
        cfg.qsy.enforce_max_channel_width,
        cfg.qsy.enforce_segment_conventions,
    ) {
        Ok(p) => p,
        Err(e) => {
            return Err(format!(
                "QSY policy config is invalid; refusing to start with permissive defaults — fix [qsy] in config: {e}"
            ));
        }
    };

    let relay_forwarder = if cfg.relay.enabled {
        let mut policy = if cfg.relay.deny_list.is_empty() {
            RelayTrustPolicy::default()
        } else {
            RelayTrustPolicy::deny_relays(cfg.relay.deny_list.iter().map(|s| s.as_str()))
        };
        policy.set_allow_list(cfg.relay.allow_list.iter().map(|s| s.as_str()));
        let ttl_ms = cfg.relay.store_forward_ttl_s.saturating_mul(1000);
        tracing::info!(
            max_hops = cfg.relay.max_hops,
            deny_count = cfg.relay.deny_list.len(),
            allow_count = cfg.relay.allow_list.len(),
            "relay forwarding enabled"
        );
        Some(RelayForwarder::new(ttl_ms, policy))
    } else {
        None
    };

    // Station identity seed for signing handshake (CONREQ/CONACK) frames. An explicit path lets
    // co-located stations (the twin rig) hold distinct identities; empty uses the platform default.
    let station_seed = load_station_seed(&cfg.station.identity_key_path)?;

    // A configured trust store carries revocations; a load *error* (unreadable/malformed) must fail
    // closed rather than silently start empty and re-admit revoked keys. A missing path is empty-ok.
    let trust_store = if !cfg.trust.store_path.is_empty() {
        match load_trust_store_from_file(std::path::Path::new(&cfg.trust.store_path)) {
            Ok(store) => {
                tracing::info!(path = %cfg.trust.store_path, "trust store loaded");
                store
            }
            Err(e) => {
                return Err(format!(
                    "trust store at {} failed to load; refusing to start with an empty store that would drop revocations — fix [trust] store_path or the file: {e}",
                    cfg.trust.store_path
                ));
            }
        }
    } else {
        Default::default()
    };

    let mut runtime_state = RuntimeControlState {
        // Set by `start_repeater_if_configured` below, which is the only thing that knows whether a
        // thread actually started. Startup used to set this from config alone while spawning
        // nothing, which is what made a config-enabled repeater unstartable (#1308 review).
        repeater_enabled: false,
        repeater,
        repeater_bursts: repeater_burst_tx,
        ptt: crate::ptt::SharedPtt::new(ptt_controller, crate::ptt::DEFAULT_PTT_MAX),
        station_seed,
        local_callsign: cfg.station.callsign.clone(),
        local_grid: cfg.station.grid_square.clone(),
        qsy_candidate_freqs: {
            // A QSY_LIST is transmitted with no SAR, so it must fit one 255-byte frame. Since #1252
            // the signature (93 chars) and freshness stamp cut the ceiling to SIX candidates at the
            // daemon's 8-character token. `candidate_freqs_hz` is unbounded in config, and an
            // over-long list used to fail at TRANSMIT — after the REQ had gone out — wedging the
            // peer until its session TTL. Warn and truncate at startup, where the operator sees it.
            const MAX_QSY_CANDIDATES: usize = 6;
            let mut c = cfg.qsy.candidate_freqs_hz.clone();
            if c.len() > MAX_QSY_CANDIDATES {
                tracing::warn!(
                    configured = c.len(),
                    used = MAX_QSY_CANDIDATES,
                    "[qsy] candidate_freqs_hz exceeds what one QSY_LIST frame can carry; using the                      first {MAX_QSY_CANDIDATES}"
                );
                c.truncate(MAX_QSY_CANDIDATES);
            }
            c
        },
        qsy_switchover_offset_s: u32::try_from(cfg.qsy.switchover_offset_s).unwrap_or_else(|_| {
            tracing::warn!(
                value = cfg.qsy.switchover_offset_s,
                "qsy.switchover_offset_s exceeds u32::MAX; clamping to u32::MAX"
            );
            u32::MAX
        }),
        qsy_scan_dwell_ms: cfg.qsy.scan_dwell_ms,
        qsy_policy,
        relay_forwarder,
        trust_store,
        dcd_squelch_default: cfg.modem.dcd_squelch,
        dcd_squelch_bands: cfg.modem.dcd_squelch_bands.clone(),
        local_ota_ladder: ota_ladder_identity,
        compress_tx,
        filexfer_policy: crate::filexfer::FileTransferPolicy::from_config(&cfg.file_transfer),
        logbook: crate::logbook::Logbook::new(
            cfg.logbook.enabled,
            &cfg.logbook.adif_path,
            &cfg.station.callsign,
            &cfg.station.grid_square,
            &cfg.logbook.peer_grids,
        ),
        discovery: build_discovery_runtime(&cfg),
        monitor: build_monitor_runtime(&cfg),
        discovery_calling_freqs_hz: cfg.discovery.calling_freqs_hz.clone(),
        discovery_rendezvous_channels_hz: cfg.discovery.rendezvous_channels_hz.clone(),
        ..RuntimeControlState::default()
    };
    validate_rendezvous_channels(&cfg);
    if cfg.logbook.enabled {
        tracing::info!(path = %cfg.logbook.adif_path, "ADIF logbook enabled");
    }

    // Spawn the independent PTT watchdog (issue #863): an OS thread that force-releases the
    // transmitter on its max-duration deadline even while this async command loop is blocked inside a
    // long handler (a QSY scan or an OTA send-retry burst), which the cooperative `select!` arm below
    // cannot do — that arm only runs when the loop re-enters `select!`. The thread holds a `Weak` to
    // the shared PTT state and exits on its own when the daemon drops `runtime_state`. `ptt` is a
    // cheap Arc-backed handle to the same state, cloned so the loop's key/unkey sites can borrow it
    // without conflicting with `&mut runtime_state`.
    let ptt = runtime_state.ptt.clone();
    let _ptt_watchdog = runtime_state.ptt.spawn_watchdog(handle.event_tx.clone());

    // Start the repeater if the operator configured it. Deliberately AFTER the PTT watchdog: the
    // repeater keys rig_b, and nothing should be able to key before the watchdog that bounds a
    // stuck carrier is running.
    crate::start_repeater_if_configured(cfg.repeater.enabled, &mut runtime_state, &handle.event_tx);

    // Apply the default DCD squelch at startup; per-band overrides kick in on retune.
    engine.set_dcd_squelch(cfg.modem.dcd_squelch);

    // Execute side-effectful commands against the live modem engine.
    // The receive ticker polls the modem for decoded bytes so the QSY responder path
    // can react to incoming RF frames without operator commands.
    let mut rx_ticker =
        tokio::time::interval(std::time::Duration::from_millis(cfg.daemon.receive_tick_ms));
    // Safety-critical PTT watchdog on its own fast timer + `select!` arm, so a client command flood can
    // no longer starve the transmitter's force-release along with the rx tick (audit robustness item):
    // the watchdog is decoupled from the rx decode, and the loop is no longer `biased` toward commands.
    let mut watchdog_ticker = tokio::time::interval(std::time::Duration::from_millis(100));
    // Emit OTA status roughly once per second (when an OTA session is active).
    let ota_status_period = (1000 / cfg.daemon.receive_tick_ms.max(1)).max(1);
    let mut ota_status_tick: u64 = 0;
    // Session id stamped into OTA ACK frames (a hash field; the sender does not
    // gate on it). The callsign keeps it stable and station-meaningful.
    let ota_session_id = if cfg.station.callsign.is_empty() {
        "ota".to_string()
    } else {
        cfg.station.callsign.clone()
    };
    // Hold ONE capture stream open across receive ticks: cpal is a callback backend
    // whose buffer only fills while the stream is held open, so reopening it every
    // tick (~20 Hz) never warms up on real hardware and decodes nothing. Opened
    // lazily and reopened on read error (e.g. device unplugged); a LoopbackBackend
    // stream clones shared buffers, so this is equivalent to per-tick reopen there.
    let capture_device = (!cfg.audio.device.is_empty()).then(|| cfg.audio.device.clone());
    let mut rx_stream: Option<Box<dyn AudioInputStream>> = None;
    // Edge-triggered so a permanently dead capture device logs once, not once per receive tick.
    // Before the stream-fault latch existed this could never become true: a lost device produced an
    // unbroken run of successful empty reads, so the reopen path below was unreachable and the
    // station went deaf in silence (audit 2026-07-19, #19).
    let mut capture_failed = false;
    // Consecutive-Nack budget: how many Nack-ACKs the IRS will key in a row with no intervening successful
    // data decode before going silent (reset on any decode). Caps a keyed Nack storm — two OTA-active ends
    // answering each other's ACK/QRM bursts forever — and a §97 babbling transmitter on repetitive
    // co-channel QRM. The sender retries on its own ACK-window timeout and the downshift recommendation
    // rides the first Nack, so ARQ is unharmed.
    const OTA_NACK_BUDGET: u32 = 3;
    let mut consecutive_ota_nack: u32 = 0;
    // Periodic station identification (REQ-REG-10): while transmitting, key up and send the
    // callsign at least every `auto_id_interval_secs`. The pure `StationIdTimer` is fed a
    // monotonic ms clock (`id_start`) and armed by polling the engine's `frames_transmitted`
    // delta, so no `note_tx()` call has to be threaded through every transmit site. Disabled
    // when the interval is 0 or the callsign is unset/default (never auto-ID as N0CALL).
    let id_start = std::time::Instant::now();
    let mut id_timer =
        StationIdTimer::new(cfg.station.auto_id_interval_secs.saturating_mul(1000), 0)
            .with_signoff_idle_ms(cfg.station.auto_id_signoff_idle_secs.saturating_mul(1000));
    let id_callsign = cfg.station.callsign.trim().to_string();
    let auto_id_active = id_timer.is_enabled()
        && !id_callsign.is_empty()
        && !id_callsign.eq_ignore_ascii_case("N0CALL");
    let mut tx_frames_seen = engine.frames_transmitted();
    // When the station ID was first deferred because another emission held the key (#1263).
    // `Some` means a deferral is in progress; it logs once on entry and once on release, rather than
    // every 50 ms tick.
    let mut id_deferred_at: Option<u64> = None;
    if auto_id_active {
        tracing::info!(
            interval_s = cfg.station.auto_id_interval_secs,
            callsign = %id_callsign,
            "periodic station ID enabled"
        );
    }
    loop {
        tokio::select! {
            // No `biased`: fair scheduling so a command flood cannot starve the rx tick or the watchdog.
            _ = watchdog_ticker.tick() => {
                // Cooperative poll: fires the same force-release path the independent watchdog thread
                // runs, so a keyed burst that ends normally within the loop is disarmed here first.
                ptt.force_release_if_expired(&handle.event_tx);
            }
            Some(cmd) = handle.commands.recv() => {
                // PTT hardware calls are synchronous; handle them before the async engine dispatch so
                // the borrow of ptt_controller doesn't cross the await point.
                // If the hardware call fails, skip the engine dispatch to avoid emitting a spurious
                // PttChanged event that would tell clients PTT is active when it is not.
                let ptt_hard_failed = handle_ptt_command(&cmd, &ptt, &handle.event_tx);
                // OTA ISS send with real-radio PTT turnaround: when a session is
                // active, a SendMessage drives the receiver-led OTA send here (where
                // the PTT controller lives) — key PTT for the data frame, release it,
                // then listen for the peer's ACK and adopt its recommendation. Handled
                // here rather than in apply_command_to_engine so PTT is sequenced
                // around the half-duplex turnaround.
                // Baseline for the post-arm capture-stream drop below.
                let tx_frames_before_cmd = engine.frames_transmitted();
                let mut ota_send_handled = false;
                if let crate::Command::SendMessage { body, to, .. } = &cmd {
                    // Suppress adaptive OTA (fixed-mode fallback) when a verified peer's rate ladder
                    // differs from ours — a `recommended_level` would otherwise mean different modes.
                    if engine.ota_active() && !runtime_state.ota_suppressed_by_peer() {
                        ota_send_handled = true;
                        // Close the persistent capture stream before keying (audit #917, finding #6).
                        // `receive_ota_ack_within` opens its OWN capture stream for the ACK window, so
                        // leaving this one open puts two concurrent input streams on one device — which
                        // an exclusive ALSA `hw:` device refuses outright, making the ACK unreceivable.
                        // It also cannot be usefully left open: nothing reads it for the whole
                        // transmit + ACK window (up to ~9 s per attempt), so on cpal the capture
                        // callback appends to an unbounded buffer and the next receive tick is handed
                        // one multi-second blob of during-transmit audio. The rx tick reopens lazily.
                        rx_stream = None;
                        // Compress the session payload on the wire when enabled; the peer's rx tick
                        // unpacks the self-describing frame. Falls back to raw bytes when disabled.
                        let payload = if compress_tx {
                            openpulse_core::compression::pack(body.as_bytes())
                        } else {
                            body.as_bytes().to_vec()
                        };
                        ota_send_with_ptt(&mut engine, &ptt, &handle.event_tx, &payload, to);
                    }
                }
                if !ptt_hard_failed && !ota_send_handled {
                    apply_command_to_engine(
                        &cmd,
                        &mut engine,
                        &handle.active_mode,
                        &handle.event_tx,
                        rig_controller.as_mut().map(|c| c as &mut (dyn CatController + Send)),
                        &mut runtime_state,
                    )
                    .await;
                }
                // A `SendFile` / `AcceptFile` queued file-transfer frames — send them PTT-keyed.
                drain_filexfer_tx(
                    &mut engine,
                    &ptt,
                    &handle.event_tx,
                    &mut runtime_state,
                );
                // Any keyed transmit on this arm — the OTA send above, a file-transfer burst, a
                // CONREQ or QSY frame from `apply_command_to_engine` — blocks the loop while nothing
                // reads `rx_stream`. On cpal that capture buffer is unbounded, so whatever it holds is
                // audio captured while this station was transmitting: never decodable, and delivered
                // to the next receive tick as one discontinuous multi-second blob. Drop it so the tick
                // reopens clean. Keyed on the transmit counter rather than the command variant so a
                // future keyed command cannot silently miss this (the OTA path was the one that got
                // noticed; the siblings were not). Non-transmitting commands keep their stream, which
                // is what stops a status-poll flood from thrashing the device open/closed.
                if engine.frames_transmitted() != tx_frames_before_cmd {
                    rx_stream = None;
                }
            }
            _ = rx_ticker.tick() => {
                // This arm TRANSMITS too — the OTA ACK, a CONACK or QSY reply out of
                // `process_received_bytes`, and the periodic §97.119 station ID below — so it needs
                // the same post-transmit stream drop the command arm has (#1319). Snapshot first;
                // the drop is at the end of the arm.
                let tx_frames_before_tick = engine.frames_transmitted();
                // Belt-and-suspenders: also check the watchdog on the rx tick (idempotent — it fires
                // once when the deadline passes). The dedicated `watchdog_ticker` arm and the
                // independent watchdog thread are the primary, flood-/block-proof paths.
                ptt.force_release_if_expired(&handle.event_tx);
                let mode = handle.active_mode.lock().await.clone();
                // Engine capture/transmit are synchronous; LoopbackBackend returns immediately, a
                // real audio backend blocks until samples arrive. The `block_in_place` below does
                // not offload that — this task is the `block_on` thread (#1264/#1301).
                let decode_start = std::time::Instant::now();
                // Accumulate a full burst before decoding: on a streaming (cpal) backend
                // one frame spans many tick windows, so decoding a single partial window
                // can't acquire it. Read the held-open capture stream and accumulate;
                // accumulate_capture returns Some only when the carrier drops.
                // Tee this tick's raw audio to the JS8 discovery dwell buffer when parked on the JS8
                // calling channel (the DCD-burst pipeline can't carry −24 dB signals; §6.2).
                let disco_dwelling = discovery_is_dwelling(&runtime_state);
                let mut discovery_raw: Vec<f32> = Vec::new();
                let burst = tokio::task::block_in_place(|| {
                    if rx_stream.is_none() {
                        rx_stream = Some(engine.open_capture_stream(capture_device.as_deref())?);
                    }
                    let read = match rx_stream.as_mut() {
                        Some(s) => s.read(),
                        None => return Ok(None),
                    };
                    match read {
                        Ok(samples) => {
                            if capture_failed {
                                capture_failed = false;
                                tracing::warn!("audio capture recovered");
                            }
                            if disco_dwelling {
                                discovery_raw = samples.clone();
                            }
                            engine.accumulate_capture(Some(&mode), samples)
                        }
                        Err(e) => {
                            // Drop the stream so the next tick reopens it.
                            rx_stream = None;
                            if capture_failed {
                                tracing::debug!(error = %e, "capture still failing; reopening");
                            } else {
                                capture_failed = true;
                                // WARN, not DEBUG: a station that cannot hear is off the air for
                                // receive, and the operator has to be told once.
                                tracing::warn!(
                                    error = %e,
                                    "audio capture failed — the station is deaf until it recovers; \
                                     reopening the device each tick"
                                );
                            }
                            Ok(None)
                        }
                    }
                });
                // Multi-mode monitor (REQ-RX-01): try the configured extra modes on this burst,
                // independent of the active session mode, and emit a MonitorFrame per decode.
                //
                // Hoisted ABOVE the dispatch on purpose. It used to live inside the non-OTA arm
                // below, which made it unreachable whenever an OTA session was active — and
                // `start_ota_session` runs once at daemon startup under `ota_enabled` and is never
                // cleared (nothing ever sets `engine.ota` back to `None`), so the monitor was dark
                // for the entire process lifetime under exactly the on-air configuration
                // (archetype scan 2026-07-29, finding 9). The monitor is about what the RADIO hears,
                // which does not depend on which decoder the session happens to be running.
                if let Ok(Some(b)) = &burst {
                    if let Some(mon) = runtime_state.monitor.as_mut() {
                        let decoded = tokio::task::block_in_place(|| mon.decode_all(&b.samples));
                        for (m, payload) in decoded {
                            let _ = handle.event_tx.send(
                                crate::protocol::ControlEvent::MonitorFrame {
                                    mode: m,
                                    bytes: payload,
                                },
                            );
                        }
                    }
                }
                // Cross-band relay (#1308): hand the same flushed burst to the running repeater.
                //
                // The repeater used to capture for itself, from a `LoopbackBackend` that
                // `build_audio_backend` hands it fresh — so through `server::run` it heard nothing
                // this daemon heard. One accumulator, one flush, both consumers.
                //
                // `try_send` on purpose: the relay spends rig_b airtime per burst while the daemon
                // keeps hearing, so a busy relay must DROP rather than grow a queue that would
                // eventually put minutes-old audio on the air.
                if let (Ok(Some(b)), Some(tx), true) = (
                    &burst,
                    runtime_state.repeater_bursts.as_ref(),
                    runtime_state.repeater_stop.is_some(),
                ) {
                    if tx.try_send(b.clone()).is_err() {
                            runtime_state.repeater_bursts_dropped =
                                runtime_state.repeater_bursts_dropped.saturating_add(1);
                            tracing::warn!(
                                dropped = runtime_state.repeater_bursts_dropped,
                                "cross-band relay is behind; dropped a burst rather than queueing \
                                 stale audio for transmission"
                        );
                    }
                }
                let bytes = match burst {
                    Ok(Some(burst)) if engine.ota_active() && !runtime_state.ota_suppressed_by_peer() => {
                        // Receiver-led OTA: decode the burst, then key PTT only to answer
                        // with the ACK carrying our absolute recommended_level.
                        match tokio::task::block_in_place(|| {
                            // `mode` is this station's ACTIVE mode — what its own non-ladder traffic
                            // (station ID, filexfer, handshake, QSY, relay) is transmitted at. Passing
                            // it enables the uncoded fallback for exactly that traffic (#1123);
                            // without it an OTA-enabled daemon cannot receive any of it.
                            engine.ota_decode_burst(&burst, &ota_session_id, Some(&mode))
                        }) {
                            Ok(res) => {
                                // A LADDER frame resets the budget and is always ACKed; a failed decode
                                // is a Nack — key it only while within OTA_NACK_BUDGET consecutive
                                // failures.
                                //
                                // A `None` ack means the uncoded fallback recovered non-ladder traffic.
                                // That is not evidence about the rate ladder in either direction, so it
                                // must leave the NACK budget alone (resetting it would credit the ladder
                                // for a frame it did not decode) and must key nothing. Before #1123 such
                                // a burst counted as a decode FAILURE, which drove `on_rx_frame(Failed)`
                                // — including its hysteresis-free fast-downshift — and keyed a NACK at
                                // the peer's own file transfer.
                                let ladder_frame = res.ack.is_some();
                                let decoded = res.payload.is_some();
                                if ladder_frame {
                                    consecutive_ota_nack =
                                        if decoded { 0 } else { consecutive_ota_nack.saturating_add(1) };
                                }
                                // Audit F6 (§97.119): the ACK keys the transmitter; without a valid MYID
                                // the daemon can't auto-ID, so decode the payload but don't send the ACK.
                                if ladder_frame
                                    && (decoded || consecutive_ota_nack <= OTA_NACK_BUDGET)
                                    && runtime_state.local_callsign_valid()
                                {
                                    // RAII guard (REQ-PTT-01): releases at block end / on unwind. On assert
                                    // failure `keyed` returns Err and we skip the ACK, leaving nothing keyed.
                                    // Skipping is correct; skipping SILENTLY is not — receiver-led ARQ then
                                    // stalls with no diagnostic anywhere, and the operator sees a link that
                                    // simply stops (audit 2026-07-19, #8).
                                    if let Some(ack) = res.ack.as_ref() {
                                        // Mode-aware ACK: K=3 union MFSK16-ACK (with a leading FSK4 copy)
                                        // when recommending the sub-floor rung, else FSK4-ACK. The ISS
                                        // union-listens, so either is heard.
                                        if let Err(crate::KeyedTxError::Assert) = crate::keyed_transmit(
                                            &ptt,
                                            Some(&handle.event_tx),
                                            "ota-ack",
                                            || {
                                                tokio::task::block_in_place(|| {
                                                    engine.transmit_ota_ack(ack, None)
                                                })
                                            },
                                        ) {
                                            tracing::warn!(
                                                "OTA ACK skipped — PTT assert failed; the sender will \
                                                 see no ACK and the ARQ exchange will stall"
                                            );
                                        }
                                    }
                                }
                                res.payload.unwrap_or_default()
                            }
                            Err(e) => {
                                tracing::debug!("OTA burst decode error: {e}");
                                Vec::new()
                            }
                        }
                    }
                    Ok(Some(burst)) => {
                        // The monitor already ran above, for every arm.
                        // Log the terminal reason like both sibling arms do. `decode_burst`'s callee
                        // emits partial diagnostics one layer down, so this arm was never fully
                        // silent — but the reason the decode ended (PluginNotFound, bad magic, CRC)
                        // was dropped on the floor while the OTA arm 30 lines up logged its
                        // equivalent (archetype scan 2026-07-29, finding 11).
                        match tokio::task::block_in_place(|| engine.decode_burst(&mode, &burst)) {
                            Ok(b) => b,
                            Err(e) => {
                                tracing::debug!("burst decode error: {e}");
                                Vec::new()
                            }
                        }
                    }
                    Ok(None) => Vec::new(),
                    Err(e) => {
                        tracing::debug!("RX capture error: {e}");
                        Vec::new()
                    }
                };
                // End-to-end session compression: a peer that packed its payload sent a self-describing
                // frame; unpack it here so routing, metrics, and message surfacing see the original bytes.
                // Non-packed frames (control frames, un-packed data) lack the magic and pass through.
                let bytes = openpulse_core::compression::unpack(&bytes).unwrap_or(bytes);
                let decode_ms = decode_start.elapsed().as_secs_f32() * 1000.0;
                if !bytes.is_empty() {
                    process_received_bytes(
                        &bytes,
                        &mut runtime_state,
                        rig_controller.as_mut().map(|c| c as &mut (dyn CatController + Send)),
                        &handle.event_tx,
                        &handle.active_mode,
                        &mut engine,
                    )
                    .await;
                    // The receive handler may have queued FileAccept/BlockAck/FileComplete, or an
                    // inbound ACK may have queued the next send burst — send them PTT-keyed.
                    drain_filexfer_tx(
                        &mut engine,
                        &ptt,
                        &handle.event_tx,
                        &mut runtime_state,
                    );
                }
                // Drop a finished or abandoned QSY negotiation BEFORE the auto-QSY gate reads it:
                // that gate returns early whenever a session exists, so a session that can never
                // complete disables the anti-jam response entirely, and an unsigned inbound QSY_REQ
                // is enough to create one (audit 2026-07-19, #4).
                runtime_state.expire_stale_qsy_session(crate::QSY_SESSION_TTL);
                // Auto-QSY if the notch persistence tracker confirmed an in-band interferer this
                // tick (one a notch can't remove). Runs every tick — interference shows during
                // silence too — and self-gates on config / candidates / an in-flight session.
                maybe_qsy_on_interference(
                    qsy_auto_on_interference,
                    &mut runtime_state,
                    rig_controller.as_mut().map(|c| c as &mut (dyn CatController + Send)),
                    &handle.event_tx,
                    &handle.active_mode,
                    &mut engine,
                )
                .await;
                // Abandon a signed handshake whose CONACK never arrived (timeout).
                expire_pending_handshake(&mut runtime_state, &handle.event_tx);
                // Fire file-transfer offer/stall/verify deadlines so a send whose peer never answered
                // (or a stalled receive) is cleared instead of pinning the subsystem (audit F-5).
                {
                    let mode = handle.active_mode.lock().await.clone();
                    crate::filexfer::poll_timeouts(
                        &mut runtime_state,
                        &handle.event_tx,
                        &mode,
                        epoch_ms(),
                    );
                }
                drain_filexfer_tx(&mut engine, &ptt, &handle.event_tx, &mut runtime_state);
                // JS8 discovery (FF-15): feed the idle predicate + dwell audio, run the slot scheduler,
                // and execute any retune / station-heard outcomes.
                let due_beacon = discovery_tick(
                    &mut runtime_state,
                    &engine,
                    rig_controller.as_mut().map(|c| c as &mut (dyn CatController + Send)),
                    &handle.event_tx,
                    &discovery_raw,
                    epoch_ms(),
                );
                // A due beacon frame is transmitted here, where the PTT controller + `&mut engine`
                // live (half-duplex: key PTT, emit, release).
                if let Some((audio, mode)) = due_beacon {
                    transmit_beacon_with_ptt(&mut engine, &ptt, &audio, &mode);
                }
                // A completed rendezvous QSY hands off to the signed session on the agreed channel: run
                // the same begin_secure_session + CONREQ path as an operator `ConnectPeer` (needs
                // `&mut engine` + the CAT rig, both owned here).
                if let Some(cmd) = take_rendezvous_connect(&mut runtime_state) {
                    apply_command_to_engine(
                        &cmd,
                        &mut engine,
                        &handle.active_mode,
                        &handle.event_tx,
                        rig_controller.as_mut().map(|c| c as &mut (dyn CatController + Send)),
                        &mut runtime_state,
                    )
                    .await;
                }
                // Refresh live metrics so the periodic metrics task can broadcast real values.
                {
                    let mut m = handle.shared_metrics.lock().await;
                    m.afc_correction_hz = engine.last_afc_offset_hz().unwrap_or(0.0);
                    // #1276: read the toggles FROM THE ENGINE here, the one place that holds it.
                    // Answering from a daemon-side shadow of the commands sent would restate what
                    // the client already believes and could not catch the engine disagreeing.
                    m.front_end = front_end_state(&engine, &runtime_state);
                    m.total_rx_bytes += bytes.len() as u64;
                    // EWMA of decode latency, sampled only when a frame was actually decoded.
                    if !bytes.is_empty() {
                        m.decode_latency_ms = if m.decode_latency_ms <= 0.0 {
                            decode_ms
                        } else {
                            m.decode_latency_ms * 0.8 + decode_ms * 0.2
                        };
                        // Live compressibility of the decoded payload stream: the session compressor's
                        // best-effort size (never larger than raw) drives the reported compress_ratio.
                        let (compressed, _algo) =
                            openpulse_core::compression::compress_if_smaller(&bytes);
                        m.raw_payload_bytes += bytes.len() as u64;
                        m.compressed_payload_bytes += compressed.len() as u64;
                    }
                }
                // Feed the spectrum/waterfall tap with the engine's most recent audio
                // window (RX capture, or the last TX). Without this the broadcast task
                // FFTs the zero-initialised tap and the panel shows a flat spectrum.
                let audio = engine.last_audio();
                if !audio.is_empty() {
                    *handle.spectrum_tap.write().await = audio.to_vec();
                }
                // Periodic OTA status broadcast (~1 Hz) while a session is active.
                ota_status_tick += 1;
                if engine.ota_active() && ota_status_tick.is_multiple_of(ota_status_period) {
                    let _ = handle.event_tx.send(ota_status_event(&engine));
                }
                // Station ID (REQ-REG-10). Arm from the TX-frame delta (any data/ACK/retransmit we
                // emitted since the last poll), then key PTT and send the callsign in the active mode
                // when either trigger is due: the 10-min *interval* ID during a communication, or the
                // *sign-off* ID once the channel has gone quiet at the end of one. Re-baseline the
                // counter afterwards so the ID frame itself is not counted as further TX activity.
                if auto_id_active {
                    let now_ms = id_start.elapsed().as_millis() as u64;
                    let tx_now = engine.frames_transmitted();
                    if tx_now != tx_frames_seen {
                        id_timer.note_tx(now_ms);
                        tx_frames_seen = tx_now;
                    }
                    let id_reason = if id_timer.id_due(now_ms) {
                        Some("interval")
                    } else if id_timer.signoff_due(now_ms) {
                        Some("sign-off")
                    } else {
                        None
                    };
                    if let Some(reason) = id_reason {
                        let id_mode = handle.active_mode.lock().await.clone();
                        let id_body = format!("DE {id_callsign}");
                        // RAII guard (REQ-PTT-01): releases at block end / on unwind; skip on assert fail.
                        // A skipped station ID is a §97.119 obligation not met, so it is logged at
                        // `error`: the operator has to know the station is transmitting without
                        // identifying, and this was previously silent (audit 2026-07-19, #8).
                        let deferred = match crate::keyed_transmit(
                            &ptt,
                            Some(&handle.event_tx),
                            "station-id",
                            || {
                                tokio::task::block_in_place(|| {
                                    engine.transmit(id_body.as_bytes(), &id_mode, None)
                                })
                            },
                        ) {
                            Ok(()) => {
                                tracing::info!(
                                    callsign = %id_callsign,
                                    mode = %id_mode,
                                    kind = reason,
                                    "transmitted station ID"
                                );
                                false
                            }
                            // A refused assert is a §97.119 obligation NOT met — error, not warn.
                            Err(crate::KeyedTxError::Assert) => {
                                tracing::error!(
                                    callsign = %id_callsign,
                                    kind = reason,
                                    "station ID NOT transmitted — PTT assert failed; §97.119 \
                                     identification has not gone out"
                                );
                                false
                            }
                            Err(crate::KeyedTxError::Transmit(e)) => {
                                tracing::warn!(
                                    error = %e, mode = %id_mode, "station-ID transmit failed"
                                );
                                false
                            }
                            // Busy, not broken (#1263).
                            Err(crate::KeyedTxError::AlreadyKeyed) => true,
                        };

                        // Advance on success and on FAULT — a persistent hardware fault is surfaced
                        // by the error above, not by per-tick retry spam — but DEFER when the rig was
                        // merely BUSY, or a station ID refused while another emission held the key
                        // would skip a whole interval and the §97.119 obligation with it (#1263).
                        //
                        // The asymmetry is the point, and it is why `AlreadyKeyed` had to be its own
                        // variant: deferring on `Assert` too would key-attempt a faulted rig at the
                        // 50 ms tick rate for the entire 180 s watchdog window.
                        if deferred {
                            if id_deferred_at.is_none() {
                                id_deferred_at = Some(now_ms);
                                tracing::warn!(
                                    callsign = %id_callsign,
                                    kind = reason,
                                    "station ID deferred — the transmitter is held by another \
                                     emission; it goes out when the key is released"
                                );
                            }
                        } else {
                            if let Some(since) = id_deferred_at.take() {
                                tracing::info!(
                                    deferred_ms = now_ms.saturating_sub(since),
                                    "deferred station ID transmitted"
                                );
                            }
                            id_timer.mark_identified(now_ms);
                            tx_frames_seen = engine.frames_transmitted();
                        }
                    }
                }
                // #1007's rule, on this arm too (#1319). Anything keyed above — the OTA ACK, a
                // CONACK or QSY reply, the station ID — blocked this loop while nothing read
                // `rx_stream`. On cpal that capture buffer is unbounded, so whatever it holds is
                // audio captured while this station was transmitting: never decodable, and handed to
                // the next tick as one discontinuous blob. A QSY line at BPSK31 is tens of seconds
                // of it. Keyed on the transmit counter, matching the command arm, so a future keyed
                // emission on this arm cannot silently miss it.
                if engine.frames_transmitted() != tx_frames_before_tick {
                    rx_stream = None;
                }
            }
        }
    }
}

/// Receiver-led OTA send with the real-radio half-duplex PTT turnaround.
///
/// For each of up to `1 + MAX_RETRIES` attempts: key PTT, transmit the data frame
/// at the current OTA mode+FEC, **release PTT**, then listen for the peer's FSK4
/// ACK (PTT down) and adopt its absolute `recommended_level` — which steps the
/// rate ladder. Splitting the transmit from the ACK listen (vs the bundled
/// `transmit_arq_ota`) is what lets PTT be keyed only for the TX, so the radio can
/// hear the ACK. PTT is a no-op on the twin rig (NoOpPtt); on a real rig this is
/// the correct turnaround. The long phases run under `block_in_place`, which on this call site
/// frees NOTHING: `server::run`'s future is `!Send` and is polled by `#[tokio::main]`'s `block_on`
/// on the main thread, where `block_in_place` has no core to hand off and runs the closure inline.
/// It is here so the call cannot panic if this code is ever polled on a worker — not to keep the
/// runtime responsive, which it does not do. The loop is unresponsive for the frame's duration
/// (~8.5 s at BPSK250, ~68 s at BPSK31); that is #1301, and #1264 was filed on the belief that this
/// comment was accurate.
/// Drain queued file-transfer fragments to the air as one PTT-keyed burst (assert → transmit all →
/// release), so the half-duplex peer can answer. Called after every command and receive tick; a no-op
/// when the queue is empty. On a PTT-assert failure the burst is dropped and the session's stall/retry
/// path recovers.
/// Upper bound on fragments per keyed burst (the plan §5.3 clamp), independent of the airtime bound.
const MAX_FRAGS_PER_BURST: usize = 64;

/// Split `n` queued fragments into airtime-bounded bursts, returning the fragment count of each burst
/// (which sum to `n`). A burst holds at most `max_frags` fragments and, past its first, stops before
/// its estimated airtime would exceed `burst_max_secs`; the first fragment is always taken, so a lone
/// oversized fragment still forms its own (never empty) burst. `air_secs(i)` estimates fragment `i`.
fn plan_bursts(
    n: usize,
    air_secs: impl Fn(usize) -> f64,
    burst_max_secs: f64,
    max_frags: usize,
) -> Vec<usize> {
    let mut bursts = Vec::new();
    let mut i = 0;
    while i < n {
        let mut count = 0;
        let mut acc = 0.0f64;
        while i + count < n && count < max_frags {
            let secs = air_secs(i + count).max(0.0);
            if count > 0 && acc + secs > burst_max_secs {
                break; // keep the first fragment even if it alone exceeds the budget
            }
            acc += secs;
            count += 1;
        }
        bursts.push(count);
        i += count;
    }
    bursts
}

/// Drain the file-transfer TX queue as one or more **airtime-bounded** PTT-keyed bursts: each burst is
/// its own assert → transmit → release cycle, sized by [`plan_bursts`] so no single keying exceeds
/// `burst_max_secs` (keeps a large transfer under the radio's PTT watchdog and yields between bursts).
fn drain_filexfer_tx(
    engine: &mut ModemEngine,
    ptt: &crate::ptt::SharedPtt,
    event_tx: &std::sync::Arc<tokio::sync::broadcast::Sender<crate::protocol::ControlEvent>>,
    runtime_state: &mut RuntimeControlState,
) {
    if runtime_state.filexfer_tx_queue.is_empty() {
        return;
    }
    let queue = std::mem::take(&mut runtime_state.filexfer_tx_queue);
    let burst_max = runtime_state.filexfer_policy.burst_max_secs;

    // Plan bursts up front (immutable engine borrow) so the keying loop can borrow the engine mutably.
    let plan = plan_bursts(
        queue.len(),
        |idx| {
            engine
                .estimate_air_secs(queue[idx].0.len(), &queue[idx].1)
                .unwrap_or(0.0)
        },
        burst_max,
        MAX_FRAGS_PER_BURST,
    );

    let mut idx = 0;
    for count in plan {
        let burst = &queue[idx..idx + count];
        idx += count;
        // RAII guard (REQ-PTT-01) per burst: releases at block end / on unwind. On assert failure abort
        // the drain — the remaining queue was already taken, so those fragments are dropped this pass.
        // One keying for the whole burst, not per fragment.
        if crate::keyed_transmit(ptt, Some(event_tx), "filexfer", || {
            for (frag, mode) in burst {
                tokio::task::block_in_place(|| engine.transmit(frag, mode, None))?;
            }
            Ok(())
        })
        .is_err()
        {
            return;
        }
    }
}

// ── JS8 discovery (FF-15) ────────────────────────────────────────────────────

/// Build the JS8 discovery runtime from `[discovery]` config. Always built (so Enable/Disable work at
/// runtime); the config `enabled` flag gates activation. The initial calling frequency is the 20 m
/// entry (or the first band in the table); `discovery_tick` re-selects the entry for the operator's
/// current home band before each activation. `None` only if the band table is empty.
fn build_discovery_runtime(
    cfg: &openpulse_config::OpenpulseConfig,
) -> Option<openpulse_discovery::DiscoveryRuntime> {
    use openpulse_discovery::{
        DiscoveryParams, DiscoveryRuntime, HintPayload, Submode, TxMode, CAP_HPX, CAP_QSY,
        CAP_RENDEZVOUS,
    };
    let d = &cfg.discovery;
    // The custom `[discovery] group` is not yet wired (the `@OPULSE` group is baked into the beacon
    // frame packing and RX filter); warn so an operator who sets it isn't misled into thinking it took
    // effect (audit: dead config field).
    if !d.group.is_empty()
        && !d
            .group
            .eq_ignore_ascii_case(openpulse_discovery::OPULSE_GROUP)
    {
        tracing::warn!(
            group = %d.group,
            "[discovery] group is reserved and not yet wired; the @OPULSE group is used regardless"
        );
    }
    // Beacon/full opt into TX (Phase E, §97.221 doc in place); anything else is RX-only. TX also
    // requires a callsign — an empty one keeps the station silent regardless of mode.
    let tx_mode = match d.mode.trim().to_ascii_lowercase().as_str() {
        "beacon" => TxMode::Beacon,
        "full" => TxMode::Full,
        _ => TxMode::RxOnly,
    };
    let calling = d
        .calling_freqs_hz
        .get("20m")
        .copied()
        .or_else(|| d.calling_freqs_hz.values().next().copied())?;
    let submode = match d.submode.to_ascii_lowercase().as_str() {
        "slow" => Submode::Slow,
        "fast" => Submode::Fast,
        "turbo" => Submode::Turbo,
        "ultra" => Submode::Ultra,
        _ => Submode::Normal,
    };
    // Advertise what this station can do; pref-channel none (63), NORMAL listen submode.
    let hint = Some(HintPayload {
        caps: CAP_HPX | CAP_RENDEZVOUS | CAP_QSY,
        pref_channel: 63,
        listen_submode: 0,
    });
    Some(DiscoveryRuntime::new(DiscoveryParams {
        enabled: d.enabled,
        idle_grace_ms: d.idle_grace_secs.saturating_mul(1000),
        dwell_ms: d.dwell_secs.saturating_mul(1000),
        station_ttl_ms: d.station_ttl_secs.saturating_mul(1000),
        submode,
        calling_freq_hz: calling,
        tx_mode,
        callsign: cfg.station.callsign.clone(),
        grid: cfg.station.grid_square.clone(),
        hint,
        heartbeat_interval_slots: d.heartbeat_interval_slots.max(1) as u64,
        hint_interval_beacons: d.hint_interval_beacons as u64,
        tx_offset_hz: 1500.0,
        max_clock_skew_ms: d.max_clock_skew_ms,
    }))
}

/// Build the simultaneous multi-mode monitor (REQ-RX-01) from `[monitor]` config; `None` when disabled
/// or no modes are listed.
fn build_monitor_runtime(
    cfg: &openpulse_config::OpenpulseConfig,
) -> Option<crate::monitor::MonitorRuntime> {
    if !cfg.monitor.enabled {
        return None;
    }
    let rt = crate::monitor::MonitorRuntime::new(cfg.monitor.modes.clone());
    if let Some(rt) = &rt {
        tracing::info!(modes = ?rt.modes(), "multi-mode receive monitor enabled");
    }
    rt
}

/// Startup bandplan gate for the rendezvous channel table: log a warning for any configured working
/// frequency the default bandplan flags (out of band / wrong segment). Advisory only — the operator's
/// channels are honoured; this surfaces a likely misconfiguration before it is used on air.
/// Whether the WebSocket control port must be disabled because control auth is required but the WS
/// endpoint cannot authenticate: true if the TCP port needs auth, or the WS bind is itself non-loopback.
fn ws_disabled_for_auth(tcp_require_auth: bool, ws_bind_addr: &str) -> bool {
    tcp_require_auth || openpulse_linksec::auth_required(ws_bind_addr, false)
}

fn validate_rendezvous_channels(cfg: &openpulse_config::OpenpulseConfig) {
    let policy = openpulse_qsy::bandplan::BandplanPolicy::default();
    for (band, freqs) in &cfg.discovery.rendezvous_channels_hz {
        for (idx, &hz) in freqs.iter().enumerate() {
            if let Err(e) = policy.validate_frequency(hz, "DATA") {
                tracing::warn!(
                    band = %band,
                    index = idx,
                    freq_hz = hz,
                    "rendezvous channel fails the bandplan check: {e}"
                );
            }
        }
    }
}

/// Retune the rig to `hz` (no rig / loopback counts as success).
fn discovery_retune(rig: &mut Option<&mut (dyn CatController + Send)>, hz: u64) -> bool {
    match rig.as_mut() {
        Some(c) => c.set_frequency(hz).is_ok(),
        None => true,
    }
}

/// One JS8 NORMAL T/R slot in ms (the discovery MVP is NORMAL-only). Used to convert a rendezvous
/// `switch_in_slots` count into a wall-clock QSY deadline.
const JS8_NORMAL_SLOT_MS: u64 = 15_000;

/// Whether discovery is parked on the JS8 calling channel (Dwelling) — the only state in which the
/// rx-tick tees its raw capture audio to the weak-signal decoder (the DCD-burst pipeline can't carry
/// −24 dB JS8; §6.2). Extracted from the rx-tick `select!` arm so the tee predicate is unit-testable.
fn discovery_is_dwelling(rs: &RuntimeControlState) -> bool {
    rs.discovery
        .as_ref()
        .is_some_and(|d| d.state() == openpulse_discovery::DiscoveryState::Dwelling)
}

/// Consume a completed-rendezvous QSY readiness into the `ConnectPeer` command that hands off to the
/// signed session on the agreed working channel, or `None` when no rendezvous is ready. Extracted from
/// the rx-tick `select!` arm so the peer→command mapping and take-once semantics are unit-testable.
fn take_rendezvous_connect(
    rs: &mut RuntimeControlState,
) -> Option<crate::protocol::ControlCommand> {
    rs.rendezvous_connect_ready
        .take()
        .map(|(peer, _freq_hz)| crate::protocol::ControlCommand::ConnectPeer { callsign: peer })
}

/// Feed one rx-tick's raw audio + the idle predicate into the discovery runtime and execute its
/// outcomes (retune via CAT, home-frequency tracking, event forwarding). No-op when unconfigured.
fn discovery_tick(
    runtime_state: &mut RuntimeControlState,
    engine: &ModemEngine,
    mut rig: Option<&mut (dyn CatController + Send)>,
    event_tx: &std::sync::Arc<tokio::sync::broadcast::Sender<crate::protocol::ControlEvent>>,
    raw_samples: &[f32],
    now_ms: u64,
) -> Option<(Vec<f32>, String)> {
    use openpulse_discovery::DiscoveryOutcome as O;
    runtime_state.discovery.as_ref()?; // nothing to do without a discovery runtime

    // A scheduled post-rendezvous QSY that has come due: both stations retune to the agreed working
    // frequency and hand off to the signed session. The `switch_in_slots` delay ensured the Accept was
    // heard first. We drop the discovery home so the stand-down does not tune back — the QSO owns the
    // dial now — and leave the handoff itself to `server::run` (it holds `&mut engine`).
    if let Some((peer, freq_hz, due_at_ms)) = runtime_state.rendezvous_qsy_due.clone() {
        if now_ms >= due_at_ms {
            runtime_state.rendezvous_qsy_due = None;
            runtime_state.discovery_home_freq_hz = None;
            if discovery_retune(&mut rig, freq_hz) {
                runtime_state.last_freq_hz = Some(freq_hz);
            }
            if let Some(rt) = runtime_state.discovery.as_mut() {
                let _ = rt.preempt(); // stand discovery down; home is cleared so RestoreHome is a no-op
            }
            runtime_state.rendezvous_connect_ready = Some((peer, freq_hz));
            crate::emit_discovery_status(runtime_state, event_tx);
            return None;
        }
    }
    // Simplified idle predicate (plan §4.3): the modem is free of any session/handshake/transfer.
    let idle = engine.hpx_state() == openpulse_core::hpx::HpxState::Idle
        && runtime_state.pending_handshake.is_none()
        && runtime_state.file_rx.is_none()
        && runtime_state.file_tx.is_none()
        && !engine.ota_active();
    // While inactive, target the JS8 calling frequency for the operator's current home band, so
    // activation QSYs within-band instead of always to 20 m. `last_freq_hz` is the home dial while
    // inactive (it becomes the JS8 freq once dwelling, so only refresh before activation).
    let inactive = runtime_state.discovery.as_ref().map(|d| d.state())
        == Some(openpulse_discovery::DiscoveryState::Inactive);
    let home_band = inactive
        .then(|| {
            runtime_state
                .last_freq_hz
                .and_then(openpulse_qsy::bandplan::band_label_for_hz)
        })
        .flatten();
    let per_band_freq =
        home_band.and_then(|label| runtime_state.discovery_calling_freqs_hz.get(label).copied());
    // The responder's usable rendezvous channels are the indices of the home band's working-channel
    // table (empty ⇒ any inbound proposal is rejected `NoCommonFreq`).
    let per_band_channels: Option<Vec<u8>> = home_band.map(|label| {
        let n = runtime_state
            .discovery_rendezvous_channels_hz
            .get(label)
            .map_or(0, |v| v.len().min(u8::MAX as usize));
        (0..n as u8).collect()
    });
    // The working-channel table for the band we are dwelling on, to resolve an agreed channel index→Hz.
    let dwell_channels: Option<Vec<u64>> = runtime_state
        .discovery
        .as_ref()
        .map(|d| d.dial_freq_hz())
        .and_then(openpulse_qsy::bandplan::band_label_for_hz)
        .and_then(|label| {
            runtime_state
                .discovery_rendezvous_channels_hz
                .get(label)
                .cloned()
        });
    // Decode (on slot boundaries) runs inside `tick`. `block_in_place` does NOT keep the async loop
    // responsive here — see the note on `ota_send_with_ptt`: on the `block_on` thread it runs inline.
    // It guards against a future where this is polled on a worker (#1264/#1301).
    let outcomes = tokio::task::block_in_place(|| {
        let rt = runtime_state.discovery.as_mut().expect("checked above");
        if let Some(hz) = per_band_freq {
            rt.set_dial_freq_hz(hz);
        }
        if let Some(ch) = per_band_channels {
            rt.set_rendezvous_channels(ch);
        }
        rt.push_audio(raw_samples);
        rt.tick(now_ms, idle)
    });
    let mut heard_peer = false;
    let mut pending_beacon: Option<(Vec<f32>, String)> = None;
    for o in outcomes {
        match o {
            O::TransmitBeacon { audio, mode } => {
                // Direct DCD gate at the emit decision (not the 0.3-persistence CSMA, which would
                // break slot alignment): defer the beacon if the channel is occupied.
                if engine.is_channel_busy() {
                    tracing::debug!("discovery: deferring beacon — channel busy");
                } else {
                    pending_beacon = Some((audio, mode));
                }
            }
            O::Retune { dial_freq_hz } => {
                runtime_state.discovery_home_freq_hz = runtime_state.last_freq_hz;
                let ok = discovery_retune(&mut rig, dial_freq_hz);
                if ok {
                    runtime_state.last_freq_hz = Some(dial_freq_hz);
                }
                if let Some(rt) = runtime_state.discovery.as_mut() {
                    let _ = rt.qsy_complete(ok);
                }
                crate::emit_discovery_status(runtime_state, event_tx);
            }
            O::RestoreHome => {
                if let Some(home) = runtime_state.discovery_home_freq_hz.take() {
                    if discovery_retune(&mut rig, home) {
                        runtime_state.last_freq_hz = Some(home);
                    }
                }
                crate::emit_discovery_status(runtime_state, event_tx);
            }
            O::StateChanged(_) => crate::emit_discovery_status(runtime_state, event_tx),
            O::StationHeard {
                callsign,
                grid,
                is_new,
            } => {
                heard_peer = true;
                let _ = event_tx.send(crate::protocol::ControlEvent::StationHeard {
                    callsign,
                    grid: grid.unwrap_or_default(),
                    is_new,
                });
            }
            O::RendezvousAgreed {
                peer,
                channel,
                switch_in_slots,
            } => {
                // Resolve the agreed channel index to Hz via the dwelling band's table, surface the
                // agreement, and schedule the QSY + handoff for after the switch delay (so the Accept is
                // heard and both stations retune together).
                match dwell_channels
                    .as_ref()
                    .and_then(|v| v.get(channel as usize))
                    .copied()
                {
                    Some(freq_hz) => {
                        let due_at_ms = now_ms + switch_in_slots as u64 * JS8_NORMAL_SLOT_MS;
                        runtime_state.rendezvous_qsy_due = Some((peer.clone(), freq_hz, due_at_ms));
                        let _ = event_tx.send(crate::protocol::ControlEvent::RendezvousAgreed {
                            peer,
                            freq_hz,
                        });
                    }
                    None => {
                        tracing::warn!(peer = %peer, channel, "rendezvous agreed on an unknown channel index");
                        let _ = event_tx.send(crate::protocol::ControlEvent::RendezvousFailed {
                            peer,
                            reason: "agreed channel index has no configured frequency".to_string(),
                        });
                    }
                }
            }
            O::RendezvousRejected { peer, reason } => {
                let _ = event_tx.send(crate::protocol::ControlEvent::RendezvousFailed {
                    peer,
                    reason: format!("peer declined ({reason:?})"),
                });
            }
            O::RendezvousTimedOut { peer } => {
                let _ = event_tx.send(crate::protocol::ControlEvent::RendezvousFailed {
                    peer,
                    reason: "no reply before timeout".to_string(),
                });
            }
        }
    }
    // Fold any newly-heard OpenPulse-marked stations into the shared peer cache (§5.2).
    if heard_peer {
        crate::sync_discovered_peers(runtime_state, now_ms);
    }
    // Hand a due beacon frame back to the caller, which owns the PTT + `&mut engine` needed to key
    // the transmitter and emit it (see `transmit_beacon_with_ptt`).
    pending_beacon
}

/// UTC epoch milliseconds now.
fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Key PTT, emit one JS8 beacon frame via the raw-audio seam, and release PTT. Silent (no
/// `PttChanged` events — this is an internal beacon, not an operator key). Any PTT/transmit error is
/// logged and the beacon slot is skipped; a failed release leaves the watchdog armed so it
/// force-releases the still-keyed transmitter.
fn transmit_beacon_with_ptt(
    engine: &mut ModemEngine,
    ptt: &crate::ptt::SharedPtt,
    audio: &[f32],
    mode: &str,
) {
    // RAII guard (REQ-PTT-01): releases at scope end, and on unwind if `transmit_raw_audio` panics.
    let _ = crate::keyed_transmit(ptt, None, "discovery-beacon", || {
        engine.transmit_raw_audio(audio, mode, None)
    });
}

/// Apply a manual `PttAssert`/`PttRelease` command to the PTT hardware, synchronously.
///
/// Returns `true` when the hardware call failed (a stuck/absent rig): the caller then skips the
/// engine dispatch so no spurious `PttChanged` tells clients PTT is active when it is not. Any other
/// command (or no controller) is a no-op returning `false`.
fn handle_ptt_command(
    cmd: &crate::Command,
    ptt: &crate::ptt::SharedPtt,
    event_tx: &tokio::sync::broadcast::Sender<crate::protocol::ControlEvent>,
) -> bool {
    // Hardware only: the watchdog arm/disarm and the `PttChanged` edge for a manual command are
    // applied in `apply_command_to_engine` (the `PttAssert`/`PttRelease` arms). Keeping this
    // HW-only preserves the #836 contract — a hard failure here skips that dispatch entirely.
    let (result, action) = match cmd {
        // #1263: the operator's key is an OWNED key now, labelled `manual`, so an automatic
        // emission arriving mid-hold is refused instead of stealing the deadline — and, critically,
        // its guard's drop can no longer release the operator's carrier.
        crate::Command::PttAssert => (
            match ptt.key_as_manual() {
                // Already keyed BY THE OPERATOR is idempotent — a second `ptt-assert` must not
                // re-arm, or re-asserting every 170 s would defeat the 180 s watchdog.
                Err(openpulse_radio::PttError::AlreadyKeyed { held_by: "manual" }) => Ok(()),
                other => other,
            },
            "assert",
        ),
        // The operator's HARD OVERRIDE, kept deliberately (maintainer decision, #1263): this drops
        // the transmitter whoever holds it. Without it an operator watching a runaway automatic
        // burst would have to wait out the full 180 s watchdog. `force_release` bumps the key
        // generation, so the displaced holder's guard cannot later release somebody else's key.
        crate::Command::PttRelease => {
            // Preserve the #836 contract: a hardware release that FAILS must report hard failure so
            // the caller skips the dispatch and no `PttChanged{false}` claims a state the rig never
            // reached. `force_release` leaves the watchdog armed in that case, so it retries.
            // (`ptt_command_guard_reports_hardware_failure_to_skip_dispatch` caught this when the
            // first version of this arm swallowed the outcome — the same shape as #1258's `"none"`
            // drift: a mechanism swap quietly changing a caller's contract.)
            let outcome = ptt.force_release_manual();
            let r = match outcome {
                openpulse_radio::shared_ptt::UnkeyOutcome::Failed => Err(
                    openpulse_radio::PttError::Serial("hardware release failed".into()),
                ),
                _ => Ok(()),
            };
            (r, "release")
        }
        _ => return false,
    };
    if let Err(e) = result {
        // A refused assert must REACH the client (#1263). The CLI's one-shot sender prints `ok`
        // for anything that is not a `CommandError`, so an operator whose key was refused because
        // an automatic emission held the transmitter would otherwise be told it succeeded.
        let refused = matches!(e, openpulse_radio::PttError::AlreadyKeyed { .. });
        if refused {
            tracing::info!("PTT {action} refused: {e}");
        } else {
            tracing::warn!("PTT {action} failed: {e}");
        }
        let _ = event_tx.send(crate::protocol::ControlEvent::CommandError {
            command: format!("ptt_{action}"),
            reason: e.to_string(),
        });
        return true;
    }
    false
}

/// Classification of one OTA send attempt's outcome, driving the retry policy in [`run_ota_retry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OtaAttempt {
    /// The peer ACKed (non-NACK): the message is delivered — stop.
    Delivered,
    /// The peer NACKed: it heard us but couldn't decode — retransmit the data frame.
    Nack,
    /// No ACK arrived within the window: the peer is silent this round.
    Timeout,
    /// The transmitter could not be keyed: nothing went out and retrying will not help (#1295).
    ///
    /// Distinct from `Delivered` because the frame did NOT reach the peer, and distinct from `Nack`
    /// because a hardware PTT fault is not transient the way a busy rig is — retrying just spends
    /// cycles while the operator learns nothing.
    PttFault,
}

/// Max data retransmissions after the first send (so up to `MAX_RETRIES + 1` sends) when the peer is
/// actively NACKing — i.e. present but unable to decode.
const MAX_RETRIES: usize = 3;
/// Consecutive full ACK-timeouts (no reply at all) after which the send is abandoned: the peer is
/// silent, so further data retransmissions are futile and would keep the daemon's single `select!`
/// control loop blocked for a full ACK window each (up to 9 s on the MFSK16 sub-floor). One retry
/// covers a lost ACK on a live link; a second silent window means give up. Bounds the silent-peer
/// stall to `MAX_SILENT_ACK_WINDOWS` windows instead of `MAX_RETRIES + 1` (audit #917 / finding #1).
const MAX_SILENT_ACK_WINDOWS: usize = 2;

/// Why the OTA send/ACK retry loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OtaSendStop {
    /// The peer ACKed — delivered.
    Delivered,
    /// The peer was silent for `MAX_SILENT_ACK_WINDOWS` consecutive windows — abandoned (link down).
    PeerSilent,
    /// The peer kept NACKing through `MAX_RETRIES` retransmissions — abandoned (present but undecodable).
    RetriesExhausted,
    /// No active OTA session to send under.
    NoSession,
    /// The transmitter could not be keyed — abandoned, and NOT delivered (#1295).
    PttFault,
}

/// Drive the OTA send/ACK retry loop, invoking `attempt` (transmit + ACK-wait, with its own side
/// effects) up to the retry budget. Stops on delivery, after `MAX_RETRIES` data retransmissions to a
/// NACKing peer, or after `MAX_SILENT_ACK_WINDOWS` consecutive silent windows. `attempt` returns `None`
/// when it cannot send at all (no active OTA session). Pure policy — unit-tested with scripted outcomes
/// so the silent-peer bound doesn't need real ACK timing.
fn run_ota_retry(mut attempt: impl FnMut() -> Option<OtaAttempt>) -> OtaSendStop {
    let mut consecutive_timeouts = 0usize;
    for _ in 0..=MAX_RETRIES {
        match attempt() {
            None => return OtaSendStop::NoSession,
            Some(OtaAttempt::Delivered) => return OtaSendStop::Delivered,
            Some(OtaAttempt::PttFault) => return OtaSendStop::PttFault,
            Some(OtaAttempt::Nack) => consecutive_timeouts = 0,
            Some(OtaAttempt::Timeout) => {
                consecutive_timeouts += 1;
                if consecutive_timeouts >= MAX_SILENT_ACK_WINDOWS {
                    return OtaSendStop::PeerSilent;
                }
            }
        }
    }
    OtaSendStop::RetriesExhausted
}

fn ota_send_with_ptt(
    engine: &mut ModemEngine,
    ptt: &crate::ptt::SharedPtt,
    event_tx: &std::sync::Arc<tokio::sync::broadcast::Sender<crate::protocol::ControlEvent>>,
    body: &[u8],
    peer: &str,
) {
    use openpulse_core::ack::{AckFrame, AckType};
    // Only adopt an ACK carrying the addressed peer's session hash — the IRS builds its ACK with its own
    // callsign as the session id, so a correctly-addressed send matches while a co-channel session's ACK is
    // filtered (else the ISS could adopt a foreign rate and mark this message delivered).
    let expected_hash = (!peer.is_empty()).then(|| AckFrame::hash_session_id(peer));
    // Mode-scaled ACK window: the sub-floor K=3 MFSK16-ACK (~5 s) needs longer than a 4 s FSK4 ACK.
    // It is a maximum — union-listen returns on the first success, so a healthy link is not slowed.
    let ack_timeout_ms = engine.ota_ack_timeout_ms();

    // Payload-capacity guard: a body over one MFSK16 frame can't ride the SL1 sub-floor rung, and bumping
    // it to a faster rung is futile in a genuine sub-floor fade (that rung won't decode either), so surface
    // it once and skip rather than burn PTT on doomed retransmissions. The message needs the link to
    // recover to a higher rung; the sub-floor rung is for short (≤ 209 B) traffic.
    if !engine.ota_payload_fits_tx_rung(body.len()) {
        tracing::warn!(
            bytes = body.len(),
            "OTA send skipped: body exceeds the MFSK16 sub-floor rung capacity ({} B); \
             waiting for the link to climb off SL1",
            openpulse_modem::engine::ModemEngine::MFSK16_OTA_MAX_PAYLOAD
        );
        let _ = event_tx.send(ota_status_event(engine));
        return;
    }

    let stop = run_ota_retry(|| {
        let mode = engine.ota_tx_mode().map(|m| m.to_owned())?; // no OTA session → stop
                                                                // Free Rs → RsStrong strengthening for small frames (see engine transmit path / #934).
        let fec = openpulse_core::fec::free_rs_strengthening(
            engine.ota_tx_fec(),
            body.len() + openpulse_core::frame::Frame::WIRE_OVERHEAD,
        );

        // Key PTT for the data frame (REQ-PTT-01). The guard is owned by `keyed_transmit` and drops
        // when it returns — i.e. BEFORE the ACK listen below, which is the half-duplex turnaround this
        // path needs; on a panic inside the closure it releases during unwind rather than leaving the
        // rig keyed.
        //
        // NOTE: the assert-failure arm returns `Delivered`, which stops the retry loop and drops the
        // operator's message with only a log line. The comment here used to say "already surfaced as
        // a warn + event"; that is false — `SharedPtt::key` emits no event on failure. Tracked
        // separately rather than changed under a keying PR.
        let tx = match crate::keyed_transmit(ptt, Some(event_tx), "ota-send", || {
            tokio::task::block_in_place(|| engine.transmit_with_fec_mode(body, &mode, fec, None))
        }) {
            Ok(()) => Ok(()),
            // #1295: NOT `Delivered`. Stopping is right — a rig that cannot be keyed will not be
            // keyed by trying again — but `Delivered` is consumed as "the peer has it": the session
            // advances and the rate controller sees a success, for a frame that never went out.
            // Before #1285 this arm was rarely reached, because an unreachable backend collapsed to
            // `None` and keying then SUCCEEDED; now every attempt against a dead rig lands here.
            Err(crate::KeyedTxError::Assert) => return Some(OtaAttempt::PttFault),
            // Busy, not broken (#1263): nothing went out, but the condition is transient, so retry
            // the data frame rather than stopping as an assert fault does.
            Err(crate::KeyedTxError::AlreadyKeyed) => return Some(OtaAttempt::Nack),
            Err(crate::KeyedTxError::Transmit(e)) => Err(e),
        };

        if let Err(e) = tx {
            tracing::warn!(error = %e, "OTA data transmit failed");
            // A local TX failure isn't the peer being silent; retry the data frame.
            return Some(OtaAttempt::Nack);
        }

        // Listen for the ACK with PTT down; adopt the peer's recommended level. Union-listen (FSK4 +
        // K=3 MFSK16-ACK) when the profile carries the sub-floor rung, so an SL1 boundary can't desync.
        match tokio::task::block_in_place(|| {
            engine.receive_ota_ack_within(None, ack_timeout_ms, expected_hash)
        }) {
            Ok(ack) => {
                engine.apply_ota_ack(&ack);
                let _ = event_tx.send(ota_status_event(engine));
                if ack.ack_type == AckType::Nack {
                    Some(OtaAttempt::Nack)
                } else {
                    Some(OtaAttempt::Delivered)
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "OTA ACK not received within window");
                Some(OtaAttempt::Timeout)
            }
        }
    });

    // Surface a give-up once so the operator isn't left thinking the message sent (a silent peer is
    // the case this bail exists for: it stops the control loop being blocked for further doomed windows).
    match stop {
        OtaSendStop::PeerSilent => {
            tracing::warn!(
                peer = %peer,
                "OTA send abandoned: peer silent for {MAX_SILENT_ACK_WINDOWS} consecutive ACK windows; \
                 the link may be down — not blocking the control loop on further retransmissions"
            );
            let _ = event_tx.send(ota_status_event(engine));
        }
        OtaSendStop::RetriesExhausted => {
            tracing::warn!(peer = %peer, "OTA send: retries exhausted (peer kept NACKing)");
            let _ = event_tx.send(ota_status_event(engine));
        }
        OtaSendStop::PttFault => {
            // Surfaced at `error`, not `warn`: the other two give-ups describe the far end, which the
            // operator cannot fix. This one is THIS station's transmitter, and it is actionable.
            tracing::error!(
                peer = %peer,
                "OTA send abandoned: the transmitter could not be keyed — nothing was radiated. \
                 Check [modem] ptt_backend and the rig; the daemon keeps retrying the connection"
            );
            let _ = event_tx.send(crate::protocol::ControlEvent::CommandError {
                command: "ota_send".to_string(),
                reason: "PTT could not be keyed; the message was NOT transmitted".to_string(),
            });
            let _ = event_tx.send(ota_status_event(engine));
        }
        OtaSendStop::Delivered | OtaSendStop::NoSession => {}
    }
}

/// Select an audio backend based on the config string.
///
/// `"cpal"` and `"default"` use [`CpalBackend`] when the `cpal` feature is compiled in.
/// All other values (and `"cpal"`/`"default"` without the feature) fall back to
/// [`LoopbackBackend`].  Production builds should be compiled with `--features cpal`.
/// Load the control-channel PSK from `OPENPULSE_CONTROL_PSK` (64 hex chars = 32 bytes).
///
/// This is the initial, testable source; keystore-backed loading (`openpulse-keystore`) is the
/// production follow-up. Returns `Ok(None)` when the variable is unset.
/// The startup warning owed to an operator who set `psk_key_id`, or `None` if they did not.
///
/// `psk_key_id` is deserialized, defaulted, and written into the shipped config template — and
/// read by nothing (#1234). Setting it was a SILENT no-op, so an operator could believe their PSK
/// came from a keystore while it came from the environment or was absent entirely. This does not
/// make the knob work; it makes its inertness audible until the keystore is wired or the field is
/// removed. Split out of `run()` so the decision is testable without starting a daemon — the
/// emission itself is not covered, only what is decided.
fn inert_psk_key_id_warning(cfg: &ControlSecurityConfig) -> Option<String> {
    if cfg.psk_key_id == ControlSecurityConfig::default().psk_key_id {
        return None;
    }
    Some(format!(
        "[control_security] psk_key_id = {:?} is set, but keystore-backed PSK loading is not \
         implemented (#1234) — nothing reads this field; the PSK is taken from \
         OPENPULSE_CONTROL_PSK only",
        cfg.psk_key_id
    ))
}

fn load_control_psk() -> Result<Option<[u8; openpulse_linksec::PSK_LEN]>, String> {
    let hex = match std::env::var("OPENPULSE_CONTROL_PSK") {
        Ok(h) => h,
        Err(_) => return Ok(None),
    };
    let hex = hex.trim();
    if hex.len() != openpulse_linksec::PSK_LEN * 2 {
        return Err(format!(
            "OPENPULSE_CONTROL_PSK must be {} hex chars (32 bytes)",
            openpulse_linksec::PSK_LEN * 2
        ));
    }
    let mut out = [0u8; openpulse_linksec::PSK_LEN];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| "OPENPULSE_CONTROL_PSK is not valid hex".to_string())?;
    }
    Ok(Some(out))
}

pub fn build_audio_backend(backend: &str) -> Box<dyn AudioBackend> {
    #[cfg(feature = "cpal")]
    {
        use openpulse_audio::CpalBackend;
        if matches!(backend, "cpal" | "default") {
            return Box::new(CpalBackend::new());
        }
    }
    if matches!(backend, "cpal" | "default") {
        tracing::warn!(
            backend,
            "cpal audio backend requested but not compiled in (missing --features cpal); using loopback"
        );
    } else if backend != "loopback" {
        tracing::warn!(backend, "unknown audio backend; using loopback");
    }
    Box::new(LoopbackBackend::default())
}

/// CAT controller backend chosen at startup. A concrete enum (not a `Box<dyn>`) so the daemon can
/// reborrow it each loop iteration without the trait-object `Drop`-in-loop borrow-checker snag.
pub enum CatBackend {
    /// hamlib `rigctld` over TCP.
    Rigctld(RigctldController),
    /// TOML-scripted serial CAT (Unix; `generic-serial` feature).
    #[cfg(feature = "generic-serial")]
    Generic(openpulse_radio::GenericSerialCat),
}

impl CatController for CatBackend {
    fn set_frequency(&mut self, hz: u64) -> Result<(), openpulse_radio::RadioError> {
        match self {
            CatBackend::Rigctld(c) => c.set_frequency(hz),
            #[cfg(feature = "generic-serial")]
            CatBackend::Generic(c) => c.set_frequency(hz),
        }
    }
    fn get_frequency(&mut self) -> Result<u64, openpulse_radio::RadioError> {
        match self {
            CatBackend::Rigctld(c) => c.get_frequency(),
            #[cfg(feature = "generic-serial")]
            CatBackend::Generic(c) => c.get_frequency(),
        }
    }
    fn set_mode(
        &mut self,
        mode: &openpulse_radio::RigMode,
    ) -> Result<(), openpulse_radio::RadioError> {
        match self {
            CatBackend::Rigctld(c) => c.set_mode(mode),
            #[cfg(feature = "generic-serial")]
            CatBackend::Generic(c) => c.set_mode(mode),
        }
    }
}

/// Build the CAT (frequency/mode) controller selected by `[radio] cat_backend`:
/// `"none"` → no CAT; `"generic"` → TOML-scripted serial (requires the `generic-serial` feature);
/// anything else → rigctld over TCP. Returns `None` (manual tuning) on a connect/open failure.
pub fn build_cat_controller(radio: &openpulse_config::RadioConfig) -> Option<CatBackend> {
    match radio.cat_backend.to_ascii_lowercase().as_str() {
        "none" => {
            tracing::info!("CAT disabled (cat_backend = \"none\"); manual frequency control");
            None
        }
        "generic" => {
            #[cfg(feature = "generic-serial")]
            {
                match openpulse_radio::GenericSerialCat::open(&radio.serial_port, &radio.rig_file) {
                    Ok(c) => {
                        tracing::info!(port = %radio.serial_port, rig_file = %radio.rig_file,
                            "generic serial CAT backend opened");
                        Some(CatBackend::Generic(c))
                    }
                    Err(err) => {
                        tracing::warn!(port = %radio.serial_port, error = %err,
                            "generic serial CAT open failed; set_freq commands will emit command_error");
                        None
                    }
                }
            }
            #[cfg(not(feature = "generic-serial"))]
            {
                tracing::warn!(
                    "cat_backend = \"generic\" requires the `generic-serial` build feature; \
                     CAT disabled (manual frequency control)"
                );
                None
            }
        }
        _ => match RigctldController::connect(&radio.rigctld_addr) {
            Ok(controller) => Some(CatBackend::Rigctld(controller)),
            Err(err) => {
                tracing::warn!(addr = %radio.rigctld_addr, error = %err,
                    "rigctld connect failed; set_freq commands will emit command_error");
                None
            }
        },
    }
}

/// The five front-end toggles, read from the engine and runtime state rather than mirrored (#1276).
///
/// One function so every emitter reports the same source of truth. `logbook` lives in
/// `RuntimeControlState`; the other four are engine state.
fn front_end_state(
    engine: &ModemEngine,
    runtime_state: &RuntimeControlState,
) -> crate::FrontEndState {
    crate::FrontEndState {
        notch: engine.is_notch_enabled(),
        agc: engine.is_agc_enabled(),
        cessb: engine.cessb_enabled(),
        logbook: runtime_state.logbook.is_enabled(),
        dcd_squelch: engine.dcd_squelch(),
    }
}

/// Why `[radio.rig_b]` cannot drive a second transmitter, or `None` if it can (#1260).
///
/// Both `RigConfig::default()` and `RadioConfig::default()` carry `rigctld_addr = "127.0.0.1:4532"`,
/// and `rig_b` is `#[serde(default)]`. So an operator who writes a `[radio.rig_b]` header without an
/// address — or leaves the config template's commented-out line commented — gets rig_b pointed at
/// the SAME rigctld as the main rig. Two `SharedPtt`s would then each be certain they own one
/// transmitter: the daemon's guard drop sends `T 0` under the repeater's frame and vice versa, and
/// neither watchdog can see the other's key. #1263's refusal rule protects only *within* one
/// `SharedPtt`, so it cannot reach this — the construction site has to.
/// Why `[repeater] tx_device` cannot be used, or `None` when it is fine.
///
/// One sound card cannot carry two capture streams (#1007), and rig_b's engine captures as well as
/// transmits — `set_default_device` names the device for `open_input` and `open_output` alike, which
/// is what lets carrier sense read rig_b's band (#1325). So pointing `tx_device` at the main rig's
/// card is the same class of error as pointing `[radio.rig_b] rigctld_addr` at the main rig's
/// rigctld, and is refused the same way: hard at startup when the repeater is enabled, a warning
/// otherwise.
///
/// Empty is not an error here — it means "OS default" and is warned about at the call site, because
/// unlike a collision it is merely *probably* wrong rather than certainly so.
fn repeater_tx_device_config_error(tx_device: &str, audio_device: &str) -> Option<String> {
    if tx_device.is_empty() || tx_device != audio_device {
        return None;
    }
    Some(format!(
        "[repeater] tx_device = \"{tx_device}\" is the same device as [audio] device — rig_b would \
         share one sound card with the main rig, which cannot carry two capture streams"
    ))
}

fn repeater_rig_b_config_error(
    rig_b_backend: &str,
    rig_b_addr: &str,
    main_ptt_backend: &str,
    main_cat_backend: &str,
    main_addr: &str,
) -> Option<String> {
    // The shared thing is the rigctld ENDPOINT, not the PTT backend: `[radio] rigctld_addr` is the
    // main rig's CAT endpoint whenever `cat_backend = "rigctld"` (the default), and rigctld keys the
    // rig over CAT. So `ptt_backend = "vox"` with a defaulted `[radio.rig_b]` still puts rig_b's
    // `T 1` on the main transmitter — invisible to both `SharedPtt`s, since neither controller is
    // even involved.
    let main_uses_rigctld = main_ptt_backend == "rigctld" || main_cat_backend == "rigctld";
    if rig_b_backend == "rigctld" && main_uses_rigctld && rig_b_addr == main_addr {
        return Some(format!(
            "[radio.rig_b] rigctld_addr = {rig_b_addr} is the rigctld the main rig already uses \
             ([radio] rigctld_addr, reached by cat_backend = \"{main_cat_backend}\" / ptt_backend = \
             \"{main_ptt_backend}\") — the repeater and the main transmitter would key and release \
             each other's PTT"
        ));
    }
    // Deliberately a string comparison: it catches the shipped defaults, which is the case this
    // exists for, and NOT an aliasing spelling such as `localhost:4532` against `127.0.0.1:4532`.
    // Resolving both sides would need a DNS lookup at startup for a config the operator wrote by
    // hand; the honest scope is "the default collision", not "every way to name one endpoint".
    None
}

fn build_ptt_controller(
    backend: &str,
    rigctld_addr: &str,
    ptt_device: &str,
    ptt_gpio: u8,
) -> Option<Box<dyn PttController + Send>> {
    // Thin adapter over the ONE builder (#1258, `openpulse_radio::ptt_builder`). This crate used to
    // carry its own seven-arm match; ARDOP carried a three-arm one, the CLI an eight-arm one, and
    // KISS none — which is how `rts`/`dtr`/`cm108`/`gpio` came to be silently unavailable on the
    // TNCs while the config documents them as shared.
    //
    // Semantics are preserved EXACTLY: every failure still collapses to `None`, so this dedupe
    // changes no behaviour. That collapse is itself a defect — it makes "the operator asked for no
    // PTT" and "the PTT the operator asked for is unusable" the same state, so a mistyped
    // `ptt_backend` starts a daemon that transmits into an unkeyed rig. Fixed in #1285, separately
    // and on purpose: a deduplication must not quietly alter a caller's contract.
    let spec = openpulse_radio::ptt_builder::PttSpec {
        backend,
        rigctld_addr,
        device: ptt_device,
        gpio_pin: ptt_gpio,
    };
    match openpulse_radio::ptt_builder::build_ptt(&spec) {
        // `"none"` maps to `Some(NoOpPtt)`, NOT to `None` — `SharedPtt` is handed the result and the
        // two are not interchangeable. The daemon's own `none_and_vox_build_a_controller` test caught
        // the drift when this adapter first passed the builder's `Ok(None)` straight through.
        Ok(None) => Some(openpulse_radio::ptt_builder::no_ptt()),
        Ok(ctrl) => ctrl,
        // A CONFIG error cannot self-heal: an unknown backend name or a feature that is not compiled
        // in is a typo the operator must fix, so the caller refuses to start (#1285). Returning
        // `None` here would be the old fail-open, and `None` means "key succeeds, transmit" —
        // which is how a mistyped `ptt_backend` came to play audio into an unkeyed rig.
        Err(openpulse_radio::PttError::Config(_)) => None,
        // A REAL backend that is merely unreachable gets a controller that refuses every key and
        // keeps trying (#1285). It must not collapse to `None`: the rig may simply not be powered up
        // yet, and refusing to start would make the daemon lose a systemd ordering race — but
        // carrying on with no controller would transmit unkeyed, which is the same harm the config
        // case has and the reason "warn and continue" was not enough on its own.
        Err(e) => {
            tracing::warn!(
                backend,
                error = %e,
                "PTT backend unreachable; every emission will be REFUSED until it reconnects"
            );
            Some(Box::new(openpulse_radio::ptt_builder::RetryingPtt::new(
                openpulse_radio::ptt_builder::OwnedPttSpec {
                    backend: backend.to_string(),
                    rigctld_addr: rigctld_addr.to_string(),
                    device: ptt_device.to_string(),
                    gpio_pin: ptt_gpio,
                },
            )))
        }
    }
}

#[cfg(test)]
mod ota_retry_tests {
    use super::{run_ota_retry, OtaAttempt, OtaSendStop, MAX_RETRIES, MAX_SILENT_ACK_WINDOWS};

    /// Drive `run_ota_retry` with a scripted sequence of attempt outcomes; asserts the loop makes the
    /// right number of attempts and stops for the right reason. `None` in the script (or running off the
    /// end) means "no more attempts scripted".
    fn drive(script: &[Option<OtaAttempt>]) -> (usize, OtaSendStop) {
        let mut i = 0;
        let stop = run_ota_retry(|| {
            let out = script.get(i).copied().flatten();
            i += 1;
            out
        });
        (i, stop)
    }

    #[test]
    fn silent_peer_bails_after_two_windows_not_the_full_retry_budget() {
        // A dead peer times out every window. Without the bail this ran MAX_RETRIES+1 (=4) windows
        // (~36 s on MFSK16); now it stops after MAX_SILENT_ACK_WINDOWS.
        let (attempts, stop) = drive(&[Some(OtaAttempt::Timeout); 8]);
        assert_eq!(stop, OtaSendStop::PeerSilent);
        assert_eq!(attempts, MAX_SILENT_ACK_WINDOWS);
        assert!(
            attempts < MAX_RETRIES + 1,
            "must bail before the full retry budget"
        );
    }

    #[test]
    fn nacking_peer_gets_the_full_retry_budget() {
        // A present-but-undecodable peer NACKs every window — it is NOT cut off early (it is alive).
        let (attempts, stop) = drive(&[Some(OtaAttempt::Nack); 8]);
        assert_eq!(stop, OtaSendStop::RetriesExhausted);
        assert_eq!(attempts, MAX_RETRIES + 1);
    }

    #[test]
    fn a_nack_resets_the_silent_counter() {
        // Timeout, then a NACK (peer reappears) resets the counter, so a subsequent single timeout does
        // not immediately abandon — the peer proved it is alive.
        let (attempts, stop) = drive(&[
            Some(OtaAttempt::Timeout),
            Some(OtaAttempt::Nack),
            Some(OtaAttempt::Timeout),
            Some(OtaAttempt::Timeout),
        ]);
        assert_eq!(stop, OtaSendStop::PeerSilent);
        assert_eq!(attempts, 4); // not abandoned at the first post-NACK timeout
    }

    #[test]
    fn delivered_on_first_ack_stops_immediately() {
        let (attempts, stop) = drive(&[Some(OtaAttempt::Delivered)]);
        assert_eq!(stop, OtaSendStop::Delivered);
        assert_eq!(attempts, 1);
    }

    /// A PTT fault stops the send WITHOUT claiming delivery (#1295).
    ///
    /// The two halves are the point. Stopping is right — a rig that cannot be keyed will not be
    /// keyed by trying again, and burning the retry budget only delays the operator finding out. But
    /// `Delivered` is consumed as "the peer has it": the session advances and the rate controller
    /// records a success for a frame that never went out.
    ///
    /// Before #1285 this arm was almost unreachable, because an unreachable backend collapsed to
    /// `None` and `SharedPtt::key` with no controller SUCCEEDS — the daemon transmitted unkeyed and
    /// the session failed later, on a missing ACK. Now every attempt against a dead rig lands here,
    /// which is why the outcome had to stop being a lie.
    #[test]
    fn a_ptt_fault_stops_the_send_without_claiming_delivery() {
        let (attempts, stop) = drive(&[Some(OtaAttempt::PttFault)]);
        assert_eq!(
            stop,
            OtaSendStop::PttFault,
            "a keying failure must be its own stop reason"
        );
        assert_ne!(
            stop,
            OtaSendStop::Delivered,
            "reporting Delivered for a frame that was never radiated advances the session and              records a rate-controller success on nothing"
        );
        assert_eq!(attempts, 1, "and it must not burn the retry budget");
    }

    /// Control: a real ACK still reports delivery, so the test above is not passing because the
    /// driver stopped reporting delivery at all.
    #[test]
    fn a_real_ack_still_reports_delivered() {
        let (_, stop) = drive(&[Some(OtaAttempt::Delivered)]);
        assert_eq!(stop, OtaSendStop::Delivered);
    }

    #[test]
    fn no_session_stops_without_sending() {
        let (attempts, stop) = drive(&[None]);
        assert_eq!(stop, OtaSendStop::NoSession);
        assert_eq!(attempts, 1); // the closure was polled once and reported "no session"
    }
}

#[cfg(test)]
mod ptt_selector_tests {
    use super::build_ptt_controller;

    #[test]
    fn none_and_vox_build_a_controller() {
        assert!(build_ptt_controller("none", "", "", 3).is_some());
        assert!(build_ptt_controller("vox", "", "", 3).is_some());
    }

    /// A device that is MISSING may appear later, so it refuses and retries rather than disabling.
    ///
    /// **CHANGED INTENT at #1285, not a test bent to fit.** This asserted `is_none()` — "disables
    /// PTT, not a crash" — and `None` is precisely the state in which `SharedPtt::key` skips the
    /// hardware assert, succeeds, and lets the caller transmit. So "gracefully disabled" meant the
    /// daemon played audio into an unkeyed rig believing it had transmitted. A CM108 adapter is USB:
    /// it can be plugged in after the daemon starts, which is why this is the retrying case and not
    /// the refuse-to-start case.
    #[test]
    fn cm108_with_a_missing_device_refuses_and_retries() {
        let ctrl = build_ptt_controller("cm108", "", "/dev/nonexistent-openpulse-hidraw-xyz", 3);
        let mut ctrl = ctrl.expect("a missing device must yield a REFUSING controller, not None");
        assert!(
            ctrl.assert_ptt().is_err(),
            "a missing CM108 device must refuse the key — None would have permitted an unkeyed \
             transmit, which is the #1285 defect"
        );
    }

    /// A spec that cannot PARSE refuses at startup; a device that is merely absent retries.
    #[test]
    fn a_malformed_gpio_spec_refuses_startup_but_an_absent_chip_retries() {
        // Unparseable, or the feature is not compiled in: a config error, which cannot self-heal,
        // so the selector returns None and `run` refuses to start on it.
        assert!(
            build_ptt_controller("gpio", "", "not-a-valid-spec", 3).is_none(),
            "a malformed GPIO spec is a config error — it must reach the startup refusal"
        );
        // An EMPTY spec is also unparseable, so it takes the same path.
        assert!(build_ptt_controller("gpio", "", "", 3).is_none());
    }

    #[test]
    fn unknown_backend_disables_ptt() {
        assert!(build_ptt_controller("nonsense", "", "", 3).is_none());
    }
}

#[cfg(test)]
mod burst_planning_tests {
    use super::{plan_bursts, MAX_FRAGS_PER_BURST};

    #[test]
    fn empty_queue_plans_nothing() {
        assert!(plan_bursts(0, |_| 1.0, 20.0, MAX_FRAGS_PER_BURST).is_empty());
    }

    #[test]
    fn small_transfer_fits_one_burst() {
        // 5 fragments × 2 s = 10 s ≤ 20 s budget → a single burst.
        assert_eq!(plan_bursts(5, |_| 2.0, 20.0, MAX_FRAGS_PER_BURST), vec![5]);
    }

    #[test]
    fn airtime_budget_splits_into_multiple_bursts() {
        // Each fragment 6 s, budget 20 s → 3 per burst (18 s), 10 fragments → 3+3+3+1.
        assert_eq!(
            plan_bursts(10, |_| 6.0, 20.0, MAX_FRAGS_PER_BURST),
            vec![3, 3, 3, 1]
        );
    }

    #[test]
    fn oversized_fragment_still_forms_its_own_burst() {
        // A single 50 s fragment exceeds the 20 s budget but must still be sent (never a zero burst).
        assert_eq!(
            plan_bursts(3, |_| 50.0, 20.0, MAX_FRAGS_PER_BURST),
            vec![1, 1, 1]
        );
    }

    #[test]
    fn fragment_count_is_clamped_even_when_airtime_is_tiny() {
        // Negligible airtime would pack everything, but max_frags caps each burst.
        assert_eq!(
            plan_bursts(150, |_| 0.001, 20.0, MAX_FRAGS_PER_BURST),
            vec![64, 64, 22]
        );
    }

    #[test]
    fn per_fragment_airtime_is_respected() {
        // Mixed sizes: 15 s, then 10 s (25 > 20 → new burst), then 3 s (13 ≤ 20 packs with the 10 s).
        let secs = [15.0, 10.0, 3.0];
        assert_eq!(
            plan_bursts(3, |i| secs[i], 20.0, MAX_FRAGS_PER_BURST),
            vec![1, 2]
        );
    }
}

#[cfg(test)]
mod discovery_tick_tests {
    use super::*;
    use crate::protocol::ControlEvent;
    use openpulse_audio::LoopbackBackend;
    use openpulse_discovery::{DiscoveryParams, DiscoveryRuntime, DiscoveryState, Submode, TxMode};
    use openpulse_radio::PttController;

    /// A NORMAL slot of audio with one heartbeat (KN4CRD EM73) at 1500 Hz (C-2 upstream vector).
    fn heartbeat_slot() -> Vec<f32> {
        use js8_plugin::costas::CostasKind;
        use js8_plugin::message::js8_info_bits;
        use js8_plugin::modulate::{modulate_tones, GfskParams};
        use js8_plugin::submode::params;
        use js8_plugin::tones::message_to_tones;
        let payload: [u8; 9] = [0x0a, 0x2f, 0xb3, 0xa3, 0xee, 0x2e, 0xe2, 0xea, 0x58];
        let info = js8_info_bits(&payload, 0);
        let sm = params(js8_plugin::submode::Submode::Normal);
        modulate_tones(
            &message_to_tones(&info, CostasKind::Original),
            1500.0,
            &GfskParams::from_submode(&sm),
        )
    }

    #[test]
    fn discovery_tick_activates_dwells_and_hears_an_injected_station() {
        let mut engine = ModemEngine::new(Box::new(LoopbackBackend::new()));
        let _ = &mut engine; // hpx_state() defaults to Idle → idle predicate holds
        let (tx, mut rx) = tokio::sync::broadcast::channel::<ControlEvent>(64);
        let ev = std::sync::Arc::new(tx);
        let mut rs = RuntimeControlState {
            last_freq_hz: Some(14_074_000), // a home frequency to save/restore
            discovery: Some(DiscoveryRuntime::new(DiscoveryParams {
                enabled: true,
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
            })),
            ..RuntimeControlState::default()
        };

        // Tick 1: idle → activate → (no rig) retune ok → dwell.
        discovery_tick(&mut rs, &engine, None, &ev, &[], 1000);
        assert_eq!(
            rs.discovery.as_ref().unwrap().state(),
            DiscoveryState::Dwelling
        );
        assert_eq!(rs.discovery_home_freq_hz, Some(14_074_000), "home saved");
        assert_eq!(
            rs.last_freq_hz,
            Some(14_078_000),
            "tuned to the JS8 calling freq"
        );

        // Tick 2 (same slot): buffer the heartbeat audio.
        discovery_tick(&mut rs, &engine, None, &ev, &heartbeat_slot(), 1000);
        // Tick 3: next UTC slot → decode → StationHeard.
        discovery_tick(&mut rs, &engine, None, &ev, &[], 16_000);

        // The station is cached and a StationHeard event was emitted.
        assert!(rs
            .discovery
            .as_ref()
            .unwrap()
            .stations()
            .get("KN4CRD")
            .is_some());
        let mut heard = false;
        while let Ok(e) = rx.try_recv() {
            if let ControlEvent::StationHeard { callsign, grid, .. } = e {
                if callsign == "KN4CRD" && grid == "EM73" {
                    heard = true;
                }
            }
        }
        assert!(heard, "a StationHeard event for KN4CRD/EM73 was emitted");
    }

    #[test]
    fn dwelling_predicate_gates_the_dwell_audio_tee() {
        // The rx-tick tees raw capture audio to the weak-signal decoder only while dwelling. Guard the
        // predicate that gates it (deleted/inverted → discovery never sees calling-channel audio, or the
        // DCD pipeline is fed −24 dB JS8 it can't carry). Companion to the inline tee in `server::run`.
        let engine = ModemEngine::new(Box::new(LoopbackBackend::new()));
        let (tx, _rx) = tokio::sync::broadcast::channel::<ControlEvent>(64);
        let ev = std::sync::Arc::new(tx);

        // No discovery runtime → never tee.
        let mut none_rs = RuntimeControlState::default();
        assert!(
            !discovery_is_dwelling(&none_rs),
            "unconfigured never dwells"
        );
        let _ = &mut none_rs;

        let mut rs = RuntimeControlState {
            last_freq_hz: Some(14_074_000),
            discovery: Some(DiscoveryRuntime::new(DiscoveryParams {
                enabled: true,
                idle_grace_ms: 0,
                dwell_ms: 0,
                station_ttl_ms: 3_600_000,
                submode: Submode::Normal,
                calling_freq_hz: 14_078_000,
                tx_mode: TxMode::RxOnly,
                callsign: String::new(),
                grid: String::new(),
                hint: None,
                heartbeat_interval_slots: 8,
                hint_interval_beacons: 3,
                tx_offset_hz: 1500.0,
                max_clock_skew_ms: 2000,
            })),
            ..RuntimeControlState::default()
        };

        // Before the first tick the runtime is Inactive → no tee.
        assert_ne!(
            rs.discovery.as_ref().unwrap().state(),
            DiscoveryState::Dwelling
        );
        assert!(
            !discovery_is_dwelling(&rs),
            "an inactive runtime does not tee audio"
        );

        // One tick activates → dwells on the calling freq → the tee opens.
        discovery_tick(&mut rs, &engine, None, &ev, &[], 1000);
        assert_eq!(
            rs.discovery.as_ref().unwrap().state(),
            DiscoveryState::Dwelling
        );
        assert!(
            discovery_is_dwelling(&rs),
            "a dwelling runtime tees the tick's raw audio"
        );
    }

    #[test]
    fn take_rendezvous_connect_maps_ready_peer_and_consumes_it() {
        // The completed-rendezvous QSY hands off to the signed session by mapping the ready (peer, freq)
        // into a `ConnectPeer` for that peer, consumed once. Guard the mapping + take-once semantics of
        // the inline `server::run` handoff (which then feeds the command to `apply_command_to_engine`).
        use crate::protocol::ControlCommand;

        let mut rs = RuntimeControlState::default();
        assert!(
            take_rendezvous_connect(&mut rs).is_none(),
            "no ready rendezvous → no connect"
        );

        rs.rendezvous_connect_ready = Some(("W1AW".into(), 14_101_000));
        match take_rendezvous_connect(&mut rs) {
            Some(ControlCommand::ConnectPeer { callsign }) => assert_eq!(callsign, "W1AW"),
            other => panic!("expected ConnectPeer for W1AW, got {other:?}"),
        }
        assert!(
            rs.rendezvous_connect_ready.is_none(),
            "the readiness is consumed (take-once) so the handoff fires exactly once"
        );
        assert!(
            take_rendezvous_connect(&mut rs).is_none(),
            "a second poll after consumption yields nothing"
        );
    }

    /// One NORMAL beacon frame (payload9 + its i3bit flag) modulated at 1500 Hz.
    fn beacon_frame(hex: &str, i3bit: u8) -> Vec<f32> {
        use js8_plugin::costas::CostasKind;
        use js8_plugin::message::js8_info_bits;
        use js8_plugin::modulate::{modulate_tones, GfskParams};
        use js8_plugin::submode::params;
        use js8_plugin::tones::message_to_tones;
        let mut p = [0u8; 9];
        for (i, b) in p.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        let info = js8_info_bits(&p, i3bit);
        let sm = params(js8_plugin::submode::Submode::Normal);
        modulate_tones(
            &message_to_tones(&info, CostasKind::Original),
            1500.0,
            &GfskParams::from_submode(&sm),
        )
    }

    #[test]
    fn discovery_tick_transmits_a_beacon_in_beacon_mode() {
        let mut engine = ModemEngine::new(Box::new(LoopbackBackend::new()));
        let (tx, _rx) = tokio::sync::broadcast::channel::<ControlEvent>(64);
        let ev = std::sync::Arc::new(tx);
        let mut rs = RuntimeControlState {
            last_freq_hz: Some(14_074_000),
            discovery: Some(DiscoveryRuntime::new(DiscoveryParams {
                enabled: true,
                idle_grace_ms: 0,
                dwell_ms: 0,
                station_ttl_ms: 3_600_000,
                submode: Submode::Normal,
                calling_freq_hz: 14_078_000,
                tx_mode: TxMode::Beacon,
                callsign: "DC0SK".into(),
                grid: "JN58".into(),
                hint: None,
                heartbeat_interval_slots: 2,
                hint_interval_beacons: 0,
                tx_offset_hz: 1500.0,
                max_clock_skew_ms: 2000,
            })),
            ..RuntimeControlState::default()
        };

        discovery_tick(&mut rs, &engine, None, &ev, &[], 1000); // activate → dwell
        let mut beacon = None;
        let mut t = 1000u64;
        for _ in 0..4 {
            t += 15_000;
            if let Some(b) = discovery_tick(&mut rs, &engine, None, &ev, &[], t) {
                beacon = Some(b);
            }
        }
        let (audio, mode) = beacon.expect("a heartbeat beacon is due in beacon mode");
        assert_eq!(mode, "JS8-NORMAL");
        assert!(!audio.is_empty());

        // The daemon transmits it via the raw-audio seam (no PTT hardware in the test).
        let ptt = crate::ptt::SharedPtt::default();
        transmit_beacon_with_ptt(&mut engine, &ptt, &audio, &mode);
        assert_eq!(engine.raw_audio_frames_transmitted(), 1);
    }

    #[test]
    fn discovery_tick_defers_a_due_beacon_when_the_channel_is_busy() {
        // The DCD gate at the beacon-emit decision must hold the beacon when the calling channel is
        // occupied (don't key over an in-progress QSO). Same beacon-mode setup as the emit test, but
        // with the engine's DCD driven busy first — no beacon frame may be handed back.
        let mut engine = ModemEngine::new(Box::new(LoopbackBackend::new()));
        engine
            .register_plugin(Box::new(BpskPlugin::new()))
            .expect("register BPSK plugin");

        // Trip DCD: loopback echoes the TX into the RX capture, so the received energy marks the
        // channel busy. Nothing in `discovery_tick` feeds the DCD, so the busy state persists.
        engine
            .transmit(b"occupying signal", "BPSK250", None)
            .unwrap();
        let _ = engine.receive("BPSK250", None).unwrap();
        assert!(
            engine.is_channel_busy(),
            "precondition: channel must read busy"
        );

        let (tx, _rx) = tokio::sync::broadcast::channel::<ControlEvent>(64);
        let ev = std::sync::Arc::new(tx);
        let mut rs = RuntimeControlState {
            last_freq_hz: Some(14_074_000),
            discovery: Some(DiscoveryRuntime::new(DiscoveryParams {
                enabled: true,
                idle_grace_ms: 0,
                dwell_ms: 0,
                station_ttl_ms: 3_600_000,
                submode: Submode::Normal,
                calling_freq_hz: 14_078_000,
                tx_mode: TxMode::Beacon,
                callsign: "DC0SK".into(),
                grid: "JN58".into(),
                hint: None,
                heartbeat_interval_slots: 2,
                hint_interval_beacons: 0,
                tx_offset_hz: 1500.0,
                max_clock_skew_ms: 2000,
            })),
            ..RuntimeControlState::default()
        };

        discovery_tick(&mut rs, &engine, None, &ev, &[], 1000); // activate → dwell
        let mut t = 1000u64;
        for _ in 0..4 {
            t += 15_000;
            assert!(
                discovery_tick(&mut rs, &engine, None, &ev, &[], t).is_none(),
                "a busy channel must defer the beacon — none may be transmitted"
            );
        }
    }

    /// A PTT double whose assert and/or release can be made to fail, standing in for a transient
    /// rigctld/serial fault.
    #[derive(Default)]
    struct FlakyPtt {
        fail_assert: bool,
        fail_release: bool,
    }
    impl PttController for FlakyPtt {
        fn assert_ptt(&mut self) -> Result<(), openpulse_radio::PttError> {
            if self.fail_assert {
                Err(openpulse_radio::PttError::Serial("assert failed".into()))
            } else {
                Ok(())
            }
        }
        fn release_ptt(&mut self) -> Result<(), openpulse_radio::PttError> {
            if self.fail_release {
                Err(openpulse_radio::PttError::Serial("stuck keyed".into()))
            } else {
                Ok(())
            }
        }
        fn is_asserted(&self) -> bool {
            false
        }
    }

    #[test]
    fn ptt_command_guard_reports_hardware_failure_to_skip_dispatch() {
        use crate::Command;

        // A failed assert/release must report `true` so the caller skips the engine dispatch and does
        // not emit a spurious PttChanged claiming a state the hardware never reached.
        let failing = crate::ptt::SharedPtt::new(
            Some(Box::new(FlakyPtt {
                fail_assert: true,
                fail_release: true,
            })),
            crate::ptt::DEFAULT_PTT_MAX,
        );
        let (ev, _rx) = tokio::sync::broadcast::channel(16);
        assert!(
            handle_ptt_command(&Command::PttAssert, &failing, &ev),
            "a failed assert must report hard failure"
        );
        assert!(
            handle_ptt_command(&Command::PttRelease, &failing, &ev),
            "a failed release must report hard failure"
        );

        // A successful call reports `false` so the dispatch proceeds and the PttChanged fires.
        let ok = crate::ptt::SharedPtt::new(
            Some(Box::new(FlakyPtt::default())),
            crate::ptt::DEFAULT_PTT_MAX,
        );
        assert!(!handle_ptt_command(&Command::PttAssert, &ok, &ev));
        assert!(!handle_ptt_command(&Command::PttRelease, &ok, &ev));

        // Non-PTT commands and the no-controller case are always no-op passes.
        assert!(!handle_ptt_command(
            &Command::PttAssert,
            &crate::ptt::SharedPtt::default(),
            &ev
        ));
        assert!(!handle_ptt_command(
            &Command::GetConfig,
            &crate::ptt::SharedPtt::new(
                Some(Box::new(FlakyPtt {
                    fail_assert: true,
                    fail_release: true,
                })),
                crate::ptt::DEFAULT_PTT_MAX,
            ),
            &ev
        ));
    }

    #[test]
    fn automatic_tx_arms_the_watchdog_and_disarms_only_on_clean_release() {
        // Audit #5: an automatic keying path must arm the PTT watchdog so a failed release (rig fault)
        // is caught, and disarm it on a clean release so the watchdog never fires spuriously.
        let mut engine = ModemEngine::new(Box::new(LoopbackBackend::new()));
        let audio = vec![0.0f32; 100];

        // Release fails → the transmitter may still be keyed, so the watchdog must stay armed.
        let ptt = crate::ptt::SharedPtt::new(
            Some(Box::new(FlakyPtt {
                fail_release: true,
                ..Default::default()
            })),
            crate::ptt::DEFAULT_PTT_MAX,
        );
        transmit_beacon_with_ptt(&mut engine, &ptt, &audio, "JS8-NORMAL");
        assert!(
            ptt.is_keyed(),
            "a failed release must leave the watchdog armed"
        );

        // Clean release → disarmed, so the watchdog can't fire on a stale timestamp.
        let ptt2 = crate::ptt::SharedPtt::new(
            Some(Box::new(FlakyPtt::default())),
            crate::ptt::DEFAULT_PTT_MAX,
        );
        transmit_beacon_with_ptt(&mut engine, &ptt2, &audio, "JS8-NORMAL");
        assert!(!ptt2.is_keyed(), "a clean release disarms the watchdog");
    }

    #[test]
    fn watchdog_releases_the_transmitter_when_the_deadline_passes() {
        // The decoupled watchdog `select!` arm calls this on its own fast timer, so a client command
        // flood can no longer starve the force-release. Verify it releases + disarms + notifies once.
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingPtt(std::sync::Arc<AtomicUsize>);
        impl PttController for CountingPtt {
            fn assert_ptt(&mut self) -> Result<(), openpulse_radio::PttError> {
                Ok(())
            }
            fn release_ptt(&mut self) -> Result<(), openpulse_radio::PttError> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            fn is_asserted(&self) -> bool {
                false
            }
        }

        let releases = std::sync::Arc::new(AtomicUsize::new(0));
        // Armed with a zero deadline → immediately expired.
        let ptt = crate::ptt::SharedPtt::new(
            Some(Box::new(CountingPtt(releases.clone()))),
            std::time::Duration::ZERO,
        );
        ptt.arm();
        let (tx, mut rx) = tokio::sync::broadcast::channel::<ControlEvent>(8);
        let ev = std::sync::Arc::new(tx);

        assert!(
            ptt.force_release_if_expired(&ev),
            "the watchdog must fire when the deadline has passed"
        );
        assert_eq!(
            releases.load(Ordering::Relaxed),
            1,
            "the watchdog must force-release the keyed transmitter"
        );
        assert!(!ptt.is_keyed(), "the watchdog disarms after firing");
        assert!(
            matches!(
                rx.try_recv(),
                Ok(ControlEvent::PttChanged { active: false })
            ),
            "clients are notified the transmitter was released"
        );

        // Idempotent: nothing armed → no second release.
        assert!(!ptt.force_release_if_expired(&ev));
        assert_eq!(
            releases.load(Ordering::Relaxed),
            1,
            "no spurious release once disarmed"
        );
    }

    #[test]
    fn discovery_tick_recognizes_an_opulse_peer_into_the_shared_cache() {
        // The four Huffman-forced frames of `DC0SK: @OPULSE OPHF1 1FAX3AIT` (Qt5 ground truth).
        let frames = [
            ("2694fa766ea662ea58", 1u8),
            ("531a90d5639ea3f5c8", 0u8),
            ("bfec6491489275029b", 0u8),
            ("b9afffffffffffffff", 2u8),
        ];
        let engine = ModemEngine::new(Box::new(LoopbackBackend::new()));
        let (tx, mut rx) = tokio::sync::broadcast::channel::<ControlEvent>(64);
        let ev = std::sync::Arc::new(tx);
        let mut rs = RuntimeControlState {
            last_freq_hz: Some(14_074_000),
            discovery: Some(DiscoveryRuntime::new(DiscoveryParams {
                enabled: true,
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
            })),
            ..RuntimeControlState::default()
        };

        discovery_tick(&mut rs, &engine, None, &ev, &[], 1000); // activate → dwell
        let mut t = 1000u64;
        for (hex, i3) in frames {
            discovery_tick(&mut rs, &engine, None, &ev, &beacon_frame(hex, i3), t);
            t += 15_000;
            discovery_tick(&mut rs, &engine, None, &ev, &[], t); // cross boundary → decode
        }

        // The recognized peer is in the shared cache with its capabilities.
        let peers = rs
            .peer_cache
            .query(0, 0, openpulse_core::peer_cache::TrustFilter::Any, 16, t);
        let peer = peers
            .iter()
            .find(|p| p.peer_id == "js8:DC0SK")
            .expect("DC0SK cached as an OpenPulse peer");
        assert_eq!(peer.capability_mask, 0xB105);

        // ListPeers surfaces it.
        crate::emit_peer_list(&mut rs, &ev, t);
        let mut listed = false;
        while let Ok(e) = rx.try_recv() {
            if let ControlEvent::PeerList { peers } = e {
                listed = peers.iter().any(|p| p.peer_id == "js8:DC0SK");
            }
        }
        assert!(listed, "ListPeers reported the recognized peer");
    }

    #[test]
    fn discovery_tick_qsys_to_the_current_home_bands_calling_freq() {
        // Home on 40 m; the runtime's initial calling freq is the 20 m default. Activation must
        // re-select the 40 m entry from the band table and QSY there, not to 20 m.
        let engine = ModemEngine::new(Box::new(LoopbackBackend::new()));
        let (tx, _rx) = tokio::sync::broadcast::channel::<ControlEvent>(64);
        let ev = std::sync::Arc::new(tx);
        let calling: std::collections::BTreeMap<String, u64> =
            [("40m", 7_078_000u64), ("20m", 14_078_000)]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
        let mut rs = RuntimeControlState {
            last_freq_hz: Some(7_074_000), // home on 40 m
            discovery_calling_freqs_hz: calling,
            discovery: Some(DiscoveryRuntime::new(DiscoveryParams {
                enabled: true,
                idle_grace_ms: 0,
                dwell_ms: 0,
                station_ttl_ms: 3_600_000,
                submode: Submode::Normal,
                calling_freq_hz: 14_078_000, // 20 m default, must be overridden
                tx_mode: openpulse_discovery::TxMode::RxOnly,
                callsign: String::new(),
                grid: String::new(),
                hint: None,
                heartbeat_interval_slots: 8,
                hint_interval_beacons: 3,
                tx_offset_hz: 1500.0,
                max_clock_skew_ms: 2000,
            })),
            ..RuntimeControlState::default()
        };

        discovery_tick(&mut rs, &engine, None, &ev, &[], 1000);
        assert_eq!(
            rs.discovery.as_ref().unwrap().state(),
            DiscoveryState::Dwelling
        );
        assert_eq!(
            rs.discovery_home_freq_hz,
            Some(7_074_000),
            "40 m home saved"
        );
        assert_eq!(
            rs.last_freq_hz,
            Some(7_078_000),
            "tuned to the 40 m JS8 calling freq, not the 20 m default"
        );
        assert_eq!(
            rs.discovery.as_ref().unwrap().dial_freq_hz(),
            7_078_000,
            "runtime dial freq reflects the per-band selection (drives DiscoveryStatus)"
        );
    }

    #[test]
    fn discovery_tick_is_a_noop_when_unconfigured() {
        let engine = ModemEngine::new(Box::new(LoopbackBackend::new()));
        let (tx, mut rx) = tokio::sync::broadcast::channel::<ControlEvent>(8);
        let ev = std::sync::Arc::new(tx);
        let mut rs = RuntimeControlState::default(); // discovery: None
        discovery_tick(&mut rs, &engine, None, &ev, &[0.0; 1000], 1000);
        assert!(rx.try_recv().is_err(), "no events without discovery");
    }

    /// Audio for one frame of a directed over at 1500 Hz.
    fn directed_frame_audio(f: &js8_plugin::BeaconFrame) -> Vec<f32> {
        js8_plugin::beacon::frame_audio(f, 1500.0, js8_plugin::submode::Submode::Normal)
    }

    #[test]
    fn discovery_tick_responds_to_a_proposal_and_emits_rendezvous_agreed() {
        let mut engine = ModemEngine::new(Box::new(LoopbackBackend::new()));
        let _ = &mut engine;
        let (tx, mut rx) = tokio::sync::broadcast::channel::<ControlEvent>(64);
        let ev = std::sync::Arc::new(tx);
        let mut rs = RuntimeControlState {
            last_freq_hz: Some(14_074_000), // 20 m home
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
                calling_freq_hz: 14_078_000,
                tx_mode: TxMode::Full, // responder role on
                callsign: "DC0SK".into(),
                grid: "JN58".into(),
                hint: None,
                heartbeat_interval_slots: 10_000, // never beacon during the test
                hint_interval_beacons: 0,
                tx_offset_hz: 1500.0,
                max_clock_skew_ms: 2000,
            })),
            ..RuntimeControlState::default()
        };

        // Activate → dwell (sets the responder's available channels from the 20 m table).
        discovery_tick(&mut rs, &engine, None, &ev, &[], 1000);
        assert_eq!(
            rs.discovery.as_ref().unwrap().state(),
            DiscoveryState::Dwelling
        );

        // KN4CRD proposes channels 1 then 0; both are available → agree on index 1 = 14_103_000.
        let frames = js8_plugin::directed("KN4CRD", "JN58", "DC0SK", "OPHF1 QSY? R7 C1 C0");
        let mut t = 1000u64;
        for f in &frames {
            discovery_tick(&mut rs, &engine, None, &ev, &directed_frame_audio(f), t);
            t += 15_000;
            discovery_tick(&mut rs, &engine, None, &ev, &[], t);
        }

        // The responder withholds its agreement until the Accept over has fully transmitted (audit #4b),
        // so tick through the Accept frames until RendezvousAgreed surfaces.
        let mut agreed = None;
        for _ in 0..10 {
            t += 15_000;
            discovery_tick(&mut rs, &engine, None, &ev, &[], t);
            while let Ok(e) = rx.try_recv() {
                if let ControlEvent::RendezvousAgreed { peer, freq_hz } = e {
                    agreed = Some((peer, freq_hz));
                }
            }
            if agreed.is_some() {
                break;
            }
        }
        assert_eq!(agreed, Some(("KN4CRD".to_string(), 14_103_000)));

        // The QSY was scheduled (not fired yet); after the switch delay it retunes + arms the handoff.
        assert!(
            rs.rendezvous_qsy_due.is_some(),
            "QSY scheduled after agreement"
        );
        discovery_tick(&mut rs, &engine, None, &ev, &[], t + 10 * 15_000);
        assert_eq!(
            rs.last_freq_hz,
            Some(14_103_000),
            "retuned to the agreed working frequency"
        );
        assert_eq!(
            rs.rendezvous_connect_ready,
            Some(("KN4CRD".to_string(), 14_103_000)),
            "handoff armed for server::run"
        );
        assert!(rs.rendezvous_qsy_due.is_none(), "schedule consumed");
        assert_eq!(
            rs.discovery.as_ref().unwrap().state(),
            DiscoveryState::Inactive,
            "discovery stood down for the QSO"
        );
    }
}

#[cfg(test)]
mod inert_knob_tests {
    use super::inert_psk_key_id_warning;
    use openpulse_config::ControlSecurityConfig;

    // Deliberately carries NO `VERIFIES:` id. It was first written as REQ-CTL-04's binding, and
    // #1237's reverse check refused it: that would have moved the requirement out of `unwired` on
    // the strength of a test asserting the daemon WARNS the knob is inert — close to the opposite
    // of the statement's content, and the exact false-claim the reverse rule exists to stop. The
    // keystore's own tests remain REQ-CTL-04's evidence; this covers the daemon's honesty about
    // an unread config field (#1234).
    #[test]
    fn a_set_psk_key_id_is_not_silently_ignored() {
        let dflt = ControlSecurityConfig::default();
        assert!(
            inert_psk_key_id_warning(&dflt).is_none(),
            "an untouched default must not nag"
        );

        let set = ControlSecurityConfig {
            psk_key_id: "my-radio-psk".into(),
            ..ControlSecurityConfig::default()
        };
        let msg = inert_psk_key_id_warning(&set)
            .expect("setting psk_key_id must produce a warning while nothing reads it");
        assert!(
            msg.contains("my-radio-psk") && msg.contains("#1234"),
            "the warning must name the value and where the gap is tracked: {msg}"
        );
        assert!(
            msg.contains("OPENPULSE_CONTROL_PSK"),
            "and must say where the PSK actually comes from: {msg}"
        );
    }
}

#[cfg(test)]
mod cat_backend_tests {
    use super::build_cat_controller;
    use openpulse_config::RadioConfig;

    #[test]
    fn cat_backend_none_yields_no_controller() {
        let radio = RadioConfig {
            cat_backend: "none".into(),
            ..RadioConfig::default()
        };
        assert!(build_cat_controller(&radio).is_none());
    }

    #[test]
    fn cat_backend_generic_without_a_rig_file_yields_no_controller() {
        // With the feature off this warns and returns None; with it on, opening an empty
        // serial_port/rig_file fails and also returns None. Either way: no controller, no panic.
        let radio = RadioConfig {
            cat_backend: "generic".into(),
            serial_port: String::new(),
            rig_file: String::new(),
            ..RadioConfig::default()
        };
        assert!(build_cat_controller(&radio).is_none());
    }
}

#[cfg(test)]
mod ws_auth_gate_tests {
    use super::ws_disabled_for_auth;

    #[test]
    fn ws_disabled_when_tcp_requires_auth() {
        // TCP auth on (non-loopback TCP bind or require_auth) → WS must be disabled even if WS is loopback.
        assert!(ws_disabled_for_auth(true, "127.0.0.1"));
        assert!(ws_disabled_for_auth(true, "0.0.0.0"));
    }

    #[test]
    fn ws_disabled_when_ws_bind_is_non_loopback() {
        // Even if the TCP port is unauthenticated loopback, a publicly-bound WS port is a bypass → disable.
        assert!(ws_disabled_for_auth(false, "0.0.0.0"));
        assert!(ws_disabled_for_auth(false, "192.168.1.10"));
    }

    #[test]
    fn ws_enabled_only_when_both_are_loopback_and_no_auth() {
        // The one safe case: no TCP auth required and the WS port is loopback-only.
        assert!(!ws_disabled_for_auth(false, "127.0.0.1"));
        assert!(!ws_disabled_for_auth(false, "localhost"));
    }
}

#[cfg(test)]
mod repeater_rig_b_tests {
    use super::{repeater_rig_b_config_error, repeater_tx_device_config_error};
    use openpulse_config::{RadioConfig, RigConfig};

    /// #1308 PR 3: rig_b sharing the main rig's sound card is #1007's rule, one device over.
    #[test]
    fn a_tx_device_equal_to_the_main_audio_device_is_refused() {
        let why = repeater_tx_device_config_error("plughw:1,0", "plughw:1,0")
            .expect("one card cannot carry the main rig's capture and rig_b's at once");
        assert!(
            why.contains("plughw:1,0"),
            "the error must name the device: {why}"
        );
    }

    /// The two controls that stop the refusal being indiscriminate.
    #[test]
    fn a_distinct_or_unset_tx_device_is_accepted() {
        assert!(
            repeater_tx_device_config_error("plughw:2,0", "plughw:1,0").is_none(),
            "two different cards are the whole point of a cross-band repeater"
        );
        assert!(
            repeater_tx_device_config_error("", "plughw:1,0").is_none(),
            "empty means OS default — probably wrong, but warned about, not refused"
        );
        assert!(
            repeater_tx_device_config_error("", "").is_none(),
            "two empties are both the OS default and must not read as a collision, or a stock \
             config would refuse to start"
        );
    }

    /// #1260: the aliasing case is not exotic — it is what an empty `[radio.rig_b]` header produces,
    /// because both defaults carry the same rigctld address.
    #[test]
    fn an_empty_rig_b_section_aliases_the_main_rig_and_is_refused() {
        let rig_b = RigConfig::default();
        let radio = RadioConfig::default();
        let why = repeater_rig_b_config_error(
            &rig_b.backend,
            &rig_b.rigctld_addr,
            "rigctld",
            &radio.cat_backend,
            &radio.rigctld_addr,
        )
        .expect(
            "a defaulted [radio.rig_b] points at the main rig's rigctld; accepting it gives two \
             SharedPtts one transmitter",
        );
        assert!(
            why.contains(&rig_b.rigctld_addr),
            "the error must name the address: {why}"
        );
    }

    /// A non-rigctld PTT backend on the main rig does NOT make the address incidental: rigctld keys
    /// the rig over CAT, so `cat_backend = "rigctld"` — the default — shares the endpoint by itself.
    #[test]
    fn a_non_rigctld_ptt_backend_does_not_excuse_the_shared_cat_endpoint() {
        for main_ptt in ["vox", "cm108", "rts"] {
            assert!(
                repeater_rig_b_config_error(
                    "rigctld",
                    "127.0.0.1:4532",
                    main_ptt,
                    "rigctld",
                    "127.0.0.1:4532"
                )
                .is_some(),
                "ptt_backend = {main_ptt} still leaves rig_b's `T 1` on the main rig's CAT rigctld"
            );
        }
    }

    /// The cases that are genuinely fine.
    #[test]
    fn a_distinct_address_or_a_rigctld_free_main_rig_is_accepted() {
        assert!(
            repeater_rig_b_config_error(
                "rigctld",
                "127.0.0.1:4533",
                "rigctld",
                "rigctld",
                "127.0.0.1:4532"
            )
            .is_none(),
            "a second rigctld on its own port is the documented cross-band setup"
        );
        assert!(
            repeater_rig_b_config_error(
                "rigctld",
                "127.0.0.1:4532",
                "cm108",
                "serial",
                "127.0.0.1:4532"
            )
            .is_none(),
            "nothing on the main rig speaks to that rigctld, so the address matching is incidental"
        );
        assert!(
            repeater_rig_b_config_error(
                "rts",
                "127.0.0.1:4532",
                "rigctld",
                "rigctld",
                "127.0.0.1:4532"
            )
            .is_none(),
            "rig_b is not on rigctld, so it never reaches the shared endpoint"
        );
    }
}

#[cfg(test)]
mod station_identity_tests {
    use super::load_station_seed;

    /// Audit 2026-07-19 #7: an unusable identity key must stop the daemon, not silently downgrade it
    /// to a random throwaway key that no peer can match to this station's known identity.
    #[test]
    fn an_unusable_identity_key_fails_closed() {
        let dir = std::env::temp_dir().join(format!("oph-ident-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let path = dir.join("identity.key");
        // The file EXISTS but is the wrong length: "present but unusable", not "first run".
        std::fs::write(&path, b"too short").expect("write");

        let err = load_station_seed(path.to_str().expect("utf8"))
            .expect_err("a malformed identity key must not be silently replaced");
        assert!(
            err.contains("refusing to start"),
            "the error must say the daemon is refusing to start: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Control: a fresh path is first run — the key is generated and persisted, and startup must
    /// still succeed. Failing closed must not become "cannot start without a pre-existing key".
    #[test]
    fn a_fresh_identity_path_still_succeeds_and_is_stable() {
        let dir = std::env::temp_dir().join(format!("oph-ident-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("identity.key");
        let p = path.to_str().expect("utf8");

        let seed = load_station_seed(p).expect("first run must generate and persist a key");
        assert_ne!(seed, [0u8; 32], "a generated seed must not be all-zero");
        assert!(path.exists(), "the generated key must be persisted");

        // A second load returns the SAME key: the identity is stable across restarts, which is the
        // property the old fail-open silently destroyed.
        assert_eq!(
            seed,
            load_station_seed(p).expect("reload"),
            "the persisted identity must be stable across loads"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The front-end toggles are readable, and their reported value is engine truth (#1276).
///
/// `SetNotch`, `SetAgc`, `SetCessb`, `SetLogbook` and `SetDcdSquelch` were write-only: a client
/// could set them and never read them back. The panel therefore kept local shadow bools starting at
/// `false`, while `notch_enabled` and `cessb_enabled` ship as `true` — so on a default install two
/// controls painted the opposite of the truth from the first frame, and the first click sent the
/// value already in force, a no-op that merely flipped the display.
///
/// Here rather than in `tests/`: `front_end_state` is private to this module and the property is
/// about what it reads. Exporting it so an integration test could call it is the "public API for an
/// instrument" shape (#1271).
#[cfg(test)]
mod front_end_readback_tests {
    use super::front_end_state;
    use crate::RuntimeControlState;
    use openpulse_audio::LoopbackBackend;
    use openpulse_modem::ModemEngine;

    fn engine() -> ModemEngine {
        let mut e = ModemEngine::new(Box::new(LoopbackBackend::new()));
        e.register_plugin(Box::new(bpsk_plugin::BpskPlugin::default()))
            .expect("register");
        e
    }

    /// The defect in one assertion: a reader initialised to `false` is wrong about the shipped
    /// defaults before anyone touches anything.
    #[test]
    fn two_of_the_five_ship_enabled_so_a_false_default_displays_their_opposite() {
        let cfg = openpulse_config::ModemConfig::default();
        assert!(
            cfg.notch_enabled && cfg.cessb_enabled,
            "notch and CE-SSB ship ENABLED — a panel shadow starting at false shows the opposite \
             of the truth on a default install, which is what #1276 is"
        );
        assert!(
            !cfg.agc_enabled,
            "AGC ships DISABLED, so the false shadow was accidentally right for it. Two of the \
             five inverted, not all five — the fix is still the class of five, but the claim must \
             not be inflated"
        );
    }

    /// `front_end_state` reports what the engine holds, not what was last commanded.
    ///
    /// This is the property that makes the readout worth having: a daemon-side shadow of the
    /// commands sent would restate what the client already believes and could never catch the
    /// engine disagreeing.
    #[test]
    fn the_report_follows_the_engine_not_the_command() {
        let mut e = engine();
        let mut rs = RuntimeControlState::default();

        e.enable_notch();
        e.disable_agc();
        rs.logbook.set_enabled(true);
        let s = front_end_state(&e, &rs);
        assert!(s.notch, "engine has the notch on");
        assert!(!s.agc, "engine has AGC off");
        assert!(s.logbook, "logbook is runtime state, not engine state");

        // Flip both on the engine only — no command, no shadow update.
        e.disable_notch();
        e.enable_agc();
        let s = front_end_state(&e, &rs);
        assert!(
            !s.notch && s.agc,
            "the report must follow the engine; a shadow of the last command would still say \
             notch-on/agc-off here"
        );
    }
}
