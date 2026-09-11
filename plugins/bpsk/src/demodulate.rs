//! BPSK demodulator.
//!
//! The demodulation pipeline is:
//!
//! ```text
//! audio samples
//!   → multiply by I/Q reference carriers
//!   → matched-filter (half-Hann w_tail) integration per symbol period
//!   → timing search over first symbol period (brute-force energy maximisation)
//!   → differential phase detection (NRZI decode)
//!   → bits → bytes
//! ```
//!
//! ## Symbol timing
//!
//! The modulator prepends [`PREAMBLE_SYMS`] symbols with alternating phases
//! (+1, −1, +1, …).  The demodulator scans every possible timing offset
//! within the first symbol period, picks the offset that maximises the
//! demodulated preamble energy, and uses that offset for the rest of the frame.
//!
//! ## Phase ambiguity
//!
//! Differential detection removes the 180° absolute-phase ambiguity that
//! would otherwise require carrier-phase recovery.

use std::f32::consts::PI;

use num_complex::Complex32;
use openpulse_core::error::ModemError;
use openpulse_core::plugin::{ModulationConfig, PulseShape};
use openpulse_dsp::acquisition::goertzel_carrier_scan;
use openpulse_dsp::constellation::{additive_snr_db_windowed, differential_llr_scale};
use openpulse_dsp::equalizer::LmsEqualizer;
use openpulse_dsp::farrow::FarrowTimingLoop;
use openpulse_dsp::filter::FirFilter;
use openpulse_dsp::pll::CarrierPll;
use openpulse_dsp::rrc::generate_rrc_coefficients;

use crate::modulate::{
    nrzi_encode, samples_per_symbol, PREAMBLE_SYMS, RRC_SPAN_SYMBOLS, TAIL_SYMS,
};
use crate::parse_baud_rate;

// ── Public entry point ────────────────────────────────────────────────────────

/// Demodulate audio `samples` and return the recovered bytes.
/// The symbol stream the decoder sees: matched-filtered, timing-recovered baseband I/Q.
///
/// Shared by `bpsk_demodulate` and `estimate_snr_db` so the SNR is measured on exactly the symbols
/// that were decoded, not on a separately-derived approximation.
fn symbol_stream(
    samples: &[f32],
    config: &ModulationConfig,
) -> Result<(Vec<f32>, Vec<f32>), ModemError> {
    symbol_stream_with_expected(samples, config, &expected_preamble_symbols(PREAMBLE_SYMS))
}

/// [`symbol_stream`] whose timing lock uses a supplied expectation.
///
/// The `-RRC` path is **not** parameterised: it locks timing with Gardner+LMS
/// rather than a preamble correlation, so a candidate sequence reaches it only
/// through training, which this seam does not cover.
fn symbol_stream_with_expected(
    samples: &[f32],
    config: &ModulationConfig,
    expected: &[f32],
) -> Result<(Vec<f32>, Vec<f32>), ModemError> {
    let baud = parse_baud_rate(&config.mode)?;
    let fs = config.sample_rate as f32;
    let fc = config.center_frequency;
    let n = samples_per_symbol(fs, baud)?;
    let rrc_alpha = if let PulseShape::Rrc { alpha } = config.pulse_shape {
        Some(alpha)
    } else if config.mode.ends_with("-RRC") {
        Some(0.35f32)
    } else {
        None
    };
    if samples.len() < n * (expected.len() + 1) {
        return Err(ModemError::Demodulation("signal too short".into()));
    }
    // Apply matched RRC RX filter for -RRC modes.
    // For RRC: downmix to baseband I/Q first, then apply the RRC as a low-pass
    // matched filter.  (Applying the baseband RRC to the passband signal would
    // place fc far outside the filter passband and attenuate the signal to ~0.)
    if let Some(alpha) = rrc_alpha {
        // The RRC path locks timing with Gardner+LMS and trains on the SHIPPED
        // sequence, so it cannot honour a candidate expectation. Refuse loudly
        // rather than silently decode a different geometry — a doc comment here
        // would be exactly the fidelity-by-comment this seam exists to remove.
        if expected != expected_preamble_symbols(PREAMBLE_SYMS) {
            return Err(ModemError::Demodulation(
                "the -RRC path cannot honour a candidate preamble: its timing is \
                 Gardner+LMS trained on the shipped sequence"
                    .into(),
            ));
        }
        Ok(bpsk_demodulate_rrc(
            samples,
            n,
            baud,
            fc,
            fs,
            alpha,
            &config.mode,
        ))
    } else {
        let offset = find_timing_offset_with_expected(samples, n, fc, fs, expected);
        let (mut iv, mut qv) = demodulate_iq(samples, n, fc, fs, offset);
        // The overlapping half-Hann modulator is a crossfade, so the one-slot matched filter recovers
        // `r_k = a_k + β·a_{k+1}` (β = 1/3). Left in, that `+β` term adds a constant positive bias to the
        // differential dot product `r_k·r_{k-1}` (a_k²=1), eroding the flip-bit margin by several dB.
        // Cancel it here (crossfade path only; the -RRC path uses Gardner+LMS and does not crossfade).
        cancel_crossfade_isi(&mut iv, &mut qv);
        Ok((iv, qv))
    }
}

/// Absolute additive SNR (dB) of a received BPSK frame — the rate controller's input.
///
/// BPSK had **no** symbol-domain estimator, so the engine fell back to the waveform-blind M2M4
/// moment estimator. M2M4 assumes a constant-modulus envelope, which a fade destroys: on Watterson
/// `moderate_f1` it read a **flat ≈ −6.6 dB from 15 dB of true SNR upward** — no information at any
/// SNR. `hpx_hf`'s SL2–SL5 are all BPSK (SL2 is the rung every session starts on), so the controller
/// saw a sub-floor number on frames that decoded perfectly and fast-downshifted the link to the
/// bottom rung, delivering nothing on a routine HF fade (issue #934).
///
/// The fix is the same one `openpulse_channel::estimate_additive_snr_db` applies to raw audio:
/// remove the *multiplicative* channel with a per-window least-squares gain before measuring the
/// residual. BPSK is differentially decoded, so the transmitted ±1 sequence is reconstructed from the
/// decisions the decoder already made; its arbitrary global sign is absorbed by the per-window gain.
pub fn estimate_snr_db(samples: &[f32], config: &ModulationConfig) -> Option<f32> {
    let (i_syms, q_syms) = symbol_stream(samples, config).ok()?;
    if i_syms.len() <= PREAMBLE_SYMS + TAIL_SYMS {
        return None;
    }
    let range_start = PREAMBLE_SYMS - 1;
    let end = i_syms.len() - TAIL_SYMS;
    if range_start >= end {
        return None;
    }
    let rx: Vec<Complex32> = i_syms[range_start..end]
        .iter()
        .zip(q_syms[range_start..end].iter())
        .map(|(&i, &q)| Complex32::new(i, q))
        .collect();
    let iq: Vec<(f32, f32)> = rx.iter().map(|z| (z.re, z.im)).collect();
    let bits = differential_decode(&iq);
    // Rebuild the transmitted symbols from the differential decisions: each "1" flips the phase.
    // The starting sign is unknown and does not matter — the per-window LS gain absorbs it.
    let mut decisions = Vec::with_capacity(rx.len());
    let mut cur = Complex32::new(1.0, 0.0);
    decisions.push(cur);
    for &flip in &bits {
        if flip {
            cur = -cur;
        }
        decisions.push(cur);
    }
    const WINDOW_SYMS: usize = 16;
    let es_n0 = additive_snr_db_windowed(&rx, &decisions, WINDOW_SYMS);

    // Convert symbol-domain Es/N0 to the *channel* SNR scale the rate ladder's floors are written in.
    // The estimate is taken after the matched filter, so it carries the mode's processing gain — a
    // 31-baud rung reads ~17 dB above the channel SNR and a 250-baud rung ~8 dB. Left unconverted the
    // receiver over-recommends badly (a 2 dB AWGN channel drove the ladder to SL5), which is just the
    // #934 scale defect wearing a different hat: never compare one scale's number against another's.
    //
    // Measured on AWGN, the offset is `10·log10(fs/baud) − MATCHED_FILTER_LOSS_DB` and the constant
    // holds to ~0.3 dB across BPSK31 and BPSK250 — an order of magnitude apart in baud — so it is the
    // pulse's noise-bandwidth loss, not a per-mode fudge. It IS pulse-specific: it is fitted to this
    // plugin's Hann/crossfade matched filter. `bpsk_snr_awgn_scale_matches_channel_snr` pins it.
    const MATCHED_FILTER_LOSS_DB: f32 = 7.1;
    let baud = parse_baud_rate(&config.mode).ok()?;
    let fs = config.sample_rate as f32;
    let processing_gain_db = 10.0 * (fs / baud).log10();
    Some(es_n0 - processing_gain_db + MATCHED_FILTER_LOSS_DB)
}

