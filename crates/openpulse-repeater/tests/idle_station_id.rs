//! §97.119(a): a repeater that has transmitted must identify even when nothing is being relayed.
//!
//! **The defect.** `maybe_identify` transmits under the CALLER's key and its only caller was
//! `relay_burst_at`, so the ID rode relay traffic. A repeater that relayed a frame and then heard
//! silence never identified: the end-of-communication half of §97.119(a) was unreachable
//! (`signoff_idle_ms` was 0, so `signoff_due` was permanently false), and the interval half fired
//! only by accident of the next relay happening after the interval had elapsed.
//!
//! **The key is the sharp edge.** An idle ID has no relay to ride, so it must take its own key —
//! calling `maybe_identify` from the tick as-is would transmit into an unkeyed rig in half duplex,
//! which is the defect class #1260 and the keyed-transmit audits exist to prevent. Every assertion
//! here is on the PTT spy, not on a return value.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use bpsk_plugin::BpskPlugin;
use openpulse_audio::LoopbackBackend;
use openpulse_modem::pipeline::AudioSamples;
use openpulse_modem::ModemEngine;
use openpulse_radio::{PttController, PttError};
use openpulse_repeater::{CrossBandRepeater, RepeaterConfig};

const MODE: &str = "BPSK250";
const INTERVAL_S: u64 = 600;
const SIGNOFF_S: u64 = 10;

