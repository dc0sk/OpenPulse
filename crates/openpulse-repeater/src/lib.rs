//! Cross-band repeater: receives frames on one modem engine and re-transmits
//! them on a second engine through a separate rig.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use openpulse_core::station_id::StationIdTimer;
use openpulse_modem::capture_ticker::CaptureTicker;
use openpulse_modem::ModemEngine;
use openpulse_radio::{PttController, PttKeyGuard, SharedPtt, DEFAULT_PTT_MAX};
use thiserror::Error;

pub use config::RepeaterConfig;

pub mod config;

/// Pause between relay attempts when nothing decoded, so an idle session does not spin (#1297).
///
/// Well under one frame's airtime at every mode the repeater supports, so it costs no latency; its
/// only job is to stop a busy-wait on a loop that now survives idle windows.
const IDLE_POLL_MS: u64 = 100;

#[derive(Debug, Error)]
pub enum RepeaterError {
    #[error("modem error: {0}")]
    Modem(String),
    #[error("PTT error: {0}")]
    Ptt(#[from] openpulse_radio::PttError),
}

/// Relays decoded frames from `engine_rx` to `engine_tx`, asserting PTT on `rig_b`.
pub struct CrossBandRepeater {
    /// PTT for the transmitting rig, under a watchdog (#1260). Its own `SharedPtt`, not the daemon's:
    /// rig_b is a different transmitter, so nesting the two would let one rig's guard release the
    /// other. The daemon must refuse a config that points both at the same rigctld.
    ptt: SharedPtt,
    /// The key a full-duplex session holds across frames. Each relayed frame extends its deadline, so
    /// the watchdog measures **silence**, not session length. `None` in half duplex.
    session_guard: Option<PttKeyGuard>,
    /// Modem engine used for receiving (driven by rig_a audio).
    engine_rx: ModemEngine,
    /// Modem engine used for re-transmitting (drives rig_b audio).
    engine_tx: ModemEngine,
    config: RepeaterConfig,
    /// §97.119 auto-ID of the transmitting rig (rig_b), independent of the daemon's main-engine timer.
    id_timer: Option<StationIdTimer>,
    /// Monotonic clock origin for the ID timer.
    start: Instant,
}

impl CrossBandRepeater {
    /// Create a new cross-band repeater.
    ///
    /// - `rig_b`: PTT controller for the transmitting rig.
    /// - `engine_rx`: modem engine wired to rig_a's audio input.
    /// - `engine_tx`: modem engine wired to rig_b's audio output.
    /// - `config`: repeater configuration.
    pub fn new(
        rig_b: Box<dyn PttController + Send>,
        engine_rx: ModemEngine,
        engine_tx: ModemEngine,
        config: RepeaterConfig,
    ) -> Self {
        // Auto-ID only with a callsign and a positive interval; rig_b is an automatically-controlled
        // station (§97.221) that must ID per §97.119, and the daemon's main-engine timer never sees it.
        let id_timer = (!config.callsign.trim().is_empty() && config.id_interval_secs > 0)
            .then(|| StationIdTimer::new(config.id_interval_secs.saturating_mul(1000), 0));
        // A `SharedPtt` with no watchdog thread is a bare `Box` with extra steps. No observer: the
        // daemon's `PttChanged` carries no rig identity, so rig_b's edges would flip the panel's
        // main-rig indicator (#1298).
        let ptt = SharedPtt::new(Some(rig_b), DEFAULT_PTT_MAX);
        let _ = ptt.spawn_watchdog(None);
        Self {
            ptt,
            session_guard: None,
            engine_rx,
            engine_tx,
            config,
            id_timer,
            start: Instant::now(),
        }
    }

    /// Attempt to receive one frame from `engine_rx` and relay it via `engine_tx`.
    ///
    /// Returns the number of bytes relayed, or `None` if no frame was available.
    /// FEC is not applied on the relay path (raw mode).
    pub fn relay_one_frame(
        &mut self,
        rx: &mut CaptureTicker,
    ) -> Result<Option<usize>, RepeaterError> {
        let now_ms = self.start.elapsed().as_millis() as u64;
        self.relay_one_frame_at(rx, now_ms)
    }

