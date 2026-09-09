//! THE #1325 GATE: the repeater must not key rig_b while rig_b's band is busy.
//!
//! **Why this exists.** A cross-band relay is an unattended §97.221 station that transmits on a band
//! it never listens to. Before this it keyed rig_b whenever a burst decoded on the *input* side, so
//! it would double with whatever QSO was already on the output — repeatedly, for as long as traffic
//! kept arriving. `openpulse-mesh`'s auto-transmit capability was REMOVED rather than guarded, and
//! "no carrier sense" was one of the four stated reasons; the repeater had the other three.
//!
//! **The obvious fix was measured and rejected.** `ModemEngine::enable_csma()` exists, but
//! `update_dcd_at_seam` has one caller — the RX capture seam — and the repeater's engines do not
//! capture, so the DCD is permanently `busy=false`. Worse than inert: every `csma_check` refusal
//! would be the 0.3 p-persistence dice, `relay_burst_at` maps it to an error, and `run_full_duplex`
//! breaks on that — measured, the FIRST unlucky roll ends the repeater. What ships instead gives
//! `engine_tx` a real capture on rig_b's own card (#1308 PR 3's `tx_device`) and reads the verdict
//! at the same `InputCapture` seam the rest of the receiver trusts.
//!
//! **Every case here asserts a REFUSAL or a deliberate non-refusal.** A test that only shows a relay
//! happening on a clear band cannot tell carrier sense from a no-op.

use std::sync::Arc;

use bpsk_plugin::BpskPlugin;
use openpulse_audio::LoopbackBackend;
use openpulse_modem::capture_ticker::CaptureTicker;
use openpulse_modem::pipeline::AudioSamples;
use openpulse_modem::ModemEngine;
use openpulse_radio::{PttController, PttError};
use openpulse_repeater::{CrossBandRepeater, RepeaterConfig};

/// A quiet-but-present band: what a clear channel actually sounds like.
///
/// Not silence. An empty capture reads NOTHING, which is the dead-device case the sense deliberately
/// refuses to call "clear" — so a fixture that pushes nothing tests the fault path, not the clear
/// path. Amplitude is well under the DCD's squelch floor.
fn quiet_band() -> Vec<f32> {
    (0..800)
        .map(|i| ((i as f32) * 0.37).sin() * 1.0e-4)
        .collect()
}

