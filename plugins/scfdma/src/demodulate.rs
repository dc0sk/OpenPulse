//! SC-FDMA demodulation: samples → FFT → LS/MMSE equalize → IDFT → payload.

use num_complex::Complex32;
use rustfft::FftPlanner;

use crate::channel::{
    apply_timing_deramp, deramp_timing, estimate_noise_var, estimate_rician_k_linear,
    flat_ce_debias, flat_channel_estimate, mmse_equalize, mmse_llr_noise_var, pilot_comb_noise_var,
    pilot_diff_noise_var, pilot_positions, timing_ramp_slope, CeSolver, DelayCe,
};
use crate::modulate::{modulate_with_params, preamble_payload};
use crate::params::PILOT_AMPLITUDE;
use crate::params::{params_for_mode, ScFdmaParams, CP, FFT_SIZE, SAMPLE_RATE, SYM_LEN};
use openpulse_dsp::constellation::{
    constellation_points, demap_symbol, estimate_decision_noise_var, symbol_llrs,
};

// Re-export from the canonical core implementation so the plugin exposes the
// same public path without duplicating the logic.
use openpulse_core::error::ModemError;
pub use openpulse_core::fec::combine_llrs_weighted;
use openpulse_core::len_prefix::{
    decode_len_prefix, decode_len_prefix_llrs, LEN_PREFIX_BITS, LEN_PREFIX_BYTES,
};
// Canonical shared implementation (openpulse-dsp); re-exported because the
// lib.rs acquisition regression test references it via this module's path.
#[cfg(test)]
pub(crate) use openpulse_dsp::acquisition::quadrature;
use openpulse_dsp::acquisition::IqMatchedFilter;

/// Frequency-shift a real passband signal DOWN by `delta_hz`, bringing a signal centred at
/// `1500 + delta_hz` back to the nominal 1500 Hz the demodulator's fixed subcarrier bins expect.
///
/// The engine supplies its settled AFC correction as `center_frequency - 1500` (the measured dial
/// offset); applying it here — instead of rejecting a non-nominal centre — lets SC-FDMA acquire
/// off-frequency signals (its own ±4 Hz sync tolerance otherwise fails on any real dial error).
/// Uses the analytic-signal (Hilbert) mix `Re{(s + j·H{s})·e^{-jθ}}` so the shift is image-free.
pub fn mix_to_nominal(samples: &[f32], delta_hz: f32) -> Vec<f32> {
    if delta_hz.abs() < 0.05 {
        return samples.to_vec();
    }
    let h = openpulse_dsp::acquisition::quadrature(samples);
    let w = std::f32::consts::TAU * delta_hz / SAMPLE_RATE as f32;
    samples
        .iter()
        .zip(h.iter())
        .enumerate()
        .map(|(n, (&s, &hs))| {
            let (sin, cos) = (w * n as f32).sin_cos();
            s * cos + hs * sin
        })
        .collect()
}

/// EMA-smooth the channel estimate across symbols (temporal averaging of CE noise on slow/static
/// channels). Resets when the raw estimate jumps relative to the running one — a fast fade or the
/// first symbol — so it never smears a genuine channel change. Returns the estimate to equalize with;
/// callers keep the *raw* estimate for `estimate_noise_var` so the debias (which assumes the residual
/// is only the fit-rejected noise) stays valid.
fn smooth_ce(ema: &mut Option<Vec<Complex32>>, raw: &[Complex32]) -> Vec<Complex32> {
    const ALPHA: f32 = 0.5; // ~2-symbol memory
    const JUMP_REL: f32 = 0.35; // reset if ||raw-ema||²/||ema||² exceeds this
    match ema {
        Some(prev) if prev.len() == raw.len() => {
            let (mut dnum, mut dden) = (0.0f32, 0.0f32);
            for (r, p) in raw.iter().zip(prev.iter()) {
                dnum += (r - p).norm_sqr();
                dden += p.norm_sqr();
            }
            if dden <= 1e-9 || dnum / dden > JUMP_REL {
                prev.copy_from_slice(raw);
            } else {
                for (p, r) in prev.iter_mut().zip(raw.iter()) {
                    *p = *r * ALPHA + *p * (1.0 - ALPHA);
                }
            }
            prev.clone()
        }
        _ => {
            *ema = Some(raw.to_vec());
            raw.to_vec()
        }
    }
}

/// One symbol's frequency-domain observation: the de-ramped `FFT_SIZE`-point spectrum.
type SymbolSpectrum = Vec<Complex32>;

/// Frame-level front end shared by every demodulation path: locate the payload, FFT and de-ramp each
/// symbol, then measure the noise variance **once for the whole frame**.
///
/// A single symbol's noise estimate has only a handful of degrees of freedom (~50 % relative error),
/// but σ² is a property of the receiver, not of the symbol. Averaging it across the frame is free
/// variance reduction, and it matters twice over: it sets the Wiener ridge of the channel estimator,
/// and `symbol_llrs` divides by it — a per-symbol σ² mis-weights whole symbols against each other in
/// the soft-Viterbi metric and in the majority-protected length prefix.
struct FrameFront {
    spectra: Vec<SymbolSpectrum>,
    /// Frame-mean per-bin noise variance.
    noise_var: f32,
    /// Frame-mean pilot power, the ridge's reference.
    chan_power: f32,
}

/// Per-subcarrier channel-estimate error variance `ε²_k`, or an empty slice for the localized layout.
///
/// Frame-constant: it depends only on the estimator's noise gain and the frame's σ². Feeds
/// [`mmse_llr_noise_var`], which without it treats the channel estimate as exact.
fn ce_error_var(front: &FrameFront, solver: Option<&CeSolver>) -> Vec<f32> {
    match solver {
        Some(s) => s.ce_error_var_per_sc(front.noise_var),
        None => Vec::new(),
    }
}

impl FrameFront {
    /// FFT every complete symbol in `samples` (already advanced past the preamble), then
    /// [`FrameFront::from_spectra`].
    fn new(samples: &[f32], p: &ScFdmaParams, ce: Option<&DelayCe>) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let fft_scale = 1.0 / (FFT_SIZE as f32).sqrt();
        let n_syms = samples.len() / SYM_LEN;