    /// [`relay_one_frame`] with an explicit monotonic clock (for deterministic ID-timing tests).
    pub fn relay_one_frame_at(
        &mut self,
        rx: &mut CaptureTicker,
        now_ms: u64,
    ) -> Result<Option<usize>, RepeaterError> {
        if !self.config.enabled {
            return Ok(None);
        }

        // Hold ONE capture stream across attempts and accumulate under DCD gating, the way the
        // daemon's rx ticker does (#1297). A capture fault is reported and retried inside the
        // ticker rather than surfacing here: treating it as fatal would stop the repeater
        // listening for good, and treating it as silence — which the previous `Err => Ok(None)`
        // arm did — made an unopenable RX device indistinguishable from a quiet band, at DEBUG.
        let Some(burst) = rx.tick(&mut self.engine_rx, &self.config.mode).burst else {
            return Ok(None);
        };

        let bytes = match self
            .engine_rx
            .decode_burst(&self.config.mode.clone(), &burst)
        {
            Ok(b) => b,
            Err(e) => {
                // A burst that does not decode is the ordinary case on a live band: noise that
                // opened the squelch, or a frame this repeater's mode cannot read. Not a fault.
                tracing::debug!(error = %e, "cross-band relay: burst did not decode");
                return Ok(None);
            }
        };

        if bytes.is_empty() {
            return Ok(None);
        }

        let n = bytes.len();
        // ONE key covers the relayed frame AND the §97.119 ID that may follow it, in both modes.
        // Before #1260 the half-duplex path asserted here and `maybe_identify` asserted again
        // underneath it, releasing rig_b mid-scope while this scope still believed it held the key.
        // The guard also closes the leak this issue was filed for: every `?` below releases.
        let guard = self.acquire_key()?;
        self.engine_tx
            .transmit(&bytes, &self.config.mode.clone(), None)
            .map_err(|e| RepeaterError::Modem(e.to_string()))?;
        if let Some(t) = self.id_timer.as_mut() {
            t.note_tx(now_ms);
        }
        self.maybe_identify(now_ms)?;

        if self.config.full_duplex {
            // Re-stamp AFTER transmitting, so the deadline measures silence since the last
            // transmission *ended*. `acquire_key`'s extend happens before `transmit` and would
            // otherwise start the clock at the transmission's beginning.
            let _ = guard.extend();
            self.session_guard = Some(guard);
        } else {
            if self.config.tx_hang_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(self.config.tx_hang_ms));
            }
            guard.release();
        }

        tracing::info!(
            mode = %self.config.mode,
            bytes = n,
            "cross-band relay: relayed frame"
        );

