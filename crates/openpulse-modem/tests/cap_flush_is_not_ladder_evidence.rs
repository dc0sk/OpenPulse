//! A cap-flushed burst is not evidence about the rate ladder (#1255).
//!
//! `accumulate_routed` returns `Ok(Some(burst))` for two events that mean opposite things: the
//! carrier dropped (the transmission ended, so the slab is a complete frame) and the accumulator hit
//! its runaway cap (the carrier was **still up**, so the slab is not one transmission at all — a
//! stuck channel, or a frame trailed by a long carrier). The caller could not tell them apart.
//!
//! A failed decode of a capped slab therefore drove `RxOutcome::Failed`, which does two things and
//! only one of them is bounded: the NACK keying is capped by `OTA_NACK_BUDGET` in the daemon, but
//! the rate controller's **demotion is not** — successive capped slabs walk `recommended_level` down
//! and the next real ACK carries it to the peer.
//!
//! Everything here drives the production capture entry (`accumulate_capture`), because the flush
//! reason only exists there; handing `ota_decode_burst` a hand-built slab cannot reproduce it.

use bpsk_plugin::BpskPlugin;
use openpulse_audio::LoopbackBackend;
use openpulse_core::profile::SessionProfile;
use openpulse_modem::pipeline::AudioSamples;
use openpulse_modem::ModemEngine;

const MODE: &str = "BPSK250";
const SESSION: &str = "cap-flush";
/// One nominal daemon read: `receive_tick_ms` (50) x the engine's 8 kHz rate.
const TICK: usize = 400;
/// Read size used to reach the cap. The active cap under `hpx_hf` is BPSK31's — **2.39 M samples**,
/// five minutes of audio — because the cap is sized from the OTA candidate set (#1249). Feeding that
/// in 400-sample ticks costs minutes of gate time for no extra coverage: the flush reason is decided
/// by `rx_burst.len() >= cap`, which is independent of how the audio was chunked, and since #1254 the
/// floor estimate is chunking-invariant too. 4096 is itself a realistic read — it is the catch-up
/// drain after a blocking decode (#1301). The carrier-drop control below still uses `TICK`.
const BULK: usize = 4096;
/// Hard bound on how much audio a cap flush may take, so a fixture that never flushes fails loudly
/// instead of hanging. Comfortably above BPSK31's cap.
const MAX_FEED: usize = 1_000_000;

/// An engine with NO OTA session yet — each test starts one after reaching the cap.
///
/// The cap is sized from the OTA candidate set (#1249), so with `hpx_hf` running it is BPSK31's
/// **2.39 M samples**, and `ota_decode_burst` then scans a five-minute slab: measured, that is ~35 s
/// per test. Reaching the cap on the base BPSK250 cap instead (298 k) exercises the identical code —
/// the flush reason is set by `rx_burst.len() >= cap` whichever cap that is — for an eighth of the
/// gate time. The session is started before the decode, which is what actually needs it.
fn engine() -> (ModemEngine, LoopbackBackend) {
    let lb = LoopbackBackend::new();
    let mut e = ModemEngine::new(Box::new(lb.clone_shared()));
    e.register_plugin(Box::new(BpskPlugin::new()))
        .expect("register");
    (e, lb)
}

/// A stuck NARROWBAND carrier — an unmodulated interferer parked in the passband.
///
/// Deliberately not broadband noise: the squelch is driven by a *spectral* floor taken as a low
/// percentile across passband bins, so wideband noise raises the floor with itself and the carrier
/// reads absent within a second (that is #1304's mechanism, measured). A tone lights a handful of
/// bins, leaves the 25th percentile on noise, and therefore holds the detector open — which is what
/// a stuck channel actually looks like and the only way to reach a cap flush at all.
fn stuck_carrier(n: usize) -> Vec<f32> {
    stuck_carrier_at(0, n)
}

/// The same tone starting at absolute sample `off`, so consecutive blocks stay phase-continuous.
fn stuck_carrier_at(off: usize, n: usize) -> Vec<f32> {
    (off..off + n)
        .map(|i| 0.30 * (2.0 * std::f32::consts::PI * 1500.0 * i as f32 / 8000.0).sin())
        .collect()
}

/// Feed in reads of `block`, returning every flushed burst.
fn feed(e: &mut ModemEngine, samples: &[f32], block: usize) -> Vec<AudioSamples> {
    let mut out = Vec::new();
    for chunk in samples.chunks(block) {
        if let Ok(Some(b)) = e.accumulate_capture(Some(MODE), chunk.to_vec()) {
            out.push(b);
        }
    }
    out
}