/// Records keying and whether the rig was keyed at the moment audio was emitted.
#[derive(Clone, Default)]
struct KeySpy {
    asserted: Arc<AtomicBool>,
    keys: Arc<AtomicUsize>,
}
impl PttController for KeySpy {
    fn assert_ptt(&mut self) -> Result<(), PttError> {
        self.asserted.store(true, Ordering::SeqCst);
        self.keys.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn release_ptt(&mut self) -> Result<(), PttError> {
        self.asserted.store(false, Ordering::SeqCst);
        Ok(())
    }
    fn is_asserted(&self) -> bool {
        self.asserted.load(Ordering::SeqCst)
    }
}

/// An output stream that refuses to emit while the rig is unkeyed — so an unkeyed ID is a hard
/// failure here rather than something a reviewer has to notice.
struct KeyCheckedOut {
    asserted: Arc<AtomicBool>,
    emitted: Arc<AtomicUsize>,
    unkeyed_emissions: Arc<AtomicUsize>,
}
impl openpulse_core::audio::AudioOutputStream for KeyCheckedOut {
    fn write(&mut self, samples: &[f32]) -> Result<(), openpulse_core::error::AudioError> {
        if !samples.is_empty() {
            self.emitted.fetch_add(1, Ordering::SeqCst);
            if !self.asserted.load(Ordering::SeqCst) {
                self.unkeyed_emissions.fetch_add(1, Ordering::SeqCst);
            }
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<(), openpulse_core::error::AudioError> {
        Ok(())
    }
    fn close(self: Box<Self>) {}
}

#[derive(Clone)]
struct KeyCheckedBackend {
    asserted: Arc<AtomicBool>,
    emitted: Arc<AtomicUsize>,
    unkeyed_emissions: Arc<AtomicUsize>,
}
impl openpulse_core::audio::AudioBackend for KeyCheckedBackend {
    fn name(&self) -> &str {
        "KeyChecked"
    }
    fn list_devices(
        &self,
    ) -> Result<Vec<openpulse_core::audio::DeviceInfo>, openpulse_core::error::AudioError> {
        Ok(vec![])
    }
    fn open_input(
        &self,
        _d: Option<&str>,
        _c: &openpulse_core::audio::AudioConfig,
    ) -> Result<Box<dyn openpulse_core::audio::AudioInputStream>, openpulse_core::error::AudioError>
    {
        Err(openpulse_core::error::AudioError::Stream("no input".into()))
    }
    fn open_output(
        &self,
        _d: Option<&str>,
        _c: &openpulse_core::audio::AudioConfig,
    ) -> Result<Box<dyn openpulse_core::audio::AudioOutputStream>, openpulse_core::error::AudioError>
    {
        Ok(Box::new(KeyCheckedOut {
            asserted: Arc::clone(&self.asserted),
            emitted: Arc::clone(&self.emitted),
            unkeyed_emissions: Arc::clone(&self.unkeyed_emissions),
        }))
    }
}

fn frame() -> Vec<f32> {
    let lb = LoopbackBackend::new();
    let mut src = ModemEngine::new(Box::new(lb.clone_shared()));
    src.register_plugin(Box::new(BpskPlugin::new()))
        .expect("register");
    src.transmit(b"relay me", MODE, None).expect("tx");
    lb.drain_samples()
}

struct Rig {
    rp: CrossBandRepeater,
    /// Live key state, so "the carrier went down" is asserted on the transmitter rather than on a
    /// struct field the repeater would have to expose only for a test.
    asserted: Arc<AtomicBool>,
    keys: Arc<AtomicUsize>,
    emitted: Arc<AtomicUsize>,
    unkeyed: Arc<AtomicUsize>,
}

fn rig(full_duplex: bool) -> Rig {
    let spy = KeySpy::default();
    let keys = Arc::clone(&spy.keys);
    let asserted = Arc::clone(&spy.asserted);
    let asserted_out = Arc::clone(&spy.asserted);
    let emitted = Arc::new(AtomicUsize::new(0));
    let unkeyed = Arc::new(AtomicUsize::new(0));
    let backend = KeyCheckedBackend {
        asserted,
        emitted: Arc::clone(&emitted),
        unkeyed_emissions: Arc::clone(&unkeyed),
    };
    let mut engine_tx = ModemEngine::new(Box::new(backend));
    engine_tx
        .register_plugin(Box::new(BpskPlugin::new()))
        .expect("register");
    let mut engine_rx = ModemEngine::new(Box::new(LoopbackBackend::new()));
    engine_rx
        .register_plugin(Box::new(BpskPlugin::new()))
        .expect("register");
    let (_tx, rx) = std::sync::mpsc::sync_channel(1);
    Rig {
        asserted: asserted_out,
        rp: CrossBandRepeater::new(
            Box::new(spy),
            engine_rx,
            engine_tx,
            rx,
            RepeaterConfig {
                mode: MODE.into(),
                tx_hang_ms: 0,
                full_duplex,
                callsign: "N0CALL".into(),
                id_interval_secs: INTERVAL_S,
                id_signoff_idle_secs: SIGNOFF_S,
                carrier_sense: false,
            },
        ),
        keys,
        emitted,
        unkeyed,
    }
}

/// THE GATE: after relaying and then going quiet, the sign-off ID goes out — keyed.
#[test]
fn an_idle_repeater_transmits_the_signoff_id_under_its_own_key() {
    let mut r = rig(false);
    let burst = AudioSamples { samples: frame() };
    r.rp.relay_burst_at(&burst, 0, None)
        .expect("relay")
        .expect("the burst must relay, or nothing is owed and this proves nothing");
    let keys_after_relay = r.keys.load(Ordering::SeqCst);

    // Nothing more arrives. Past the sign-off idle window, an ID is owed.
    let identified =
        r.rp.identify_if_due_at((SIGNOFF_S + 1) * 1_000)
            .expect("the idle ID must not error");

    assert!(
        identified,
        "no station ID was sent after {SIGNOFF_S}s of silence following a transmission. \
         §97.119(a) requires identification at the END of a communication, and this repeater's \
         only ID path rode relay traffic — so a station that relays once and then hears nothing \
         never identifies at all."
    );
    assert!(
        r.keys.load(Ordering::SeqCst) > keys_after_relay,
        "the idle ID did not take a key of its own"
    );
    assert_eq!(
        r.unkeyed.load(Ordering::SeqCst),
        0,
        "audio was emitted while the rig was UNKEYED — `maybe_identify` transmits under the \
         caller's key, and an idle ID has no relay to ride"
    );
    assert!(r.emitted.load(Ordering::SeqCst) > 0, "nothing was emitted");
}

/// The interval ID is owed too, on the same idle path.
#[test]
fn an_idle_repeater_transmits_the_interval_id() {
    let mut r = rig(false);
    let burst = AudioSamples { samples: frame() };
    r.rp.relay_burst_at(&burst, 0, None).expect("relay");

    assert!(
        r.rp.identify_if_due_at((INTERVAL_S + 1) * 1_000)
            .expect("idle ID"),
        "no interval ID after {INTERVAL_S}s"
    );
    assert_eq!(r.unkeyed.load(Ordering::SeqCst), 0, "unkeyed ID");
}

/// TERMINATION: having identified, an idle repeater must go quiet rather than ID every tick.
///
/// `mark_identified` clears `tx_since_id`, which both `id_due` and `signoff_due` require. This is
/// the assertion that would catch a 10 Hz ID loop — the failure mode if the arming bit were left set.
#[test]
fn an_idle_repeater_does_not_identify_again_until_it_transmits_again() {
    let mut r = rig(false);
    let burst = AudioSamples { samples: frame() };
    r.rp.relay_burst_at(&burst, 0, None).expect("relay");
    assert!(r.rp.identify_if_due_at(20_000).expect("first ID"));

    let keys_after_id = r.keys.load(Ordering::SeqCst);
    for tick in 1..=50 {
        assert!(
            !r.rp
                .identify_if_due_at(20_000 + tick * 100)
                .expect("subsequent ticks"),
            "the repeater identified again at tick {tick} without having transmitted in between — \
             at the 100 ms idle tick that is an ID roughly ten times a second"
        );
    }
    assert_eq!(
        r.keys.load(Ordering::SeqCst),
        keys_after_id,
        "the transmitter was keyed again while nothing was owed"
    );
}

/// A sign-off ID ends the full-duplex hold instead of extending it.
///
/// Sign-off fires 10 s after the last transmission while the full-duplex carrier is held for 180 s
/// of silence. Extending it there would re-stamp the watchdog and prolong a DEAD carrier to 190 s
/// past the last relay; the sign-off IS the end-of-communication marker, so the key goes down.
#[test]
fn a_signoff_id_drops_the_full_duplex_carrier_rather_than_extending_it() {
    let mut r = rig(true);
    let burst = AudioSamples { samples: frame() };
    r.rp.relay_burst_at(&burst, 0, None).expect("relay");

    assert!(
        r.rp.identify_if_due_at((SIGNOFF_S + 1) * 1_000)
            .expect("idle ID"),
        "no sign-off ID in full duplex"
    );
    assert!(
        !r.asserted.load(Ordering::SeqCst),
        "the full-duplex carrier is still keyed after the sign-off ID. That ID marks the end of the \
         communication, so nothing is left to hold it up — extending instead would keep a dead \
         carrier on the air for another full watchdog timeout."
    );
}