        Ok(Some(n))
    }

    /// Take the key for one transmission, reusing a live full-duplex session key and extending its
    /// deadline (#1260).
    ///
    /// The extend is what makes the watchdog measure silence rather than session length: a repeater
    /// relaying traffic keeps its key, one that has gone quiet loses it and re-keys on the next frame.
    /// A `false` from `extend` means the watchdog already took the key, which is the signal to re-key
    /// rather than transmit into an unkeyed rig.
    fn acquire_key(&mut self) -> Result<PttKeyGuard, RepeaterError> {
        if let Some(g) = self.session_guard.take() {
            if g.extend() {
                return Ok(g);
            }
            tracing::warn!("cross-band relay: session PTT expired on silence; re-keying");
        }
        self.ptt.keyed(None).map_err(RepeaterError::Ptt)
    }

    /// Transmit `DE <callsign>` on rig_b if the auto-ID interval has elapsed. Never keys: it runs
    /// under the caller's key, so the ID goes out under the same carrier as the traffic it identifies
    /// (#1260). No-op without a timer.
    fn maybe_identify(&mut self, now_ms: u64) -> Result<(), RepeaterError> {
        let due = self.id_timer.as_ref().is_some_and(|t| t.id_due(now_ms));
        if !due {
            return Ok(());
        }
        let id_body = format!("DE {}", self.config.callsign);
        self.engine_tx
            .transmit(id_body.as_bytes(), &self.config.mode.clone(), None)
            .map_err(|e| RepeaterError::Modem(e.to_string()))?;
        if let Some(t) = self.id_timer.as_mut() {
            t.mark_identified(now_ms);
        }
        tracing::info!(callsign = %self.config.callsign, "cross-band relay: transmitted station ID");
        Ok(())
    }

    /// Run the relay loop until `stop` is set, returning the total number of frames relayed.
    ///
    /// In **half-duplex** (the default) each relayed frame keys, transmits, IDs if due, and releases.
    ///
    /// In **full-duplex** (`config.full_duplex`) the key is *held across* frames instead of dropped
    /// between them — which is what the flag buys — and every relayed frame re-stamps the watchdog
    /// deadline. So the bound is [`DEFAULT_PTT_MAX`] of **silence**, not of session length: a busy
    /// repeater keeps its carrier indefinitely, a quiet one drops it and re-keys on the next frame.
    ///
    /// Changed in #1260: the session no longer keys eagerly at start. A watchdog is in-process, so an
    /// unbounded deliberate hold does not mean "a hung repeater keys forever" — it means a *dead
    /// daemon* leaves rig_b keyed, since there is no shutdown release anywhere and `RigctldPtt` has no
    /// `Drop`. On an unattended §97.221 station that is the worst available configuration, and an
    /// eager key made it the state of an idle repeater rather than a fault case.
    ///
    /// A capture with no decodable frame is not an error and does not end the session (#1297).
    /// PTT is released when the loop returns, on the error path too.
    pub fn run_full_duplex(&mut self, stop: Arc<AtomicBool>) -> Result<u64, RepeaterError> {
        if !self.config.enabled {
            return Ok(0);
        }
        // The capture stream is owned HERE, not on the struct: `Box<dyn AudioInputStream>` is not
        // `Send` (a cpal `Stream` is not, on most hosts) and the daemon moves the repeater into a
        // thread, so a stream field would make `CrossBandRepeater` unspawnable. The daemon's own rx
        // ticker keeps its stream as a loop local for the same reason.
        let mut rx = CaptureTicker::new(None);
        let mut count = 0u64;
        let result = loop {
            if stop.load(Ordering::Relaxed) {
                break Ok(count);
            }
            match self.relay_one_frame(&mut rx) {
                Ok(Some(_)) => count += 1,
                // Since #1297 an idle window is `Ok(None)` rather than a session-ending error, so
                // this arm is now reached continuously instead of never. Without a pause the loop
                // burns a core on loopback and re-opens a cpal input stream thousands of times a
                // second, because `receive()` opens one per call. The sleep bounds that; it does not
                // fix it — the real fix is routing RX through the accumulating capture path (#1297).
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(IDLE_POLL_MS)),
                Err(e) => break Err(e),
            }
        };
        // Drop whatever the session still holds. Generation-scoped, so a guard the watchdog already
        // force-released is a silent no-op rather than a release of somebody else's key.
        self.session_guard = None;
        result
    }

    /// The mode this repeater receives and re-transmits.
    ///
    /// The daemon declares it to the engine as the relay rung so the RX burst cap covers it (#1308):
    /// the repeater reads the engine's bursts, so a cap sized from `[modem] mode` alone would
    /// truncate the frames it exists to forward.
    pub fn mode(&self) -> &str {
        &self.config.mode
    }

    /// Return whether the repeater is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }
}

