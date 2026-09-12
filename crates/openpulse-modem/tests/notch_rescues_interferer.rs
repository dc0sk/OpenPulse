//! The receiver notch must earn its default-on status: it has to rescue a decode that fails without
//! it, and the rescue has to be *attributable to the interferer* (REQ-QRM-01, whose bullet in
//! `docs/dev/requirements.md` carries the provenance and the only dated measurements).
//!
//! There is no verdict table here, deliberately. The rescue set is **not an interval** — a louder
//! tone is easier for the detector to see and harder for the demodulator to survive at the same
//! time — so there is no width to pin, and the gates assert that a rescue EXISTS and is attributable,
//! never that it happens at a particular amplitude. `probe_band_sweep` is the instrument to re-run.
//!
//! **The previous version of this file was right about its verdicts and wrong about its mechanism,
//! which is the lesson.** It recorded a rescue at one amplitude; that reproduces exactly. But
//! instrumenting what the notch actually did showed the 2200 Hz interferer was **never notched at
//! any amplitude**: it sat inside the receiver's own protected band, which `peaks_from_spectrum`
//! skips structurally, because `receive_with_timeout_fec_inner` did not record `rx_mode` and the
//! band fell back to `notch_fallback_bw_hz` (2000 Hz) — 4x BPSK250's real occupied width. The
//! rescue was real; **what produced it is not established**, only that it was not the interferer.
//!
//! Two consequences. A decode-outcome assertion cannot see any of that, so the rescue is now
//! conjoined with attribution (a notch placed ON the interferer) and causation (removing only the
//! interferer suffices). And resting on an unidentified effect left no margin, so the gate was a
//! single point on a decode cliff that the #1148 keystream change tipped. With the band repaired the
//! rescue returns and the notch places its highest-prominence notch on the interferer.

use std::time::Duration;

use bpsk_plugin::BpskPlugin;
use openpulse_audio::LoopbackBackend;
use openpulse_core::fec::FecMode;
use openpulse_modem::capture_replay::{load_corpus, Capture};
use openpulse_modem::channel_sim::ChannelSimHarness;
use openpulse_modem::ModemEngine;

