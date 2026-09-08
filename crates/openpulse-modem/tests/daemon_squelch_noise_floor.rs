//! The DAEMON's carrier detector must square with the band it is listening to (REQ-DCD-01).
//!
//! **Why this file exists, and why it is not about any mode.** The receive machinery hardened by
//! #1020/#1021/#1039/#1040/#1045/#1049 — `EnergyGate`, the AFC settle, the preamble-correlation
//! veto, the condemnation recovery — lives entirely in the scanning `receive_with_timeout*` family.
//! The shipped daemon calls **none of it**: `server.rs`'s rx tick uses `accumulate_capture` →
//! `decode_burst` / `ota_decode_burst`. Its only frame-start decision is `DcdState`, and that was
//! created with a **fixed** 0.01 RMS squelch.
//!
//! A real band floor walks straight over a constant. The recorded IC-9700 idle capture measures
//! ≈ 0.126 RMS — **12× that squelch** — so on that band the DCD reads permanently busy, the burst
//! never ends on a carrier drop, and it flushes only when it hits the runaway cap. `decode_burst`
//! then scans just the first few acquisition windows of a cap-length buffer of noise.
//!
//! This is the mode-independent half of the problem, which is where it belongs: level, floor and
//! interference are properties of the environment, not of the waveform. Frame *detection* stays
//! per-waveform (codec2 correlates against per-mode templates too); what must be mode-independent is
//! the criterion, and "is the channel busy" is exactly that.
//!
//! Everything here drives the **production entry** (`accumulate_capture`), never the convenience
//! seam — the repo's standing rule for cross-cutting receive behaviour.

use bpsk_plugin::BpskPlugin;
use openpulse_audio::LoopbackBackend;
use openpulse_dsp::noise_floor::WINDOW;
use openpulse_modem::capture_replay::{load_corpus, Capture};
use openpulse_modem::ModemEngine;

fn engine() -> ModemEngine {
    let lb = LoopbackBackend::new();
    let mut e = ModemEngine::new(Box::new(lb.clone_shared()));
    e.register_plugin(Box::new(BpskPlugin::new())).unwrap();
    e
}

/// Block sizes these tests sweep — chosen to span REGIMES, not to predict the driver (#1254).
///
/// This file previously fed a hard-coded 800, which is above the tracker's 512-sample analysis
/// window and therefore the one regime where the pre-#1254 tracker warmed at all. Both tests below
/// FAIL on pre-#1254 code at the nominal tick size — verified — so the fixture's block size, not the
/// daemon's, was what made this acceptance gate pass.
///
/// The nominal tick IS derived (`receive_tick_ms` x the engine's sample rate). The rest are
/// deliberately NOT a model of the driver: cpal's ALSA host requests a ~25 ms period
/// (`set_period_time_near(25_000)`), but `_near` means the device chooses — under PipeWire's ALSA
/// plugin the period is the graph quantum, ~171 samples at 8 kHz, so reads there are not multiples
/// of 200 at all. Nobody has measured the real distribution (that needs `/proc/asound/…/hw_params`
/// on a listening daemon), so this claims **coverage, not realism**: sizes below, at and above
/// `WINDOW`, including ones that do not divide it.
fn sweep_block_sizes() -> Vec<usize> {
    let tick_ms = openpulse_config::OpenpulseConfig::default()
        .daemon
        .receive_tick_ms as f32;
    let rate = openpulse_core::audio::AudioConfig::default().sample_rate as f32;
    let nominal = ((rate * tick_ms / 1000.0).round() as usize).max(1);
    vec![171, nominal, WINDOW - 1, WINDOW, WINDOW + 1, 4096]
}

fn corpus(name: &str) -> Capture {
    load_corpus(name).unwrap_or_else(|e| panic!("corpus file {name} must load: {e}"))
}

/// Feed `samples` to the production capture entry in read-sized blocks, returning every burst it
/// flushed.
fn feed(e: &mut ModemEngine, mode: &str, samples: &[f32], block: usize) -> Vec<usize> {
    let mut bursts = Vec::new();
    for chunk in samples.chunks(block) {
        if let Ok(Some(b)) = e.accumulate_capture(Some(mode), chunk.to_vec()) {
            bursts.push(b.samples.len());
        }
    }
    bursts
}