        let mut spectra = Vec::with_capacity(n_syms);
        for sym_idx in 0..n_syms {
            let start = sym_idx * SYM_LEN + CP;
            if start + FFT_SIZE > samples.len() {
                break;
            }
            let mut freq: Vec<Complex32> = samples[start..start + FFT_SIZE]
                .iter()
                .map(|&s| Complex32::new(s * fft_scale, 0.0))
                .collect();
            fft.process(&mut freq);
            spectra.push(freq);
        }
        Self::from_spectra(spectra, p, ce)
    }

    /// De-ramp already-transformed spectra and measure the frame's noise and channel power.
    ///
    /// The GPU paths batch the FFT on device and enter here, so they cannot silently drift from the
    /// CPU front end (they used to skip `deramp_timing` entirely — a divergence that only appeared
    /// under sample-rate offset).
    ///
    /// σ² is the **smaller** of two estimators that fail in opposite directions: the comb estimator
    /// over-reports on channels with delay spread (leakage into its noise taps), the adjacent-symbol
    /// difference over-reports under fast fading and residual carrier offset. Neither can be trusted
    /// alone; the minimum is right whenever at least one of the two assumptions holds.
    fn from_spectra(
        mut spectra: Vec<SymbolSpectrum>,
        p: &ScFdmaParams,
        ce: Option<&DelayCe>,
    ) -> Self {
        // Remove the sampling-frequency-offset / residual-timing phase ramp before de-spreading
        // (mirrors the OFDM path); critical for SC-FDMA under SRO.
        //
        // The slope is fitted across ALL symbols at once. The offset is constant over a frame, so a
        // frame-wide fit cuts estimator noise by sqrt(n_symbols) — which is what makes this usable for
        // the localized (block-pilot) layout, whose 4 pilots give only 3 adjacent products per symbol.
        //
        // The two layouts are fitted differently, for a physical reason:
        //
        // * Interleaved (13 pilots): fit PER SYMBOL. Under a sample-rate offset the ramp grows with
        //   symbol index, so a per-symbol fit tracks it and a frame-wide average does not. Averaging
        //   here measurably decalibrated the LLRs (`llr_reliability` fired on SCFDMA52-16QAM).
        // * Localized (4 pilots = 3 adjacent products): fit ONCE across the frame. A per-symbol
        //   estimate is so noisy that de-rotating 65 subcarriers by it is worse than not correcting —
        //   it broke SCFDMA52-LP on AWGN at 20 dB. This layout is a flat-channel demonstrator that
        //   does not claim SRO tracking, so trading the growing-ramp term for sqrt(n_symbols) less
        //   estimator noise is the right trade for it and the wrong one for the interleaved modes.
        if p.localized {
            let views: Vec<&[Complex32]> = spectra.iter().map(|f| &f[..]).collect();
            if let Some(slope) = timing_ramp_slope(p, &views) {
                for freq in &mut spectra {
                    apply_timing_deramp(p, freq, slope);
                }
            }
        } else {
            for freq in &mut spectra {
                deramp_timing(p, freq);
            }
        }

        let Some(ce) = ce else {
            return Self {
                spectra,
                noise_var: 1e-6,
                chan_power: 1.0,
            };
        };

        let (mut comb_sum, mut comb_n) = (0.0f32, 0usize);
        let (mut cp_sum, mut cp_n) = (0.0f32, 0usize);
        for freq in &spectra {
            if let Some(nv) = pilot_comb_noise_var(p, freq) {
                comb_sum += nv;
                comb_n += 1;
            }
            cp_sum += ce.channel_power(freq);
            cp_n += 1;
        }
        let (mut diff_sum, mut diff_n) = (0.0f32, 0usize);
        for pair in spectra.windows(2) {
            if let Some(nv) = pilot_diff_noise_var(p, &pair[0], &pair[1]) {
                diff_sum += nv;
                diff_n += 1;
            }
        }

        let comb = (comb_n > 0).then(|| comb_sum / comb_n as f32);
        let diff = (diff_n > 0).then(|| diff_sum / diff_n as f32);
        let noise_var = match (comb, diff) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => 1e-6,
        }
        .max(1e-9);
        let chan_power = if cp_n > 0 {
            (cp_sum / cp_n as f32).max(1e-12)
        } else {
            1.0
        };
        Self {
            spectra,
            noise_var,
            chan_power,
        }
    }

    fn is_empty(&self) -> bool {
        self.spectra.is_empty()
    }

    /// Wiener channel-estimate solver for this frame, or `None` for the flat (localized) layout.
    fn solver(&self, p: &ScFdmaParams, ce: &DelayCe) -> Option<CeSolver> {
        (!p.localized).then(|| ce.solver(self.noise_var, self.chan_power))
    }

    /// Per-symbol channel estimate and the σ² to equalize and compute LLRs with.
    fn estimate(
        &self,
        p: &ScFdmaParams,
        solver: Option<&CeSolver>,
        freq: &[Complex32],
    ) -> Vec<Complex32> {
        match solver {
            Some(s) => s.estimate(freq),
            None => flat_channel_estimate(p, freq),
        }
    }

    /// Frame σ² for equalization. The localized path has no comb, so it falls back to the
    /// (per-symbol) pilot-residual estimator at its own debias.
    fn noise_var_for(
        &self,
        p: &ScFdmaParams,
        solver: Option<&CeSolver>,
        freq: &[Complex32],
        h: &[Complex32],
    ) -> f32 {
        match solver {
            Some(_) => self.noise_var,
            None => estimate_noise_var(p, freq, h, flat_ce_debias(p)).max(1e-6),
        }
    }
}

pub fn scfdma_demodulate(samples: &[f32], mode: &str) -> Result<Vec<u8>, ModemError> {
    let p = params_for_mode(mode).ok_or_else(|| {
        ModemError::Configuration(format!("SC-FDMA plugin: unknown mode '{mode}'"))
    })?;
    demodulate_with_params(samples, &p)
}

/// Demodulate SC-FDMA samples and return per-bit soft values (LLRs).
///
/// Positive values indicate bit 0 is more likely; negative values indicate bit 1.
pub fn scfdma_demodulate_soft(samples: &[f32], mode: &str) -> Result<Vec<f32>, ModemError> {
    let p = params_for_mode(mode).ok_or_else(|| {
        ModemError::Configuration(format!("SC-FDMA plugin: unknown mode '{mode}'"))
    })?;
    Ok(demodulate_soft_with_params(samples, &p)?.llrs)
}