pub fn bpsk_demodulate(samples: &[f32], config: &ModulationConfig) -> Result<Vec<u8>, ModemError> {
    bpsk_demodulate_with_expected(samples, config, &expected_preamble_symbols(PREAMBLE_SYMS))
}

/// [`bpsk_demodulate`] whose timing lock and framing use a supplied expectation.
///
/// Both the timing correlation and the preamble/data symbol boundaries follow
/// `expected.len()`, so a candidate preamble is decoded end-to-end on its own
/// geometry — the column that decides whether a candidate is viable at all.
pub fn bpsk_demodulate_with_expected(
    samples: &[f32],
    config: &ModulationConfig,
    expected: &[f32],
) -> Result<Vec<u8>, ModemError> {
    let preamble_syms = expected.len();
    // Front-end (matched filter + timing) lives in `symbol_stream`, shared with `estimate_snr_db`
    // so the SNR is measured on exactly the symbols that get decoded.
    let (i_syms, q_syms) = symbol_stream_with_expected(samples, config, expected)?;

    if i_syms.len() <= preamble_syms + TAIL_SYMS {
        return Err(ModemError::Demodulation(
            "no data symbols after preamble".into(),
        ));
    }

    // Differential phase detection (handles absolute-phase ambiguity).
    // We take consecutive (I,Q) pairs and compute Re(z[k] * conj(z[k-1])).
    // Positive → same phase → NRZI "0" (no flip); negative → "1" (flip).
    let data_syms_start = preamble_syms;
    let data_syms_end = i_syms.len() - TAIL_SYMS;

    if data_syms_start >= data_syms_end {
        return Ok(vec![]);
    }

    // Build the full range including the last preamble symbol as the reference
    // for the first data bit.
    let range_start = preamble_syms - 1; // include prev preamble symbol as reference
    let iq: Vec<(f32, f32)> = i_syms[range_start..data_syms_end]
        .iter()
        .zip(q_syms[range_start..data_syms_end].iter())
        .map(|(&i, &q)| (i, q))
        .collect();

    let bits = differential_decode(&iq);
    let bytes = bits_to_bytes(&bits);
    Ok(bytes)
}

// ── AFC frequency-offset estimator ───────────────────────────────────────────

/// Estimate the carrier frequency offset in Hz from demodulated IQ symbols.
///
/// Uses the IQ-squaring method: squaring each complex symbol removes the DBPSK
/// data modulation (since `2·φ_data ∈ {0, 2π}`), leaving a phasor that rotates
/// at `4π·Δf/baud_rate` radians per symbol.  The mean phase of consecutive
/// squared-symbol products then gives `Δf`.
///
/// **Tracking range:** `|Δf| ≤ baud_rate / 4`
/// - BPSK31:  ±7.8 Hz
/// - BPSK63:  ±15.6 Hz
/// - BPSK100: ±25 Hz
/// - BPSK250: ±62.5 Hz  ← covers the ±50 Hz spec at the widest BPSK mode
pub fn estimate_frequency_offset(i_syms: &[f32], q_syms: &[f32], baud_rate: f32) -> f32 {
    if i_syms.len() < 2 {
        return 0.0;
    }

    // z²[k] = (I[k]+jQ[k])² = (I²-Q²) + j(2IQ)
    let re2: Vec<f32> = i_syms
        .iter()
        .zip(q_syms.iter())
        .map(|(&i, &q)| i * i - q * q)
        .collect();
    let im2: Vec<f32> = i_syms
        .iter()
        .zip(q_syms.iter())
        .map(|(&i, &q)| 2.0 * i * q)
        .collect();

    // D[k] = z²[k] * conj(z²[k-1]); accumulate sum
    let mut re_sum = 0.0f32;
    let mut im_sum = 0.0f32;
    for k in 1..re2.len() {
        re_sum += re2[k] * re2[k - 1] + im2[k] * im2[k - 1];
        im_sum += im2[k] * re2[k - 1] - re2[k] * im2[k - 1];
    }

    // Δf = baud_rate * atan2(im, re) / (4π)
    im_sum.atan2(re_sum) * baud_rate / (4.0 * PI)
}

/// Wide-range carrier frequency estimator using the Goertzel algorithm on the
/// squared signal.
///
/// Squaring removes BPSK modulation, leaving a tone at 2×fc.  A Goertzel
/// search in 25 Hz steps over 2×fc ± 800 Hz (= fc ± 400 Hz at baseband)
/// locates the dominant peak.  **Acquisition range: ±400 Hz.**
fn estimate_carrier_hz_wide(samples: &[f32], config: &ModulationConfig) -> Option<f32> {
    let baud = crate::parse_baud_rate(&config.mode).ok()?;
    let fs = config.sample_rate as f32;
    let fc = config.center_frequency;
    let n = crate::modulate::samples_per_symbol(fs, baud).ok()?;

    if samples.len() < n * PREAMBLE_SYMS {
        return None;
    }

    // Limit to 4× preamble length to keep per-call cost bounded.
    let window_len = (n * PREAMBLE_SYMS * 4).min(samples.len());
    goertzel_carrier_scan(&samples[..window_len], fs, fc, 2, 400.0, 12.5)
}

/// Run a lightweight demodulation pass to estimate the carrier frequency offset.
///
/// **Two-stage estimator:**
/// 1. Goertzel coarse search (±400 Hz, 12.5 Hz resolution at baseband) to handle
///    large initial offsets — e.g., VHF crystal errors of up to ±3 ppm on 144 MHz.
/// 2. IQ-squaring fine correction at the Goertzel-corrected centre frequency to
///    recover the sub-step residual (≤ 6.25 Hz) for accurate tracking.
///
/// Returns `None` if the buffer is too short for either path.
pub fn afc_estimate_hz(samples: &[f32], config: &ModulationConfig) -> Option<f32> {
    afc_estimate_hz_with_expected(samples, config, &expected_preamble_symbols(PREAMBLE_SYMS))
}

