//! The FSK4-ACK scan must reach the whole listen window, and must not starve on a foreign ACK (#1247).
//!
//! `decode_fsk4_ack_in_stream` bounded its trial-decode onset scan at 16 000 samples — 2 s at 8 kHz —
//! inside a window the same function sizes at 4 s, or 9 s when the profile carries the MFSK16
//! sub-floor rung, which `hpx_hf` does. Nothing rescued an ACK past that bound: the whole-buffer
//! decode cannot carry a noisy lead (that is why the scan exists), a normal-rung ACK is transmitted
//! as FSK4 rather than K=3, and a retry opens a fresh buffer instead of accumulating.
//!
//! **Both tests deliver the capture in chunks**, via `LoopbackBackend::push_frame` (one queued frame
//! per read). That is load-bearing: the unpaced loopback returns a whole `fill_samples` capture in
//! ONE read, so the growth-throttled rescan never runs more than once and neither defect below can
//! occur. Every pre-existing ACK test feeds it that way, which is why both survived a green suite.

use fsk4_plugin::Fsk4Plugin;
use mfsk16_plugin::Mfsk16Plugin;
use openpulse_audio::LoopbackBackend;
use openpulse_core::ack::{AckFrame, AckType};
use openpulse_core::profile::SessionProfile;
use openpulse_core::rate::SpeedLevel;
use openpulse_modem::engine::ModemEngine;

/// One nominal FSK4-ACK frame; also the chunk size, so the growth throttle fires once per chunk.
const CHUNK: usize = 4160;
const LISTEN_MS: u64 = 1500;

fn hf_engine() -> (ModemEngine, LoopbackBackend) {
    let backend = LoopbackBackend::new();
    let mut engine = ModemEngine::new(Box::new(backend.clone_shared()));
    engine
        .register_plugin(Box::new(Mfsk16Plugin::new()))
        .unwrap();
    engine.register_plugin(Box::new(Fsk4Plugin::new())).unwrap();
    engine.start_ota_session(SessionProfile::hpx_hf());
    (engine, backend)
}

/// FSK4 ACK audio for `session_id`, recommending a normal rung so `transmit_ota_ack` takes the FSK4
/// branch rather than the K=3 MFSK16 one.
fn ack_audio(session_id: &str, level: SpeedLevel) -> (Vec<f32>, AckFrame) {
    let (mut irs, bk) = hf_engine();
    let ack = AckFrame::new(AckType::AckDown, session_id).with_recommended_level(level);
    irs.transmit_ota_ack(&ack, None).expect("transmit FSK4 ACK");
    (bk.drain_samples(), ack)
}

/// Deliver `capture` one CHUNK per read and listen through the production entry.
fn listen_chunked(capture: &[f32], expected_hash: Option<u16>) -> Option<AckFrame> {
    let (mut iss, bk) = hf_engine();
    for c in capture.chunks(CHUNK) {
        bk.push_frame(c);
    }
    iss.receive_ota_ack_within(None, LISTEN_MS, expected_hash)
        .ok()
}

/// Quiet lead of `n` samples, so the ACK's onset is at a known offset.
fn silence(n: usize) -> Vec<f32> {
    vec![0.0; n]
}

/// Deliberate misalignment so no ACK starts on a CHUNK boundary.
///
/// Load-bearing: `receive_ota_ack_within` also runs the whole-buffer `decode_fsk4_ack` on each raw
/// chunk. A FSK4 ACK is almost exactly one CHUNK long, so a chunk-aligned ACK is decoded by THAT
/// path and the in-stream scan is never exercised — measured, an aligned fixture passes even with
/// the scan sabotaged to restart from zero. Every offset below is therefore odd-sized.
const SKEW: usize = 613;

/// THE CAP: an ACK arriving after the old 2 s bound must still be found.
#[test]
fn an_ack_past_the_old_two_second_bound_is_still_found() {
    let (ack_a, ack) = ack_audio("late", SpeedLevel::Sl2);
    // 30 000 samples ≈ 3.75 s — inside every listen window this profile uses, and past the old cap.
    let mut cap = silence(30_000 + SKEW);
    cap.extend_from_slice(&ack_a);
    cap.extend(silence(2 * CHUNK));

    let got = listen_chunked(&cap, None).expect(
        "no ACK recovered from a capture whose ACK begins at 30 000 samples. The trial-decode scan \
         was capped at 16 000, so an ACK past ~2 s was unreachable by every decoder in the path — \
         the whole-buffer decode cannot carry a lead, and a normal-rung ACK is not sent as K=3.",
    );
    assert_eq!(got.recommended_level, ack.recommended_level);
    assert_eq!(got.ack_type, AckType::AckDown);
}

/// Control: the same listen still finds an EARLY ACK, so the fix did not simply move the blind spot.
#[test]
fn an_early_ack_is_still_found() {
    let (ack_a, ack) = ack_audio("early", SpeedLevel::Sl2);
    let mut cap = silence(CHUNK + SKEW);
    cap.extend_from_slice(&ack_a);
    cap.extend(silence(2 * CHUNK));

    let got = listen_chunked(&cap, None).expect("an ACK near the buffer start must still be found");
    assert_eq!(got.recommended_level, ack.recommended_level);
}

/// THE STARVATION: a foreign-session ACK early in the buffer must not hide a real one behind it.
///
/// The scan returns the FIRST CRC-valid offset. When that ACK failed the caller's session check, the
/// old scan restarted from zero on the next tick and found the SAME foreign ACK again — every tick,
/// for the whole window, deterministically. This is not a probabilistic miss; it is a live lock.
///
/// Only reachable in the session-hash mode: with an ACK-MAC key set (E7) a foreign ACK fails the MAC
/// inside the trial decode and never returns `Some` at all.
#[test]
fn a_foreign_session_ack_does_not_starve_the_listen() {
    let (foreign_a, foreign) = ack_audio("theirs", SpeedLevel::Sl4);
    let (mine_a, mine) = ack_audio("mine", SpeedLevel::Sl2);
    assert_ne!(
        foreign.session_hash, mine.session_hash,
        "fixture premise: the two ACKs must belong to different sessions"
    );

    // BOTH ACKs sit inside the old 16 000-sample cap, deliberately: this test must fail for the
    // resume defect ONLY. With ours past the cap it also fails when the cap is reinstated, and the
    // two mechanisms stop being distinguishable — measured, an earlier version of this fixture did
    // exactly that.
    let mut cap = silence(SKEW);
    cap.extend_from_slice(&foreign_a);
    cap.extend(silence(300));
    cap.extend_from_slice(&mine_a);
    cap.extend(silence(2 * CHUNK));
    assert!(
        SKEW + 2 * foreign_a.len() + 300 < 16_000,
        "fixture premise: both ACKs must lie inside the old cap, or this test cannot isolate the \
         resume defect from the cap defect"
    );

    let got = listen_chunked(&cap, Some(mine.session_hash)).expect(
        "the listen timed out with our ACK present in the buffer. A foreign-session ACK ahead of it \
         won the scan on every tick, because the scan restarted from zero each time instead of \
         resuming past the offset it had already rejected (#1247).",
    );
    assert_eq!(got.session_hash, mine.session_hash);
    assert_eq!(got.recommended_level, mine.recommended_level);
}