/// Per-frame quality metrics produced during soft demodulation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SoftFrameMetrics {
    /// Mean decision-residual noise variance across demodulated symbols.
    ///
    /// A distance-to-nearest-constellation-point metric, so it *saturates* once symbol errors are
    /// common: it can only ever under-report a change in noise power. Use [`Self::mean_pilot_noise_var`]
    /// when a calibrated noise measurement is wanted.
    pub mean_noise_var: f32,
    /// Frame noise variance measured from the pilots — a direct, non-saturating noise-power estimate,
    /// and the σ² the LLRs were scaled by.
    pub mean_pilot_noise_var: f32,
    /// Mean estimated Rician K-factor in dB across symbols.
    pub mean_rician_k_db: f32,
    /// Number of symbols included in the metric averages.
    pub symbols_used: usize,
}

/// Soft demodulation output with reliability metrics for adaptive combining.
#[derive(Debug, Clone, PartialEq)]
pub struct SoftDemodOutput {
    /// Payload LLRs (positive => likely 0, negative => likely 1).
    pub llrs: Vec<f32>,
    /// Aggregated frame metrics measured from pilots/channel estimate.
    pub metrics: SoftFrameMetrics,
}

/// Demodulate SC-FDMA samples into LLRs and frame quality metrics.
pub fn scfdma_demodulate_soft_with_metrics(
    samples: &[f32],
    mode: &str,
) -> Result<SoftDemodOutput, ModemError> {
    let p = params_for_mode(mode).ok_or_else(|| {
        ModemError::Configuration(format!("SC-FDMA plugin: unknown mode '{mode}'"))
    })?;
    demodulate_soft_with_params(samples, &p)
}

fn demodulate_with_params(samples: &[f32], p: &ScFdmaParams) -> Result<Vec<u8>, ModemError> {
    let sync = modulate_with_params(&preamble_payload(p), p);
    if samples.len() < sync.len() + SYM_LEN {
        return Err(ModemError::Demodulation("signal too short".into()));
    }

    let Some(offset) = find_sync_offset(samples, &sync) else {
        return Err(ModemError::Demodulation("no SC-FDMA sync detected".into()));
    };
    let payload_start = offset + sync.len();
    if payload_start >= samples.len() {
        return Err(ModemError::Demodulation(
            "SC-FDMA frame truncated after sync".into(),
        ));
    }

    let samples = &samples[payload_start..];
    let n_syms = samples.len() / SYM_LEN;
    if n_syms == 0 {
        return Err(ModemError::Demodulation(
            "SC-FDMA frame truncated after sync".into(),
        ));
    }

    let mut planner = FftPlanner::<f32>::new();
    // N_data-point IDFT to undo DFT precoding.
    let idft = planner.plan_fft_inverse(p.n_data);
    let idft_scale = 1.0 / (p.n_data as f32).sqrt();

    // Pilot → subcarrier interpolator; the mode-constant part is built once, the Wiener solver once
    // per frame (it depends on the frame's noise-to-channel-power ratio).
    let ce = DelayCe::new(p);
    let front = FrameFront::new(samples, p, (!p.localized).then_some(&ce));
    if front.is_empty() {
        return Err(ModemError::Demodulation(
            "SC-FDMA frame truncated after sync".into(),
        ));
    }
    let solver = front.solver(p, &ce);
    let ce_err = ce_error_var(&front, solver.as_ref());

    let mut bits: Vec<bool> = Vec::with_capacity(n_syms * p.bits_per_symbol());
    let mut ema_h: Option<Vec<Complex32>> = None;

    for freq in &front.spectra {
        // Step 2: channel estimation + MMSE equalization. The EMA-smoothed estimate equalizes so CE
        // noise averages across symbols on a slow channel.
        let raw_h = front.estimate(p, solver.as_ref(), freq);
        let noise_var = front.noise_var_for(p, solver.as_ref(), freq, &raw_h);
        let h_est = smooth_ce(&mut ema_h, &raw_h);
        // MMSE attenuates by `alpha_avg`; undo it before demapping so QAM hard decisions are not
        // biased toward the origin (mirrors the soft path — PSK is angle-only, so unaffected there).
        let (_, alpha_avg) = mmse_llr_noise_var(p, &h_est, noise_var, &ce_err);
        let mut equalized = mmse_equalize(p, freq, &h_est, noise_var);

        // Step 3: IDFT(N_data) — undo DFT precoding; scale to preserve energy.
        idft.process(&mut equalized);
        let data_syms: Vec<Complex32> = equalized
            .iter()
            .map(|c| c * idft_scale / alpha_avg)
            .collect();

        // Step 4: Demap recovered symbols according to the constellation order.
        for sym in &data_syms {
            let b = demap_symbol(*sym, p.bits_per_sc);
            for bit_pos in 0..p.bits_per_sc {
                bits.push((b >> bit_pos) & 1 == 1);
            }
        }
    }

    let raw = bits_to_bytes(&bits);

    // Strip the majority-protected length prefix.
    let Some(payload_len) = decode_len_prefix(&raw) else {
        return Err(ModemError::Demodulation(
            "SC-FDMA frame shorter than length prefix".into(),
        ));
    };
    let available = raw.len() - LEN_PREFIX_BYTES;
    let take = (payload_len as usize).min(available);
    Ok(raw[LEN_PREFIX_BYTES..LEN_PREFIX_BYTES + take].to_vec())
}