/// [`afc_estimate_hz`] whose stage-2 timing lock uses a supplied expectation.
///
/// Stage 2 calls [`find_timing_offset`], which correlates against the expected
/// preamble — so estimating AFC on candidate-preamble audio through the shipped
/// entry point measures a TX/RX mismatch, not the candidate.
pub fn afc_estimate_hz_with_expected(
    samples: &[f32],
    config: &ModulationConfig,
    expected: &[f32],
) -> Option<f32> {
    let baud = crate::parse_baud_rate(&config.mode).ok()?;
    let fs = config.sample_rate as f32;
    let n = crate::modulate::samples_per_symbol(fs, baud).ok()?;

    // Stage 1: coarse Goertzel acquisition (±400 Hz).
    let coarse = estimate_carrier_hz_wide(samples, config);

    if samples.len() < n * (expected.len() + 1) {
        return coarse;
    }

    // Stage 2: fine IQ-squaring at the Goertzel-corrected centre frequency to
    // eliminate the sub-step quantisation error (≤ 6.25 Hz).
    let c = coarse.unwrap_or(0.0);
    let corrected_fc = config.center_frequency + c;
    let offset = find_timing_offset_with_expected(samples, n, corrected_fc, fs, expected);
    let (i_syms, q_syms) = demodulate_iq(samples, n, corrected_fc, fs, offset);
    let residual = estimate_frequency_offset(&i_syms, &q_syms, baud);

    // The residual should be within ±baud/4 of 0 after Goertzel correction.
    // If it is, combine both stages; otherwise fall back to the coarse estimate.
    if residual.abs() < baud / 4.0 {
        Some(c + residual)
    } else {
        coarse
    }
}

/// Demodulate audio `samples` and return per-bit soft log-likelihood ratios.
///
/// Returns one `f32` per decoded bit, with **positive = bit more likely 0**.
/// Uses the differential-detection dot product directly (real part of
/// z[k] × conj(z[k−1])) as the soft value; positive dot → same phase → bit 0.
///
/// Returns one `f32` per decoded bit (positive = bit more likely 0).
///
/// For non-RRC modes: differential cross-correlation dot product on Hann-windowed symbols.
/// For RRC modes: same dot product applied to Gardner+LMS recovered symbols.
pub fn bpsk_demodulate_soft(
    samples: &[f32],
    config: &ModulationConfig,
) -> Result<Vec<f32>, ModemError> {
    let baud = parse_baud_rate(&config.mode)?;
    let fs = config.sample_rate as f32;
    let fc = config.center_frequency;
    let n = samples_per_symbol(fs, baud)?;

    if samples.len() < n * (PREAMBLE_SYMS + 1) {
        return Err(ModemError::Demodulation("signal too short".into()));
    }

    let rrc_alpha = if let PulseShape::Rrc { alpha } = config.pulse_shape {
        Some(alpha)
    } else if config.mode.ends_with("-RRC") {
        Some(0.35f32)
    } else {
        None
    };

    let (i_syms, q_syms) = if let Some(alpha) = rrc_alpha {
        bpsk_demodulate_rrc(samples, n, baud, fc, fs, alpha, &config.mode)
    } else {
        let offset = find_timing_offset(samples, n, fc, fs);
        // NOTE: crossfade-ISI cancellation is deliberately NOT applied on the soft path. BPSK is
        // *differential*, so the backward-substitution recursion inflates the noise LLRs of a deeply
        // faded attempt instead of suppressing them — that breaks the 1/σ² LLR calibration HARQ MAP
        // combining relies on (regressed `llr_calibration::a_deeply_faded_extra_attempt_does_not_hurt`).
        // The cancellation stays on the hard differential path (`bpsk_demodulate`), where it restores the
        // decision margin without disturbing any soft-combining scale.
        demodulate_iq(samples, n, fc, fs, offset)
    };

    if i_syms.len() <= PREAMBLE_SYMS + TAIL_SYMS {
        return Err(ModemError::Demodulation(
            "no data symbols after preamble".into(),
        ));
    }

    let range_start = PREAMBLE_SYMS - 1;
    let data_syms_end = i_syms.len() - TAIL_SYMS;

    let iq: Vec<(f32, f32)> = i_syms[range_start..data_syms_end]
        .iter()
        .zip(q_syms[range_start..data_syms_end].iter())
        .map(|(&i, &q)| (i, q))
        .collect();

    // dot = Re(z[k] × conj(z[k-1])) = i1*i0 + q1*q0
    // Positive → same phase → NRZI "0" → bit 0 → LLR > 0 ✓
    let mut llrs: Vec<f32> = iq
        .windows(2)
        .map(|w| {
            let (i0, q0) = w[0];
            let (i1, q1) = w[1];
            i1 * i0 + q1 * q0
        })
        .collect();
    // cross = Im(z[k] × conj(z[k-1])): mean zero, so it carries only noise.
    let crosses: Vec<f32> = iq
        .windows(2)
        .map(|w| {
            let (i0, q0) = w[0];
            let (i1, q1) = w[1];
            q1 * i0 - i1 * q0
        })
        .collect();

    // Calibrate the soft values into *true* log-likelihood ratios (magnitude ∝ 1/σ²). Nothing that
    // decodes a single frame notices — soft Viterbi, min-sum LDPC and max-log turbo are all
    // scale-invariant — but HARQ soft combining across receive attempts does: uncalibrated, an attempt
    // from a deep fade votes as loudly as a clean one. See `openpulse_core::fec::combine_llrs_map`.
    let scale = differential_llr_scale(&llrs, &crosses);
    for l in llrs.iter_mut() {
        *l *= scale;
    }
    Ok(llrs)
}

/// GPU RRC demodulation: downmix on CPU, matched RRC filter on GPU, timing + LMS on CPU.
#[cfg(feature = "gpu")]
fn bpsk_demodulate_rrc_gpu(
    samples: &[f32],
    config: &ModulationConfig,
    ctx: &openpulse_gpu::GpuContext,
) -> Result<Vec<u8>, ModemError> {
    let baud = parse_baud_rate(&config.mode)?;
    let fs = config.sample_rate as f32;
    let fc = config.center_frequency;
    let n = samples_per_symbol(fs, baud)?;
    let alpha = if let PulseShape::Rrc { alpha } = config.pulse_shape {
        alpha
    } else {
        0.35
    };

    if samples.len() < n * (PREAMBLE_SYMS + 1) {
        return Err(ModemError::Demodulation("signal too short".into()));
    }

    let two_pi = 2.0 * PI;
    let num_taps = RRC_SPAN_SYMBOLS * n + 1;
    let coeffs = generate_rrc_coefficients(fs, baud, alpha, num_taps);
    let group_delay = (num_taps - 1) / 2;

    let i_mix: Vec<f32> = samples
        .iter()
        .enumerate()
        .map(|(k, &s)| s * (two_pi * fc * k as f32 / fs).cos() * 2.0)
        .collect();
    let q_mix: Vec<f32> = samples
        .iter()
        .enumerate()
        .map(|(k, &s)| -s * (two_pi * fc * k as f32 / fs).sin() * 2.0)
        .collect();

    // GPU FIR: pad tail by group_delay, filter, then trim.
    let gpu_rrc = |mix: Vec<f32>| -> Option<Vec<f32>> {
        let padded: Vec<f32> = mix
            .iter()
            .copied()
            .chain(std::iter::repeat_n(0.0, group_delay))
            .collect();
        let filtered = openpulse_gpu::gpu_rrc_fir(ctx, &padded, &coeffs)?;
        Some(filtered[group_delay..].to_vec())
    };

    let (i_bb, q_bb) = match (gpu_rrc(i_mix), gpu_rrc(q_mix)) {
        (Some(i), Some(q)) => (i, q),
        // GPU error — fall back to CPU path.
        _ => return bpsk_demodulate(samples, config),
    };

    let initial_timing = find_timing_offset_bb(&i_bb, &q_bb, n);
    // De-rotate to the preamble carrier phase before LMS (see bpsk_demodulate_rrc).
    let phase_0 = coarse_baseband_phase(&i_bb, &q_bb, n, initial_timing);
    let (sin0, cos0) = (-phase_0).sin_cos();
    let (i_rot, q_rot): (Vec<f32>, Vec<f32>) = i_bb
        .iter()
        .zip(q_bb.iter())
        .map(|(&i, &q)| (i * cos0 - q * sin0, i * sin0 + q * cos0))
        .unzip();
    let (i_out, q_out) = gardner_sample_rrc(&i_rot, &q_rot, n, initial_timing);
    let (i_syms, q_syms) = bpsk_lms_equalize(&i_out, &q_out, &config.mode);

    if i_syms.len() <= PREAMBLE_SYMS + TAIL_SYMS {
        return Err(ModemError::Demodulation(
            "no data symbols after preamble".into(),
        ));
    }

    let data_syms_end = i_syms.len() - TAIL_SYMS;
    let range_start = PREAMBLE_SYMS - 1;
    let iq: Vec<(f32, f32)> = i_syms[range_start..data_syms_end]
        .iter()
        .zip(q_syms[range_start..data_syms_end].iter())
        .map(|(&i, &q)| (i, q))
        .collect();

    let bits = differential_decode(&iq);
    Ok(bits_to_bytes(&bits))
}

