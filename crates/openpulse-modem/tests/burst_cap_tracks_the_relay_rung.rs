//! The RX burst cap must cover the RELAY consumer's rung, not just this station's (#1308).
//!
//! Under #1308's decision the cross-band repeater stops capturing its own audio and reads the bursts
//! the daemon's accumulator flushes. So the cap — which bounds runaway growth, sized from a mode —
//! must cover whatever the repeater is configured to receive, exactly as #1249 made it cover the OTA
//! candidate rungs rather than the configured mode alone.
//!
//! Without that, a station on `mode = "BPSK250"` (cap 37.3 s) relaying `[repeater] mode = "BPSK31"`
//! (66.6 s at one RS block, 131.8 s at two) has **every frame it exists to forward** force-flushed
//! mid-frame. The two modes being DIFFERENT is the whole configuration in which the defect exists;
//! `burst_cap_frame_length.rs` queries the cap with the slow rung itself and cannot see it.

use bpsk_plugin::BpskPlugin;
use openpulse_audio::LoopbackBackend;
use openpulse_core::fec::FecMode;
use openpulse_modem::engine::ModemEngine;

/// What this station is configured to receive.
const CONFIGURED_MODE: &str = "BPSK250";
/// What the repeater is configured to relay — deliberately slower, i.e. a longer frame.
const RELAY_MODE: &str = "BPSK31";
/// Two RS blocks, so the frame is the long case rather than the short one.
const TWO_BLOCK_PAYLOAD: usize = 213;
/// Daemon default `receive_tick_ms = 50` at 8 kHz.
const TICK_SAMPLES: usize = 400;

fn engine() -> (ModemEngine, LoopbackBackend) {
    let backend = LoopbackBackend::new();
    let mut e = ModemEngine::new(Box::new(backend.clone_shared()));
    e.register_plugin(Box::new(BpskPlugin::new())).unwrap();
    (e, backend)
}

fn relay_rung_frame() -> Vec<f32> {
    let (mut tx, bk) = engine();
    tx.transmit_with_fec_mode(
        &vec![0x5Au8; TWO_BLOCK_PAYLOAD],
        RELAY_MODE,
        FecMode::Rs,
        None,
    )
    .expect("transmit the relay rung");
    bk.drain_samples()
}

fn flushes(e: &mut ModemEngine, frame: &[f32]) -> Vec<usize> {
    let mut out = Vec::new();
    for chunk in frame.chunks(TICK_SAMPLES) {
        if let Ok(Some(b)) = e.accumulate_capture(Some(CONFIGURED_MODE), chunk.to_vec()) {
            out.push(b.samples.len());
        }
    }
    for _ in 0..8 {
        if let Ok(Some(b)) = e.accumulate_capture(Some(CONFIGURED_MODE), vec![0.0; TICK_SAMPLES]) {
            out.push(b.samples.len());
        }
    }
    out
}

/// THE GATE: with a relay mode declared, the relay rung's frame survives as ONE burst.
#[test]
fn a_declared_relay_rung_is_not_force_flushed_by_the_configured_modes_cap() {
    let frame = relay_rung_frame();
    let (mut e, _bk) = engine();
    e.set_relay_mode(Some(RELAY_MODE.to_string()));

    let bursts = flushes(&mut e, &frame);
    assert_eq!(
        bursts.len(),
        1,
        "the {RELAY_MODE} frame flushed as {} burst(s) of {bursts:?} samples. The cap was sized from \
         {CONFIGURED_MODE} alone, so a repeater relaying a slower rung has every frame it exists to \
         forward truncated mid-frame (#1308).",
        bursts.len()
    );
    assert!(
        bursts[0] >= frame.len(),
        "the single burst is {} samples for a {}-sample frame — truncated, not whole",
        bursts[0],
        frame.len()
    );
}

/// POSITIVE CONTROL: with NO relay mode declared, the same frame IS force-flushed.
///
/// Without this the gate above could pass because the frame happens to fit the configured mode's cap,
/// in which case it would assert nothing about the relay rung at all.
#[test]
fn without_a_relay_mode_the_same_frame_is_truncated() {
    let frame = relay_rung_frame();
    let (mut e, _bk) = engine();

    let bursts = flushes(&mut e, &frame);
    assert!(
        bursts.len() > 1 || bursts.first().is_some_and(|&n| n < frame.len()),
        "the {RELAY_MODE} frame survived a cap sized from {CONFIGURED_MODE} with no relay mode \
         declared ({bursts:?} for {} samples) — so the gate above is not testing the cap widening, \
         and this fixture cannot distinguish the fix from its absence",
        frame.len()
    );
}

/// Clearing the declaration narrows the cap back.
///
/// A station that has stopped relaying should not keep paying a wider runaway bound. Asserted on the
/// accumulator, not on a getter: an accessor readable only by a test is a public API existing for an
/// instrument, and the reachability ratchet rejects one — which is how the first version of this
/// file was caught.
#[test]
fn clearing_the_relay_mode_narrows_the_cap_back() {
    let frame = relay_rung_frame();

    let (mut e, _bk) = engine();
    e.set_relay_mode(Some(RELAY_MODE.to_string()));
    assert_eq!(
        flushes(&mut e, &frame).len(),
        1,
        "premise: with the rung declared the frame survives whole"
    );

    let (mut e2, _bk2) = engine();
    e2.set_relay_mode(Some(RELAY_MODE.to_string()));
    e2.set_relay_mode(None);
    let bursts = flushes(&mut e2, &frame);
    assert!(
        bursts.len() > 1 || bursts.first().is_some_and(|&n| n < frame.len()),
        "after clearing the relay rung the cap is still wide ({bursts:?} for {} samples) — a station \
         that stopped relaying keeps a bound sized for a consumer that no longer exists",
        frame.len()
    );
}