/// Hold an unbroken carrier until the accumulator flushes it, and return that burst.
///
/// The length is DISCOVERED rather than transcribed: the active cap is the max over the OTA
/// candidate set, which no public accessor exposes, and hard-coding BPSK31's 2.39 M would silently
/// stop testing a cap flush the day the candidate set changes.
fn flush_by_cap(e: &mut ModemEngine, lead: &[f32]) -> AudioSamples {
    if !lead.is_empty() {
        assert!(
            feed(e, lead, BULK).is_empty(),
            "the lead alone flushed a burst; it must not reach the cap on its own"
        );
    }
    let mut fed = lead.len();
    while fed < MAX_FEED {
        let block = stuck_carrier_at(fed, BULK);
        if let Ok(Some(b)) = e.accumulate_capture(Some(MODE), block) {
            return b;
        }
        fed += BULK;
    }
    panic!(
        "fed {MAX_FEED} samples of unbroken carrier without a cap flush — either the squelch \
            never opened (the fixture is wrong) or the cap is larger than this bound"
    );
}

/// THE FIX: a capped slab that does not decode must not move the ladder or key an ACK.
#[test]
fn a_capped_slab_that_fails_to_decode_is_not_ladder_evidence() {
    let (mut e, _lb) = engine();
    let burst = flush_by_cap(&mut e, &[]);
    e.start_ota_session(SessionProfile::hpx_hf());
    let before = e.ota_rx_recommended_level().expect("session started");
    let res = e
        .ota_decode_burst(&burst, SESSION, Some(MODE))
        .expect("decode");

    assert!(
        res.payload.is_none(),
        "the tone decoded as a frame; the fixture is wrong"
    );
    assert!(
        res.ack.is_none(),
        "a cap-flushed slab that decoded nothing produced an ACK frame. In the daemon \
         `ladder_frame = res.ack.is_some()`, so this keys the transmitter and radiates a NACK on a \
         stuck channel (#1178 class) — and drives RxOutcome::Failed into the rate controller, whose \
         demotion is NOT bounded by OTA_NACK_BUDGET the way the keying is."
    );
    assert_eq!(
        e.ota_rx_recommended_level(),
        Some(before),
        "the rate ladder moved on a slab that was never one transmission"
    );
}

/// FALSIFIES THE ISSUE'S PREMISE, and is why the decode must still run.
///
/// #1255 says a capped burst "is never one legitimate frame". It can be: a frame at the head of the
/// slab, followed by carrier that outlasts it, is whole and decodable. And when the squelch sits
/// below the band floor EVERY burst is a cap flush — that is #1254's regime — so skipping the decode
/// on a cap flush would have made the daemon deaf on a hot band rather than merely quieter.
#[test]
fn a_capped_slab_can_still_contain_a_decodable_frame() {
    let (mut e, _lb) = engine();
    let frame = {
        let lb = LoopbackBackend::new();
        let mut tx = ModemEngine::new(Box::new(lb.clone_shared()));
        tx.register_plugin(Box::new(BpskPlugin::new()))
            .expect("register");
        tx.transmit(b"head of a capped slab", MODE, None)
            .expect("tx");
        lb.drain_samples()
    };

    let burst = flush_by_cap(&mut e, &frame);
    e.start_ota_session(SessionProfile::hpx_hf());
    let res = e
        .ota_decode_burst(&burst, SESSION, Some(MODE))
        .expect("decode");

    assert_eq!(
        res.payload.as_deref(),
        Some(&b"head of a capped slab"[..]),
        "a frame at the head of a cap-flushed slab did not decode. Skipping the decode on a cap \
         flush — the other half of #1255's proposal — loses exactly this, and on a hot band where \
         every burst is a cap flush it loses everything."
    );
}

/// POSITIVE CONTROL: the same noise ending in a CARRIER DROP still counts as a failed decode.
///
/// Without this the fix is indistinguishable from "the OTA arm stopped NACKing at all".
#[test]
fn a_carrier_drop_slab_that_fails_to_decode_still_moves_the_ladder() {
    let (mut e, _lb) = engine();
    let mut buf = stuck_carrier(40 * TICK);
    buf.extend(std::iter::repeat_n(0.0f32, 8 * TICK)); // silence → carrier drops → flush

    let bursts = feed(&mut e, &buf, TICK);
    assert!(!bursts.is_empty(), "no burst flushed");
    e.start_ota_session(SessionProfile::hpx_hf());
    let acked = bursts.iter().any(|b| {
        e.ota_decode_burst(b, SESSION, Some(MODE))
            .map(|r| r.ack.is_some())
            .unwrap_or(false)
    });
    assert!(
        acked,
        "a carrier-drop slab that failed to decode produced no ACK — the #1255 fix has suppressed \
         the NACK path wholesale instead of only on cap flushes"
    );
}