/// Counts keying so a "did not transmit" claim rests on the transmitter, not on a return value.
#[derive(Clone, Default)]
struct CountingPtt {
    keys: Arc<std::sync::atomic::AtomicUsize>,
}
impl PttController for CountingPtt {
    fn assert_ptt(&mut self) -> Result<(), PttError> {
        self.keys.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn release_ptt(&mut self) -> Result<(), PttError> {
        Ok(())
    }
    fn is_asserted(&self) -> bool {
        false
    }
}

fn engine_on(lb: &LoopbackBackend) -> ModemEngine {
    let mut e = ModemEngine::new(Box::new(lb.clone_shared()));
    e.register_plugin(Box::new(BpskPlugin::new()))
        .expect("register bpsk");
    e
}

/// One real decodable frame — the thing arriving on the INPUT side.
fn input_frame() -> Vec<f32> {
    let lb = LoopbackBackend::new();
    let mut src = ModemEngine::new(Box::new(lb.clone_shared()));
    src.register_plugin(Box::new(BpskPlugin::new()))
        .expect("register");
    src.transmit(b"relay me", "BPSK250", None).expect("tx");
    let s = lb.drain_samples();
    assert!(!s.is_empty(), "fixture frame is empty");
    s
}

struct Rig {
    repeater: CrossBandRepeater,
    /// The capture the daemon's repeater thread would own. Held here so the test drives the same
    /// object the production path does.
    sensor: CaptureTicker,
    keys: Arc<std::sync::atomic::AtomicUsize>,
    /// rig_b's card — what the repeater senses before keying.
    rig_b_audio: LoopbackBackend,
}

fn rig(carrier_sense: bool) -> Rig {
    let ptt = CountingPtt::default();
    let keys = Arc::clone(&ptt.keys);
    let rig_b_audio = LoopbackBackend::new();
    let (_btx, brx) = std::sync::mpsc::sync_channel(1);
    let repeater = CrossBandRepeater::new(
        Box::new(ptt),
        engine_on(&LoopbackBackend::new()),
        engine_on(&rig_b_audio),
        brx,
        RepeaterConfig {
            mode: "BPSK250".into(),
            tx_hang_ms: 0,
            full_duplex: false,
            carrier_sense,
            ..Default::default()
        },
    );
    Rig {
        repeater,
        sensor: CaptureTicker::new(None),
        keys,
        rig_b_audio,
    }
}

fn relay(r: &mut Rig, frame: &[f32]) -> Option<usize> {
    r.repeater
        .relay_burst(
            &AudioSamples {
                samples: frame.to_vec(),
            },
            Some(&mut r.sensor),
        )
        .expect("relay must not error")
}

/// THE GATE: a decodable input frame must NOT key rig_b while rig_b's band carries a signal.
#[test]
fn a_busy_output_band_stops_the_relay_from_keying() {
    let frame = input_frame();
    let mut r = rig(true);
    // rig_b's card is carrying somebody else's transmission. Push several reads' worth so the sense
    // finds it whichever tick it lands on.
    for _ in 0..8 {
        r.rig_b_audio.push_frame(&frame);
    }

    let relayed = relay(&mut r, &frame);

    assert_eq!(
        relayed, None,
        "the burst was relayed onto a busy output band"
    );
    assert_eq!(
        r.keys.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "rig_b was KEYED while its band was busy — the station doubled with whoever was already \
         there. This is the §97.221 case #1325 was filed for; assert on the transmitter, because a \
         return value alone cannot tell you the key stayed down."
    );
    assert_eq!(
        r.repeater.bursts_deferred(),
        1,
        "the deferral must be counted, or a busy band is indistinguishable from a burst that \
         simply failed to decode"
    );
}

/// THE CONTROL that stops the gate above passing for the wrong reason: with sensing OFF, the very
/// same busy band and the very same burst DO key the rig.
///
/// Without this, a repeater that had silently stopped relaying altogether would pass.
#[test]
fn the_same_burst_and_band_relay_when_sensing_is_off() {
    let frame = input_frame();
    let mut r = rig(false);
    for _ in 0..8 {
        r.rig_b_audio.push_frame(&frame);
    }

    let relayed = relay(&mut r, &frame);

    assert!(
        relayed.is_some(),
        "with carrier_sense = false the identical burst must still relay — otherwise the gate above \
         proves nothing about SENSING, only that this fixture never relays"
    );
    assert!(
        r.keys.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "rig_b was never keyed even with sensing off"
    );
    assert_eq!(
        r.repeater.bursts_deferred(),
        0,
        "nothing was deferred, because nothing was sensed"
    );
}

/// A full-duplex session that already holds the key must keep relaying: sensing governs channel
/// ACQUISITION, not continuation.
///
/// This is the hole that disqualified the CAT S-meter design — a station that sensed while keyed
/// would read its own carrier as busy and fall silent after one frame.
#[test]
fn a_full_duplex_session_does_not_sense_against_its_own_carrier() {
    let frame = input_frame();
    let ptt = CountingPtt::default();
    let keys = Arc::clone(&ptt.keys);
    let rig_b_audio = LoopbackBackend::new();
    let (_btx, brx) = std::sync::mpsc::sync_channel(1);
    let mut rp = CrossBandRepeater::new(
        Box::new(ptt),
        engine_on(&LoopbackBackend::new()),
        engine_on(&rig_b_audio),
        brx,
        RepeaterConfig {
            mode: "BPSK250".into(),
            full_duplex: true,
            carrier_sense: true,
            ..Default::default()
        },
    );

    // First frame: the band is quiet but readable, so the key is taken and HELD.
    //
    // Exactly ONE quiet block, not a queue of them. `push_frame` hands back one block per read, so
    // a backlog left here would still be sitting in front of the busy signal pushed below and the
    // second sense would read *quiet* — which is how this test first passed with the
    // skip-while-keyed rule sabotaged, i.e. while proving nothing. One block is enough: the sense
    // needs one non-empty read to call the band readable, and later empty reads do not un-prove it.
    rig_b_audio.push_frame(&quiet_band());
    let mut sensor = CaptureTicker::new(None);
    let first = rp
        .relay_burst(
            &AudioSamples {
                samples: frame.clone(),
            },
            Some(&mut sensor),
        )
        .expect("relay");
    assert!(
        first.is_some(),
        "the first frame should relay on a clear band"
    );

    // Now rig_b's card carries our own held carrier. The second frame must still relay.
    for _ in 0..8 {
        rig_b_audio.push_frame(&frame);
    }
    let second = rp
        .relay_burst(
            &AudioSamples {
                samples: frame.clone(),
            },
            Some(&mut sensor),
        )
        .expect("relay");

    assert!(
        second.is_some(),
        "a full-duplex session stopped relaying once its own transmission appeared on rig_b's \
         card. Sensing must be skipped while the session already holds the key, or full duplex \
         relays exactly one frame and then goes silent forever."
    );
    assert_eq!(
        rp.bursts_deferred(),
        0,
        "a session holding the key must defer nothing"
    );
    assert!(keys.load(std::sync::atomic::Ordering::SeqCst) > 0);
}

/// Sensing must not be satisfiable by a dead capture: silence from a device that is not working is
/// not evidence the band is clear.
#[test]
fn an_unreadable_band_is_treated_as_busy_not_as_clear() {
    let frame = input_frame();
    let mut r = rig(true);
    // Nothing is ever pushed to rig_b's card, so every read returns empty.
    let relayed = relay(&mut r, &frame);

    assert_eq!(
        relayed, None,
        "a band that could not be read was treated as CLEAR — an unattended station transmitting \
         blind is exactly the failure this gate exists to prevent"
    );
    assert_eq!(r.keys.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(r.repeater.bursts_deferred(), 1);
}
