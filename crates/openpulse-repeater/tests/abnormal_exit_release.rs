//! A repeater that survives an abnormal exit must not survive still holding rig_b's key.
//!
//! **Why this is not covered by the existing unwind test.** `shared_ptt.rs` already proves a
//! `PttKeyGuard` releases the transmitter when a panic unwinds past it — but that test holds the
//! guard on the STACK. In full duplex this repeater stores the live guard in `self.session_guard`
//! so it can be held across frames, and unwinding does not run `run_full_duplex`'s own
//! `session_guard = None`. rig_b was released anyway only because the thread's closure DROPPED the
//! whole repeater, dropping the guard with it.
//!
//! Catching the panic to hand the repeater back removes that. A panic anywhere between taking the
//! full-duplex key and the next `acquire_key` — the idle `recv_timeout` and the whole `decode_burst`
//! surface, i.e. most of a session — would otherwise return a repeater with rig_b **still keyed**,
//! bounded only by the silence watchdog, and the next enable would `extend()` that stale key.
//!
//! So the existing test is the artificially-easy fixture for this design, and this is the one that
//! matches it: a guard held where the repeater really holds it.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use bpsk_plugin::BpskPlugin;
use openpulse_audio::LoopbackBackend;
use openpulse_modem::pipeline::AudioSamples;
use openpulse_modem::ModemEngine;
use openpulse_radio::{PttController, PttError};
use openpulse_repeater::{CrossBandRepeater, RepeaterConfig};

/// Tracks real key state, so "released" is asserted on the transmitter rather than on a field.
#[derive(Clone, Default)]
struct KeyState {
    asserted: Arc<AtomicBool>,
    releases: Arc<AtomicUsize>,
}
impl PttController for KeyState {
    fn assert_ptt(&mut self) -> Result<(), PttError> {
        self.asserted.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn release_ptt(&mut self) -> Result<(), PttError> {
        self.asserted.store(false, Ordering::SeqCst);
        self.releases.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn is_asserted(&self) -> bool {
        self.asserted.load(Ordering::SeqCst)
    }
}

fn engine() -> ModemEngine {
    let mut e = ModemEngine::new(Box::new(LoopbackBackend::new()));
    e.register_plugin(Box::new(BpskPlugin::new()))
        .expect("register");
    e
}

fn frame() -> Vec<f32> {
    let lb = LoopbackBackend::new();
    let mut src = ModemEngine::new(Box::new(lb.clone_shared()));
    src.register_plugin(Box::new(BpskPlugin::new()))
        .expect("register");
    src.transmit(b"hold the key", "BPSK250", None).expect("tx");
    lb.drain_samples()
}

/// THE GATE: a full-duplex session's stored key is released when the session ends abnormally.
#[test]
fn an_abnormal_exit_releases_a_key_the_session_was_holding() {
    let ptt = KeyState::default();
    let asserted = Arc::clone(&ptt.asserted);
    let (_tx, rx) = std::sync::mpsc::sync_channel(1);
    let mut rp = CrossBandRepeater::new(
        Box::new(ptt),
        engine(),
        engine(),
        rx,
        RepeaterConfig {
            mode: "BPSK250".into(),
            // Full duplex is the whole point: it is what stores the guard on the struct.
            full_duplex: true,
            carrier_sense: false,
            ..Default::default()
        },
    );

    let burst = AudioSamples { samples: frame() };
    rp.relay_burst_at(&burst, 0, None)
        .expect("relay")
        .expect("the burst must relay, or no key is held and this proves nothing");

    // Precondition, asserted rather than assumed: the session really is holding the key.
    assert!(
        asserted.load(Ordering::SeqCst),
        "full duplex did not hold the key across the relay, so the case under test does not exist \
         in this fixture"
    );

    rp.release_after_abnormal_exit();

    assert!(
        !asserted.load(Ordering::SeqCst),
        "the repeater was left holding rig_b keyed after an abnormal exit. Since the thread now \
         hands the repeater back instead of dropping it, nothing else releases this key — the \
         silence watchdog would hold the transmitter up for its full timeout, and a re-enable \
         inside that window extends the stale key rather than taking a fresh one."
    );
    // And it is still usable afterwards: releasing must not wedge it.
    assert!(
        rp.relay_burst_at(&burst, 1_000, None).is_ok(),
        "the repeater could not relay after an abnormal-exit release"
    );
}
