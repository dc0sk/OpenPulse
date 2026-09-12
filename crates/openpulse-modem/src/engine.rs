//! The core [`ModemEngine`] struct.

use openpulse_audio::tanh_limit;
use openpulse_dsp::acquisition::{DdcMatchedFilter, IqMatchedFilter};
use rand::Rng;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use openpulse_core::ack::AckFrame;
use openpulse_core::ack::AckType;
use openpulse_core::audio::{AudioBackend, AudioConfig, AudioInputStream};
use openpulse_core::conv::ConvCodec;
use openpulse_core::dcd::DcdState;
use openpulse_core::error::{ModemError, PluginError};
use openpulse_core::fec::{
    apply_window_retransmit, combine_llrs_map, combine_llrs_map_in_ranges,
    encode_window_retransmit, FecCodec, FecMode, Interleaver, ShortFecCodec, SoftCombiner,
    WindowArqFeedback, DEFAULT_INTERLEAVER_DEPTH,
};
use openpulse_core::frame::Frame;
use openpulse_core::hpx::{HpxEvent, HpxSession, HpxState, HpxTransition};
use openpulse_core::ldpc::{IterativeDecoder, LdpcCodec};
use openpulse_core::ota_rate::{OtaRateController, RxOutcome};
use openpulse_core::plugin::{ModulationConfig, PluginRegistry, PreambleTemplate};
use openpulse_core::profile::SessionProfile;
use openpulse_core::rate::OtaAggressiveness;
use openpulse_core::rate::RateEvent;
use openpulse_core::rate::SpeedLevel;
use openpulse_core::signed_envelope::SignedEnvelope;
use openpulse_core::soft_viterbi::SoftViterbiCodec;
use openpulse_core::trust::{
    evaluate_handshake, CertificateSource, ConnectionTrustLevel, HandshakeDecision, PolicyProfile,
    PublicKeyTrustLevel, SigningMode,
};
use openpulse_core::turbo::{turbo_decode_soft, turbo_encode, TURBO_MAX_INFO_BYTES};
use openpulse_core::tx_metadata::{TxMetadata, TxSessionLog};
use openpulse_core::wire_query::{callsign_hash, BroadcastFrame, WireEnvelope, WireMsgType};

use crate::event::EngineEvent;
use crate::harq::{HarqDecision, HarqPolicy};
use crate::pipeline::{
    AudioSamples, BackpressurePolicy, DecodedFrame, PipelineMetricsSnapshot, PipelineScheduler,
    PipelineStage, WirePayload,
};
use crate::rate_policy::{RateAdaptationPolicy, RateChangePayload};

#[derive(Debug, Clone)]
pub struct SecureSessionParams {
    pub local_minimum_mode: SigningMode,
    pub peer_supported_modes: Vec<SigningMode>,
    pub key_trust: PublicKeyTrustLevel,
    pub certificate_source: CertificateSource,
    pub psk_validated: bool,
}

/// Scan/retry policy for [`ModemEngine::receive_with_timeout`], extracted as
/// a pure state machine over (elapsed seconds, buffer length) so the policy is
/// unit-testable without an audio backend.
///
/// Responsibilities: incremental scan-position bookkeeping (never re-try a
/// start position), the post-AFC-settle scan reset back to the first-energy
/// position, and the wall-clock full-buffer retry cadence.
struct ScanPlanner {
    step: usize,
    min_frame_samples: usize,
    last_tried_end: usize,
    first_energy_pos: Option<usize>,
    /// Elapsed-seconds timestamp of the last fired retry.
    last_retry_at_secs: Option<u64>,
    /// Consecutive micro-sweep failures against a FULLY buffered window from the settled
    /// position. Only counted once the whole frame would be present if it really started there,
    /// so "the frame has not arrived yet" is never mistaken for "the anchor is wrong".
    settle_failures: usize,
    /// Fully-buffered micro-sweep failures that condemn an anchor. Defaults to
    /// [`Self::SETTLE_FAILURE_LIMIT`]; overridable so the recovery cost can be measured against the
    /// alternative of changing the wire format (#1062).
    settle_failure_limit: usize,
}

impl ScanPlanner {
    /// Full-buffer retries start at this elapsed time.  The wall-clock
    /// trigger exists because effective sample rates vary widely between
    /// audio stacks (FT-991A PipeWire: 2 300–7 600 samples/s), making
    /// sample-count thresholds unreliable.
    const RETRY_START_SECS: u64 = 12;
    /// Re-fire cadence: each subsequent retry sees a longer buffer until the
    /// frame fits and the decode succeeds.
    const RETRY_INTERVAL_SECS: u64 = 2;

    fn new(step: usize, min_frame_samples: usize) -> Self {
        Self {
            step,
            min_frame_samples,
            last_tried_end: 0,
            first_energy_pos: None,
            last_retry_at_secs: None,
            settle_failures: 0,
            settle_failure_limit: Self::SETTLE_FAILURE_LIMIT,
        }
    }

    /// Forward half-symbol onsets the micro-sweep cycles through before an anchor is condemned.
    ///
    /// Load-bearing in two places that must agree: the sweep in the receive loop takes
    /// `fep + (k % SWEEP_OFFSETS) * (step/2)`, and [`Self::unsettle`] reopens the scan past the
    /// span those offsets covered. They were chosen independently — the sweep spanned four symbols
    /// while the re-anchor advanced one — so three quarters of every recovery step re-tested ground
    /// already proven undecodable (#1040).
    const SWEEP_OFFSETS: usize = 9;

    /// How many fully-buffered micro-sweep failures condemn a settle.
    ///
    /// The sweep cycles `SWEEP_OFFSETS` forward half-symbol offsets, one per iteration, so a full
    /// cycle is `SWEEP_OFFSETS` attempts; two cycles gives the anchor a second chance against a
    /// grown buffer before it is abandoned.
    const SETTLE_FAILURE_LIMIT: usize = 2 * Self::SWEEP_OFFSETS;

    /// Record a micro-sweep decode failure at a fully-buffered settled position, and report
    /// whether the anchor has now been condemned.
    ///
    /// **Why this exists.** `note_settled` used to be permanent, with no way back. The energy gate
    /// falls back to an absolute floor until it has 32 windows of history, so a real receiver's
    /// idle noise can trip it and settle AFC on noise *before* the frame arrives (measured on air,
    /// issue #1021: settled at 0.2 s, 40 000 samples early, on a -49.8 Hz estimate when the true
    /// offset was +1.2 Hz). The micro-sweep then re-decodes from that noise position forever — its
    /// reach is only a few symbols — and for `long_frame` modes the full-buffer retry that would
    /// have rescued it is disabled precisely because a settle exists. Uncoded survived only because
    /// its shorter frame keeps that retry enabled.
    fn note_settle_failure(&mut self) -> bool {
        self.settle_failures += 1;
        self.settle_failures >= self.settle_failure_limit
    }

    /// Abandon a settled position that has proved undecodable, and reopen the search **past it**.
    ///
    /// **The condemned position must not be re-offered to the scan.** Rewinding `last_tried_end` to
    /// 0 — which this did until 2026-07-29 — restarts the broad scan at the beginning, where the
    /// same idle noise passes the same energy gate and re-settles at the same sample. That is a
    /// livelock, not a recovery: measured on the real on-air capture
    /// `ic9700-frame-bpsk250-rs-whitened.wav`, the receiver settled **78 times and condemned 77
    /// times at the identical position (sample 96)**, burning the entire 40 s listen window ~82 000
    /// samples short of a frame that is byte-perfect on the air (#1021).
    ///
    /// Resuming just past the anchor is sound by the same argument the old comment made for
    /// rewinding: **a premature noise anchor sits BEFORE the real frame**, so the frame is later.
    /// And the anchor has earned its exclusion — `SETTLE_FAILURE_LIMIT` fully-buffered decodes
    /// across a 9-offset sweep have already failed there, so it is proven undecodable rather than
    /// merely unpromising.
    /// Resume **past the span the micro-sweep actually tested**, not one scan step past the anchor.
    ///
    /// The sweep tried onsets at `condemned + k*(step/2)` for k in `0..SWEEP_OFFSETS` — four whole
    /// symbols at the default 9 offsets — and every one failed against a fully buffered window, so
    /// that entire span is proven undecodable. Advancing by a single `step` re-offered three
    /// quarters of it, and each re-anchor costs `SETTLE_FAILURE_LIMIT` fully-buffered decodes of a
    /// multi-second window. Measured on air 2026-07-30: the run that passed spent 15 condemnations
    /// walking 480 samples in 32-sample increments and only just finished inside its listen window
    /// (#1040).
    ///
    /// The bound is two-sided, which is why this is a span and not simply a bigger constant.
    /// Advancing *less* re-tests proven ground; advancing *more* skips audio the sweep never
    /// examined, and for `long_frame` modes the full-buffer retry that might have rescued a skipped
    /// preamble is disabled precisely because a settle occurred.
    fn unsettle(&mut self) {
        let condemned = self.first_energy_pos.unwrap_or(0);
        self.first_energy_pos = None;
        self.settle_failures = 0;
        let half = (self.step / 2).max(1);
        let swept = (Self::SWEEP_OFFSETS - 1) * half;
        self.last_tried_end = condemned.saturating_add(swept).saturating_add(1);
    }

    /// `true` once AFC settling has located the first signal energy.
    fn is_settled(&self) -> bool {
        self.first_energy_pos.is_some()
    }

    /// The settled first-energy (≈ preamble) position, if settling has occurred.
    fn first_energy_pos(&self) -> Option<usize> {
        self.first_energy_pos
    }

    /// Record a successful AFC settle at `pos` (the refined preamble onset).
    ///
    /// The decode from this position is driven by the dedicated first-energy
    /// re-decode in the receive loop (which re-tries as the buffer grows), so we
    /// do NOT rewind `last_tried_end`: rewinding made the broad scan re-decode a
    /// huge range every time the buffer jumped, stalling the loop.
    fn note_settled(&mut self, pos: usize) {
        self.first_energy_pos = Some(pos);
    }

    /// Untried scan start positions for the current buffer, ending exactly at
    /// the last position that still fits a minimal frame.
    fn scan_positions(&self, buffer_len: usize) -> impl Iterator<Item = usize> + use<> {
        let new_end = buffer_len.saturating_sub(self.min_frame_samples);
        (self.last_tried_end..=new_end).step_by(self.step.max(1))
    }

    /// Mark the current buffer's positions as tried.
    fn commit_scan(&mut self, buffer_len: usize) {
        let new_end = buffer_len.saturating_sub(self.min_frame_samples);
        if new_end > self.last_tried_end {
            self.last_tried_end = new_end;
        }
    }

    /// Whether a full-buffer retry fires now.  Consumes the tick: the next
    /// retry becomes due `RETRY_INTERVAL_SECS` later.
    fn retry_due(&mut self, elapsed_secs: u64, buffer_len: usize) -> bool {
        if buffer_len == 0 || elapsed_secs < Self::RETRY_START_SECS {
            return false;
        }
        let ready = match self.last_retry_at_secs {
            None => true,
            Some(t) => elapsed_secs.saturating_sub(t) >= Self::RETRY_INTERVAL_SECS,
        };
        if ready {
            self.last_retry_at_secs = Some(elapsed_secs);
        }
        ready
    }
}

/// Buffering time above which the settled full-buffer retry starves the capture read loop.
///
/// 120 000 samples is 15 s at 8 kHz. The retry re-scans the whole buffer every 2 s, which for a
/// frame this long outruns the read cadence so the frame never finishes buffering.
pub const LONG_FRAME_SAMPLES: usize = 120_000;

/// How many times a mode's raw `max_frame_samples` a coded frame can actually reach.
///
/// **Measured, not guessed** (`tests/fec_slice_expansion.rs`, BPSK250 @ 8 kHz, worst case over
/// payloads that straddle the RS block boundary): `None` 0.93, `Rs`/`RsStrong`/`RsInterleaved`
/// 1.77, `LdpcHighRate` 1.50, `Ldpc` 2.65, `Concatenated`/`SoftConcatenated` 3.55. The values below
/// carry ~15 % headroom over those.
///
/// A single blanket factor cannot serve that spread, and the previous blanket ×3 was wrong in
/// **both** directions at once: it over-reserved for RS — a plugin's `max_frame_samples` is already
/// sized for "a full 255-byte RS block + envelope", so ×3 double-counted an expansion the geometry
/// already contained — while under-reserving for `Concatenated`, whose real frame (264 704 samples)
/// exceeds the ×3 bound (223 872) and would be silently truncated at the largest payloads.
///
/// The over-reserve is not merely wasteful: the scanning receive cannot judge a settled position
/// undecodable until this much audio exists past it, so an inflated reserve makes that recovery
/// unreachable on a real capture — the mechanism that kept the coded rungs broken on air (#1021).
fn fec_slice_factor(fec: FecMode) -> usize {
    match fec {
        FecMode::None => 1,
        // One or two 255-byte RS blocks over a geometry already sized for one.
        FecMode::Rs | FecMode::RsStrong | FecMode::RsInterleaved => 2,
        // Byte-exact short block: barely larger than the frame it wraps.
        FecMode::ShortRs => 2,
        FecMode::LdpcHighRate => 2,
        FecMode::Ldpc => 3,
        // Rate-1/2 convolutional stacked on RS, and the soft variants of the same.
        FecMode::Concatenated | FecMode::SoftConcatenated | FecMode::Turbo => 4,
    }
}

/// Per-attempt slice length and long-frame classification for a mode's raw geometry under `fec`.
///
/// Returns `(max_frame_samples, long_frame)`.
///
/// **The widening and the classification live in one function on purpose.** A coded frame is 3x the
/// raw geometry, and what starves the capture read loop is how long the frame actually takes to
/// buffer. Classifying on the raw value — which is what this code did until 2026-07-20 — put three
/// modes with ~28 s coded frames (`BPSK250`, `BPSK250-RRC`, `QPSK125`) on the wrong side, so they kept
/// the retry that starves the capture and never finished buffering on a real audio path. Splitting
/// these two steps apart is what allowed them to drift out of order; keep them together.
pub fn frame_plan(raw_max_frame_samples: usize, fec: FecMode) -> (usize, bool) {
    let coded = raw_max_frame_samples.saturating_mul(fec_slice_factor(fec));
    (coded, coded > LONG_FRAME_SAMPLES)
}

/// How much post-onset audio must exist before a decode failure is evidence that the ONSET is wrong
/// rather than that the frame has not finished arriving.
///
/// **Deliberately not widened by `fec`** — this is the one thing that distinguishes it from
/// [`frame_plan`], and the reason it is a named function rather than an inline expression. A
/// plugin's `max_frame_samples` is already "the largest frame this mode emits, plus margin";
/// `frame_plan`'s factor is a per-attempt *slice* reserve, generous so a slice still contains the
/// frame wherever inside it the frame begins. Sizing arrival from that reserve demanded 149.2 s of
/// post-onset audio for BPSK31 — more than any configured harness listens for — which disabled the
/// settle recovery outright on `hpx_hf`'s entry rung (archetype scan 2026-07-29, finding 4).
///
/// A multi-block frame can exceed this, so the threshold can be reached slightly early; the
/// `ScanPlanner::SETTLE_FAILURE_LIMIT` budget absorbs that. An unreachable threshold cannot be
/// absorbed by anything. Gate: `tests/coded_noise_settle_recovery.rs`.
pub fn frame_arrival_samples(raw_max_frame_samples: usize, _fec: FecMode) -> usize {
    raw_max_frame_samples
}

/// Un-whiten soft decisions coming off a demodulator.
///
/// The wire is whitened before modulation (see `stage_modulate_payload`), so every receive path
/// must undo it. A hard path XORs; a soft path cannot, because an LLR carries confidence as well as
/// a decision — flipping a bit negates its LLR and leaves the magnitude alone. Wrapping every
/// `demodulate_soft` call in this keeps the two families symmetric.
fn openpulse_modem_descramble_soft(mut llrs: Vec<f32>) -> Vec<f32> {
    openpulse_core::scramble::descramble_llrs(&mut llrs);
    llrs
}

/// Settled AFC corrections below this magnitude (Hz) are treated as measurement
/// noise and snapped to zero.  A short data-aided/blind estimate on a zero-offset
/// frame lands a few tenths of a Hz off; applying that spurious correction breaks
/// modes that re-fit carrier phase from the (now over-corrected) preamble — 8PSK's
/// `carrier_phase_correct` enters a fragile drift-fit branch at ≥0.5 Hz.  Real HF
/// offsets are tens to hundreds of Hz (the carrier-offset regression uses 15 Hz;
/// the measured inter-rig offset is ~400 Hz), so this never suppresses a real one.
const AFC_SETTLE_DEADBAND_HZ: f32 = 2.0;

/// Onset-step multiplier for the #1118 acquisition pass.
///
/// The pass settles on a coarser grid than the decode scan uses, because a settle needs only to land
/// on signal while a decode needs the onset to a symbol period. 4 keeps the whole scan span reachable
/// — the real #1021 lead-in is 126 symbol periods — while cutting the settle count to a quarter.
///
/// Derived from the cost that forced it, not tuned: at every onset, the pass turned a Watterson
/// fading test from minutes into 98+ minutes without finishing.
const PHASE2_STEP_MULTIPLIER: usize = 4;

/// Maximum AFC correction magnitude accepted after settling.
///
/// The Goertzel acquisition range is ±400 Hz (`range_hz = 800` in `estimate_carrier_hz_wide`), so
/// this is that range plus a small margin. The convergence guard (`|change| > 5 Hz`) still rejects
/// flat noise that produces a near-zero stable estimate.
///
/// **Corrected 2026-08-18**: this comment used to justify the bound with "on-air measurements show a
/// consistent ~400 Hz carrier offset between the two rigs". That figure is unsupported — it traces to
/// the two-station OTA notes, whose CFO readings are marked unreliable in the same paragraph (the
/// spectral peak-picker was measuring dev-host birdies). The cleanly measured inter-rig offset is
/// **−64 Hz** (`openpulse-channel/src/cfo.rs`). The bound itself is unchanged and is justified by the
/// estimator's own range, which is what it was always really about.
///
/// Module-scoped rather than function-local since #1118: the daemon's burst scan applies the same
/// guard, and a plausibility bound that two acquisition paths hold independently is one they can
/// drift apart on.
const AFC_MAX_CORRECTION_HZ: f32 = 450.0;

/// Longest preamble template the correlation will use, in samples (256 ms at 8 kHz).
///
/// Two costs grow with the template and both bite at once: the grid step must shrink as `fs/tlen`
/// to stay inside the correlation's coherent bandwidth (a step wider than that steps *over* the
/// peak and reads noise), and every extra grid point is another full correlation. BPSK31's
/// 31-symbol preamble is 7936 samples — a ~1 s coherent integration needing a ~0.5 Hz step, i.e.
/// hundreds of correlations of an 8000-sample template per candidate settle.
///
/// **This is a POST-DDC budget, not a raw-sample cap.** It was the latter until the phase-0 work
/// on #1062, and as a raw cap it excluded BPSK31/63/100 from the veto entirely — the modes that
/// listen longest and are most exposed to a noise settle, excluded not because correlation could
/// not help them but because it was priced in the wrong domain. A template longer than this is now
/// decimated to fit rather than refused: see [`DdcMatchedFilter`], whose equivalence to the
/// passband correlator is measured exact to four decimals at decimation up to 32.
///
/// **Read that equivalence claim with its provenance.** It was measured in
/// `tests/ddc_correlation_equivalence.rs`, which **reimplements** the mixer locally rather than
/// calling `DdcMatchedFilter`, and whose tests are all `#[ignore]`d — so the number came from a
/// copy, in the release profile, and nothing keeps the two copies in step. They diverged once
/// already: both carried an underflowing loop guard, harmless under release wrapping and fatal in
/// dev, and only the shipped one has been fixed. What is machine-checked today is
/// `openpulse_dsp::acquisition::tests::ddc_mix_keeps_the_first_sample_and_every_decim_th_one`;
/// the four-decimal figure itself is not.
const MAX_PREAMBLE_CORRELATION_SAMPLES: usize = 2_048;

/// A mode's preamble matched filter together with the ρ constants its plugin measured for it.
///
/// The constants are carried per mode rather than held as engine-wide values because ρ is
/// normalised: its noise floor is set by the template's length, so one threshold cannot serve two
/// waveforms. BPSK250's 0.40 sits *below* QPSK500's recorded idle-noise ceiling of 0.429 — an
/// engine-wide constant would have corroborated QPSK settles on pure noise. See
/// [`openpulse_core::plugin::PreambleTemplate`].
struct PreambleVeto {
    filter: VetoCorrelator,
    rho_threshold: f32,
    rho_grid_hz: f32,
    /// The ρ a delivered frame is known to reach (#1060). Bounds the runtime calibration from
    /// above; `None` means the veto never stands down, which is where every mode starts.
    delivered_frame_rho_bound: Option<f32>,
}

/// Passband for templates inside the budget; decimated baseband for the ones that are not.
///
/// Both compute the same ρ. The split exists only because correlating a 16 128-sample template in
/// the passband costs what the budget refuses, and decimating it does not change the answer —
/// measured, not assumed.
enum VetoCorrelator {
    Passband(IqMatchedFilter),
    Ddc(DdcMatchedFilter),
}

impl VetoCorrelator {
    /// Template length in the domain the budget is measured in.
    fn len(&self) -> usize {
        match self {
            Self::Passband(f) => f.len(),
            Self::Ddc(f) => f.len(),
        }
    }

    /// Input samples the template spans, which sets its duration and so its coherent bandwidth.
    ///
    /// Equal to [`Self::len`] on the passband arm and `decim` times larger on the DDC arm. The two
    /// were one number while every template was passband, and a frequency-resolution derivation
    /// written against `len()` then silently changed meaning the day a template was decimated.
    fn input_span(&self) -> usize {
        match self {
            Self::Passband(f) => f.len(),
            Self::Ddc(f) => f.input_span(),
        }
    }
}

impl PreambleVeto {
    fn new(t: PreambleTemplate, center_hz: f32, sample_rate: f32, occupied_bw_hz: f32) -> Self {
        let raw = t.samples.len();
        let filter = if raw <= MAX_PREAMBLE_CORRELATION_SAMPLES {
            VetoCorrelator::Passband(IqMatchedFilter::new(t.samples))
        } else {
            // Cutoff comes from the SIGNAL, and the decimation from the cutoff — not the reverse.
            // The first version of this derived cutoff from the decimation factor, which has no
            // baud term at all: it left BPSK31 with a 540 Hz passband for a 40 Hz signal (throwing
            // away most of the out-of-band rejection this path exists to provide) while a wide mode
            // at high decimation would have been filtered by its own anti-alias stage.
            let cutoff = occupied_bw_hz / 2.0 + t.rho_grid_hz + occupied_bw_hz * 0.15;
            // Decimated Nyquist must clear the cutoff with transition margin, and the template must
            // fit the budget. Take the stricter of the two.
            let by_budget = raw.div_ceil(MAX_PREAMBLE_CORRELATION_SAMPLES);
            let by_bandwidth = ((sample_rate / 2.0) / (cutoff * 1.25)).floor().max(1.0) as usize;
            let decim = by_budget.min(by_bandwidth).max(1);
            VetoCorrelator::Ddc(DdcMatchedFilter::new(
                &t.samples,
                center_hz,
                sample_rate,
                cutoff,
                decim,
            ))
        };
        Self {
            filter,
            rho_threshold: t.rho_threshold,
            rho_grid_hz: t.rho_grid_hz,
            delivered_frame_rho_bound: t.delivered_frame_rho_bound,
        }
    }
}

/// What the correlation veto decided for one settle query (#1049, REQ-RX-02/03).
///
/// Returned by [`ModemEngine::decide_preamble_veto`], the one decide-and-record step both acquisition
/// paths share, so each call site can do its own path-specific work without re-deriving the decision.
#[derive(Debug, Clone, Copy, PartialEq)]
enum VetoVerdict {
    /// The preamble correlation cleared the (calibrated) threshold.
    Corroborated,
    /// The veto is standing down (REQ-RX-03): the settle is let through without a verdict on ρ.
    StoodDown,
    /// ρ fell below the threshold — energy without a preamble.
    Rejected {
        /// The threshold it was compared against, for the caller's log line.
        threshold: f32,
    },
}

impl VetoVerdict {
    /// Whether the settle proceeds: corroborated, or let through by a stand-down.
    fn accepted(self) -> bool {
        !matches!(self, Self::Rejected { .. })
    }
}

/// Result of [`ModemEngine::afc_mini_settle`].
struct AfcSettleOutcome {
    /// Correction after the one-shot wide-scan anchor pass.
    anchor: f32,
    /// Correction after the fine-tracking passes.
    fine: f32,
    /// Absolute change introduced by the final fine pass (convergence check).
    last_delta: f32,
}

/// Adaptive scan energy gate: a noise-floor-relative threshold, from the first window.
///
/// The gate keeps a short history of window energies and uses the 25th percentile as the
/// noise-floor estimate (robust to up to 75 % signal-bearing windows), gating at 3× that floor,
/// clamped to `[ABS_THRESHOLD, MAX_THRESHOLD]`. A fixed 1e-4 gate alone passes every position when
/// the band noise floor is elevated (on-air QRM ≈ 1.5e-3), firing the expensive AFC mini-settle at
/// every scan step.
///
/// **There is no cold-start fallback, and that is the point (#1021).** This returned the fixed
/// `ABS_THRESHOLD` until it held `MIN_HISTORY = 32` windows — but a real receiver's idle floor sits
/// *above* that constant, so the very first window of pure noise passed, and the receiver settled
/// AFC on it before the frame arrived. Measured on `ic9700-frame-bpsk250-rs-whitened.wav`: idle
/// floor **4.1e-4**, four times the 1e-4 fallback, settling at sample 96 with a bogus +364 Hz
/// correction some 82 000 samples before a frame that is byte-perfect on the air. Worse, that
/// station **passes** `scripts/onair-rx-level-check.sh`, which only checks the floor against the
/// *ceiling* — leaving `1e-4 … 1.07e-3` a blind window, and that is exactly where a correctly
/// configured station sits.
///
/// **Why estimating from one window is safe, which is what made the fallback look necessary.**
/// With a single window the 25th percentile is that window, so the threshold is 3× it — and the
/// two regimes separate cleanly, in opposite directions:
///
/// | first window is | floor×3 | clamped to | verdict |
/// |---|---|---|---|
/// | real idle noise (4e-4) | 1.2e-3 | 1.2e-3 | **rejected** — correct, it is noise |
/// | a fixture's own signal (0.36, measured) | 1.08 | `MAX_THRESHOLD` 3.2e-3 | **passes** — 100× below the signal |
///
/// `MAX_THRESHOLD` is what protects the buffer-is-the-frame fixtures whose window 1 *is* signal:
/// their level is two orders of magnitude above the clamp, so a self-derived threshold can never
/// gate them out. (Note the clamp's own comment claimed loopback levels of 1e-3…5e-3; measured,
/// `route_clean` delivers ≈ 0.36 — the clamp has far more headroom than it advertised.)
struct EnergyGate {
    history: std::collections::VecDeque<f32>,
}

impl EnergyGate {
    /// Absolute floor (DcdState default: 0.01 RMS → 1e-4 mean-square). A lower bound on the
    /// adaptive threshold, never a substitute for it.
    const ABS_THRESHOLD: f32 = 0.0001;
    /// Upper clamp. Measured `route_clean` loopback level is ≈ 0.36 mean-square, so this sits ~100×
    /// below the weakest fixture signal — the headroom that lets the threshold be derived from a
    /// single window without ever gating out a real one.
    const MAX_THRESHOLD: f32 = 0.0032;
    const HISTORY: usize = 128;

    fn new() -> Self {
        Self {
            history: std::collections::VecDeque::with_capacity(Self::HISTORY),
        }
    }

    /// Threshold from whatever history exists. Empty only before the first `passes` call.
    fn threshold(&self) -> f32 {
        if self.history.is_empty() {
            return Self::ABS_THRESHOLD;
        }
        let mut sorted: Vec<f32> = self.history.iter().copied().collect();
        sorted.sort_by(f32::total_cmp);
        let floor = sorted[sorted.len() / 4];
        // `MAX_THRESHOLD` keeps the clamped value from ever gating out a fixture signal. When the
        // real floor is above it the clamp lands *under* the noise and the gate stops carrying
        // information; that regime is handled by the preamble-correlation veto on the settle, not by
        // moving this threshold — see the removal note at the condemnation site.
        (floor * 3.0).clamp(Self::ABS_THRESHOLD, Self::MAX_THRESHOLD)
    }

    /// Record one gate-window energy and return whether it passes the gate.
    ///
    /// The window is recorded **before** the threshold is computed, so the very first window is
    /// judged against a floor derived from itself rather than from a constant. That is what removes
    /// the cold-start blind spot: idle noise cannot clear 3× its own level, while a fixture's
    /// full-scale signal clears the clamped ceiling by two orders of magnitude.
    fn passes(&mut self, mean_sq: f32) -> bool {
        if self.history.len() == Self::HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(mean_sq);
        mean_sq >= self.threshold()
    }
}

/// Refine a coarse first-energy position to the actual signal onset.
///
/// The energy gate's wide window (`acq_samples`, ~32 symbols) trips up to a full
/// window before the true onset — its tail catches the first signal samples — so
/// the coarse position can sit a whole acquisition window ahead of the preamble,
/// far beyond the demodulator's one-symbol timing search.  Scan symbol-length
/// sub-windows across the gate span and return the first whose energy reaches a
/// quarter of the span's peak (where the signal turns on), so the preamble lands
/// within one symbol period of the returned position.
fn refine_onset(buf: &[f32], start: usize, span: usize, step: usize) -> usize {
    let end = (start + span).min(buf.len());
    if step == 0 || end <= start + step {
        return start;
    }
    let energy = |p: usize| -> f32 {
        let e = (p + step).min(buf.len());
        buf[p..e].iter().map(|s| s * s).sum::<f32>() / (e - p) as f32
    };
    let positions: Vec<usize> = (start..end).step_by(step).collect();
    let peak = positions.iter().map(|&p| energy(p)).fold(0.0f32, f32::max);
    if peak <= 0.0 {
        return start;
    }
    positions
        .into_iter()
        .find(|&p| energy(p) >= peak * 0.25)
        .unwrap_or(start)
}

/// Internal return of [`ModemEngine::ota_decode_and_ack`]: the decoded
/// payload+mode (if any), the ACK frame to send, and the last decode error.
///
/// The ACK is `None` for a frame recovered by the uncoded fallback — that traffic is not part of
/// the rate ladder, so there is nothing to acknowledge and nothing to key the transmitter for.
type OtaDecodeOutcome = (
    Option<(Vec<u8>, String)>,
    Option<AckFrame>,
    Option<ModemError>,
);

/// Outcome of one daemon-facing OTA receive poll ([`ModemEngine::poll_ota_rx`]):
/// the decode result plus the ACK frame the caller must transmit back.
#[derive(Debug, Clone)]
pub struct OtaRxResult {
    /// Decoded payload, or `None` when every candidate failed (the ACK is a Nack).
    pub payload: Option<Vec<u8>>,
    /// ACK frame to transmit back to the sender (key PTT around the transmit), or `None` when the
    /// burst was non-ladder traffic recovered by the uncoded fallback.
    ///
    /// `Option` rather than a `control: bool` beside a always-present frame on purpose: a caller
    /// must not be able to transmit an ACK that should not exist, and the type change forces every
    /// transmit site into a compile-visible decision.
    pub ack: Option<AckFrame>,
    /// Mode string a candidate decoded at, for event reporting.
    pub mode: Option<String>,
}

/// The modem engine.
///
/// # Example
/// ```no_run
/// use openpulse_modem::ModemEngine;
/// use openpulse_audio::LoopbackBackend;
/// use bpsk_plugin::BpskPlugin;
///
/// let mut engine = ModemEngine::new(Box::new(LoopbackBackend::new()));
/// engine.register_plugin(Box::new(BpskPlugin::new()));
/// engine.transmit(b"Hello", "BPSK100", None).unwrap();
/// let received = engine.receive("BPSK100", None).unwrap();
/// ```
pub struct ModemEngine {
    audio: Box<dyn AudioBackend>,
    plugins: PluginRegistry,
    sequence: u16,
    hpx: HpxSession,
    scheduler: PipelineScheduler,
    trust_policy_profile: PolicyProfile,
    active_handshake: Option<HandshakeDecision>,
    /// Residual carrier frequency error measured at the corrected reference from
    /// the most recent demodulation call.  This is the error *after* applying
    /// `afc_correction_hz`; the total offset from the nominal centre frequency is
    /// approximately `afc_correction_hz + last_afc_offset_hz`.
    last_afc_offset_hz: Option<f32>,
    /// Accumulated AFC carrier correction applied to demodulation (Hz).
    afc_correction_hz: f32,
    /// Whether the AFC tracking loop is active (default: true).
    afc_enabled: bool,
    /// Fraction of the estimated offset applied to the correction each frame.
    afc_step: f32,
    /// Audio centre frequency used for modulation and demodulation (Hz).
    center_frequency: f32,
    rate_policy: RateAdaptationPolicy,
    /// Receiver-led OTA rate controller (per-direction lockstep); `None` until
    /// [`start_ota_session`](ModemEngine::start_ota_session) is called.
    ota: Option<OtaRateController>,
    /// Externally-supplied RX SNR estimate (dB) for OTA adaptive decisions.
    /// When `None`, the weak LLR-magnitude proxy is used. A real estimator
    /// (or a channel-sim harness) should feed this for meaningful stepping.
    rx_snr_estimate: Option<f32>,
    /// Soft LLRs retained from failed OTA bursts, keyed by mode, for HARQ soft-combining
    /// across retransmissions ([`decode_combined_llrs`](Self::decode_combined_llrs)). Bounded
    /// to [`OTA_HARQ_MAX_ATTEMPTS`] vectors per mode; cleared on any successful decode and on
    /// OTA session start/stop.
    ///
    /// **Oldest first** — the decoder relies on that order to try suffixes that exclude an
    /// abandoned message's bursts, which is the only thing separating them from this frame's.
    ota_retained_llrs: std::collections::HashMap<String, Vec<Vec<f32>>>,
    /// Session id the retained LLRs belong to; a burst under a different session clears them.
    ///
    /// Note this guard is inert on the daemon, which pins the session id to the local callsign
    /// (`server.rs`); the suffix trial in `ota_decode_and_ack_inner`, not this, is what keeps a
    /// stale message's LLRs out of the next one's combine.
    ota_retained_session: Option<String>,
    /// Per-session key (ECDH-derived at the handshake) for authenticating the OTA rate ACK (E7).
    /// When set, ACKs are encoded/verified with a keyed MAC instead of the public FNV hash + CRC,
    /// so a listener can't forge rate-control ACKs. `None` = legacy unauthenticated ACK.
    ack_mac_key: Option<[u8; 32]>,
    dcd: DcdState,
    /// Passband noise-floor tracker driving the DCD squelch. Mode-independent by design.
    noise_floor: openpulse_dsp::noise_floor::NoiseFloorTracker,
    csma_enabled: bool,
    csma_persistence: f32,
    event_tx: broadcast::Sender<EngineEvent>,
    /// Sender-local sequence counter for broadcast frames.
    broadcast_seq: u16,
    /// Callsign used in broadcast frame headers (set via `set_callsign`).
    callsign: String,
    /// TX attenuation in dB applied to output samples (0.0 = no attenuation).
    tx_attenuation_db: f32,
    /// Soft TX limiter threshold (0.0 = disabled). See `tanh_limit`.
    tx_limiter_threshold: f32,
    /// Maximum TX power in watts for regulatory compliance (0.0 = no limit).
    max_power_watts: f32,
    /// Transmission metadata log for regulatory compliance (station_id, timestamps).
    tx_session_log: TxSessionLog,
    /// Where the §97 TX record is appended as NDJSON (#1110). `None` disables spilling.
    tx_log_path: Option<std::path::PathBuf>,
    /// Latched after a spill write fails, so a full disk cannot emit a warning per frame.
    tx_log_failed: bool,
    /// Default audio device name used when a per-call `device` is `None`.
    /// Lets a daemon pin its engine to a specific capture/playback device (e.g. an
    /// `snd-aloop` PCM) without threading the name through every transmit/receive.
    default_device: Option<String>,
    /// Most recent audio window the engine captured (RX) or emitted (TX), bounded
    /// to [`SPECTRUM_TAP_MAX`] samples. A spectrum/waterfall consumer (e.g. the
    /// daemon's control-port broadcast) reads this so the FFT is of real audio, not
    /// silence. Empty until the first transmit/receive.
    last_audio: Vec<f32>,
    /// [`capture_burst`](ModemEngine::capture_burst) accumulator: samples gathered
    /// across receive ticks while a carrier is present, flushed for decode when it
    /// drops. Lets a tick-based daemon assemble a full frame from a streaming
    /// (cpal) backend instead of decoding one partial tick window.
    rx_burst: Vec<f32>,
    /// Mode of a relay consumer reading this engine's bursts, for the runaway cap (#1308).
    relay_mode: Option<String>,
    /// Whether the last burst was flushed by the runaway CAP rather than by a carrier drop (#1255).
    ///
    /// A cap flush means the carrier was still up when the accumulator hit its bound, so the slab is
    /// not one transmission: it is a stuck channel, or a frame with a long carrier behind it. Consumed
    /// by `ota_decode_and_ack_inner`, which must not treat a failed decode of such a slab as evidence
    /// about the rate ladder.
    last_flush_capped: bool,
    /// Set while decoding an already-front-end-processed burst (e.g. `decode_burst` scans a burst that
    /// `accumulate_routed` already ran through the InputCapture seam). Makes the nested
    /// `route_audio_stage(InputCapture)` in the per-slice decode a pass-through, so the stateful AGC and
    /// the DCD latch are not applied a second time per scan slice.
    input_prerouted: bool,
    /// Set while a multi-attempt scan is running, to suppress `AfcUpdate` emission from attempts
    /// whose AFC correction is about to be ROLLED BACK.
    ///
    /// Every scanning loop (`decode_burst_inner`, the OTA candidate/HARQ loops, and
    /// `receive_with_timeout_fec`'s retry loop) restores `afc_correction_hz = afc_before` after a
    /// failed attempt. An event emitted from inside one of those attempts therefore narrates a
    /// hypothesis the engine immediately discards — it is not state. Emitting them also floods the
    /// 64-slot broadcast ring (a BPSK250 noise scan makes ~129 attempts), which EVICTS genuine
    /// events: that is how a failed OTA burst could lose its own `OtaRateDecision` (#1081).
    ///
    /// The rule is commit-gating, not failure-gating: single-window receives keep their estimate
    /// even when the decode fails, so those emissions are real state and must keep flowing — which
    /// is what keeps the TUI's AFC meter alive during acquisition. Same shape as `DcdChange`, which
    /// is already change-gated at the seam.
    suppress_afc_events: bool,
    /// Whether [`capture_burst`](ModemEngine::capture_burst) is mid-burst (carrier
    /// was present on a prior tick and not yet flushed).
    rx_capturing: bool,
    /// Master enable for CE-SSB TX envelope conditioning. Default on; it only acts
    /// on modes that benefit (multicarrier — see [`cessb_benefits`]), so it is a
    /// no-op for single-carrier modes regardless.
    cessb_enabled: bool,
    /// Receiver-side automatic notch on captured audio (default off). Removes out-of-band CW
    /// interference (QRM) before demod; its protected band tracks the active mode's occupied
    /// bandwidth so the signal is never notched. See `docs/dev/notch-equalizer-experiment.md`.
    notch_enabled: bool,
    /// The notch bank, used only while `notch_enabled`.
    notch_bank: openpulse_dsp::notch::NotchBank,
    /// Protected-band full bandwidth (Hz) used when the active mode can't report its occupied
    /// bandwidth (e.g. multicarrier modes, or a mode-agnostic capture); ± half this around the
    /// carrier is never notched.
    notch_fallback_bw_hz: f32,
    /// Confirmed in-band interferers (Hz) from the notch persistence tracker — a notch can't
    /// remove these, so they are QSY (move-frequency) candidates. Empty unless persistence is on.
    notch_in_band_interferers: Vec<f32>,
    /// Active mode for the receiver front end, set by the capture entry points and read at the
    /// `PipelineStage::InputCapture` notch seam (where the mode isn't otherwise in scope).
    rx_mode: Option<String>,
    /// Count of capture blocks the notch processed — a tripwire: an enabled notch that never runs
    /// on a given path (e.g. a new capture path that skips the InputCapture seam) leaves this at 0.
    notch_blocks_processed: u64,
    notch_freqs_seen: std::collections::BTreeSet<i32>,
    notch_protect_extremes: Option<(f32, f32, f32, f32)>,
    /// Count of settle anchors condemned by the micro-sweep and handed back to the scan.
    ///
    /// A *count*, not a pass/fail: the settle recovery can succeed while re-anchoring dozens of
    /// times, and that difference is invisible to any decode-or-not gate. Measured on air
    /// 2026-07-30 the passing run needed 15 condemnations, each costing
    /// `SETTLE_FAILURE_LIMIT` fully-buffered decodes, and only just finished inside the listen
    /// window (#1040).
    settle_condemnations: u64,
    /// Sample positions of condemned settle anchors, for diagnosing WHERE a receiver is thrashing.
    ///
    /// The count alone cannot distinguish the two failure shapes that produce it: an anchor that
    /// holds one wrong position until the window expires, and a recovery that advances through the
    /// buffer and still never decodes. Those are different bugs with different fixes, and only the
    /// positions separate them. Bounded so a pathological run cannot grow it without limit.
    condemned_positions: Vec<usize>,
    /// Sample positions of settles the correlation ACCEPTED.
    ///
    /// The companion to `condemned_positions`, and it exists for the same reason the accept COUNTER
    /// does: logging only the anchors that failed cannot show where the one that succeeded landed,
    /// so "the recovery walk reached the frame" stays an inference. Recording only failures was the
    /// same asymmetry twice.
    accepted_settle_positions: Vec<usize>,
    /// Per micro-sweep attempt: `(attempt_index, onset, window_len, afc_before)`.
    ///
    /// Exists to settle whether the second sweep cycle can do anything the first did not.
    /// `SETTLE_FAILURE_LIMIT` is `2 * SWEEP_OFFSETS` so the anchor gets "a second chance against a
    /// grown buffer" — but failures are only counted once `window_complete`, and past that point
    /// the window is a fixed slice of already-captured audio, while `afc_correction_hz` is restored
    /// after every failure. If attempt `k + SWEEP_OFFSETS` has identical inputs to attempt `k`,
    /// the second cycle is provably inert and halving the limit is safe by construction rather
    /// than by fitting a constant to whichever captures happen to exist (#1062).
    sweep_attempt_inputs: Vec<(usize, usize, usize, f32)>,
    /// Optional work-unit budget for the retry scan, replacing its wall-clock budget.
    ///
    /// The shipped budget compares *elapsed real time* against the audio duration a pass covers
    /// (`retry_started.elapsed() > retry_budget`), which is right in production — a scan that
    /// cannot walk its own buffer faster than real time can never catch up — and fatal for
    /// measurement, because the number of positions examined then depends on machine speed and
    /// load. The same input decoded on one run and failed on the next, and a debug build is slower
    /// still, which is the shape #1058 records as a debug/release verdict split.
    ///
    /// `None` keeps the shipped wall-clock behaviour. `Some(n)` abandons the pass after `n`
    /// positions regardless of time, making a measurement a function of the audio alone.
    deterministic_scan_positions: Option<usize>,
    /// Optional cap on outer receive-loop iterations, replacing the wall-clock listen deadline.
    ///
    /// Bounding the retry pass alone is not enough: the outer loop keeps scanning until
    /// `listen_for` expires, so under load it completes fewer passes and reaches a different
    /// verdict. Measured: the same input decoded 5/5 unloaded and 0/5 on eight busy cores, with
    /// condemnations 582 vs 296. Both budgets must be work-based for a run to be reproducible.
    deterministic_max_iterations: Option<usize>,
    /// Override for `ScanPlanner`'s condemnation threshold; `None` = the shipped `2 *
    /// SWEEP_OFFSETS` = 18.
    ///
    /// One sweep cycle is `SWEEP_OFFSETS` attempts; the shipped value runs **two**, to give the
    /// anchor "a second chance against a grown buffer". Where the buffer is already complete — a
    /// frame sitting behind continuous pre-frame interference — the second cycle cannot find what
    /// the first did not, and doubles the cost of clearing each swept span. This knob exists to
    /// measure whether halving it is the cheaper alternative to a wire-format change (#1062).
    settle_failure_limit: Option<usize>,
    /// Times `afc_mini_settle` was entered — a tripwire for the ACQUISITION CHAIN ITSELF (#1118).
    ///
    /// Distinct from the `rho_*` counters on purpose. Those record what the #1049 preamble veto
    /// decided, so they only move on a mode publishing a template (BPSK250 alone today) and only
    /// once the veto block is reached. This one increments at the chain's ENTRY, on every mode, so
    /// "did this receive path run the acquisition chain?" has an answer that does not depend on the
    /// veto being reachable. Added because a gate built on the `rho_*` counters alone stays green if
    /// the chain is ever half-wired into the streaming path — gate and settle without the veto.
    afc_settle_attempts: u64,
    /// Settles rejected because the preamble correlation did not corroborate them (#1049).
    ///
    /// A tripwire as much as a counter: it stays 0 when the mode publishes no template, so a test
    /// that expects the correlation gate to be doing work can tell "it accepted everything" from
    /// "it never ran" — the two are indistinguishable in a decode-or-not assertion.
    rho_rejected_settles: u64,
    /// Settles the preamble correlation *accepted* (#1049).
    ///
    /// The companion to `rho_rejected_settles`, and it exists because that counter alone cannot
    /// answer the question the veto is for. A zero rejection count is consistent with three very
    /// different worlds: no settle was attempted, every settle was accepted, or the mode publishes
    /// no template. Only the accept count separates "the gate passed this input" from "the gate
    /// never saw it" — which is exactly what a test feeding deliberate interference must know.
    rho_accepted_settles: u64,
    /// Receiver-side streaming AGC on captured audio (default off). Normalises the level so the
    /// PSK/QAM ladder sees a consistent amplitude despite QSB fading and inter-station spread.
    /// Active-span gated: the gain only adapts on carrier-present blocks (RMS ≥ DCD threshold) and
    /// is frozen through silence, so a long leading gap can't ramp it to its clamp before the burst.
    agc_enabled: bool,
    /// The AGC loop, used only while `agc_enabled`.
    agc: openpulse_dsp::agc::Agc,
    /// Count of capture blocks the AGC processed — same tripwire role as `notch_blocks_processed`.
    agc_blocks_processed: u64,
    /// Count of capture blocks the DC block processed — tripwire for the always-on DC removal
    /// (REQ-PHY-02); stays 0 if a capture path ever skips the InputCapture seam.
    dc_blocks_processed: u64,
    /// Count of capture blocks DCD processed at the seam — tripwire for the carrier detector, which
    /// runs on the PRE-AGC level so the AGC's boost can't fool the squelch (stays 0 if a capture path
    /// skips the InputCapture seam).
    dcd_blocks_processed: u64,
    /// Runtime calibration of the correlation threshold to this station's own noise (#1060,
    /// REQ-RX-02). Fed from the veto's own query stream, so it costs no extra correlation.
    rho_calibration: crate::rho_calibration::RhoCalibration,
    /// Whether the veto is currently standing down because no threshold separates noise from
    /// delivered frames (REQ-RX-03). Latched per transition so the log says it once.
    rho_stand_down: bool,
    /// Settles the veto let through *because* it was standing down — the count that distinguishes
    /// "the veto agreed" from "the veto was not running". Counted on both acquisition paths since
    /// #1342; on the daemon's phase-2 path the unit is one coarse-grid settle query, the unit
    /// `rho_accepted_settles` already has there.
    rho_stand_down_settles: u64,
    /// Monotonic count of frames emitted at the single TX seam (`stage_emit_output`) — every
    /// transmit path (data, FEC, ACK, retransmit, QSY, ID) increments it once. A pollable
    /// TX-activity signal for the daemon's periodic station-ID timer (REQ-REG-10).
    frames_transmitted: u64,
    /// Tripwire: frames emitted via `transmit_raw_audio` (the JS8 beacon path).
    raw_audio_frames_transmitted: u64,
}

/// CE-SSB TX conditioning clip level as a multiple of the RMS envelope. 2.0×
/// recovered ~2.7 dB average power on OFDM at zero BER cost in the channel-sim
/// measurement (`tests/cessb_power_evm.rs`).
const CESSB_CLIP_RATIO: f32 = 2.0;
/// Peak-stretcher look-ahead window (samples) for CE-SSB TX conditioning.
const CESSB_LOOKAHEAD: usize = 16;

/// How far above the tracked noise floor the channel counts as busy, as an RMS ratio.
///
/// **Bracketed by measurement, not chosen.** On the recorded IC-9700 hot floor the spectral floor
/// reads 0.138 RMS while idle measures 0.126 total, so the margin must exceed 0.126/0.138 = 0.91 or
/// idle reads busy. With a frame at the corpus test's level the total reaches 0.225 against a floor
/// still at 0.140, so it must stay under 0.225/0.140 = 1.61 or the frame never opens the squelch.
/// 1.25 sits between them with ~1.35x headroom on the idle side and ~1.29x on the signal side —
/// about +1.9 dB over the floor, i.e. deliberately sensitive, which is what a data modem wants.
///
/// Both bounds are real failures with names: too low is the permanently-busy daemon this replaced,
/// too high is a receiver that cannot hear. `daemon_squelch_noise_floor.rs` pins both sides.
const DCD_SQUELCH_MARGIN: f32 = 1.25;

/// Absolute lower bound on the squelch, so a digitally-silent input cannot drive the threshold to
/// zero and make every sample a carrier. A guard against a degenerate floor, NOT a squelch policy —
/// the FT-991A capture's floor is 0.0006 RMS, so this must stay well below anything real.
const DCD_MIN_SQUELCH_THRESHOLD: f32 = 0.001;

/// Floor for the [`ModemEngine::burst_cap_samples`] runaway guard (~30 s at 8 kHz), so a fast mode
/// still accumulates a usable burst, and the cap used when the receive mode is unknown or unregistered.
///
/// This was the *whole* cap until 2026-07-29, as a flat constant. It is far shorter than the frames
/// the ladder's slow rungs emit — BPSK31 + Rs measures 532 480 samples (66.6 s) and BPSK63 + Rs
/// 266 240 (33.3 s) — so on a streaming capture it force-flushed mid-frame on every normal
/// transmission, splitting the burst into two preamble-less halves. SL2 (BPSK31) is `hpx_hf`'s
/// `initial_level`, so this sat on the entry rung of every session. Gate:
/// `tests/burst_cap_frame_length.rs`.
///
/// Note what the per-mode figure alone did NOT fix (#1249): it is derived from whatever mode is
/// passed in, and the daemon passes its *configured* mode, not the OTA rung the peer is sending at.
/// `ModemEngine::active_burst_cap_samples` is what closes that gap.
const BURST_MIN_CAP_SAMPLES: usize = 240_000;

/// Absolute ceiling on the burst accumulator (~320 s at 8 kHz, ≈10 MB of `f32`). The cap exists so a
/// carrier that never drops — a DCD threshold under the noise floor — cannot grow the buffer without
/// bound; this is what keeps that property when the per-mode figure is derived from geometry.
const BURST_MAX_CAP_SAMPLES: usize = 2_560_000;

/// Largest value [`fec_slice_factor`] returns. The burst cap must clear the longest frame a mode can
/// emit under *any* FEC, and the receive tick does not know which one the transmitter used.
const MAX_FEC_SLICE_FACTOR: usize = 4;

/// Cap on the [`ModemEngine::last_audio`] window — a few FFT frames is plenty for a
/// representative spectrum row and bounds the per-call clone.
const SPECTRUM_TAP_MAX: usize = 16384;

/// Max soft-LLR bursts retained (and MAP-combined) per OTA mode before the oldest is
/// dropped. Three matches the HARQ diversity depth measured in `harq_fade_diversity`.
const OTA_HARQ_MAX_ATTEMPTS: usize = 3;

/// One Reed–Solomon code block, RS(255,223). A SoftConcatenated frame at or below this is a single
/// block; the burst interleaver only benefits frames larger than one block.
const RS_BLOCK_BYTES: usize = 255;

impl ModemEngine {
    /// Create a new engine backed by the given audio backend.
    pub fn new(audio: Box<dyn AudioBackend>) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            audio,
            plugins: PluginRegistry::new(),
            sequence: 0,
            hpx: HpxSession::new(),
            scheduler: PipelineScheduler::new(8, BackpressurePolicy::Block),
            trust_policy_profile: PolicyProfile::Balanced,
            active_handshake: None,
            last_afc_offset_hz: None,
            afc_correction_hz: 0.0,
            afc_enabled: true,
            afc_step: 0.1,
            center_frequency: 1500.0,
            rate_policy: RateAdaptationPolicy::new(),
            ota: None,
            rx_snr_estimate: None,
            ota_retained_llrs: std::collections::HashMap::new(),
            ota_retained_session: None,
            ack_mac_key: None,
            dcd: DcdState::new(0.01, 800), // 100 ms hold at 8 kHz; re-aimed per band at the seam
            noise_floor: openpulse_dsp::noise_floor::NoiseFloorTracker::default(),
            csma_enabled: false,
            csma_persistence: 0.3,
            event_tx,
            broadcast_seq: 0,
            callsign: String::new(),
            tx_attenuation_db: 0.0,
            tx_limiter_threshold: 0.0,
            max_power_watts: 0.0, // 0.0 means no limit
            tx_session_log: TxSessionLog::new("UNKNOWN"),
            tx_log_path: None,
            tx_log_failed: false,
            default_device: None,
            last_audio: Vec::new(),
            rx_burst: Vec::new(),
            relay_mode: None,
            last_flush_capped: false,
            input_prerouted: false,
            suppress_afc_events: false,
            rx_capturing: false,
            cessb_enabled: true,
            notch_enabled: false,
            notch_bank: openpulse_dsp::notch::NotchBank::new(
                openpulse_dsp::notch::NotchParams::default(),
            ),
            notch_fallback_bw_hz: 2000.0,
            notch_in_band_interferers: Vec::new(),
            rx_mode: None,
            notch_blocks_processed: 0,
            notch_freqs_seen: std::collections::BTreeSet::new(),
            notch_protect_extremes: None,
            settle_condemnations: 0,
            afc_settle_attempts: 0,
            condemned_positions: Vec::new(),
            accepted_settle_positions: Vec::new(),
            sweep_attempt_inputs: Vec::new(),
            deterministic_scan_positions: None,
            deterministic_max_iterations: None,
            settle_failure_limit: None,
            rho_rejected_settles: 0,
            rho_accepted_settles: 0,
            agc_enabled: false,
            // target RMS 0.3 (headroom below ±1.0), slow loop (α=0.02), ±40 dB clamp.
            agc: openpulse_dsp::agc::Agc::new(0.3, 0.02, 40.0),
            agc_blocks_processed: 0,
            dc_blocks_processed: 0,
            dcd_blocks_processed: 0,
            rho_calibration: crate::rho_calibration::RhoCalibration::new(),
            rho_stand_down: false,
            rho_stand_down_settles: 0,
            frames_transmitted: 0,
            raw_audio_frames_transmitted: 0,
        }
    }

    /// Enable the receiver-side automatic notch (removes out-of-band CW interference before
    /// demod). Off by default. The protected band tracks the active mode so the signal is never
    /// notched; an in-band interferer still can't be removed (that is a QSY case).
    pub fn enable_notch(&mut self) {
        self.notch_enabled = true;
    }

    /// Disable the receiver-side automatic notch.
    pub fn disable_notch(&mut self) {
        self.notch_enabled = false;
    }

    /// Whether the receiver-side automatic notch is enabled.
    pub fn is_notch_enabled(&self) -> bool {
        self.notch_enabled
    }

    /// Number of capture blocks the notch has processed. A tripwire for the "feature wired at a
    /// seam the runtime path skips" class of gap: if the notch is enabled but this stays 0 while
    /// the daemon runs, the receive path isn't reaching the front-end seam.
    pub fn notch_blocks_processed(&self) -> u64 {
        self.notch_blocks_processed
    }

    /// Every centre frequency (Hz, 10 Hz bins) the notch has placed since the engine was built.
    ///
    /// The union across blocks, not a snapshot: `notch_active_freqs` reports only the most recent
    /// block, so on a capture whose interferer outlasts the frame it cannot answer "was the
    /// interferer ever notched at all".
    ///
    /// DORMANT(#1148): consumed by the REQ-QRM-01 band probe while the notch's default-on evidence
    /// is re-derived on the post-#1148 wire. Exit condition: the rewritten gate consumes this
    /// permanently, or it is removed with the probe.
    pub fn notch_freqs_seen(&self) -> Vec<f32> {
        self.notch_freqs_seen
            .iter()
            .map(|&b| b as f32 * 10.0)
            .collect()
    }

    /// Extremes of the protected passband over every block the notch processed:
    /// `(lo_min, lo_max, hi_min, hi_max)`, or `None` if it never ran.
    ///
    /// An envelope rather than a snapshot, because the band is `center + afc_correction_hz ± bw/2`
    /// and therefore MOVES with the AFC. A frequency `f` was protected in EVERY block iff
    /// `f >= lo_max && f <= hi_min`; otherwise it was exposed in at least one block. A last-write
    /// snapshot cannot distinguish those, which is the same defect `notch_freqs_seen` exists to
    /// avoid for the placed frequencies.
    ///
    /// DORMANT(#1148): an interferer inside this band is structurally un-notchable
    /// (`peaks_from_spectrum` skips the protected span), so a QRM test whose tone falls inside it
    /// measures nothing about the notch — and that is not visible from the tone's frequency alone,
    /// because `notch_fallback_bw_hz` is 4x BPSK250's real occupied width. Same exit condition as
    /// [`Self::notch_freqs_seen`].
    pub fn notch_protect_extremes(&self) -> Option<(f32, f32, f32, f32)> {
        self.notch_protect_extremes
    }

    /// How many settle anchors the micro-sweep has condemned and returned to the scan.
    ///
    /// Exposed so a test can gate the *cost* of the settle recovery, not merely whether it
    /// eventually worked. `the_real_on_air_frame_decodes` passed throughout the period when
    /// recovery re-anchored 78 times (#1021) and again when it crawled 15 times (#1040): a
    /// decode-or-not assertion cannot see the difference.
    pub fn settle_condemnations(&self) -> u64 {
        self.settle_condemnations
    }

    /// Sample positions of condemned settle anchors, oldest first (bounded).
    pub fn condemned_positions(&self) -> &[usize] {
        &self.condemned_positions
    }

    /// Sample positions of settles the correlation accepted, oldest first (bounded).
    pub fn accepted_settle_positions(&self) -> &[usize] {
        &self.accepted_settle_positions
    }

    /// Per-attempt micro-sweep inputs: `(attempt_index, onset, window_len, afc_before)`.
    pub fn sweep_attempt_inputs(&self) -> &[(usize, usize, usize, f32)] {
        &self.sweep_attempt_inputs
    }

    /// Budget the retry scan in positions rather than wall-clock time.
    ///
    /// For reproducible measurement only; `None` (the default) keeps production behaviour. See
    /// the field docs for why the wall-clock budget makes decode outcomes machine-dependent.
    pub fn set_deterministic_scan_positions(&mut self, positions: Option<usize>) {
        self.deterministic_scan_positions = positions;
    }

    /// Cap outer receive-loop iterations instead of using the wall-clock listen deadline.
    ///
    /// Pair with [`Self::set_deterministic_scan_positions`]; bounding one without the other still
    /// leaves the verdict machine-dependent. Measurement only — `None` keeps production behaviour.
    pub fn set_deterministic_max_iterations(&mut self, iterations: Option<usize>) {
        self.deterministic_max_iterations = iterations;
    }

    /// Override the anchor-condemnation threshold. `None` = the shipped `2 * SWEEP_OFFSETS`.
    pub fn set_settle_failure_limit(&mut self, limit: Option<usize>) {
        self.settle_failure_limit = limit;
    }

    /// Times the AFC settle was entered — acquisition-chain tripwire (#1118). See the field docs.
    ///
    /// DORMANT(#1118): no production caller by design. It is an instrument, like its four siblings
    /// (`rho_accepted_settles`, `rho_rejected_settles`, `dcd_blocks_processed`,
    /// `settle_condemnations`), which are all likewise unreferenced from production and baselined.
    /// A tripwire whose value production consumed would be a feature, not a tripwire.
    ///
    /// It becomes reachable — and should leave `reachability-baseline.txt` — if the daemon seam is
    /// closed and the acquisition chain gains a streaming-path home worth reporting in
    /// `SessionDiagnostics`. Do NOT invent a production caller to satisfy the ratchet; that trades a
    /// truthful "dormant" for a fictional "used".
    pub fn afc_settle_attempts(&self) -> u64 {
        self.afc_settle_attempts
    }

    /// How many candidate settles the preamble correlation refused (#1049).
    ///
    /// Zero has two meanings and a test must distinguish them: the gate ran and accepted
    /// everything, or the mode publishes no preamble template and the gate never ran at all. Pair
    /// this with a case that must reject.
    pub fn rho_rejected_settles(&self) -> u64 {
        self.rho_rejected_settles
    }

    /// Settles the preamble correlation accepted. See [`Self::rho_rejected_settles`].
    pub fn rho_accepted_settles(&self) -> u64 {
        self.rho_accepted_settles
    }

    /// Tripwire count of capture blocks the DCD carrier detector processed at the seam. DCD runs on the
    /// PRE-AGC level, so an enabled AGC's boost can't push sub-squelch noise over the busy threshold.
    pub fn dcd_blocks_processed(&self) -> u64 {
        self.dcd_blocks_processed
    }

    /// How many correlation samples the runtime threshold calibration holds (#1060, REQ-RX-02).
    ///
    /// A tripwire, not a curiosity: a calibration that never runs on a receive path reads as a
    /// working feature until this stays 0.
    pub fn rho_calibration_samples(&self) -> usize {
        self.rho_calibration.len()
    }

    /// The threshold the veto is actually comparing against for `mode`, published constant included.
    pub fn rho_effective_threshold(&self, mode: &str) -> Option<f32> {
        let veto = self.build_preamble_veto(mode, AudioConfig::default().sample_rate)?;
        Some(self.rho_calibration.effective_threshold(veto.rho_threshold))
    }

    /// Whether the veto is standing down for want of a separating threshold (REQ-RX-03), and how
    /// many settles it has let through in that state.
    pub fn rho_stand_down(&self) -> (bool, u64) {
        (self.rho_stand_down, self.rho_stand_down_settles)
    }

    /// Monotonic count of frames emitted at the TX seam. The daemon polls the delta to detect
    /// transmit activity for the periodic station-ID timer (REQ-REG-10) without threading a
    /// `note_tx()` call through every transmit call site.
    pub fn frames_transmitted(&self) -> u64 {
        self.frames_transmitted
    }

    /// Emit a Morse CW station identification (keyed sine) for `text` through the single TX seam —
    /// used to honour the ARDOP `CWID` option alongside the digital ID. Counts as a transmitted
    /// frame (`frames_transmitted`). No-op (returns `Ok`) when `text` has no renderable characters.
    pub fn emit_cw_id(&mut self, text: &str, device: Option<&str>) -> Result<(), ModemError> {
        let fs = AudioConfig::default().sample_rate;
        let samples = openpulse_core::cw_id::CwId::default().samples(text, fs);
        if samples.is_empty() {
            return Ok(());
        }
        let routed = self.route_audio_stage(PipelineStage::OutputEmit, AudioSamples { samples })?;
        self.stage_emit_output(device, "CW", &routed)
    }

    /// Configure the notch bank: max simultaneous notches, sharpness `q` (BW ≈ f0/q), and the
    /// protected-band fallback bandwidth (Hz) used when the active mode can't report its own.
    pub fn configure_notch(&mut self, max_notches: usize, q: f32, fallback_bw_hz: f32) {
        use openpulse_dsp::notch::{NotchBank, NotchParams};
        self.notch_bank = NotchBank::new(NotchParams {
            max_notches,
            q,
            ..NotchParams::default()
        });
        self.notch_fallback_bw_hz = fallback_bw_hz;
    }

    /// Enable notch persistence/silence tracking: a tone must appear in this many signal-absent
    /// blocks before it counts as a confirmed external interferer. 0 disables it (default). This
    /// lets the notch null externally-confirmed tones robustly, and surfaces in-band ones via
    /// [`in_band_interferers`](Self::in_band_interferers) for QSY.
    pub fn set_notch_persistence(&mut self, min_silence_hits: u32) {
        self.notch_bank.set_persistence(min_silence_hits);
    }

    /// Confirmed in-band interferers (Hz): a notch can't remove these without harming the signal,
    /// so they are QSY (move-frequency) candidates. Empty unless notch persistence is enabled.
    pub fn in_band_interferers(&self) -> &[f32] {
        &self.notch_in_band_interferers
    }

    /// Forget the confirmed in-band interferers and the notch persistence state — e.g. after a
    /// QSY to a new frequency, where the old interferers no longer apply.
    pub fn clear_in_band_interferers(&mut self) {
        self.notch_in_band_interferers.clear();
        self.notch_bank.clear_persistence();
    }

    /// Centre frequencies (Hz) of the notches placed on the most recent captured block.
    ///
    /// TODO(#1092): no caller. Inherits REQ-QRM-01 (notch default-on) as the observability half of
    /// its surface — the siblings `in_band_interferers` / `clear_in_band_interferers` are live, this
    /// one has no consumer. Wire it into the diagnostics bundle only if REQ-OBS grows to ask for it;
    /// do not invent the requirement to justify the accessor.
    pub fn notch_active_freqs(&self) -> Vec<f32> {
        self.notch_bank.active_freqs()
    }

    /// Apply the receiver notch to a captured block: protect the active mode's occupied band
    /// (so the signal is never notched), then null out-of-band CW interferers. When persistence
    /// is on, feed the block to the silence tracker and surface any confirmed in-band interferer
    /// (a QSY case the notch can't fix).
    fn apply_rx_notch(&mut self, mode: Option<&str>, samples: Vec<f32>) -> Vec<f32> {
        let center = self.center_frequency + self.afc_correction_hz;
        let bw = mode
            .and_then(|m| self.plugins.get(m).and_then(|p| p.occupied_bandwidth_hz(m)))
            .unwrap_or(self.notch_fallback_bw_hz);
        let half = bw / 2.0;
        let (lo, hi) = ((center - half).max(0.0), center + half);
        self.notch_protect_extremes = Some(match self.notch_protect_extremes {
            None => (lo, lo, hi, hi),
            Some((lo_min, lo_max, hi_min, hi_max)) => (
                lo_min.min(lo),
                lo_max.max(lo),
                hi_min.min(hi),
                hi_max.max(hi),
            ),
        });
        self.notch_bank.set_protect_band(lo, hi);

        // Persistence: the bank classifies the block (our wideband signal fills the protected
        // band; a lone CW tone does not), so it can tell an external interferer from our own lines.
        self.notch_bank.observe(&samples);
        let in_band = self.notch_bank.in_band_interferers();
        if in_band != self.notch_in_band_interferers {
            if !in_band.is_empty() {
                tracing::warn!(freqs_hz = ?in_band, "in-band interference confirmed; a notch cannot remove it — QSY recommended");
            }
            self.notch_in_band_interferers = in_band;
        }
        self.notch_bank.process_block(&samples)
    }

    /// Enable the receiver-side streaming AGC (level normalisation before demod). Off by default.
    pub fn enable_agc(&mut self) {
        self.agc_enabled = true;
    }

    /// Disable the receiver-side streaming AGC.
    pub fn disable_agc(&mut self) {
        self.agc_enabled = false;
        self.agc.reset();
    }

    /// Whether the receiver-side streaming AGC is enabled.
    pub fn is_agc_enabled(&self) -> bool {
        self.agc_enabled
    }

    /// Configure the AGC loop: target output RMS, adaptation rate `bandwidth` (α in (0,1]), and the
    /// symmetric gain clamp in dB. Resets the loop. See [`openpulse_dsp::agc::Agc::new`].
    pub fn configure_agc(&mut self, target_rms: f32, bandwidth: f32, max_gain_db: f32) {
        self.agc = openpulse_dsp::agc::Agc::new(target_rms, bandwidth, max_gain_db);
    }

    /// Number of capture blocks the AGC has processed — a tripwire for the "feature wired at a seam
    /// the runtime path skips" class of gap (see [`Self::notch_blocks_processed`]).
    pub fn agc_blocks_processed(&self) -> u64 {
        self.agc_blocks_processed
    }

    /// Number of capture blocks the DC block (REQ-PHY-02) has processed — a tripwire that the
    /// always-on DC removal runs on every receive path (it lives at the single InputCapture seam).
    pub fn dc_blocks_processed(&self) -> u64 {
        self.dc_blocks_processed
    }

    /// Current AGC gain in dB (0 dB = unity). A readout of the active-span loop state.
    pub fn agc_gain_db(&self) -> f32 {
        self.agc.gain_db()
    }

    /// Apply the streaming AGC to one capture block, active-span gated: the gain only adapts on
    /// carrier-present blocks (RMS ≥ DCD squelch) and is frozen through silence, so a long leading
    /// gap can't ramp the gain to its clamp before the burst arrives.
    fn apply_rx_agc(&mut self, mut samples: Vec<f32>) -> Vec<f32> {
        let n = samples.len();
        let rms = if n == 0 {
            0.0
        } else {
            (samples.iter().map(|s| s * s).sum::<f32>() / n as f32).sqrt()
        };
        if rms >= self.dcd.threshold() {
            self.agc.unlock();
        } else {
            self.agc.lock();
        }
        self.agc.process(&mut samples);
        samples
    }

    /// Enable/disable CE-SSB TX envelope conditioning (master switch). It still
    /// only acts on modes that benefit ([`cessb_benefits`](Self::cessb_benefits)).
    pub fn set_cessb_enabled(&mut self, enabled: bool) {
        self.cessb_enabled = enabled;
    }

    /// Whether CE-SSB TX conditioning is enabled (master switch).
    pub fn cessb_enabled(&self) -> bool {
        self.cessb_enabled
    }

    /// Whether `mode` benefits from CE-SSB conditioning — **only the QPSK-subcarrier
    /// OFDM waveforms** (`OFDM16`, `OFDM52`). Every denser constellation and every
    /// single-carrier waveform is excluded, each decided by end-to-end decode through
    /// the real engine+channel path, not synthetic raw-BER:
    ///
    /// 1. **SC-FDMA is excluded entirely.** Despite its multicarrier subcarrier
    ///    structure it is a *single-carrier* FDM waveform, low-PAPR by construction,
    ///    so CE-SSB recovers only ~⅓ of OFDM's average-power gain (2.6 vs 8.5 dB at
    ///    the 2.0×rms operating point on the 64QAM rung) while its EVM alone injects
    ///    ~0.5 % raw BER — collapsing decode of SCFDMA52-{32,64}QAM (5/30 vs 30/30
    ///    through AWGN 35 dB).
    /// 2. **OFDM ≥16QAM is excluded.** Their decision regions are too tight for the
    ///    CE-SSB EVM. 32/64QAM collapse outright (OFDM52-32QAM 0/20, -64QAM 3/20 vs
    ///    20/20 off, soft FEC through AWGN); 16QAM is marginal — it survives easy
    ///    AWGN but breaks on the realistic HF fading path (OFDM52-16QAM soft-FEC
    ///    Watterson Good-F1: 0/16 on vs 16/16 off; uncoded AWGN 20 dB: 0/20 vs
    ///    20/20). The earlier `cessb_benefits_hold_on_ofdm_hom` claim measured raw
    ///    BER at a fixed operating point and missed the acquisition/decode failure
    ///    on the real path (DSP playbook: validate FEC-protected modes WITH FEC,
    ///    and against the fading channel — dense constellations are the canaries).
    ///
    /// The exclusions all trace to one principle: **CE-SSB trades in-band EVM for
    /// average-power gain, and that trade only wins where the envelope is high-PAPR
    /// *and* the decision margins are loose.** QPSK-subcarrier OFDM sums ~52 carriers
    /// into a near-Gaussian envelope that rarely nulls hard, so envelope limiting costs
    /// almost no EVM; higher-order (8PSK/QAM/APSK) subcarriers and single-carrier QAM
    /// transit near the constellation origin, where the envelope passes through zero and
    /// the instantaneous phase goes discontinuous (the "equal-amplitude" singularity —
    /// *Dave's Hacks*, Feb 2025, catalogued in `docs/dev/research/references.md`).
    /// Limiting that envelope injects EVM their tighter slicers can't absorb, so CE-SSB
    /// is gated OFF for them — 8PSK included (a marginal-SNR sweep goes 12/12 → 0/12
    /// with CE-SSB on, and decodes only once gated off).
    /// Measured in `openpulse-linksim/tests/cessb_ab.rs` and `tests/cessb_power_evm.rs`.
    pub fn cessb_benefits(mode: &str) -> bool {
        let m = mode.to_ascii_uppercase();
        if !m.starts_with("OFDM") {
            return false;
        }
        // Only the QPSK-subcarrier OFDM modes (OFDM16, OFDM52) tolerate the clip. Every
        // higher-order constellation is gated off: the in-band clipping distortion exceeds the
        // tighter decision margins, costing several dB — peak-fair `cessb_power_evm` shows
        // OFDM52-8PSK going BER 0.0000→0.0026, and a marginal-SNR sweep has it fail entirely with
        // CE-SSB on (12/12 → 0/12 at 12–16 dB) but decode once gated off.
        !(m.contains("8PSK")
            || m.contains("16QAM")
            || m.contains("32QAM")
            || m.contains("64QAM")
            || m.contains("32APSK"))
    }

    /// Apply CE-SSB envelope conditioning to a real passband TX block and rescale
    /// to the original peak, so the freed headroom becomes average power at the same
    /// PEP. Returns the input unchanged if the envelope is degenerate.
    fn cessb_condition_tx(&self, samples: &[f32]) -> Vec<f32> {
        let fs = AudioConfig::default().sample_rate as f32;
        let (i, q) = openpulse_core::iq::hilbert_iq(samples, self.center_frequency, fs);
        let env = openpulse_dsp::cessb::envelope(&i, &q);
        let rms_env = (env.iter().map(|e| e * e).sum::<f32>() / env.len().max(1) as f32).sqrt();
        if rms_env <= f32::MIN_POSITIVE {
            return samples.to_vec();
        }
        let level = CESSB_CLIP_RATIO * rms_env;
        let gain = openpulse_dsp::cessb::peak_stretch_gain(&env, level, CESSB_LOOKAHEAD);
        let mut out = openpulse_dsp::cessb::apply_gain(samples, &gain);
        // Restore the original peak: the average-power gain is realised by scaling
        // the now-lower-PAPR signal back up to the same peak (PEP).
        let p0 = samples.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        let p1 = out.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        if p0 > 0.0 && p1 > f32::MIN_POSITIVE {
            let scale = p0 / p1;
            for x in &mut out {
                *x *= scale;
            }
        }
        out
    }

    /// Most recent audio window the engine captured (RX) or emitted (TX).
    ///
    /// Bounded to the last [`SPECTRUM_TAP_MAX`] samples; empty until the first
    /// transmit/receive. Intended for a spectrum/waterfall tap so the FFT sees real
    /// audio rather than silence.
    pub fn last_audio(&self) -> &[f32] {
        &self.last_audio
    }

    /// Record the most recent audio window for the spectrum tap (keeps the tail).
    fn record_audio(&mut self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        let start = samples.len().saturating_sub(SPECTRUM_TAP_MAX);
        self.last_audio.clear();
        self.last_audio.extend_from_slice(&samples[start..]);
    }

    /// Pin all audio I/O to `device` (by backend device name) when a per-call
    /// `device` argument is `None`. Pass `None` to clear (use the backend default).
    ///
    /// Used by the daemon to bind one engine to a specific full-duplex device — the
    /// real-audio twin-station rig points station A at one `snd-aloop` PCM and
    /// station B at the crossed PCM so the kernel routes A↔B.
    pub fn set_default_device(&mut self, device: Option<String>) {
        self.default_device = device;
    }

    /// Subscribe to the real-time engine event stream.
    ///
    /// Returns a [`broadcast::Receiver`] that receives every [`EngineEvent`]
    /// emitted after this call.  If a receiver falls behind, `try_recv()` returns
    /// `TryRecvError::Lagged(n)` indicating the number of dropped events; callers
    /// must handle this variant explicitly.
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.event_tx.subscribe()
    }

    /// Returns the active trust policy profile used as session default.
    pub fn trust_policy_profile(&self) -> PolicyProfile {
        self.trust_policy_profile
    }

    /// Sets the active trust policy profile used as session default.
    pub fn set_trust_policy_profile(&mut self, profile: PolicyProfile) {
        self.trust_policy_profile = profile;
    }

    /// Returns the residual carrier frequency error measured at the corrected
    /// reference from the most recent demodulation call, in Hz.
    ///
    /// This is the error *after* `afc_correction_hz` has been applied.  The
    /// total offset from the nominal centre frequency is approximately
    /// `afc_correction_hz() + last_afc_offset_hz()`.  Returns `None` until the
    /// first receive or if the active plugin does not support AFC.
    pub fn last_afc_offset_hz(&self) -> Option<f32> {
        self.last_afc_offset_hz
    }

    /// Returns the accumulated AFC carrier correction applied to demodulation (Hz).
    pub fn afc_correction_hz(&self) -> f32 {
        self.afc_correction_hz
    }

    /// Sets the audio centre frequency used for modulation and demodulation.
    pub fn set_center_frequency(&mut self, hz: f32) {
        self.center_frequency = hz;
    }

    /// Returns the audio centre frequency.
    pub fn center_frequency(&self) -> f32 {
        self.center_frequency
    }

    /// Enable the AFC tracking loop (default: enabled).
    ///
    /// No caller (#1092): `disable_afc` is driven by a CLI flag at startup and the processes that
    /// use it are one-shot, so nothing needs to restore the default at runtime. Kept as the
    /// symmetric setter of a live toggle — it inherits REQ-PHY-03 from the AFC loop itself.
    pub fn enable_afc(&mut self) {
        self.afc_enabled = true;
    }

    /// Disable the AFC tracking loop.
    pub fn disable_afc(&mut self) {
        self.afc_enabled = false;
    }

    /// Reset the accumulated AFC correction and offset estimate to zero.
    pub fn reset_afc(&mut self) {
        self.afc_correction_hz = 0.0;
        self.last_afc_offset_hz = None;
    }

    /// Enable 0.3-persistence CSMA channel access control.
    ///
    /// When enabled, [`transmit`](Self::transmit) checks the DCD state before
    /// emitting audio.  If the channel is busy, or if the random p-persistence
    /// draw fails (70% of the time on a clear channel), it returns
    /// [`ModemError::ChannelBusy`] and the caller should back off and retry.
    pub fn enable_csma(&mut self) {
        self.csma_enabled = true;
    }

    /// Disable CSMA channel access control.
    pub fn disable_csma(&mut self) {
        self.csma_enabled = false;
    }

    /// Whether CSMA channel-access deferral is currently enabled.
    pub fn is_csma_enabled(&self) -> bool {
        self.csma_enabled
    }

    /// Returns `true` if the DCD detector currently sees a busy channel.
    pub fn is_channel_busy(&self) -> bool {
        self.dcd.is_busy()
    }

    /// Returns the most recent DCD RMS energy estimate.
    pub fn dcd_energy(&self) -> f32 {
        self.dcd.energy()
    }

    /// Check CSMA policy and return `ChannelBusy` if the channel is occupied
    /// or the p-persistence draw fails.  Called before encoding to avoid
    /// burning sequence numbers on a deferred transmission.
    fn csma_check(&self) -> Result<(), ModemError> {
        if !self.csma_enabled {
            return Ok(());
        }
        if self.dcd.is_busy() {
            return Err(ModemError::ChannelBusy);
        }
        // 0.3-persistence: transmit only with 30% probability on a clear channel
        let p: f32 = rand::thread_rng().gen();
        if p >= self.csma_persistence {
            return Err(ModemError::ChannelBusy);
        }
        Ok(())
    }

    /// Begin an adaptive-rate session using the given profile.
    ///
    /// Initialises a bidirectional rate adapter
    /// ([`openpulse_core::rate::BiDirRateAdapter`]) at `profile.initial_level`
    /// and stores the profile so that
    /// [`current_adaptive_mode`](Self::current_adaptive_mode)
    /// can resolve the current mode string on each transmit/receive cycle.
    pub fn start_adaptive_session(&mut self, profile: SessionProfile) {
        self.rate_policy.start_session(profile);
    }

    /// Cap the adaptive ladder at `max` (host/bandwidth limit, e.g. ARDOP `ARQBW`); `None` clears
    /// the cap. The active session is clamped immediately and future AckUp steps respect it.
    pub fn set_arq_max_tx_level(&mut self, max: Option<openpulse_core::rate::SpeedLevel>) {
        self.rate_policy.set_max_tx_level(max);
    }

    /// The active adaptive profile's defined `(level, mode)` pairs (ascending), for mapping a
    /// bandwidth cap in Hz to a max speed level. Empty when no adaptive session is active.
    pub fn adaptive_profile_modes(&self) -> Vec<(openpulse_core::rate::SpeedLevel, &'static str)> {
        self.rate_policy.defined_modes()
    }

    /// A2 (backlog-aware gating): minimum queued TX bytes required before an
    /// AckUp upgrade is acted on. `0` (default) disables the gate. Prevents
    /// spending upgrade airtime when only a frame or two remain queued.
    pub fn set_min_backlog_for_upgrade(&mut self, bytes: usize) {
        self.rate_policy.set_min_backlog_for_upgrade(bytes);
    }

    /// A2: update the current queued TX backlog (bytes) used by the gate.
    pub fn set_tx_backlog(&mut self, bytes: usize) {
        self.rate_policy.set_tx_backlog(bytes);
    }

    /// A3 (anti-oscillation): suppress this many upgrade attempts after a
    /// downgrade. `0` (default) disables the hold.
    pub fn set_upgrade_hold_frames(&mut self, frames: u32) {
        self.rate_policy.set_upgrade_hold_frames(frames);
    }

    /// Apply an [`OtaAggressiveness`] preset: sets the A2 backlog gate and the A3
    /// re-upgrade hold together so an operator picks one behaviour instead of
    /// tuning both knobs.
    pub fn set_ota_aggressiveness(&mut self, preset: OtaAggressiveness) {
        let (min_backlog, hold) = preset.knobs();
        self.rate_policy.set_min_backlog_for_upgrade(min_backlog);
        self.rate_policy.set_upgrade_hold_frames(hold);
    }

    /// Apply a received ACK type to the TX-direction rate adapter.
    ///
    /// Returns [`RateEvent::Maintained`] when no adaptive session is active.
    pub fn apply_ack(&mut self, ack: AckType) -> RateEvent {
        let (event, payload) = self.rate_policy.apply_ack(ack);
        if let Some(p) = payload {
            self.emit_rate_change(p);
        }
        event
    }

    /// Apply a received ACK frame, updating both TX and RX directions.
    ///
    /// When the frame carries a `reverse_ack`, the RX-direction adapter is also
    /// updated and a second `RateChange` event is emitted.
    pub fn apply_ack_frame(&mut self, frame: &openpulse_core::ack::AckFrame) -> RateEvent {
        let (tx_event, payloads) = self.rate_policy.apply_ack_frame(frame);
        for p in payloads {
            self.emit_rate_change(p);
        }
        tx_event
    }

    fn emit_rate_change(&self, payload: RateChangePayload) {
        let _ = self.event_tx.send(EngineEvent::RateChange {
            event: payload.event,
            speed_level: payload.speed_level,
            mode: payload.mode,
            direction: payload.direction,
            trigger: payload.trigger,
        });
    }

    /// Return the mode string for the current TX speed level of the active adaptive session.
    ///
    /// Returns `None` when no profile is active or the current speed level has no
    /// mode assigned (e.g. SL1 chirp fallback, reserved levels).
    pub fn current_adaptive_mode(&self) -> Option<&str> {
        self.rate_policy.current_adaptive_mode()
    }

    /// Return the mode string for the current RX speed level.
    pub fn current_rx_mode(&self) -> Option<&str> {
        self.rate_policy.current_rx_mode()
    }

    /// Return the current TX [`SpeedLevel`](openpulse_core::rate::SpeedLevel).
    pub fn current_tx_level(&self) -> Option<openpulse_core::rate::SpeedLevel> {
        self.rate_policy.current_tx_level()
    }

    /// Return the SNR estimate (dB) measured during the most recent
    /// [`receive`](Self::receive) or [`receive_with_ack_hint`](Self::receive_with_ack_hint) call.
    ///
    /// Derived from mean absolute LLR magnitude; useful for display and logging.
    /// Returns `None` if no receive call that supports soft demodulation has completed yet.
    pub fn last_rx_snr_db(&self) -> Option<f32> {
        self.rate_policy.last_rx_snr_db()
    }

    /// Apply a raw SNR estimate to the TX-direction rate adapter.
    ///
    /// If `snr_db` drops below the per-level SNR floor in the active session
    /// profile, the TX speed level is stepped down immediately — without waiting
    /// for a NACK — and a [`EngineEvent::RateChange`] is emitted with
    /// `trigger: Some(SnrFloor)`.  If `snr_db` rises above the ceiling, the
    /// upgrade-candidate flag is set; no level change occurs until the next
    /// ACK-UP is received.
    ///
    /// Does nothing when no adaptive session is active.
    pub fn apply_snr_hint(&mut self, snr_db: f32) {
        if let Some(payload) = self.rate_policy.apply_snr_hint(snr_db) {
            self.emit_rate_change(payload);
        }
    }

    // ── Receiver-led OTA adaptive rate-stepping ────────────────────────────────

    /// Start a receiver-led, per-direction OTA rate session for `profile`.
    ///
    /// Pairs [`respond_arq_ota`](Self::respond_arq_ota) (data receiver, leads its
    /// direction) with [`apply_ota_ack`](Self::apply_ota_ack) +
    /// [`ota_tx_mode`](Self::ota_tx_mode) (data sender, follows the peer).
    ///
    /// **A profile with an empty or partial `fec_modes` table is a legal OTA profile** (#1126).
    /// `SessionProfile::fec_for` yields `FecMode::None` for any rung the table does not populate, so
    /// such a ladder transmits uncoded — deliberately, in `hpx_modcod`'s SL7. The consequence to know
    /// before choosing one: on an uncoded rung whose mode equals the station's active mode, a ladder
    /// frame and a non-ladder control frame (station ID, filexfer, handshake, QSY, relay) are
    /// byte-identical on the wire, so the receive arm's candidates-first ordering is the only thing
    /// separating them and control traffic can be counted as a ladder decode by the evidence-based
    /// climb.
    pub fn start_ota_session(&mut self, profile: SessionProfile) {
        self.ota = Some(OtaRateController::new(profile));
        self.ota_retained_llrs.clear();
        self.ota_retained_session = None;
    }

    /// Set (or clear) the per-session OTA-ACK MAC key derived from the handshake key agreement (E7).
    /// When set, OTA rate ACKs are authenticated with a keyed MAC; `None` restores the legacy path.
    pub fn set_ack_mac_key(&mut self, key: Option<[u8; 32]>) {
        self.ack_mac_key = key;
    }

    /// Whether an OTA-ACK MAC key is currently set (test/observability).
    pub fn has_ack_mac_key(&self) -> bool {
        self.ack_mac_key.is_some()
    }

    /// Stop the active OTA session (drops the controller). No-op if none active.
    pub fn stop_ota_session(&mut self) {
        self.ota = None;
        self.ota_retained_llrs.clear();
        self.ota_retained_session = None;
    }

    /// Whether a receiver-led OTA session is active.
    pub fn ota_active(&self) -> bool {
        self.ota.is_some()
    }

    /// Mode string the local station should transmit data at under the OTA session.
    pub fn ota_tx_mode(&self) -> Option<&str> {
        self.ota.as_ref().and_then(|o| o.tx_mode())
    }

    /// FEC scheme to transmit data with at the current OTA TX level (MODCOD).
    /// Returns [`FecMode::None`] when no OTA session is active.
    pub fn ota_tx_fec(&self) -> FecMode {
        self.ota
            .as_ref()
            .map(|o| o.tx_fec())
            .unwrap_or(FecMode::None)
    }

    /// Current OTA TX speed level (the level the peer last recommended to us).
    pub fn ota_tx_level(&self) -> Option<SpeedLevel> {
        self.ota.as_ref().map(|o| o.tx_level())
    }

    /// One-RS-block OTA payload cap for the fixed 255-byte MFSK16 sub-floor frame:
    /// `BLOCK_DATA_STANDARD(223) − 4 B RS length prefix − 10 B Frame envelope`. A larger body needs ≥2 RS
    /// blocks, which the MFSK16 modulator rejects — so the sub-floor rung is skipped for it.
    pub const MFSK16_OTA_MAX_PAYLOAD: usize = 223 - 4 - 10;

    /// Whether a `body_len`-byte OTA data frame can ride the current TX rung. Only the fixed 255-byte MFSK16
    /// sub-floor frame is capacity-limited (one RS block); every other rung carries multi-block RS. A caller
    /// that gets `false` must NOT transmit — a body over the cap can't ride SL1, and bumping it to the next
    /// rung is futile on `hpx_hf` (the peer's
    /// SL1-settled candidate set never includes SL2), so it would only burn airtime on a doomed frame.
    pub fn ota_payload_fits_tx_rung(&self, body_len: usize) -> bool {
        match self.ota.as_ref().and_then(|o| o.tx_mode()) {
            Some("MFSK16") => body_len <= Self::MFSK16_OTA_MAX_PAYLOAD,
            _ => true,
        }
    }

    /// Absolute level we are currently recommending to the peer (goes in our ACK).
    pub fn ota_rx_recommended_level(&self) -> Option<SpeedLevel> {
        self.ota.as_ref().map(|o| o.rx_recommended_level())
    }

    /// Highest level we have actually decoded (the lockstep anchor).
    pub fn ota_rx_confirmed_level(&self) -> Option<SpeedLevel> {
        self.ota.as_ref().map(|o| o.rx_confirmed_level())
    }

    /// Supply an external RX SNR estimate (dB) for OTA adaptive decisions, or
    /// `None` to fall back to the built-in silence-gated M2M4 moment estimator on
    /// the captured envelope.
    ///
    /// A channel-sim harness that knows the true SNR can feed it here to bypass
    /// the on-air estimate; otherwise the M2M4 estimate drives the rate ladder.
    pub fn set_rx_snr_estimate(&mut self, snr_db: Option<f32>) {
        self.rx_snr_estimate = snr_db;
    }

    /// Clamp the OTA rate ladder to `[min, max]` (each `None` = the profile bound).
    ///
    /// Use to cap the top rung (regulatory bandwidth / robustness) or floor the bottom.
    /// No-op without an active OTA session.
    pub fn ota_set_level_bounds(&mut self, min: Option<SpeedLevel>, max: Option<SpeedLevel>) {
        if let Some(o) = self.ota.as_mut() {
            o.set_level_bounds(min, max);
        }
    }

    /// Pin the OTA session to a fixed level (manual override; stops adapting).
    /// No-op without an active OTA session.
    pub fn ota_lock_level(&mut self, level: SpeedLevel) {
        if let Some(o) = self.ota.as_mut() {
            o.lock_level(level);
        }
    }

    /// Release an OTA level lock and resume adapting. No-op without a session.
    pub fn ota_unlock(&mut self) {
        if let Some(o) = self.ota.as_mut() {
            o.unlock();
        }
    }

    /// Whether the OTA session is locked to a fixed level.
    pub fn ota_is_locked(&self) -> bool {
        self.ota.as_ref().is_some_and(|o| o.is_locked())
    }

    /// Sender side: adopt the peer's absolute `recommended_level` from a received ACK.
    ///
    /// A no-op when the frame carries no recommendation or no OTA session is active.
    /// The absolute target means a lost ACK never desyncs — the next ACK re-states it.
    pub fn apply_ota_ack(&mut self, frame: &AckFrame) {
        if let (Some(o), Some(level)) = (self.ota.as_mut(), frame.recommended_level) {
            o.adopt_recommendation(level);
        }
    }

    /// Sender side (ISS): one-call OTA data frame — transmit at the current OTA
    /// mode+FEC, wait for the FSK4-ACK, adopt the peer's `recommended_level`, and
    /// retry on Nack / missing ACK.
    ///
    /// The half-duplex counterpart to [`respond_arq_ota`](Self::respond_arq_ota):
    /// it transmits then listens on the same device, so it suits a real radio (or a
    /// loopback that feeds TX back to RX) where the peer answers in-band. Returns the
    /// adopted TX [`SpeedLevel`] on success, or [`ModemError::ArqMaxRetries`] after
    /// `1 + max_retries` attempts. Always adopts a `recommended_level` carried by any
    /// ACK (even a Nack) so the absolute target can never drift.
    pub fn transmit_arq_ota(
        &mut self,
        data: &[u8],
        device: Option<&str>,
        max_retries: usize,
    ) -> Result<SpeedLevel, ModemError> {
        // Single-shot ACK receive (timeout 0): the ACK is already in the buffer for
        // synchronous in-process callers (tests). The daemon uses the timeout form.
        self.transmit_arq_ota_within(data, device, max_retries, 0)
    }

    /// As [`transmit_arq_ota`](Self::transmit_arq_ota), but each attempt waits up to
    /// `ack_timeout_ms` for the FSK4-ACK to arrive (re-capturing on the device until
    /// it decodes or the deadline passes). `0` = single-shot (the original behaviour).
    ///
    /// Needed by a free-running daemon: after the data frame is transmitted, the
    /// peer's ACK only returns after its own receive tick + the channel round-trip,
    /// so a single immediate read misses it. With a timeout the sender owns the RX
    /// for the turnaround and adopts the peer's absolute `recommended_level` — which
    /// is what steps the rate ladder.
    pub fn transmit_arq_ota_within(
        &mut self,
        data: &[u8],
        device: Option<&str>,
        max_retries: usize,
        ack_timeout_ms: u64,
    ) -> Result<SpeedLevel, ModemError> {
        let attempts = 1 + max_retries;
        let mut last_err: Option<ModemError> = None;
        for _ in 0..attempts {
            let mode = self
                .ota_tx_mode()
                .ok_or_else(|| ModemError::Configuration("no OTA session active".into()))?
                .to_owned();
            // Opportunistically strengthen Rs → RsStrong when the stronger code costs no extra RS
            // block for this frame's size (free on the wire; #934 follow-up). Roughly doubles the weak
            // rungs' fading decode on small frames; a no-op on the sizes that would need a 2nd block.
            let fec = openpulse_core::fec::free_rs_strengthening(
                self.ota_tx_fec(),
                data.len() + openpulse_core::frame::Frame::WIRE_OVERHEAD,
            );
            self.transmit_with_fec_mode(data, &mode, fec, device)?;
            match self.receive_ack_with_short_fec_within(device, ack_timeout_ms) {
                Ok(ack) => {
                    self.apply_ota_ack(&ack);
                    if ack.ack_type != AckType::Nack {
                        return self.ota_tx_level().ok_or_else(|| {
                            ModemError::Configuration("no OTA session active".into())
                        });
                    }
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or(ModemError::ArqMaxRetries(attempts)))
    }

    /// Receiver side: capture one data frame, demodulate with the OTA candidate
    /// fallback, reply with an ACK carrying the absolute `recommended_level`, and
    /// return the payload.
    ///
    /// Tries the candidate modes (`{recommended, confirmed}`, recommended first) on
    /// the *same* captured buffer, so a sender that has not yet adopted our last
    /// recommendation (lost ACK) is still decoded at the confirmed level. On total
    /// decode failure it replies `Nack` (still carrying the current recommendation)
    /// and returns the decode error.
    pub fn respond_arq_ota(
        &mut self,
        session_id: &str,
        device: Option<&str>,
    ) -> Result<Vec<u8>, ModemError> {
        let samples = self.stage_capture_input(None, device)?;
        let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;

        // `None` fallback mode: this entry has no production caller and no notion of an active
        // non-ladder mode (`stage_capture_input(None, ..)` above leaves `rx_mode` unset), so it keeps
        // its pre-#1123 behaviour exactly. Pinned by `fallback_mode_none_changes_nothing`.
        let (decoded, ack_frame, last_err) = self.ota_decode_and_ack(&samples, session_id, None)?;
        // `ack_frame` is always `Some` here: it is `None` only for an uncoded-fallback decode, which
        // a `None` fallback mode cannot produce.
        if let Some(ack) = &ack_frame {
            self.transmit_ack_with_short_fec(ack, device)?;
        }

        match decoded {
            Some((payload, mode)) => {
                let _ = self.event_tx.send(EngineEvent::FrameReceived {
                    mode,
                    bytes: payload.len(),
                });
                Ok(payload)
            }
            None => Err(last_err.unwrap_or_else(|| {
                ModemError::Configuration("OTA receive: no candidate decoded".into())
            })),
        }
    }

    /// Daemon-facing OTA receive poll: capture one window and, **only if the
    /// channel carries energy**, run the receiver-led decode and return the
    /// decoded payload plus the ACK frame to transmit. Returns `Ok(None)` on an
    /// idle window so the caller never keys PTT to ACK silence.
    ///
    /// Unlike [`respond_arq_ota`](Self::respond_arq_ota) this does **not** transmit
    /// the ACK: a half-duplex caller keys PTT around
    /// `transmit_ack_with_short_fec(&result.ack)` so the radio receives with PTT
    /// released and only keys to answer. The idle gate uses the immediate-window
    /// RMS (not the held DCD busy flag) so the trailing DCD hold after a burst does
    /// not trigger a spurious ACK on silence.
    pub fn poll_ota_rx(
        &mut self,
        session_id: &str,
        device: Option<&str>,
    ) -> Result<Option<OtaRxResult>, ModemError> {
        let samples = self.stage_capture_input(None, device)?;
        let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;

        if samples.samples.is_empty() || self.dcd.energy() < self.dcd.threshold() {
            return Ok(None);
        }

        // `None` fallback mode — see `respond_arq_ota`; this entry has no production caller either.
        let (decoded, ack, last_err) = self.ota_decode_and_ack(&samples, session_id, None)?;
        let (payload, mode) = match decoded {
            Some((p, m)) => (Some(p), Some(m)),
            None => {
                if let Some(e) = &last_err {
                    debug!("poll_ota_rx: energetic window failed to decode: {e}");
                }
                (None, None)
            }
        };
        if let Some(p) = &payload {
            let _ = self.event_tx.send(EngineEvent::FrameReceived {
                mode: mode.clone().unwrap_or_default(),
                bytes: p.len(),
            });
        }
        Ok(Some(OtaRxResult { payload, ack, mode }))
    }

    /// Update DCD from a captured window at the InputCapture seam, emitting a `DcdChange` event on a
    /// flip. Called on the PRE-AGC (post-notch) samples so the carrier detector measures the true channel
    /// level: an enabled AGC that has boosted its gain must not push sub-squelch noise over the busy
    /// threshold (that self-sustaining "held gain × noise → busy forever" deadlock wedged CSMA TX).
    fn update_dcd_at_seam(&mut self, samples: &[f32]) {
        self.dcd_blocks_processed = self.dcd_blocks_processed.wrapping_add(1);
        // Re-aim the squelch at the band we are actually on, BEFORE judging this block.
        //
        // `DcdState` shipped with a fixed 0.01 RMS threshold, and a real band floor walks straight
        // over a constant: the recorded IC-9700 idle capture measures 0.126 RMS, twelve times it. On
        // that band the daemon's carrier detect reads permanently busy, the burst never ends on a
        // carrier drop, and only the runaway cap flushes it — so the decoder is handed a bufferful
        // of noise and a real frame never arrives at all. This is the *daemon's* half of the same
        // hot-floor failure the scanning receive path hit five times; none of that path's machinery
        // (`EnergyGate`, the settle, the #1049 veto) runs here.
        //
        // The floor comes from the passband spectral distribution, not from block energies, because
        // a carrier that stays on raises every block and drags a time-domain percentile up with it —
        // exactly how `EnergyGate` saturates. Re-measured on the real capture after #1254, at the
        // daemon's own read size and with the frame SUPERIMPOSED on the noise (the fixture used to
        // append it, which put digital silence in the noise bins under the frame and collapsed the
        // floor instead of testing it): the spectral floor reads **0.1289 RMS on idle and 0.1343
        // with a frame present** (+4 %), while the total level goes **0.126 → 0.2255** (+79 %).
        // That immovability is the whole point. The older figures here (0.138/0.140) were the EMA's
        // output when it stepped once per multi-window read; it now steps per window.
        //
        // The premise is bounded, and #1304 is where it fails: a signal filling most of the
        // analysed 300–2700 Hz band is not a narrowband carrier, and the estimator recovers ITS
        // mean square. OFDM52/SCFDMA52 at 2031 Hz are exactly that.
        //
        // Deliberately mode-independent. A noise floor is a property of the band, not the waveform,
        // and this sits at the single shared `InputCapture` seam so every receive path gets it.
        if let Some(floor_rms) = self
            .noise_floor
            .update(samples, AudioConfig::default().sample_rate as f32)
            .map(|m| m.sqrt())
        {
            self.dcd
                .set_threshold((floor_rms * DCD_SQUELCH_MARGIN).max(DCD_MIN_SQUELCH_THRESHOLD));
        }
        let prev_busy = self.dcd.is_busy();
        self.dcd.update(samples);
        if self.dcd.is_busy() != prev_busy {
            let _ = self.event_tx.send(EngineEvent::DcdChange {
                busy: self.dcd.is_busy(),
                energy: self.dcd.energy(),
            });
        }
    }

    /// Tick-based burst capture for a free-running daemon over a streaming audio
    /// backend: accumulate captured samples while a carrier is present and return
    /// the **whole burst** once the carrier drops (or a safety cap is hit), else
    /// `None` while still accumulating or idle.
    ///
    /// On a real (cpal) backend a single frame spans many short receive-tick
    /// windows; decoding one partial window can't acquire the frame. Buffering the
    /// burst and decoding it as one unit — via [`decode_burst`](Self::decode_burst)
    /// or [`ota_decode_burst`](Self::ota_decode_burst) — fixes that. The
    /// in-process loopback delivers a frame atomically, so it flushes on the next
    /// (quiet) tick. Carrier presence is the per-window RMS vs the DCD squelch.
    pub fn capture_burst(
        &mut self,
        device: Option<&str>,
    ) -> Result<Option<AudioSamples>, ModemError> {
        let samples = self.stage_capture_input(None, device)?;
        self.accumulate_routed(samples)
    }

    /// Open a capture stream on `device` (or the engine's default device) for a
    /// caller that will own it across receive ticks and feed each `read()` to
    /// [`accumulate_capture`](Self::accumulate_capture). Returning the stream keeps
    /// it on the caller's thread — required for a streaming (cpal) backend, whose
    /// callback only fills its buffer while the stream is held open. A
    /// [`LoopbackBackend`](openpulse_audio::LoopbackBackend) stream clones the same
    /// shared buffers, so this is equivalent to per-tick reopen there.
    pub fn open_capture_stream(
        &self,
        device: Option<&str>,
    ) -> Result<Box<dyn AudioInputStream>, ModemError> {
        let audio_cfg = AudioConfig::default();
        self.audio
            .open_input(device.or(self.default_device.as_deref()), &audio_cfg)
            .map_err(|e| ModemError::Audio(e.to_string()))
    }

    /// Runaway cap for the burst accumulator, in samples, derived from `mode`'s real frame geometry.
    ///
    /// A flat cap cannot serve a ladder spanning 31 to 9600 baud: what bounds runaway growth for
    /// BPSK250 (74 624-sample frames) truncates BPSK31 (596 992) mid-frame. Scaling the mode's own
    /// `max_frame_samples` by the worst FEC expansion keeps the guard's purpose while making it
    /// impossible for it to fire on a legitimate frame. An unknown or unregistered mode falls back to
    /// [`BURST_MIN_CAP_SAMPLES`], which is also the floor for fast modes.
    pub fn burst_cap_samples(&self, mode: Option<&str>) -> usize {
        let raw = mode
            .and_then(|m| {
                let cfg = ModulationConfig {
                    mode: m.to_string(),
                    ..ModulationConfig::default()
                };
                self.plugins.get(m).and_then(|p| p.frame_geometry(&cfg))
            })
            .map_or(0, |g| g.max_frame_samples);
        raw.saturating_mul(MAX_FEC_SLICE_FACTOR)
            .clamp(BURST_MIN_CAP_SAMPLES, BURST_MAX_CAP_SAMPLES)
    }

    /// Burst cap for what this receiver may actually be sent, not just for the mode it is configured
    /// with (#1249).
    ///
    /// `rx_mode` is the operator's configured mode; under an OTA session the peer transmits at
    /// whatever rung the ladder selected, which is a different and often much slower mode. Sizing the
    /// cap from `rx_mode` alone force-flushed every `hpx_hf` entry-rung frame mid-frame on the default
    /// config (BPSK250's cap is 37.3 s; an SL2 BPSK31+Rs frame is 66.6 s, or 131.8 s at two RS
    /// blocks).
    ///
    /// The bound is the **OTA candidate set**, not the whole profile: `rx_candidates` is what
    /// `ota_decode_and_ack_inner` actually attempts (at most two rungs, plus the uncoded fallback,
    /// which is `rx_mode`), so a frame at any other rung is discarded however long it was held.
    /// Sizing from the profile would reserve buffer for frames the decode arm throws away by
    /// construction. It also narrows as the link climbs, and `ota_lock_level` / `ota_set_level_bounds`
    /// pin both candidate levels, so a locked session gets a correspondingly tighter cap for free.
    ///
    /// Deliberately does NOT set `rx_mode` from the rung: that field also aims the receiver notch's
    /// protected band, where a wrong mode makes an interferer un-notchable.
    fn active_burst_cap_samples(&self) -> usize {
        let base = self.burst_cap_samples(self.rx_mode.as_deref());
        // A relay consumer reads the SAME bursts this accumulator flushes (#1308), so the cap must
        // cover its rung too — otherwise a repeater configured slower than `rx_mode` has every frame
        // it exists to forward force-flushed mid-frame. Same shape as the OTA candidates below, and
        // the same defect class as #1249: the cap is sized from what may ARRIVE, never from the mode
        // this station happens to be configured with.
        let base = self
            .relay_mode
            .as_deref()
            .map(|m| base.max(self.burst_cap_samples(Some(m))))
            .unwrap_or(base);
        match self.ota.as_ref() {
            Some(ota) => ota
                .rx_candidates()
                .into_iter()
                .map(|(_, mode, _)| self.burst_cap_samples(Some(mode)))
                .fold(base, usize::max),
            None => base,
        }
    }

    /// Declare a relay consumer's mode, so the burst cap covers what IT must receive (#1308).
    ///
    /// `None` clears it. Set while a cross-band repeater is enabled: the repeater reads the bursts
    /// this engine flushes rather than capturing its own audio, so a cap sized without its rung would
    /// truncate exactly the frames it exists to forward.
    pub fn set_relay_mode(&mut self, mode: Option<String>) {
        self.relay_mode = mode;
    }

    /// Burst-accumulate samples the CALLER already captured from a persistent input
    /// stream, returning a complete burst when the carrier drops (same contract as
    /// [`capture_burst`](Self::capture_burst)).
    ///
    /// cpal is a callback backend whose stream needs tens of ms to start delivering
    /// after `play()`; reopening it every tick (as `capture_burst` does) never warms
    /// up on real hardware, so the buffer stays empty and no carrier is ever seen. A
    /// tick-based caller on a real audio backend should instead open one input
    /// stream, keep it open, and feed each `read()` here — the daemon receive loop
    /// does this. Records the spectrum/waterfall tap from these samples.
    pub fn accumulate_capture(
        &mut self,
        mode: Option<&str>,
        samples: Vec<f32>,
    ) -> Result<Option<AudioSamples>, ModemError> {
        self.record_audio(&samples); // RX window (raw channel audio) for the spectrum/waterfall tap
                                     // The notch is applied once, at the single `PipelineStage::InputCapture` seam in
                                     // `route_audio_stage` (reached via `accumulate_routed` below); just record the mode here.
        self.rx_mode = mode.map(|m| m.to_string());
        self.accumulate_routed(AudioSamples { samples })
    }

    /// Shared burst gather/flush over already-captured samples: route the input
    /// pipeline + DCD, accumulate while the carrier is present (per-window RMS vs the
    /// DCD squelch), and flush the whole burst when it drops or the cap is hit.
    fn accumulate_routed(
        &mut self,
        samples: AudioSamples,
    ) -> Result<Option<AudioSamples>, ModemError> {
        // DCD is updated inside the seam on the PRE-AGC level; gate burst accumulation on that
        // true-channel energy, not the AGC-boosted sample RMS (which would latch a boosted noise floor
        // as a permanent "carrier present").
        // Whether the adaptive floor existed BEFORE this block was judged (#1254). The seam warms the
        // tracker and re-aims the squelch inside `route_audio_stage`, so this must be read first.
        let was_cold = self.noise_floor.mean_sq().is_none();
        let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;
        let carrier_present =
            !samples.samples.is_empty() && self.dcd.energy() >= self.dcd.threshold();

        if carrier_present {
            // Carrier present: keep accumulating this burst.
            self.rx_burst.extend_from_slice(&samples.samples);
            self.rx_capturing = true;
            // Sized by what may ARRIVE (configured mode ∪ the OTA candidate rungs), not by the
            // configured mode alone — see `active_burst_cap_samples` (#1249).
            if self.rx_burst.len() >= self.active_burst_cap_samples() {
                self.rx_capturing = false;
                // The carrier is STILL PRESENT — this slab is not one transmission (#1255).
                self.last_flush_capped = true;
                return Ok(Some(AudioSamples {
                    samples: std::mem::take(&mut self.rx_burst),
                }));
            }
            Ok(None)
        } else if self.rx_capturing && !self.rx_burst.is_empty() {
            self.rx_capturing = false;
            // COLD START (#1254): until the tracker has its first observation the squelch is still
            // the fixed default, so on a band whose floor sits above it every read before the first
            // full window reads as "carrier". Flushing that is not a harmless short burst: with
            // `ota_enabled` the failed decode drives `on_rx_frame(Failed)` AND returns an ACK frame,
            // so the daemon KEYS THE TRANSMITTER and radiates a NACK on pure noise at every start —
            // the #1178 class, spent RF, on exactly the hot bands REQ-DCD-01 exists for.
            //
            // Discard rather than suppress: suppressing carrier-detect while cold would drop the
            // samples themselves, and on a quiet backend (loopback, the twin rig) the tracker is
            // cold when a frame's first read lands — that would eat a third of a BPSK250 preamble.
            // Here nothing is lost, because a burst is only discarded when the FIRST warm judgement
            // says the carrier is absent; if it says present, the cold samples stay in the burst.
            // A tracker that can never warm leaves this branch exactly as it was.
            if was_cold && self.noise_floor.mean_sq().is_some() {
                self.rx_burst.clear();
                return Ok(None);
            }
            // Carrier dropped after a burst → the frame is complete; flush it.
            self.last_flush_capped = false;
            Ok(Some(AudioSamples {
                samples: std::mem::take(&mut self.rx_burst),
            }))
        } else {
            Ok(None)
        }
    }

    /// Onset-scan bounds for a gathered burst: `(step, scan_end, max_frame_samples)`.
    ///
    /// ONE definition, used by BOTH daemon decode arms. They had diverged — the uncoded arm scanned
    /// and the coded arm did not — which cost every RS-coded frame on the on-air corpus (#1138). A
    /// shared helper is the point: the geometry cannot drift apart again without both arms moving.
    ///
    /// The `4 x acquisition window` bound is inherited from the uncoded arm rather than re-derived.
    /// Note it is not obviously the right bound: burst lead-in is set by the DCD tick and hold
    /// window, not by acquisition geometry, and on the #1021 capture it cleared a 4032-sample
    /// lead-in by only 64 samples. Widening it is a separate question that affects both arms and
    /// wants its own measurement — this change deliberately does not answer it.
    fn burst_onset_scan_bounds(&self, mode: &str, n: usize) -> (usize, usize, usize) {
        let (step, acq_samples, min_frame_samples, max_frame_samples) =
            self.frame_scan_geometry(mode, AudioConfig::default().sample_rate);
        let scan_end = n
            .saturating_sub(min_frame_samples)
            .min(acq_samples.saturating_mul(4));
        (step.max(1), scan_end, max_frame_samples)
    }

    /// Onset-scan geometry for `mode`: (scan step, acquisition window, min frame
    /// samples, max frame samples). Prefers the plugin's `frame_geometry`; falls back
    /// to trailing-digit baud with a 32-symbol preamble for unregistered plugins.
    fn frame_scan_geometry(&self, mode: &str, sample_rate: u32) -> (usize, usize, usize, usize) {
        let geometry = self.plugins.get(mode).and_then(|p| {
            p.frame_geometry(&ModulationConfig {
                mode: mode.to_string(),
                sample_rate,
                ..ModulationConfig::default()
            })
        });
        match geometry {
            Some(g) => (
                g.symbol_period_samples.max(1),
                g.preamble_samples.max(g.symbol_period_samples).max(1),
                g.min_frame_samples.max(1),
                g.max_frame_samples.max(g.min_frame_samples),
            ),
            None => {
                let step = {
                    let baud: u32 = mode
                        .trim_end_matches("-RRC")
                        .bytes()
                        .rev()
                        .take_while(|b| b.is_ascii_digit())
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .fold(0u32, |acc, b| acc * 10 + (b - b'0') as u32);
                    if baud > 0 {
                        (sample_rate as f32 / baud as f32).round() as usize
                    } else {
                        32
                    }
                };
                (step, step * 32, step * 33, step * 2280)
            }
        }
    }

    /// Decode a captured burst (from [`capture_burst`](Self::capture_burst) or
    /// [`accumulate_capture`](Self::accumulate_capture)) as a data frame in `mode`.
    ///
    /// A DCD-detected burst is not sample-accurate, and the engine's single-window
    /// demod settles AFC on the window start (DSP playbook §3), so decoding only from
    /// sample 0 usually misframes. Scan onset offsets across the captured lead-in —
    /// reusing the same `decode_attempt` + frame geometry as the live timeout
    /// receiver — and return the first frame that validates.
    pub fn decode_burst(
        &mut self,
        mode: &str,
        burst: &AudioSamples,
    ) -> Result<Vec<u8>, ModemError> {
        // The burst was already front-end-processed by `accumulate_routed`; suppress the InputCapture
        // seam for the per-slice decode below so the AGC/DCD are not re-applied per scan slice (audit
        // #11). Restore the flag on every exit.
        let was_prerouted = self.input_prerouted;
        self.input_prerouted = true;
        // Suppress per-attempt AfcUpdate: the scan rolls back `afc_correction_hz` after every failed
        // attempt, so those events narrate hypotheses, not state (see `suppress_afc_events`).
        let was_quiet = self.suppress_afc_events;
        self.suppress_afc_events = true;
        let result = self.decode_burst_inner(mode, burst);
        self.input_prerouted = was_prerouted;
        self.suppress_afc_events = was_quiet;
        // A successful scan's correction IS committed — emit exactly one for it.
        if result.is_ok() {
            self.emit_afc_update(mode);
        }
        result
    }

    /// [`decode_burst`](Self::decode_burst) without its acquisition pass — the onset scan only.
    ///
    /// For callers that own a phase-2 pass of their own and must not pay for a second one. The OTA
    /// arm is the only such caller today (#1118).
    fn decode_burst_phase1(
        &mut self,
        mode: &str,
        burst: &AudioSamples,
    ) -> Result<Vec<u8>, ModemError> {
        let was_prerouted = self.input_prerouted;
        self.input_prerouted = true;
        let was_quiet = self.suppress_afc_events;
        self.suppress_afc_events = true;
        let sr = AudioConfig::default().sample_rate;
        let (_, _, min_frame_samples, max_frame_samples) = self.frame_scan_geometry(mode, sr);
        let n = burst.samples.len();
        let result = if n < min_frame_samples {
            self.receive_from_samples(
                mode,
                AudioSamples {
                    samples: burst.samples.clone(),
                },
            )
        } else {
            let (step, scan_end, _) = self.burst_onset_scan_bounds(mode, n);
            self.scan_burst_onsets(
                mode,
                &burst.samples,
                step,
                scan_end,
                max_frame_samples,
                false,
            )
        };
        self.input_prerouted = was_prerouted;
        self.suppress_afc_events = was_quiet;
        result
    }

    fn decode_burst_inner(
        &mut self,
        mode: &str,
        burst: &AudioSamples,
    ) -> Result<Vec<u8>, ModemError> {
        let sr = AudioConfig::default().sample_rate;
        let (_, _, min_frame_samples, max_frame_samples) = self.frame_scan_geometry(mode, sr);
        let n = burst.samples.len();
        if n < min_frame_samples {
            // Too short to hold a frame: one direct attempt for the error/SNR path.
            return self.receive_from_samples(
                mode,
                AudioSamples {
                    samples: burst.samples.clone(),
                },
            );
        }
        // The carrier onset sits within the captured lead-in; scan up to a few acquisition windows
        // past sample 0 (bounded so a noise burst can't spin). Shared with the CODED arm via
        // `burst_onset_scan_bounds` so the two cannot diverge again (#1138).
        let (step, scan_end, _) = self.burst_onset_scan_bounds(mode, n);
        // PHASE 1 — today's path exactly: every onset at the current correction. Bit-identical to
        // the behaviour before #1118, which is the whole reason the two-phase shape was chosen: a
        // frame that decodes today still decodes here, and phase 2 cannot regress it.
        match self.scan_burst_onsets(
            mode,
            &burst.samples,
            step,
            scan_end,
            max_frame_samples,
            false,
        ) {
            Ok(payload) => Ok(payload),
            Err(phase1_err) => {
                // PHASE 2 — acquire the carrier, then retry (#1118, REQ-PHY-03). Reached only when
                // every onset failed at the current correction, which is the evidence that the
                // burst may be off frequency. Measured: the daemon decodes 0 Hz and 20 Hz offsets
                // without this and fails from 50 Hz up, while the same path handed a correct centre
                // frequency decodes to 400 Hz.
                //
                // Phase 1's error is the one returned: it is the failure a caller expects, and
                // phase 2's is an artefact of a rescue attempt that was never going to be reached
                // on a healthy link.
                match self.acquire_burst_correction(mode, &burst.samples, step, scan_end) {
                    Some(correction) => {
                        self.afc_correction_hz = correction;
                        let out = self.scan_burst_onsets(
                            mode,
                            &burst.samples,
                            step,
                            scan_end,
                            max_frame_samples,
                            false,
                        );
                        if out.is_err() {
                            self.afc_correction_hz = 0.0;
                        }
                        out.map_err(|_| phase1_err)
                    }
                    None => Err(phase1_err),
                }
            }
        }
    }

    /// Estimate one carrier correction for a whole burst, at bounded cost (#1118).
    ///
    /// **Why this is not "settle at every onset and retry there".** That was the first
    /// implementation, and the workspace gate found what the design review had predicted: on a
    /// fading fixture almost every burst fails phase 1, so phase 2 ran on all of them, at up to
    /// ~129 onsets each with a decode retry apiece. `ota_channel_adaptation`'s Watterson test went
    /// from minutes to **over 98 minutes without finishing**.
    ///
    /// A count cap alone cannot fix that: the real #1021 on-air lead-in is 4032 samples — 126 symbol
    /// periods at BPSK250 — so a cap small enough to bound the cost is also small enough to put the
    /// frame out of reach. The span has to stay; what changes is the density and what is done with
    /// the result.
    ///
    /// * **settle on a COARSE grid** (`PHASE2_STEP_MULTIPLIER × step`) across the same span, so the
    ///   cost is a bounded fraction of a scan rather than a multiple of one;
    /// * take the **median** of the corrections that pass the guards. Settles that land on the frame
    ///   agree near the true offset; settles on noise scatter, and a median is what a scattered
    ///   minority cannot move — the same robustness argument the #1060 calibration rests on;
    /// * hand that one correction to a single ordinary fine scan, which is where onset precision is
    ///   actually needed.
    ///
    /// Worst case is therefore `span/coarse_step` settles plus **one** extra fine scan — the ~2×
    /// bound the review asked for — instead of a settle and a decode at every onset.
    fn acquire_burst_correction(
        &mut self,
        mode: &str,
        samples: &[f32],
        step: usize,
        scan_end: usize,
    ) -> Option<f32> {
        let sr = AudioConfig::default().sample_rate;
        let (_, acq_samples, min_frame_samples, _) = self.frame_scan_geometry(mode, sr);
        let afc_window = acq_samples.max(min_frame_samples);
        let veto = self.build_preamble_veto(mode, sr);
        let coarse = step.saturating_mul(PHASE2_STEP_MULTIPLIER).max(1);
        let entry = self.afc_correction_hz;
        let mut corrections: Vec<f32> = Vec::new();
        let mut start = 0usize;
        loop {
            if self.acquire_at_onset(mode, samples, start, afc_window, veto.as_ref()) {
                corrections.push(self.afc_correction_hz);
            }
            self.afc_correction_hz = entry;
            if start >= scan_end {
                break;
            }
            start = (start + coarse).min(scan_end);
        }
        if corrections.is_empty() {
            return None;
        }
        corrections.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(corrections[corrections.len() / 2])
    }

    /// Acquire the carrier at `start`, and report whether a decode retry there is worth its cost.
    ///
    /// ONE definition, called by both daemon decode arms (#1118). They have diverged once already —
    /// #1138 cost every RS-coded frame on the on-air corpus because the uncoded arm scanned onsets
    /// and the coded arm did not — so the acquisition decision lives in a single place by
    /// construction rather than by intention.
    ///
    /// Returns `false` (and leaves the correction for the caller to roll back) when:
    /// * the onset's settle window is not fully inside the burst;
    /// * the settle fails the CLI path's own two guards — converged (`last_delta < 5`,
    ///   `|fine − anchor| ≤ 20`) and plausible (`|fine| ≤ AFC_MAX_CORRECTION_HZ`);
    /// * the correction lands inside `AFC_SETTLE_DEADBAND_HZ`, i.e. it is not a correction at all —
    ///   phase 1 already tried this onset at ~0 and failed, so a retry would buy nothing;
    /// * a published preamble template says the window is not this waveform (#1049), unless the
    ///   #1060 calibration has stood the veto down.
    fn acquire_at_onset(
        &mut self,
        mode: &str,
        samples: &[f32],
        start: usize,
        afc_window: usize,
        veto: Option<&PreambleVeto>,
    ) -> bool {
        let n = samples.len();
        let settle_end = (start + afc_window).min(n);
        if settle_end.saturating_sub(start) < afc_window {
            return false;
        }
        let outcome = self.afc_mini_settle(mode, &samples[start..settle_end]);
        let converged = outcome.last_delta < 5.0 && (outcome.fine - outcome.anchor).abs() <= 20.0;
        let plausible = outcome.fine.abs() <= AFC_MAX_CORRECTION_HZ;
        let worth_retrying = outcome.fine.abs() >= AFC_SETTLE_DEADBAND_HZ;
        if !(converged && plausible && worth_retrying) {
            return false;
        }
        let Some(v) = veto else {
            return true;
        };
        let Some((rho, _)) = self.preamble_rho(v, &samples[start..settle_end], outcome.fine) else {
            return true;
        };
        // Decide and record through the one step the CLI path uses too (#1342): the calibration
        // sample, the stand-down latch and its log line, and all three counters. Without the counters
        // the daemon's veto runs unobservably — the half-wiring the #1118 seam gate exists for — and
        // without the latch a stand-down here was neither reported nor logged.
        self.decide_preamble_veto(v, rho, start).accepted()
    }

    /// One onset scan over a gathered burst, optionally acquiring the carrier at each onset.
    ///
    /// `settle = false` is the pre-#1118 scan. `settle = true` runs `afc_mini_settle` at each onset
    /// under the same two guards the CLI path applies, and skips the decode retry where the settle
    /// is not worth spending one on. Rollback discipline is the same in both: `afc_correction_hz` is
    /// restored on every failed attempt and committed only on success (#1143).
    #[allow(clippy::too_many_arguments)]
    fn scan_burst_onsets(
        &mut self,
        mode: &str,
        samples: &[f32],
        step: usize,
        scan_end: usize,
        max_frame_samples: usize,
        settle: bool,
    ) -> Result<Vec<u8>, ModemError> {
        let n = samples.len();
        let sr = AudioConfig::default().sample_rate;
        let (_, acq_samples, min_frame_samples, _) = self.frame_scan_geometry(mode, sr);
        let afc_window = acq_samples.max(min_frame_samples);
        let veto = if settle {
            self.build_preamble_veto(mode, sr)
        } else {
            None
        };
        let mut start = 0usize;
        let mut last_err = ModemError::Demodulation("no onset attempted".into());
        loop {
            let end = (start + max_frame_samples).min(n);
            let afc_before = self.afc_correction_hz;
            let mut skip = false;
            if settle {
                skip = !self.acquire_at_onset(mode, samples, start, afc_window, veto.as_ref());
                if skip {
                    self.afc_correction_hz = afc_before;
                }
            }
            if !skip {
                let slice = samples[start..end].to_vec();
                match self.decode_attempt(
                    mode,
                    AudioSamples {
                        samples: slice.clone(),
                    },
                    FecMode::None,
                ) {
                    Ok(payload) => {
                        // E1 skipped the estimate on every attempt; the WINNING slice still needs
                        // one, so the single `AfcUpdate` the wrapper emits carries a real
                        // correction.
                        self.update_afc_estimate(mode, &slice);
                        return Ok(payload);
                    }
                    Err(e) => {
                        self.afc_correction_hz = afc_before; // undo the failed attempt's AFC drift
                        last_err = e;
                    }
                }
            }
            if start >= scan_end {
                return Err(last_err);
            }
            start = (start + step).min(scan_end);
        }
    }

    /// Decode a captured burst with the OTA candidate fallback and build the ACK to
    /// send back (does not transmit it) — the burst-input counterpart of
    /// [`poll_ota_rx`](Self::poll_ota_rx) for a daemon using
    /// [`capture_burst`](Self::capture_burst).
    ///
    /// `fallback_mode` is the mode this station is *operating* on — the one its own non-ladder
    /// traffic (station ID, filexfer fragments, handshake, QSY, relay) is transmitted at. A burst
    /// that no rung candidate decodes is retried uncoded at that mode; on success the OTA controller
    /// is left untouched and the returned `ack` is `None`, because such a frame is not ladder
    /// traffic. Pass `None` to disable the fallback entirely (#1123).
    pub fn ota_decode_burst(
        &mut self,
        burst: &AudioSamples,
        session_id: &str,
        fallback_mode: Option<&str>,
    ) -> Result<OtaRxResult, ModemError> {
        let (decoded, ack, last_err) = self.ota_decode_and_ack(burst, session_id, fallback_mode)?;
        let (payload, mode) = match decoded {
            Some((p, m)) => (Some(p), Some(m)),
            None => {
                if let Some(e) = &last_err {
                    debug!("ota_decode_burst: burst failed to decode: {e}");
                }
                (None, None)
            }
        };
        // `FrameReceived` is already emitted by the inner `decode_attempt` → `receive_from_samples`
        // on a successful decode; emitting again here double-counted it on the OTA path only.
        Ok(OtaRxResult { payload, ack, mode })
    }

    /// Shared OTA receive core: run the candidate-fallback decode on an already
    /// captured window, update the receiver-led controller, and build the ACK frame
    /// to send back. Captures and transmits nothing — callers own those, so the
    /// daemon can key PTT only around the ACK transmit. Returns the decoded
    /// payload+mode (if any), the ACK frame, and the last decode error.
    fn ota_decode_and_ack(
        &mut self,
        samples: &AudioSamples,
        session_id: &str,
        fallback_mode: Option<&str>,
    ) -> Result<OtaDecodeOutcome, ModemError> {
        // Every caller (`respond_arq_ota`, `poll_ota_rx`, `ota_decode_burst`) front-ends the burst at the
        // shared `route_audio_stage(InputCapture)` seam BEFORE calling. Suppress the seam for the
        // per-candidate + soft-HARQ decode loops below so the stateful notch-persistence counter and the
        // streaming AGC don't advance once PER candidate — which wastes DSP passes and can prematurely trip
        // the auto-QSY-on-interference path. This is the same guard `decode_burst` applies to
        // `decode_burst_inner`. Restore on every exit.
        let was_prerouted = self.input_prerouted;
        self.input_prerouted = true;
        // Same rollback property as `decode_burst`: every candidate and every HARQ trial restores
        // `afc_correction_hz` on failure.
        let was_quiet = self.suppress_afc_events;
        self.suppress_afc_events = true;
        let result = self.ota_decode_and_ack_inner(samples, session_id, fallback_mode);
        self.input_prerouted = was_prerouted;
        self.suppress_afc_events = was_quiet;
        if let Ok((Some((_, mode)), _, _)) = &result {
            let mode = mode.clone();
            self.emit_afc_update(&mode);
        }
        result
    }

    fn ota_decode_and_ack_inner(
        &mut self,
        samples: &AudioSamples,
        session_id: &str,
        fallback_mode: Option<&str>,
    ) -> Result<OtaDecodeOutcome, ModemError> {
        let candidates: Vec<(SpeedLevel, String, FecMode)> = self
            .ota
            .as_ref()
            .ok_or_else(|| ModemError::Configuration("no OTA session active".into()))?
            .rx_candidates()
            .into_iter()
            .map(|(l, m, f)| (l, m.to_string(), f))
            .collect();
        // NO DUPLICATE RsStrong CANDIDATES. The sender opportunistically strengthens Rs → RsStrong
        // when it costs no extra block (`free_rs_strengthening`), and this used to append a mirror
        // RsStrong candidate for every Rs one so the receiver could try both.
        //
        // That is now dead work: the `Rs` candidate already covers it EVERYWHERE in this function.
        // The standalone arm decodes via `rs_decode_prefix_free_strengthened`, which tries
        // `FecCodec::strong()` after plain Rs; `decode_combined_llrs` is free-strengthened too; and
        // the HARQ soft filter never listed `RsStrong` in the first place. The duplicates were the
        // mechanism at #941 and became a strict subset when the free-strengthened arm landed —
        // nobody removed them.
        //
        // The comment they carried said "one extra ~µs RS decode". Measured on a real filexfer
        // burst, that was wrong by five orders of magnitude: each duplicate costs a full-slice
        // DEMOD, not an RS decode, and they were HALF the candidate loop's 116 s. `FecCodec::strong`
        // reaches the same wire bytes either way, so no frame that decoded before stops decoding.
        // Gate: `free_rs_strengthening_ota`.

        // AFC accumulates across calls, so a failed wrong-mode candidate would
        // poison the correct candidate's correction. Isolate each attempt: reset to
        // the pre-frame AFC before every try, keeping only the successful update.
        // Each candidate carries its own MODCOD FEC, applied via decode_attempt.
        let afc_before = self.afc_correction_hz;
        let mut decoded: Option<(Vec<u8>, SpeedLevel, String)> = None;
        // The [start, end) span that actually decoded, for the SNR measurement (#1142). Set at
        // EVERY site that sets `decoded` — a site that decodes without recording its span would
        // silently fall back to abstention and cost the ladder its SNR climb on that path.
        let mut decoded_span: Option<(usize, usize)> = None;
        let mut last_err: Option<ModemError> = None;
        // Offset 0 only, exactly as before #1138. The onset SCAN is deliberately NOT here — it
        // runs after the uncoded fallback below, so its cost lands only on bursts nothing else
        // could decode. See the scan block for why that ordering is the additive one.
        for (level, mode, fec) in &candidates {
            self.afc_correction_hz = afc_before;
            let slice = AudioSamples {
                samples: samples.samples.clone(),
            };
            match self.decode_attempt(mode, slice, *fec) {
                Ok(payload) => {
                    // Offset 0 decoded, so the frame genuinely starts there and the whole burst is
                    // the right window — this is the one path where they coincide.
                    decoded_span = Some((0, samples.samples.len()));
                    decoded = Some((payload, *level, mode.clone()));
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }

        // Uncoded fallback for NON-LADDER traffic (#1123).
        //
        // The rung candidates above are the only thing this arm used to try, and every `hpx_*`
        // profile that populates a FEC table codes every rung — so an uncoded frame matched nothing
        // and the daemon simply could not receive its own station ID, filexfer fragments, handshake
        // CONREQ/CONACK, QSY frames or relay envelopes whenever an OTA session was active. Those go
        // out via `transmit`, at the station's ACTIVE mode, which is what `fallback_mode` carries.
        //
        // Placed AFTER the standalone candidates and BEFORE the HARQ block below, and the ordering
        // is load-bearing in both directions:
        //   * before HARQ, because the HARQ loop RETAINS this burst's LLRs per soft candidate on
        //     failure. Running the fallback afterwards would retain one garbage vector for every
        //     control frame, self-inflicting exactly the stale-LLR contamination the widest-first
        //     suffix trial exists to contain — and it would need an "un-retain" mechanism to undo.
        //     Returning here means nothing was ever retained and there is nothing to undo.
        //   * after the candidates, so a ladder frame always keeps first claim on the burst. Under a
        //     profile with no FEC table (`fec_for` is `unwrap_or(FecMode::None)`) the rung candidates
        //     are themselves uncoded, and if such a rung's mode equals the active mode the two frame
        //     classes are indistinguishable on the wire; candidates-first is what keeps those
        //     counting as ladder traffic. Whether such profiles are legal OTA profiles at all is a
        //     separate question (see #1123).
        //
        // Returns EARLY on success, bypassing `decoded`: assigning it would clear the retained LLRs
        // and run the controller update, and a frame that is not ladder traffic must do neither. The
        // `AckFrame` is `None` for the same reason — there is nothing to acknowledge, and the daemon
        // must not key the transmitter for it.
        if decoded.is_none() {
            if let Some(mode) = fallback_mode {
                // Same isolation every candidate gets: a failed attempt's AFC drift must not
                // poison this one.
                self.afc_correction_hz = afc_before;
                // PHASE 1 ONLY here (#1118). `decode_burst` runs its own acquisition pass when its
                // scan fails, and on a coded ladder burst that pass is pure cost: an RS frame will
                // never decode uncoded, at any frequency. Measured before this split: an
                // on-frequency coded burst spent 129 settles inside this fallback and changed no
                // verdict. The fallback mode gets its acquisition pass with every other candidate,
                // in the single phase-2 block below.
                if let Ok(payload) = self.decode_burst_phase1(mode, samples) {
                    debug!(
                        "ota fallback decoded {} bytes of non-ladder traffic at {mode}",
                        payload.len()
                    );
                    return Ok((Some((payload, mode.to_string())), None, last_err));
                }
                self.afc_correction_hz = afc_before;
            }
        }

        // ONSET SCAN (#1138) — the frame need not start at sample 0.
        //
        // `decode_burst_inner`, this arm's uncoded sibling, has always scanned; this arm made one
        // attempt at offset 0 and so could not decode a frame a few thousand samples into a burst —
        // the demodulator's timing search spans a single symbol period (32 samples at BPSK250).
        // Real captures put the frame exactly there: replaying the on-air corpus through both
        // receive paths measured CLI 5/7 versus daemon 1/7, with onsets of 4032 and 224 samples.
        // With this scan the daemon reads 5/7, matching the CLI on every capture.
        //
        // WHY HERE, after the fallback rather than inside the candidate loop. An exhaustive
        // decode-driven search pays its FULL cost exactly when there is nothing to find, because
        // its exit condition is exhaustion — and the bursts with nothing to find are the peer's
        // non-ladder traffic. Measured on a real filexfer fragment: scanning inside the candidate
        // loop cost 116.5 s before the fallback (which then decoded it in 28 ms) and timed out
        // `twin_daemon_bridge::a_file_crosses_the_bridge_with_ota_enabled`. Appending the scan is
        // also the genuinely ADDITIVE change: the pre-#1138 order was candidates@0 → fallback →
        // HARQ, and this adds a stage rather than inserting ~129 attempts ahead of traffic that
        // decodes immediately.
        //
        // Placed BEFORE HARQ for the same reason the fallback is: HARQ retains this burst's LLRs on
        // failure, and a scan running after it would have nothing to undo but would delay retention.
        if decoded.is_none() {
            let n = samples.samples.len();
            'scan: for (level, mode, fec) in &candidates {
                let (step, scan_end, max_frame_samples) = self.burst_onset_scan_bounds(mode, n);
                if scan_end == 0 {
                    continue; // nothing to search: attempt 0 already covered this candidate
                }
                let mut start = step;
                loop {
                    self.afc_correction_hz = afc_before;
                    let end = (start + max_frame_samples).min(n);
                    if start >= end {
                        break;
                    }
                    let slice = samples.samples[start..end].to_vec();
                    match self.decode_attempt(
                        mode,
                        AudioSamples {
                            samples: slice.clone(),
                        },
                        *fec,
                    ) {
                        Ok(payload) => {
                            // E1 skips the estimate on every attempt; the WINNING slice still needs
                            // one so the wrapper's single `AfcUpdate` carries a real correction.
                            self.update_afc_estimate(mode, &slice);
                            // The window that DECODED is the only defensible place to measure the
                            // SNR (#1142) — measuring over the whole gathered burst hands the
                            // estimator's timing search a lead-in it cannot see past.
                            decoded_span = Some((start, end));
                            decoded = Some((payload, *level, mode.clone()));
                            break 'scan;
                        }
                        Err(e) => {
                            last_err = Some(e);
                            if start >= scan_end {
                                break;
                            }
                            start = (start + step).min(scan_end);
                        }
                    }
                }
            }
            self.afc_correction_hz = if decoded.is_some() {
                self.afc_correction_hz
            } else {
                afc_before
            };
        }

        // PHASE 2 — the same candidate scan, acquiring the carrier at each onset (#1118,
        // REQ-PHY-03).
        //
        // Reached only when every candidate failed at every onset at the current correction, which
        // is the evidence that the burst may be OFF FREQUENCY. Measured before this existed: the
        // daemon decodes 0 Hz and 20 Hz offsets and fails from 50 Hz up, while the same path handed
        // a correct centre frequency decodes to 400 Hz — so the missing capability was frequency
        // acquisition alone, and REQ-PHY-03 requires ±50 Hz without operator intervention.
        //
        // Ordered here, after the plain scan and before HARQ, for the same reason the plain scan
        // sits where it does: its cost lands only on bursts nothing cheaper could decode, and HARQ
        // must not retain LLRs for a burst this might still recover.
        //
        // `acquire_at_onset` is shared with the uncoded arm so the two cannot drift apart the way
        // they did in #1138.
        if decoded.is_none() {
            let n = samples.samples.len();
            // The fallback mode rides along as an uncoded candidate: non-ladder traffic (station ID,
            // filexfer, handshake, QSY, relay) is exactly as likely to arrive off frequency as a
            // ladder frame, and #1123 is the record of what happens when this arm forgets it.
            let mut phase2: Vec<(SpeedLevel, String, FecMode)> = candidates.clone();
            if let Some(m) = fallback_mode {
                if !phase2
                    .iter()
                    .any(|(_, cm, cf)| cm == m && *cf == FecMode::None)
                {
                    phase2.push((SpeedLevel::Sl1, m.to_string(), FecMode::None));
                }
            }
            'settle_scan: for (level, mode, fec) in &phase2 {
                let (step, scan_end, max_frame_samples) = self.burst_onset_scan_bounds(mode, n);
                // ONE bounded acquisition for the whole burst, then an ordinary fine scan at the
                // correction it found. Settling at every onset and retrying there is what the
                // workspace gate caught as a 98-minute non-terminating test; see
                // `acquire_burst_correction` for why a count cap could not fix it.
                self.afc_correction_hz = afc_before;
                let Some(correction) =
                    self.acquire_burst_correction(mode, &samples.samples, step, scan_end)
                else {
                    continue;
                };
                let mut start = 0usize;
                loop {
                    self.afc_correction_hz = correction;
                    let end = (start + max_frame_samples).min(n);
                    if start < end {
                        let slice = samples.samples[start..end].to_vec();
                        match self.decode_attempt(
                            mode,
                            AudioSamples {
                                samples: slice.clone(),
                            },
                            *fec,
                        ) {
                            Ok(payload) => {
                                self.update_afc_estimate(mode, &slice);
                                decoded_span = Some((start, end));
                                decoded = Some((payload, *level, mode.clone()));
                                break 'settle_scan;
                            }
                            Err(e) => last_err = Some(e),
                        }
                    }
                    if start >= scan_end {
                        break;
                    }
                    start = (start + step).min(scan_end);
                }
            }
            // Commit only on success — the rollback discipline every other arm follows (#1143).
            self.afc_correction_hz = if decoded.is_some() {
                self.afc_correction_hz
            } else {
                afc_before
            };
        }

        // HARQ soft-combining across OTA retransmissions (additive — runs only when every
        // standalone candidate above failed). For each soft-capable candidate carrying a soft
        // FEC, demodulate the burst to LLRs, MAP-combine them with LLRs retained from earlier
        // failed bursts of the same mode, and retry the soft decode. This is the
        // standalone-then-combine union of #694, now stateful across the daemon's async bursts:
        // the diversity gain (measured 0.43 → 0.67 on `moderate_f1` SCFDMA52-16QAM) only reaches
        // the air here. Retain this burst on continued failure; clear all retained LLRs on any
        // success so a delivered frame's soft info can't bleed into the next one.
        if decoded.is_none() {
            if self.ota_retained_session.as_deref() != Some(session_id) {
                self.ota_retained_llrs.clear();
                self.ota_retained_session = Some(session_id.to_string());
            }
            for (level, mode, fec) in &candidates {
                if decoded.is_some() {
                    break;
                }
                let soft = self
                    .plugins
                    .get(mode)
                    .map(|p| p.supports_soft_demod(mode))
                    .unwrap_or(false)
                    && matches!(
                        fec,
                        FecMode::SoftConcatenated
                            | FecMode::Ldpc
                            | FecMode::LdpcHighRate
                            | FecMode::Rs
                    );
                // `Rs` is admitted alongside the soft-FEC modes: `decode_combined_llrs` handles it by
                // hard-deciding the *combined* vector, so a MAP sum across fades pulls the error count
                // under RS(255,223)'s 16-byte-per-block capacity. This is what puts HARQ on the MFSK16
                // sub-floor rung (SL1), worth ~2.5 dB there — 0.117 → 0.750 at -4 dB on `moderate_f1`,
                // and it decodes at -6 dB where no single burst ever does. It also opens the plain-RS
                // mid-ladder rungs (`hpx_hf` SL6/SL9).
                //
                // MFSK16 was held out until now because every one of its frames is one fixed 255-byte
                // block, so nothing could separate an abandoned message's retained LLRs from a
                // retransmission — worst case, delivering the wrong message. The suffix trial below
                // contains that by construction, and `mfsk16_harq.rs` gates both halves: no dilution
                // and zero false deliveries (audit 2026-07-15 / 2026-07-16 #4).
                if !soft {
                    continue;
                }
                self.afc_correction_hz = afc_before;
                let Ok(llrs) = self.ota_demodulate_soft(mode, &samples.samples) else {
                    continue;
                };
                // Align every retained burst onto this one's LLR grid: truncate if longer, zero-pad
                // if shorter.
                //
                // A faded demod recovers a varying symbol count for the *same* frame — 576, 4096,
                // 4112, 4160, 4168, 4192, 4288, 4320 all observed on one OFDM52-16QAM frame — so an
                // equality filter here silently discarded genuine retransmissions and threw away
                // diversity already paid for in airtime. The variation is *trailing*: same-frame
                // bursts of different lengths agree on 0.817 of their bit signs over the overlap
                // versus 0.811 for equal-length ones, i.e. they share a start and a grid, so index k
                // means the same bit in both.
                //
                // Truncation alone is not enough: a short runt would drag `combine_llrs_map`'s
                // min-length output below the frame and destroy an otherwise-decodable combine, and
                // a runt can be the *newest* vector, where the suffix trial below cannot drop it.
                // Zero-padding is the principled complement — an LLR of 0 is P(0)=P(1)=0.5, exactly
                // "this burst never recovered that symbol", and contributes nothing to the sum.
                let retained: Vec<Vec<f32>> = self
                    .ota_retained_llrs
                    .get(mode)
                    .map(|r| {
                        r.iter()
                            .map(|v| {
                                let mut aligned = vec![0.0f32; llrs.len()];
                                let n = v.len().min(llrs.len());
                                aligned[..n].copy_from_slice(&v[..n]);
                                aligned
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // Try the newest-first suffixes of the retained set, widest first, until one decodes.
                //
                // Summing LLRs is the MAP combine only for repeated observations of the *same* bits;
                // fold in an abandoned message's burst and it corrupts this one. Nothing above
                // separates them: LLR length never did (two same-length messages collide, and every
                // MFSK16 frame is one fixed 255-byte block) and it no longer filters at all, and the
                // session guard never fires on the daemon — it pins `session_id` to the local
                // callsign. No identity test on the LLRs
                // themselves is dependable either: a short payload pads out to the RS block with bytes
                // both messages share, which correlates *different* frames strongly enough to close
                // the margin (measured: sign agreement 0.61-0.73 for different 61-byte messages, vs
                // 0.68-0.86 for genuine retransmissions — overlapping populations).
                //
                // What holds regardless is the *ordering*: a stale message's bursts are always older
                // than this message's, so some suffix excludes all of them. Widest-first keeps the
                // common case at one decode and preserves full diversity when every retained burst
                // really is this frame; a stale vector then costs only the attempts before the suffix
                // that drops it. Success is a superset of the single whole-set combine this replaces
                // — the same standalone-then-combine union #694 established (audit 2026-07-16 #4).
                for take in (1..=retained.len()).rev() {
                    let combined = {
                        let mut set: Vec<&[f32]> = retained[retained.len() - take..]
                            .iter()
                            .map(|v| v.as_slice())
                            .collect();
                        set.push(llrs.as_slice());
                        combine_llrs_map(&set)
                    };
                    if let Ok(payload) = self.decode_combined_llrs(mode, &combined, *fec) {
                        // HARQ demodulates at offset 0 over the whole burst (#1139), so no better
                        // window is known here. Recording the full span keeps the reading no worse
                        // than before this change on the one path that cannot do better; when #1139
                        // gives HARQ an onset, this span should follow it.
                        decoded_span = Some((0, samples.samples.len()));
                        decoded = Some((payload, *level, mode.clone()));
                        break;
                    }
                }
                if decoded.is_some() {
                    break;
                }
                let buf = self.ota_retained_llrs.entry(mode.clone()).or_default();
                buf.push(llrs);
                if buf.len() > OTA_HARQ_MAX_ATTEMPTS {
                    let excess = buf.len() - OTA_HARQ_MAX_ATTEMPTS;
                    buf.drain(0..excess);
                }
            }
            // RESTORE THE AFC ON FAILURE, as every sibling arm does (#1139).
            //
            // `ota_demodulate_soft` ends in `update_afc_estimate`, so a HARQ trial mutates
            // `afc_correction_hz` — and this was the ONLY failure path that never put it back. The
            // candidates reset before each try, the uncoded fallback restores on both exits, and the
            // onset scan restores when it gives up; HARQ leaked. The polluted value then became the
            // NEXT burst's `afc_before`, the baseline every later attempt resets to, so one failed
            // combine degraded every subsequent burst in the session.
            //
            // The estimate is junk precisely here: it is made on an offset-0 slice of a burst that
            // already failed to decode, which on a lead-in burst is misframed by construction.
            // Measured worth +0.025 on BPSK250+Rs at 4 dB — isolated by ablation, where restoring
            // the AFC between bursts reproduced the fresh-engine baseline with ZERO per-trial diffs.
            //
            // Deliberately narrow: only the FAILURE path is restored. Cross-burst AFC tracking on a
            // successful decode is the loop's purpose and is untouched.
            if decoded.is_none() {
                self.afc_correction_hz = afc_before;
            }
        }
        if decoded.is_some() {
            self.ota_retained_llrs.clear();
        }

        // SNR for the receiver decision — measured on the span that DECODED, or not at all (#1142).
        //
        // This used to run over `samples.samples`, the whole gathered burst. `rx_snr_db` locks
        // sub-symbol timing by correlating the buffer's first 32 symbols against the preamble over
        // offsets 0..31; once a lead-in pushes the frame past that ~1056-sample window the
        // correlation sees no preamble at any offset, the lock becomes a noise argmax, and the frame
        // is demodulated at a wrong sub-symbol offset. The reading is then a deterministic function
        // of `(chosen_offset - lead) mod 32` — swept measurement at one operating point: a smooth
        // single-peaked curve from **+5.35 dB to −8.39 dB against a true 5.82 dB**, the same shape
        // for every noise realization and merely ROTATED by it. So the error is deterministic in the
        // misalignment and uniform-random in which misalignment a burst gets.
        //
        // Not a dilution effect: `additive_snr_db_windowed` sums signal and noise power linearly, so
        // the few percent of noise-only symbols a lead-in adds are worth ~0.25 dB at most.
        //
        // When nothing decoded there is no defensible window — the frame's position is exactly what
        // the failed decode could not establish — so the estimate is `None` and the controller falls
        // through to its NACK hysteresis. Do NOT substitute the whole burst here "to have a number":
        // a value that is wrong by a draw cannot be corrected by an offset, and it is precisely what
        // drove FastDownshift to the bottom of the ladder on a single failed decode.
        let snr = self.rx_snr_estimate.or_else(|| {
            let (start, end) = decoded_span?;
            let m = decoded.as_ref().map(|(_, _, m)| m.as_str())?;
            Some(self.rx_snr_db(m, &samples.samples[start..end]))
        });

        // #1255: a cap-flushed slab is not evidence about the rate ladder, in either direction.
        //
        // The cap exceeds the longest candidate frame, so hitting it means the carrier was still up:
        // the slab is a stuck channel or a frame trailed by a long carrier, not one transmission
        // that failed. Feeding `RxOutcome::Failed` here does two things, and only one of them is
        // bounded. The NACK keying is capped by `OTA_NACK_BUDGET` (`server.rs`), but the rate
        // controller's demotion is NOT: three such slabs walk `recommended_level` down and the next
        // real ACK carries it to the peer.
        //
        // Routed into the discrimination #1123 already built rather than a new `BurstEnd` type on
        // the seam: returning `ack: None` puts this in the daemon's existing `ladder_frame == false`
        // branch, which already means "key nothing, leave the budget alone". Zero daemon changes and
        // no signature churn across ~19 test files.
        //
        // The DECODE above still ran, and must: when the squelch sits below the floor EVERY burst is
        // a cap flush (that is #1254's regime), so skipping the decode here would have made the
        // daemon deaf on a hot band. `take()` so the flag is consumed once and a later hand-built
        // burst cannot read a stale `true`.
        if decoded.is_none() && std::mem::take(&mut self.last_flush_capped) {
            tracing::debug!("OTA: cap-flushed burst did not decode; not ladder evidence (#1255)");
            return Ok((None, None, last_err));
        }
        self.last_flush_capped = false;

        let ota = self
            .ota
            .as_mut()
            .ok_or_else(|| ModemError::Configuration("no OTA session active".into()))?;
        // Captured BEFORE the decision so the event can report the transition rather than just the
        // resulting state — `to` alone cannot distinguish a hold from a move that landed here.
        let from_level = ota.rx_recommended_level();
        let decoded_level = decoded.as_ref().map(|(_, level, _)| *level);
        let (rx_ack, decoded) = match decoded {
            Some((payload, level, mode)) => {
                let ack = ota.on_rx_frame(RxOutcome::Decoded(level), snr);
                (ack, Some((payload, mode)))
            }
            None => {
                let ack = ota.on_rx_frame(RxOutcome::Failed, snr);
                (ack, None)
            }
        };
        // Emitted on EVERY decision, including ones that move nothing: a failed decode that leaves
        // the level alone is invisible in the periodic `OtaStatus` snapshot, because that snapshot
        // reports state and the state did not change (#1081). The mutable borrow of `self.ota` ends
        // here, which is why the send is not inside the match above.
        let _ = self.event_tx.send(EngineEvent::OtaRateDecision {
            from: from_level,
            to: rx_ack.recommended_level,
            decoded_level,
            snr_db: snr,
            decision: rx_ack.decision,
        });
        let ack_frame = AckFrame::new(rx_ack.ack_type, session_id)
            .with_recommended_level(rx_ack.recommended_level);
        Ok((decoded, Some(ack_frame), last_err))
    }

    /// Demodulate a burst to soft LLRs through the RX front-end seam, for HARQ retention.
    ///
    /// Mirrors the soft branch of [`receive_from_samples_with_fec`](Self::receive_from_samples_with_fec):
    /// routes the samples through `InputCapture` (notch/AGC/DCD), demodulates soft with the current
    /// AFC correction, then refines the AFC estimate. Emits no frame/decode events — the caller owns
    /// the decode.
    fn ota_demodulate_soft(&mut self, mode: &str, samples: &[f32]) -> Result<Vec<f32>, ModemError> {
        let routed = self.route_audio_stage(
            PipelineStage::InputCapture,
            AudioSamples {
                samples: samples.to_vec(),
            },
        )?;
        let mod_cfg = ModulationConfig {
            mode: mode.to_string(),
            center_frequency: self.center_frequency + self.afc_correction_hz,
            afc_correction_hz: self.afc_correction_hz,
            ..ModulationConfig::default()
        };
        let llrs = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            openpulse_modem_descramble_soft(plugin.demodulate_soft(&routed.samples, &mod_cfg)?)
        };
        self.update_afc_estimate(mode, &routed.samples);
        Ok(llrs)
    }

    /// Decode already-demodulated (and possibly MAP-combined) soft LLRs under `fec` into a frame
    /// payload, running the same `DemodulateDecode` → `HpxStateUpdate` routing and `FrameReceived`
    /// emission as the live receive path.
    ///
    /// The soft counterpart of the per-attempt FEC dispatch in
    /// [`receive_from_samples_with_fec`](Self::receive_from_samples_with_fec), split out so HARQ
    /// combining can decode a summed-LLR vector without re-capturing audio. Side effects (HPX state,
    /// `FrameReceived`) fire only after a successful frame decode, so a failed trial never moves state.
    fn decode_combined_llrs(
        &mut self,
        mode: &str,
        llrs: &[f32],
        fec: FecMode,
    ) -> Result<Vec<u8>, ModemError> {
        let corrected = match fec {
            FecMode::SoftConcatenated => {
                let rs = soft_concat_decode_llrs(llrs)?;
                self.route_wire_stage(PipelineStage::DemodulateDecode, WirePayload { bytes: rs })?
            }
            FecMode::Ldpc => {
                let info = decode_ldpc_llrs(&LdpcCodec::new(), llrs)?;
                self.route_wire_stage(PipelineStage::DemodulateDecode, WirePayload { bytes: info })?
            }
            FecMode::LdpcHighRate => {
                let info = decode_ldpc_llrs(&LdpcCodec::high_rate(), llrs)?;
                self.route_wire_stage(PipelineStage::DemodulateDecode, WirePayload { bytes: info })?
            }
            // RS-family: the MAP-combine sharpened per-bit reliability; RS still consumes a hard
            // decision. Supports combining a plain-RS OTA rung's retransmissions.
            FecMode::Rs => {
                let wire = self.route_wire_stage(
                    PipelineStage::DemodulateDecode,
                    WirePayload {
                        bytes: hard_decide(llrs),
                    },
                )?;
                WirePayload {
                    bytes: self.rs_decode_free_strengthened(&wire.bytes)?,
                }
            }
            FecMode::RsInterleaved => {
                let wire = self.route_wire_stage(
                    PipelineStage::DemodulateDecode,
                    WirePayload {
                        bytes: hard_decide(llrs),
                    },
                )?;
                let deint = Interleaver::new(DEFAULT_INTERLEAVER_DEPTH).deinterleave(&wire.bytes);
                WirePayload {
                    bytes: FecCodec::new().decode(&deint)?,
                }
            }
            other => {
                return Err(ModemError::Demodulation(format!(
                    "FEC mode {other:?} does not support soft-LLR combining"
                )))
            }
        };
        let frame = self.stage_decode_frame(&corrected)?;
        let frame = self.route_decoded_stage(PipelineStage::HpxStateUpdate, frame)?;
        let _ = self.event_tx.send(EngineEvent::FrameReceived {
            mode: mode.to_string(),
            bytes: frame.payload.len(),
        });
        Ok(frame.payload)
    }

    /// Select HARQ retry parameters from SNR/fading state.
    ///
    /// This deterministic mapping is the Item 6 policy hook for choosing
    /// retry FEC mode and ACK timeout without mutating engine state.
    pub fn select_harq_decision(
        &self,
        snr_db: f32,
        fading_depth_db: f32,
        retry_index: u8,
    ) -> HarqDecision {
        HarqPolicy::default().select(snr_db, fading_depth_db, retry_index)
    }

    /// HARQ decision specialised to `mode`'s demodulator capability.
    ///
    /// Identical to [`select_harq_decision`](Self::select_harq_decision) except
    /// the high-rate-LDPC tier may engage when the mode's plugin produces genuine
    /// soft LLRs (the dense rungs).  Unknown modes fall back to hard-only.
    pub fn select_harq_decision_for_mode(
        &self,
        mode: &str,
        snr_db: f32,
        fading_depth_db: f32,
        retry_index: u8,
    ) -> HarqDecision {
        let soft_capable = self
            .plugins
            .get(mode)
            .map(|p| p.supports_soft_demod(mode))
            .unwrap_or(false);
        HarqPolicy::default()
            .with_soft_capable(soft_capable)
            .select(snr_db, fading_depth_db, retry_index)
    }

    /// Returns the current HPX state for this engine session.
    pub fn hpx_state(&self) -> HpxState {
        self.hpx.state()
    }

    /// Returns the active HPX session id when a secure session is in progress.
    pub fn hpx_session_id(&self) -> Option<&str> {
        self.hpx.session_id()
    }

    /// Returns emitted HPX transitions for the current session lifecycle.
    pub fn hpx_transitions(&self) -> &[HpxTransition] {
        self.hpx.transitions()
    }

    /// Returns a snapshot of per-stage pipeline queue metrics.
    pub fn pipeline_metrics_snapshot(&self) -> PipelineMetricsSnapshot {
        self.scheduler.metrics_snapshot()
    }

    /// Returns the active handshake decision for the secure session, if present.
    pub fn active_handshake(&self) -> Option<&HandshakeDecision> {
        self.active_handshake.as_ref()
    }

    /// Starts a secure HPX session and enforces handshake policy before entering transfer.
    pub fn begin_secure_session(
        &mut self,
        params: SecureSessionParams,
        timestamp_ms: u64,
    ) -> Result<HandshakeDecision, ModemError> {
        self.hpx_apply_event(HpxEvent::StartSession, timestamp_ms)?;
        self.hpx_apply_event(HpxEvent::DiscoveryOk, timestamp_ms.saturating_add(1))?;

        let handshake = evaluate_handshake(
            self.trust_policy_profile,
            params.local_minimum_mode,
            &params.peer_supported_modes,
            params.key_trust,
            params.certificate_source,
            params.psk_validated,
        )
        .map_err(|e| {
            let _ = self.hpx_apply_event(
                HpxEvent::SignatureVerificationFailed,
                timestamp_ms.saturating_add(2),
            );
            ModemError::Configuration(format!("secure handshake rejected: {e:?}"))
        })?;

        let required = minimum_trust_for_profile(self.trust_policy_profile);
        if handshake.trust.decision < required {
            let _ = self.hpx_apply_event(
                HpxEvent::SignatureVerificationFailed,
                timestamp_ms.saturating_add(2),
            );
            return Err(ModemError::Configuration(format!(
                "secure handshake trust '{}' is below required '{}' for profile '{}', reason_code={}",
                format!("{:?}", handshake.trust.decision).to_lowercase(),
                format!("{:?}", required).to_lowercase(),
                format!("{:?}", self.trust_policy_profile).to_lowercase(),
                handshake.trust.reason_code
            )));
        }

        self.hpx_apply_event(HpxEvent::TrainingOk, timestamp_ms.saturating_add(3))?;
        self.active_handshake = Some(handshake.clone());
        let _ = self.event_tx.send(EngineEvent::SessionStarted {
            session_id: self.hpx_session_id().map(str::to_string),
            peer_modes: params
                .peer_supported_modes
                .iter()
                .map(|m| format!("{m:?}"))
                .collect::<Vec<_>>()
                .join(","),
        });
        Ok(handshake)
    }

    /// Gracefully closes an active secure HPX session.
    pub fn end_secure_session(&mut self, timestamp_ms: u64) -> Result<(), ModemError> {
        if self.hpx_state() == HpxState::Idle {
            self.active_handshake = None;
            return Ok(());
        }

        let session_id = self.hpx_session_id().map(str::to_string);
        self.hpx_apply_event(HpxEvent::LocalCancel, timestamp_ms)?;
        self.hpx_apply_event(HpxEvent::TransferComplete, timestamp_ms.saturating_add(1))?;
        self.active_handshake = None;
        let _ = self.event_tx.send(EngineEvent::SessionEnded {
            session_id,
            reason: "local cancel".to_string(),
        });
        Ok(())
    }

    /// Apply an HPX state-machine event and return the emitted transition event.
    pub fn hpx_apply_event(
        &mut self,
        event: HpxEvent,
        timestamp_ms: u64,
    ) -> Result<HpxTransition, ModemError> {
        let transition = self
            .hpx
            .apply_event(event, timestamp_ms)
            .map_err(|e| ModemError::Configuration(e.to_string()))?;
        let _ = self.event_tx.send(EngineEvent::HpxTransition {
            from: transition.from_state,
            to: transition.to_state,
            event: transition.event,
            session_id: transition.session_id.clone(),
        });
        Ok(transition)
    }

    /// Encode an application payload into a signed envelope wire blob.
    pub fn encode_signed_envelope(
        &self,
        payload: &[u8],
        signing_mode: SigningMode,
        signer_id: &str,
        key_id: &str,
        signature: &[u8],
    ) -> Result<Vec<u8>, ModemError> {
        let session_id = self.hpx_session_id().unwrap_or("unsessioned");
        SignedEnvelope::new(
            session_id,
            self.sequence as u64,
            signing_mode,
            payload.to_vec(),
            signer_id,
            key_id,
            signature.to_vec(),
        )
        .encode()
    }

    /// Decode and verify a signed envelope wire blob.
    pub fn decode_signed_envelope(
        &self,
        envelope_bytes: &[u8],
    ) -> Result<SignedEnvelope, ModemError> {
        SignedEnvelope::decode(envelope_bytes)
    }

    /// Register a modulation plugin.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the plugin's trait version is incompatible with the framework.
    pub fn register_plugin(
        &mut self,
        plugin: Box<dyn openpulse_core::plugin::ModulationPlugin>,
    ) -> Result<(), PluginError> {
        info!("registering plugin: {}", plugin.info().name);
        self.plugins.register(plugin)?;
        info!("plugin registered successfully");
        Ok(())
    }

    /// Return the underlying plugin registry (read-only).
    pub fn plugins(&self) -> &PluginRegistry {
        &self.plugins
    }

    /// Encode `data` into a [`Frame`], modulate it with the plugin that
    /// handles `mode`, and write the resulting audio to the output device.
    ///
    /// Pass `device = None` to use the backend's default output device.
    pub fn transmit(
        &mut self,
        data: &[u8],
        mode: &str,
        device: Option<&str>,
    ) -> Result<(), ModemError> {
        if self.hpx_state() != HpxState::Idle {
            if self.hpx_state() != HpxState::ActiveTransfer {
                return Err(ModemError::Configuration(
                    "cannot transmit: secure session is not in active transfer".to_string(),
                ));
            }
            if self.active_handshake.is_none() {
                return Err(ModemError::Configuration(
                    "cannot transmit: secure handshake not established".to_string(),
                ));
            }
        }

        // CSMA check before encoding so a deferral does not burn a sequence number.
        self.csma_check()?;

        let outbound = self.stage_encode_frame(data)?;
        let outbound = self.route_wire_stage(PipelineStage::EncodeModulate, outbound)?;

        debug!(
            "transmitting {} byte frame (seq={}, mode={mode})",
            outbound.bytes.len(),
            self.sequence.wrapping_sub(1)
        );

        let samples = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_modulate_payload(plugin, mode, &outbound)?
        };
        let samples = self.route_audio_stage(PipelineStage::OutputEmit, samples)?;
        info!(
            "modulated {} bytes → {} audio samples",
            outbound.bytes.len(),
            samples.samples.len()
        );

        self.stage_emit_output(device, mode, &samples)?;
        // (Regulatory TX logging now happens for every frame inside `stage_emit_output`.)

        let _ = self.event_tx.send(EngineEvent::FrameTransmitted {
            mode: mode.to_string(),
            bytes: outbound.bytes.len(),
        });
        Ok(())
    }

    /// Estimate the on-air duration, in seconds, of a `payload_len`-byte frame sent in `mode`.
    ///
    /// Modulates a representative zero buffer through the mode's real modulator and divides the
    /// produced sample count by the sample rate. This is pure — it does not touch the wire sequence,
    /// PTT, or the audio backend — so callers (e.g. airtime-bounded TX burst planning) can size bursts
    /// without side effects. It omits frame + FEC expansion, so it is a slight under-estimate; callers
    /// that need a safety bound should keep `burst_max_secs` comfortably under any PTT watchdog.
    /// Returns `None` for an unknown mode or a modulator error.
    pub fn estimate_air_secs(&self, payload_len: usize, mode: &str) -> Option<f64> {
        let plugin = self.plugins.get(mode)?;
        let mod_cfg = ModulationConfig {
            mode: mode.to_string(),
            center_frequency: self.center_frequency,
            ..ModulationConfig::default()
        };
        let samples = plugin.modulate(&vec![0u8; payload_len], &mod_cfg).ok()?;
        let fs = AudioConfig::default().sample_rate as f64;
        if fs <= 0.0 {
            return None;
        }
        Some(samples.len() as f64 / fs)
    }

    /// Encode `data`, modulate to baseband I/Q, and write to the IQ output stream.
    ///
    /// Requires the audio backend to support [`AudioBackend::open_iq_output`].
    /// Returns `ModemError::Configuration` when the backend has no IQ output.
    ///
    /// Compliance fencing (audit G-2): this IQ path does not share `stage_emit_output`, so it applies
    /// the regulatory bookkeeping itself — TX attenuation on the baseband IQ, the §97 TX-metadata log,
    /// and the `frames_transmitted` bump that arms the auto-ID timer (all via `record_tx_frame`). The
    /// only seam transforms it still omits are the **audio-envelope-domain** CE-SSB conditioner and the
    /// `tanh` peak limiter, which have no IQ-domain equivalent yet — a hardware/PA limiter or SDR
    /// headroom is the caller's responsibility on this path.
    pub fn transmit_iq(
        &mut self,
        data: &[u8],
        mode: &str,
        device: Option<&str>,
    ) -> Result<(), ModemError> {
        self.csma_check()?;

        let outbound = self.stage_encode_frame(data)?;
        let outbound = self.route_wire_stage(PipelineStage::EncodeModulate, outbound)?;

        let (mut i_bb, mut q_bb) = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            // Via the shared stage, NOT `plugin.modulate_iq` directly: the bytes must be whitened
            // exactly as the audio path whitens them, because every receive path un-whitens
            // unconditionally. Calling the plugin here is what made this transmit undecodable.
            self.stage_modulate_payload_iq(plugin, mode, &outbound)?
        };

        // Power control: apply the configured TX attenuation to the baseband IQ (same dB as the audio
        // seam). Default 0 dB is a no-op.
        let atten_linear = 10.0f32.powf(self.tx_attenuation_db / 20.0);
        if (atten_linear - 1.0).abs() >= 1e-6 {
            for s in i_bb.iter_mut() {
                *s *= atten_linear;
            }
            for s in q_bb.iter_mut() {
                *s *= atten_linear;
            }
        }

        let audio_cfg = AudioConfig::default();
        let mut stream = self
            .audio
            .open_iq_output(device.or(self.default_device.as_deref()), &audio_cfg)
            .ok_or_else(|| {
                ModemError::Configuration("audio backend does not support IQ output".to_string())
            })?
            .map_err(|e| ModemError::Audio(e.to_string()))?;

        stream
            .write_iq(&i_bb, &q_bb)
            .map_err(|e| ModemError::Audio(e.to_string()))?;
        stream
            .flush()
            .map_err(|e| ModemError::Audio(e.to_string()))?;

        // Route through the same compliance bookkeeping as the audio seam: regulatory log + frame count.
        self.record_tx_frame(mode)?;

        let _ = self.event_tx.send(EngineEvent::FrameTransmitted {
            mode: mode.to_string(),
            bytes: outbound.bytes.len(),
        });
        Ok(())
    }

    /// Read audio from the input device, demodulate with the plugin for
    /// `mode`, and return the decoded frame payload.
    ///
    /// Pass `device = None` to use the backend's default input device.
    pub fn receive(&mut self, mode: &str, device: Option<&str>) -> Result<Vec<u8>, ModemError> {
        let samples = self.stage_capture_input(Some(mode), device)?;
        self.receive_from_samples(mode, samples)
    }

    /// Receive a frame by listening on the input stream until a decode succeeds
    /// or the timeout elapses (no FEC).
    pub fn receive_with_timeout(
        &mut self,
        mode: &str,
        device: Option<&str>,
        listen_for: Duration,
    ) -> Result<Vec<u8>, ModemError> {
        self.receive_with_timeout_fec(mode, device, listen_for, FecMode::None)
    }

    /// As [`receive_with_timeout`](Self::receive_with_timeout) but applies the FEC
    /// codec `fec` to each decode attempt — the timeout-scanning counterpart of
    /// [`receive_with_fec_mode`](Self::receive_with_fec_mode), needed for live
    /// (loopback / on-air) reception of FEC-protected frames.
    pub fn receive_with_fec_mode_timeout(
        &mut self,
        mode: &str,
        fec: FecMode,
        device: Option<&str>,
        listen_for: Duration,
    ) -> Result<Vec<u8>, ModemError> {
        self.receive_with_timeout_fec(mode, device, listen_for, fec)
    }

    /// Suppression wrapper for the scanning receiver — see `suppress_afc_events`.
    ///
    /// The scan loop below restores `afc_correction_hz` after failed attempts (both in the settle
    /// micro-sweep and the full-buffer retry), so per-attempt `AfcUpdate`s report corrections the
    /// engine discards, and at ~129 attempts they evict genuine events from the 64-slot ring. A
    /// wrapper rather than in-line guards because this function has many return points and each one
    /// would have to restore the flag.
    fn receive_with_timeout_fec(
        &mut self,
        mode: &str,
        device: Option<&str>,
        listen_for: Duration,
        fec: FecMode,
    ) -> Result<Vec<u8>, ModemError> {
        let was_quiet = self.suppress_afc_events;
        self.suppress_afc_events = true;
        let result = self.receive_with_timeout_fec_inner(mode, device, listen_for, fec);
        self.suppress_afc_events = was_quiet;
        // The correction a successful acquisition settled on IS committed state.
        if result.is_ok() {
            self.emit_afc_update(mode);
        }
        result
    }

    fn receive_with_timeout_fec_inner(
        &mut self,
        mode: &str,
        device: Option<&str>,
        listen_for: Duration,
        fec: FecMode,
    ) -> Result<Vec<u8>, ModemError> {
        let audio_cfg = AudioConfig::default();
        let mut stream = self
            .audio
            .open_input(device.or(self.default_device.as_deref()), &audio_cfg)
            .map_err(|e| ModemError::Audio(e.to_string()))?;

        // Record the mode for the receiver front end. Unlike `receive_with_fec`, this path reads
        // the stream directly instead of going through `stage_capture_input`, so nothing else sets
        // it — and the notch's protected band is `center ± bw/2` with `bw` falling back to
        // `notch_fallback_bw_hz` (2000 Hz) when the mode is unknown. That fallback is 4x BPSK250's
        // real occupied width, so it protected 500..2500 Hz and made every interferer in that span
        // structurally un-notchable (`peaks_from_spectrum` skips the protected band). Measured on
        // the REQ-QRM-01 fixture: without this line the 2200 Hz interferer is never notched at any
        // amplitude; with it the band is 1250..1750 and the notch places 2199 Hz as its
        // highest-prominence notch. Gated by `the_notch_rescues_a_decode_that_fails_without_it`
        // (its ATTRIBUTION and CAUSATION legs), in `tests/notch_rescues_interferer.rs`.
        self.rx_mode = Some(mode.to_string());

        let deadline = Instant::now() + listen_for;
        let start_time = Instant::now();
        let mut accumulated = Vec::new();
        let mut last_err: Option<ModemError> = None;
        let mut loop_iterations: usize = 0;

        // Frame geometry: scan step, acquisition window, and per-attempt slice
        // bounds.  Preferred source is the plugin itself via frame_geometry().
        // The legacy fallback (trailing mode-name digits as baud, 32-symbol
        // preamble) is only correct for modes named after their baud rate —
        // it parsed OFDM52's subcarrier count as baud and SCFDMA52-64QAM-P4
        // as 4 baud — and remains only for unregistered/external plugins.
        let geometry = self.plugins.get(mode).and_then(|p| {
            p.frame_geometry(&ModulationConfig {
                mode: mode.to_string(),
                sample_rate: audio_cfg.sample_rate,
                ..ModulationConfig::default()
            })
        });
        let (step, acq_samples, min_frame_samples, max_frame_samples) = match geometry {
            Some(g) => (
                g.symbol_period_samples.max(1),
                g.preamble_samples.max(g.symbol_period_samples).max(1),
                g.min_frame_samples.max(1),
                g.max_frame_samples.max(g.min_frame_samples),
            ),
            None => {
                let step = {
                    let baud: u32 = mode
                        .trim_end_matches("-RRC")
                        .bytes()
                        .rev()
                        .take_while(|b| b.is_ascii_digit())
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .fold(0u32, |acc, b| acc * 10 + (b - b'0') as u32);
                    if baud > 0 {
                        (audio_cfg.sample_rate as f32 / baud as f32).round() as usize
                    } else {
                        32
                    }
                };
                // 33 = PREAMBLE_SYMS(32) + 1 data symbol; 2280 = preamble +
                // full 255-byte RS frame at 1 bit/symbol + 10 % margin.
                (step, step * 32, step * 33, step * 2280)
            }
        };

        // A "long-frame" mode takes many seconds of real-time audio to buffer (the
        // slow BPSK rungs: BPSK31 ≈ 12 s).  For these the settled full-buffer retry
        // is skipped (its O(buffer) re-scan every 2 s outruns the read cadence and
        // starves the loop so the frame never finishes buffering — the first-energy
        // micro-sweep owns the decode instead).  Every other mode keeps the retry:
        // the wideband multicarrier modes (SCFDMA/OFDM) in particular have short
        // frames AND a marginal settle that the single-carrier first-energy path
        // can't decode, so they depend on the retry's per-position re-acquisition.
        // FEC widens the per-attempt slice (conv rate-1/2 ≈ 2×, RS ≈ 1.15×) so the whole coded frame
        // is decoded rather than truncated, and the long-frame classification must be made on that
        // widened length — see `frame_plan` for what went wrong when it was not.
        // Keep the RAW geometry: it is the plugin's own "largest frame this mode emits, plus margin"
        // and is the right measure of *has the frame arrived yet*. `frame_plan`'s widened value is a
        // per-attempt SLICE reserve — deliberately generous so the slice still contains the frame
        // wherever inside it the frame starts — and using a slice reserve to judge arrival made the
        // settle-recovery precondition unreachable for the slow rungs: BPSK31 needed 149.2 s of
        // post-onset audio, more than any configured harness listens for (archetype scan
        // 2026-07-29, finding 4). The two lengths answer different questions; keep them apart.
        let arrival_samples = frame_arrival_samples(max_frame_samples, fec);
        let (max_frame_samples, long_frame) = frame_plan(max_frame_samples, fec);

        // AFC settling window.  It must span at least one data symbol past the
        // preamble so the plugin's fine (IQ-squaring) estimator engages; on a
        // pure preamble-length window (`acq_samples`) the estimator falls back to
        // the coarse ±12.5 Hz Goertzel grid, whose ≤6.25 Hz residual is inside the
        // faster BPSK modes' tolerance but exceeds BPSK31's ±7.8 Hz (= baud/4 for
        // differential detection) — which is why 31.25-baud frames were
        // undecodable while BPSK63/100/250 passed.  `min_frame_samples`
        // (= preamble + 1 symbol) is exactly that fine-AFC threshold and is only
        // one symbol longer than `acq_samples`, so settling cost is unchanged.
        let afc_window = acq_samples.max(min_frame_samples);

        // Adaptive silence gate (absolute floor 1e-4 mean-square, raised above
        // an elevated band noise floor; see EnergyGate).  Silence is typically
        // < 2.5e-5 mean-square; a live BPSK carrier at 30 % full-scale gives
        // ≈ 0.045.
        let mut energy_gate = EnergyGate::new();
        // Correlation corroboration for a settle (#1049). `None` when the mode's plugin publishes
        // no preamble template, or when the template is too long to correlate affordably — in both
        // cases the settle is decided on energy alone, exactly as before.
        let preamble_matcher = self.build_preamble_veto(mode, audio_cfg.sample_rate);
        // Cost of the last full-buffer retry pass, and how much audio it covered. If a pass costs
        // more wall time than the audio it walked, further passes can only fall further behind.
        let mut retry_cost_secs: f64 = 0.0;
        let mut retry_span_secs: f64 = 0.0;
        let mut retry_over_budget = false;
        // Scan/retry policy state (see ScanPlanner).  On first signal
        // detection the engine runs fast AFC settling passes in-place (no
        // decode), then the planner resets the scan to that position so the
        // first full decode attempt uses a converged AFC correction.  Without
        // this, afc_step=0.1 takes ~22 scan positions (~704 samples) to
        // converge, by which point the scan has advanced past the preamble
        // start and can never re-decode it.
        let mut planner = ScanPlanner::new(step, min_frame_samples);
        if let Some(limit) = self.settle_failure_limit {
            planner.settle_failure_limit = limit.max(1);
        }
        // Round-robin forward-onset offset for the first-energy re-decode (see the
        // fep block below).  Persisted across iterations so each iteration tries
        // exactly ONE onset — running all offsets per iteration starves the read
        // loop (each BPSK31 decode is a full demod of a multi-second window), so
        // the frame never finishes buffering.
        let mut fep_offset_k = 0usize;

        loop {
            let chunk = stream
                .read()
                .map_err(|e| ModemError::Audio(e.to_string()))?;
            if !chunk.is_empty() {
                accumulated.extend(chunk);
                debug!("received {} accumulated audio samples", accumulated.len());
            }

            // One-shot full-frame retry around the first-energy position.
            // Fires when accumulated ≥ fep + max_frame_samples.  By then the full
            // frame is in the buffer.  Retry positions span fep ± one symbol period
            // (step samples) only — NOT a full preamble lookback.  The preamble must
            // be near the START of each slice so that find_timing_offset (which only
            // searches within one symbol period) can locate it.  Earlier runs used
            // fep ± PREAMBLE_SYMS (1024 samples) which placed the preamble 32 symbols
            // into the slice for positions before fep, causing find_timing_offset to
            // return a garbage offset and decode the preamble bits as frame data.
            // Retry fires when enough audio has accumulated to guarantee the
            // full frame is in the buffer:
            //   accumulated ≥ signal_arrival_samples + frame_size
            //
            // For 8 kHz (loopback / IC-9700 USB):
            //   signal_arrival ≈ IRS_STARTUP_WAIT × 8000 = 40 000 samples
            //   frame_size     ≈ 20 224 samples  (BPSK250, 64 B payload)
            //   minimum        ≈ 60 000 → 7.5 s at 8 kHz
            //
            // For FT-991A PipeWire (~3 600 effective samples/s):
            //   signal_arrival ≈ 6 s × 3 600 = 21 600 samples
            //   frame_size     ≈ 20 224 samples
            //   minimum        ≈ 42 000; trigger at 60 000 → fires at 16.7 s
            //   IRS kill window ≈ 17 s — just within budget
            //
            // A fep-relative threshold (fep + N) fails when fep fires on
            // early noise far before the signal; the fixed count avoids that.
            // Wall-clock trigger: retry fires every 2 s starting at T=12 s.
            // The FT-991A PipeWire effective rate varies between 2 300 and
            // 7 600 samples/s, making sample-count thresholds unreliable.
            // At the first firing the signal may not yet be fully buffered
            // (slice too short → CRC fails).  Re-firing every 2 s lets each
            // subsequent attempt use a longer accumulated buffer until the
            // frame fits and the decode succeeds.
            // The full-buffer retry is the fallback for a missed settle.  For
            // long-frame modes only, skip it once settled: its O(buffer) re-scan
            // every 2 s outlasts the read cadence on a multi-second frame (BPSK31),
            // starving the loop so the frame never finishes buffering — and the
            // first-energy micro-sweep below already owns the decode there.  Other
            // modes (notably wideband SCFDMA/OFDM, whose marginal settle the
            // single-carrier micro-sweep can't decode) keep the retry; their short
            // frames re-scan cheaply, so it never starves them.
            // THIRD wall-clock dependency, and the one #1066 missed. `retry_due` schedules the
            // full-buffer retry on elapsed SECONDS, so a slower profile fires fewer retries per
            // unit of work. That is correct in production for the reason `RETRY_START_SECS`
            // documents — effective sample rates vary 2300-7600/s between audio stacks, so a
            // sample count is not a reliable clock there. It is fatal to measurement, and it
            // dominates the no-template path specifically: with no preamble template the veto
            // never runs, so the full-buffer retry is that path's only rescue. Measured on
            // QPSK500 at an identical work budget, debug reached 27 condemnations and decoded
            // where release reached 222 and did not.
            //
            // In measurement mode the buffer IS an exact clock, so derive it from there.
            // Derived from LOOP ITERATIONS, not from the buffer. A buffer-derived clock looks
            // right and is not: `LoopbackBackend::read` without pacing delivers everything on the
            // first call, so the derived time jumps straight past `RETRY_START_SECS` and every
            // retry front-loads — deterministic, but different behaviour rather than the same
            // behaviour made reproducible (measured: 0 condemnations where the shipped path takes
            // ~250). Iterations advance once per read cycle in both regimes.
            //
            // The nominal rate is the daemon's 100 ms receive tick, i.e. 10 iterations per second,
            // which is what makes 120/20 the iteration equivalents of the shipped 12 s/2 s.
            const NOMINAL_ITERS_PER_SEC: usize = 10;
            let elapsed_secs = match self.deterministic_max_iterations {
                Some(_) => (loop_iterations / NOMINAL_ITERS_PER_SEC) as u64,
                None => start_time.elapsed().as_secs(),
            };
            // A retry pass that takes longer than the audio it covers can never catch up: the buffer
            // grows faster than the scan walks it, so every later pass is further behind and the
            // frame is never reached. `long_frame` is only a *proxy* for that — it guesses from frame
            // geometry, and geometry is the wrong variable. `PILOT-QPSK500` (55 200 coded samples,
            // "short") costs ~640 ms per decode attempt and needs ~1000 positions, i.e. ~11 minutes of
            // CPU for a 45 s listen, while `QPSK250` at twice the frame length passes comfortably.
            // So measure the previous pass instead of predicting it: once a pass has proved slower
            // than real time, stop retrying and leave the decode to the first-energy micro-sweep.
            let retry_hopeless =
                retry_over_budget || (retry_cost_secs > retry_span_secs && retry_span_secs > 0.0);
            if (!long_frame || !planner.is_settled())
                && !retry_hopeless
                && planner.retry_due(elapsed_secs, accumulated.len())
            {
                let retry_started = Instant::now();
                let retry_span_start = accumulated.len();
                {
                    // Scan the entire accumulated buffer from the start.
                    // The AFC correction is kept from the settled value:
                    // when settling succeeded on the real signal the
                    // correction is valid (e.g. −43.8 Hz carrier offset).
                    // A 43.8 Hz offset at 250 baud causes a 63° phase ramp
                    // per symbol, which flips preamble bits after 2 symbols
                    // and prevents timing lock — resetting to 0 would cause
                    // all retry positions to fail even when the signal is
                    // present.  If settling was rejected (saved_afc = 0)
                    // the retry falls back to AFC=0 naturally.
                    let retry_end = accumulated.len().saturating_sub(min_frame_samples);
                    let saved_afc = self.afc_correction_hz;
                    // Budget the pass from INSIDE. Measuring it afterwards is useless: the pathological
                    // case is a pass that never finishes at all (`PILOT-QPSK500` needs ~1000 positions
                    // at ~640 ms each — ~11 minutes for a 45 s listen), so it must be abandoned while
                    // running. The budget is the audio the pass covers: a scan that cannot walk its own
                    // buffer in less than real time can never catch up, because the buffer keeps growing.
                    let retry_budget = Duration::from_secs_f64(
                        retry_span_start as f64 / audio_cfg.sample_rate as f64,
                    );
                    let mut over_budget = false;
                    for (scanned, start) in (0..=retry_end).step_by(step).enumerate() {
                        // Check periodically rather than per position; the check is cheap but the
                        // decode it guards is not, so granularity of ~16 positions is plenty.
                        let exhausted = match self.deterministic_scan_positions {
                            Some(limit) => scanned >= limit,
                            None => scanned % 16 == 0 && retry_started.elapsed() > retry_budget,
                        };
                        if exhausted {
                            over_budget = true;
                            break;
                        }
                        let gate_end = (start + acq_samples).min(accumulated.len());
                        let gate_len = gate_end - start;
                        // Adaptive energy gate: skip silent positions.  The
                        // mini-settle AFC stability guard (divergence check
                        // below) handles noise positions that pass this gate
                        // by rejecting them before the expensive decode runs.
                        if gate_len > 0 {
                            let msq = accumulated[start..gate_end]
                                .iter()
                                .map(|s| s * s)
                                .sum::<f32>()
                                / gate_len as f32;
                            if !energy_gate.passes(msq) {
                                continue;
                            }
                        }
                        // Mini-settle: 6 fast AFC passes refine the carrier
                        // estimate before the full decode (anchor + fine, see
                        // afc_mini_settle).  Only skip if the result diverged
                        // past the Goertzel acquisition limit — a convergence
                        // guard on |change| would incorrectly block signals at
                        // exactly fc (0 Hz offset) and signals at the Goertzel
                        // boundary (which saturate and accumulate).
                        let settle_end = (start + afc_window).min(accumulated.len());
                        if settle_end - start >= afc_window {
                            let settle =
                                self.afc_mini_settle(mode, &accumulated[start..settle_end]);
                            // Stability guard: reject if the fine-track
                            // drifted >20 Hz from the anchor (unstable noise)
                            // or exceeded the Goertzel range.  The energy gate
                            // above already filters silence; this catches
                            // noise that slips through.
                            if (settle.fine - settle.anchor).abs() > 20.0
                                || settle.fine.abs() > AFC_MAX_CORRECTION_HZ
                            {
                                self.afc_correction_hz = saved_afc;
                                continue;
                            }
                        }
                        let end = (start + max_frame_samples).min(accumulated.len());
                        if end.saturating_sub(start) < min_frame_samples {
                            continue;
                        }
                        debug!(
                            "AFC full-retry: pos={start} correction={:.1}Hz",
                            self.afc_correction_hz
                        );
                        match self.decode_attempt(
                            mode,
                            AudioSamples {
                                samples: accumulated[start..end].to_vec(),
                            },
                            fec,
                        ) {
                            Ok(payload) => return Ok(payload),
                            Err(err) => {
                                debug!("AFC full-retry: pos={start} FAILED: {err}");
                                last_err = Some(err);
                                self.afc_correction_hz = saved_afc;
                            }
                        }
                    }
                    self.afc_correction_hz = saved_afc;
                    if over_budget {
                        retry_over_budget = true;
                    }
                }
                retry_cost_secs = retry_started.elapsed().as_secs_f64();
                retry_span_secs = retry_span_start as f64 / audio_cfg.sample_rate as f64;
                if retry_over_budget {
                    debug!(
                        "full-buffer retry exceeded its {retry_span_secs:.1} s budget after \
                         {retry_cost_secs:.1} s — slower than real time, disabling further retries"
                    );
                }
            }

            // Once settling has located the preamble (first_energy_pos), re-decode
            // from there on EVERY iteration with the current — possibly grown —
            // buffer.  A long frame preceded by silence (e.g. BPSK31: ~12 s frame
            // after the IRS startup wait) may not have fully arrived when the
            // preamble position was first scanned, giving a truncated window ("no
            // data symbols after preamble"); the broad scan then advances past it
            // via commit_scan and never returns, so without this the frame never
            // decodes.  Bounded to one decode per iteration.
            if let Some(fep) = planner.first_energy_pos() {
                // Forward onset micro-sweep.  The settled onset (`fep`) lands at or
                // slightly before the true preamble, but the energy gate + refine
                // can sit up to ~1-2 symbols early on a clean turn-on, and a
                // demodulator only searches one symbol period for timing.  The
                // decodable onset window is narrow (~2 symbols) and asymmetric — a
                // start can be ~1.5 symbols early but barely a third of a symbol
                // late — so the lowest baud rate (BPSK31, 256 samples/symbol) sits
                // right at the boundary and fails on runs where the estimate lands
                // a touch too early.  `fep` is never *after* the onset (the gate
                // trips on the rising edge or before), so sweeping a few half-symbol
                // steps FORWARD reliably lands one attempt inside the window.  The
                // extra attempts only run once the frame is fully buffered (a short
                // buffer fails "frame too short" for every forward offset too, so we
                // skip the sweep in that case and just wait for more audio).
                // Forward-onset micro-sweep, ONE offset per iteration.  The settled
                // onset sits at or slightly before the true preamble (the gate trips
                // on the rising edge or earlier), but the energy gate + refine can be
                // up to ~1-2 symbols early on a clean turn-on, and the demodulator
                // only searches one symbol period for timing.  The decodable onset
                // window is narrow (~2 symbols) and asymmetric — a start may be ~1.5
                // symbols early but barely a third late — so the lowest baud rate
                // (BPSK31) sits at the boundary and fails on runs where the estimate
                // lands a touch early.  Stepping a few half-symbols FORWARD lands one
                // attempt in the window.  Critically this cycles ONE offset per
                // iteration (not all at once): each BPSK31 decode demodulates a
                // multi-second window, so sweeping every offset per read would starve
                // the loop and the long frame would never finish buffering.
                let half = (step / 2).max(1);
                // Same constant `ScanPlanner::unsettle` sizes its re-anchor from: the span swept
                // here is exactly the span proven undecodable when the anchor is condemned. These
                // were independent literals until #1040, and the mismatch was the crawl.
                let onset = fep + (fep_offset_k % ScanPlanner::SWEEP_OFFSETS) * half;
                fep_offset_k = fep_offset_k.wrapping_add(1);
                let end = (onset + max_frame_samples).min(accumulated.len());
                if end.saturating_sub(onset) >= min_frame_samples {
                    // Only a FULLY buffered window can condemn the anchor: a short window fails
                    // for lack of audio, not because the position is wrong, and counting those
                    // would abandon a perfectly good settle on a slow frame.
                    //
                    // Measured against `arrival_samples` (the raw geometry), not the widened slice
                    // reserve — see where `arrival_samples` is bound. A multi-block frame can exceed
                    // this, so the threshold can be reached slightly early; the
                    // `SETTLE_FAILURE_LIMIT` budget absorbs that, because each of those 18 iterations
                    // demodulates a multi-second window and far more audio has arrived by the end of
                    // them. An unreachable threshold, by contrast, disables the recovery outright.
                    let window_complete = accumulated.len() >= onset + arrival_samples;
                    let afc_before = self.afc_correction_hz;
                    if window_complete && self.sweep_attempt_inputs.len() < 4_096 {
                        self.sweep_attempt_inputs.push((
                            fep_offset_k.wrapping_sub(1),
                            onset,
                            end - onset,
                            afc_before,
                        ));
                    }
                    // The first-energy path was the ONLY decode route with no position trace, so
                    // which onsets it actually tried had to be inferred from `unsettle` arithmetic
                    // — and the inference is mode-dependent enough to invert a conclusion (#1058).
                    // Log the onset WITH the correction: this path decodes at the anchor's settled
                    // AFC, while the full-buffer retry mini-settles per candidate, so the two can
                    // disagree at the same sample offset for a reason that is not the offset.
                    debug!(
                        "first-energy attempt: onset={onset} (fep={fep} k={}) correction={:.1}Hz \
                         window_complete={window_complete}",
                        fep_offset_k.wrapping_sub(1) % ScanPlanner::SWEEP_OFFSETS,
                        afc_before
                    );
                    match self.decode_attempt(
                        mode,
                        AudioSamples {
                            samples: accumulated[onset..end].to_vec(),
                        },
                        fec,
                    ) {
                        Ok(payload) => return Ok(payload),
                        Err(err) => {
                            last_err = Some(err);
                            self.afc_correction_hz = afc_before;
                            // A settle that cannot decode its own fully-buffered window, across a
                            // whole sweep of onsets, is not a settle — it is noise the gate
                            // mistook for a preamble. Re-open the search rather than re-decoding
                            // the same wrong position until the listen window expires (#1021).
                            let planner_limit = planner.settle_failure_limit;
                            if window_complete && planner.note_settle_failure() {
                                debug!(
                                    "settle at {fep} condemned after {} fully-buffered failures; \
                                     re-opening the scan (AFC reset from {:.1} Hz)",
                                    planner_limit, self.afc_correction_hz
                                );
                                planner.unsettle();
                                self.settle_condemnations += 1;
                                if self.condemned_positions.len() < 4_096 {
                                    self.condemned_positions.push(fep);
                                }
                                // A condemnation used to raise the energy gate here
                                // (`EnergyGate::note_condemned`, #1045). **Removed 2026-07-31 after
                                // ablation: it was inert where the correlation veto runs, and
                                // actively harmful where it does not.** On BPSK250 removing it is
                                // bit-identical (4/4/5 condemnations at leads 40k/80k/120k, all
                                // decoding, same ρ rejections). On QPSK500 — no preamble template,
                                // so energy is still the only frame-start criterion — removing it
                                // turns **FAIL into OK** at leads 40k and 80k (92/87 condemnations
                                // and no decode, versus 315 and a decode).
                                //
                                // The mechanism compounds: each condemnation raised the floor
                                // through `.max()`, and with nothing suppressing the noise settles
                                // that drive it, the raises stacked until the gate sat *above the
                                // signal* and no settle was possible at all. #1045 measured its fix
                                // on BPSK250 alone and applied it to every mode.
                                //
                                // The eliminations recorded with #1045 still stand and are not
                                // reopened by this: do not re-engage a floor raise on *level*
                                // saturation (it gates out every buffer-is-the-frame fixture, and no
                                // absolute bound separates them — the 0.010 AGC fixture sits below
                                // the 0.0154 hot noise floor), and do not force the full-buffer
                                // retry live (it reuses the same gate and settles on noise too).
                                // The correction came from the discredited anchor; keeping it
                                // would bias every subsequent acquisition.
                                self.afc_correction_hz = 0.0;
                            }
                        }
                    }
                }
            }

            // Broad scan to LOCATE the first signal energy and settle AFC.  Once
            // settled, the first-energy re-decode above owns the decode (re-trying
            // the preamble as the buffer grows), so the broad scan stops: continuing
            // it would re-decode every forward position on a full-buffer window each
            // iteration, starving the loop so the frame never finishes buffering.
            // The T>=12 s full-buffer retry remains as a fallback for a bad settle.
            if !accumulated.is_empty() && !planner.is_settled() {
                'inner: for start in planner.scan_positions(accumulated.len()) {
                    // Fast energy gate: check the first 32 symbol periods at this
                    // position.  Silence costs < 0.1 ms; only emit the full
                    // demodulation call (≈ 90 ms on a Pi 4) when signal is present.
                    let gate_end = (start + acq_samples).min(accumulated.len());
                    let gate_len = gate_end - start;
                    let mean_sq = if gate_len > 0 {
                        accumulated[start..gate_end]
                            .iter()
                            .map(|s| s * s)
                            .sum::<f32>()
                            / gate_len as f32
                    } else {
                        0.0
                    };
                    if !energy_gate.passes(mean_sq) {
                        continue;
                    }

                    // On the very first signal-energy position, run 6 fast AFC
                    // estimation passes in-place before attempting any decode.
                    // A temporary step of 0.7 converges in 6 iterations:
                    // (1 − 0.3⁶) × 150 Hz ≈ 149.9 Hz — effectively one-shot for
                    // crystal errors up to ±300 Hz on 144 MHz (≈ ±2 ppm).
                    if !planner.is_settled() {
                        // Refine the coarse gate position to the true signal onset
                        // BEFORE settling AFC.  The energy gate can trip up to a full
                        // acquisition window early, with the signal entering only at the
                        // window tail (e.g. QPSK500: the gate trips ~240 samples before
                        // the frame).  Settling at the coarse position then runs the
                        // carrier estimator over a mostly-silent window, which yields a
                        // confident-but-bogus correction (QPSK500: a stable ~257 Hz from
                        // ~2 signal symbols, last_delta≈0 so it passes the convergence
                        // guard) that breaks the decode at the correct onset.  Settling
                        // from the onset keeps the window on signal.
                        let onset = refine_onset(&accumulated, start, acq_samples, step);
                        // Settle over `afc_window` (preamble + 1 symbol) from the onset,
                        // NOT max_frame_samples: the latter makes settling O(N²) in buffer
                        // length when the noise floor is above the gate (every position
                        // fires the gate, each runs 6 Goertzel passes on the full slice)
                        // and the scan falls behind live audio.  afc_window is
                        // ~preamble-sized (fast) yet long enough to engage the plugin's
                        // fine AFC stage — see its definition above.
                        let settle_end = (onset + afc_window).min(accumulated.len());
                        if settle_end - onset < afc_window {
                            // The onset's signal window is not fully buffered yet; wait for
                            // the next read (the broad scan re-runs as the buffer grows).
                            continue;
                        }
                        let settle = self.afc_mini_settle(mode, &accumulated[onset..settle_end]);
                        // Stability check: the final fine pass must have converged (small
                        // last delta), the fine track must agree with the anchor within
                        // 20 Hz (real carrier), and the magnitude must not exceed the
                        // Goertzel acquisition range.
                        let converged =
                            settle.last_delta < 5.0 && (settle.fine - settle.anchor).abs() <= 20.0;
                        let plausible = settle.fine.abs() <= AFC_MAX_CORRECTION_HZ;
                        if !converged || !plausible {
                            debug!(
                                "AFC settling rejected at onset={onset} (coarse={start}): \
                                 converged={converged} plausible={plausible} \
                                 correction={:.1}Hz",
                                self.afc_correction_hz
                            );
                            self.afc_correction_hz = 0.0;
                            continue;
                        }
                        // Corroborate the settle by CORRELATION before believing it (#1049).
                        //
                        // Energy answers "is something here", never "is this a preamble", and that
                        // gap is the whole #1020/#1021/#1039/#1040/#1045 family: on a band floor
                        // above the gate the receiver settles AFC on idle noise before the frame
                        // arrives, then spends its listen window re-decoding that position. codec2
                        // decides frame start on a normalised correlation ratio with no absolute
                        // energy threshold at all, which makes that failure impossible for it by
                        // construction.
                        //
                        // The check runs AFTER the settle, not before it, because a matched filter
                        // integrates coherently: at this template length a 20 Hz carrier offset
                        // already drops a real frame's ρ to 0.332 and 400 Hz drops it to 0.016,
                        // while this chain is required to acquire to ±400 Hz (`AFC_MAX_CORRECTION_HZ`
                        // caps it there; the measured decode reach is 600 Hz via the retry arm).
                        // Checking first would reject every off-frequency frame. The settle supplies
                        // the frequency and the correlation confirms the *waveform* — the settle
                        // proposes, the correlation disposes.
                        //
                        // Only this arm is gated. The full-buffer retry (above) never calls
                        // `note_settled`, so it cannot strand the receiver on a noise anchor, which
                        // is the specific failure this exists to prevent; leaving it ungated also
                        // preserves it as the documented fallback for a bad settle.
                        if let Some(veto) = preamble_matcher.as_ref() {
                            let corr_end = (onset + afc_window).min(accumulated.len());
                            if let Some((rho, _)) =
                                self.preamble_rho(veto, &accumulated[onset..corr_end], settle.fine)
                            {
                                // REQ-RX-02 / REQ-RX-03. Every query is a calibration sample: the
                                // stream this compares against is the stream it is built from, which
                                // is why it costs no extra correlation. See `rho_calibration`.
                                let verdict = self.decide_preamble_veto(veto, rho, onset);
                                if let VetoVerdict::Rejected { threshold } = verdict {
                                    debug!(
                                        "settle at onset={onset} rejected: preamble correlation \
                                         rho={rho:.3} < {threshold:.2} (published {:.2}, \
                                         correction={:.1}Hz) — energy without a preamble",
                                        veto.rho_threshold, settle.fine
                                    );
                                    self.afc_correction_hz = 0.0;
                                    continue;
                                }
                                // **The correlation VETOES a settle; it does not place it.**
                                //
                                // Placing it was tried and reverted, and the reason is worth
                                // keeping. On the saturating-floor reproduction the surviving
                                // settles are not noise — they sit on the frame's leading EDGE
                                // (onsets 39328…39972 for a frame at 40000, ρ climbing 0.461 →
                                // 1.000 as the window slides on), where a partial overlap clears
                                // the threshold but the demodulator cannot decode, and each one
                                // then costs `SETTLE_FAILURE_LIMIT` decodes to condemn. Snapping
                                // the onset to the correlation's answer fixes exactly that.
                                //
                                // It also breaks the opposite case, because an ALTERNATING preamble
                                // is periodic: an alignment two symbols late still matches 29 of 31
                                // symbols. Measured on the capture-AGC fixture, whose frame starts
                                // at sample 0 with ρ = 0.877, the argmax chose offset 65 — two
                                // symbol periods in — truncating the preamble into "invalid magic".
                                // Taking the first threshold crossing instead fixes that one and
                                // un-fixes the first, because the partial overlap already clears
                                // 0.40. Both live in the same search, and every rule that separates
                                // them (a peak-ratio, a decisive-improvement margin) is a constant
                                // fitted to these two fixtures — the archetype this repo keeps
                                // paying for.
                                //
                                // So the snap is left out. The veto is what carries #1049's value:
                                // it removes the settle-on-NOISE class outright, which is the class
                                // the five defects were. The residual edge settles are what the
                                // micro-sweep and condemnation recovery already exist to handle.
                                // Placing the onset properly needs a preamble whose autocorrelation
                                // is not periodic — a PN or chirp sync word, a wire-format change.
                                if self.accepted_settle_positions.len() < 4_096 {
                                    self.accepted_settle_positions.push(onset);
                                }
                                if matches!(verdict, VetoVerdict::StoodDown) {
                                    debug!(
                                        "settle at onset={onset} let through: the veto is \
                                         standing down (rho={rho:.3})"
                                    );
                                } else {
                                    debug!("settle at onset={onset} corroborated: rho={rho:.3}");
                                }
                            }
                        }
                        planner.note_settled(onset);
                        info!(
                            "AFC settling done: correction={:.1}Hz onset={onset} (coarse={start}) buf_len={}",
                            self.afc_correction_hz,
                            accumulated.len()
                        );
                        break 'inner;
                    }

                    // Bound the demodulation window to one maximum-length frame so
                    // the per-attempt cost does not grow with accumulated buffer size.
                    let end = (start + max_frame_samples).min(accumulated.len());
                    // Save AFC state before each decode attempt: on failure the
                    // attempted demodulation has already called update_afc_estimate
                    // (step=0.1 per call).  Without the restore, ~1744 failed
                    // attempts per outer loop accumulate >1000 Hz of drift.
                    let afc_before = self.afc_correction_hz;
                    debug!("AFC decode: pos={} correction={:.1}Hz", start, afc_before);
                    match self.decode_attempt(
                        mode,
                        AudioSamples {
                            samples: accumulated[start..end].to_vec(),
                        },
                        fec,
                    ) {
                        Ok(payload) => return Ok(payload),
                        Err(err) => {
                            last_err = Some(err);
                            self.afc_correction_hz = afc_before;
                        }
                    }
                }
                planner.commit_scan(accumulated.len());
            }

            loop_iterations += 1;
            match self.deterministic_max_iterations {
                Some(max) => {
                    if loop_iterations >= max {
                        break;
                    }
                }
                None => {
                    if Instant::now() >= deadline {
                        break;
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            ModemError::Demodulation(format!(
                "no decodable frame within {} ms",
                listen_for.as_millis()
            ))
        }))
    }

    fn receive_from_samples(
        &mut self,
        mode: &str,
        samples: AudioSamples,
    ) -> Result<Vec<u8>, ModemError> {
        let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;

        info!("received {} audio samples", samples.samples.len());

        let (wire, snr_opt) = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            let mod_cfg = ModulationConfig {
                mode: mode.to_string(),
                center_frequency: self.center_frequency + self.afc_correction_hz,
                afc_correction_hz: self.afc_correction_hz,
                ..ModulationConfig::default()
            };
            // Prefer soft demodulation: a single pass yields both LLRs (for SNR)
            // and hard bits (via sign decision), avoiding a redundant demodulate() call.
            // Only plugins that declare soft support take this path; for them a soft
            // error is a genuine demodulation failure, not a cue to re-demodulate hard
            // (which would double the per-attempt cost and can't succeed where the
            // soft pass failed — both share the same acquisition front end).
            // Absolute RX SNR for rate adaptation: the mode's calibrated symbol-domain estimate
            // (M2M4 fallback inside `rx_snr_db`). The old mean-|LLR| proxy reads ≈ −2 dB on a
            // clean path (only a relative confidence indicator) and can't drive the SNR-hint
            // ladder.
            //
            // Computed on BOTH branches. Until 2026-07-30 it lived inside the soft arm, so the
            // predicate gating it was the demodulator's *soft capability* rather than the
            // availability of an SNR estimate — two unrelated properties. `QPSK250-D` implements
            // `estimate_snr_db` and reports `supports_soft_demod = false` (differential has no soft
            // path, #923), so `hpx_hf`'s whole hard-FEC lower half recorded nothing (archetype scan
            // 2026-07-29, finding 10). The rate ladder was unaffected — it reads a value computed in
            // a separate call path — but `last_rx_snr_db()` feeds the QSY scan's candidate scoring
            // (which scored every channel on `unwrap_or(0.0)`) and the ADIF logbook's `rx_snr`.
            let snr = self.rx_snr_db(mode, &samples.samples);
            if plugin.supports_soft_demod(mode) {
                let llrs = openpulse_modem_descramble_soft(
                    plugin.demodulate_soft(&samples.samples, &mod_cfg)?,
                );
                let wire_bytes: Vec<u8> = llrs
                    .chunks(8)
                    .map(|byte_llrs| {
                        byte_llrs.iter().enumerate().fold(0u8, |acc, (i, &llr)| {
                            acc | ((llr.is_sign_negative() as u8) << i)
                        })
                    })
                    .collect();
                (WirePayload { bytes: wire_bytes }, Some(snr))
            } else {
                let wire = self.stage_demodulate_payload(plugin, mode, &samples)?;
                (wire, Some(snr))
            }
        };
        let wire = self.route_wire_stage(PipelineStage::DemodulateDecode, wire)?;
        debug!("demodulated {} bytes", wire.bytes.len());

        self.update_afc_estimate(mode, &samples.samples);
        self.emit_afc_update(mode);

        let frame = self.stage_decode_frame(&wire)?;
        let frame = self.route_decoded_stage(PipelineStage::HpxStateUpdate, frame)?;
        info!("received frame seq={}", frame.sequence);

        // RECORD THE SNR ONLY ON A VALIDATED FRAME (#1142).
        //
        // This used to run above, right after demodulation — before magic, CRC and sequence had
        // been checked, and before the `?` on `stage_decode_frame` could return. During a scan that
        // means every failed attempt overwrote `last_rx_snr_db()` with a reading taken on a
        // MISFRAMED slice, and the last one to run is the value the QSY scan's candidate scoring
        // and the ADIF logbook then read.
        //
        // The AFC directly below is rolled back on exactly this reasoning — the scan wrappers reset
        // `afc_correction_hz` at the top of every iteration precisely so a failed attempt cannot
        // leak. The SNR sat two lines away with no equivalent, which is the blind-sibling shape.
        if let Some(snr) = snr_opt {
            self.rate_policy.record_rx_snr(snr);
        }

        let _ = self.event_tx.send(EngineEvent::FrameReceived {
            mode: mode.to_string(),
            bytes: frame.payload.len(),
        });
        Ok(frame.payload)
    }

    /// Dispatch one decode attempt: no-FEC uses the unchanged
    /// [`receive_from_samples`](Self::receive_from_samples); otherwise the
    /// FEC-aware path. Keeps the `FecMode::None` behaviour byte-identical.
    fn decode_attempt(
        &mut self,
        mode: &str,
        samples: AudioSamples,
        fec: FecMode,
    ) -> Result<Vec<u8>, ModemError> {
        match fec {
            FecMode::None => self.receive_from_samples(mode, samples),
            _ => self.receive_from_samples_with_fec(mode, samples, fec),
        }
    }

    /// FEC-aware counterpart of [`receive_from_samples`](Self::receive_from_samples):
    /// demodulate the slice, apply codec `fec`, then decode the frame. Mirrors the
    /// one-shot `receive_with_*_fec` methods but operates on a provided sample slice
    /// so the timeout-scanning loop can apply FEC per attempt.
    /// Logging wrapper: the coded path had NO instrumentation at all, so an on-air coded
    /// failure produced an empty log while the uncoded path logged every attempt — the
    /// difference that made issue #1021 undiagnosable from a real capture. Every per-attempt
    /// outcome is now visible at `--log debug`, at one line per attempt (same order of volume
    /// as the uncoded path's "demodulated N bytes").
    fn receive_from_samples_with_fec(
        &mut self,
        mode: &str,
        samples: AudioSamples,
        fec: FecMode,
    ) -> Result<Vec<u8>, ModemError> {
        let n = samples.samples.len();
        let result = self.receive_from_samples_with_fec_inner(mode, samples, fec);
        match &result {
            Ok(payload) => debug!(
                "fec attempt OK: mode={mode} fec={fec:?} samples={n} payload={} bytes",
                payload.len()
            ),
            Err(e) => debug!("fec attempt FAILED: mode={mode} fec={fec:?} samples={n}: {e}"),
        }
        result
    }

    fn receive_from_samples_with_fec_inner(
        &mut self,
        mode: &str,
        samples: AudioSamples,
        fec: FecMode,
    ) -> Result<Vec<u8>, ModemError> {
        let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;

        let mod_cfg = ModulationConfig {
            mode: mode.to_string(),
            center_frequency: self.center_frequency + self.afc_correction_hz,
            afc_correction_hz: self.afc_correction_hz,
            ..ModulationConfig::default()
        };

        // Turbo is a soft code but a *fixed-block* one (the QPP interleaver block
        // size is `llrs.len()/3`), so the scanning slice's trailing-noise LLRs make
        // the block size wrong — it can't decode through this path and is rejected
        // below. It is deliberately excluded here so it does not pay for a soft
        // demodulation it cannot use.
        let soft = matches!(
            fec,
            FecMode::SoftConcatenated | FecMode::Ldpc | FecMode::LdpcHighRate
        );

        // Soft codecs consume LLRs; hard codecs consume demodulated wire bytes.
        let (llrs, raw_wire) = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            if soft {
                (
                    Some(openpulse_modem_descramble_soft(
                        plugin.demodulate_soft(&samples.samples, &mod_cfg)?,
                    )),
                    None,
                )
            } else {
                (
                    None,
                    Some(self.stage_demodulate_payload(plugin, mode, &samples)?),
                )
            }
        };

        // What the demodulator actually produced, before any codec runs. Distinguishes "the
        // demod yielded nothing / a length the codec must reject" from "the codec ran and
        // failed" — the two look identical from the outside and need different fixes. For the
        // hard codecs the byte count is what the multiple-of-255 / prefix logic keys off.
        debug!(
            "fec demod: mode={mode} fec={fec:?} soft={soft} wire_bytes={} llrs={}",
            raw_wire.as_ref().map_or(0, |w| w.bytes.len()),
            llrs.as_ref().map_or(0, |l| l.len())
        );

        // The SNR reading is COMPUTED here (soft demod has run and the samples are in hand) but
        // RECORDED only once the frame validates, at the end of this function (#1142).
        // `llrs.is_some()` means the demodulator mechanically produced LLRs, which is a long way
        // from "this burst was a frame".
        let pending_snr = llrs
            .is_some()
            .then(|| self.rx_snr_db(mode, &samples.samples));

        // SKIP THE AFC ESTIMATE INSIDE A SCAN — it is work whose result is thrown away.
        //
        // `suppress_afc_events` is raised by the scan wrappers, and those loops reset
        // `afc_correction_hz` to the pre-attempt value at the top of EVERY iteration. So on a failed
        // scan attempt this estimate is rolled back by design; the reset is the proof that nothing
        // downstream can depend on it. Measured on a real filexfer burst it was 60.7 s of 116.5 s —
        // 52% of the candidate loop spent computing corrections it then discarded.
        //
        // The successful attempt still needs one: the scan wrappers recompute it on the WINNING
        // slice before emitting, so "keep only the successful update" stays literally true and a
        // committed decode still reports exactly one `AfcUpdate` (gates: `afc_event_flood`,
        // `engine_events`).
        if !self.suppress_afc_events {
            self.update_afc_estimate(mode, &samples.samples);
        }
        self.emit_afc_update(mode);

        // Invariant (audit G-7): the demod above populates exactly one of `raw_wire` / `llrs`, keyed
        // to the FEC family it will be decoded with — hard-decision modes (Rs*/Concatenated) carry
        // `raw_wire = Some`, soft-decision modes (SoftConcatenated/Ldpc*) carry `llrs = Some`. Each
        // per-arm `.unwrap()` below is guarded by that producer↔arm pairing, never operator input.
        let corrected = match fec {
            FecMode::Rs => {
                let wire =
                    self.route_wire_stage(PipelineStage::DemodulateDecode, raw_wire.unwrap())?;
                WirePayload {
                    // decode_prefix, not decode: this is the SCANNING receive, so `wire.bytes` is a
                    // fixed-length window out of the capture buffer — its length is a function of
                    // the window, not the frame, so `decode` rejected it on the multiple-of-255
                    // gate before RS ever ran whenever the capture outlasted the frame
                    // (audit 2026-07-19). `decode_combined_llrs` and the single-shot
                    // `receive_with_fec_mode` keep strict `decode` — they know the frame extent.
                    bytes: self.rs_decode_prefix_free_strengthened(&wire.bytes)?,
                }
            }
            FecMode::RsInterleaved => {
                let wire =
                    self.route_wire_stage(PipelineStage::DemodulateDecode, raw_wire.unwrap())?;
                WirePayload {
                    // Prefix trial, not a straight deinterleave: the permutation is derived from the
                    // buffer length, so the window length must be trimmed to the frame's *before* it
                    // is unscrambled. Same reason the `Rs` arm above uses `decode_prefix`.
                    bytes: rs_interleaved_decode_prefix(DEFAULT_INTERLEAVER_DEPTH, &wire.bytes)?,
                }
            }
            FecMode::SoftConcatenated => {
                let llrs = llrs.unwrap();
                let rs = soft_concat_decode_llrs(&llrs)?;
                self.route_wire_stage(PipelineStage::DemodulateDecode, WirePayload { bytes: rs })?
            }
            FecMode::Ldpc => {
                let llrs = llrs.unwrap();
                // Prefix, not strict: trailing-noise codewords past the frame must not abort it.
                let info = decode_ldpc_llrs_prefix(&LdpcCodec::new(), &llrs)?;
                self.route_wire_stage(PipelineStage::DemodulateDecode, WirePayload { bytes: info })?
            }
            FecMode::LdpcHighRate => {
                let llrs = llrs.unwrap();
                let info = decode_ldpc_llrs_prefix(&LdpcCodec::high_rate(), &llrs)?;
                self.route_wire_stage(PipelineStage::DemodulateDecode, WirePayload { bytes: info })?
            }
            FecMode::Concatenated => {
                let wire =
                    self.route_wire_stage(PipelineStage::DemodulateDecode, raw_wire.unwrap())?;
                let conv = ConvCodec::new().decode(&wire.bytes)?;
                WirePayload {
                    bytes: FecCodec::new().decode(&conv)?,
                }
            }
            FecMode::RsStrong => {
                let wire =
                    self.route_wire_stage(PipelineStage::DemodulateDecode, raw_wire.unwrap())?;
                WirePayload {
                    bytes: FecCodec::strong().decode_prefix(&wire.bytes)?,
                }
            }
            // ShortRs (byte-exact, no length prefix) and Turbo (fixed QPP block size
            // = llrs.len()/3) both need the exact frame length, which the scanning
            // receive can't guarantee (trailing-noise samples inflate the count), so
            // they stay single-shot.
            other => {
                return Err(ModemError::Demodulation(format!(
                    "FEC mode {other:?} is not supported by the timeout receive; \
                     use receive_with_fec_mode for a single-shot decode"
                )))
            }
        };

        let frame = self.stage_decode_frame(&corrected)?;
        let frame = self.route_decoded_stage(PipelineStage::HpxStateUpdate, frame)?;

        // Success-gated (#1142): recorded only now that magic, CRC and sequence have passed.
        // Recorded at demod time it survived every failed scan attempt, so a failed burst left
        // `last_rx_snr_db()` holding the last MISFRAMED slice's value — which the QSY scan's
        // candidate scoring and the ADIF logbook read as the SNR of the frame they heard. The AFC
        // in this same function is rolled back on exactly this reasoning; the SNR was not.
        if let Some(snr) = pending_snr {
            self.rate_policy.record_rx_snr(snr);
        }
        let _ = self.event_tx.send(EngineEvent::FrameReceived {
            mode: mode.to_string(),
            bytes: frame.payload.len(),
        });
        Ok(frame.payload)
    }

    /// Receive a data frame and derive an ACK type recommendation for the sender.
    ///
    /// This is the full adaptive receive path:
    /// 1. Captures audio samples, demodulates, and decodes the payload (identical
    ///    to [`receive`](Self::receive)).
    /// 2. Estimates receive-path SNR from the mean absolute LLR magnitude.
    /// 3. Applies the SNR estimate to the RX direction of the active rate adapter.
    /// 4. Returns the decoded payload together with the [`AckType`] the caller
    ///    should transmit back to the sender via
    ///    [`transmit_ack_with_short_fec`](Self::transmit_ack_with_short_fec).
    ///
    /// When no adaptive session is active the returned `AckType` is always
    /// [`AckType::AckOk`].
    ///
    /// On decode failure returns `Err`; the caller should transmit
    /// [`AckType::Nack`] in that case.
    pub fn receive_with_ack_hint(
        &mut self,
        mode: &str,
        device: Option<&str>,
    ) -> Result<(Vec<u8>, AckType), ModemError> {
        let samples = self.stage_capture_input(Some(mode), device)?;
        let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;

        self.update_afc_estimate(mode, &samples.samples);
        self.emit_afc_update(mode);

        let mod_cfg = ModulationConfig {
            mode: mode.to_string(),
            center_frequency: self.center_frequency + self.afc_correction_hz,
            afc_correction_hz: self.afc_correction_hz,
            ..ModulationConfig::default()
        };

        let plugin = self
            .plugins
            .get(mode)
            .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;

        let llrs =
            openpulse_modem_descramble_soft(plugin.demodulate_soft(&samples.samples, &mod_cfg)?);
        // Absolute SNR for the rate decision: the mode's calibrated symbol-domain estimate (M2M4
        // fallback inside `rx_snr_db`); the mean-|LLR| proxy reads ~-2 dB on a clean path and can't
        // drive the ladder.
        let snr_db = self.rx_snr_db(mode, &samples.samples);

        let wire_bytes: Vec<u8> = llrs
            .chunks(8)
            .map(|byte_llrs| {
                byte_llrs.iter().enumerate().fold(0u8, |acc, (i, &llr)| {
                    acc | ((llr.is_sign_negative() as u8) << i)
                })
            })
            .collect();

        let wire = WirePayload { bytes: wire_bytes };
        let wire = self.route_wire_stage(PipelineStage::DemodulateDecode, wire)?;
        let frame = self.stage_decode_frame(&wire)?;
        let frame = self.route_decoded_stage(PipelineStage::HpxStateUpdate, frame)?;
        info!(
            "receive_with_ack_hint: seq={} snr={:.1}dB",
            frame.sequence, snr_db
        );

        // Success-gated (#1142). Recorded before `stage_decode_frame`, a reading taken on a slice
        // that turned out not to be a frame still reached `last_rx_snr_db()` — the value the QSY
        // scan scores candidates on and the ADIF logbook writes into the QSO record.
        self.rate_policy.record_rx_snr(snr_db);

        let _ = self.event_tx.send(EngineEvent::FrameReceived {
            mode: mode.to_string(),
            bytes: frame.payload.len(),
        });

        let ack_type = self.rate_policy.select_rx_ack_type(snr_db);
        Ok((frame.payload, ack_type))
    }

    /// ISS ARQ transmit: send `data`, wait for a FSK4-ACK reply, retry on Nack.
    ///
    /// Transmits the frame up to `1 + max_retries` times (initial attempt plus
    /// retries).  On each attempt:
    /// - A successful ACK (`AckOk`, `AckUp`, `AckDown`) is applied to the TX
    ///   rate adapter and the call returns `Ok(rate_event)`.
    /// - A `Nack` or a receive error is treated as a delivery failure; the TX
    ///   adapter is stepped down and the frame is retransmitted.
    ///
    /// Returns [`ModemError::ArqMaxRetries`] if no ACK is received after all
    /// attempts are exhausted.
    ///
    /// Pass `max_retries = 0` to transmit once with no retry (equivalent to
    /// `transmit` followed by a single `receive_ack_with_short_fec`).
    pub fn transmit_arq(
        &mut self,
        data: &[u8],
        mode: &str,
        device: Option<&str>,
        max_retries: usize,
    ) -> Result<RateEvent, ModemError> {
        let attempts = 1 + max_retries;
        for attempt in 0..attempts {
            let current_mode = self.current_adaptive_mode().unwrap_or(mode).to_owned();
            self.transmit(data, &current_mode, device)?;

            match self.receive_ack_with_short_fec(device) {
                Ok(ack_frame) if ack_frame.ack_type != AckType::Nack => {
                    let rate_event = self.apply_ack_frame(&ack_frame);
                    info!(
                        "ARQ: ACK {:?} after attempt {}/{}",
                        ack_frame.ack_type,
                        attempt + 1,
                        attempts
                    );
                    return Ok(rate_event);
                }
                Ok(_nack) => {
                    // Nack: step down TX rate and retry.
                    let _ = self.apply_ack(AckType::AckDown);
                    info!(
                        "ARQ: Nack on attempt {}/{}, retrying",
                        attempt + 1,
                        attempts
                    );
                }
                Err(e) => {
                    // No ACK received at all: treat as implicit Nack.
                    let _ = self.apply_ack(AckType::AckDown);
                    info!(
                        "ARQ: no ACK on attempt {}/{} ({e}), retrying",
                        attempt + 1,
                        attempts
                    );
                }
            }
        }
        Err(ModemError::ArqMaxRetries(attempts))
    }

    /// IRS side of an ARQ exchange: receive one data frame and reply with an ACK.
    ///
    /// Receives at the current RX adaptive mode when a session is active, else at
    /// `mode`. On a clean decode it replies with the SNR-derived [`AckType`] (always
    /// [`AckType::AckOk`] without an adaptive session) and returns the payload; on
    /// decode failure it replies [`AckType::Nack`] and returns the error, so the
    /// transmitting [`transmit_arq`](Self::transmit_arq) peer retransmits.
    ///
    /// This is the reliable, fixed-mode counterpart to `transmit_arq`. Adaptive
    /// rate-stepping in the RX direction (keeping the IRS RX level in lockstep with
    /// the ISS TX level across an `AckUp`) is layered on top separately.
    pub fn respond_arq(
        &mut self,
        mode: &str,
        session_id: &str,
        device: Option<&str>,
    ) -> Result<Vec<u8>, ModemError> {
        let rx_mode = self.current_rx_mode().unwrap_or(mode).to_owned();
        match self.receive_with_ack_hint(&rx_mode, device) {
            Ok((payload, ack_type)) => {
                let ack = AckFrame::new(ack_type, session_id);
                self.transmit_ack_with_short_fec(&ack, device)?;
                Ok(payload)
            }
            Err(e) => {
                let nack = AckFrame::new(AckType::Nack, session_id);
                let _ = self.transmit_ack_with_short_fec(&nack, device);
                Err(e)
            }
        }
    }

    /// Set the station callsign used in broadcast frame headers.
    pub fn set_callsign(&mut self, callsign: impl Into<String>) {
        self.callsign = callsign.into();
        self.update_tx_session_callsign();
    }

    /// The station callsign stamped into TX frame headers and the regulatory TX-metadata log.
    pub fn callsign(&self) -> &str {
        &self.callsign
    }

    /// Set the TX attenuation applied to all transmitted audio (dB; 0.0 = no attenuation).
    ///
    /// Negative values reduce output level; e.g. `-6.0` halves the amplitude.
    /// Call this whenever the rig frequency changes to restore the per-band setting.
    pub fn set_tx_attenuation_db(&mut self, db: f32) {
        self.tx_attenuation_db = db;
    }

    /// Return the current TX attenuation in dB.
    pub fn tx_attenuation_db(&self) -> f32 {
        self.tx_attenuation_db
    }

    /// Set the DCD/squelch RMS threshold — the carrier-present level used by
    /// channel-busy detection, CSMA, and [`capture_burst`](Self::capture_burst)'s
    /// burst-flush. Raise it on a noisy band so the noise floor doesn't read as a
    /// permanent carrier; call on frequency change to restore the per-band value.
    pub fn set_dcd_squelch(&mut self, threshold: f32) {
        self.dcd.set_threshold(threshold);
    }

    /// Return the current DCD/squelch RMS threshold.
    pub fn dcd_squelch(&self) -> f32 {
        self.dcd.threshold()
    }

    /// Set the soft TX limiter threshold (0.0 disables the limiter).
    pub fn set_tx_limiter_threshold(&mut self, threshold: f32) {
        self.tx_limiter_threshold = threshold;
    }

    /// Set the maximum TX power in watts for regulatory compliance (0.0 = no limit).
    pub fn set_max_power_watts(&mut self, watts: f32) {
        self.max_power_watts = watts.max(0.0);
    }

    /// Return the current maximum TX power limit in watts.
    pub fn max_power_watts(&self) -> f32 {
        self.max_power_watts
    }

    /// Return reference to the transmission session log for regulatory compliance.
    pub fn tx_session_log(&self) -> &TxSessionLog {
        &self.tx_session_log
    }

    /// Persist the §97 TX record to disk (#1110). `None` disables spilling.
    ///
    /// Disk is the record and the in-memory `TxSessionLog` is a bounded query cache — the log was
    /// previously unbounded AND lost on restart, and the restart half is what made it unfit as a
    /// compliance record regardless of its size.
    pub fn set_tx_log_path(&mut self, path: Option<std::path::PathBuf>) {
        self.tx_log_path = path;
        self.tx_log_failed = false;
    }

    /// Append one NDJSON record. A failed write must NEVER stop a transmission that is already on
    /// the air, so every error here is logged and swallowed — and logged only ONCE per configured
    /// path, because a full or unwritable disk would otherwise emit a line per frame forever.
    fn spill_tx_metadata(&mut self, metadata: &TxMetadata) {
        let Some(path) = self.tx_log_path.clone() else {
            return;
        };
        if self.tx_log_failed {
            return;
        }
        let line = match serde_json::to_string(metadata) {
            Ok(l) => l,
            Err(e) => {
                warn!("tx log: could not serialise TX metadata: {e}");
                self.tx_log_failed = true;
                return;
            }
        };
        let write = (|| -> std::io::Result<()> {
            use std::io::Write as _;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            writeln!(f, "{line}")
        })();
        if let Err(e) = write {
            warn!(
                path = %path.display(),
                "tx log: append failed ({e}); the §97 record is incomplete from here on. \
                 Further failures on this path are suppressed."
            );
            self.tx_log_failed = true;
        }
    }

    /// Whether the TX-log spill has given up (a write failed). Tripwire: a compliance record that
    /// silently stopped being written is the failure mode worth surfacing.
    pub fn tx_log_failed(&self) -> bool {
        self.tx_log_failed
    }

    /// Clear the transmission session log.
    ///
    /// FIXME(#1092): this is the ONLY bound on `TxSessionLog.frames`, and it has no caller.
    /// `record_tx_frame` pushes one `TxMetadata` at every emit seam with no cap, so a long-running
    /// daemon grows the log without limit. The fix is NOT to call this on a timer — that would
    /// destroy the §97 compliance record it exists to keep. It needs a retention decision
    /// (bounded window vs. spill to disk) taken against the regulatory requirement, not here.
    pub fn clear_tx_session_log(&mut self) {
        self.tx_session_log = TxSessionLog::new(self.callsign.clone());
    }

    /// Update callsign in active TX session log.
    fn update_tx_session_callsign(&mut self) {
        self.tx_session_log.station_id = self.callsign.clone();
    }

    /// Transmit a one-to-many broadcast frame.
    ///
    /// Unlike [`transmit`](Self::transmit), this method bypasses the CSMA
    /// persistence check — broadcasts are short, and the sender is responsible
    /// for scheduling.  No ACK is expected; no session state is updated.
    ///
    /// The frame is wrapped in a `BroadcastFrame` payload inside a `WireEnvelope`
    /// with `dst_peer_id = [0; 32]` (broadcast address) and `hop_index = 0`.
    /// `ttl` limits how many times relay nodes may re-broadcast the frame.
    pub fn broadcast(
        &mut self,
        payload: &[u8],
        mode: &str,
        ttl: u8,
        device: Option<&str>,
    ) -> Result<(), ModemError> {
        // Defer on a busy channel before burning a sequence number (matches every other TX path).
        self.csma_check()?;

        let seq = self.broadcast_seq;
        self.broadcast_seq = self.broadcast_seq.wrapping_add(1);

        let frame = BroadcastFrame {
            callsign_hash: callsign_hash(&self.callsign),
            seq,
            ttl,
            flags: 0,
            payload: payload.to_vec(),
        };

        let envelope = WireEnvelope {
            msg_type: WireMsgType::BroadcastFrame,
            flags: 0,
            session_id: 0,
            src_peer_id: [0u8; 32],
            dst_peer_id: [0u8; 32], // broadcast address
            nonce: nonce_from_seq(seq),
            timestamp_ms: 0,
            hop_limit: ttl,
            hop_index: 0,
            payload: frame.encode(),
            signature: None,
        };

        let wire_bytes = envelope
            .encode()
            .map_err(|e| ModemError::Configuration(e.to_string()))?;

        let outbound = self.stage_encode_frame(&wire_bytes)?;
        let outbound = self.route_wire_stage(PipelineStage::EncodeModulate, outbound)?;

        let samples = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_modulate_payload(plugin, mode, &outbound)?
        };
        let samples = self.route_audio_stage(PipelineStage::OutputEmit, samples)?;
        self.stage_emit_output(device, mode, &samples)?;

        let _ = self.event_tx.send(EngineEvent::FrameTransmitted {
            mode: mode.to_string(),
            bytes: payload.len(),
        });
        Ok(())
    }

    /// Like [`transmit`](Self::transmit) but wraps the encoded frame bytes
    /// with Reed-Solomon FEC before modulation.
    ///
    /// On a noisy channel the receiver can use [`receive_with_fec`](Self::receive_with_fec)
    /// to correct up to **16 byte errors per 255-byte RS block** after
    /// demodulation.
    pub fn transmit_with_fec(
        &mut self,
        data: &[u8],
        mode: &str,
        device: Option<&str>,
    ) -> Result<(), ModemError> {
        self.csma_check()?;

        let frame_wire = self.stage_encode_frame(data)?;
        let fec_bytes = FecCodec::new().encode(&frame_wire.bytes);
        let fec_wire = WirePayload { bytes: fec_bytes };
        let fec_wire = self.route_wire_stage(PipelineStage::EncodeModulate, fec_wire)?;

        debug!(
            "FEC transmitting {} byte FEC block (seq={}, mode={mode})",
            fec_wire.bytes.len(),
            self.sequence.wrapping_sub(1)
        );

        let samples = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_modulate_payload(plugin, mode, &fec_wire)?
        };
        let samples = self.route_audio_stage(PipelineStage::OutputEmit, samples)?;
        self.stage_emit_output(device, mode, &samples)?;
        let _ = self.event_tx.send(EngineEvent::FrameTransmitted {
            mode: mode.to_string(),
            bytes: fec_wire.bytes.len(),
        });
        Ok(())
    }

    /// Like [`receive`](Self::receive) but applies Reed-Solomon FEC error
    /// correction after demodulation before decoding the frame.
    pub fn receive_with_fec(
        &mut self,
        mode: &str,
        device: Option<&str>,
    ) -> Result<Vec<u8>, ModemError> {
        let samples = self.stage_capture_input(Some(mode), device)?;
        let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;

        let raw_wire = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_demodulate_payload(plugin, mode, &samples)?
        };
        let raw_wire = self.route_wire_stage(PipelineStage::DemodulateDecode, raw_wire)?;

        self.update_afc_estimate(mode, &samples.samples);
        self.emit_afc_update(mode);

        let corrected_bytes = self.rs_decode_free_strengthened(&raw_wire.bytes)?;
        let corrected_wire = WirePayload {
            bytes: corrected_bytes,
        };

        let frame = self.stage_decode_frame(&corrected_wire)?;
        let frame = self.route_decoded_stage(PipelineStage::HpxStateUpdate, frame)?;
        info!("FEC receive: frame seq={}", frame.sequence);

        let _ = self.event_tx.send(EngineEvent::FrameReceived {
            mode: mode.to_string(),
            bytes: frame.payload.len(),
        });
        Ok(frame.payload)
    }

    /// Like [`transmit_with_fec`](Self::transmit_with_fec) but also applies a
    /// stride interleaver after RS encoding so that burst channel errors are
    /// dispersed across blocks before the receiver corrects them.
    pub fn transmit_with_fec_interleaved(
        &mut self,
        data: &[u8],
        mode: &str,
        device: Option<&str>,
        interleaver_depth: usize,
    ) -> Result<(), ModemError> {
        self.csma_check()?;

        let frame_wire = self.stage_encode_frame(data)?;
        let fec_bytes = FecCodec::new().encode(&frame_wire.bytes);
        let interleaved = Interleaver::new(interleaver_depth).interleave(&fec_bytes);
        let il_wire = WirePayload { bytes: interleaved };
        let il_wire = self.route_wire_stage(PipelineStage::EncodeModulate, il_wire)?;

        let samples = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_modulate_payload(plugin, mode, &il_wire)?
        };
        let samples = self.route_audio_stage(PipelineStage::OutputEmit, samples)?;
        self.stage_emit_output(device, mode, &samples)?;
        let _ = self.event_tx.send(EngineEvent::FrameTransmitted {
            mode: mode.to_string(),
            bytes: il_wire.bytes.len(),
        });
        Ok(())
    }
    /// Like [`receive_with_fec`](Self::receive_with_fec) but deinterleaves the
    /// received bytes before RS decoding to undo the transmitter's interleaving.
    pub fn receive_with_fec_interleaved(
        &mut self,
        mode: &str,
        device: Option<&str>,
        interleaver_depth: usize,
    ) -> Result<Vec<u8>, ModemError> {
        let samples = self.stage_capture_input(Some(mode), device)?;
        let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;

        let raw_wire = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_demodulate_payload(plugin, mode, &samples)?
        };
        let raw_wire = self.route_wire_stage(PipelineStage::DemodulateDecode, raw_wire)?;

        self.update_afc_estimate(mode, &samples.samples);
        self.emit_afc_update(mode);

        let deinterleaved = Interleaver::new(interleaver_depth).deinterleave(&raw_wire.bytes);
        let corrected_bytes = FecCodec::new().decode(&deinterleaved)?;
        let corrected_wire = WirePayload {
            bytes: corrected_bytes,
        };

        let frame = self.stage_decode_frame(&corrected_wire)?;
        let frame = self.route_decoded_stage(PipelineStage::HpxStateUpdate, frame)?;
        let _ = self.event_tx.send(EngineEvent::FrameReceived {
            mode: mode.to_string(),
            bytes: frame.payload.len(),
        });
        Ok(frame.payload)
    }

    /// Transmit with concatenated Conv(rate-1/2) inner + RS outer FEC.
    ///
    /// TX chain: frame encode → RS encode → Conv encode → modulate → emit.
    /// Use [`receive_with_concatenated_fec`](Self::receive_with_concatenated_fec)
    /// on the receive side.
    pub fn transmit_with_concatenated_fec(
        &mut self,
        data: &[u8],
        mode: &str,
        device: Option<&str>,
    ) -> Result<(), ModemError> {
        self.csma_check()?;

        let frame_wire = self.stage_encode_frame(data)?;
        let rs_bytes = FecCodec::new().encode(&frame_wire.bytes);
        let conv_bytes = ConvCodec::new().encode(&rs_bytes);
        let fec_wire = WirePayload { bytes: conv_bytes };
        let fec_wire = self.route_wire_stage(PipelineStage::EncodeModulate, fec_wire)?;

        debug!(
            "concatenated FEC transmitting {} bytes (seq={}, mode={mode})",
            fec_wire.bytes.len(),
            self.sequence.wrapping_sub(1)
        );

        let samples = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_modulate_payload(plugin, mode, &fec_wire)?
        };
        let samples = self.route_audio_stage(PipelineStage::OutputEmit, samples)?;
        self.stage_emit_output(device, mode, &samples)?;
        let _ = self.event_tx.send(EngineEvent::FrameTransmitted {
            mode: mode.to_string(),
            bytes: fec_wire.bytes.len(),
        });
        Ok(())
    }

    /// Receive with concatenated Conv(rate-1/2) inner + RS outer FEC.
    ///
    /// RX chain: capture → demodulate → Conv decode → RS decode → frame decode.
    pub fn receive_with_concatenated_fec(
        &mut self,
        mode: &str,
        device: Option<&str>,
    ) -> Result<Vec<u8>, ModemError> {
        let samples = self.stage_capture_input(Some(mode), device)?;
        let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;

        let raw_wire = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_demodulate_payload(plugin, mode, &samples)?
        };
        let raw_wire = self.route_wire_stage(PipelineStage::DemodulateDecode, raw_wire)?;

        self.update_afc_estimate(mode, &samples.samples);
        self.emit_afc_update(mode);

        let conv_decoded = ConvCodec::new().decode(&raw_wire.bytes)?;
        let rs_decoded = FecCodec::new().decode(&conv_decoded)?;
        let corrected_wire = WirePayload { bytes: rs_decoded };

        let frame = self.stage_decode_frame(&corrected_wire)?;
        let frame = self.route_decoded_stage(PipelineStage::HpxStateUpdate, frame)?;
        info!("concatenated FEC receive: frame seq={}", frame.sequence);
        let _ = self.event_tx.send(EngineEvent::FrameReceived {
            mode: mode.to_string(),
            bytes: frame.payload.len(),
        });
        Ok(frame.payload)
    }

    /// Transmit with K=7 soft-decision Conv inner + RS outer FEC (BL-FEC-5).
    ///
    /// TX chain: frame encode → RS encode → SoftViterbiCodec encode → modulate → emit.
    pub fn transmit_with_soft_viterbi_fec(
        &mut self,
        data: &[u8],
        mode: &str,
        device: Option<&str>,
    ) -> Result<(), ModemError> {
        self.csma_check()?;

        let frame_wire = self.stage_encode_frame(data)?;
        let rs_bytes = FecCodec::new().encode(&frame_wire.bytes);
        let sv_bytes = soft_concat_encode(&rs_bytes);
        let fec_wire = WirePayload { bytes: sv_bytes };
        let fec_wire = self.route_wire_stage(PipelineStage::EncodeModulate, fec_wire)?;

        debug!(
            "soft-Viterbi FEC transmitting {} bytes (seq={}, mode={mode})",
            fec_wire.bytes.len(),
            self.sequence.wrapping_sub(1)
        );

        let samples = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_modulate_payload(plugin, mode, &fec_wire)?
        };
        let samples = self.route_audio_stage(PipelineStage::OutputEmit, samples)?;
        self.stage_emit_output(device, mode, &samples)?;
        let _ = self.event_tx.send(EngineEvent::FrameTransmitted {
            mode: mode.to_string(),
            bytes: fec_wire.bytes.len(),
        });
        Ok(())
    }

    /// Receive with K=7 soft-decision Conv inner + RS outer FEC (BL-FEC-5).
    ///
    /// RX chain: capture → demodulate_soft → SoftViterbiCodec decode → RS decode → frame decode.
    pub fn receive_with_soft_viterbi_fec(
        &mut self,
        mode: &str,
        device: Option<&str>,
    ) -> Result<Vec<u8>, ModemError> {
        let samples = self.stage_capture_input(Some(mode), device)?;
        let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;

        let llrs = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            let mod_cfg = ModulationConfig {
                mode: mode.to_string(),
                center_frequency: self.center_frequency + self.afc_correction_hz,
                ..ModulationConfig::default()
            };
            openpulse_modem_descramble_soft(plugin.demodulate_soft(&samples.samples, &mod_cfg)?)
        };

        self.update_afc_estimate(mode, &samples.samples);
        self.emit_afc_update(mode);

        let rs_decoded = soft_concat_decode_llrs(&llrs)?;
        let corrected_wire = WirePayload { bytes: rs_decoded };
        let corrected_wire =
            self.route_wire_stage(PipelineStage::DemodulateDecode, corrected_wire)?;

        let frame = self.stage_decode_frame(&corrected_wire)?;
        let frame = self.route_decoded_stage(PipelineStage::HpxStateUpdate, frame)?;
        info!("soft-Viterbi FEC receive: frame seq={}", frame.sequence);
        let _ = self.event_tx.send(EngineEvent::FrameReceived {
            mode: mode.to_string(),
            bytes: frame.payload.len(),
        });
        Ok(frame.payload)
    }

    /// Transmit with RS(255,191) t=32 strong FEC (corrects up to 32 byte errors/block).
    ///
    /// TX chain: frame encode → RS strong encode → modulate → emit.
    pub fn transmit_with_strong_fec(
        &mut self,
        data: &[u8],
        mode: &str,
        device: Option<&str>,
    ) -> Result<(), ModemError> {
        self.csma_check()?;
        let wire = self.stage_encode_frame(data)?;
        let fec_wire = WirePayload {
            bytes: FecCodec::strong().encode(&wire.bytes),
        };
        let fec_wire = self.route_wire_stage(PipelineStage::EncodeModulate, fec_wire)?;
        let samples = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_modulate_payload(plugin, mode, &fec_wire)?
        };
        let samples = self.route_audio_stage(PipelineStage::OutputEmit, samples)?;
        self.stage_emit_output(device, mode, &samples)?;
        let _ = self.event_tx.send(EngineEvent::FrameTransmitted {
            mode: mode.to_string(),
            bytes: fec_wire.bytes.len(),
        });
        Ok(())
    }

    /// Receive with RS(255,191) t=32 strong FEC.
    ///
    /// RX chain: capture → demodulate → RS strong decode → frame decode.
    pub fn receive_with_strong_fec(
        &mut self,
        mode: &str,
        device: Option<&str>,
    ) -> Result<Vec<u8>, ModemError> {
        let samples = self.stage_capture_input(Some(mode), device)?;
        let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;

        let raw_wire = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_demodulate_payload(plugin, mode, &samples)?
        };
        let raw_wire = self.route_wire_stage(PipelineStage::DemodulateDecode, raw_wire)?;

        self.update_afc_estimate(mode, &samples.samples);
        self.emit_afc_update(mode);

        let rs_decoded = FecCodec::strong().decode(&raw_wire.bytes)?;
        let frame = self.stage_decode_frame(&WirePayload { bytes: rs_decoded })?;
        let frame = self.route_decoded_stage(PipelineStage::HpxStateUpdate, frame)?;
        let _ = self.event_tx.send(EngineEvent::FrameReceived {
            mode: mode.to_string(),
            bytes: frame.payload.len(),
        });
        Ok(frame.payload)
    }

    /// Transmit with rate-1/2 LDPC FEC (1024 info bits → 2048 codeword bits, min-sum BP).
    ///
    /// TX chain: frame encode → LDPC encode (128 B → 256 B per block) → modulate → emit.
    ///
    /// The encoded frame is split across as many LDPC blocks as it needs; a `Frame`'s payload length
    /// is a `u8`, so that is at most three.
    pub fn transmit_with_ldpc(
        &mut self,
        data: &[u8],
        mode: &str,
        device: Option<&str>,
    ) -> Result<(), ModemError> {
        self.transmit_with_ldpc_codec(data, mode, &LdpcCodec::new(), device)
    }

    /// Transmit with high-rate LDPC FEC (rate ≈8/9, 1024 info bits → 1152 codeword
    /// bits) for the dense, high-SNR rungs (8PSK / 16QAM / 32APSK).
    ///
    /// TX chain: frame encode → LDPC encode (128 B → 144 B per block) → modulate → emit.
    pub fn transmit_with_ldpc_high_rate(
        &mut self,
        data: &[u8],
        mode: &str,
        device: Option<&str>,
    ) -> Result<(), ModemError> {
        self.transmit_with_ldpc_codec(data, mode, &LdpcCodec::high_rate(), device)
    }

    /// Transmit one frame through the given LDPC codec preset.  Shared by the
    /// rate-1/2 and high-rate public methods.
    ///
    /// The frame is split into `info_bytes()`-sized blocks, each encoded independently and the
    /// codewords concatenated.  A `Frame`'s payload length is a `u8`, so the wire frame never exceeds
    /// 265 bytes and this is at most three blocks.  The final block is zero-padded; `Frame::decode`
    /// reads its own length field, so the padding is discarded on receive.
    fn transmit_with_ldpc_codec(
        &mut self,
        data: &[u8],
        mode: &str,
        codec: &LdpcCodec,
        device: Option<&str>,
    ) -> Result<(), ModemError> {
        self.csma_check()?;

        let frame_wire = self.stage_encode_frame(data)?;
        let codeword = encode_ldpc_blocks(codec, &frame_wire.bytes);
        let fec_wire = WirePayload { bytes: codeword };
        let fec_wire = self.route_wire_stage(PipelineStage::EncodeModulate, fec_wire)?;

        debug!(
            "LDPC transmitting {} B codeword (seq={}, mode={mode})",
            fec_wire.bytes.len(),
            self.sequence.wrapping_sub(1)
        );

        let samples = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_modulate_payload(plugin, mode, &fec_wire)?
        };
        let samples = self.route_audio_stage(PipelineStage::OutputEmit, samples)?;
        self.stage_emit_output(device, mode, &samples)?;
        let _ = self.event_tx.send(EngineEvent::FrameTransmitted {
            mode: mode.to_string(),
            bytes: fec_wire.bytes.len(),
        });
        Ok(())
    }

    /// Receive with rate-1/2 LDPC FEC via min-sum belief propagation.
    ///
    /// RX chain: capture → demodulate_soft → LDPC decode_soft → frame decode.
    pub fn receive_with_ldpc(
        &mut self,
        mode: &str,
        device: Option<&str>,
    ) -> Result<Vec<u8>, ModemError> {
        self.receive_with_ldpc_codec(mode, &LdpcCodec::new(), device)
    }

    /// Receive with high-rate LDPC FEC (rate ≈8/9) for the dense, high-SNR rungs.
    ///
    /// Mirror of [`receive_with_ldpc`](Self::receive_with_ldpc) with the
    /// [`LdpcCodec::high_rate`] preset.
    pub fn receive_with_ldpc_high_rate(
        &mut self,
        mode: &str,
        device: Option<&str>,
    ) -> Result<Vec<u8>, ModemError> {
        self.receive_with_ldpc_codec(mode, &LdpcCodec::high_rate(), device)
    }

    /// Receive one frame through the given LDPC codec preset.  Shared by the
    /// rate-1/2 and high-rate public methods; the LLR slice length comes from the
    /// codec's own `codeword_bytes()`.
    fn receive_with_ldpc_codec(
        &mut self,
        mode: &str,
        codec: &LdpcCodec,
        device: Option<&str>,
    ) -> Result<Vec<u8>, ModemError> {
        let samples = self.stage_capture_input(Some(mode), device)?;
        let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;

        let llrs = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            let mod_cfg = ModulationConfig {
                mode: mode.to_string(),
                center_frequency: self.center_frequency + self.afc_correction_hz,
                ..ModulationConfig::default()
            };
            openpulse_modem_descramble_soft(plugin.demodulate_soft(&samples.samples, &mod_cfg)?)
        };

        self.update_afc_estimate(mode, &samples.samples);
        self.emit_afc_update(mode);

        // LDPC block is codeword_bytes × 8 coded bits; trim any excess LLRs.
        let info_bytes = decode_ldpc_llrs(codec, &llrs)?;

        let corrected_wire = WirePayload { bytes: info_bytes };
        let corrected_wire =
            self.route_wire_stage(PipelineStage::DemodulateDecode, corrected_wire)?;

        let frame = self.stage_decode_frame(&corrected_wire)?;
        let frame = self.route_decoded_stage(PipelineStage::HpxStateUpdate, frame)?;
        info!("LDPC receive: frame seq={}", frame.sequence);
        let _ = self.event_tx.send(EngineEvent::FrameReceived {
            mode: mode.to_string(),
            bytes: frame.payload.len(),
        });
        Ok(frame.payload)
    }

    /// Encode `data` with rate-1/3 PCCC turbo FEC and transmit.
    ///
    /// TX chain: frame encode → turbo encode → modulate → emit.
    pub fn transmit_with_turbo(
        &mut self,
        data: &[u8],
        mode: &str,
        device: Option<&str>,
    ) -> Result<(), ModemError> {
        self.csma_check()?;

        let frame_wire = self.stage_encode_frame(data)?;
        if frame_wire.bytes.len() > TURBO_MAX_INFO_BYTES {
            return Err(ModemError::Frame(format!(
                "turbo: encoded frame {} B exceeds one-block limit of {} B; split payload at call site",
                frame_wire.bytes.len(),
                TURBO_MAX_INFO_BYTES,
            )));
        }
        let codeword = turbo_encode(&frame_wire.bytes)?;
        let fec_wire = WirePayload { bytes: codeword };
        let fec_wire = self.route_wire_stage(PipelineStage::EncodeModulate, fec_wire)?;

        debug!(
            "Turbo transmitting {} B codeword (mode={mode})",
            fec_wire.bytes.len()
        );

        let samples = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_modulate_payload(plugin, mode, &fec_wire)?
        };
        let samples = self.route_audio_stage(PipelineStage::OutputEmit, samples)?;
        self.stage_emit_output(device, mode, &samples)?;
        let _ = self.event_tx.send(EngineEvent::FrameTransmitted {
            mode: mode.to_string(),
            bytes: fec_wire.bytes.len(),
        });
        Ok(())
    }

    /// Receive with rate-1/3 PCCC turbo FEC (Max-Log-MAP BCJR, 8 iterations).
    ///
    /// RX chain: capture → demodulate_soft → turbo decode → frame decode.
    pub fn receive_with_turbo(
        &mut self,
        mode: &str,
        device: Option<&str>,
    ) -> Result<Vec<u8>, ModemError> {
        let samples = self.stage_capture_input(Some(mode), device)?;
        let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;

        let llrs = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            let mod_cfg = ModulationConfig {
                mode: mode.to_string(),
                center_frequency: self.center_frequency + self.afc_correction_hz,
                ..ModulationConfig::default()
            };
            openpulse_modem_descramble_soft(plugin.demodulate_soft(&samples.samples, &mod_cfg)?)
        };

        self.update_afc_estimate(mode, &samples.samples);
        self.emit_afc_update(mode);

        // Timing recovery can yield ±1–2 fewer symbols than transmitted; pad to the
        // next multiple of 3 so turbo_decode_soft's divisibility check always passes.
        // Padded LLRs are 0.0 (maximum uncertainty), which the BCJR decoder handles
        // gracefully — they correspond to the padding bits the encoder added to reach
        // the QPP block size.
        let llrs = if llrs.len() % 3 == 0 {
            llrs
        } else {
            let pad = 3 - (llrs.len() % 3);
            let mut v = llrs;
            v.extend(std::iter::repeat_n(0.0f32, pad));
            v
        };
        let info_bytes = turbo_decode_soft(&llrs)?;

        let corrected_wire = WirePayload { bytes: info_bytes };
        let corrected_wire =
            self.route_wire_stage(PipelineStage::DemodulateDecode, corrected_wire)?;

        let frame = self.stage_decode_frame(&corrected_wire)?;
        let frame = self.route_decoded_stage(PipelineStage::HpxStateUpdate, frame)?;
        info!("Turbo receive: frame seq={}", frame.sequence);
        let _ = self.event_tx.send(EngineEvent::FrameReceived {
            mode: mode.to_string(),
            bytes: frame.payload.len(),
        });
        Ok(frame.payload)
    }

    /// Receive via Memory-ARQ soft combining: capture `n_frames` sample buffers,
    /// average them element-wise, then demodulate and RS-decode the combined signal.
    ///
    /// Combining N identical retransmissions improves effective SNR by ~3 dB per
    /// doubling of N (10 log₁₀ N dB total gain over a single reception).
    ///
    /// Decodes using the standard RS codec (t=16).  For frames transmitted with
    /// [`transmit_with_strong_fec`](Self::transmit_with_strong_fec) use
    /// [`receive_with_strong_fec`](Self::receive_with_strong_fec) instead.
    pub fn receive_with_soft_combining(
        &mut self,
        mode: &str,
        device: Option<&str>,
        n_frames: usize,
    ) -> Result<Vec<u8>, ModemError> {
        if n_frames == 0 {
            return Err(ModemError::Frame(
                "soft combining: n_frames must be ≥ 1".to_string(),
            ));
        }
        let mut combiner = SoftCombiner::new();
        for _ in 0..n_frames {
            let samples = self.stage_capture_input(Some(mode), device)?;
            let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;

            combiner.push(&samples.samples);
        }

        let combined = AudioSamples {
            samples: combiner.combine(),
        };

        self.update_afc_estimate(mode, &combined.samples);
        if let Some(hz) = self.last_afc_offset_hz {
            let _ = self.event_tx.send(EngineEvent::AfcUpdate {
                offset_hz: hz,
                correction_hz: self.afc_correction_hz,
                mode: mode.to_string(),
            });
        }

        let raw_wire = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_demodulate_payload(plugin, mode, &combined)?
        };
        let raw_wire = self.route_wire_stage(PipelineStage::DemodulateDecode, raw_wire)?;

        let rs_decoded = self.rs_decode_free_strengthened(&raw_wire.bytes)?;
        let frame = self.stage_decode_frame(&WirePayload { bytes: rs_decoded })?;
        let frame = self.route_decoded_stage(PipelineStage::HpxStateUpdate, frame)?;
        let _ = self.event_tx.send(EngineEvent::FrameReceived {
            mode: mode.to_string(),
            bytes: frame.payload.len(),
        });
        Ok(frame.payload)
    }

    /// Receive via LLR combining: demodulate each attempt separately, sum the soft LLRs, then
    /// RS decode.
    ///
    /// Calibrated LLRs already carry `1/σ²`, so the sum IS the inverse-noise weighting; the explicit
    /// re-weighting this used to do applied σ⁻² twice and cost ~0.75 dB (removed in PR #686).
    ///
    /// Each attempt is first RS-decoded **on its own**; only if every attempt fails are they combined by
    /// [`combine_llrs_map`] — their plain sum, the exact MAP combine for repeated observations of the same
    /// bits — and the combined LLRs hard-decided and RS-decoded. Success is therefore a strict superset of
    /// plain ARQ retry and of combining alone, neither of which dominates the other on a fading channel.
    ///
    /// A calibrated demodulator's LLRs already carry `1/σ²`, so the sum *is* inverse-noise weighting;
    /// a good attempt dominates a faded one on the strength of its own LLR magnitudes. This used to
    /// re-weight the sum by a `1 / mean(|LLR|)` "noise-variance proxy", which applied `σ⁻²` a second
    /// time and threw away information from the weaker attempts (measured: 0.75 dB of threshold on a
    /// graded 0/−4/−8 dB attempt set).
    ///
    /// This provides ~2–4 dB improvement over equal-weight *sample* combining when
    /// different attempts experience different SNR (e.g., Watterson fading).
    ///
    /// TX chain: `transmit_with_fec` (RS-protected).  For Conv+RS frames use
    /// `receive_with_soft_viterbi_fec` on the combined samples instead.
    pub fn receive_with_llr_combining(
        &mut self,
        mode: &str,
        device: Option<&str>,
        n_frames: usize,
    ) -> Result<Vec<u8>, ModemError> {
        if n_frames == 0 {
            return Err(ModemError::Frame(
                "llr combining: n_frames must be ≥ 1".to_string(),
            ));
        }

        let mod_cfg = ModulationConfig {
            mode: mode.to_string(),
            center_frequency: self.center_frequency + self.afc_correction_hz,
            ..ModulationConfig::default()
        };

        let mut attempts: Vec<Vec<f32>> = Vec::with_capacity(n_frames);

        for i in 0..n_frames {
            let samples = self.stage_capture_input(Some(mode), device)?;
            let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;

            // Update AFC from the first captured frame; no extra clone needed.
            if i == 0 {
                self.update_afc_estimate(mode, &samples.samples);
                if let Some(hz) = self.last_afc_offset_hz {
                    let _ = self.event_tx.send(EngineEvent::AfcUpdate {
                        offset_hz: hz,
                        correction_hz: self.afc_correction_hz,
                        mode: mode.to_string(),
                    });
                }
            }

            let llrs = {
                let plugin = self
                    .plugins
                    .get(mode)
                    .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
                openpulse_modem_descramble_soft(plugin.demodulate_soft(&samples.samples, &mod_cfg)?)
            };

            attempts.push(llrs);
        }

        self.combine_and_decode_llrs(mode, &attempts)
    }

    /// Decode a set of soft-LLR "looks" of the *same* frame via the union of decode-each-alone and the
    /// MAP-combined (summed) LLRs — the audio-free core of [`receive_with_llr_combining`].
    ///
    /// Exposed so an external diversity/HARQ combiner that has already demodulated its branches — e.g. a
    /// frequency-diversity mode that demodulates two band-separated carriers to two calibrated LLR
    /// vectors — can reuse the exact union decode rather than re-capturing audio.
    ///
    /// Combining is not uniformly better than plain retry, and which wins depends on the channel: under a
    /// deep fade the looks carry complementary information and the sum decodes where none does; when one
    /// look is simply clean, summing it with a ruined one can *lose* a frame a bare decode would keep. The
    /// union sidesteps the choice — each look is one RS decode, and success is a strict superset of both.
    /// Only the winning decode runs `HpxStateUpdate` and emits `FrameReceived`; the trials must not move
    /// state.
    pub fn combine_and_decode_llrs(
        &mut self,
        mode: &str,
        attempts: &[Vec<f32>],
    ) -> Result<Vec<u8>, ModemError> {
        if attempts.is_empty() {
            return Err(ModemError::Frame(
                "llr combining: attempts must be ≥ 1".to_string(),
            ));
        }

        let mut decoded = None;
        for llrs in attempts {
            let Ok(wire) = self.route_wire_stage(
                PipelineStage::DemodulateDecode,
                WirePayload {
                    bytes: hard_decide(llrs),
                },
            ) else {
                continue;
            };
            let Ok(rs) = self.rs_decode_free_strengthened(&wire.bytes) else {
                continue;
            };
            if let Ok(frame) = self.stage_decode_frame(&WirePayload { bytes: rs }) {
                decoded = Some(frame);
                break;
            }
        }

        let frame = match decoded {
            Some(frame) => frame,
            None => {
                let attempt_refs: Vec<&[f32]> = attempts.iter().map(|l| l.as_slice()).collect();
                let combined_llrs = combine_llrs_map(&attempt_refs);
                let hard_wire = WirePayload {
                    bytes: hard_decide(&combined_llrs),
                };
                let hard_wire =
                    self.route_wire_stage(PipelineStage::DemodulateDecode, hard_wire)?;
                let rs_decoded = self.rs_decode_free_strengthened(&hard_wire.bytes)?;
                self.stage_decode_frame(&WirePayload { bytes: rs_decoded })?
            }
        };

        let frame = self.route_decoded_stage(PipelineStage::HpxStateUpdate, frame)?;
        let _ = self.event_tx.send(EngineEvent::FrameReceived {
            mode: mode.to_string(),
            bytes: frame.payload.len(),
        });
        Ok(frame.payload)
    }

    /// Receive via Window-ARQ range-limited MAP LLR combining.
    ///
    /// Captures `n_frames` receive attempts, combines soft LLRs only inside
    /// `feedback.ranges` via [`combine_llrs_map_in_ranges`], then takes hard decisions and RS-decodes
    /// the combined protected frame. Outside selected ranges, the first attempt is preserved.
    ///
    /// This path is mode-agnostic and works for any registered plugin that
    /// implements `demodulate_soft`.
    pub fn receive_with_window_arq(
        &mut self,
        mode: &str,
        device: Option<&str>,
        n_frames: usize,
        feedback: &WindowArqFeedback,
    ) -> Result<Vec<u8>, ModemError> {
        if n_frames == 0 {
            return Err(ModemError::Frame(
                "window-arq combining: n_frames must be >= 1".to_string(),
            ));
        }

        let mod_cfg = ModulationConfig {
            mode: mode.to_string(),
            center_frequency: self.center_frequency + self.afc_correction_hz,
            ..ModulationConfig::default()
        };

        let mut attempts: Vec<Vec<f32>> = Vec::with_capacity(n_frames);

        for i in 0..n_frames {
            let samples = self.stage_capture_input(Some(mode), device)?;
            let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;

            if i == 0 {
                self.update_afc_estimate(mode, &samples.samples);
                if let Some(hz) = self.last_afc_offset_hz {
                    let _ = self.event_tx.send(EngineEvent::AfcUpdate {
                        offset_hz: hz,
                        correction_hz: self.afc_correction_hz,
                        mode: mode.to_string(),
                    });
                }
            }

            let llrs = {
                let plugin = self
                    .plugins
                    .get(mode)
                    .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
                openpulse_modem_descramble_soft(plugin.demodulate_soft(&samples.samples, &mod_cfg)?)
            };

            attempts.push(llrs);
        }

        let attempt_refs: Vec<&[f32]> = attempts.iter().map(|l| l.as_slice()).collect();
        let combined_llrs = combine_llrs_map_in_ranges(&attempt_refs, feedback);

        let hard_bytes: Vec<u8> = combined_llrs
            .chunks(8)
            .map(|chunk| {
                chunk.iter().enumerate().fold(0u8, |acc, (i, &llr)| {
                    acc | ((llr.is_sign_negative() as u8) << i)
                })
            })
            .collect();

        // OFDM/SC-FDMA pad the last symbol to a whole subcarrier boundary; the
        // resulting hard_bytes may be a few bytes longer than an exact RS multiple.
        // Trim to the nearest multiple of 255 (RS BLOCK_TOTAL) so FecCodec::decode
        // doesn't reject the buffer.
        const RS_BLOCK: usize = 255;
        let rs_len = (hard_bytes.len() / RS_BLOCK) * RS_BLOCK;
        let mut hard_bytes = hard_bytes;
        if rs_len > 0 && rs_len < hard_bytes.len() {
            hard_bytes.truncate(rs_len);
        }
        let hard_wire = WirePayload { bytes: hard_bytes };
        let hard_wire = self.route_wire_stage(PipelineStage::DemodulateDecode, hard_wire)?;

        let rs_decoded = self.rs_decode_free_strengthened(&hard_wire.bytes)?;
        let frame = self.stage_decode_frame(&WirePayload { bytes: rs_decoded })?;
        let frame = self.route_decoded_stage(PipelineStage::HpxStateUpdate, frame)?;
        let _ = self.event_tx.send(EngineEvent::FrameReceived {
            mode: mode.to_string(),
            bytes: frame.payload.len(),
        });
        Ok(frame.payload)
    }

    /// Build and transmit a selective Window-ARQ retransmit packet.
    ///
    /// The sender provides the original RS-protected frame bytes and the
    /// receiver-provided `feedback` failed ranges. Only failed byte windows are
    /// emitted, reducing retry airtime compared to full-frame retransmit.
    ///
    /// Returns the encoded retransmit packet bytes that were emitted.
    pub fn transmit_window_retransmit_packet(
        &mut self,
        protected_frame: &[u8],
        feedback: &WindowArqFeedback,
        mode: &str,
        device: Option<&str>,
    ) -> Result<Vec<u8>, ModemError> {
        self.csma_check()?;

        let packet = encode_window_retransmit(protected_frame, feedback)?;
        let wire = WirePayload {
            bytes: packet.clone(),
        };
        let wire = self.route_wire_stage(PipelineStage::EncodeModulate, wire)?;

        let samples = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_modulate_payload(plugin, mode, &wire)?
        };
        let samples = self.route_audio_stage(PipelineStage::OutputEmit, samples)?;
        self.stage_emit_output(device, mode, &samples)?;

        let _ = self.event_tx.send(EngineEvent::FrameTransmitted {
            mode: mode.to_string(),
            bytes: wire.bytes.len(),
        });
        Ok(packet)
    }

    /// Receive one selective Window-ARQ retransmit packet for `mode`.
    ///
    /// This method demodulates raw retransmit bytes and does not attempt frame
    /// decode. The returned packet is consumed by
    /// [`receive_with_window_arq_selective`](Self::receive_with_window_arq_selective)
    /// or call-site patch logic.
    pub fn receive_window_retransmit_packet(
        &mut self,
        mode: &str,
        device: Option<&str>,
    ) -> Result<Vec<u8>, ModemError> {
        let samples = self.stage_capture_input(Some(mode), device)?;
        let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;

        let wire = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_demodulate_payload(plugin, mode, &samples)?
        };
        let wire = self.route_wire_stage(PipelineStage::DemodulateDecode, wire)?;

        self.update_afc_estimate(mode, &samples.samples);
        self.emit_afc_update(mode);

        Ok(wire.bytes)
    }

    /// Full selective Window-ARQ receive path.
    ///
    /// Applies `n_packets` retransmit packets to `protected_frame` using
    /// `apply_window_retransmit`, then RS-decodes and frame-decodes the repaired
    /// buffer.
    pub fn receive_with_window_arq_selective(
        &mut self,
        mode: &str,
        device: Option<&str>,
        protected_frame: &mut [u8],
        n_packets: usize,
    ) -> Result<Vec<u8>, ModemError> {
        if n_packets == 0 {
            return Err(ModemError::Frame(
                "window-arq selective: n_packets must be >= 1".to_string(),
            ));
        }

        for _ in 0..n_packets {
            let packet = self.receive_window_retransmit_packet(mode, device)?;
            apply_window_retransmit(protected_frame, &packet)?;
        }

        let rs_decoded = self.rs_decode_free_strengthened(protected_frame)?;
        let frame = self.stage_decode_frame(&WirePayload { bytes: rs_decoded })?;
        let frame = self.route_decoded_stage(PipelineStage::HpxStateUpdate, frame)?;
        let _ = self.event_tx.send(EngineEvent::FrameReceived {
            mode: mode.to_string(),
            bytes: frame.payload.len(),
        });
        Ok(frame.payload)
    }

    /// Transmit one HARQ attempt selected from SNR/fading state.
    ///
    /// Returns the [`HarqDecision`] that was applied for this attempt.
    pub fn transmit_with_harq_attempt(
        &mut self,
        data: &[u8],
        mode: &str,
        snr_db: f32,
        fading_depth_db: f32,
        retry_index: u8,
        device: Option<&str>,
    ) -> Result<HarqDecision, ModemError> {
        let decision =
            self.select_harq_decision_for_mode(mode, snr_db, fading_depth_db, retry_index);
        self.transmit_with_fec_mode(data, mode, decision.fec_mode, device)?;
        Ok(decision)
    }

    /// Receive one HARQ attempt selected from SNR/fading state.
    ///
    /// Returns `(payload, decision)` where `decision` is the FEC/timeout policy
    /// that was applied to decode this attempt.
    pub fn receive_with_harq_attempt(
        &mut self,
        mode: &str,
        snr_db: f32,
        fading_depth_db: f32,
        retry_index: u8,
        device: Option<&str>,
    ) -> Result<(Vec<u8>, HarqDecision), ModemError> {
        let decision =
            self.select_harq_decision_for_mode(mode, snr_db, fading_depth_db, retry_index);
        let payload = self.receive_with_fec_mode(mode, decision.fec_mode, device)?;
        Ok((payload, decision))
    }

    /// Transmit with the codec selected by `fec`.
    ///
    /// This is the single-call dispatch over every `FecMode` variant so callers
    /// can drive FEC selection from the negotiated `FecMode` without a match
    /// statement at every call site.
    ///
    /// `FecMode::None` maps to plain [`transmit`](Self::transmit).
    /// `FecMode::RsInterleaved` and `FecMode::Concatenated` use
    /// [`DEFAULT_INTERLEAVER_DEPTH`].
    /// `FecMode::Ldpc` calls [`transmit_with_ldpc`](Self::transmit_with_ldpc).
    /// `FecMode::ShortRs` is supported for both ACK frames (5-byte fixed) and
    /// data frames (≤ 223 bytes). Data frames are dispatched to
    /// [`transmit_with_short_fec_data`](Self::transmit_with_short_fec_data);
    /// ACK frames should call
    /// [`transmit_ack_with_short_fec`](Self::transmit_ack_with_short_fec) directly.
    pub fn transmit_with_fec_mode(
        &mut self,
        data: &[u8],
        mode: &str,
        fec: FecMode,
        device: Option<&str>,
    ) -> Result<(), ModemError> {
        match fec {
            FecMode::None => self.transmit(data, mode, device),
            // Free strengthening at the canonical dispatch seam: whitening (#1021) removed the
            // artificially-easy zero padding, and the measured cost pushed the small-frame Rs rungs
            // over the t=16 cliff on a fade (QPSK250-D at floor+4 on moderate_f1: 0.08 with Rs,
            // 0.42 with RsStrong — which is free for these sizes). The ARQ path already did this
            // (see `transmit_arq_frames_with`); doing it here covers every caller, and every `Rs`
            // receive arm dual-decodes via `rs_decode_free_strengthened`.
            FecMode::Rs => {
                match openpulse_core::fec::free_rs_strengthening(
                    FecMode::Rs,
                    data.len() + openpulse_core::frame::Frame::WIRE_OVERHEAD,
                ) {
                    FecMode::RsStrong => self.transmit_with_strong_fec(data, mode, device),
                    _ => self.transmit_with_fec(data, mode, device),
                }
            }
            FecMode::RsInterleaved => {
                self.transmit_with_fec_interleaved(data, mode, device, DEFAULT_INTERLEAVER_DEPTH)
            }
            FecMode::Concatenated => self.transmit_with_concatenated_fec(data, mode, device),
            FecMode::ShortRs => self.transmit_with_short_fec_data(data, mode, device),
            FecMode::RsStrong => self.transmit_with_strong_fec(data, mode, device),
            FecMode::SoftConcatenated => self.transmit_with_soft_viterbi_fec(data, mode, device),
            FecMode::Ldpc => self.transmit_with_ldpc(data, mode, device),
            FecMode::LdpcHighRate => self.transmit_with_ldpc_high_rate(data, mode, device),
            FecMode::Turbo => self.transmit_with_turbo(data, mode, device),
        }
    }

    /// Receive with the codec selected by `fec`.
    ///
    /// Mirror of [`transmit_with_fec_mode`](Self::transmit_with_fec_mode).
    /// `FecMode::ShortRs` dispatches to
    /// [`receive_with_short_fec_data`](Self::receive_with_short_fec_data); for
    /// ACK frames call
    /// [`receive_ack_with_short_fec`](Self::receive_ack_with_short_fec) directly.
    pub fn receive_with_fec_mode(
        &mut self,
        mode: &str,
        fec: FecMode,
        device: Option<&str>,
    ) -> Result<Vec<u8>, ModemError> {
        // Warn when a soft-input FEC mode is paired with a plugin that only
        // produces hard-decision ±1.0 LLRs — the decoder gains nothing.
        let is_soft_fec = matches!(
            fec,
            FecMode::SoftConcatenated | FecMode::Ldpc | FecMode::LdpcHighRate | FecMode::Turbo
        );
        if is_soft_fec {
            if let Some(plugin) = self.plugins.get(mode) {
                if !plugin.supports_soft_demod(mode) {
                    tracing::warn!(
                        mode,
                        fec = ?fec,
                        "soft-FEC mode paired with a plugin that provides only hard-decision LLRs; \
                         iteration gain will be zero — consider a plugin that overrides supports_soft_demod()"
                    );
                }
            }
        }
        match fec {
            FecMode::None => self.receive(mode, device),
            FecMode::Rs => self.receive_with_fec(mode, device),
            FecMode::RsInterleaved => {
                self.receive_with_fec_interleaved(mode, device, DEFAULT_INTERLEAVER_DEPTH)
            }
            FecMode::Concatenated => self.receive_with_concatenated_fec(mode, device),
            FecMode::ShortRs => self.receive_with_short_fec_data(mode, device),
            FecMode::RsStrong => self.receive_with_strong_fec(mode, device),
            FecMode::SoftConcatenated => self.receive_with_soft_viterbi_fec(mode, device),
            FecMode::Ldpc => self.receive_with_ldpc(mode, device),
            FecMode::LdpcHighRate => self.receive_with_ldpc_high_rate(mode, device),
            FecMode::Turbo => self.receive_with_turbo(mode, device),
        }
    }

    /// Encode `ack` with ShortFecCodec (5 → 13 bytes) and emit via FSK4-ACK.
    pub fn transmit_ack_with_short_fec(
        &mut self,
        ack: &AckFrame,
        device: Option<&str>,
    ) -> Result<(), ModemError> {
        self.csma_check()?;
        let raw = ack.encode_maybe_authenticated(self.ack_mac_key.as_ref());
        let fec_bytes = ShortFecCodec::new().encode(&raw)?;
        let wire = WirePayload { bytes: fec_bytes };
        let mode = "FSK4-ACK";
        let samples = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_modulate_payload(plugin, mode, &wire)?
        };
        let samples = self.route_audio_stage(PipelineStage::OutputEmit, samples)?;
        self.stage_emit_output(device, mode, &samples)
    }

    /// MFSK16-ACK copies for the sub-floor union ACK — the measured knee (REQ-WSIG-01): K=3 clears ≥0.99
    /// at 3 dB below the MFSK16 data floor; K=2 is marginal (~0.88).
    const MFSK16_ACK_COPIES: usize = 3;
    /// Inter-copy silence between MFSK16-ACK copies. Decorrelates the fades enough that no frequency hop is
    /// needed (measured `hop=0 ≡ hop=500 Hz`), so the ACK stays 500 Hz.
    const MFSK16_ACK_GAP_S: f32 = 0.5;
    /// MFSK16-ACK on-air geometry (40 symbols × 256 samples/symbol at 8 kHz), shared by the K=3 transmit,
    /// the union decode, and the receive-loop throttle so they can't drift.
    const MFSK16_ACK_SPS: usize = 256;
    const MFSK16_ACK_SYMS: usize = 40;
    // REMOVED (#1247): `FSK4_ACK_SEARCH_SAMPLES = 16_000`, a ~2 s "turnaround-jitter bound" on the
    // FSK4-ACK trial decoder. Its premise — "the copy is always near the buffer start" — was an
    // assumption no measurement supported, and IRS turnaround has never been measured on hardware
    // (`docs/dev/ota-hardware-validation.md`). Inside a 4-9 s listen window it made an ACK past 2 s
    // unreachable by every decoder in the path. The scan is resumable now, so it covers the whole
    // window for less total work than the capped rescan did — see `decode_fsk4_ack_in_stream_from`.

    /// Transmit the sub-floor ARQ ACK as [`MFSK16_ACK_COPIES`] time-spaced `MFSK16-ACK` copies in a single
    /// PTT keying (0.5 s silence gaps), for the receiver to union-decode with
    /// [`openpulse_core::ack::decode_ack_from_llr_copies`]. The FSK4-ACK waveform dies far above the MFSK16
    /// data floor, so the sub-floor rung needs this robust return channel (REQ-WSIG-01).
    pub fn transmit_ack_mfsk16_k3(
        &mut self,
        ack: &AckFrame,
        device: Option<&str>,
    ) -> Result<(), ModemError> {
        self.csma_check()?;
        let raw = ack.encode_maybe_authenticated(self.ack_mac_key.as_ref());
        let fec_bytes = ShortFecCodec::new().encode(&raw)?;
        let wire = WirePayload { bytes: fec_bytes };
        // A LEADING short FSK4-ACK copy so a mixed-profile peer that listens FSK4-only still hears the
        // recommendation (else its ACK channel blacks out) and a compatible peer's union-listen returns on
        // it fast in mild conditions. Sent FIRST so it lands inside the 4 s FSK4 window; the K=3 MFSK16
        // copies follow for a deep sub-floor fade where FSK4 dies. The receiver acquires the leading copy
        // via `decode_fsk4_ack_in_stream` (FSK4-ACK has no sync preamble).
        let fsk4 = {
            let plugin = self
                .plugins
                .get("FSK4-ACK")
                .ok_or_else(|| ModemError::PluginNotFound("FSK4-ACK".to_string()))?;
            self.stage_modulate_payload(plugin, "FSK4-ACK", &wire)?
        };
        let mfsk = {
            let plugin = self
                .plugins
                .get("MFSK16-ACK")
                .ok_or_else(|| ModemError::PluginNotFound("MFSK16-ACK".to_string()))?;
            self.stage_modulate_payload(plugin, "MFSK16-ACK", &wire)?
        };
        let gap = (Self::MFSK16_ACK_GAP_S * AudioConfig::default().sample_rate as f32) as usize;
        let mut buf = Vec::with_capacity(
            fsk4.samples.len() + (mfsk.samples.len() + gap) * Self::MFSK16_ACK_COPIES,
        );
        buf.extend_from_slice(&fsk4.samples);
        for _ in 0..Self::MFSK16_ACK_COPIES {
            buf.extend(std::iter::repeat_n(0.0f32, gap));
            buf.extend_from_slice(&mfsk.samples);
        }
        let samples =
            self.route_audio_stage(PipelineStage::OutputEmit, AudioSamples { samples: buf })?;
        self.stage_emit_output(device, "MFSK16-ACK", &samples)
    }

    /// Receive an FSK4 short-FEC ACK, re-capturing until it decodes or `timeout_ms`
    /// elapses. `0` falls back to a single immediate read
    /// ([`receive_ack_with_short_fec`](Self::receive_ack_with_short_fec)).
    pub fn receive_ack_with_short_fec_within(
        &mut self,
        device: Option<&str>,
        timeout_ms: u64,
    ) -> Result<AckFrame, ModemError> {
        if timeout_ms == 0 {
            return self.receive_ack_with_short_fec(device);
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match self.receive_ack_with_short_fec(device) {
                Ok(ack) => return Ok(ack),
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(e);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    /// Demodulate FSK4-ACK, ShortFecCodec decode (13 → 5 bytes), return `AckFrame`.
    pub fn receive_ack_with_short_fec(
        &mut self,
        device: Option<&str>,
    ) -> Result<AckFrame, ModemError> {
        let samples = self.stage_capture_input(None, device)?;
        let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;
        self.decode_fsk4_ack(&samples)
    }

    /// FSK4-ACK demod + ShortFec decode + `AckFrame` parse over an already-captured, already-routed
    /// window. Extracted so the union-listen path can try it on each read without re-capturing.
    fn decode_fsk4_ack(&mut self, samples: &AudioSamples) -> Result<AckFrame, ModemError> {
        let mode = "FSK4-ACK";
        let wire = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_demodulate_payload(plugin, mode, samples)?
        };
        self.update_afc_estimate(mode, &samples.samples);
        self.emit_afc_update(mode);
        let decoded = ShortFecCodec::new().decode(&wire.bytes)?;
        let n = decoded.len();
        let arr: [u8; 5] = decoded.try_into().map_err(|_| {
            ModemError::Frame(format!("ShortFEC ACK decode: expected 5 bytes, got {n}"))
        })?;
        AckFrame::decode_maybe_authenticated(&arr, self.ack_mac_key.as_ref())
            .map_err(|e| ModemError::Frame(format!("AckFrame decode: {e:?}")))
    }

    /// Deterministic sample length of a FSK4-ACK frame (13 ShortFec bytes → fixed symbols → fixed samples),
    /// obtained via the same `stage_modulate_payload` the transmit path uses so it matches exactly.
    fn fsk4_ack_frame_len(&self) -> Option<usize> {
        let plugin = self.plugins.get("FSK4-ACK")?;
        let wire = WirePayload {
            bytes: vec![0u8; 13],
        };
        self.stage_modulate_payload(plugin, "FSK4-ACK", &wire)
            .ok()
            .map(|s| s.samples.len())
    }

    /// Side-effect-free FSK4-ACK decode of exactly one frame-length window (no AFC/event mutation), for the
    /// trial-decode acquisition — mirrors `decode_fsk4_ack` via `stage_demodulate_payload` so it matches the
    /// transmit path. The FSK4-ACK plugin demods `window.len()/sps` symbols, so the window must be one frame.
    fn fsk4_ack_at(&self, window: &[f32]) -> Option<AckFrame> {
        let plugin = self.plugins.get("FSK4-ACK")?;
        let samples = AudioSamples {
            samples: window.to_vec(),
        };
        let wire = self
            .stage_demodulate_payload(plugin, "FSK4-ACK", &samples)
            .ok()?;
        let decoded = ShortFecCodec::new().decode(&wire.bytes).ok()?;
        let arr: [u8; 5] = decoded.as_slice().try_into().ok()?;
        AckFrame::decode_maybe_authenticated(&arr, self.ack_mac_key.as_ref()).ok()
    }

    /// Trial-decode offsets not yet tried, given the buffer length (#1247). Pure, so it can be gated
    /// without an instrument.
    ///
    /// Returns the inclusive grid span `(first, last)` to try, or `None` when nothing new is
    /// scannable. Offsets stay on the `step` grid so a resumed scan tries exactly the positions a
    /// from-scratch scan would, in the same order — coverage AND detection latency are unchanged.
    ///
    /// Mirrors `ScanPlanner`'s never-retry-a-start-position rule for the data path; the ACK path had
    /// a fixed cap instead, which is #1247.
    fn ack_scan_span(
        last_tried: Option<usize>,
        len: usize,
        frame_len: usize,
        step: usize,
    ) -> Option<(usize, usize)> {
        let step = step.max(1);
        let max_start = len.checked_sub(frame_len)?;
        let highest = (max_start / step) * step;
        let first = match last_tried {
            None => 0,
            Some(l) => l.checked_add(step)?,
        };
        (first <= highest).then_some((first, highest))
    }

    /// Acquire a FSK4-ACK frame within a longer capture by trial-decoding a frame-length window at coarse
    /// offsets (CRC-gated). FSK4-ACK has no sync preamble, so the dual-waveform sub-floor ACK's LEADING FSK4
    /// copy can't be isolated by the plain whole-buffer demod; a non-MFSK16 peer needs this to hear it.
    ///
    /// **Resumable (#1247).** `last_tried` is the highest offset already attempted; the scan continues past
    /// it and returns the offset it stopped at, so the caller can resume. Two things this fixes:
    ///
    /// * There is no longer an offset CAP. The old bound was 16 000 samples — 2 s — inside a listen window
    ///   the same function sizes at 4 s, or 9 s when the profile carries the MFSK16 sub-floor rung, which
    ///   `hpx_hf` does. An ACK past 2 s was unreachable by every decoder in the path: the whole-buffer
    ///   decode cannot carry a noisy lead, a normal-rung ACK is transmitted as FSK4 rather than K=3, and
    ///   retries open a fresh buffer. Raising that cap was the obvious fix and the wrong one — the caller
    ///   re-invokes this per frame of buffer growth, so a raised cap multiplies an already-quadratic cost.
    /// * A foreign ACK no longer starves the listen. The scan returns the FIRST CRC-valid offset; when that
    ///   ACK fails the caller's session check, the old scan restarted from zero on the next tick and found
    ///   the SAME foreign ACK again, deterministically, for the whole window. Resuming past it reaches a
    ///   real ACK behind it.
    ///
    /// Safe to resume only because a trial decode is side-effect-free AND its result does not depend on
    /// state that moves during the listen: `fsk4_ack_at` takes `&self`, and the demod config's
    /// `afc_correction_hz` stays put because FSK4-ACK publishes no AFC estimator. That second half is an
    /// inherited property, not a designed one, so it is pinned by
    /// `fsk4_ack_publishes_no_afc_estimator_so_a_resumed_scan_is_equivalent`.
    fn decode_fsk4_ack_in_stream_from(
        &self,
        samples: &[f32],
        last_tried: Option<usize>,
    ) -> (Option<AckFrame>, Option<usize>) {
        let Some(fsk4_len) = self.fsk4_ack_frame_len() else {
            return (None, last_tried);
        };
        let sps = (AudioConfig::default().sample_rate as usize / 100).max(1); // FSK4-ACK is 100 baud
        let step = (sps / 4).max(1);
        let Some((first, last)) = Self::ack_scan_span(last_tried, samples.len(), fsk4_len, step)
        else {
            return (None, last_tried);
        };
        // No energy gate here. #894 added one (`rms >= 0.3 * peak`) because an all-zero window then
        // decoded degenerately past ShortFec+CRC as a valid AckOk. Wire whitening (#1027) removed that
        // case — a silent window demodulates to 13 zero bytes, which descramble to a non-codeword and
        // fail RS — so the gate's justification is gone while its cost is not: `peak` is taken over the
        // WHOLE buffer, so in band noise the threshold lands within a few percent of a true ACK window's
        // RMS and refuses the real frame about 60 % of the time at the ACK channel's operating point.
        // The whitening property this relies on is pinned by `silent_window_is_not_a_valid_short_fec_ack`
        // (a keystream change would otherwise re-open the degenerate case silently).
        let mut off = first;
        while off <= last {
            if let Some(ack) = self.fsk4_ack_at(&samples[off..off + fsk4_len]) {
                return (Some(ack), Some(off));
            }
            off += step;
        }
        (None, Some(last))
    }

    /// Does the active OTA profile include the MFSK16 sub-floor rung? Gates the union-listen ACK path so
    /// profiles without it keep the fast FSK4-only receive (no turnaround regression).
    pub fn ota_profile_has_mfsk16(&self) -> bool {
        self.ota
            .as_ref()
            .is_some_and(|o| o.profile_has_mode("MFSK16"))
    }

    /// ACK-listen deadline for the current rung: the sub-floor K=3 MFSK16-ACK (≈5 s + turnaround) needs a
    /// longer window than a 4 s FSK4 ACK. It is a *maximum* — union-listen returns on the first success, so
    /// a healthy link still returns in ~one FSK4 frame.
    pub fn ota_ack_timeout_ms(&self) -> u64 {
        if self.ota_profile_has_mfsk16() {
            9000
        } else {
            4000
        }
    }

    /// Transmit the OTA ACK in the waveform the rung needs: the K=3 union MFSK16-ACK when the ACK
    /// recommends the MFSK16 sub-floor rung (FSK4 dies there), else the standard FSK4-ACK. Correctness does
    /// NOT depend on the ISS guessing this — the ISS union-listens for both (`receive_ota_ack_within`).
    pub fn transmit_ota_ack(
        &mut self,
        ack: &AckFrame,
        device: Option<&str>,
    ) -> Result<(), ModemError> {
        let mfsk16 = matches!(
            (self.ota.as_ref(), ack.recommended_level),
            (Some(o), Some(level)) if o.mode_for_level(level) == Some("MFSK16")
        );
        if mfsk16 {
            self.transmit_ack_mfsk16_k3(ack, device)
        } else {
            self.transmit_ack_with_short_fec(ack, device)
        }
    }

    /// ISS-side OTA ACK receive. When the profile carries the MFSK16 sub-floor rung, **union-listen**: on
    /// each captured read try the fast FSK4 ACK, and accumulate for the ≈5 s K=3 MFSK16-ACK and try that
    /// too — returning on the first success. This is the ACK-path analogue of `rx_candidates` union-demod:
    /// the ISS cannot know which waveform the IRS chose (the "drop to SL1" recommendation travels in a
    /// waveform the ISS isn't yet expecting), so it must accept either, or the rung desyncs at every SL1
    /// boundary. Without the sub-floor rung the fast FSK4-only path is unchanged (no regression).
    pub fn receive_ota_ack_within(
        &mut self,
        device: Option<&str>,
        timeout_ms: u64,
        expected_session_hash: Option<u16>,
    ) -> Result<AckFrame, ModemError> {
        // Reject an ACK whose session hash isn't the peer's — a co-channel session's ACK is otherwise a
        // full-protocol-validity false-accept (adopts a foreign rate + returns success, dropping the message
        // as delivered). `None` disables the check (in-process tests). Mismatch ⇒ keep listening, not error.
        // When a session ACK-MAC key is set (E7), the keyed decode already rejects any ACK not from this
        // session (a foreign/forged one fails the MAC and never decodes), and the authenticated frame carries
        // no session hash — so the hash filter is bypassed.
        let has_key = self.ack_mac_key.is_some();
        let session_ok = move |ack: &AckFrame| {
            has_key || expected_session_hash.is_none_or(|h| ack.session_hash == h)
        };
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        // Throttle the (expensive) K=3 union decode: only attempt it once `accum` holds a full 3-copy span,
        // and thereafter only after it has grown by another copy. Otherwise a streaming backend that returns
        // a small chunk every 30 ms would re-run up to 4 full MFSK16-ACK demods over the whole growing
        // buffer every tick (~O(n²) over the 9 s window). A loopback returns the whole ACK in one read, so
        // it still crosses the span on the first iteration and decodes immediately.
        let copy_len = Self::MFSK16_ACK_SYMS * Self::MFSK16_ACK_SPS;
        let gap = (Self::MFSK16_ACK_GAP_S * AudioConfig::default().sample_rate as f32) as usize;
        let full_span = Self::MFSK16_ACK_COPIES * copy_len + (Self::MFSK16_ACK_COPIES - 1) * gap;
        let mut next_k3_at = full_span;
        // The dual-waveform ACK leads with a FSK4 copy; acquire it from the accumulated stream (a non-MFSK16
        // peer's only route to the recommendation). Same throttle idea — retry per FSK4-frame of growth.
        let fsk4_len = self.fsk4_ack_frame_len().unwrap_or(4160);
        let mut next_fsk4_at = fsk4_len;
        // Highest FSK4 trial offset already attempted, so the scan never re-tries one (#1247).
        let mut fsk4_scan_from: Option<usize> = None;
        let mut accum: Vec<f32> = Vec::new();
        // Hold ONE capture stream open for the whole window so the ~4.84 s K=3 ACK is captured as CONTIGUOUS
        // audio. Re-opening per read (the old `stage_capture_input` path) discards the audio a cpal backend
        // buffers between reads, hole-punching `accum` so `decode_mfsk16_k3_ack`'s contiguous slot geometry
        // (`onset + k·span`) can never bracket an intact copy — the K=3 ACK would be undecodable on real
        // hardware. The daemon's data-RX tick holds its stream open for exactly this reason. A loopback
        // backend drains its shared buffer atomically, so this stays behaviour-identical in tests.
        let mut stream = self.open_capture_stream(device)?;
        loop {
            match stream.read() {
                Ok(chunk) => {
                    if let Ok(routed) = self.route_audio_stage(
                        PipelineStage::InputCapture,
                        AudioSamples { samples: chunk },
                    ) {
                        if let Ok(ack) = self.decode_fsk4_ack(&routed) {
                            if session_ok(&ack) {
                                return Ok(ack);
                            }
                        }
                        accum.extend_from_slice(&routed.samples);
                        if accum.len() >= next_fsk4_at {
                            next_fsk4_at = accum.len() + fsk4_len;
                            // Resume past offsets already tried (#1247), and keep going within THIS
                            // tick when a hit is rejected: a foreign-session ACK sitting early in the
                            // buffer otherwise wins the scan on every tick and starves the listen.
                            loop {
                                let (found, last) =
                                    self.decode_fsk4_ack_in_stream_from(&accum, fsk4_scan_from);
                                // Only continue while the scan is making progress. `last` always
                                // advances today (a hit returns its own offset), but this loop is
                                // the one place where it not advancing would spin forever instead of
                                // failing — found by sabotaging the resume and watching the test
                                // HANG rather than fail. An invariant worth enforcing, not assuming.
                                let advanced = last > fsk4_scan_from;
                                if last.is_some() {
                                    fsk4_scan_from = last;
                                }
                                match found {
                                    Some(ack) if session_ok(&ack) => return Ok(ack),
                                    Some(_) if advanced => continue,
                                    _ => break,
                                }
                            }
                        }
                        if accum.len() >= next_k3_at {
                            next_k3_at = accum.len() + copy_len;
                            if let Some(ack) = self.decode_mfsk16_k3_ack(&accum) {
                                if session_ok(&ack) {
                                    return Ok(ack);
                                }
                            }
                        }
                    }
                }
                // Transient read error: reopen so the next tick warms a fresh stream (matches the daemon).
                Err(_) => {
                    if let Ok(s) = self.open_capture_stream(device) {
                        stream = s;
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(ModemError::Demodulation(
                    "OTA ACK not received within window".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    /// Union-decode a K=3 MFSK16-ACK from a captured window: **anchor on Costas acquisition** (robust at the
    /// sub-floor's ≤ 7 dB SNR, where broadband RMS onset just triggers on noise), then — since the 3 copies
    /// sit exactly a span apart in one transmission — demodulate the slots at `anchor + k·span` and
    /// `decode_ack_from_llr_copies`. Probing ±2 spans means the anchor can be any of the three copies; the
    /// per-copy CRC gate rejects a slot that locks silence/a gap.
    fn decode_mfsk16_k3_ack(&self, samples: &[f32]) -> Option<AckFrame> {
        let copy_len = Self::MFSK16_ACK_SYMS * Self::MFSK16_ACK_SPS;
        if samples.len() < copy_len {
            return None;
        }
        let fs = AudioConfig::default().sample_rate;
        let gap = (Self::MFSK16_ACK_GAP_S * fs as f32) as usize;
        let span = copy_len + gap;
        let cfg = ModulationConfig {
            mode: "MFSK16-ACK".to_string(),
            center_frequency: self.center_frequency,
            sample_rate: fs,
            ..ModulationConfig::default()
        };
        let plugin = self.plugins.get("MFSK16-ACK")?;
        // One Costas lock over the whole buffer finds the strongest copy; the others are ±span from it.
        let anchor = plugin.acquire_copy_offset(samples, &cfg)? as i64;
        let mut copies: Vec<Vec<f32>> = Vec::new();
        for k in -2..=2i64 {
            let start = anchor + k * span as i64;
            if start < 0 {
                continue;
            }
            let start = start as usize;
            let end = (start + copy_len + gap).min(samples.len());
            if end.saturating_sub(start) < copy_len {
                continue;
            }
            if let Ok(llrs) = plugin
                .demodulate_soft(&samples[start..end], &cfg)
                .map(openpulse_modem_descramble_soft)
            {
                copies.push(llrs);
            }
        }
        if copies.is_empty() {
            return None;
        }
        let refs: Vec<&[f32]> = copies.iter().map(|c| c.as_slice()).collect();
        openpulse_core::ack::decode_ack_from_llr_copies_maybe_auth(&refs, self.ack_mac_key.as_ref())
    }

    /// ECC bytes appended by the ShortRs data-frame codec (t = 16).
    const SHORT_FEC_DATA_ECC_LEN: usize = 32;

    /// Frame envelope (magic + ver + seq + len + CRC) bytes added by
    /// [`stage_encode_frame`]. Mirrors `openpulse_core::frame::Frame::encode`.
    const FRAME_ENVELOPE_LEN: usize = 4 + 1 + 2 + 1 + 2;

    /// Maximum user payload accepted by [`transmit_with_short_fec_data`].
    ///
    /// The on-air buffer is `Frame(payload) + 32 B ECC`, which must fit in
    /// `ShortFecCodec`'s 255-byte block, i.e.
    /// `FRAME_ENVELOPE_LEN + payload + ECC_LEN ≤ 255`.
    const SHORT_FEC_DATA_MAX_PAYLOAD: usize =
        255 - Self::SHORT_FEC_DATA_ECC_LEN - Self::FRAME_ENVELOPE_LEN;

    /// Transmit `payload` using the short-block RS codec.
    ///
    /// The bytes on the wire are `Frame(payload) + 32 B ECC` —
    /// `payload.len() + 42` bytes — instead of the full 255-byte block
    /// produced by [`transmit_with_fec`](Self::transmit_with_fec). Strength is
    /// t = 16 byte errors per frame.
    ///
    /// Maximum payload is
    /// [`SHORT_FEC_DATA_MAX_PAYLOAD`](Self::SHORT_FEC_DATA_MAX_PAYLOAD)
    /// (213 bytes); larger payloads return `ModemError::Frame`.
    ///
    /// The receiver determines the data length from the demodulated byte count,
    /// so this path only round-trips reliably when the modulation plugin emits
    /// the exact number of bytes corresponding to the transmitted frame
    /// (loopback and well-framed half-duplex paths). Paths that pad to a
    /// subcarrier boundary (OFDM/SC-FDMA) are not supported.
    pub fn transmit_with_short_fec_data(
        &mut self,
        payload: &[u8],
        mode: &str,
        device: Option<&str>,
    ) -> Result<(), ModemError> {
        if payload.len() > Self::SHORT_FEC_DATA_MAX_PAYLOAD {
            return Err(ModemError::Frame(format!(
                "ShortRs data frame: payload {} bytes exceeds maximum {}",
                payload.len(),
                Self::SHORT_FEC_DATA_MAX_PAYLOAD
            )));
        }
        self.csma_check()?;

        let frame_wire = self.stage_encode_frame(payload)?;
        let fec_bytes =
            ShortFecCodec::with_ecc_len(Self::SHORT_FEC_DATA_ECC_LEN).encode(&frame_wire.bytes)?;
        let wire = WirePayload { bytes: fec_bytes };
        let wire = self.route_wire_stage(PipelineStage::EncodeModulate, wire)?;

        let samples = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_modulate_payload(plugin, mode, &wire)?
        };
        let samples = self.route_audio_stage(PipelineStage::OutputEmit, samples)?;
        self.stage_emit_output(device, mode, &samples)?;
        let _ = self.event_tx.send(EngineEvent::FrameTransmitted {
            mode: mode.to_string(),
            bytes: wire.bytes.len(),
        });
        Ok(())
    }

    /// Demodulate and decode a frame emitted by
    /// [`transmit_with_short_fec_data`](Self::transmit_with_short_fec_data).
    pub fn receive_with_short_fec_data(
        &mut self,
        mode: &str,
        device: Option<&str>,
    ) -> Result<Vec<u8>, ModemError> {
        let samples = self.stage_capture_input(Some(mode), device)?;
        let samples = self.route_audio_stage(PipelineStage::InputCapture, samples)?;

        let wire = {
            let plugin = self
                .plugins
                .get(mode)
                .ok_or_else(|| ModemError::PluginNotFound(mode.to_string()))?;
            self.stage_demodulate_payload(plugin, mode, &samples)?
        };
        let wire = self.route_wire_stage(PipelineStage::DemodulateDecode, wire)?;

        self.update_afc_estimate(mode, &samples.samples);
        self.emit_afc_update(mode);

        let corrected_bytes =
            ShortFecCodec::with_ecc_len(Self::SHORT_FEC_DATA_ECC_LEN).decode(&wire.bytes)?;
        let corrected_wire = WirePayload {
            bytes: corrected_bytes,
        };
        let frame = self.stage_decode_frame(&corrected_wire)?;
        let frame = self.route_decoded_stage(PipelineStage::HpxStateUpdate, frame)?;
        let _ = self.event_tx.send(EngineEvent::FrameReceived {
            mode: mode.to_string(),
            bytes: frame.payload.len(),
        });
        Ok(frame.payload)
    }

    fn stage_encode_frame(&mut self, data: &[u8]) -> Result<WirePayload, ModemError> {
        let _stage = PipelineStage::EncodeModulate;
        let frame = Frame::new(self.sequence, data.to_vec())
            .map_err(|e| ModemError::Frame(e.to_string()))?;
        self.sequence = self.sequence.wrapping_add(1);
        Ok(WirePayload {
            bytes: frame.encode(),
        })
    }

    fn stage_modulate_payload(
        &self,
        plugin: &dyn openpulse_core::plugin::ModulationPlugin,
        mode: &str,
        wire: &WirePayload,
    ) -> Result<AudioSamples, ModemError> {
        let _stage = PipelineStage::EncodeModulate;
        let mod_cfg = ModulationConfig {
            mode: mode.to_string(),
            center_frequency: self.center_frequency,
            ..ModulationConfig::default()
        };
        let samples = plugin.modulate(&self.wire_for_modulation(wire), &mod_cfg)?;
        Ok(AudioSamples { samples })
    }

    /// The wire bytes exactly as they go on the air: **whitened**.
    ///
    /// Placing the transform after FEC means the padding and the parity are whitened too — and RS
    /// padding producing 6.2 s of unmodulated carrier is precisely the on-air defect this prevents
    /// (#1021). See `openpulse_core::scramble`.
    ///
    /// **Every modulation entry point must take its bytes from here.** There are two, because the
    /// plugin trait exposes two different methods with different return types:
    /// [`stage_modulate_payload`](Self::stage_modulate_payload) (audio, `modulate`) and
    /// [`stage_modulate_payload_iq`](Self::stage_modulate_payload_iq) (baseband, `modulate_iq`).
    /// Until 2026-07-29 the audio one whitened inline under a comment asserting it was "the single
    /// TX seam, so every caller is covered by construction" — which was false: `transmit_iq` reached
    /// `modulate_iq` directly and whitened nothing, while every RECEIVE path un-whitens
    /// unconditionally. An I/Q-transmitted frame therefore decoded to `invalid magic` on a receiver
    /// of the same build (archetype scan 2026-07-29, finding 8). The claim is now true because the
    /// bytes come from one function rather than because a comment says so; the gate that keeps it
    /// true is `tests/iq_decode_round_trip.rs`, which decodes an I/Q transmission end to end.
    fn wire_for_modulation(&self, wire: &WirePayload) -> Vec<u8> {
        openpulse_core::scramble::scrambled(&wire.bytes)
    }

    /// Baseband-I/Q counterpart of [`stage_modulate_payload`](Self::stage_modulate_payload).
    fn stage_modulate_payload_iq(
        &self,
        plugin: &dyn openpulse_core::plugin::ModulationPlugin,
        mode: &str,
        wire: &WirePayload,
    ) -> Result<(Vec<f32>, Vec<f32>), ModemError> {
        let _stage = PipelineStage::EncodeModulate;
        let mod_cfg = ModulationConfig {
            mode: mode.to_string(),
            center_frequency: self.center_frequency,
            ..ModulationConfig::default()
        };
        plugin.modulate_iq(&self.wire_for_modulation(wire), &mod_cfg)
    }

    /// Transmit pre-built baseband audio (e.g. a JS8 beacon frame) through the OutputEmit seam,
    /// **without** the HPX `Frame` envelope. Applies the CSMA channel-busy gate, the OutputEmit-stage
    /// transforms, the regulatory TX-metadata log, and the `frames_transmitted` counter — so a
    /// raw-audio transmission is recorded and channel-gated exactly like every framed path (audit
    /// G-2's IQ gap does not apply here). `mode` is an informational label for the regulatory log
    /// (e.g. `"JS8-NORMAL"`); no plugin lookup is done, so a non-registered waveform is fine.
    pub fn transmit_raw_audio(
        &mut self,
        samples: &[f32],
        mode: &str,
        device: Option<&str>,
    ) -> Result<(), ModemError> {
        self.csma_check()?;
        let audio = AudioSamples {
            samples: samples.to_vec(),
        };
        let audio = self.route_audio_stage(PipelineStage::OutputEmit, audio)?;
        self.stage_emit_output(device, mode, &audio)?;
        self.raw_audio_frames_transmitted = self.raw_audio_frames_transmitted.wrapping_add(1);
        let _ = self.event_tx.send(EngineEvent::FrameTransmitted {
            mode: mode.to_string(),
            bytes: samples.len(),
        });
        Ok(())
    }

    /// Tripwire: number of raw-audio frames emitted via [`transmit_raw_audio`] (stays 0 if the JS8
    /// beacon path never runs on a station).
    pub fn raw_audio_frames_transmitted(&self) -> u64 {
        self.raw_audio_frames_transmitted
    }

    fn stage_emit_output(
        &mut self,
        device: Option<&str>,
        mode: &str,
        samples: &AudioSamples,
    ) -> Result<(), ModemError> {
        let _stage = PipelineStage::OutputEmit;

        let audio_cfg = AudioConfig::default();
        let mut stream = self
            .audio
            .open_output(device.or(self.default_device.as_deref()), &audio_cfg)
            .map_err(|e| ModemError::Audio(e.to_string()))?;

        let atten_linear = 10.0f32.powf(self.tx_attenuation_db / 20.0);
        let mut write_samples: Vec<f32> = if (atten_linear - 1.0).abs() < 1e-6 {
            samples.samples.clone()
        } else {
            samples.samples.iter().map(|s| s * atten_linear).collect()
        };
        // CE-SSB envelope conditioning: only for high-PAPR modes that benefit
        // (multicarrier), and only when enabled. Raises average power at the same
        // peak; a no-op for single-carrier modes. See `cessb_condition_tx`.
        if self.cessb_enabled && Self::cessb_benefits(mode) {
            write_samples = self.cessb_condition_tx(&write_samples);
        }
        let threshold = self.tx_limiter_threshold;
        if threshold > 0.0 {
            tanh_limit(&mut write_samples, threshold);
        }

        self.record_audio(&write_samples); // TX window for the spectrum/waterfall tap
        stream
            .write(&write_samples)
            .map_err(|e| ModemError::Audio(e.to_string()))?;
        stream
            .flush()
            .map_err(|e| ModemError::Audio(e.to_string()))?;

        self.record_tx_frame(mode)?;

        Ok(())
    }

    /// Record one emitted frame in the §97 regulatory TX-metadata log and bump `frames_transmitted`
    /// (which arms the auto-ID timer). Called at **every** emit seam — the audio path
    /// (`stage_emit_output`) and the IQ path (`transmit_iq`) — so no on-air TX escapes the log or the
    /// station-ID accounting.
    fn record_tx_frame(&mut self, mode: &str) -> Result<(), ModemError> {
        self.update_tx_session_callsign();
        let tx_seq = self.sequence.wrapping_sub(1);
        let metadata = TxMetadata::new(&self.callsign, mode, self.max_power_watts, tx_seq);
        self.tx_session_log
            .log_frame(metadata.clone())
            .map_err(|err| ModemError::Configuration(err.to_string()))?;
        self.spill_tx_metadata(&metadata);
        debug!("logged TX metadata: {}", metadata.to_log_line());
        self.frames_transmitted = self.frames_transmitted.wrapping_add(1);
        Ok(())
    }

    fn stage_capture_input(
        &mut self,
        mode: Option<&str>,
        device: Option<&str>,
    ) -> Result<AudioSamples, ModemError> {
        let _stage = PipelineStage::InputCapture;
        let audio_cfg = AudioConfig::default();
        let mut stream = self
            .audio
            .open_input(device.or(self.default_device.as_deref()), &audio_cfg)
            .map_err(|e| ModemError::Audio(e.to_string()))?;

        let samples = stream
            .read()
            .map_err(|e| ModemError::Audio(e.to_string()))?;
        self.record_audio(&samples); // RX window (raw channel audio) for the spectrum/waterfall tap
                                     // Record the mode for the receiver front end; the notch is applied once, at the single
                                     // `PipelineStage::InputCapture` seam in `route_audio_stage`, which every receive path hits.
        self.rx_mode = mode.map(|m| m.to_string());
        Ok(AudioSamples { samples })
    }

    /// Decide and RECORD one correlation-veto query — the single step both acquisition paths share.
    ///
    /// It exists because the two copies drifted twice: #1168 wired the accept/reject counters to the
    /// daemon's phase-2 site after its gate caught the veto deciding invisibly, and the stand-down
    /// latch, its log lines and its counter stayed CLI-only (#1342). It owns the calibration sample,
    /// the effective threshold, the stand-down decision (whose hysteresis lives in `RhoCalibration`),
    /// the engine's announce latch with its two `warn!` lines, and all three counters. A stood-down
    /// settle counts as accepted — `rho_accepted_settles` means "the gate passed this input", which
    /// it did — and also in `rho_stand_down_settles`, which tells a stand-down apart from agreement.
    /// Each caller keeps only its path-specific work (the CLI's rejection log, correction reset and
    /// accepted-position record; the daemon's caller-side rollback).
    fn decide_preamble_veto(&mut self, veto: &PreambleVeto, rho: f32, onset: usize) -> VetoVerdict {
        // REQ-RX-02: every query is a calibration sample — the stream this compares against is the
        // stream it is built from, so it costs no extra correlation.
        self.rho_calibration.push_at(rho, onset);
        let threshold = self.rho_calibration.effective_threshold(veto.rho_threshold);
        let stand_down = self
            .rho_calibration
            .stands_down(veto.rho_threshold, veto.delivered_frame_rho_bound);
        if stand_down != self.rho_stand_down {
            self.rho_stand_down = stand_down;
            if stand_down {
                warn!(
                    "preamble veto STANDING DOWN: derived threshold {threshold:.3} exceeds the \
                     delivered-frame bound {:.3} — no threshold separates this station's noise from \
                     a frame it could decode, so frame start falls back to energy alone",
                    veto.delivered_frame_rho_bound.unwrap_or(f32::NAN)
                );
            } else {
                warn!(
                    "preamble veto re-engaged: derived threshold {threshold:.3} is back under the \
                     delivered-frame bound"
                );
            }
        }
        if stand_down {
            self.rho_stand_down_settles += 1;
            self.rho_accepted_settles += 1;
            VetoVerdict::StoodDown
        } else if rho < threshold {
            self.rho_rejected_settles += 1;
            VetoVerdict::Rejected { threshold }
        } else {
            self.rho_accepted_settles += 1;
            VetoVerdict::Corroborated
        }
    }

    /// Build the correlation veto for `mode`, or `None` if this mode gets the energy-only settle.
    ///
    /// Extracted so [`Self::preamble_veto_active`] can report the same answer the receive path
    /// acts on. A second copy of this predicate would be a copy that can drift, and the property
    /// being pinned — *which* modes have a veto — is exactly the one that changed silently when a
    /// cost limit stopped discarding oversized templates.
    fn build_preamble_veto(&self, mode: &str, sample_rate: u32) -> Option<PreambleVeto> {
        self.plugins
            .get(mode)
            .and_then(|p| {
                p.preamble_template(&ModulationConfig {
                    sample_rate,
                    mode: mode.to_string(),
                    center_frequency: self.center_frequency,
                    ..ModulationConfig::default()
                })
            })
            .filter(|t| !t.samples.is_empty())
            // A template whose constants were derived for a DIFFERENT mode is refused, loudly. The
            // plugin cannot enforce this alone: the previous defence was the raw-sample cap, a cost
            // limit standing in for a correctness property, which vanished when the cap became a
            // post-decimation budget.
            .filter(|t| {
                if t.for_mode == mode {
                    return true;
                }
                warn!(
                    "refusing preamble template for {mode}: constants derived for {} — rho is \
                     normalised, so a threshold and grid measured on one template do not transfer",
                    t.for_mode
                );
                false
            })
            .map(|t| {
                // Occupied bandwidth drives the anti-alias cutoff; fall back to a conservative
                // full-passband figure if the plugin publishes none, which only widens it.
                let bw = self
                    .plugins
                    .get(mode)
                    .and_then(|p| p.occupied_bandwidth_hz(mode))
                    .unwrap_or(sample_rate as f32 / 4.0);
                PreambleVeto::new(t, self.center_frequency, sample_rate as f32, bw)
            })
    }

    /// Whether `mode` gets a preamble-correlation veto, as the receive path would decide it.
    ///
    /// Exists to be pinned. When a capability is gated by a resource limit, the set of modes that
    /// have it grows silently the day the limit moves — and every new member arrives wearing
    /// whatever constants the old members were using. Pinning the membership, not just the
    /// behaviour of current members, is what turns that into a test failure.
    pub fn preamble_veto_active(&self, mode: &str) -> bool {
        self.build_preamble_veto(mode, AudioConfig::default().sample_rate)
            .is_some()
    }

    /// Peak normalised preamble correlation in `window`, searched around the settled correction.
    ///
    /// `None` when the window is too short to hold the template — not a rejection: a candidate that
    /// has not finished buffering has not been measured, and treating "no measurement" as "no
    /// preamble" would gate out every frame that arrives one read at a time.
    ///
    /// The grid step is derived from the template's own coherent bandwidth rather than fixed — see
    /// [`Self::preamble_search_plan`], which owns that derivation and the reason it uses the
    /// template's span rather than its sample count.
    fn preamble_rho(
        &self,
        veto: &PreambleVeto,
        window: &[f32],
        settled_hz: f32,
    ) -> Option<(f32, usize)> {
        let (freqs, bound) = self.preamble_search_plan(veto, window, settled_hz)?;
        let fs = AudioConfig::default().sample_rate as f32;
        match &veto.filter {
            VetoCorrelator::Passband(f) => f
                .search_normalized_over_frequency(window, bound, 0.05, fs, &freqs)
                .map(|(r, _)| (r.rho, r.offset)),
            // The DDC path folds the residual frequency into its mix and searches every onset its
            // decimated window allows, so it takes no separate timing bound.
            VetoCorrelator::Ddc(f) => f
                .search_normalized_over_frequency(window, 0.05, &freqs)
                .map(|(r, _)| (r.rho, r.offset)),
        }
    }

    /// The residual-frequency grid and timing bound for the correlation check.
    ///
    /// The grid step is derived from the template's own coherent bandwidth rather than fixed. A
    /// matched filter's ρ falls off over roughly `1 / (template duration)`, so a step chosen for one
    /// baud rate steps clean over the peak at another and reads noise — the same class of error as a
    /// constant fitted to one artifact.
    ///
    /// **Duration comes from the span, never from the sample count.** `fs / len` is the duration's
    /// reciprocal only while the template is passband; on the DDC arm `len()` is the *decimated*
    /// count, so `fs / len` reads `decim / duration` and the grid comes out `decim`x too coarse.
    /// The engine's own grid is the only consumer of this, and the worst-case residual is half a
    /// step, so the resulting coherent loss is `|sinc(decim / 8)|` — 0.974 at `decim = 1`, 0.637 at
    /// 4, and an exact **null** at 8. It stayed invisible because `MODES_WITH_VETO` has only ever
    /// held BPSK250, whose 992-sample template is passband; the first mode published on the DDC arm
    /// would have activated it. Pinned by `the_grid_step_follows_the_template_span_not_its_decimated_length`.
    fn preamble_search_plan(
        &self,
        veto: &PreambleVeto,
        window: &[f32],
        settled_hz: f32,
    ) -> Option<(Vec<f32>, usize)> {
        let tlen = veto.filter.len();
        if window.len() <= tlen {
            return None;
        }
        let fs = AudioConfig::default().sample_rate as f32;
        // The 0.5 Hz floor bounds the hypothesis count for long templates. It does not bind for any
        // passband mode (BPSK250 steps at 2.016 Hz) and DOES bind for a BPSK31-length template
        // (0.256 Hz), leaving a worst case of |sinc(0.25)| = 0.900 — exactly the quarter-cycle
        // criterion this derivation is built on, so the floor is kept rather than removed.
        let step = (0.25 * fs / veto.filter.input_span() as f32).max(0.5);
        let n = (veto.rho_grid_hz / step).round() as i32;
        let freqs: Vec<f32> = (-n..=n).map(|k| settled_hz + k as f32 * step).collect();
        // The search bound is whatever timing slack the window leaves past the template — about two
        // symbol periods for every mode, since the settle window is the preamble plus one symbol.
        // That is the span `refine_onset` can be wrong over, which is why the micro-sweep exists.
        Some((freqs, window.len() - tlen))
    }

    /// Fast AFC settle over one acquisition window: a one-shot wide-scan
    /// anchor pass (`afc_step = 1.0` sets the correction directly to the
    /// Goertzel peak — iterative passes diverge for carriers at the scan
    /// boundary) followed by 5 fine-tracking passes at `afc_step = 0.7`.
    ///
    /// Saves and restores `afc_step` internally; `afc_correction_hz` is left
    /// at the fine estimate so the caller can accept it or restore its own
    /// saved value.  This is the ONLY place that temporarily mutates the AFC
    /// state for settling — the previous inline copies of this sequence each
    /// hand-rolled the save/restore and had already caused >1000 Hz of
    /// accumulated drift once (review E5).
    fn afc_mini_settle(&mut self, mode: &str, window: &[f32]) -> AfcSettleOutcome {
        self.afc_settle_attempts = self.afc_settle_attempts.wrapping_add(1);
        let saved_step = self.afc_step;
        self.afc_step = 1.0;
        self.afc_correction_hz = 0.0;
        self.update_afc_estimate(mode, window);
        let anchor = self.afc_correction_hz;
        self.afc_step = 0.7;
        let mut prev = anchor;
        for _ in 0..5 {
            prev = self.afc_correction_hz;
            self.update_afc_estimate(mode, window);
        }
        self.afc_step = saved_step;
        let last_delta = (self.afc_correction_hz - prev).abs();
        // Snap a sub-noise-floor correction to zero (see AFC_SETTLE_DEADBAND_HZ):
        // applying a spurious few-tenths-of-a-Hz correction over-corrects a
        // zero-offset frame and breaks 8PSK's preamble phase re-fit.
        if self.afc_correction_hz.abs() < AFC_SETTLE_DEADBAND_HZ {
            self.afc_correction_hz = 0.0;
        }
        AfcSettleOutcome {
            anchor,
            fine: self.afc_correction_hz,
            last_delta,
        }
    }

    /// Absolute RX SNR (dB) for the rate decision. Prefers the active plugin's calibrated
    /// symbol-domain estimate ([`ModulationPlugin::estimate_snr_db`]) — waveform-aware, so it keeps
    /// tracking SNR up the ladder where the constant-modulus M2M4 moment estimator saturates — and
    /// falls back to silence-gated M2M4 when the plugin has no estimator.
    ///
    /// Public so an external harness (e.g. `openpulse-linksim`) can drive a receiver-led ladder with
    /// the *same* SNR the daemon uses, instead of a tx-vs-rx estimator that counts delay spread as
    /// noise (which reads ≈ −8 dB for a 25 dB OFDM signal through moderate_f1).
    pub fn rx_snr_db(&self, mode: &str, samples: &[f32]) -> f32 {
        let fc = self.center_frequency + self.afc_correction_hz;
        let fs = AudioConfig::default().sample_rate as f32;
        if let Some(plugin) = self.plugins.get(mode) {
            let mod_cfg = ModulationConfig {
                mode: mode.to_string(),
                center_frequency: fc,
                afc_correction_hz: self.afc_correction_hz,
                ..ModulationConfig::default()
            };
            if let Some(snr) = plugin.estimate_snr_db(samples, &mod_cfg) {
                return snr;
            }
        }
        openpulse_core::snr_estimate::m2m4_snr_db_gated_from_real(samples, fc, fs)
    }

    /// Emit an `AfcUpdate` for the CURRENT AFC state, unless a scan is suppressing them.
    ///
    /// See `suppress_afc_events`: an attempt whose correction is about to be rolled back has no
    /// state to report, and emitting one per attempt evicts genuine events from the broadcast ring.
    /// A scan that ultimately succeeds emits exactly one, from the caller, once the kept correction
    /// is committed.
    fn emit_afc_update(&self, mode: &str) {
        if self.suppress_afc_events {
            return;
        }
        if let Some(hz) = self.last_afc_offset_hz {
            let _ = self.event_tx.send(EngineEvent::AfcUpdate {
                offset_hz: hz,
                correction_hz: self.afc_correction_hz,
                mode: mode.to_string(),
            });
        }
    }

    fn update_afc_estimate(&mut self, mode: &str, samples: &[f32]) {
        let mod_cfg = ModulationConfig {
            mode: mode.to_string(),
            center_frequency: self.center_frequency + self.afc_correction_hz,
            afc_correction_hz: self.afc_correction_hz,
            ..ModulationConfig::default()
        };
        let estimate = self
            .plugins
            .get(mode)
            .and_then(|p| p.estimate_afc_hz(samples, &mod_cfg));
        self.last_afc_offset_hz = estimate;
        if self.afc_enabled {
            if let Some(offset) = estimate {
                self.afc_correction_hz += self.afc_step * offset;
                debug!(
                    "AFC: raw_estimate={:.1}Hz correction={:.1}Hz total_offset={:.1}Hz",
                    offset,
                    self.afc_correction_hz,
                    offset + self.afc_correction_hz
                );
            }
        }
    }

    fn stage_demodulate_payload(
        &self,
        plugin: &dyn openpulse_core::plugin::ModulationPlugin,
        mode: &str,
        samples: &AudioSamples,
    ) -> Result<WirePayload, ModemError> {
        let _stage = PipelineStage::DemodulateDecode;
        let mod_cfg = ModulationConfig {
            mode: mode.to_string(),
            center_frequency: self.center_frequency + self.afc_correction_hz,
            afc_correction_hz: self.afc_correction_hz,
            ..ModulationConfig::default()
        };
        let mut wire_bytes = plugin.demodulate(&samples.samples, &mod_cfg)?;
        // Un-whiten: the mirror of stage_modulate_payload. Self-inverse, so the same call undoes
        // it. This is the single hard-decision RX seam; the soft (LLR) paths negate instead of
        // XOR-ing, via scramble::descramble_llrs.
        openpulse_core::scramble::scramble(&mut wire_bytes);
        Ok(WirePayload { bytes: wire_bytes })
    }

    fn stage_decode_frame(&self, wire: &WirePayload) -> Result<DecodedFrame, ModemError> {
        let _stage = PipelineStage::DemodulateDecode;
        let frame = Frame::decode(&wire.bytes)?;
        Ok(DecodedFrame {
            sequence: frame.sequence,
            payload: frame.payload,
        })
    }

    /// RS-decode a wire that may carry the freely-strengthened code.
    ///
    /// `transmit_with_fec_mode` upgrades `Rs` to `RsStrong` whenever the stronger code costs no
    /// extra 255-byte block (`free_rs_strengthening`), so every `Rs` receive arm must accept both
    /// parities. **RS(255,191) and RS(255,223) are nested codes** — the strong generator contains
    /// every root of the standard one — so a clean strong codeword decodes under the t=16 codec
    /// with ZERO corrections, returning 32 bytes of strong parity as "data" (measured: the frame
    /// CRC then reads 0x0000). Decode success therefore cannot arbitrate which code is on the
    /// wire; the frame CRC can, and this is the same arbiter the OTA receive candidates already
    /// use. A t=16 candidate is accepted only if its frame validates; otherwise the strong decode
    /// is tried.
    fn rs_decode_free_strengthened(&self, bytes: &[u8]) -> Result<Vec<u8>, ModemError> {
        if let Ok(d) = FecCodec::new().decode(bytes) {
            if self
                .stage_decode_frame(&WirePayload { bytes: d.clone() })
                .is_ok()
            {
                return Ok(d);
            }
        }
        FecCodec::strong().decode(bytes)
    }

    /// `decode_prefix` variant of [`rs_decode_free_strengthened`](Self::rs_decode_free_strengthened)
    /// for the scanning receive, whose input length is a function of the capture window rather than
    /// the frame.
    fn rs_decode_prefix_free_strengthened(&self, bytes: &[u8]) -> Result<Vec<u8>, ModemError> {
        if let Ok(d) = FecCodec::new().decode_prefix(bytes) {
            if self
                .stage_decode_frame(&WirePayload { bytes: d.clone() })
                .is_ok()
            {
                return Ok(d);
            }
        }
        FecCodec::strong().decode_prefix(bytes)
    }

    fn route_wire_stage(
        &mut self,
        stage: PipelineStage,
        payload: WirePayload,
    ) -> Result<WirePayload, ModemError> {
        self.scheduler
            .route_wire(stage, payload)
            .map_err(|e| ModemError::Configuration(e.to_string()))
    }

    fn route_audio_stage(
        &mut self,
        stage: PipelineStage,
        payload: AudioSamples,
    ) -> Result<AudioSamples, ModemError> {
        let routed = self
            .scheduler
            .route_audio(stage, payload)
            .map_err(|e| ModemError::Configuration(e.to_string()))?;
        // The receiver front end lives at this single seam: every capture path funnels its raw
        // samples through `route_audio_stage(InputCapture)` exactly once, so placing front-end
        // transforms here (rather than in any one capture entry function) covers them all by
        // construction. Order: notch (remove interference) → AGC (normalise the cleaned level).
        if stage == PipelineStage::InputCapture && !self.input_prerouted {
            let mut samples = routed.samples;
            // REQ-PHY-02: remove DC bias (SSB audio paths / soundcard offset) before demod.
            // Per-burst mean subtraction is a transient-free high-pass at ~1/burst Hz (≪10 Hz for
            // any real burst): the heterodyne PSK/QAM demods already reject a 0 Hz offset, but the
            // DCD/CSMA energy gate and AGC use mean-square/RMS, which a DC offset inflates — so this
            // de-biases those. A constant shift leaves all AC content (carrier band) bit-identical,
            // so it never perturbs acquisition; on a zero-DC signal (loopback) the mean is ~0 → no-op.
            self.dc_blocks_processed = self.dc_blocks_processed.wrapping_add(1);
            samples = apply_dc_block(samples);
            if self.notch_enabled {
                self.notch_blocks_processed = self.notch_blocks_processed.wrapping_add(1);
                let mode = self.rx_mode.clone();
                samples = self.apply_rx_notch(mode.as_deref(), samples);
                for f in self.notch_bank.active_freqs() {
                    self.notch_freqs_seen.insert((f / 10.0).round() as i32);
                }
            }
            // Carrier detect BEFORE the AGC, on the true (pre-boost) level. The AGC only normalises the
            // level for the demodulator; the squelch/CSMA must see the real channel energy.
            self.update_dcd_at_seam(&samples);
            if self.agc_enabled {
                self.agc_blocks_processed = self.agc_blocks_processed.wrapping_add(1);
                samples = self.apply_rx_agc(samples);
            }
            return Ok(AudioSamples { samples });
        }
        Ok(routed)
    }

    fn route_decoded_stage(
        &mut self,
        stage: PipelineStage,
        payload: DecodedFrame,
    ) -> Result<DecodedFrame, ModemError> {
        self.scheduler
            .route_decoded(stage, payload)
            .map_err(|e| ModemError::Configuration(e.to_string()))
    }
}

fn nonce_from_seq(seq: u16) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..2].copy_from_slice(&seq.to_le_bytes());
    n
}

/// Hard-decide an LLR stream into bytes: negative LLR → bit 1, positive → bit 0, LSB-first per byte —
/// the order every plugin's `demodulate_soft` emits.
fn hard_decide(llrs: &[f32]) -> Vec<u8> {
    llrs.chunks(8)
        .map(|chunk| {
            chunk.iter().enumerate().fold(0u8, |acc, (i, &llr)| {
                acc | ((llr.is_sign_negative() as u8) << i)
            })
        })
        .collect()
}

/// Encode `info` as a sequence of independent LDPC blocks, zero-padding the last one.
fn encode_ldpc_blocks(codec: &LdpcCodec, info: &[u8]) -> Vec<u8> {
    let k = codec.info_bytes();
    let mut out = Vec::with_capacity(info.len().div_ceil(k) * codec.codeword_bytes());
    for chunk in info.chunks(k) {
        let mut block = chunk.to_vec();
        block.resize(k, 0);
        out.extend_from_slice(&codec.encode(&block));
    }
    out
}

/// SoftConcatenated encode: RS-coded bytes → block-interleave → K=7 convolutional inner code.
///
/// The byte interleaver between the outer RS and inner conv is what makes this burst-tolerant: a
/// deep-fade burst that overwhelms the Viterbi produces a run of byte errors, and deinterleaving
/// spreads that run *across RS blocks* so the outer RS corrects it (measured burst-fade FER 0.98→0.20
/// @4 dB, zero AWGN cost). It MUST be undone before RS on every decode path — see
/// [`soft_concat_decode_llrs`].
///
/// Applied only to *multi-block* frames: a single RS block gains nothing (RS corrects its t=16 errors
/// wherever they sit) and interleaving merely reshuffles a threshold frame's SRO/noise errors, which
/// measurably tipped a marginal single-block SRO case. The permutation is length-preserving, so the
/// receiver mirrors the same gate from the Viterbi-decoded length alone.
fn soft_concat_encode(rs_bytes: &[u8]) -> Vec<u8> {
    let payload = if rs_bytes.len() > RS_BLOCK_BYTES {
        Interleaver::new(DEFAULT_INTERLEAVER_DEPTH).interleave(rs_bytes)
    } else {
        rs_bytes.to_vec()
    };
    SoftViterbiCodec.encode(&payload)
}

/// SoftConcatenated decode from soft LLRs: soft-Viterbi → (block-deinterleave for multi-block frames) →
/// RS. Returns the RS-decoded frame-wire bytes. The exact counterpart of [`soft_concat_encode`]; every
/// SoftConcatenated decode site funnels through this so the interleaver can never be applied one-sided.
fn soft_concat_decode_llrs(llrs: &[f32]) -> Result<Vec<u8>, ModemError> {
    let sv = SoftViterbiCodec.decode_soft(llrs)?;
    let deint = if sv.len() > RS_BLOCK_BYTES {
        Interleaver::new(DEFAULT_INTERLEAVER_DEPTH).deinterleave(&sv)
    } else {
        sv
    };
    FecCodec::new().decode(&deint)
}

/// Decode a concatenated LDPC codeword stream back to its information bytes.
///
/// The block count comes from the LLR count: the soft demodulators return exactly the transmitted
/// bit count (their length prefix trims the modulation padding). Trailing LLRs that do not complete a
/// block are dropped — a whole block is the smallest decodable unit.
///
/// Strict: **every** whole codeword must decode. Correct only where the caller knows the frame
/// extent (the single-shot `receive_with_ldpc*` and the HARQ combiner, whose LLR vector is one
/// attempt's worth). The scanning receive must use [`decode_ldpc_llrs_prefix`] instead.
fn decode_ldpc_llrs(codec: &LdpcCodec, llrs: &[f32]) -> Result<Vec<u8>, ModemError> {
    let n_bits = codec.codeword_bytes() * 8;
    if llrs.len() < n_bits {
        return Err(ModemError::Fec(format!(
            "LDPC: {} LLRs is less than one {n_bits}-bit codeword",
            llrs.len()
        )));
    }
    let mut out = Vec::with_capacity((llrs.len() / n_bits) * codec.info_bytes());
    for block in llrs.chunks_exact(n_bits) {
        out.extend_from_slice(&codec.decode_soft(block)?);
    }
    Ok(out)
}

/// LDPC counterpart of [`FecCodec::decode_prefix`] for the SCANNING receive.
///
/// The scanning slice is sized from `frame_plan`'s FEC reserve, not from the frame, so a real
/// capture — which always outlasts the frame — leaves whole codewords of trailing noise past the
/// last real one. [`decode_ldpc_llrs`] aborts the entire frame when any of them fails belief
/// propagation, so `--fec ldpc` failed on every capture longer than the frame and reported "LDPC did
/// not converge": a channel message for a length bug (archetype scan 2026-07-29). Stopping at the
/// first failed codeword keeps the prefix that decoded, which is where the frame is — the frame's
/// own magic/CRC in `stage_decode_frame` is what decides whether enough of it arrived.
fn decode_ldpc_llrs_prefix(codec: &LdpcCodec, llrs: &[f32]) -> Result<Vec<u8>, ModemError> {
    let n_bits = codec.codeword_bytes() * 8;
    if llrs.len() < n_bits {
        return Err(ModemError::Fec(format!(
            "LDPC: {} LLRs is less than one {n_bits}-bit codeword",
            llrs.len()
        )));
    }
    let mut out = Vec::with_capacity((llrs.len() / n_bits) * codec.info_bytes());
    let mut first_err: Option<ModemError> = None;
    for block in llrs.chunks_exact(n_bits) {
        match codec.decode_soft(block) {
            Ok(info) => out.extend_from_slice(&info),
            Err(e) => {
                first_err = Some(e);
                break;
            }
        }
    }
    if out.is_empty() {
        return Err(first_err
            .unwrap_or_else(|| ModemError::Fec("LDPC prefix decode produced nothing".into())));
    }
    Ok(out)
}

/// `RsInterleaved` counterpart of [`FecCodec::decode_prefix`] for the SCANNING receive.
///
/// `Interleaver::deinterleave` derives its permutation from `data.len()`, so handing it the capture
/// window rather than the frame unscrambles with a *different* permutation than the transmitter used
/// and scatters the bytes. That made this arm fail at **every** non-trivial capture length — not just
/// long ones — and `FecCodec::decode_prefix` cannot rescue it, because trimming after the wrong
/// permutation has already run recovers nothing (archetype scan 2026-07-29).
///
/// The transmitter interleaves exactly the RS-coded bytes (a whole number of 255-byte blocks), so
/// trying each block-count prefix *and* deinterleaving that prefix at its own length reproduces the
/// transmit permutation exactly at the right `k`.
fn rs_interleaved_decode_prefix(depth: usize, data: &[u8]) -> Result<Vec<u8>, ModemError> {
    let blocks = data.len() / RS_BLOCK_BYTES;
    if blocks == 0 {
        return Err(ModemError::Fec(format!(
            "FEC data length {} is shorter than one {RS_BLOCK_BYTES}-byte block",
            data.len()
        )));
    }
    let il = Interleaver::new(depth);
    let mut last: Option<ModemError> = None;
    for k in 1..=blocks {
        let deint = il.deinterleave(&data[..k * RS_BLOCK_BYTES]);
        match FecCodec::new().decode(&deint) {
            Ok(out) => return Ok(out),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| ModemError::Fec("RsInterleaved prefix decode failed".into())))
}

fn minimum_trust_for_profile(profile: PolicyProfile) -> ConnectionTrustLevel {
    match profile {
        PolicyProfile::Strict => ConnectionTrustLevel::Verified,
        PolicyProfile::Balanced => ConnectionTrustLevel::PskVerified,
        PolicyProfile::Permissive => ConnectionTrustLevel::Reduced,
    }
}

/// Remove the DC component of a captured burst by subtracting its mean (REQ-PHY-02).
///
/// This is a transient-free high-pass at ≈ `sample_rate / len` Hz — far below 10 Hz for any real
/// burst. A constant offset is the only thing removed, so the carrier-band (AC) content is
/// bit-identical and demodulation/acquisition is unaffected; on a near-zero-DC signal the mean is
/// ~0 and this is a no-op. Its value is de-biasing the mean-square energy the DCD/CSMA gate and the
/// AGC RMS compute, which a soundcard/SSB DC offset would otherwise inflate.
fn apply_dc_block(mut samples: Vec<f32>) -> Vec<f32> {
    let n = samples.len();
    if n == 0 {
        return samples;
    }
    let mean = samples.iter().sum::<f32>() / n as f32;
    if mean != 0.0 {
        for s in samples.iter_mut() {
            *s -= mean;
        }
    }
    samples
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod ack_scan_tests {
    use super::*;

    /// The resumable scan must try exactly the offsets a from-scratch scan would — no repeats, no
    /// gaps — however the buffer grows (#1247).
    ///
    /// Pure, so this gates the property directly rather than through a counter or an exported
    /// accessor. `ScanPlanner` has the same rule for the data path; the ACK path had a fixed cap.
    #[test]
    fn a_resumed_scan_covers_exactly_the_offsets_a_full_scan_would() {
        const FRAME: usize = 4160;
        const STEP: usize = 20;
        // Growth that is uneven, sometimes smaller than one step, and lands off the step grid.
        let lengths = [
            0, 100, 4159, 4160, 4165, 4180, 4181, 8000, 8001, 20_000, 20_003, 60_000, 72_000,
        ];

        let mut last: Option<usize> = None;
        let mut tried: Vec<usize> = Vec::new();
        for len in lengths {
            if let Some((first, hi)) = ModemEngine::ack_scan_span(last, len, FRAME, STEP) {
                let mut off = first;
                while off <= hi {
                    tried.push(off);
                    off += STEP;
                }
                last = Some(hi);
            }
        }

        let final_len = *lengths.last().expect("non-empty");
        let expected: Vec<usize> = (0..=(final_len - FRAME) / STEP).map(|k| k * STEP).collect();
        assert_eq!(
            tried, expected,
            "a resumed scan did not cover the same offsets, in the same order, as one full scan of \
             the final buffer — so resuming changes either coverage or detection latency"
        );

        let mut sorted = tried.clone();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            tried.len(),
            "an offset was tried twice; the whole point of resuming is that it is not"
        );
    }

    /// A buffer shorter than one frame yields nothing, and never panics on the subtraction.
    #[test]
    fn a_buffer_shorter_than_one_frame_yields_no_offsets() {
        assert_eq!(ModemEngine::ack_scan_span(None, 0, 4160, 20), None);
        assert_eq!(ModemEngine::ack_scan_span(None, 4159, 4160, 20), None);
        assert_eq!(
            ModemEngine::ack_scan_span(None, 4160, 4160, 20),
            Some((0, 0))
        );
        // Already past the end: nothing new.
        assert_eq!(ModemEngine::ack_scan_span(Some(0), 4160, 4160, 20), None);
    }

    /// THE PROPERTY THAT MAKES RESUMING EQUIVALENT, pinned because it is INHERITED, not designed.
    ///
    /// A resumed scan is only equivalent to a rescan if a trial decode at a given offset gives the
    /// same answer later in the listen as it would have earlier. `fsk4_ack_at` takes `&self` and
    /// mutates nothing, but its demod config carries `afc_correction_hz` — and the listen loop calls
    /// `decode_fsk4_ack` on every chunk, which feeds `update_afc_estimate`. The correction stays put
    /// only because `Fsk4Plugin` does not override `estimate_afc_hz` and inherits the trait's `None`.
    ///
    /// Give FSK4-ACK an AFC estimator and resumed and from-scratch scans diverge silently. This test
    /// is what makes that a deliberate decision rather than a surprise.
    #[test]
    fn fsk4_ack_publishes_no_afc_estimator_so_a_resumed_scan_is_equivalent() {
        let mut e = ModemEngine::new(Box::new(openpulse_audio::LoopbackBackend::new()));
        e.register_plugin(Box::new(fsk4_plugin::Fsk4Plugin::new()))
            .expect("register");
        let plugin = e.plugins.get("FSK4-ACK").expect("FSK4-ACK registered");
        let cfg = openpulse_core::plugin::ModulationConfig {
            mode: "FSK4-ACK".to_string(),
            ..Default::default()
        };
        let noise: Vec<f32> = (0..8000)
            .map(|i| ((i * 7919) % 211) as f32 / 211.0 - 0.5)
            .collect();
        assert!(
            plugin.estimate_afc_hz(&noise, &cfg).is_none(),
            "FSK4-ACK now publishes an AFC estimate, so `afc_correction_hz` can move DURING an ACK \
             listen — and a resumed trial-decode scan is no longer equivalent to rescanning from \
             zero (#1247). Either thread a config captured at listen start into \
             `decode_fsk4_ack_in_stream_from`, or restore the cap and accept the unreachable region."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bpsk_plugin::BpskPlugin;
    use openpulse_audio::LoopbackBackend;

    fn make_engine() -> ModemEngine {
        let mut engine = ModemEngine::new(Box::new(LoopbackBackend::new()));
        engine
            .register_plugin(Box::new(BpskPlugin::new()))
            .expect("failed to register BPSK plugin");
        engine
    }

    #[test]
    fn transmit_then_receive() {
        let mut engine = make_engine();
        engine.transmit(b"Hello", "BPSK100", None).unwrap();
        let received = engine.receive("BPSK100", None).unwrap();
        assert_eq!(received, b"Hello");
    }

    #[test]
    fn capture_burst_accumulates_fragmented_frame_then_decodes() {
        // Simulate a streaming backend delivering one frame across several tick
        // windows: capture_burst must accumulate (returning None) until the carrier
        // drops, then flush the whole burst so decode_burst recovers the payload.
        let tx_lb = LoopbackBackend::new();
        let mut tx = ModemEngine::new(Box::new(tx_lb.clone_shared()));
        tx.register_plugin(Box::new(BpskPlugin::new())).unwrap();
        tx.transmit(b"burst capture", "BPSK250", None).unwrap();
        let frame = tx_lb.drain_samples();
        assert!(!frame.is_empty());

        let rx_lb = LoopbackBackend::new_split();
        let mut rx = ModemEngine::new(Box::new(rx_lb.clone_shared()));
        rx.register_plugin(Box::new(BpskPlugin::new())).unwrap();

        // Feed the frame in 4 fragments across 4 ticks — each must keep accumulating.
        let chunk = frame.len() / 4 + 1;
        for frag in frame.chunks(chunk) {
            rx_lb.fill_samples(frag);
            assert!(
                rx.capture_burst(None).unwrap().is_none(),
                "mid-burst tick must keep accumulating"
            );
        }
        // A quiet tick (no carrier) flushes the complete burst.
        let burst = rx
            .capture_burst(None)
            .unwrap()
            .expect("carrier drop must flush the accumulated burst");
        assert_eq!(burst.samples.len(), frame.len(), "burst is the whole frame");
        let decoded = rx.decode_burst("BPSK250", &burst).unwrap();
        assert_eq!(&decoded[..b"burst capture".len()], b"burst capture");
    }

    /// #1123 retention purity: recovering non-ladder traffic must leave the HARQ diversity set
    /// untouched.
    ///
    /// This is the gate that pins the fallback's ORDERING, and it is the reason the fallback runs
    /// before the HARQ block rather than after it. The HARQ loop retains this burst's LLRs per soft
    /// candidate whenever the standalone candidates fail; a fallback placed after it would retain
    /// one garbage vector for every station ID, filexfer fragment and QSY frame the station hears —
    /// self-inflicting exactly the stale-LLR contamination the widest-first suffix trial exists to
    /// contain, and needing an "un-retain" mechanism to undo. Returning before the retention push
    /// means there is nothing to undo.
    ///
    /// A unit test rather than an integration test plus a new `pub` accessor: the state it must
    /// observe (`ota_retained_llrs`) is private, and exporting it purely to be probed is the
    /// construct the reachability ratchet exists to refuse.
    #[test]
    fn a_fallback_decode_retains_no_harq_llrs() {
        let lb = LoopbackBackend::new();
        let mut tx = ModemEngine::new(Box::new(lb.clone_shared()));
        tx.register_plugin(Box::new(BpskPlugin::new())).unwrap();
        tx.transmit(b"control frame", "BPSK250", None).unwrap();
        let signal = lb.drain_samples();

        let rx_lb = LoopbackBackend::new();
        let mut rx = ModemEngine::new(Box::new(rx_lb.clone_shared()));
        rx.register_plugin(Box::new(BpskPlugin::new())).unwrap();
        rx.register_plugin(Box::new(ofdm_plugin::OfdmPlugin::new()))
            .unwrap();
        // SL9 is OFDM52-16QAM + SoftConcatenated — a SOFT rung, so the HARQ block would demodulate
        // and retain this burst's LLRs if it ever reached it.
        rx.start_ota_session(SessionProfile::hpx_hf());
        rx.ota_lock_level(SpeedLevel::Sl9);

        let burst = AudioSamples { samples: signal };
        let res = rx
            .ota_decode_burst(&burst, "retention", Some("BPSK250"))
            .expect("must not error");
        assert_eq!(
            res.payload.as_deref(),
            Some(&b"control frame"[..]),
            "precondition: the fallback must recover the control frame"
        );
        assert!(
            rx.ota_retained_llrs.is_empty(),
            "a fallback decode must retain NO HARQ LLRs; retained {:?}",
            rx.ota_retained_llrs.keys().collect::<Vec<_>>()
        );
    }

    /// Audit: the OTA candidate/soft-HARQ decode loop must NOT re-run the InputCapture front-end — its
    /// callers already routed the burst through the seam. With the notch enabled, `ota_decode_burst`
    /// must not advance the notch-processed tripwire (which would prematurely trip auto-QSY).
    #[test]
    fn ota_decode_does_not_rerun_the_input_capture_front_end() {
        let rx_lb = LoopbackBackend::new();
        let mut rx = ModemEngine::new(Box::new(rx_lb.clone_shared()));
        rx.register_plugin(Box::new(BpskPlugin::new())).unwrap();
        rx.enable_notch();
        rx.start_ota_session(SessionProfile::hpx500());

        // Any non-empty burst; the decode result is irrelevant — we assert only that the OTA decode
        // path did not re-apply the front-end (the caller owns that).
        let burst = AudioSamples {
            samples: vec![0.01f32; 4000],
        };
        let before = rx.notch_blocks_processed();
        // `Some(mode)` so the #1123 uncoded fallback runs too: it calls `decode_burst`, which is a
        // second nested front-end user, and this seam assertion is exactly what must cover it.
        let _ = rx.ota_decode_burst(&burst, "sess-ota-notch", Some("BPSK250"));
        assert_eq!(
            rx.notch_blocks_processed(),
            before,
            "ota_decode_and_ack must suppress the InputCapture seam (input_prerouted); a per-candidate \
             re-run advances the notch-persistence counter and can prematurely trip auto-QSY"
        );
    }

    #[test]
    fn accumulate_capture_streams_burst_and_feeds_spectrum_tap() {
        // The daemon owns ONE persistent input stream (a per-tick reopen never warms
        // up cpal) and feeds each read() to accumulate_capture. Verify it accumulates
        // across reads, flushes on carrier drop, decodes, and feeds the spectrum tap.
        let tx_lb = LoopbackBackend::new();
        let mut tx = ModemEngine::new(Box::new(tx_lb.clone_shared()));
        tx.register_plugin(Box::new(BpskPlugin::new())).unwrap();
        tx.transmit(b"streamed burst", "BPSK250", None).unwrap();
        let frame = tx_lb.drain_samples();
        assert!(!frame.is_empty());

        let mut rx = ModemEngine::new(Box::new(LoopbackBackend::new()));
        rx.register_plugin(Box::new(BpskPlugin::new())).unwrap();
        assert!(rx.last_audio().is_empty(), "no audio captured yet");

        let chunk = frame.len() / 4 + 1;
        for frag in frame.chunks(chunk) {
            assert!(
                rx.accumulate_capture(None, frag.to_vec())
                    .unwrap()
                    .is_none(),
                "mid-burst read must keep accumulating"
            );
        }
        assert!(
            !rx.last_audio().is_empty(),
            "accumulate_capture must feed the spectrum/waterfall tap"
        );
        // A silent read (carrier dropped) flushes the complete burst.
        let burst = rx
            .accumulate_capture(None, vec![0.0; 256])
            .unwrap()
            .expect("carrier drop must flush the accumulated burst");
        assert_eq!(burst.samples.len(), frame.len(), "burst is the whole frame");
        let decoded = rx.decode_burst("BPSK250", &burst).unwrap();
        assert_eq!(&decoded[..b"streamed burst".len()], b"streamed burst");
    }

    #[test]
    fn decode_burst_scans_onset_when_frame_not_at_sample_zero() {
        // A DCD-detected hardware burst starts before the true frame onset, so the
        // engine's single-window demod (which settles AFC on the window start)
        // misframes from sample 0. decode_burst must scan onset offsets to recover it.
        let tx_lb = LoopbackBackend::new();
        let mut tx = ModemEngine::new(Box::new(tx_lb.clone_shared()));
        tx.register_plugin(Box::new(BpskPlugin::new())).unwrap();
        tx.transmit(b"onset scan", "BPSK250", None).unwrap();
        let frame = tx_lb.drain_samples();
        assert!(!frame.is_empty());

        let mut rx = ModemEngine::new(Box::new(LoopbackBackend::new()));
        rx.register_plugin(Box::new(BpskPlugin::new())).unwrap();

        // Prepend lead-in (one BPSK250 symbol period = 32 samples × 3) so the frame
        // onset is not at sample 0; decoding only from 0 would fail.
        let mut buf = vec![0.0f32; 32 * 3];
        buf.extend_from_slice(&frame);
        let decoded = rx
            .decode_burst("BPSK250", &AudioSamples { samples: buf })
            .expect("onset scan must recover a frame that does not start at sample 0");
        assert_eq!(&decoded[..b"onset scan".len()], b"onset scan");
    }

    #[test]
    fn last_audio_window_is_populated_for_the_spectrum_tap() {
        // The spectrum/waterfall tap reads last_audio(); it must hold real samples
        // after a transmit (TX window) and after a receive (RX window), not stay
        // empty — otherwise the daemon FFTs silence and the panel is flat.
        let mut engine = make_engine();
        assert!(
            engine.last_audio().is_empty(),
            "no audio captured/emitted yet"
        );
        engine.transmit(b"spectrum", "BPSK100", None).unwrap();
        assert!(
            !engine.last_audio().is_empty(),
            "transmit must populate the spectrum-tap window"
        );
        let _ = engine.receive("BPSK100", None).unwrap();
        assert!(
            !engine.last_audio().is_empty(),
            "receive must populate the spectrum-tap window"
        );
    }

    #[test]
    fn default_device_is_used_as_fallback_without_breaking_loopback() {
        // The default-device fallback (per-call None → engine default) must route
        // through the same open path; LoopbackBackend ignores the device name, so a
        // round-trip with a default device set still succeeds. This guards the
        // `device.or(self.default_device...)` plumbing the real-audio rig relies on.
        let mut engine = make_engine();
        engine.set_default_device(Some("snd-aloop-pcm".into()));
        engine.transmit(b"Hello", "BPSK100", None).unwrap();
        let received = engine.receive("BPSK100", None).unwrap();
        assert_eq!(received, b"Hello");
    }

    #[test]
    fn transmit_then_receive_with_timeout() {
        let mut engine = make_engine();
        engine.transmit(b"Hello", "BPSK100", None).unwrap();
        // Use a generous timeout — this test validates correctness, not speed.
        // AFC settling (6 Goertzel scans) plus the full RS-FEC decode can take
        // several hundred milliseconds in debug builds.
        let received = engine
            .receive_with_timeout("BPSK100", None, Duration::from_secs(30))
            .unwrap();
        assert_eq!(received, b"Hello");
    }

    #[test]
    fn unknown_mode_returns_error() {
        let mut engine = make_engine();
        assert!(engine.transmit(b"x", "UNKNOWN", None).is_err());
    }

    #[test]
    fn default_trust_policy_is_balanced() {
        let engine = make_engine();
        assert_eq!(engine.trust_policy_profile(), PolicyProfile::Balanced);
    }

    #[test]
    fn trust_policy_profile_can_be_updated() {
        let mut engine = make_engine();
        engine.set_trust_policy_profile(PolicyProfile::Strict);
        assert_eq!(engine.trust_policy_profile(), PolicyProfile::Strict);
    }

    #[test]
    fn secure_session_success_enters_active_transfer() {
        let mut engine = make_engine();
        engine.set_trust_policy_profile(PolicyProfile::Balanced);

        let decision = engine
            .begin_secure_session(
                SecureSessionParams {
                    local_minimum_mode: SigningMode::Normal,
                    peer_supported_modes: vec![SigningMode::Normal, SigningMode::Psk],
                    key_trust: PublicKeyTrustLevel::Full,
                    certificate_source: CertificateSource::OutOfBand,
                    psk_validated: false,
                },
                1_000,
            )
            .expect("secure session should start");

        assert_eq!(engine.hpx_state(), HpxState::ActiveTransfer);
        assert_eq!(decision.trust.decision, ConnectionTrustLevel::Verified);
        assert!(engine.active_handshake().is_some());
    }

    #[test]
    fn balanced_profile_rejects_unverified_handshake() {
        let mut engine = make_engine();
        engine.set_trust_policy_profile(PolicyProfile::Balanced);

        let err = engine
            .begin_secure_session(
                SecureSessionParams {
                    local_minimum_mode: SigningMode::Normal,
                    peer_supported_modes: vec![SigningMode::Normal],
                    key_trust: PublicKeyTrustLevel::Unknown,
                    certificate_source: CertificateSource::OutOfBand,
                    psk_validated: false,
                },
                2_000,
            )
            .expect_err("balanced should reject unverified trust");

        assert!(err.to_string().contains("below required 'pskverified'"));
        assert_eq!(engine.hpx_state(), HpxState::Failed);
    }

    #[test]
    fn strict_profile_rejects_psk_verified_but_not_verified() {
        let mut engine = make_engine();
        engine.set_trust_policy_profile(PolicyProfile::Strict);

        let err = engine
            .begin_secure_session(
                SecureSessionParams {
                    local_minimum_mode: SigningMode::Normal,
                    peer_supported_modes: vec![SigningMode::Normal],
                    key_trust: PublicKeyTrustLevel::Marginal,
                    certificate_source: CertificateSource::OverAir,
                    psk_validated: true,
                },
                2_500,
            )
            .expect_err("strict should reject trust below verified");

        assert!(err.to_string().contains("below required 'verified'"));
        assert_eq!(engine.hpx_state(), HpxState::Failed);
    }

    #[test]
    fn transmit_rejected_when_secure_session_not_active_transfer() {
        let mut engine = make_engine();
        engine.hpx_apply_event(HpxEvent::StartSession, 10).unwrap();

        let err = engine.transmit(b"hello", "BPSK100", None).unwrap_err();
        assert!(err
            .to_string()
            .contains("secure session is not in active transfer"));
    }

    #[test]
    fn transmit_allowed_after_secure_handshake() {
        let mut engine = make_engine();
        engine.set_trust_policy_profile(PolicyProfile::Permissive);

        engine
            .begin_secure_session(
                SecureSessionParams {
                    local_minimum_mode: SigningMode::Relaxed,
                    peer_supported_modes: vec![SigningMode::Normal, SigningMode::Relaxed],
                    key_trust: PublicKeyTrustLevel::Marginal,
                    certificate_source: CertificateSource::OutOfBand,
                    psk_validated: false,
                },
                3_000,
            )
            .unwrap();

        assert!(engine.transmit(b"payload", "BPSK100", None).is_ok());
    }

    #[test]
    fn signed_envelope_round_trip_helpers() {
        let engine = make_engine();
        let bytes = engine
            .encode_signed_envelope(
                b"payload",
                SigningMode::Normal,
                "peer-a",
                "key-1",
                &[1, 2, 3, 4],
            )
            .expect("encode envelope");

        let decoded = engine
            .decode_signed_envelope(&bytes)
            .expect("decode envelope");
        assert_eq!(decoded.payload, b"payload");
        assert_eq!(decoded.signature.signer_id, "peer-a");
        assert_eq!(decoded.signature.key_id, "key-1");
    }

    #[test]
    fn energy_gate_never_falls_below_the_absolute_floor() {
        let mut g = EnergyGate::new();
        // Loopback silence well below the absolute floor: always gated, at any history depth.
        for _ in 0..10 {
            assert!(!g.passes(0.000_025));
        }
        // A loopback-level signal passes regardless of history depth.
        assert!(g.passes(0.002));
    }

    /// THE #1021 REGIME: a real receiver's idle floor must be gated out by the **first** window.
    ///
    /// This is the case the removed cold-start fallback could not handle. The measured IC-9700
    /// idle floor is 4.1e-4 — four times `ABS_THRESHOLD` — so while the gate returned that
    /// constant, the first window of pure noise passed, AFC settled on it, and a byte-perfect
    /// frame arriving 82 000 samples later never decoded.
    #[test]
    fn energy_gate_rejects_a_real_idle_floor_from_the_very_first_window() {
        let mut g = EnergyGate::new();
        assert!(
            !g.passes(0.000_41),
            "a real on-air idle floor must not pass the FIRST gate window — that is exactly the \
             #1021 noise settle"
        );
        for _ in 0..40 {
            assert!(!g.passes(0.000_41), "and must keep being rejected");
        }
        // The burst measured over that same floor must still pass, or the fix would be a blindfold.
        assert!(
            g.passes(0.001_97),
            "the measured on-air burst level must still clear the adaptive threshold"
        );
    }

    /// The counterweight: a fixture whose very first window IS the signal must still pass.
    ///
    /// This is why the threshold can be derived from a single window at all. `route_clean` delivers
    /// ≈ 0.36 mean-square (measured), and `MAX_THRESHOLD` clamps the self-derived threshold at
    /// 3.2e-3 — two orders of magnitude below — so buffer-is-the-frame harnesses are untouched.
    #[test]
    fn energy_gate_passes_a_fixture_whose_first_window_is_signal() {
        let mut g = EnergyGate::new();
        assert!(
            g.passes(0.36),
            "a full-scale fixture signal must pass on its own first window, or every \
             buffer-is-the-frame test breaks"
        );
    }

    #[test]
    fn energy_gate_rises_above_elevated_noise_floor() {
        let mut g = EnergyGate::new();
        // On-air QRM floor ≈ 1.5e-3 passes the fixed 1e-4 gate at every
        // position; after the history fills the adaptive threshold must gate
        // it out (threshold clamps at 3.2e-3 ≥ 1.5e-3).
        for _ in 0..EnergyGate::HISTORY {
            g.passes(0.0015);
        }
        assert!(!g.passes(0.0015), "steady QRM floor must be gated out");
        // A genuine signal above the clamped threshold still passes.
        assert!(g.passes(0.0045));
    }

    #[test]
    fn scan_planner_incremental_positions_never_repeat() {
        let mut p = ScanPlanner::new(32, 1056);
        let first: Vec<usize> = p.scan_positions(3000).collect();
        assert_eq!(first.first(), Some(&0));
        // Last position still fits a minimal frame: largest step ≤ 3000−1056.
        assert_eq!(*first.last().unwrap(), 1920);
        p.commit_scan(3000);
        // More audio: the scan resumes at the committed boundary.
        let second: Vec<usize> = p.scan_positions(4000).collect();
        assert_eq!(second.first(), Some(&(3000 - 1056)));
        // Largest 1944 + k·32 that still fits a minimal frame (≤ 2944).
        assert_eq!(*second.last().unwrap(), 2936);
    }

    #[test]
    fn scan_planner_settle_records_first_energy_without_rewind() {
        let mut p = ScanPlanner::new(32, 1056);
        p.commit_scan(50_000);
        p.note_settled(1234);
        // Settling records the first-energy position for the dedicated re-decode,
        // but must NOT rewind the scan — rewinding made the broad scan re-decode
        // the whole buffer each iteration and stalled the loop.
        assert!(p.is_settled());
        assert_eq!(p.first_energy_pos(), Some(1234));
        assert_eq!(p.scan_positions(50_000).next(), Some(50_000 - 1056));
    }

    /// #1040: a condemned anchor must not hand back ground the micro-sweep already tested.
    ///
    /// The sweep tries onsets at `fep + k*(step/2)` for k in 0..SWEEP_OFFSETS, i.e. it proves a
    /// span of `(SWEEP_OFFSETS-1) * step/2` samples — four whole symbols at the default 9 offsets —
    /// undecodable before condemning. `unsettle` used to advance by a single `step`, so three
    /// quarters of every re-anchor re-tested proven ground. Each of those cycles costs
    /// `SETTLE_FAILURE_LIMIT` fully-buffered decodes, which is why the on-air recovery of
    /// 2026-07-30 crawled through 15 condemnations to cover ~480 samples.
    #[test]
    fn scan_planner_reanchors_past_the_span_the_sweep_already_proved() {
        const STEP: usize = 32;
        let mut p = ScanPlanner::new(STEP, 1056);
        p.commit_scan(200_000);

        let anchor = 84_408usize;
        p.note_settled(anchor);
        for _ in 0..ScanPlanner::SETTLE_FAILURE_LIMIT - 1 {
            assert!(
                !p.note_settle_failure(),
                "must not condemn before the limit"
            );
        }
        assert!(p.note_settle_failure(), "the limit must condemn");
        p.unsettle();

        assert!(!p.is_settled(), "condemning must clear the settle");

        // The last onset the sweep actually tried.
        let swept_through = anchor + (ScanPlanner::SWEEP_OFFSETS - 1) * (STEP / 2);
        let resume = p
            .scan_positions(200_000)
            .next()
            .expect("the scan must reopen");
        assert!(
            resume > swept_through,
            "re-anchored at {resume}, but the micro-sweep already proved every onset through \
             {swept_through} undecodable — re-offering that span is the #1040 crawl"
        );
        // ...and it must not overshoot into untested audio, which could skip a real frame onset.
        assert!(
            resume <= swept_through + STEP,
            "re-anchored at {resume}, past the tested span end {swept_through} by more than one \
             symbol: audio beyond the sweep is UNTESTED and may hold the real preamble"
        );
    }

    #[test]
    fn scan_planner_retry_cadence() {
        let mut p = ScanPlanner::new(32, 1056);
        // Before T=12 s: never.
        assert!(!p.retry_due(0, 10_000));
        assert!(!p.retry_due(11, 10_000));
        // Empty buffer: never.
        assert!(!p.retry_due(20, 0));
        // First firing at T>=12 with data.
        assert!(p.retry_due(12, 10_000));
        // Within the 2 s interval: no.
        assert!(!p.retry_due(13, 10_000));
        // After the interval: fires again.
        assert!(p.retry_due(14, 10_000));
        assert!(!p.retry_due(15, 10_000));
        assert!(p.retry_due(16, 10_000));
    }
}

/// The DDC veto arm, built and exercised through the engine's own seams.
///
/// `VetoCorrelator::Ddc` is chosen whenever a template exceeds `MAX_PREAMBLE_CORRELATION_SAMPLES`,
/// and **no test in the workspace had ever built it** — which is how `ddc_mix` carried an
/// unconditional `usize` underflow from `caa7e1ae` until 2026-08-19. Four independent layers hid it:
/// the DDC equivalence tests are `#[ignore]`d; they never construct `DdcMatchedFilter` but
/// reimplement the mixer locally, carrying the same expression; both copies had only ever run in
/// release, where the wrap is exact; and `chain_veto_slow_rung::q1`, which *would* have built the
/// arm, is `#[ignore]`d too.
///
/// **What this pins, exactly:** the engine builds the Ddc arm for an oversized template, honours the
/// post-decimation budget, and its own grid plan plus Ddc dispatch return near-unity ρ for the
/// template's own signal. It pins **wiring and computation, never thresholds** — no shipped mode
/// reaches this arm (the only `preamble_template` impl is BPSK250's at 992 samples), so
/// production-entry behaviour ships with the first mode that publishes a long template. Whether that
/// day has come is guarded by `tests/veto_membership_pin.rs`.
///
/// A unit module rather than an integration test because `build_preamble_veto`, `preamble_rho` and
/// `VetoCorrelator` are all private, and a probe needing private access is a unit test — not an
/// exported accessor, which the reachability ratchet correctly refuses.
#[cfg(test)]
mod ddc_veto_arm {
    use super::*;
    use openpulse_audio::LoopbackBackend;
    use openpulse_core::plugin::{
        ModulationConfig, ModulationPlugin, PluginInfo, PreambleTemplate,
    };

    const MODE: &str = "LONGTMPL";
    const FS: u32 = 8_000;
    const FC: f32 = 1_500.0;
    /// Longer than `MAX_PREAMBLE_CORRELATION_SAMPLES`, so the Ddc arm is the only possible choice.
    /// Sized like BPSK31's real template (31 symbols x 256 sps) so the decimation maths is the same.
    const TEMPLATE_SAMPLES: usize = 7_936;

    /// A stub whose only real methods are the ones the veto path calls.
    ///
    /// Its ρ constants are **arbitrary** and `for_mode` is stamped to satisfy the engine's
    /// derived-for check (`#1053`). That is legitimate here because nothing below asserts a
    /// threshold — publishing invented constants to test a *threshold* would be the exact defect
    /// that check exists to prevent.
    struct LongTemplatePlugin(PluginInfo);

    impl LongTemplatePlugin {
        fn new() -> Self {
            Self(PluginInfo {
                name: "LONGTMPL".into(),
                version: "0.1.0".into(),
                description: "test-only plugin publishing an oversized preamble template".into(),
                author: "tests".into(),
                supported_modes: vec![MODE.into()],
                trait_version_required: "3.0".into(),
            })
        }

        /// A tone burst at the carrier: the correlation is against this exact signal, so its shape
        /// only has to be in-band and non-degenerate.
        fn template_samples(config: &ModulationConfig) -> Vec<f32> {
            let fs = config.sample_rate as f32;
            (0..TEMPLATE_SAMPLES)
                .map(|n| {
                    let t = n as f32 / fs;
                    (2.0 * std::f32::consts::PI * config.center_frequency * t).sin() * 0.5
                })
                .collect()
        }
    }

    impl ModulationPlugin for LongTemplatePlugin {
        fn info(&self) -> &PluginInfo {
            &self.0
        }
        fn modulate(&self, _d: &[u8], _c: &ModulationConfig) -> Result<Vec<f32>, ModemError> {
            Err(ModemError::Modulation("stub: never modulates".into()))
        }
        fn demodulate(&self, _s: &[f32], _c: &ModulationConfig) -> Result<Vec<u8>, ModemError> {
            Err(ModemError::Demodulation("stub: never demodulates".into()))
        }
        fn supports_mode(&self, mode: &str) -> bool {
            mode == MODE
        }
        fn occupied_bandwidth_hz(&self, _mode: &str) -> Option<f32> {
            Some(200.0)
        }
        fn preamble_template(&self, config: &ModulationConfig) -> Option<PreambleTemplate> {
            Some(PreambleTemplate::new(
                MODE,
                Self::template_samples(config),
                0.40,
                20.0,
            ))
        }
    }

    fn engine_with_long_template() -> ModemEngine {
        let mut e = ModemEngine::new(Box::new(LoopbackBackend::new()));
        e.register_plugin(Box::new(LongTemplatePlugin::new()))
            .expect("register stub");
        e
    }

    /// Construction alone is the regression gate: pre-fix, `DdcMatchedFilter::new` runs `ddc_mix`
    /// over the template and panics in the dev profile before any search happens.
    #[test]
    fn an_oversized_template_builds_the_ddc_arm_within_the_budget() {
        let e = engine_with_long_template();
        let veto = e
            .build_preamble_veto(MODE, FS)
            .expect("an oversized template must still yield a veto — via the Ddc arm");

        let decimated = match &veto.filter {
            VetoCorrelator::Ddc(f) => f.len(),
            VetoCorrelator::Passband(_) => panic!(
                "a {TEMPLATE_SAMPLES}-sample template took the Passband arm;                  MAX_PREAMBLE_CORRELATION_SAMPLES is {MAX_PREAMBLE_CORRELATION_SAMPLES}, so either                  the budget moved or the arm selection did"
            ),
        };
        assert!(
            decimated <= MAX_PREAMBLE_CORRELATION_SAMPLES,
            "the point of the Ddc arm is that the budget is honoured AFTER decimation, and              {decimated} exceeds {MAX_PREAMBLE_CORRELATION_SAMPLES}"
        );
    }

    /// Stronger than "the counters moved": the arm must compute a CORRECT ρ through the engine's own
    /// grid plan and dispatch, for the template's own signal.
    #[test]
    fn the_ddc_arm_correlates_its_own_template_through_the_engine() {
        let e = engine_with_long_template();
        let veto = e.build_preamble_veto(MODE, FS).expect("veto");
        let cfg = ModulationConfig {
            sample_rate: FS,
            mode: MODE.into(),
            center_frequency: FC,
            ..ModulationConfig::default()
        };
        let template = LongTemplatePlugin::template_samples(&cfg);

        // WINDOW SIZE IS A TRAP. `preamble_search_plan` compares the window against the DECIMATED
        // template length, but the DDC needs the window to survive decimation AND the filter's
        // group delay: roughly `tlen * decim + ntap`. A window sized just above the decimated
        // length passes the plan, then every grid frequency is skipped inside the search and the
        // result is a silent `None` — "not measured" wearing the costume of "scored low". Sized
        // well above the passband requirement, and a `None` fails loudly below.
        let mut window = vec![0.0f32; 512];
        window.extend_from_slice(&template);
        window.extend(std::iter::repeat_n(0.0f32, 2_048));

        let (rho, _offset) = e
            .preamble_rho(&veto, &window, 0.0)
            .expect("the Ddc arm returned None: it did not measure this window at all");
        assert!(
            rho >= 0.9,
            "the Ddc correlator scored {rho:.3} against its OWN template; anything short of              near-unity means the mix, decimation or normalisation is wrong, not that the signal is"
        );
    }

    /// THE GATE: no residual inside the grid's reach may cost more coherence than the quarter-cycle
    /// criterion the grid is derived from.
    ///
    /// `preamble_search_plan` sizes its step from the template's coherent bandwidth, and the
    /// expression for that was `0.25 * fs / veto.filter.len()`. That is `1 / (4 * duration)` only
    /// while the template is passband; on the Ddc arm `len()` is the **decimated** count, so it read
    /// `decim / (4 * duration)` and the grid came out `decim`x too coarse. The worst-case residual is
    /// half a step, so the loss is `|sinc(decim / 8)|` — 0.974 at 1, 0.900 at 2, **0.637 at 4**, and
    /// an exact null at 8, which is the D a PN-63 preamble at this length would take.
    ///
    /// Invisible until now because the defect needs a template on the Ddc arm and
    /// `tests/veto_membership_pin.rs` pins `MODES_WITH_VETO = ["BPSK250"]`, whose 992 samples are
    /// passband (`decim = 1`, loss 0.974). The two things that looked like cover are not:
    /// `ddc_correlation_equivalence::p1` compares the arms with `search_normalized`, over onsets at a
    /// SINGLE frequency hypothesis, so "decimating it does not change the answer" is silent about
    /// the grid; and the sibling test above scores at residual 0.0, where every grid contains the
    /// answer exactly.
    ///
    /// **Swept rather than sampled at one residual.** A single worst-case point for the old grid is
    /// also a near-best point for the new one (0.512 Hz sits mid-bin at the old 1.0246 Hz step and
    /// 0.012 Hz from a hypothesis at 0.5), so a point test would overstate the fix by measuring two
    /// different places. The minimum over a full step is the property the derivation actually
    /// promises. The old step is 1.0246 Hz rather than `0.25 * 8000 / (7936 / 4)` = 1.008 because
    /// `ddc_mix` keeps only the anti-alias FIR's valid region: the decimated template is 1952
    /// samples, spanning 7808 input samples rather than 7936.
    ///
    /// The offset is applied by building the template at `fc + delta` — the repo's idiom
    /// (`demod_parity.rs`, `carrier_offset_matrix.rs`, `ChannelSimHarness::route_with_cfo`).
    /// Multiplying a finished frame by `cos(2*pi*delta*t)` is NOT this: it is amplitude modulation,
    /// putting half-amplitude copies at `fc +/- delta`, which caps rho near `1/sqrt(2)` and makes it
    /// nearly grid-INDEPENDENT — so it cannot measure a grid at all. That mistake is live in
    /// `bpsk31_constant_derivation::r2` (#1062).
    #[test]
    fn the_grid_step_follows_the_template_span_not_its_decimated_length() {
        let e = engine_with_long_template();
        let veto = e.build_preamble_veto(MODE, FS).expect("veto");

        // Structural half, so a failure localises to the derivation rather than to the correlator.
        let (freqs, _) = e
            .preamble_search_plan(&veto, &vec![0.0f32; 32_768], 0.0)
            .expect("plan");
        let step = freqs
            .windows(2)
            .map(|w| w[1] - w[0])
            .fold(f32::INFINITY, f32::min);
        // `by_span` uses the raw template length, while the correlator's true span is a little
        // shorter — `ddc_mix` keeps only the anti-alias FIR's valid region, 7808 of 7936 samples
        // here. So the comparison carries a tolerance, sized to sit above that trim (~1.6 %) and far
        // below the smallest defect it exists for (a factor of `decim` >= 2). An exact comparison
        // passes today only because the 0.5 Hz floor binds both sides, and would fail on CORRECT
        // code the day the floor is removed.
        let by_span = (0.25 * FS as f32 / TEMPLATE_SAMPLES as f32).max(0.5);
        assert!(
            step <= 1.25 * by_span,
            "grid step {step:.4} Hz exceeds the {by_span:.4} Hz this template's time span allows by \
             more than the 25 % FIR-trim tolerance — it is being derived from the decimated length, \
             which is shorter by the decimation factor and so overstates the resolution by it"
        );

        // Behavioural half: sweep one full step of residual and take the worst.
        let mut worst = f32::INFINITY;
        let mut worst_at = 0.0f32;
        let mut delta = 0.0f32;
        while delta <= 0.55 {
            let offset_cfg = ModulationConfig {
                sample_rate: FS,
                mode: MODE.into(),
                center_frequency: FC + delta,
                ..ModulationConfig::default()
            };
            // Sized to the sibling test's rule — above `tlen * decim + ntap` = 7936 + 129 — but no
            // further: every extra sample is another lag at each of the 81 grid frequencies, and
            // this sweep pays that twelve times.
            let mut window = vec![0.0f32; 256];
            window.extend_from_slice(&LongTemplatePlugin::template_samples(&offset_cfg));
            window.extend(std::iter::repeat_n(0.0f32, 512));
            let (rho, _) = e
                .preamble_rho(&veto, &window, 0.0)
                .expect("the Ddc arm did not measure this window");
            if rho < worst {
                worst = rho;
                worst_at = delta;
            }
            delta += 0.05;
        }

        // |sinc(0.25)| = 0.900 is what a step at the 0.5 Hz floor promises; the pre-fix grid stepped
        // at 1.0246 Hz and bottoms at |sinc(0.5)| = 0.637 at its 0.512 Hz mid-bin.
        assert!(
            worst >= 0.85,
            "worst rho over one grid step is {worst:.3} at a {worst_at:.2} Hz residual. The              quarter-cycle criterion this grid is derived from promises 0.900; 0.637 is what a grid              {decim}x too coarse gives, and a real frame that far off frequency would be vetoed as              noise.",
            decim = TEMPLATE_SAMPLES / veto.filter.len()
        );
    }
}

/// A veto stand-down is RECORDED on every path that decides the veto (#1342).
///
/// **A recording gate, not evidence that a real station stands down on the daemon path.** It primes
/// the calibration directly — 64 samples at ρ 0.40 give an anchor of 0.40, so the derived level is
/// 0.40 × 1.8 = 0.72, above BPSK250's delivered-frame bound of 0.50, and every query stands down —
/// which skips how a station's median would get there. How often a real station does is unmeasured
/// (#1342). Priming needs the private `rho_calibration`, which is why this is an in-crate unit test
/// rather than an exported accessor.
///
/// **The fixture is copied, not shared by reference**: a lib unit test cannot import the integration
/// test's helpers. The geometry constants — mode, FEC, the 200 Hz `CfoChannel` shift that stops phase 1
/// decoding so phase 2 must run, the 8000-sample silence either side, the work budget and the daemon's
/// tick — are `tests/daemon_runs_acquisition_chain.rs`'s; the payload differs. Fidelity to that
/// fixture is not load-bearing: the arms carry their own executable pins — `prime`'s derived level
/// above the bound, `afc_settle_attempts() > 0`, and the counters.
///
/// Three arms. The CLI arm is the control for the instrument: that site wrote the latch and the
/// counter before #1342, and the arm passes with the daemon site's pre-fix inline code restored
/// (sabotage A), proving the fixture reaches a stand-down at all. The two daemon arms, one per
/// `server::run` decode arm, fail with that code restored. With every query standing down, each must
/// count as a stand-down AND as an accept and none as a reject, which pins the shared counting rule
/// as well as the latch.
#[cfg(test)]
mod stand_down_is_recorded_on_every_path {
    use super::*;
    use bpsk_plugin::BpskPlugin;
    use openpulse_audio::LoopbackBackend;
    use openpulse_channel::ChannelModel;
    use openpulse_core::fec::FecMode;
    use openpulse_core::profile::SessionProfile;
    use std::time::Duration;

    const MODE: &str = "BPSK250";
    const FEC: FecMode = FecMode::Rs;
    const PAYLOAD: &[u8] = b"stand-down recording probe";
    const SAMPLE_RATE: u64 = 8_000;
    const LEAD_SAMPLES: usize = 8_000;
    const OFFSET_HZ: f32 = 200.0;
    const SCAN_POSITIONS: usize = 3_000;
    const MAX_ITERATIONS: usize = 400;
    const PRIME_RHO: f32 = 0.40;
    const PRIME_SAMPLES: usize = 64;

    fn engine() -> (LoopbackBackend, ModemEngine) {
        let backend = LoopbackBackend::new();
        let mut e = ModemEngine::new(Box::new(backend.clone_shared()));
        e.register_plugin(Box::new(BpskPlugin::new()))
            .expect("register bpsk");
        e.set_deterministic_scan_positions(Some(SCAN_POSITIONS));
        e.set_deterministic_max_iterations(Some(MAX_ITERATIONS));
        (backend, e)
    }

    /// Prime the calibration so the veto must stand down, and ASSERT that it must — otherwise the
    /// arms below would measure something other than a stand-down.
    fn prime(e: &mut ModemEngine) {
        for _ in 0..PRIME_SAMPLES {
            e.rho_calibration.push(PRIME_RHO);
        }
        assert_eq!(
            e.rho_calibration.anchor(),
            Some(PRIME_RHO),
            "priming did not reach the calibration's minimum sample count"
        );
        let derived = e
            .rho_calibration
            .effective_threshold(bpsk_plugin::modulate::PREAMBLE_RHO_THRESHOLD);
        assert!(
            derived > bpsk_plugin::modulate::DELIVERED_FRAME_RHO_BOUND,
            "primed derived level {derived:.3} is not above the delivered-frame bound, so the veto \
             would not stand down and this fixture would not exercise the stand-down at all"
        );
    }

    fn signal_with_frame() -> Vec<f32> {
        let (backend, mut e) = engine();
        e.transmit_with_fec_mode(PAYLOAD, MODE, FEC, None)
            .expect("transmit");
        let tx = backend.drain_samples();
        let mut cfo = openpulse_channel::cfo::CfoChannel::new(
            openpulse_channel::cfo::CfoConfig::new(OFFSET_HZ, SAMPLE_RATE as f32),
        )
        .expect("finite offset");
        let shifted = cfo.apply(&tx);
        let mut out = vec![0.0f32; LEAD_SAMPLES];
        out.extend_from_slice(&shifted);
        out.extend(std::iter::repeat_n(0.0f32, LEAD_SAMPLES));
        out
    }

    /// Feed the signal the way `server::run`'s receive tick does, then silence so the burst flushes.
    fn capture_via_daemon_path(e: &mut ModemEngine, signal: &[f32]) -> AudioSamples {
        let tick_ms = openpulse_config::DaemonConfig::default().receive_tick_ms;
        let tick = (SAMPLE_RATE * tick_ms / 1_000) as usize;
        let mut flushed = None;
        for chunk in signal.chunks(tick) {
            if let Ok(Some(b)) = e.accumulate_capture(Some(MODE), chunk.to_vec()) {
                flushed = Some(b);
            }
        }
        for _ in 0..6 {
            if let Ok(Some(b)) = e.accumulate_capture(Some(MODE), vec![0.0f32; tick]) {
                flushed = Some(b);
            }
        }
        flushed.expect("the carrier rose and dropped, so a burst must flush")
    }

    fn assert_recorded(arm: &str, e: &ModemEngine) {
        assert!(
            e.afc_settle_attempts() > 0,
            "{arm}: the AFC settle never ran (settle_attempts=0) — on a daemon arm, phase 2 was not \
             reached — so the veto was never asked and nothing \
             below means anything"
        );
        let (latched, stood_down) = e.rho_stand_down();
        assert!(
            latched,
            "{arm}: the veto stood down (derived 0.72 > bound 0.50) but the engine's announce latch \
             reads false — the stand-down is neither reported nor logged on this path (#1342)"
        );
        assert!(
            stood_down >= 1
                && stood_down == e.rho_accepted_settles()
                && e.rho_rejected_settles() == 0,
            "{arm}: with every query standing down, each must count as a stand-down AND as an \
             accept, and none as a reject; got stand_down_settles={stood_down} accepted={} \
             rejected={}",
            e.rho_accepted_settles(),
            e.rho_rejected_settles()
        );
    }

    /// CONTROL: the CLI path recorded stand-downs before #1342, so this proves the fixture reaches one.
    #[test]
    fn the_cli_path_records_a_stand_down() {
        let signal = signal_with_frame();
        let (backend, mut e) = engine();
        prime(&mut e);
        backend.fill_samples(&signal);
        let _ = e.receive_with_fec_mode_timeout(MODE, FEC, None, Duration::from_millis(20_000));
        assert_recorded("CLI receive", &e);
    }

    /// The default daemon's decode arm (`ota_enabled` off): `server::run` calls `decode_burst`.
    #[test]
    fn the_daemon_decode_burst_arm_records_a_stand_down() {
        let signal = signal_with_frame();
        let (_backend, mut e) = engine();
        prime(&mut e);
        let burst = capture_via_daemon_path(&mut e, &signal);
        let _ = e.decode_burst(MODE, &burst);
        assert_recorded("decode_burst", &e);
    }

    /// The on-air configuration's arm (`ota_enabled`): `server::run` calls `ota_decode_burst`.
    #[test]
    fn the_daemon_ota_arm_records_a_stand_down() {
        let signal = signal_with_frame();
        let (_backend, mut e) = engine();
        e.start_ota_session(SessionProfile::hpx_hf());
        assert!(e.ota_active(), "without a session this arm early-returns");
        prime(&mut e);
        let burst = capture_via_daemon_path(&mut e, &signal);
        let outcome = e.ota_decode_burst(&burst, "stand-down", Some(MODE));
        assert!(
            !matches!(&outcome, Err(err) if err.to_string().contains("no OTA session active")),
            "ota_decode_burst early-returned without decoding: {outcome:?}"
        );
        assert_recorded("ota_decode_burst", &e);
    }
}

/// #1060: does a real receive filter lift idle-noise ρ above the shipped veto threshold?
///
/// **This is the measurement that decides whether there is a defect at all**, and it is the one
/// thing in the #1062 family that simulation cannot settle. Every band-limited figure in
/// `preamble_rho_fade_and_filter_probe.rs` uses a brick-wall FFT mask, sharper than any real
/// filter. #1060 records the true 500 Hz value as lying between **0.196** (SSB-shaped) and
/// **0.441** (brick-wall), against a shipped threshold of **0.40**. Where it actually falls decides
/// whether the deployed BPSK250 veto is silently inert for narrow-filter stations *today*.
///
/// It lives here, as a unit test, rather than in `tests/`, for a reason worth keeping: it calls the
/// receive path's OWN `build_preamble_veto` + `preamble_rho` — both private — instead of
/// reimplementing the correlation. A probe that rebuilds the correlation can drift from the shipped
/// veto without either side changing visibly, which already cost this repo an inverted conclusion
/// when a reproduction harness carried hand-transcribed parameters. An earlier draft exported a
/// `pub fn` accessor to keep the probe in `tests/`; the reachability ratchet correctly rejected it
/// as public API no production code calls, and being a unit test is the fix rather than the
/// workaround — private access is exactly what an instrument measuring internals needs.
///
/// # Recording the capture
///
/// On the rig, with **the 500 Hz receive filter engaged**, no transmission anywhere, SDR stopped:
///
/// ```text
/// DURATION=45 OUT=/tmp/idle-500hz.wav scripts/onair-rx-idle-floor.sh plughw:CARD=CODEC,DEV=0
/// OPHF_IDLE_WAV=/tmp/idle-500hz.wav cargo test -p openpulse-modem --no-default-features \
///   idle_rho_against_the_shipped_threshold -- --ignored --nocapture
/// ```
///
/// Record the filter width and rig with the number: ρ is normalised and level-insensitive, but the
/// filter is the variable under test, and a capture whose filter setting is unrecorded measures
/// nothing.
#[cfg(test)]
mod idle_rho_probe {
    use super::*;
    use crate::capture_replay::load_wav;
    use bpsk_plugin::BpskPlugin;
    use openpulse_audio::LoopbackBackend;

    /// The only mode that publishes a template; its threshold is the constant under test.
    const MODE: &str = "BPSK250";

    #[test]
    #[ignore = "needs a rig capture; set OPHF_IDLE_WAV"]
    fn idle_rho_against_the_shipped_threshold() {
        let Ok(path) = std::env::var("OPHF_IDLE_WAV") else {
            panic!("set OPHF_IDLE_WAV to a recorded idle capture (see this module's docs)");
        };
        let capture = load_wav(&path).unwrap_or_else(|e| panic!("loading {path}: {e}"));

        let mut engine = ModemEngine::new(Box::new(LoopbackBackend::new()));
        engine
            .register_plugin(Box::new(BpskPlugin::new()))
            .expect("register bpsk");
        let sample_rate = AudioConfig::default().sample_rate;
        let veto = engine
            .build_preamble_veto(MODE, sample_rate)
            .expect("BPSK250 must publish a template, or this measures nothing");
        let threshold = veto.rho_threshold;

        // Window it the way the receive path sees it — the veto runs on an acquisition window, not
        // the whole file. Quarter-window steps so a peak cannot fall between positions.
        let samples = &capture.samples;
        let window = 8_000usize.min(samples.len());
        assert!(
            window > 0 && samples.len() >= window,
            "capture shorter than one acquisition window ({} samples)",
            samples.len()
        );

        let mut rhos: Vec<f32> = Vec::new();
        let mut start = 0usize;
        while start + window <= samples.len() {
            // settled_hz = 0.0: idle noise has no carrier to settle on, so this is the grid search
            // the receive path runs after a settle that landed on noise — the failure mode the veto
            // exists to refuse.
            if let Some((rho, _)) = engine.preamble_rho(&veto, &samples[start..start + window], 0.0)
            {
                rhos.push(rho);
            }
            start += (window / 4).max(1);
        }

        assert!(
            !rhos.is_empty(),
            "no window produced a ρ — the measurement did not run, which is NOT the same as a low ρ"
        );
        rhos.sort_by(|a, b| a.partial_cmp(b).expect("finite ρ"));
        let n = rhos.len();
        let pick = |q: f64| rhos[((n as f64 - 1.0) * q).round() as usize];
        let mean = rhos.iter().sum::<f32>() / n as f32;
        let over = rhos.iter().filter(|r| **r >= threshold).count();

        println!("\n#1060 — idle ρ from a rig capture: {path}");
        println!("  mode {MODE}   windows {n}   shipped threshold {threshold:.3}");
        println!(
            "  mean {mean:.3}   p50 {:.3}   p90 {:.3}   MAX {:.3}",
            pick(0.50),
            pick(0.90),
            rhos[n - 1]
        );
        println!("  windows at or over the threshold: {over} / {n}");
        println!("  reference: 0.196 SSB-shaped, 0.441 brick-wall, {threshold:.3} shipped");
        println!(
            "  VERDICT: {}",
            if over == 0 {
                "idle noise never reaches the threshold here — the veto discriminates."
            } else {
                "idle noise REACHES the threshold — the veto is degraded or inert at this width."
            }
        );
        println!("  Scope: one capture, one rig, one filter setting.\n");
    }
}
