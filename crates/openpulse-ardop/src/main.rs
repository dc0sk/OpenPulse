use clap::Parser;
use openpulse_ardop::{ArdopConfig, ArdopServer};
use openpulse_audio::loopback::LoopbackBackend;
use openpulse_core::relay::{RelayForwarder, RelayTrustPolicy};
use openpulse_core::trust_store_file::load_trust_store_from_file;
use openpulse_modem::ModemEngine;
use openpulse_radio::{NoOpPtt, PttController};

#[cfg(feature = "cpal")]
use openpulse_audio::CpalBackend;

#[derive(Parser)]
#[command(
    name = "openpulse-tnc",
    about = "OpenPulse ARDOP-compatible TNC",
    long_about = "OpenPulse ARDOP-compatible TNC.",
    author,
    version
)]
struct Cli {
    /// ARDOP command port (overrides config file).
    #[arg(long)]
    cmd_port: Option<u16>,

    /// ARDOP data port (overrides config file).
    #[arg(long)]
    data_port: Option<u16>,

    /// Modulation mode (overrides config file).
    #[arg(long)]
    mode: Option<String>,

    /// Bind address (overrides config file).
    #[arg(long)]
    bind: Option<String>,

    /// Audio backend: default | cpal | loopback (overrides config file).
    #[arg(long)]
    backend: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let mut cfg = openpulse_config::load()?;

    // CLI flags override config file values.
    if let Some(p) = cli.cmd_port {
        cfg.ardop.cmd_port = p;
    }
    if let Some(p) = cli.data_port {
        cfg.ardop.data_port = p;
    }
    if let Some(m) = cli.mode {
        cfg.modem.mode = m;
    }
    if let Some(b) = cli.bind {
        cfg.ardop.bind_addr = b;
    }
    if let Some(b) = cli.backend {
        cfg.audio.backend = b;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&cfg.logging.level)),
        )
        .init();

    let audio: Box<dyn openpulse_core::audio::AudioBackend> = match cfg.audio.backend.as_str() {
        "loopback" => Box::new(LoopbackBackend::default()),
        #[cfg(feature = "cpal")]
        "cpal" | "default" => Box::new(CpalBackend::new()),
        #[cfg(not(feature = "cpal"))]
        "cpal" => {
            tracing::warn!(
                "cpal backend not compiled in (build with --features cpal); using loopback"
            );
            Box::new(LoopbackBackend::default())
        }
        #[cfg(not(feature = "cpal"))]
        "default" => Box::new(LoopbackBackend::default()),
        name => {
            anyhow::bail!("unknown audio backend '{name}' — use 'default', 'cpal', or 'loopback'")
        }
    };

    let mut engine = ModemEngine::new(audio);
    engine.register_plugin(Box::new(bpsk_plugin::BpskPlugin::new()))?;
    engine.register_plugin(Box::new(fsk4_plugin::Fsk4Plugin::new()))?;
    engine.register_plugin(Box::new(ofdm_plugin::OfdmPlugin::new()))?;
    engine.register_plugin(Box::new(qpsk_plugin::QpskPlugin::new()))?;
    engine.register_plugin(Box::new(psk8_plugin::Psk8Plugin::new()))?;
    engine.register_plugin(Box::new(qam64_plugin::Qam64Plugin::new()))?;
    engine.register_plugin(Box::new(scfdma_plugin::ScFdmaPlugin::new()))?;
    engine.register_plugin(Box::new(pilot_plugin::PilotPlugin::new()))?;
    // Declared TX power for the §97 regulatory TX-metadata log. The operating callsign is the host
    // `MYID` (set at runtime, mirrored into the engine by the command handler), not config.
    engine.set_max_power_watts(cfg.station.tx_power_watts);

    // Opt-in adaptive ARQ: starting the session activates the worker's adaptive TX/RX path
    // (transmit_arq / receive_with_ack_hint) and makes ARQBW/ARQTIMEOUT effective.
    if cfg.ardop.enable_adaptive_arq {
        let name = if cfg.ardop.adaptive_profile.is_empty() {
            "hpx500"
        } else {
            &cfg.ardop.adaptive_profile
        };
        match openpulse_core::profile::SessionProfile::by_name(name) {
            Some(profile) => {
                // The MFSK16 sub-floor rung's robust ACK (K=3 union) lives only on the daemon's
                // receiver-led OTA path, not this RateAdapter path — and MFSK16 isn't even registered here.
                // A profile that maps it (hpx_hf) would fail at deep fade; warn rather than fail silently.
                if profile
                    .defined_levels()
                    .into_iter()
                    .any(|l| profile.mode_for(l) == Some("MFSK16"))
                {
                    tracing::warn!(
                        profile = %name,
                        "adaptive_profile maps an MFSK16 sub-floor rung, which the ARDOP adaptive path does \
                         not support (its ACK isn't the K=3 union); the rung will fail at deep fade"
                    );
                }
                engine.start_adaptive_session(profile);
                tracing::info!(profile = %name, "adaptive ARQ session enabled");
            }
            None => tracing::warn!(
                profile = %name,
                "unknown adaptive_profile; adaptive ARQ not started (fixed-mode operation)"
            ),
        }
    }

    let config = ArdopConfig {
        bind_addr: cfg.ardop.bind_addr.clone(),
        command_port: cfg.ardop.cmd_port,
        data_port: cfg.ardop.data_port,
        mode: cfg.modem.mode.clone(),
        loopback: false,
        auto_id_interval_secs: cfg.station.auto_id_interval_secs,
        auto_id_signoff_idle_secs: cfg.station.auto_id_signoff_idle_secs,
    };

    tracing::info!(
        "OpenPulse TNC listening on {}:{} (cmd) / {}:{} (data)",
        config.bind_addr,
        config.command_port,
        config.bind_addr,
        config.data_port,
    );

    let trust_store = if !cfg.trust.store_path.is_empty() {
        // Audit F2: the ARDOP TNC does not run the signed CONREQ/CONACK handshake — CONNECT drives the
        // local HPX state machine and data-port bytes are an unauthenticated passthrough — so this
        // trust store is loaded but never consulted to authenticate a peer. Warn so operators aren't
        // misled into thinking this bridge gates connections by trust.
        tracing::warn!(
            path = %cfg.trust.store_path,
            "trust store configured but the ARDOP TNC does not authenticate peers over RF; it is not consulted"
        );
        match load_trust_store_from_file(std::path::Path::new(&cfg.trust.store_path)) {
            Ok(store) => {
                tracing::info!(path = %cfg.trust.store_path, "trust store loaded");
                store
            }
            Err(e) => {
                tracing::warn!(path = %cfg.trust.store_path, error = %e, "failed to load trust store; starting with empty store");
                Default::default()
            }
        }
    } else {
        Default::default()
    };

    let relay_forwarder = if cfg.relay.enabled {
        let mut policy = if cfg.relay.deny_list.is_empty() {
            RelayTrustPolicy::default()
        } else {
            RelayTrustPolicy::deny_relays(cfg.relay.deny_list.iter().map(|s| s.as_str()))
        };
        policy.set_allow_list(cfg.relay.allow_list.iter().map(|s| s.as_str()));
        let ttl_ms = cfg.relay.store_forward_ttl_s.saturating_mul(1000);
        let fwd = RelayForwarder::new(ttl_ms, policy);
        tracing::info!(
            max_hops = cfg.relay.max_hops,
            deny_count = cfg.relay.deny_list.len(),
            ttl_s = cfg.relay.store_forward_ttl_s,
            "relay forwarding enabled"
        );
        Some(fwd)
    } else {
        None
    };

    ArdopServer::with_trust_relay_ptt(
        engine,
        config,
        trust_store,
        relay_forwarder,
        build_ptt(&cfg),
    )
    .run()
    .await?;
    Ok(())
}