/// Equalized, de-spread data symbols on the [`constellation_points`] scale (FFT → DFT-CE → MMSE →
/// IDFT, with the `alpha_avg` attenuation undone and the EMA channel smoothing of the decode path).
/// Shared front-end for the display scatter and the symbol-domain SNR estimate. `None` if the mode is
/// unknown or sync fails.
fn equalized_data_symbols(samples: &[f32], p: &ScFdmaParams) -> Option<Vec<Complex32>> {
    let sync = modulate_with_params(&preamble_payload(p), p);
    if samples.len() < sync.len() + SYM_LEN {
        return None;
    }
    let offset = find_sync_offset(samples, &sync)?;
    let payload_start = offset + sync.len();
    if payload_start >= samples.len() {
        return None;
    }
    let samples = &samples[payload_start..];
    let n_syms = samples.len() / SYM_LEN;
    if n_syms == 0 {
        return None;
    }

    let mut planner = FftPlanner::<f32>::new();
    let idft = planner.plan_fft_inverse(p.n_data);
    let idft_scale = 1.0 / (p.n_data as f32).sqrt();
    let ce = DelayCe::new(p);
    let front = FrameFront::new(samples, p, (!p.localized).then_some(&ce));
    if front.is_empty() {
        return None;
    }
    let solver = front.solver(p, &ce);
    let ce_err = ce_error_var(&front, solver.as_ref());

    let mut ema_h: Option<Vec<Complex32>> = None;
    let mut syms: Vec<Complex32> = Vec::with_capacity(n_syms * p.n_data);
    for freq in &front.spectra {
        let raw_h = front.estimate(p, solver.as_ref(), freq);
        let noise_var = front.noise_var_for(p, solver.as_ref(), freq, &raw_h);
        let h_est = smooth_ce(&mut ema_h, &raw_h);
        let (_, alpha_avg) = mmse_llr_noise_var(p, &h_est, noise_var, &ce_err);
        let mut equalized = mmse_equalize(p, freq, &h_est, noise_var);
        idft.process(&mut equalized);
        syms.extend(equalized.iter().map(|c| c * idft_scale / alpha_avg));
    }
    Some(syms)
}

/// Per-subcarrier error-vector magnitude (dB), measured **before** the DFT de-spread.
///
/// SC-FDMA de-spreads with an IDFT, which averages every subcarrier into every output symbol. That
/// is the modulation's whole point on a selective channel — and it is also why a post-despread
/// measurement cannot tell a *narrowband* impairment from a *broadband* one: one ruined subcarrier
/// and a uniformly noisy band produce the same smeared constellation. `SCFDMA52-64QAM` fails on the
/// dual-soundcard hardware loopback while decoding cleanly in-process and while `SCFDMA52-32QAM` —
/// one constellation order down, same subcarriers, same pilots — decodes the *same* captured audio.
/// A decode-threshold sweep put the mode's AWGN floor at 14 dB against a cable measuring 71 dB SNR,
/// so whatever is doing the damage is not noise-like. This is the measurement that separates the
/// remaining possibilities, taken at the only point where the per-subcarrier structure still exists.
///
/// Returns one `(absolute subcarrier index, EVM dB)` pair per data subcarrier, in ascending
/// frequency order, averaged over every payload symbol in the frame. The absolute index is carried
/// rather than implied because the data-subcarrier map differs between pilot spacings — spacing 5
/// gives 52 data subcarriers, spacing 4 gives 49 — so a bare vector would not be comparable across
/// the `-P4` variant it most needs comparing against.
///
/// The reference is the receiver's own hard decision, re-spread back to the frequency domain: decide
/// each de-spread symbol, forward-DFT the decisions, and difference against what was equalized. So
/// this measures residual *after* equalization, which is what a channel-estimate or equalizer defect
/// would leave behind — not raw channel response. Diagnostic only: nothing in the decode path calls
/// it, so it costs a production receive nothing.
///
/// **Validity floor — read this before attributing damage to a subcarrier.** The reference is the
/// receiver's own hard decision, so the measurement degenerates once those decisions stop being
/// mostly right. Measured by sweeping SNR under a notch at a known subcarrier: the peak sits on the
/// right subcarrier down to about **−11 dB mean EVM**, and is lost by **−7.5 dB**. Above that floor
/// the *mean* is still meaningful (it keeps tracking SNR monotonically) but the per-subcarrier
/// profile is not — a badly damaged frame will show a large spread that says nothing about where the
/// damage is. `evm_localization_is_valid` pins the floor.
///
/// It also needs the frame near the start of the buffer: `find_sync_offset` did not locate the
/// preamble in a 12 s capture that it found immediately once trimmed to the burst. Trim before
/// measuring, or the function returns `None`.
///
/// `None` if the mode is unknown or sync fails.
pub fn scfdma_subcarrier_evm_db(samples: &[f32], mode: &str) -> Option<Vec<(usize, f32)>> {
    let p = params_for_mode(mode)?;
    let sync = modulate_with_params(&preamble_payload(&p), &p);
    if samples.len() < sync.len() + SYM_LEN {
        return None;
    }
    let offset = find_sync_offset(samples, &sync)?;
    let payload_start = offset + sync.len();
    if payload_start >= samples.len() {
        return None;
    }
    let samples = &samples[payload_start..];
    if samples.len() / SYM_LEN == 0 {
        return None;
    }

    let mut planner = FftPlanner::<f32>::new();
    let idft = planner.plan_fft_inverse(p.n_data);
    let dft = planner.plan_fft_forward(p.n_data);
    let idft_scale = 1.0 / (p.n_data as f32).sqrt();
    let ce = DelayCe::new(&p);
    let front = FrameFront::new(samples, &p, (!p.localized).then_some(&ce));
    if front.is_empty() {
        return None;
    }
    let solver = front.solver(&p, &ce);
    let ce_err = ce_error_var(&front, solver.as_ref());
    let points = constellation_points(p.bits_per_sc);

    let mut ema_h: Option<Vec<Complex32>> = None;
    let mut err_pow = vec![0.0f64; p.n_data];
    let mut ref_pow = vec![0.0f64; p.n_data];
    let mut n_syms = 0usize;

    for freq in &front.spectra {
        let raw_h = front.estimate(&p, solver.as_ref(), freq);
        let noise_var = front.noise_var_for(&p, solver.as_ref(), freq, &raw_h);
        let h_est = smooth_ce(&mut ema_h, &raw_h);
        let (_, alpha_avg) = mmse_llr_noise_var(&p, &h_est, noise_var, &ce_err);
        if alpha_avg.abs() < 1e-9 {
            continue;
        }
        let equalized = mmse_equalize(&p, freq, &h_est, noise_var);

        // De-spread, decide, and re-spread the decisions to rebuild what a clean frame would have
        // put on each subcarrier.
        let mut despread = equalized.clone();
        idft.process(&mut despread);
        let mut redecided: Vec<Complex32> = despread
            .iter()
            .map(|c| {
                let sym = c * idft_scale / alpha_avg;
                // Nearest constellation point by Euclidean distance, taken directly over the point
                // table rather than through `demap_symbol` — that returns a bit pattern, and mapping
                // it back to a point would just re-do this search through a lookup.
                let decided = points
                    .iter()
                    .min_by(|a, b| (sym - a.1).norm_sqr().total_cmp(&(sym - b.1).norm_sqr()))
                    .map(|(_, pt)| *pt)
                    .unwrap_or(sym);
                // Undo the same scaling, so the forward transform below lands back on `equalized`'s
                // scale rather than the constellation's.
                decided * alpha_avg / idft_scale
            })
            .collect();
        dft.process(&mut redecided);

        // rustfft is unnormalized, so a forward-after-inverse round trip carries a factor of n_data.
        let norm = 1.0 / p.n_data as f32;
        for k in 0..p.n_data {
            let reference = redecided[k] * norm;
            let residual = equalized[k] - reference;
            err_pow[k] += f64::from(residual.norm_sqr());
            ref_pow[k] += f64::from(reference.norm_sqr());
        }
        n_syms += 1;
    }

    if n_syms == 0 {
        return None;
    }

    // Data subcarriers in ascending absolute order — the same walk `mmse_equalize` uses to fill
    // `equalized`, so index `k` here is subcarrier `sc` there.
    let data_scs: Vec<usize> = (p.first_sc..=p.last_sc)
        .filter(|&sc| !crate::channel::is_pilot(&p, sc))
        .collect();

    Some(
        data_scs
            .into_iter()
            .enumerate()
            .map(|(k, sc)| {
                let evm_db = if ref_pow[k] > 0.0 {
                    10.0 * (err_pow[k] / ref_pow[k]).log10() as f32
                } else {
                    f32::NAN
                };
                (sc, evm_db)
            })
            .collect(),
    )
}

