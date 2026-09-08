//! `openpulse-tui` — live TUI dashboard for the OpenPulse modem engine.

use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tracing::Level;

use bpsk_plugin::BpskPlugin;
use fsk4_plugin::Fsk4Plugin;
use mfsk16_plugin::Mfsk16Plugin;
use ofdm_plugin::OfdmPlugin;
use openpulse_audio::LoopbackBackend;
use openpulse_modem::ModemEngine;
use pilot_plugin::PilotPlugin;
use psk8_plugin::Psk8Plugin;
use qam64_plugin::Qam64Plugin;
use qpsk_plugin::QpskPlugin;
use scfdma_plugin::ScFdmaPlugin;

#[cfg(feature = "cpal-backend")]
use openpulse_audio::CpalBackend;

mod app;
mod events;
mod ui;

#[derive(clap::Parser)]
#[command(
    name = "openpulse-tui",
    about = "OpenPulse TUI dashboard",
    long_about = "OpenPulse TUI dashboard.",
    author,
    version
)]
struct Cli {
    /// Modulation mode to drive the receive loop.
    #[arg(short, long, default_value = "BPSK100")]
    mode: String,

    /// Audio backend: loopback | default | cpal.
    #[arg(long, default_value = "loopback")]
    backend: String,

    /// Verbosity level.
    #[arg(long, default_value = "warn")]
    log: String,
}

fn main() -> Result<()> {
    use clap::Parser as _;
    let cli = Cli::parse();

    let level: Level = cli.log.parse().unwrap_or(Level::WARN);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .init();

    // Loaded BEFORE the engine so `[audio] device` can be pinned at construction (#1311). The TUI
    // takes its backend from `--backend` but has no device flag, so the config is the only source.
    let cfg = openpulse_config::load().unwrap_or_default();

    let audio: Box<dyn openpulse_core::audio::AudioBackend> = match cli.backend.as_str() {
        "loopback" => Box::new(LoopbackBackend::new()),
        #[cfg(feature = "cpal-backend")]
        "default" | "cpal" => Box::new(CpalBackend::new()),
        #[cfg(not(feature = "cpal-backend"))]
        "default" => {
            eprintln!(
                "note: cpal backend not compiled in (enable --features cpal-backend); falling back to loopback"
            );
            Box::new(LoopbackBackend::new())
        }
        name => anyhow::bail!("unknown backend '{name}'"),
    };

    let mut engine = ModemEngine::new(audio);
    // Pin audio I/O to the configured device (#1311). Without this the engine falls back to the
    // OS default: `stage_capture_input` resolves `device.or(self.default_device)`, and every call
    // site in this binary passes `None`, so an operator with a USB soundcard interface plus onboard
    // audio silently got the onboard card. `[audio] device` was honoured by exactly one engine in
    // the workspace before this.
    if !cfg.audio.device.is_empty() {
        engine.set_default_device(Some(cfg.audio.device.clone()));
    }
    engine.register_plugin(Box::new(BpskPlugin::new()))?;
    engine.register_plugin(Box::new(Fsk4Plugin::new()))?;
    engine.register_plugin(Box::new(OfdmPlugin::new()))?;
    engine.register_plugin(Box::new(Psk8Plugin::new()))?;
    engine.register_plugin(Box::new(Qam64Plugin::new()))?;
    engine.register_plugin(Box::new(QpskPlugin::new()))?;
    engine.register_plugin(Box::new(ScFdmaPlugin::new()))?;
    engine.register_plugin(Box::new(Mfsk16Plugin::new()))?;
    engine.register_plugin(Box::new(PilotPlugin::new()))?;

    let rx = engine.subscribe();
    let worker = events::spawn_worker(engine, cli.mode.clone(), rx);

    if cfg.station.callsign.trim().eq_ignore_ascii_case("N0CALL") {
        anyhow::bail!(
            "invalid callsign N0CALL in configuration; set [station].callsign before running openpulse-tui"
        );
    }

    let initial_qsy_enabled = cfg.qsy.enabled;
    let initial_allow_tuner_on_high_swr = cfg.qsy.allow_integrated_tuner_on_high_swr;
    let initial_bandplan_mode = if cfg.qsy.bandplan_awareness_enabled {
        cfg.qsy.bandplan_mode.clone()
    } else {
        "unrestricted".to_string()
    };

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_tui(
        &mut terminal,
        worker,
        initial_qsy_enabled,
        initial_bandplan_mode,
        initial_allow_tuner_on_high_swr,
    );

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    result
}

fn run_tui(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    worker: std::sync::mpsc::Receiver<events::WorkerMsg>,
    initial_qsy_enabled: bool,
    initial_bandplan_mode: String,
    initial_allow_tuner_on_high_swr: bool,
) -> Result<()> {
    let mut app = app::App {
        qsy_enabled: initial_qsy_enabled,
        bandplan_mode: initial_bandplan_mode,
        allow_tuner_on_high_swr: initial_allow_tuner_on_high_swr,
        ..Default::default()
    };
    let tick = Duration::from_millis(100);

    loop {
        terminal.draw(|f| ui::render(f, &app))?;

        if event::poll(tick)? {
            if let Event::Key(key) = event::read()? {
                match (key.code, key.modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => break,
                    (KeyCode::Char('p'), _) => app.paused = !app.paused,
                    (KeyCode::Up, _) => {
                        app.scroll_offset = app.scroll_offset.saturating_sub(1);
                    }
                    (KeyCode::Down, _) => {
                        app.scroll_offset = app.scroll_offset.saturating_add(1);
                    }
                    (KeyCode::Char('Q'), _) => {
                        app.qsy_enabled = !app.qsy_enabled;
                        let _ = openpulse_config::save_qsy_config(
                            app.qsy_enabled,
                            &app.bandplan_mode,
                            app.allow_tuner_on_high_swr,
                        );
                    }
                    (KeyCode::Char('b'), _) => {
                        app.bandplan_mode = cycle_bandplan(&app.bandplan_mode).to_string();
                        let _ = openpulse_config::save_qsy_config(
                            app.qsy_enabled,
                            &app.bandplan_mode,
                            app.allow_tuner_on_high_swr,
                        );
                    }
                    (KeyCode::Char('t'), _) => {
                        app.allow_tuner_on_high_swr = !app.allow_tuner_on_high_swr;
                        let _ = openpulse_config::save_qsy_config(
                            app.qsy_enabled,
                            &app.bandplan_mode,
                            app.allow_tuner_on_high_swr,
                        );
                    }
                    _ => {}
                }
            }
        }

        match events::drain_worker(&mut app, &worker) {
            Ok(()) => {}
            Err(e) => {
                // Surface fatal worker error before exiting.
                app.fatal_error = Some(e);
                terminal.draw(|f| ui::render(f, &app))?;
                std::thread::sleep(Duration::from_secs(3));
                break;
            }
        }
    }

    Ok(())
}

const BANDPLAN_CYCLE: &[&str] = &["unrestricted", "ham-iaru-r1", "ham-iaru-r2", "ham-iaru-r3"];

fn cycle_bandplan(current: &str) -> &'static str {
    let pos = BANDPLAN_CYCLE
        .iter()
        .position(|&s| s == current)
        .unwrap_or(0);
    BANDPLAN_CYCLE[(pos + 1) % BANDPLAN_CYCLE.len()]
}