/// GPU-accelerated demodulation path.
#[cfg(feature = "gpu")]
pub fn bpsk_demodulate_with_gpu(
    samples: &[f32],
    config: &ModulationConfig,
    ctx: &openpulse_gpu::GpuContext,
) -> Result<Vec<u8>, ModemError> {
    // RRC path: downmix on CPU, matched RRC filter on GPU, timing + LMS on CPU.
    if matches!(config.pulse_shape, PulseShape::Rrc { .. }) || config.mode.ends_with("-RRC") {
        return bpsk_demodulate_rrc_gpu(samples, config, ctx);
    }

    let baud = parse_baud_rate(&config.mode)?;
    let fs = config.sample_rate as f32;
    let fc = config.center_frequency;
    let n = samples_per_symbol(fs, baud)?;

    if samples.len() < n * (PREAMBLE_SYMS + 1) {
        return Err(ModemError::Demodulation("signal too short".into()));
    }

    let expected = expected_preamble_symbols(PREAMBLE_SYMS);
    let offset = match openpulse_gpu::timing_offset_search_gpu(
        ctx,
        samples,
        n,
        PREAMBLE_SYMS,
        &expected,
        fc,
        fs,
    ) {
        Some(o) => o,
        None => return bpsk_demodulate(samples, config),
    };

    let effective = &samples[offset.min(samples.len())..];
    let (i_syms, q_syms) = match openpulse_gpu::bpsk_iq_demod_gpu(ctx, effective, n, fc, fs, offset)
    {
        Some(iq) => iq,
        None => return bpsk_demodulate(samples, config),
    };

    if i_syms.len() <= PREAMBLE_SYMS + TAIL_SYMS {
        return Err(ModemError::Demodulation(
            "no data symbols after preamble".into(),
        ));
    }

    let data_syms_end = i_syms.len() - TAIL_SYMS;
    if PREAMBLE_SYMS >= data_syms_end {
        return Ok(vec![]);
    }

    let range_start = PREAMBLE_SYMS - 1;
    let iq: Vec<(f32, f32)> = i_syms[range_start..data_syms_end]
        .iter()
        .zip(q_syms[range_start..data_syms_end].iter())
        .map(|(&i, &q)| (i, q))
        .collect();

    let bits = differential_decode(&iq);
    Ok(bits_to_bytes(&bits))
}

// ── RRC baseband demodulation path ───────────────────────────────────────────

/// Full RRC demodulation: downmix → matched RRC filter → timing → sample.
///
/// The RRC filter is a low-pass (baseband) filter.  It must be applied AFTER
/// downmixing to baseband, not directly to the bandpass signal.
fn bpsk_demodulate_rrc(
    samples: &[f32],
    n: usize,
    baud: f32,
    fc: f32,
    fs: f32,
    alpha: f32,
    mode: &str,
) -> (Vec<f32>, Vec<f32>) {
    let two_pi = 2.0 * PI;
    let num_taps = RRC_SPAN_SYMBOLS * n + 1;
    let coeffs = generate_rrc_coefficients(fs, baud, alpha, num_taps);
    let group_delay = (num_taps - 1) / 2;

    // 1. Downmix to baseband I and Q (factor of 2 compensates the carrier ½).
    let i_mix: Vec<f32> = samples
        .iter()
        .enumerate()
        .map(|(k, &s)| s * (two_pi * fc * k as f32 / fs).cos() * 2.0)
        .collect();
    let q_mix: Vec<f32> = samples
        .iter()
        .enumerate()
        .map(|(k, &s)| -s * (two_pi * fc * k as f32 / fs).sin() * 2.0)
        .collect();

    // 2. Apply RRC matched filter with group delay compensation to each channel.
    let rrc_filter = |mix: Vec<f32>| -> Vec<f32> {
        let padded: Vec<f32> = mix
            .iter()
            .copied()
            .chain(std::iter::repeat_n(0.0, group_delay))
            .collect();
        let mut fir = FirFilter::new(coeffs.clone());
        let filtered = fir.apply(&padded);
        filtered[group_delay..].to_vec()
    };

    let i_bb = rrc_filter(i_mix);
    let q_bb = rrc_filter(q_mix);

    // 3. Coarse timing acquisition via preamble correlation (brute-force, same as Hann path).
    let initial_timing = find_timing_offset_bb(&i_bb, &q_bb, n);

    // 3b. De-rotate the baseband to the preamble's carrier phase BEFORE the
    // LMS stage.  The equalizer trains against the REAL (±1, 0) preamble
    // targets; at a ~90° carrier phase the symbol energy lives in Q and 32
    // training symbols are not enough for the complex taps to converge from a
    // full quadrature rotation — decision-directed mode then destabilises.
    // The known preamble gives the phase directly (same pattern as 64QAM).
    let phase_0 = coarse_baseband_phase(&i_bb, &q_bb, n, initial_timing);
    let (sin0, cos0) = (-phase_0).sin_cos();
    let (i_rot, q_rot): (Vec<f32>, Vec<f32>) = i_bb
        .iter()
        .zip(q_bb.iter())
        .map(|(&i, &q)| (i * cos0 - q * sin0, i * sin0 + q * cos0))
        .unzip();

    // 4. Adaptive timing recovery via Gardner detector starting from the acquired offset.
    let (i_out, q_out) = gardner_sample_rrc(&i_rot, &q_rot, n, initial_timing);

    // 5. LMS equalizer: train on the known preamble symbols, then decision-directed.
    // RRC path: DFE enabled for BPSK250 to handle multipath ISI.
    let (i_eq, q_eq) = bpsk_lms_equalize(&i_out, &q_out, mode);

    (i_eq, q_eq)
}

/// Carrier phase of the preamble at `timing`: arg of the complex correlation
/// `Σ (i + jq)·e` against the real expected preamble amplitudes.
fn coarse_baseband_phase(i_bb: &[f32], q_bb: &[f32], n: usize, timing: usize) -> f32 {
    let expected = expected_preamble_symbols(PREAMBLE_SYMS);
    let (mut re, mut im) = (0.0f32, 0.0f32);
    for (s, &e) in expected.iter().enumerate() {
        let idx = timing + s * n;
        if idx >= i_bb.len() {
            break;
        }
        re += i_bb[idx] * e;
        im += q_bb.get(idx).copied().unwrap_or(0.0) * e;
    }
    im.atan2(re)
}