/// Equalized, de-spread constellation symbols for display — the real QAM scatter the receiver
/// recovers, normalized to RMS ≈ 1 and capped in point count. Returns `None` if the mode is unknown
/// or sync fails. Display-only.
pub fn scfdma_constellation(samples: &[f32], mode: &str) -> Option<Vec<(f32, f32)>> {
    let p = params_for_mode(mode)?;
    let syms = equalized_data_symbols(samples, &p)?;
    Some(normalize_constellation_for_display(&syms))
}

/// Symbol-domain RX SNR (dB) from the equalized SC-FDMA data symbols via [`qam_symbol_snr_db`]. Lets
/// the narrowband SL10 rung self-measure SNR (M2M4 reads garbage on a multicarrier envelope), so the
/// receiver-led ladder can climb through it into the OFDM rungs. `None` if sync fails.
pub fn estimate_snr_db(samples: &[f32], mode: &str) -> Option<f32> {
    let p = params_for_mode(mode)?;
    let syms = equalized_data_symbols(samples, &p)?;
    if syms.is_empty() {
        return None;
    }
    Some(openpulse_dsp::constellation::qam_symbol_snr_db(
        &syms,
        p.bits_per_sc,
    ))
}

/// Normalize equalized symbols to RMS ≈ 1 and subsample to a bounded point count for plotting.
pub(crate) fn normalize_constellation_for_display(syms: &[Complex32]) -> Vec<(f32, f32)> {
    const MAX_POINTS: usize = 256;
    if syms.is_empty() {
        return Vec::new();
    }
    let rms = (syms.iter().map(|c| c.norm_sqr()).sum::<f32>() / syms.len() as f32)
        .sqrt()
        .max(1e-9);
    let step = (syms.len() / MAX_POINTS).max(1);
    syms.iter()
        .step_by(step)
        .map(|c| (c.re / rms, c.im / rms))
        .collect()
}

fn demodulate_soft_with_params(
    samples: &[f32],
    p: &ScFdmaParams,
) -> Result<SoftDemodOutput, ModemError> {
    let sync = modulate_with_params(&preamble_payload(p), p);
    if samples.len() < sync.len() + SYM_LEN {
        return Err(ModemError::Demodulation("signal too short".into()));
    }

    let Some(offset) = find_sync_offset(samples, &sync) else {
        return Err(ModemError::Demodulation("no SC-FDMA sync detected".into()));
    };
    let payload_start = offset + sync.len();
    if payload_start >= samples.len() {
        return Err(ModemError::Demodulation(
            "SC-FDMA frame truncated after sync".into(),
        ));
    }

    let samples = &samples[payload_start..];
    let n_syms = samples.len() / SYM_LEN;
    if n_syms == 0 {
        return Err(ModemError::Demodulation(
            "SC-FDMA frame truncated after sync".into(),
        ));
    }

    let mut planner = FftPlanner::<f32>::new();
    let idft = planner.plan_fft_inverse(p.n_data);
    let idft_scale = 1.0 / (p.n_data as f32).sqrt();

    let ce = DelayCe::new(p);
    let front = FrameFront::new(samples, p, (!p.localized).then_some(&ce));
    if front.is_empty() {
        return Err(ModemError::Demodulation(
            "SC-FDMA frame truncated after sync".into(),
        ));
    }
    let solver = front.solver(p, &ce);
    let ce_err = ce_error_var(&front, solver.as_ref());

    let points = constellation_points(p.bits_per_sc);
    let mut llrs = Vec::with_capacity(n_syms * p.bits_per_symbol());
    let mut noise_sum = 0.0f32;
    let mut k_db_sum = 0.0f32;
    let mut metric_symbols = 0usize;
    let pilot_scs = pilot_positions(p);
    let mut h_pilots_buf = vec![Complex32::new(0.0, 0.0); pilot_scs.len()];
    let mut ema_h: Option<Vec<Complex32>> = None;

    for freq in &front.spectra {
        let freq = freq.as_slice();
        let raw_h = front.estimate(p, solver.as_ref(), freq);
        let pilot_noise_var = front.noise_var_for(p, solver.as_ref(), freq, &raw_h);
        let h_est = smooth_ce(&mut ema_h, &raw_h);

        // Rician K for SoftFrameMetrics: reuse pre-allocated buffer to avoid per-symbol allocation.
        for (buf, &sc) in h_pilots_buf.iter_mut().zip(pilot_scs.iter()) {
            *buf = freq[sc] / Complex32::new(PILOT_AMPLITUDE, 0.0);
        }
        let k_linear = estimate_rician_k_linear(&h_pilots_buf);
        let k_db = 10.0 * (k_linear + 1e-6).log10();

        let (llr_noise_var, alpha_avg) = mmse_llr_noise_var(p, &h_est, pilot_noise_var, &ce_err);
        let mut equalized = mmse_equalize(p, freq, &h_est, pilot_noise_var);

        idft.process(&mut equalized);
        // Divide by alpha_avg to restore unit-constellation scale after MMSE bias.
        let data_syms: Vec<Complex32> = equalized
            .iter()
            .map(|c| *c * idft_scale / alpha_avg)
            .collect();

        for sym in &data_syms {
            llrs.extend(symbol_llrs(*sym, p.bits_per_sc, llr_noise_var, &points));
        }

        // Decision-residual metric for inverse-noise combining: computed after
        // alpha_avg normalization so symbols are on the unit-constellation scale.
        let decision_noise_var = estimate_decision_noise_var(&data_syms, p.bits_per_sc).max(1e-6);
        noise_sum += decision_noise_var;
        k_db_sum += k_db;
        metric_symbols += 1;
    }

    let Some(payload_len) = decode_len_prefix_llrs(&llrs) else {
        return Err(ModemError::Demodulation(
            "SC-FDMA frame shorter than length prefix".into(),
        ));
    };
    let payload_bits = (payload_len as usize).saturating_mul(8);
    let available_payload_bits = llrs.len().saturating_sub(LEN_PREFIX_BITS);
    let take = if payload_bits == 0 && available_payload_bits > 0 {
        // A noisy length prefix can decode to zero under fading; in that case,
        // return all whole-byte payload bits so downstream soft combining still
        // has useful information.
        available_payload_bits - (available_payload_bits % 8)
    } else {
        payload_bits.min(available_payload_bits)
    };
    Ok(SoftDemodOutput {
        llrs: llrs[LEN_PREFIX_BITS..LEN_PREFIX_BITS + take].to_vec(),
        metrics: SoftFrameMetrics {
            mean_noise_var: noise_sum / metric_symbols.max(1) as f32,
            mean_pilot_noise_var: front.noise_var,
            mean_rician_k_db: k_db_sum / metric_symbols.max(1) as f32,
            symbols_used: metric_symbols,
        },
    })
}