/// The probe payload. `PROBE_PAYLOAD_LEN` makes the size a variable, so the expected bytes MUST be
/// derived here rather than written as a literal — a hardcoded 18-byte literal made every
/// larger-payload probe report FAIL whether or not the frame decoded (#1148 triage).
fn payload() -> Vec<u8> {
    // PAYLOAD SIZE IS A VARIABLE HERE, not a detail. An 18-byte payload is zero-padded into a
    // 255-byte RS block, and an additive scrambler puts that padding on the air as RAW KEYSTREAM —
    // so ~76 % of the wire block is keystream. `PROBE_PAYLOAD_LEN` lets the band probe ask what a
    // representative frame does (180 B leaves 13 %).
    let n: usize = std::env::var("PROBE_PAYLOAD_LEN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(18);
    (0..n).map(|i| b"notch rescue probe "[i % 19]).collect()
}

fn frame() -> Vec<f32> {
    let lb = LoopbackBackend::new();
    let mut e = ModemEngine::new(Box::new(lb.clone_shared()));
    e.register_plugin(Box::new(BpskPlugin::new())).unwrap();
    e.transmit_with_fec_mode(&payload(), "BPSK250", FecMode::Rs, None)
        .expect("transmit");
    lb.drain_samples()
}

/// Ladder of interferer amplitudes searched for a rescue, ordered by measured likelihood so the
/// search short-circuits early. NOT pinned operating points: the rescue set is not an interval (see
/// REQ-QRM-01 for the dated measurement), so the gate asserts a rescue EXISTS and is attributable,
/// never that it happens at a particular level. 0.35 is the third rung rather than 0.40 because 0.40
/// was measured not to rescue.
///
/// Its LENGTH is the search cap: every entry is tried, so the panic message cannot claim to have
/// searched an amplitude it never reached. Bounding the sweep in POINTS rather than per-point work
/// keeps each verdict meaning what it means (the whole #1066 work budget applies to every point);
/// it only refuses to hunt forever. The worst case costs three full-budget failures, and is reached
/// only when the notch is genuinely broken — which is when the time is worth spending.
const RESCUE_LADDER: [f32; 3] = [0.30, 0.60, 0.35];

/// Well outside BPSK250's occupied band at fc 1500, so it is notchable rather than a QSY case.
const INTERFERER_HZ: f32 = 2_200.0;

/// Dead centre of BPSK250's 1250–1750 Hz occupied band: structurally un-notchable, a QSY case.
const IN_BAND_HZ: f32 = 1_500.0;

/// How much work a run may spend.
///
/// A parameter, never an env var read inside the run: libtest runs tests as threads in ONE process,
/// so an env-var budget set by one test would silently change another's verdict.
#[derive(Clone, Copy, PartialEq)]
enum Budget {
    /// The #1066 work budget. The only setting under which a DECODE verdict means anything.
    Full,
    /// Enough for the notch to see blocks, not to find a frame. Valid ONLY for questions about what
    /// the notch did; any `decoded` from it is meaningless and must not be asserted on.
    Engagement,
}

/// What one capture did, beyond whether it decoded.
struct Run {
    decoded: bool,
    freqs_seen: Vec<f32>,
    blocks: u64,
    /// `(lo_min, lo_max, hi_min, hi_max)` over every slice — an envelope, because the band moves
    /// with the AFC correction and a snapshot would dress one sample as universal.
    protect: Option<(f32, f32, f32, f32)>,
}

impl Run {
    /// Whether a notch was placed within 60 Hz of `hz` in ANY slice (a union, not a snapshot).
    fn notched_near(&self, hz: f32) -> Vec<f32> {
        self.freqs_seen
            .iter()
            .copied()
            .filter(|f| (f - hz).abs() <= 60.0)
            .collect()
    }
}

/// The frame buried in recorded hot floor, plus `tones` as `(hz, amplitude)`.
fn build_buffer(tones: &[(f32, f32)]) -> Vec<f32> {
    let hot = load_corpus("ic9700-idle-hot.wav").expect("corpus");
    let f = frame();
    let mut buf = hot.cycled(0, 40_000);
    buf.extend(f.iter().map(|s| s * 0.3));
    buf.extend(hot.cycled(40_000, 40_000));
    for (n, s) in buf.iter_mut().enumerate() {
        for &(hz, amp) in tones {
            *s += amp * (2.0 * std::f32::consts::PI * hz * n as f32 / 8_000.0).cos();
        }
    }
    buf
}

/// Remove `freqs` from `buf` with a MANUALLY placed notch bank, outside the engine.
///
/// The causation arm: the engine's own notch decides *what* to notch, so a rescue with it enabled
/// only shows that removing SOMETHING helped — which is exactly how this gate passed for its whole
/// life while never touching the interferer. Pre-filtering only the interferer, then decoding with
/// the engine notch OFF, tests whether removing THAT tone suffices.
fn prefilter(buf: &[f32], freqs: &[f32]) -> Vec<f32> {
    use openpulse_dsp::notch::{NotchBank, NotchMode, NotchParams};
    // Same params the engine builds its bank with (`ModemEngine::new` → `NotchParams::default()`),
    // so this removes the same SHAPE of notch. A test that called `configure_notch` would break
    // that mirror and the arm would compare two different filters.
    let mut bank = NotchBank::new(NotchParams::default());
    bank.set_mode(NotchMode::Fixed);
    bank.set_notch_freqs(freqs);
    let mut out = Vec::with_capacity(buf.len());
    for chunk in buf.chunks(4096) {
        out.extend(bank.process_block(chunk));
    }
    out
}

/// Decode one prepared buffer, reporting what the notch did as well as whether it decoded.
fn run_buffer(notch: bool, buf: Vec<f32>, budget: Budget) -> Run {
    let mut h = ChannelSimHarness::new();
    for eng in [&mut h.tx_engine, &mut h.rx_engine] {
        eng.register_plugin(Box::new(BpskPlugin::new())).unwrap();
    }
    if notch {
        h.rx_engine.enable_notch();
    }
    // #1066: the receive verdict was bounded by WALL CLOCK, so the same input decoded 5/5 on an idle
    // machine and 0/5 on eight busy cores. Bound the search in WORK instead, so this asserts a
    // property of the signal rather than of the host. The budget reconciles every fixture in the
    // #1058 family (PR #1070); it is chosen, not derived.
    //
    // `PROBE_CHEAP_BUDGET` shrinks it for ENGAGEMENT-only questions (did the notch fire?), which do
    // not need a decode. It invalidates the decode verdict, so the probe prints DECODE-INVALID and
    // no gate sets it.
    let (pos, iters) = match budget {
        Budget::Full => (8_000, 64_000),
        Budget::Engagement => (200, 2_000),
    };
    h.rx_engine.set_deterministic_scan_positions(Some(pos));
    h.rx_engine.set_deterministic_max_iterations(Some(iters));
    h.feed_capture(&Capture {
        samples: buf,
        sample_rate: 8_000,
    });
    let decoded = h
        .rx_engine
        .receive_with_fec_mode_timeout("BPSK250", FecMode::Rs, None, Duration::from_millis(40_000))
        .map(|got| got == payload())
        .unwrap_or(false);
    Run {
        decoded,
        freqs_seen: h.rx_engine.notch_freqs_seen(),
        blocks: h.rx_engine.notch_blocks_processed(),
        protect: h.rx_engine.notch_protect_extremes(),
    }
}

/// Decode with the standard single out-of-band interferer at `amplitude`.
fn decodes_with(notch: bool, amplitude: f32) -> bool {
    run_buffer(
        notch,
        build_buffer(&[(INTERFERER_HZ, amplitude)]),
        Budget::Full,
    )
    .decoded
}

/// THE GATE: the notch rescues a decode that fails without it, AND the rescue is attributable to the
/// interferer rather than to whatever else the notch happened to remove.
///
/// Four legs, because the first alone is what passed for a year on the wrong mechanism:
///  1. BASELINE — the fixture decodes with no tone and no notch, so an off-arm failure is evidence
///     about the interferer and not about the hot floor having gone marginal.
///  2. EXISTS — some ladder amplitude decodes with the notch on and fails with it off.
///  3. ATTRIBUTION — at that amplitude the notch actually placed a notch ON the interferer.
///  4. CAUSATION — removing ONLY the interferer, with the engine notch off, is itself enough to
///     decode. Without this, 2+3 are still only correlation: the notch could be placing a notch on
///     the interferer while the rescue comes from the 17 birdies it removes at the same time.
#[test]
#[ignore = "SLOW (~35 min): opt-in since #1274. Run with `scripts/slow-tests.sh`, or `cargo test -p openpulse-modem --no-default-features --test notch_rescues_interferer -- --ignored`."]
fn the_notch_rescues_a_decode_that_fails_without_it() {
    assert!(
        run_buffer(false, build_buffer(&[]), Budget::Full).decoded,
        "BASELINE BROKEN: the fixture does not decode with NO interferer and NO notch, so nothing \
         below is evidence about interference. Re-derive the fixture before reading any rescue."
    );

    let mut tried = Vec::new();
    let mut rescue = None;
    for &amp in RESCUE_LADDER.iter() {
        let on = run_buffer(true, build_buffer(&[(INTERFERER_HZ, amp)]), Budget::Full);
        tried.push(amp);
        if on.decoded {
            rescue = Some((amp, on));
            break;
        }
    }
    let (amp, on) = rescue.unwrap_or_else(|| {
        panic!(
            "the notch rescued at NONE of the {} amplitudes tried ({tried:?}). Either the notch \
             regressed, or the operating band moved off this ladder — re-derive it with \
             `probe_band_sweep` (AMPS=... NOTCH=on,off) and reorder RESCUE_LADDER, rather than \
             pinning whatever single amplitude happens to work today.",
            tried.len()
        )
    });

    assert!(
        !decodes_with(false, amp),
        "the decode SUCCEEDED without the notch at interferer amplitude {amp}, so the rescue above \
         proves nothing. Re-derive the level at which the interferer actually breaks acquisition."
    );

    let near = on.notched_near(INTERFERER_HZ);
    assert!(
        !near.is_empty(),
        "ATTRIBUTION FAILED: the notch rescued the decode at amplitude {amp} without ever placing \
         a notch within 60 Hz of the {INTERFERER_HZ} Hz interferer (it placed {} distinct \
         frequencies across {} slices: {:?}). That is how this gate passed for its whole life — by \
         incidentally stripping RFI birdies out of the recorded floor while the tone it names went \
         untouched. A decode outcome alone cannot see this.",
        on.freqs_seen.len(),
        on.blocks,
        on.freqs_seen
    );

    let filtered = prefilter(&build_buffer(&[(INTERFERER_HZ, amp)]), &[INTERFERER_HZ]);
    assert!(
        run_buffer(false, filtered, Budget::Full).decoded,
        "CAUSATION FAILED: removing ONLY the {INTERFERER_HZ} Hz interferer (manual notch, engine \
         notch off) does not restore the decode at amplitude {amp}, yet the engine notch rescues \
         it. So the rescue is NOT attributable to removing the interferer — the engine notch is \
         doing something else useful, and REQ-QRM-01's claim needs restating to match."
    );
}

/// THE HONEST LIMIT: in-band QRM is a QSY case, and the notch does not make it worse.
///
/// Replaces a test that pinned an amplitude at which the interferer defeats the notch — a construct
/// that could only fire on the notch getting BETTER (which it did, once the protected band was
/// repaired) or on cliff drift, never on a defect.
///
/// PAIRED-TONE: equal-amplitude tones in-band (1500 Hz, dead centre) and out-of-band (2200 Hz). The
/// out-of-band one is the POSITIVE CONTROL — without it, any claim about the in-band tone is an
/// absence read through a filter never shown to detect anything.
///
/// **What this deliberately does NOT assert.** An earlier version asserted that no notch is ever
/// placed near 1500 Hz. That is a FALSE INVARIANT: on this path the `InputCapture` seam runs inside
/// each decode attempt with that attempt's *trial* `afc_correction_hz` — corrections the scan rolls
/// back on failure (#1123's rolled-back-`AfcUpdate` flood) — so `notch_freqs_seen` and
/// `notch_protect_extremes` union over hypotheses, not receiver state. Measured 2026-08-22: the
/// in-band tone ALONE leaves the AFC at exactly 0 with the band exactly nominal and nothing notched
/// near it; the out-of-band tone alone pulls trials to +248..+352 Hz; only the PAIR reaches
/// +324..+378, lifting the band's lower edge past 1500 in attempts that are demodulating 300+ Hz off
/// the signal and fail whatever the notch does. The daemon cannot exhibit this by construction (call
/// graph, not measured): `accumulate_routed` runs the seam once at capture with the committed
/// correction, and `decode_burst` sets `input_prerouted` to suppress it per slice.
///
/// **No-harm is RECORDED here, not asserted, because at this level it would be a tautology.**
/// Measured 2026-08-22 at full budget: notch ON → FAIL, OFF → FAIL. Equal, so the notch costs
/// nothing even here — but an assertion of the form "ON decoded OR OFF did not" is trivially true
/// whenever OFF fails, and buying that tautology costs two full-budget failures. The non-vacuous
/// no-harm assertion is `the_notch_costs_nothing_when_there_is_nothing_to_notch`, where OFF
/// genuinely succeeds and ON is therefore obliged to.
#[test]
#[ignore = "SLOW (~35 min): opt-in since #1274. Run with `scripts/slow-tests.sh`, or `cargo test -p openpulse-modem --no-default-features --test notch_rescues_interferer -- --ignored`."]
fn in_band_qrm_is_a_qsy_case_and_the_notch_does_not_worsen_it() {
    // ENGAGEMENT budget: this asserts what the DETECTOR did, not whether the frame decoded, and a
    // decode verdict is not read from it. That keeps the test to ~1 min instead of two full-budget
    // failures (~25 min each) bought for nothing — see the no-harm note below.
    let r = run_buffer(
        true,
        build_buffer(&[(IN_BAND_HZ, 0.30), (INTERFERER_HZ, 0.30)]),
        Budget::Engagement,
    );
    assert!(
        r.blocks > 0,
        "the notch never processed a block, so this test measured nothing about it"
    );
    assert!(
        !r.notched_near(INTERFERER_HZ).is_empty(),
        "POSITIVE CONTROL FAILED: the notch placed nothing near the OUT-of-band {INTERFERER_HZ} Hz \
         tone, so any conclusion about the in-band tone is an absence read through a filter that \
         has not been shown to detect anything. Placed: {:?}",
        r.freqs_seen
    );
}

/// THE OTHER HALF OF DEFAULT-ON: the notch costs nothing when there is nothing to notch.
///
/// A default that only ever helps in the presence of QRM must also be free in its absence, and that
/// claim was asserted in `openpulse-config`'s doc while being measured by no test — the notch is off
/// by default on a bare `ModemEngine`, so the rest of the suite provides no incidental evidence.
#[test]
#[ignore = "SLOW (~35 min): opt-in since #1274. Run with `scripts/slow-tests.sh`, or `cargo test -p openpulse-modem --no-default-features --test notch_rescues_interferer -- --ignored`."]
fn the_notch_costs_nothing_when_there_is_nothing_to_notch() {
    assert!(
        run_buffer(false, build_buffer(&[]), Budget::Full).decoded,
        "BASELINE BROKEN: the clean fixture does not decode even with the notch off"
    );
    assert!(
        run_buffer(true, build_buffer(&[]), Budget::Full).decoded,
        "the notch BROKE a decode that succeeds without it on a clean capture. Default-on requires \
         it to be free where there is nothing to remove; this is the half that says so."
    );
}

/// REVIEW PROBE (#1148 gate triage): sweep the (notch, amplitude) grid from the env so the
/// operating band can be re-located without recompiling. AMPS="0.25,0.30,0.35" NOTCH="on,off".
#[test]
#[ignore = "review probe (#1148 gate triage), asserts nothing — NOT part of the notch gate; scripts/slow-tests.sh skips it by name. Run with --ignored --nocapture"]
fn probe_band_sweep() {
    let amps: Vec<f32> = std::env::var("AMPS")
        .unwrap_or_else(|_| "0.30".into())
        .split(',')
        .map(|s| s.trim().parse().expect("amp"))
        .collect();
    let notches: Vec<bool> = std::env::var("NOTCH")
        .unwrap_or_else(|_| "on".into())
        .split(',')
        .map(|s| s.trim() == "on")
        .collect();
    // `PROBE_IN_BAND_AMP` adds a second tone at IN_BAND_HZ, so the paired-tone case (and its
    // single-tone controls) can be measured without recompiling.
    let in_band: f32 = std::env::var("PROBE_IN_BAND_AMP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    for &a in &amps {
        for &n in &notches {
            let mut tones = Vec::new();
            if a > 0.0 {
                tones.push((INTERFERER_HZ, a));
            }
            if in_band > 0.0 {
                tones.push((IN_BAND_HZ, in_band));
            }
            let budget = if std::env::var("PROBE_CHEAP_BUDGET").is_ok() {
                Budget::Engagement
            } else {
                Budget::Full
            };
            let r = run_buffer(n, build_buffer(&tones), budget);
            let ok = r.decoded;
            if std::env::var("PROBE_TRIPWIRE").is_ok() {
                println!(
                    "  tripwire: slices={} protect={:?} near_{INTERFERER_HZ}={:?} \
                     near_{IN_BAND_HZ}={:?} n_freqs={}",
                    r.blocks,
                    r.protect,
                    r.notched_near(INTERFERER_HZ),
                    r.notched_near(IN_BAND_HZ),
                    r.freqs_seen.len()
                );
            }
            let verdict = if std::env::var("PROBE_CHEAP_BUDGET").is_ok() {
                "DECODE-INVALID (cheap budget)"
            } else if ok {
                "OK"
            } else {
                "FAIL"
            };
            println!(
                "PROBE notch={} amp={a:.2} -> {verdict}",
                if n { "on" } else { "off" }
            );
        }
    }
}