/// Select the LMS tap/step profile for a given mode.
///
/// BPSK250 has a 4 ms/symbol period — short enough that Watterson Moderate/Poor
/// delay spread (0.5–3 ms) produces multi-symbol ISI.  A 9-tap feedforward
/// plus 2-tap DFE with a tighter step gives better convergence on the
/// RRC+Gardner path where multipath ISI is the dominant impairment.
/// Narrow-band HF modes (BPSK31/63/100) have symbol periods ≥ 10 ms and are
/// inherently ISI-immune at typical HF delay spreads; the baseline 7-tap
/// equalizer is sufficient.
fn lms_profile(mode: &str) -> (usize, usize, f32) {
    if mode.contains("250") {
        (9, 2, 0.015)
    } else {
        (7, 0, 0.02)
    }
}

/// Apply a mode-aware LMS equalizer to BPSK symbol-rate I/Q.
///
/// Trains on the first `PREAMBLE_SYMS` samples using the known preamble
/// sequence, then switches to decision-directed mode.  Called only from the
/// RRC+Gardner path; the Hann-windowed non-RRC path does not apply LMS
/// (the integration already suppresses ISI and LMS decision-directed mode
/// degrades fading-channel performance).
fn bpsk_lms_equalize(i_syms: &[f32], q_syms: &[f32], mode: &str) -> (Vec<f32>, Vec<f32>) {
    let training = expected_preamble_symbols(PREAMBLE_SYMS.min(i_syms.len()));
    let training_q = vec![0.0f32; training.len()];
    let (fwd_len, dfe_len, mu) = lms_profile(mode);
    // Train-then-freeze: unsupervised DD adaptation drifts to a wrong
    // self-consistent equilibrium (gain creep + Q contamination) over long
    // frames even on clean input; the residual carrier is already tracked by
    // the Costas PLL ahead of this stage, so frozen taps stay valid.
    let mut eq = LmsEqualizer::new(fwd_len, dfe_len, mu).with_frozen_dd();
    eq.process_frame(i_syms, q_syms, &training, &training_q, |i, _q| {
        (if i >= 0.0 { 1.0 } else { -1.0 }, 0.0)
    })
}

// ── Timing search ─────────────────────────────────────────────────────────────

/// Interpolating symbol sampling with true timing-drift tracking.
///
/// `initial_timing` seeds the start position from brute-force preamble
/// correlation; the Farrow loop then tracks fractional timing AND the actual
/// samples-per-symbol period for the remainder of the frame.  The previous
/// fixed-stride GardnerDetector could not adjust the sampling instant at all
/// (its mu clamp kept the strobe interval at exactly `n`), so a sound-card
/// sample-rate offset slid long frames into heavy ISI.
fn gardner_sample_rrc(
    i_bb: &[f32],
    q_bb: &[f32],
    n: usize,
    initial_timing: usize,
) -> (Vec<f32>, Vec<f32>) {
    let start = initial_timing.min(i_bb.len());
    // Stronger-than-default gains: at ≤ 250 baud the HF delay spread is
    // sub-symbol, so the multipath bias that forces the conservative default
    // gains on high-baud modes does not apply — and the long low-baud frames
    // need the loop to track 150 ppm with ≤ a few % of period lag.
    let (i_out, q_out) = FarrowTimingLoop::new(n)
        .with_gains(0.05, 0.002)
        .process(i_bb, q_bb, start);

    // Track residual carrier frequency with a BPSK Costas PLL.  A sample-rate
    // offset shifts the carrier too (150 ppm ⇒ −0.225 Hz at 1500 Hz), which
    // rotates the constellation ~90° per ~300 symbols at 250 baud — the LMS
    // stage trains on REAL (±1, 0) targets and cannot follow a rotation, so
    // without the PLL its decision-directed mode collapses mid-frame.  The
    // PLL's 180° ambiguity is harmless: decoding is differential.
    // Level-normalise before the Costas loop: its BPSK discriminant `q·sgn(i)` scales with the symbol
    // amplitude, so a quiet station's small symbols weaken the loop until it can't acquire a sub-deadband
    // residual offset. A no-op at nominal amplitude (RMS ≈ 1); a uniform scale, so decisions are unchanged.
    let ms = i_out
        .iter()
        .zip(q_out.iter())
        .map(|(&i, &q)| i * i + q * q)
        .sum::<f32>()
        / i_out.len().max(1) as f32;
    let k = if ms > 1e-18 { 1.0 / ms.sqrt() } else { 1.0 };

    let mut pll = CarrierPll::new(0.02, 1);
    let mut i_trk = Vec::with_capacity(i_out.len());
    let mut q_trk = Vec::with_capacity(q_out.len());
    for (&s_i, &s_q) in i_out.iter().zip(q_out.iter()) {
        let (s_i, s_q) = (s_i * k, s_q * k);
        pll.update(s_i, s_q);
        let (ci, cq) = pll.correct(s_i, s_q);
        i_trk.push(ci);
        q_trk.push(cq);
    }
    (i_trk, q_trk)
}

/// Brute-force timing search on the baseband I/Q signal (after downmix + RRC).
///
/// Tries every offset in 0..n, samples the baseband at positions
/// `offset + k*n`, and correlates with the expected preamble pattern using the
/// carrier-phase-invariant squared magnitude `(Σ I·e)² + (Σ Q·e)²`.  The old
/// I-only signed correlation collapsed at a ~90° carrier phase (preamble
/// energy entirely in Q) and was additionally polarity-sensitive — the same
/// bug class fixed in QPSK/8PSK/SCFDMA/OFDM.
fn find_timing_offset_bb(i_bb: &[f32], q_bb: &[f32], n: usize) -> usize {
    let expected = expected_preamble_symbols(PREAMBLE_SYMS);
    let mut best_off = 0usize;
    let mut best_score = f32::NEG_INFINITY;

    for off in 0..n {
        if i_bb.len() < off + n * PREAMBLE_SYMS {
            break;
        }
        let (re, im) = (0..PREAMBLE_SYMS).fold((0.0f32, 0.0f32), |(re, im), s| {
            let idx = off + s * n;
            (
                re + i_bb[idx] * expected[s],
                im + q_bb.get(idx).copied().unwrap_or(0.0) * expected[s],
            )
        });
        let score = re * re + im * im;
        if score > best_score {
            best_score = score;
            best_off = off;
        }
    }
    best_off
}

/// Try every possible timing offset within one symbol period.  Return the
/// offset that gives the maximum preamble correlation magnitude.
fn find_timing_offset(samples: &[f32], n: usize, fc: f32, fs: f32) -> usize {
    find_timing_offset_with_expected(
        samples,
        n,
        fc,
        fs,
        &expected_preamble_symbols(PREAMBLE_SYMS),
    )
}