/// Locate the preamble within `samples` via a phase-insensitive matched filter.
///
/// A bare real cross-correlation (`Σ a·b`) is carrier-phase sensitive: over the
/// async-audio loopback the two sound-card clocks impose an arbitrary carrier
/// phase, and a ~90° rotation collapses the real correlation to near zero,
/// landing on a wrong offset.  The shared [`IqMatchedFilter`] correlates
/// against BOTH the preamble and its quadrature (Hilbert) companion and
/// maximises the magnitude, removing that dependence — the per-symbol
/// pilot/MMSE equalizer then handles the residual phase.  The search is
/// bounded to the slice front: the receive engine aligns each window to the
/// detected signal start, so the preamble appears near the front, and an
/// unbounded scan over a multi-second slice is O(N²) (too slow for the
/// real-time loop) and prone to spurious far-field peaks.
///
/// Returns `None` when the best alignment's normalised correlation falls below
/// the detection floor — on a no-signal window the unnormalised argmax is an
/// arbitrary noise offset, and demodulating from it produces garbage bytes
/// (including a random length prefix) at full frame cost.
///
/// The accepted offset is backed off `SYNC_EARLY_BIAS` samples ahead of the correlation peak, so the
/// FFT window never starts late on a multipath channel; see the body.
fn find_sync_offset(samples: &[f32], sync: &[f32]) -> Option<usize> {
    if samples.len() <= sync.len() {
        return None;
    }
    const SEARCH_CAP: usize = 8192;
    // Minimum normalised correlation to accept a sync lock.  Noise scores
    // ≲ 0.1 with a multi-symbol template; a real (even band-limited, faded)
    // preamble correlates well above this.
    const DETECTION_FLOOR_RHO: f32 = 0.15;
    // A window must carry at least this fraction of the mean window energy for its ρ to be trusted;
    // below it, ρ is the ratio of two vanishing quantities. 1 % admits a 20 dB preamble fade.
    const MIN_WINDOW_ENERGY_FRAC: f32 = 0.01;

    // Argmax over the NORMALISED correlation, not the unnormalised score. When the preamble itself
    // lands in a fade, its window energy is low and `search`'s energy-favouring argmax hands the frame
    // to a data-region window that merely shares the pilot comb. Measured under a flat Watterson fade:
    // ρ = 0.994 at the true offset (window energy 19.4) versus ρ = 0.657 at offset +4896 (energy 83.0),
    // which won on energy and left the demodulator with 4 symbols instead of 21.
    let filt = IqMatchedFilter::new(sync.to_vec());
    let result = filt.search_normalized(samples, SEARCH_CAP, MIN_WINDOW_ENERGY_FRAC)?;
    if result.rho < DETECTION_FLOOR_RHO {
        return None;
    }

    // Start EARLY of the correlation peak, never on it.
    //
    // The matched filter takes the argmax, which on a multipath channel sits on whichever ray is
    // instantaneously strongest — the *delayed* one about half the time. A late window start pulls
    // samples of the next symbol into the FFT. The cyclic prefix only protects an **early** start:
    // there the window merely begins inside the symbol's own prefix, a circular shift, i.e. a linear
    // phase ramp across subcarriers that `deramp_timing` removes. `DelayCe`'s basis is two-sided, so
    // its reach is unaffected. The budget is `CP − delay_spread` (32 − ~16 samples).
    //
    // `ofdm::find_first_data_body` solves the same problem by scanning back for the earliest
    // correlation tap above 0.20 × the peak. That does not port here: OFDM brackets its scan with a
    // Schmidl–Cox coarse detection, so the scan window always contains signal, whereas SC-FDMA searches
    // from the front of a slice that may begin with silence — and a *normalised* correlation against a
    // partially-silent window inflates, so the earliest-tap rule latches onto noise. Measured: it broke
    // the noiseless clean channel outright (BER 0.68).
    //
    // Measured, noiseless static two-ray `x[n] + a·x[n−d]`, a = 1.0: SCFDMA52-16QAM BER 1.000 → 0.000
    // at d = 4, QPSK 0.098 → 0.000 at d = 4 and 0.121 → 0.000 at d = 8. Watterson `good_f1` frame
    // success (60 frames, soft FEC) 0.27 → 0.72 on SCFDMA52-16QAM; the AWGN sweep is bit-for-bit
    // unchanged, so this costs nothing on a flat channel.
    //
    // The bias also bounds the *reach*: a stronger delayed ray at delay `d` lands the argmax `d` past
    // the true onset, so the window starts late (→ next-symbol ISI, a hard 0.00 cliff) once `d` exceeds
    // the bias. At bias 8 the wideband SCFDMA52-16/32/64QAM rungs cliffed at d = 10 noiselessly; raising
    // it to 16 pushes the reach to a ±16-sample (2 ms) delay spread — the CCIR-poor HF profile — still an
    // early start well inside the 32-sample CP (a pure circular shift `deramp_timing` removes). The CE
    // basis needs no widening: `deramp_timing` re-centres the impulse response on its power centroid, so
    // the existing ±10-sample `DelayCe` covers the *re-centred* relative spread of a 16-sample two-ray
    // channel. (A wider basis was tried and reverted — it over-fit pilot noise on flat channels and broke
    // the `llr_reliability` calibration gate.)
    const SYNC_EARLY_BIAS: usize = 16;
    Some(result.offset.saturating_sub(SYNC_EARLY_BIAS))
}

