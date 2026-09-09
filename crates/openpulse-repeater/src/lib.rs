//! Cross-band repeater: receives frames on one modem engine and re-transmits
//! them on a second engine through a separate rig.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openpulse_core::station_id::StationIdTimer;
use openpulse_modem::pipeline::AudioSamples;
use openpulse_modem::ModemEngine;
use openpulse_radio::{PttController, PttKeyGuard, SharedPtt, DEFAULT_PTT_MAX};
use thiserror::Error;

pub use config::RepeaterConfig;

pub mod config;

/// How long the relay loop waits for a burst before re-checking `stop`.
///
/// Since #1308 this is a RECV timeout, not a sleep: the loop blocks on the daemon's burst channel
/// rather than polling a capture, so an idle band produces no wakeups at all instead of a stream of
/// empty reads. Its only job now is to keep `stop` responsive; it costs no relay latency, because a
/// burst wakes the loop immediately.
///
/// (It was introduced by #1297 to bound a busy-wait that no longer exists — the loop then polled a
/// capture stream and had to sleep between attempts.)
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
    /// Modem engine used to DECODE relayed bursts. It captures nothing (#1308): the bursts arrive
    /// from the daemon, which is the only holder of the receive rig's capture stream.
    engine_rx: ModemEngine,
    /// Bursts the daemon's accumulator flushed, bounded and lossy on purpose — see `run_full_duplex`.
    bursts: Receiver<AudioSamples>,
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
    /// - `engine_rx`: modem engine used to DECODE bursts; it captures nothing.
    /// - `engine_tx`: modem engine wired to rig_b's audio output.
    /// - `bursts`: bursts flushed by the daemon's accumulator on the receive rig (#1308).
    /// - `config`: repeater configuration.
    pub fn new(
        rig_b: Box<dyn PttController + Send>,
        engine_rx: ModemEngine,
        engine_tx: ModemEngine,
        bursts: Receiver<AudioSamples>,
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
            bursts,
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
    pub fn relay_burst(&mut self, burst: &AudioSamples) -> Result<Option<usize>, RepeaterError> {
        let now_ms = self.start.elapsed().as_millis() as u64;
        self.relay_burst_at(burst, now_ms)
    }

    /// [`relay_one_frame`] with an explicit monotonic clock (for deterministic ID-timing tests).
    pub fn relay_burst_at(
        &mut self,
        burst: &AudioSamples,
        now_ms: u64,
    ) -> Result<Option<usize>, RepeaterError> {
        // The burst arrives from the DAEMON's accumulator (#1308). This engine holds no capture
        // stream: the receive rig has exactly one, and it is the daemon's — #1007's rule. The burst
        // has already been through the daemon's `InputCapture` seam, and `decode_burst` suppresses a
        // second pass, so the repeater hears through the daemon's notch/AGC/DCD tuned to the
        // daemon's active mode. That is the accepted cost of the #1308 decision, not an oversight.
        let bytes = match self
            .engine_rx
            .decode_burst(&self.config.mode.clone(), burst)
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
        let mut count = 0u64;
        let result = loop {
            if stop.load(Ordering::Relaxed) {
                break Ok(count);
            }
            // Block on the daemon's bursts rather than polling a capture (#1308). The timeout is
            // what makes `stop` responsive; there is no idle spin to bound any more, because an idle
            // band produces no bursts at all rather than a stream of empty reads.
            let burst = match self
                .bursts
                .recv_timeout(Duration::from_millis(IDLE_POLL_MS))
            {
                Ok(b) => b,
                Err(RecvTimeoutError::Timeout) => continue,
                // The daemon dropped the sender: it is shutting down or the repeater was disabled.
                // Ending the session is right, and #1298 reports the exit.
                Err(RecvTimeoutError::Disconnected) => break Ok(count),
            };
            match self.relay_burst(&burst) {
                Ok(Some(_)) => count += 1,
                Ok(None) => {}
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
}

#[cfg(test)]
mod full_duplex_silence_tests {
    use super::*;
    use bpsk_plugin::BpskPlugin;
    use openpulse_audio::LoopbackBackend;
    use openpulse_radio::PttError;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::sync_channel;
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

    fn decode_engine() -> ModemEngine {
        let mut e = ModemEngine::new(Box::new(LoopbackBackend::new()));
        e.register_plugin(Box::new(BpskPlugin::new()))
            .expect("register");
        e
    }

    /// One BPSK250 frame's audio, as the daemon's accumulator would hand it over.
    fn frame_audio() -> Vec<f32> {
        let lb = LoopbackBackend::new();
        let mut src = ModemEngine::new(Box::new(lb.clone_shared()));
        src.register_plugin(Box::new(BpskPlugin::new()))
            .expect("register");
        src.transmit(b"fd frame", "BPSK250", None).expect("tx");
        lb.drain_samples()
    }

    fn repeater(spy: &SpyPtt, full_duplex: bool) -> CrossBandRepeater {
        // The channel is unused by these tests — they call `relay_burst_at` directly — but the
        // repeater owns one, so it is created and dropped here.
        let (_tx, rx) = sync_channel(1);
        CrossBandRepeater::new(
            Box::new(spy.clone()),
            decode_engine(),
            decode_engine(),
            rx,
            RepeaterConfig {
                full_duplex,
                ..Default::default()
            },
        )
    }

    /// Relay one burst, the way the daemon's loop does since #1308: the repeater no longer captures,
    /// so a test hands it a burst rather than driving a capture stream.
    fn relay_one(rp: &mut CrossBandRepeater, audio: &[f32], now_ms: u64) {
        let burst = AudioSamples {
            samples: audio.to_vec(),
        };
        rp.relay_burst_at(&burst, now_ms)
            .expect("relay")
            .expect("the burst must relay");
    }

    /// The half of "the deadline measures silence" that no integration test can reach.
    ///
    /// `acquire_key`'s re-key branch fires only after the watchdog has taken a held key, and the
    /// repeater's `SharedPtt` is built inside `new()` at [`DEFAULT_PTT_MAX`] — 180 s — with no
    /// injection point from `tests/`. A unit test can shorten it, so this is a unit test rather than
    /// a public setter existing only for a probe.
    #[test]
    fn a_full_duplex_key_lost_to_silence_is_re_taken_on_the_next_frame() {
        let spy = SpyPtt::default();
        let mut rp = repeater(&spy, true);
        rp.ptt.set_max_duration(Duration::from_millis(120));
        let frame = frame_audio();

        relay_one(&mut rp, &frame, 0);
        assert_eq!(
            *spy.edges.lock().expect("lock"),
            vec!["assert"],
            "the first burst takes the key and holds it"
        );

        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(
            *spy.edges.lock().expect("lock"),
            vec!["assert", "release"],
            "the watchdog must drop a full-duplex carrier that has gone silent — otherwise the only \
             bound on it is the daemon staying alive"
        );

        relay_one(&mut rp, &frame, 1_000);
        assert_eq!(
            *spy.edges.lock().expect("lock"),
            vec!["assert", "release", "assert"],
            "the next burst must RE-KEY; a stale guard would transmit into an unkeyed rig"
        );
        assert_eq!(spy.asserts.load(Ordering::SeqCst), 2);
    }

    /// Control: relaying steadily must NOT lose the key, or the test above would pass on a repeater
    /// that simply re-keys every burst — which is half duplex, not the flag's promise.
    #[test]
    fn steady_traffic_extends_the_deadline_instead_of_re_keying() {
        let spy = SpyPtt::default();
        let mut rp = repeater(&spy, true);
        rp.ptt.set_max_duration(Duration::from_millis(250));
        let frame = frame_audio();

        for i in 0..4u64 {
            relay_one(&mut rp, &frame, i * 100);
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