/// [`find_timing_offset`] correlating against a supplied expectation.
///
/// The search span follows `expected.len()` rather than [`PREAMBLE_SYMS`], so a
/// candidate preamble is searched over its own length.
pub fn find_timing_offset_with_expected(
    samples: &[f32],
    n: usize,
    fc: f32,
    fs: f32,
    expected: &[f32],
) -> usize {
    let mut best_energy = f32::NEG_INFINITY;
    let mut best_offset = 0usize;
    let syms = expected.len();

    for offset in 0..n {
        if samples.len() < offset + n * syms {
            break;
        }
        // Demodulate ONLY the preamble span at this offset (the slice may be
        // multi-second; demodulating all of it per offset is O(offsets × N)).
        let span_end = (offset + n * syms).min(samples.len());
        let (i_syms, q_syms) = demodulate_iq(&samples[..span_end], n, fc, fs, offset);
        if i_syms.len() < syms {
            continue;
        }

        // Correlate the first PREAMBLE_SYMS symbols with the expected
        // alternating pattern that NRZI-encoding the preamble bits produces,
        // using the carrier-phase-invariant magnitude (Σ I·e)² + (Σ Q·e)².
        // The previous |Σ I·e| handled the 180° polarity ambiguity but
        // collapsed at a ~90° carrier phase where the preamble energy lives in
        // Q.  The differential BPSK decoder handles polarity after timing lock.
        let (re, im) = i_syms[..syms]
            .iter()
            .zip(q_syms[..syms].iter())
            .zip(expected.iter())
            .fold((0.0f32, 0.0f32), |(re, im), ((&i, &q), &e)| {
                (re + i * e, im + q * e)
            });
        let energy = re * re + im * im;

        if energy > best_energy {
            best_energy = energy;
            best_offset = offset;
        }
    }

    best_offset
}

/// Build the expected I-channel amplitudes for the preamble.
pub fn expected_preamble_symbols(len: usize) -> Vec<f32> {
    // The preamble bits are 1,0,1,0,… → NRZI gives phase_neg = T,T,F,F,T,T,…
    // but we want the raw alternating for correlation: +1,−1,+1,−1,…
    // Actually NRZI(1,0,1,0,…):
    //   bit1: flip → phase_neg=true  → amplitude −1
    //   bit0: keep → phase_neg=true  → amplitude −1
    //   bit1: flip → phase_neg=false → amplitude +1
    //   bit0: keep → phase_neg=false → amplitude +1
    // → pattern: −1,−1,+1,+1,−1,−1,+1,+1,…
    // Pre-compute this via nrzi_encode.
    expected_symbols_for(&crate::modulate::preamble_bits(len))
}

/// Build the expected I-channel amplitudes for an arbitrary preamble bit pattern.
pub fn expected_symbols_for(bits: &[bool]) -> Vec<f32> {
    let phases = nrzi_encode(bits);
    phases
        .iter()
        .map(|&neg| if neg { -1.0f32 } else { 1.0f32 })
        .collect()
}

// ── IQ demodulation ───────────────────────────────────────────────────────────

/// Mix `samples` with I and Q reference carriers, apply the matched filter
/// (half-Hann w_tail, 1→0) and integrate over each symbol period.
///
/// Returns `(i_values, q_values)` — one value per symbol.
fn demodulate_iq(
    samples: &[f32],
    n: usize,
    fc: f32,
    fs: f32,
    offset: usize,
) -> (Vec<f32>, Vec<f32>) {
    let effective = &samples[offset.min(samples.len())..];
    let n_syms = effective.len() / n;
    let two_pi = 2.0 * PI;

    let mut i_out = Vec::with_capacity(n_syms);
    let mut q_out = Vec::with_capacity(n_syms);

    for sym_idx in 0..n_syms {
        let sym_start = sym_idx * n;
        let mut i_sum = 0.0f32;
        let mut q_sum = 0.0f32;
        let mut norm = 0.0f32;

        for i in 0..n {
            let global_n = (offset + sym_start + i) as f32;
            let sample = effective[sym_start + i];

            // Matched filter for the overlapping half-Hann modulator: the
            // decreasing half (w_tail = 1→0) correlates with the current
            // symbol's tail and is approximately orthogonal to the next
            // symbol's rising head, keeping ISI below the decision threshold.
            let window = 0.5 * (1.0 + (PI * i as f32 / n as f32).cos());

            let t = global_n / fs;
            let ci = (two_pi * fc * t).cos();
            let cq = -(two_pi * fc * t).sin();

            // Factor-of-2 compensates for the ½ in the carrier product.
            i_sum += sample * ci * window * 2.0;
            q_sum += sample * cq * window * 2.0;
            norm += window * window;
        }

        if norm > 1e-9 {
            i_sum /= norm;
            q_sum /= norm;
        }

        i_out.push(i_sum);
        q_out.push(q_sum);
    }

    (i_out, q_out)
}

/// Crossfade-ISI coefficient for the overlapping half-Hann pulse: the one-slot matched filter recovers
/// `r_k = a_k + β·a_{k+1}` where `β = Σ(w_head·w_tail)/Σw_tail² = 1/3` (same integrals as rectangular QPSK).
const CROSSFADE_ISI_BETA: f32 = 1.0 / 3.0;

/// Remove the crossfade ISI from the recovered symbol stream in place by stable backward substitution:
/// `r_k = a_k + β·a_{k+1}` is bidiagonal, so `a_k = r_k − β·a_{k+1}`. The tail symbols (successor→0) give
/// the terminal; noise is amplified by only `1/(1−β²) = 1.125` (+0.5 dB), far less than the several-dB
/// differential-margin loss the uncancelled `+β` bias costs. Crossfade (non-RRC) path only.
fn cancel_crossfade_isi(i_syms: &mut [f32], q_syms: &mut [f32]) {
    let beta = CROSSFADE_ISI_BETA;
    let n = i_syms.len().min(q_syms.len());
    for k in (0..n.saturating_sub(1)).rev() {
        i_syms[k] -= beta * i_syms[k + 1];
        q_syms[k] -= beta * q_syms[k + 1];
    }
}

// ── Differential phase detection (NRZI decode) ───────────────────────────────
/// Decode bits from consecutive complex (I, Q) symbol pairs.
///
/// `Re(z[k] * conj(z[k−1]))` is positive when the phase is the same
/// ("0" bit / no flip) and negative when the phase has flipped ("1" bit).
fn differential_decode(iq: &[(f32, f32)]) -> Vec<bool> {
    iq.windows(2)
        .map(|w| {
            let (i0, q0) = w[0];
            let (i1, q1) = w[1];
            // Real part of z1 * conj(z0) = i1*i0 + q1*q0
            let dot = i1 * i0 + q1 * q0;
            dot < 0.0 // negative → phase flipped → bit "1"
        })
        .collect()
}

// ── Bit/byte helpers ──────────────────────────────────────────────────────────