// ── Constellation demapping (shared) ──────────────────────────────────────────
// Hard demapper, soft-LLR, constellation points, and decision-noise estimation
// come from `openpulse_dsp::constellation` (imported above).

// ── Bit helpers ───────────────────────────────────────────────────────────────

fn bits_to_bytes(bits: &[bool]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| {
            c.iter()
                .enumerate()
                .fold(0u8, |acc, (i, &b)| acc | ((b as u8) << i))
        })
        .collect()
}

/// GPU-accelerated soft demodulator.  Batches all per-symbol 256-point FFTs in
/// a single GPU dispatch; channel estimation, MMSE equalization, IDFT, and LLR
/// computation remain on CPU.  Returns `None` on GPU error (caller falls back).
#[cfg(feature = "gpu")]
pub fn scfdma_demodulate_soft_gpu(
    samples: &[f32],
    mode: &str,
    ctx: &std::sync::Arc<openpulse_gpu::GpuContext>,
) -> Option<Vec<f32>> {
    let p = params_for_mode(mode)?;

    let sync = modulate_with_params(&preamble_payload(&p), &p);
    if samples.len() < sync.len() + SYM_LEN {
        return None;
    }

    let offset = find_sync_offset(samples, &sync)?;
    let payload_start = offset + sync.len();
    if payload_start >= samples.len() {
        return None;
    }

    let payload_samples = &samples[payload_start..];
    let n_syms = payload_samples.len() / SYM_LEN;
    if n_syms == 0 {
        return None;
    }

    let fft_scale = 1.0 / (FFT_SIZE as f32).sqrt();
    let mut packed: Vec<f32> = Vec::with_capacity(n_syms * FFT_SIZE * 2);
    for sym_idx in 0..n_syms {
        let start = sym_idx * SYM_LEN + CP;
        if start + FFT_SIZE > payload_samples.len() {
            break;
        }
        for &s in &payload_samples[start..start + FFT_SIZE] {
            packed.push(s * fft_scale);
            packed.push(0.0);
        }
    }
    let actual_syms = packed.len() / (FFT_SIZE * 2);
    if actual_syms == 0 {
        return None;
    }

    let gpu_out = openpulse_gpu::gpu_fft256_batch(ctx, &packed, true)?;

    let mut planner = rustfft::FftPlanner::<f32>::new();
    let idft = planner.plan_fft_inverse(p.n_data);
    let idft_scale = 1.0 / (p.n_data as f32).sqrt();

    let ce = DelayCe::new(&p);
    let spectra: Vec<SymbolSpectrum> = (0..actual_syms)
        .map(|sym_idx| {
            let base = sym_idx * FFT_SIZE * 2;
            (0..FFT_SIZE)
                .map(|k| Complex32::new(gpu_out[base + k * 2], gpu_out[base + k * 2 + 1]))
                .collect()
        })
        .collect();
    let front = FrameFront::from_spectra(spectra, &p, (!p.localized).then_some(&ce));
    if front.is_empty() {
        return None;
    }
    let solver = front.solver(&p, &ce);
    let ce_err = ce_error_var(&front, solver.as_ref());

    let points = constellation_points(p.bits_per_sc);
    let mut all_llrs: Vec<f32> = Vec::with_capacity(actual_syms * p.bits_per_symbol());
    let mut ema_h: Option<Vec<Complex32>> = None;

    for freq in &front.spectra {
        let raw_h = front.estimate(&p, solver.as_ref(), freq);
        let pilot_noise_var = front.noise_var_for(&p, solver.as_ref(), freq, &raw_h);
        let h_est = smooth_ce(&mut ema_h, &raw_h);

        let (llr_noise_var, alpha_avg) = mmse_llr_noise_var(&p, &h_est, pilot_noise_var, &ce_err);
        let mut equalized = mmse_equalize(&p, freq, &h_est, pilot_noise_var);

        idft.process(&mut equalized);
        let data_syms: Vec<Complex32> = equalized
            .iter()
            .map(|c| *c * idft_scale / alpha_avg)
            .collect();

        for sym in &data_syms {
            all_llrs.extend(symbol_llrs(*sym, p.bits_per_sc, llr_noise_var, &points));
        }
    }

    // Strip the majority-protected length prefix from the LLR stream,
    // mirroring the CPU path.
    let payload_len = decode_len_prefix_llrs(&all_llrs)? as usize;
    let payload_bits = payload_len.saturating_mul(8);
    let available_payload_bits = all_llrs.len().saturating_sub(LEN_PREFIX_BITS);
    let take = if payload_bits == 0 && available_payload_bits > 0 {
        available_payload_bits - (available_payload_bits % 8)
    } else {
        payload_bits.min(available_payload_bits)
    };
    Some(all_llrs[LEN_PREFIX_BITS..LEN_PREFIX_BITS + take].to_vec())
}