/// THE DEFECT: a recorded idle noise floor must not read as a carrier.
///
/// This is the whole bug in one assertion. Nothing is transmitted — the input is 20 s of audio a
/// radio actually produced while nobody was talking. A receiver that calls that a carrier has no
/// squelch at all on that band, and every burst it hands the decoder is noise.
// VERIFIES: REQ-DCD-01 — carrier detect tracks the band noise floor, not a fixed squelch
#[test]
fn a_recorded_idle_floor_is_not_mistaken_for_a_carrier() {
    let hot = corpus("ic9700-idle-hot.wav");
    // Guard the premise: this file exists to BE a floor above the fixed squelch. If it were quiet,
    // the test would pass on any code.
    let rms = hot.mean_sq().sqrt();
    assert!(
        rms > 0.01,
        "recorded floor is {rms:.4} RMS, no longer above the 0.01 fixed squelch — this test's \
         premise is gone"
    );

    for block in sweep_block_sizes() {
        let mut e = engine();
        let cold_squelch = e.dcd_squelch();
        // Feed MORE than the runaway cap. This length is load-bearing: below the cap a
        // permanently-busy receiver accumulates silently and flushes nothing, so a shorter feed
        // passes this test while the defect is fully present — measured, that is exactly what
        // 160 000 samples did.
        let cap = e.burst_cap_samples(Some("BPSK250"));
        let idle = hot.cycled(0, cap + 40_000);
        let bursts = feed(&mut e, "BPSK250", &idle, block);

        // Tripwire: an outcome assertion below says nothing about REQ-DCD-01 unless the adaptive
        // floor actually engaged. Compared against the value BEFORE the feed, not against 0.01 —
        // on a quiet fixture a WARMED tracker clamps to DCD_MIN_SQUELCH_THRESHOLD, which is lower.
        assert_ne!(
            e.dcd_squelch(),
            cold_squelch,
            "the noise-floor tracker never warmed at block size {block}, so the squelch is still \
             the configured default and this test measured nothing about REQ-DCD-01 (#1254)"
        );
        assert!(
            bursts.is_empty(),
            "the receiver flushed {} burst(s) of {:?} samples from PURE RECORDED IDLE at \
             {rms:.4} RMS with {block}-sample reads, against a {cap}-sample runaway cap. The \
             daemon's carrier detector is a fixed threshold, so a band whose floor sits above it \
             reads as permanently busy: the burst never ends on a carrier drop, and the cap alone \
             flushes it — handing the decoder a bufferful of noise and nothing else.",
            bursts.len(),
            bursts
        );
    }
}

/// The other half: raising the squelch must not make the receiver deaf.
///
/// A threshold that adapts to the floor could trivially pass the test above by sitting above
/// everything. This is the negative control — a real frame in that same recorded floor must still
/// produce exactly one burst, and it must be bounded around the frame rather than a cap-length dump.
#[test]
fn a_frame_in_that_same_floor_still_produces_a_bounded_burst() {
    let hot = corpus("ic9700-idle-hot.wav");
    let frame = {
        let lb = LoopbackBackend::new();
        let mut e2 = ModemEngine::new(Box::new(lb.clone_shared()));
        e2.register_plugin(Box::new(BpskPlugin::new())).unwrap();
        e2.transmit(b"adaptive squelch probe", "BPSK250", None)
            .expect("transmit");
        lb.drain_samples()
    };
    assert!(!frame.is_empty(), "fixture frame is empty");

    // SUPERIMPOSE the frame on the noise; do not concatenate it. Appending it put digital silence
    // in the noise bins underneath the frame, so the tracker's floor COLLAPSED to the 0.001 clamp
    // while the frame played and the burst boundary was decided by the floor re-learning the
    // resumed noise afterwards — not by the 1.25 margin this test claims to exercise. Measured
    // before the fix: squelch 0.174 → 0.001 → 0.169 across the frame, with trailing flickers.
    let mut buf = hot.cycled(0, 24_000);
    let base = buf.len();
    buf.extend(hot.cycled(24_000, frame.len() + 24_000));
    for (i, s) in frame.iter().enumerate() {
        buf[base + i] += s * 0.3;
    }

    for block in sweep_block_sizes() {
        let mut e = engine();
        let cold_squelch = e.dcd_squelch();
        let bursts = feed(&mut e, "BPSK250", &buf, block);
        let cap = e.burst_cap_samples(Some("BPSK250"));

        assert_ne!(
            e.dcd_squelch(),
            cold_squelch,
            "the tracker never warmed at block size {block}; this control cannot tell an adaptive \
             squelch from the fixed one it replaced (#1254)"
        );
        // Count only bursts that could plausibly BE the frame. A cold-start or flicker burst of a
        // few hundred samples must not satisfy the negative control — measured, an earlier version
        // of this assertion was satisfied by a 400-sample burst on PURE IDLE, so it could not fail
        // at all below the analysis window.
        //
        // The allowance is one block at each edge — derived from the sampling granularity, not a
        // fitted tolerance. A burst is accumulated in whole reads and the frame ramps, so its first
        // read can fall under the threshold and so can its last. Anything shorter is not the frame.
        let framed: Vec<usize> = bursts
            .iter()
            .copied()
            .filter(|n| n + 2 * block >= frame.len())
            .collect();
        assert!(
            !framed.is_empty(),
            "no burst containing the frame at block size {block} (saw {bursts:?}, frame is {} \
             samples). Two failures land here and the burst lengths tell them apart: on a FIXED \
             squelch the floor keeps the carrier permanently 'present' so the burst never ends and \
             the frame is still unflushed; on an over-raised adaptive squelch the frame never \
             opens it at all. Cap is {cap} samples.",
            frame.len()
        );
        let longest = framed.iter().copied().max().unwrap_or(0);
        assert!(
            longest < cap,
            "the longest burst is {longest} samples at block size {block}, at the {cap}-sample \
             runaway cap — the carrier never 'dropped', so this is the permanently-busy failure \
             wearing a burst's clothes"
        );
    }
}