/// Pack LSB-first bits into bytes.  Trailing incomplete bytes are zero-padded.
pub(crate) fn bits_to_bytes(bits: &[bool]) -> Vec<u8> {
    bits.chunks(8)
        .map(|chunk| {
            chunk
                .iter()
                .enumerate()
                .fold(0u8, |acc, (i, &b)| acc | ((b as u8) << i))
        })
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modulate::bytes_to_bits;

    #[test]
    fn bits_to_bytes_round_trip() {
        let original = b"Hello";
        let bits = bytes_to_bits(original);
        let back = bits_to_bytes(&bits);
        assert_eq!(&back[..original.len()], original);
    }

    #[test]
    fn differential_decode_same_phase() {
        // All same phase → all 0 bits
        let iq: Vec<(f32, f32)> = vec![(1.0, 0.0); 9];
        let bits = differential_decode(&iq);
        assert!(bits.iter().all(|&b| !b));
    }

    #[test]
    fn differential_decode_alternating_phase() {
        // Alternating phases → alternating 1,0,1,0,...
        // Actually alternating +1/-1 means:
        // (1,0),(−1,0): dot=−1 → 1
        // (−1,0),(1,0): dot=−1 → 1
        // all 1s
        let iq: Vec<(f32, f32)> = (0..9)
            .map(|i| (if i % 2 == 0 { 1.0 } else { -1.0 }, 0.0))
            .collect();
        let bits = differential_decode(&iq);
        assert!(bits.iter().all(|&b| b));
    }

    #[test]
    fn loopback_round_trip_bpsk250_non_rrc() {
        // Regression guard: BPSK250 non-RRC (Hann) path must round-trip cleanly.
        // This path does NOT apply LMS equalization (Hann integration is sufficient;
        // LMS decision-directed degrades fading-channel FEC performance).
        use crate::modulate::bpsk_modulate;
        let cfg = ModulationConfig {
            mode: "BPSK250".to_string(),
            sample_rate: 8000,
            center_frequency: 1500.0,
            ..ModulationConfig::default()
        };
        let original = b"OpenPulseHF";
        let samples = bpsk_modulate(original, &cfg).unwrap();
        let recovered = bpsk_demodulate(&samples, &cfg).unwrap();
        assert!(
            recovered.len() >= original.len(),
            "Recovered {} bytes, expected at least {}",
            recovered.len(),
            original.len()
        );
        assert_eq!(
            &recovered[..original.len()],
            original,
            "BPSK250 non-RRC clean loopback must recover payload exactly"
        );
    }

    #[test]
    fn loopback_round_trip() {
        use crate::modulate::bpsk_modulate;
        let cfg = ModulationConfig {
            mode: "BPSK100".to_string(),
            sample_rate: 8000,
            center_frequency: 1500.0,
            ..ModulationConfig::default()
        };
        let original = b"AB";
        let samples = bpsk_modulate(original, &cfg).unwrap();
        let recovered = bpsk_demodulate(&samples, &cfg).unwrap();
        assert_eq!(&recovered[..original.len()], original);
    }

    /// Timing lock at a 90° carrier phase: with the old I-only correlation the
    /// preamble energy lives entirely in Q at this phase, the metric collapses
    /// to ~0 for every offset, and the search picks an arbitrary (wrong)
    /// timing.  fc=2000 Hz at fs=8000 gives a carrier phase step of exactly
    /// π/2 per sample, so one prepended silent sample puts the preamble start
    /// at carrier phase 90°.
    #[test]
    fn bpsk250_timing_correct_at_carrier_90_degrees() {
        use crate::modulate::bpsk_modulate;
        let fc = 2000.0f32;
        let cfg = ModulationConfig {
            mode: "BPSK250".to_string(),
            sample_rate: 8000,
            center_frequency: fc,
            ..ModulationConfig::default()
        };
        let payload: Vec<u8> = (0u8..64).collect();
        let signal = bpsk_modulate(&payload, &cfg).unwrap();
        let mut samples = vec![0.0f32; 1];
        samples.extend_from_slice(&signal);

        let recovered = bpsk_demodulate(&samples, &cfg).unwrap();
        assert_eq!(&recovered[..payload.len()], &payload[..]);
    }

    /// Same carrier-90° regression for the RRC path, whose old baseband search
    /// was additionally polarity-sensitive (signed I-only score, no abs).
    #[test]
    fn bpsk250_rrc_timing_correct_at_carrier_90_degrees() {
        use crate::modulate::bpsk_modulate;
        let fc = 2000.0f32;
        let cfg = ModulationConfig {
            mode: "BPSK250-RRC".to_string(),
            sample_rate: 8000,
            center_frequency: fc,
            ..ModulationConfig::default()
        };
        let payload: Vec<u8> = (0u8..64).collect();
        let signal = bpsk_modulate(&payload, &cfg).unwrap();
        let mut samples = vec![0.0f32; 1];
        samples.extend_from_slice(&signal);

        let recovered = bpsk_demodulate(&samples, &cfg).unwrap();
        assert_eq!(&recovered[..payload.len()], &payload[..]);
    }

    /// Sample-rate-offset acceptance (review A13): a full 255-byte BPSK250-RRC
    /// frame decodes through a 150 ppm resampling — the two-free-running-
    /// sound-card condition.  The frame is ~66k samples, so 150 ppm drifts the
    /// ISI-free sampling instant by ~10 samples (31% of the 32-sample symbol
    /// period); the fixed-stride timing path cannot decode this.
    #[test]
    fn bpsk250_rrc_decodes_through_150ppm_sample_rate_offset() {
        use crate::modulate::bpsk_modulate;
        let cfg = ModulationConfig {
            mode: "BPSK250-RRC".to_string(),
            sample_rate: 8000,
            center_frequency: 1500.0,
            ..ModulationConfig::default()
        };
        let payload: Vec<u8> = (0..255u8).collect();
        let tx = bpsk_modulate(&payload, &cfg).unwrap();

        // Linear resampling models the receiver clock running 150 ppm fast.
        let ratio = 1.0f64 + 150e-6;
        let out_len = (tx.len() as f64 / ratio) as usize;
        let rx: Vec<f32> = (0..out_len)
            .map(|k| {
                let pos = k as f64 * ratio;
                let base = pos.floor() as usize;
                let t = (pos - base as f64) as f32;
                if base + 1 < tx.len() {
                    tx[base] * (1.0 - t) + tx[base + 1] * t
                } else {
                    tx[tx.len() - 1]
                }
            })
            .collect();

        let recovered = bpsk_demodulate(&rx, &cfg).unwrap();
        assert!(
            recovered.len() >= payload.len(),
            "recovered only {} bytes",
            recovered.len()
        );
        let first_bad = recovered[..payload.len()]
            .iter()
            .zip(payload.iter())
            .position(|(a, b)| a != b);
        let n_bad = recovered[..payload.len()]
            .iter()
            .zip(payload.iter())
            .filter(|(a, b)| a != b)
            .count();
        let bit_errs: u32 = recovered[..payload.len()]
            .iter()
            .zip(payload.iter())
            .map(|(a, b)| (a ^ b).count_ones())
            .sum();
        assert_eq!(
            &recovered[..payload.len()],
            &payload[..],
            "BPSK250-RRC 150 ppm SRO: first mismatch byte {first_bad:?}, {n_bad}/255 bad bytes, {bit_errs} bit errors"
        );
    }

    #[test]
    fn afc_estimate_near_zero_for_matched_carrier() {
        use crate::modulate::bpsk_modulate;
        let cfg = ModulationConfig {
            mode: "BPSK250".to_string(),
            sample_rate: 8000,
            center_frequency: 1500.0,
            ..ModulationConfig::default()
        };
        let samples = bpsk_modulate(b"HelloWorld", &cfg).unwrap();
        // Estimate AFC with the correct carrier — should be near zero.
        let offset = afc_estimate_hz(&samples, &cfg).expect("afc estimate");
        assert!(
            offset.abs() < 5.0,
            "expected near-zero AFC offset, got {offset:.2} Hz"
        );
    }

    #[test]
    fn afc_estimate_detects_known_offset() {
        use crate::modulate::bpsk_modulate;
        // Modulate at 1500 Hz, then estimate AFC with a 20 Hz lower reference.
        // The estimator should report ≈ +20 Hz (signal is above reference).
        let true_fc = 1500.0f32;
        let ref_fc = 1480.0f32;
        let cfg_tx = ModulationConfig {
            mode: "BPSK250".to_string(),
            sample_rate: 8000,
            center_frequency: true_fc,
            ..ModulationConfig::default()
        };
        let cfg_rx = ModulationConfig {
            mode: "BPSK250".to_string(),
            sample_rate: 8000,
            center_frequency: ref_fc,
            ..ModulationConfig::default()
        };
        let samples = bpsk_modulate(b"HelloWorld", &cfg_tx).unwrap();
        let offset = afc_estimate_hz(&samples, &cfg_rx).expect("afc estimate");
        // Allow ±8 Hz tolerance (estimator range is baud/4 = 62.5 Hz for BPSK250).
        assert!(
            (offset - 20.0).abs() < 8.0,
            "expected ≈+20 Hz AFC offset, got {offset:.2} Hz"
        );
    }

    #[test]
    fn afc_wide_detects_large_offset() {
        use crate::modulate::bpsk_modulate;
        // 150 Hz offset — beyond the narrow ±62.5 Hz range; wide Goertzel must acquire.
        let true_fc = 1650.0f32;
        let ref_fc = 1500.0f32;
        let cfg_tx = ModulationConfig {
            mode: "BPSK250".to_string(),
            sample_rate: 8000,
            center_frequency: true_fc,
            ..ModulationConfig::default()
        };
        let cfg_rx = ModulationConfig {
            mode: "BPSK250".to_string(),
            sample_rate: 8000,
            center_frequency: ref_fc,
            ..ModulationConfig::default()
        };
        let samples = bpsk_modulate(b"HelloWorldABCDEFGHIJKLMNOP", &cfg_tx).unwrap();
        let offset = afc_estimate_hz(&samples, &cfg_rx).expect("afc estimate");
        // Goertzel step is 12.5 Hz at baseband; allow ±15 Hz tolerance.
        assert!(
            (offset - 150.0).abs() < 15.0,
            "expected ≈+150 Hz AFC offset, got {offset:.2} Hz"
        );
    }

    #[cfg(feature = "gpu")]
    #[test]
    fn gpu_demodulate_matches_cpu() {
        use crate::modulate::bpsk_modulate;
        let cfg = ModulationConfig {
            mode: "BPSK250".to_string(),
            sample_rate: 8000,
            center_frequency: 1500.0,
            ..ModulationConfig::default()
        };
        let payload = b"AB";
        let samples = bpsk_modulate(payload, &cfg).unwrap();

        let cpu_out = bpsk_demodulate(&samples, &cfg).unwrap();

        let Some(ctx) = openpulse_gpu::GpuContext::init() else {
            eprintln!("skipping gpu_demodulate_matches_cpu: no compatible adapter");
            return;
        };
        let gpu_out = bpsk_demodulate_with_gpu(&samples, &cfg, &ctx).unwrap();

        assert_eq!(
            &cpu_out[..payload.len()],
            payload,
            "CPU path should recover payload"
        );
        assert_eq!(cpu_out, gpu_out, "GPU demodulation must match CPU output");
    }

    // ── LMS profile and Watterson channel stress tests ──────────────────────

    #[test]
    fn lms_profile_bpsk250_uses_dfe() {
        let (fwd, dfe, mu) = lms_profile("BPSK250");
        assert_eq!(fwd, 9);
        assert_eq!(dfe, 2);
        assert!(mu < 0.02, "BPSK250 mu should be tighter than baseline");
    }

    #[test]
    fn lms_profile_narrow_modes_use_baseline() {
        for mode in ["BPSK31", "BPSK63", "BPSK100"] {
            let (fwd, dfe, mu) = lms_profile(mode);
            assert_eq!(fwd, 7, "{mode}: expect 7-tap fwd");
            assert_eq!(dfe, 0, "{mode}: expect no DFE");
            assert!((mu - 0.02).abs() < 1e-6, "{mode}: expect mu=0.02");
        }
    }

    #[test]
    fn bpsk250_watterson_moderate_f1_decode_coverage() {
        use crate::modulate::bpsk_modulate;
        use openpulse_channel::watterson::WattersonChannel;
        use openpulse_channel::{ChannelModel, WattersonConfig};

        let cfg = ModulationConfig {
            mode: "BPSK250".to_string(),
            sample_rate: 8000,
            center_frequency: 1500.0,
            ..ModulationConfig::default()
        };
        let payload: Vec<u8> = (0..96u8).map(|v| v ^ 0x5A).collect();
        let tx = bpsk_modulate(&payload, &cfg).expect("modulate");

        let bit_error_rate = |expected: &[u8], got: &[u8]| -> f32 {
            let n = expected.len().min(got.len());
            if n == 0 {
                return 1.0;
            }
            let bit_errors: u32 = expected
                .iter()
                .zip(got.iter())
                .take(n)
                .map(|(&a, &b)| (a ^ b).count_ones())
                .sum();
            bit_errors as f32 / (n as f32 * 8.0)
        };

        let mut decoded = 0usize;
        let mut good_ber = 0usize;
        let mut best_ber = f32::INFINITY;
        for seed in [
            0x6101u64, 0x6102, 0x6103, 0x6104, 0x6105, 0x6106, 0x6107, 0x6108,
        ] {
            let mut ch = WattersonChannel::new(WattersonConfig::moderate_f1(Some(seed)))
                .expect("watterson moderate f1");
            let rx = ch.apply(&tx);
            if let Ok(recovered) = bpsk_demodulate(&rx, &cfg) {
                if recovered.len() >= payload.len() {
                    decoded += 1;
                    let ber = bit_error_rate(&payload, &recovered[..payload.len()]);
                    best_ber = best_ber.min(ber);
                    if ber <= 0.12 {
                        good_ber += 1;
                    }
                }
            }
        }

        assert!(
            decoded >= 6,
            "BPSK250 moderate_f1 should decode payload length in most trials, decoded={decoded}/8"
        );
        assert!(
            good_ber >= 2,
            "BPSK250 moderate_f1 should include at least two low-BER decodes, good_ber={good_ber}/8, best_ber={best_ber:.3}"
        );
    }

    #[test]
    fn bpsk250_watterson_poor_f1_decode_presence() {
        use crate::modulate::bpsk_modulate;
        use openpulse_channel::watterson::WattersonChannel;
        use openpulse_channel::{ChannelModel, WattersonConfig};

        let cfg = ModulationConfig {
            mode: "BPSK250".to_string(),
            sample_rate: 8000,
            center_frequency: 1500.0,
            ..ModulationConfig::default()
        };
        let payload: Vec<u8> = (0..96u8).collect();
        let tx = bpsk_modulate(&payload, &cfg).expect("modulate");

        let mut decoded = 0usize;
        let mut best_ber = f32::INFINITY;
        for seed in [0x6201u64, 0x6202, 0x6203, 0x6204, 0x6205, 0x6206] {
            let mut ch = WattersonChannel::new(WattersonConfig::poor_f1(Some(seed)))
                .expect("watterson poor f1");
            let rx = ch.apply(&tx);
            if let Ok(recovered) = bpsk_demodulate(&rx, &cfg) {
                if recovered.len() >= payload.len() {
                    decoded += 1;
                    let ber: f32 = payload
                        .iter()
                        .zip(recovered.iter())
                        .take(payload.len())
                        .map(|(&a, &b)| (a ^ b).count_ones() as f32)
                        .sum::<f32>()
                        / (payload.len() as f32 * 8.0);
                    best_ber = best_ber.min(ber);
                }
            }
        }

        assert!(
            decoded >= 1,
            "BPSK250 poor_f1 should produce at least one full-length decode, decoded={decoded}/6"
        );
        assert!(
            best_ber < 0.5,
            "BPSK250 poor_f1 best BER must beat random (0.5), got best_ber={best_ber:.3}"
        );
    }
}
