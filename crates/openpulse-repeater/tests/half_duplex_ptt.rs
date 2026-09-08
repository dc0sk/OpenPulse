//! `run_full_duplex` must honour `config.full_duplex` (audit 2026-07-19, finding #2).
//!
//! `full_duplex` defaults to **false**, and its own doc comment reads "When true, PTT is held for the
//! entire relay session by `run_full_duplex()`". Four methods in the crate check the flag before
//! keying; `run_full_duplex` — the one that actually keys the transmitter for the whole session — did
//! not. Enabling the repeater on a default config therefore held an unbounded dead-air carrier, and
//! double-keyed against the per-frame assert/release that half-duplex relaying already does.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bpsk_plugin::BpskPlugin;
use openpulse_audio::LoopbackBackend;
use openpulse_modem::ModemEngine;
use openpulse_radio::{PttController, PttError};
use openpulse_repeater::{CrossBandRepeater, RepeaterConfig};

/// Records the exact PTT edge sequence so a test can assert ordering, not just totals.
#[derive(Clone, Default)]
struct SpyPtt {
    edges: Arc<Mutex<Vec<&'static str>>>,
    asserted: Arc<AtomicBool>,
    peak_concurrent: Arc<AtomicUsize>,
}

impl SpyPtt {
    fn edges(&self) -> Vec<&'static str> {
        self.edges.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl PttController for SpyPtt {
    fn assert_ptt(&mut self) -> Result<(), PttError> {
        // Catch a double-key: asserting while already asserted is the half-duplex bug's signature.
        if self.asserted.swap(true, Ordering::SeqCst) {
            self.peak_concurrent.store(2, Ordering::SeqCst);
        }
        self.edges
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push("assert");
        Ok(())
    }
    fn release_ptt(&mut self) -> Result<(), PttError> {
        self.asserted.store(false, Ordering::SeqCst);
        self.edges
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push("release");
        Ok(())
    }
    fn is_asserted(&self) -> bool {
        self.asserted.load(Ordering::SeqCst)
    }
}

fn engine() -> ModemEngine {
    let mut e = ModemEngine::new(Box::new(LoopbackBackend::new()));
    // Without a plugin the relay loop errors on the first iteration and the session ends before the
    // PTT behaviour under test can be observed.
    e.register_plugin(Box::new(BpskPlugin::new()))
        .expect("register bpsk");
    e
}

/// Run a repeater session briefly, then stop it, and return the observed PTT edges.
///
/// The session's return value is deliberately ignored. Since #1297 an idle loopback no longer ends
/// the session — a capture that does not demodulate is silence, not a fault — so the loop simply
/// spins until `stop`. The question this file asks is whether the transmitter came up at all with
/// nothing to relay, and whether it was left keyed afterwards.
fn run_session(full_duplex: bool) -> SpyPtt {
    let spy = SpyPtt::default();
    let config = RepeaterConfig {
        enabled: true,
        full_duplex,
        ..Default::default()
    };
    let mut rp = CrossBandRepeater::new(Box::new(spy.clone()), engine(), engine(), config);

    let stop = Arc::new(AtomicBool::new(false));
    let stop_c = stop.clone();
    // Let the loop spin a few times with no traffic, then ask it to stop.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(60));
        stop_c.store(true, Ordering::Relaxed);
    });
    let _ = rp.run_full_duplex(stop);
    spy
}

/// THE GATE: with `full_duplex = false` (the default), an idle session must never key the rig.
///
/// No traffic is relayed here, so in half-duplex there is nothing to transmit and therefore no
/// legitimate reason for the transmitter to come up at all.
#[test]
fn half_duplex_session_does_not_hold_ptt() {
    let spy = run_session(false);

    assert_eq!(
        spy.edges(),
        Vec::<&str>::new(),
        "half-duplex idle session keyed the transmitter — an unbounded dead-air carrier on the \
         DEFAULT config (full_duplex defaults to false)"
    );
    assert!(
        !spy.is_asserted(),
        "transmitter left keyed after the session ended"
    );
}