#[cfg(test)]
mod full_duplex_silence_tests {
    use super::*;
    use bpsk_plugin::BpskPlugin;
    use openpulse_audio::LoopbackBackend;
    use openpulse_radio::PttError;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Clone, Default)]
    struct SpyPtt {
        edges: Arc<Mutex<Vec<&'static str>>>,
        asserts: Arc<AtomicUsize>,
    }
    impl PttController for SpyPtt {
        fn assert_ptt(&mut self) -> Result<(), PttError> {
            self.asserts.fetch_add(1, Ordering::SeqCst);
            self.edges.lock().expect("lock").push("assert");
            Ok(())
        }
        fn release_ptt(&mut self) -> Result<(), PttError> {
            self.edges.lock().expect("lock").push("release");
            Ok(())
        }
        fn is_asserted(&self) -> bool {
            false
        }
    }

    fn engine_with_plugin(lb: &LoopbackBackend) -> ModemEngine {
        let mut e = ModemEngine::new(Box::new(lb.clone_shared()));
        e.register_plugin(Box::new(BpskPlugin::new()))
            .expect("register");
        e
    }

    /// Tick until a frame is relayed, the way the daemon's loop does.
    ///
    /// Since #1297 one `relay_one_frame_at` is one capture TICK, not one receive attempt: the burst
    /// accumulator flushes when the carrier drops, which on `LoopbackBackend` is the first empty
    /// read after the frame. So relaying takes at least two calls.
    fn relay_until(rp: &mut CrossBandRepeater, now_ms: u64) -> usize {
        let mut rx = CaptureTicker::new(None);
        for _ in 0..16 {
            match rp.relay_one_frame_at(&mut rx, now_ms).expect("relay") {
                Some(n) => return n,
                None => continue,
            }
        }
        panic!("no frame relayed within 16 ticks");
    }

    /// The half of "the deadline measures silence" that no integration test can reach.
    ///
    /// `acquire_key`'s re-key branch fires only after the watchdog has taken a held key, and the
    /// repeater's `SharedPtt` is built inside `new()` at [`DEFAULT_PTT_MAX`] — 180 s — with no
    /// injection point from `tests/`. A unit test can shorten it, so this is a unit test rather
    /// than a public setter existing only for a probe.
    ///
    /// Without the re-key, every frame after the watchdog fired would be played into an UNKEYED
    /// rig: the inner emissions were gated on `!full_duplex` before #1260, so nothing would have
    /// keyed again for the life of the session.
    #[test]
    fn a_full_duplex_key_lost_to_silence_is_re_taken_on_the_next_frame() {
        let spy = SpyPtt::default();
        let lb_rx = LoopbackBackend::new();
        let config = RepeaterConfig {
            enabled: true,
            full_duplex: true,
            ..Default::default()
        };
        let mut rp = CrossBandRepeater::new(
            Box::new(spy.clone()),
            engine_with_plugin(&lb_rx),
            engine_with_plugin(&LoopbackBackend::new()),
            config,
        );
        // Reach past the constructor's 180 s so the silence bound is observable in a test.
        rp.ptt.set_max_duration(Duration::from_millis(120));

        let feed = || {
            let mut src = ModemEngine::new(Box::new(lb_rx.clone_shared()));
            src.register_plugin(Box::new(BpskPlugin::new()))
                .expect("register src");
            src.transmit(b"fd frame", "BPSK250", None).expect("tx");
        };

        feed();
        relay_until(&mut rp, 0);
        assert_eq!(
            *spy.edges.lock().expect("lock"),
            vec!["assert"],
            "the first frame takes the key and holds it"
        );

        // Go quiet for longer than the deadline; the watchdog must take the key back.
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(
            *spy.edges.lock().expect("lock"),
            vec!["assert", "release"],
            "the watchdog must drop a full-duplex carrier that has gone silent — otherwise the only \
             bound on it is the daemon staying alive"
        );

        feed();
        relay_until(&mut rp, 1_000);
        assert_eq!(
            *spy.edges.lock().expect("lock"),
            vec!["assert", "release", "assert"],
            "the next frame must RE-KEY; a stale guard would transmit into an unkeyed rig"
        );
        assert_eq!(spy.asserts.load(Ordering::SeqCst), 2);
    }

    /// Control: relaying steadily must NOT lose the key, or the test above would pass on a repeater
    /// that simply re-keys every frame — which is half duplex, not the flag's promise.
    #[test]
    fn steady_traffic_extends_the_deadline_instead_of_re_keying() {
        let spy = SpyPtt::default();
        let lb_rx = LoopbackBackend::new();
        let config = RepeaterConfig {
            enabled: true,
            full_duplex: true,
            ..Default::default()
        };
        let mut rp = CrossBandRepeater::new(
            Box::new(spy.clone()),
            engine_with_plugin(&lb_rx),
            engine_with_plugin(&LoopbackBackend::new()),
            config,
        );
        rp.ptt.set_max_duration(Duration::from_millis(250));

        // Four relays at 100 ms spacing span 400 ms — well past the deadline had it not been
        // re-stamped by each frame.
        for i in 0..4u64 {
            let mut src = ModemEngine::new(Box::new(lb_rx.clone_shared()));
            src.register_plugin(Box::new(BpskPlugin::new()))
                .expect("register src");
            src.transmit(b"fd frame", "BPSK250", None).expect("tx");
            relay_until(&mut rp, i * 100);
            std::thread::sleep(Duration::from_millis(100));
        }

        assert_eq!(
            *spy.edges.lock().expect("lock"),
            vec!["assert"],
            "a repeater relaying continuously must hold ONE key across 400 ms with a 250 ms bound — \
             the deadline measures silence, not session length"
        );
    }
}