fn build_ptt(cfg: &openpulse_config::OpenpulseConfig) -> Box<dyn PttController + Send> {
    // #1258: the ONE builder. This match handled `vox`, `rigctld` and `none` only, so `rts`, `dtr`,
    // `cm108` and `gpio` — all documented in `[modem]`, which is captioned "shared by all TNC
    // binaries", and all supported by the daemon — fell through to `NoOpPtt`. An operator who
    // validated `ptt_backend = "cm108"` against the daemon and then started this TNC on the same
    // config got an unkeyed rig.
    //
    // Fail-open is PRESERVED here (this returns a controller, never an error) so the dedupe changes
    // no behaviour; whether an unusable backend should refuse startup is #1285.
    match openpulse_radio::ptt_builder::build_ptt(&openpulse_radio::ptt_builder::PttSpec {
        backend: &cfg.modem.ptt_backend,
        rigctld_addr: &cfg.radio.rigctld_addr,
        device: &cfg.modem.ptt_device,
        gpio_pin: cfg.modem.ptt_gpio,
    }) {
        Ok(Some(ctrl)) => {
            tracing::info!(backend = %cfg.modem.ptt_backend, "PTT controller ready");
            ctrl
        }
        Ok(None) => Box::new(NoOpPtt::new()),
        Err(e) => {
            tracing::warn!(
                backend = %cfg.modem.ptt_backend,
                error = %e,
                "PTT unavailable; using NoOpPtt — this TNC will transmit without keying the rig"
            );
            Box::new(NoOpPtt::new())
        }
    }
}