/// With `full_duplex = true` an idle session must not key either — changed in #1260.
///
/// The flag means "hold the key *across frames*", not "hold it from session start". The watchdog is
/// in-process, so an eager unbounded hold is not merely a hung-repeater risk: a daemon that dies
/// leaves rig_b keyed with nothing to release it (no shutdown handler, and `RigctldPtt` has no
/// `Drop`), and eager keying made that the resting state of an idle unattended §97.221 station.
/// That full duplex still holds one key across real traffic is pinned in
/// `repeater_integration::full_duplex_holds_one_key_across_frames_and_releases_it_at_session_end`.
#[test]
fn full_duplex_idle_session_does_not_hold_ptt_either() {
    let spy = run_session(true);

    assert_eq!(
        spy.edges(),
        Vec::<&str>::new(),
        "a full-duplex session with nothing to relay keyed the transmitter"
    );
    assert!(
        !spy.is_asserted(),
        "transmitter left keyed after a full-duplex session"
    );
}

/// THE #1260 GATE: a transmit failure mid-relay must not leave rig_b keyed.
///
/// `relay_one_frame` asserted, then `?`-returned past its own release on any transmit or ID error.
/// With a real `RigctldPtt` on `[radio.rig_b]` — which has no `Drop` — that left the cross-band rig
/// keyed indefinitely, with no watchdog in this crate to take it back.
#[test]
fn a_failed_relay_transmit_does_not_leave_the_transmitter_keyed() {
    let spy = SpyPtt::default();
    let config = RepeaterConfig {
        enabled: true,
        ..Default::default()
    };
    // RX decodes; TX has no plugin registered, so `transmit` fails after the key is taken.
    let engine_tx = ModemEngine::new(Box::new(LoopbackBackend::new()));
    let (engine_rx, lb_rx) = {
        let lb = LoopbackBackend::new();
        let mut e = ModemEngine::new(Box::new(lb.clone_shared()));
        e.register_plugin(Box::new(BpskPlugin::new()))
            .expect("register");
        (e, lb)
    };
    let mut src = ModemEngine::new(Box::new(lb_rx.clone_shared()));
    src.register_plugin(Box::new(BpskPlugin::new()))
        .expect("register src");
    src.transmit(b"relay frame", "BPSK250", None).expect("tx");

    let mut rp = CrossBandRepeater::new(Box::new(spy.clone()), engine_rx, engine_tx, config);
    // Since #1297 one call is one capture TICK: the burst flushes on the first empty read after the
    // frame, so the transmit — and its failure — happen on a later tick than the one that read it.
    let mut rx = openpulse_modem::capture_ticker::CaptureTicker::new(None);
    let mut err = None;
    for _ in 0..16 {
        match rp.relay_one_frame(&mut rx) {
            Ok(_) => continue,
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    let err = err.expect("the tx engine has no plugin, so the relay must fail within 16 ticks");

    assert!(
        !spy.is_asserted(),
        "the transmitter is still keyed after a failed relay ({err}) — this is the stuck-carrier \
         path #1260 was filed for"
    );
    assert_eq!(
        spy.edges(),
        vec!["assert", "release"],
        "the key must be taken and then released exactly once on the error path"
    );
}

/// Control: a disabled repeater must not key at all, in either mode.
#[test]
fn disabled_repeater_never_keys() {
    for full_duplex in [false, true] {
        let spy = SpyPtt::default();
        let config = RepeaterConfig {
            enabled: false,
            full_duplex,
            ..Default::default()
        };
        let mut rp = CrossBandRepeater::new(Box::new(spy.clone()), engine(), engine(), config);
        let relayed = rp
            .run_full_duplex(Arc::new(AtomicBool::new(false)))
            .expect("a disabled repeater returns Ok(0) immediately");

        assert_eq!(relayed, 0);
        assert_eq!(
            spy.edges(),
            Vec::<&str>::new(),
            "a disabled repeater keyed the transmitter (full_duplex={full_duplex})"
        );
    }
}