/// GPU-accelerated hard demodulator.  Batches all per-symbol 256-point FFTs
/// into a single GPU dispatch; channel estimation, MMSE equalization, IDFT, and
/// demapping remain on CPU.  Returns `None` on GPU error (caller falls back to CPU).
#[cfg(feature = "gpu")]
pub fn scfdma_demodulate_gpu(
    samples: &[f32],
    mode: &str,
    ctx: &std::sync::Arc<openpulse_gpu::GpuContext>,
) -> Option<Vec<u8>> {
    let p = params_for_mode(mode)?;

    let sync = modulate_with_params(&preamble_payload(&p), &p);
    if samples.len() < sync.len() + SYM_LEN {
        return None;
    }

    let offset = find_sync_offset(samples, &sync)?;
    let payload_start = offset + sync.len();
    if payload_start >= samples.len() {
        return None;
    }

    let payload_samples = &samples[payload_start..];
    let n_syms = payload_samples.len() / SYM_LEN;
    if n_syms == 0 {
        return None;
    }

    // Pack all symbol windows as interleaved (re, 0) complex f32 pairs.
    let mut packed: Vec<f32> = Vec::with_capacity(n_syms * FFT_SIZE * 2);
    for sym_idx in 0..n_syms {
        let start = sym_idx * SYM_LEN + CP;
        if start + FFT_SIZE > payload_samples.len() {
            break;
        }
        let fft_scale = 1.0 / (FFT_SIZE as f32).sqrt();
        for &s in &payload_samples[start..start + FFT_SIZE] {
            packed.push(s * fft_scale);
            packed.push(0.0);
        }
    }
    let actual_syms = packed.len() / (FFT_SIZE * 2);
    if actual_syms == 0 {
        return None;
    }

    let gpu_out = openpulse_gpu::gpu_fft256_batch(ctx, &packed, true)?;

    // Reconstruct Complex32 frequency bins per symbol and run CPU equalization.
    let mut planner = rustfft::FftPlanner::<f32>::new();
    let idft = planner.plan_fft_inverse(p.n_data);
    let idft_scale = 1.0 / (p.n_data as f32).sqrt();

    let ce = DelayCe::new(&p);
    let spectra: Vec<SymbolSpectrum> = (0..actual_syms)
        .map(|sym_idx| {
            let base = sym_idx * FFT_SIZE * 2;
            (0..FFT_SIZE)
                .map(|k| Complex32::new(gpu_out[base + k * 2], gpu_out[base + k * 2 + 1]))
                .collect()
        })
        .collect();
    let front = FrameFront::from_spectra(spectra, &p, (!p.localized).then_some(&ce));
    if front.is_empty() {
        return None;
    }
    let solver = front.solver(&p, &ce);
    let ce_err = ce_error_var(&front, solver.as_ref());

    let mut bits: Vec<bool> = Vec::with_capacity(actual_syms * p.bits_per_symbol());
    let mut ema_h: Option<Vec<Complex32>> = None;

    for freq in &front.spectra {
        let raw_h = front.estimate(&p, solver.as_ref(), freq);
        let noise_var = front.noise_var_for(&p, solver.as_ref(), freq, &raw_h);
        let h_est = smooth_ce(&mut ema_h, &raw_h);
        // Undo the MMSE amplitude bias before demapping, as the CPU hard path does.
        let (_, alpha_avg) = mmse_llr_noise_var(&p, &h_est, noise_var, &ce_err);
        let mut equalized = mmse_equalize(&p, freq, &h_est, noise_var);

        idft.process(&mut equalized);
        let data_syms: Vec<Complex32> = equalized
            .iter()
            .map(|c| c * idft_scale / alpha_avg)
            .collect();

        for sym in &data_syms {
            let b = demap_symbol(*sym, p.bits_per_sc);
            for bit_pos in 0..p.bits_per_sc {
                bits.push((b >> bit_pos) & 1 == 1);
            }
        }
    }

    let raw = bits_to_bytes(&bits);

    // Strip the majority-protected length prefix (mirrors the CPU path).
    let payload_len = decode_len_prefix(&raw)? as usize;
    let available = raw.len() - LEN_PREFIX_BYTES;
    let take = payload_len.min(available);
    Some(raw[LEN_PREFIX_BYTES..LEN_PREFIX_BYTES + take].to_vec())
}

#[cfg(test)]
mod ema_tests {
    use super::*;
    use crate::channel::{pilot_positions, pilot_value, DelayCe};
    use crate::params::{PILOT_AMPLITUDE, SCFDMA52};

    /// #4 justification: on a static channel, EMA-smoothing the per-symbol CE across symbols reduces
    /// CE noise vs the raw per-symbol estimate — the benefit that helps long frames on slow channels.
    #[test]
    fn ema_smoothing_reduces_static_ce_noise() {
        let p = SCFDMA52;
        let pilots = pilot_positions(&p);
        let sigma = 0.2f32;
        let ce = DelayCe::new(&p).solver(
            sigma * sigma,
            PILOT_AMPLITUDE * PILOT_AMPLITUDE + sigma * sigma,
        );
        let mut st = 0x2468u64;
        let mut lcg = || {
            st = st.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((st >> 40) as f32) / ((1u64 << 24) as f32)
        };
        let (mut raw_mse, mut sm_mse, mut n) = (0.0f32, 0.0f32, 0usize);
        for _ in 0..64 {
            let mut ema: Option<Vec<Complex32>> = None;
            for sym in 0..16 {
                let mut freq = vec![Complex32::new(0.0, 0.0); FFT_SIZE];
                for (k, &sc) in pilots.iter().enumerate() {
                    let (u1, u2) = (lcg().max(1e-6), lcg());
                    let m = (-2.0 * (sigma * sigma / 2.0) * u1.ln()).sqrt();
                    let a = std::f32::consts::TAU * u2;
                    freq[sc] = pilot_value(&p, k) + Complex32::new(m * a.cos(), m * a.sin());
                }
                let raw = ce.estimate(&freq);
                let sm = smooth_ce(&mut ema, &raw);
                if sym >= 2 {
                    for sc in p.first_sc..=p.last_sc {
                        raw_mse += (raw[sc - p.first_sc] - Complex32::new(1.0, 0.0)).norm_sqr();
                        sm_mse += (sm[sc - p.first_sc] - Complex32::new(1.0, 0.0)).norm_sqr();
                    }
                    n += p.total_sc();
                }
            }
        }
        let raw_db = 10.0 * (raw_mse / n as f32).log10();
        let sm_db = 10.0 * (sm_mse / n as f32).log10();
        println!(
            "static CE-MSE: raw={raw_db:.2}dB  ema={sm_db:.2}dB  gain={:.2}dB",
            raw_db - sm_db
        );
        assert!(
            sm_db < raw_db - 1.0,
            "EMA CE-MSE {sm_db:.2}dB should beat raw {raw_db:.2}dB by >1 dB"
        );
    }
}
