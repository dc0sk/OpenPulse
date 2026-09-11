//! Shared, carrier-phase-insensitive frame acquisition primitives.
//!
//! A bare real cross-correlation (`Σ a·b`) against a known passband template is
//! carrier-phase sensitive: an arbitrary carrier phase (async sound-card clocks,
//! multi-second capture latency) rotates the received waveform, and at ~90° the
//! real correlation collapses to near zero so the search locks to a wrong
//! offset.  This failure was independently found and fixed in the QPSK, OFDM
//! (#385), and SCFDMA (#386) plugins.  This module is the single shared
//! implementation of the fixed pattern: correlate against BOTH the template and
//! its quadrature (Hilbert) companion and use the I/Q magnitude, which is
//! invariant to the carrier phase.

use num_complex::Complex32;
use rustfft::FftPlanner;

/// Quadrature (90°-shifted) companion of a real signal via the FFT Hilbert
/// transform: the imaginary part of the analytic signal.
pub fn quadrature(x: &[f32]) -> Vec<f32> {
    let n = x.len();
    if n == 0 {
        return vec![];
    }
    let mut planner = FftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(n);
    let inv = planner.plan_fft_inverse(n);
    let mut buf: Vec<Complex32> = x.iter().map(|&v| Complex32::new(v, 0.0)).collect();
    fwd.process(&mut buf);
    let half = n / 2;
    for (k, c) in buf.iter_mut().enumerate() {
        if k == 0 || (n.is_multiple_of(2) && k == half) {
            // DC and Nyquist unchanged.
        } else if k < half {
            *c *= 2.0; // positive frequencies doubled
        } else {
            *c = Complex32::new(0.0, 0.0); // negative frequencies zeroed
        }
    }
    inv.process(&mut buf);
    let scale = 1.0 / n as f32;
    buf.iter().map(|c| c.im * scale).collect()
}

/// Result of an [`IqMatchedFilter`] search.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IqSearchResult {
    /// Best-scoring sample offset into the searched slice.
    pub offset: usize,
    /// Raw (unnormalised) squared correlation magnitude at `offset`.
    pub score: f32,
    /// Normalised correlation magnitude ρ ∈ [0, 1] at `offset`:
    /// `|corr| / sqrt(window_energy × template_energy)`.  Use this for
    /// presence detection — on noise it stays well below typical lock values.
    pub rho: f32,
}

/// Carrier-phase-insensitive matched filter for a known real passband template.
pub struct IqMatchedFilter {
    template: Vec<f32>,
    template_q: Vec<f32>,
    t_energy: f32,
}

impl IqMatchedFilter {
    /// Build the filter; precomputes the Hilbert quadrature companion.
    pub fn new(template: Vec<f32>) -> Self {
        let template_q = quadrature(&template);
        let t_energy = template.iter().map(|&x| x * x).sum();
        Self {
            template,
            template_q,
            t_energy,
        }
    }

    /// Template length in samples.
    pub fn len(&self) -> usize {
        self.template.len()
    }

    /// Returns `true` if the template is empty.
    pub fn is_empty(&self) -> bool {
        self.template.is_empty()
    }

    /// Correlation magnitude² and window energy at one offset.
    fn score_at(&self, samples: &[f32], offset: usize) -> (f32, f32) {
        let win = &samples[offset..offset + self.template.len()];
        let mut dot_i = 0.0f32;
        let mut dot_q = 0.0f32;
        let mut energy = 0.0f32;
        for (m, &s) in win.iter().enumerate() {
            dot_i += s * self.template[m];
            dot_q += s * self.template_q[m];
            energy += s * s;
        }
        (dot_i * dot_i + dot_q * dot_q, energy)
    }

    /// Search offsets `0..=bound` (clamped to the available samples) for the
    /// maximum unnormalised correlation magnitude.
    ///
    /// The argmax uses the *unnormalised* score: like the original `Σ a·b` it
    /// favours high-correlation *and* high-energy alignment, so a deep-fade
    /// low-energy window cannot win.  The returned [`IqSearchResult::rho`] is
    /// the normalised magnitude at the winning offset, suitable for a
    /// detection threshold.  Returns `None` when the slice is shorter than the
    /// template.
    pub fn search(&self, samples: &[f32], bound: usize) -> Option<IqSearchResult> {
        if samples.len() < self.template.len() || self.template.is_empty() {
            return None;
        }
        let max_offset = (samples.len() - self.template.len()).min(bound);

        let mut best_offset = 0usize;
        let mut best_score = f32::NEG_INFINITY;
        for offset in 0..=max_offset {
            let (score, _) = self.score_at(samples, offset);
            if score > best_score {
                best_score = score;
                best_offset = offset;
            }
        }

        let (score, energy) = self.score_at(samples, best_offset);
        let denom = (energy * self.t_energy).sqrt() + 1e-12;
        Some(IqSearchResult {
            offset: best_offset,
            score,
            rho: score.sqrt() / denom,
        })
    }

    /// Search offsets `0..=bound` for the maximum **normalised** correlation ρ, ignoring windows
    /// whose energy is below `min_energy_frac` of the mean window energy over the search range.
    ///
    /// Prefer this to [`search`](Self::search) whenever the signal may fade *during the preamble*.
    /// `search`'s unnormalised argmax deliberately favours high-energy alignment, on the reasoning
    /// that "a deep-fade low-energy window cannot win" — but when the preamble itself is the faded
    /// part, that is exactly backwards. Measured on SC-FDMA under a flat Watterson fade: at the true
    /// offset ρ = 0.994 with window energy 19.4, while a data-region window 4896 samples later scored
    /// higher on energy alone with ρ = 0.657, and the demodulator locked onto it. ρ is amplitude
    /// invariant, so it is unmoved by the fade.
    ///
    /// The energy floor is what keeps ρ meaningful: on a near-silent window both numerator and
    /// denominator vanish and ρ is numerical noise.
    ///
    /// Returns `None` when the slice is shorter than the template or no window clears the floor.
    pub fn search_normalized(
        &self,
        samples: &[f32],
        bound: usize,
        min_energy_frac: f32,
    ) -> Option<IqSearchResult> {
        if samples.len() < self.template.len() || self.template.is_empty() {
            return None;
        }
        let max_offset = (samples.len() - self.template.len()).min(bound);

        let mut scored: Vec<(f32, f32)> = Vec::with_capacity(max_offset + 1);
        let mut energy_sum = 0.0f64;
        for offset in 0..=max_offset {
            let (score, energy) = self.score_at(samples, offset);
            energy_sum += energy as f64;
            scored.push((score, energy));
        }
        let mean_energy = (energy_sum / (max_offset + 1) as f64) as f32;
        let floor = mean_energy * min_energy_frac;

        let mut best_offset = None;
        let mut best_rho = f32::NEG_INFINITY;
        for (offset, &(score, energy)) in scored.iter().enumerate() {
            if energy < floor {
                continue;
            }
            let rho = score.sqrt() / ((energy * self.t_energy).sqrt() + 1e-12);
            if rho > best_rho {
                best_rho = rho;
                best_offset = Some(offset);
            }
        }
        let offset = best_offset?;
        Some(IqSearchResult {
            offset,
            score: scored[offset].0,
            rho: best_rho,
        })
    }

    /// Peak normalised correlation ρ over timing offsets `0..=bound` **and** a residual-frequency
    /// grid, returning the best `(result, frequency)`.
    ///
    /// **Why the frequency dimension is not optional.** A matched filter integrates coherently, so
    /// a carrier offset `Δf` rotates the window against the template and ρ collapses roughly as
    /// `|sinc(Δf·T)|` over the template span `T`. Measured on a 1024-sample (128 ms) BPSK250
    /// preamble: ρ = 1.000 at 0 Hz, 0.332 at 20 Hz, 0.016 at 400 Hz — while the same acquisition
    /// chain is required to pull in offsets to ±600 Hz (`tests/carrier_offset_acquisition.rs`).
    /// A bare [`search_normalized`](Self::search_normalized) used as a presence test would
    /// therefore reject real frames a few Hz off-frequency, and shortening the template to widen
    /// the tolerance destroys the discrimination it exists for (at 256 samples the recorded idle
    /// floor itself reaches ρ = 0.377).
    ///
    /// Each grid point rotates the *analytic template* rather than mixing the window, so no
    /// per-hypothesis Hilbert transform is needed: `t' = t·e^{j2πfm/fs}` is exact for the analytic
    /// pair this filter already holds. `t_energy` is recomputed per rotation because `Σ Re(t')²`
    /// is not rotation-invariant.
    ///
    /// Note the cost of the extra dimension in false alarms: taking a maximum over more hypotheses
    /// raises the noise floor of ρ itself. Measured over recorded idle audio, worst-case noise ρ
    /// rises from 0.157 (single frequency) to 0.233 (±160 Hz at 4 Hz). Size the grid from the
    /// offsets that must be acquired, not from what is cheap.
    pub fn search_normalized_over_frequency(
        &self,
        samples: &[f32],
        bound: usize,
        min_energy_frac: f32,
        sample_rate: f32,
        freqs: &[f32],
    ) -> Option<(IqSearchResult, f32)> {
        if samples.len() < self.template.len() || self.template.is_empty() || sample_rate <= 0.0 {
            return None;
        }
        let mut best: Option<(IqSearchResult, f32)> = None;
        let mut ti = vec![0.0f32; self.template.len()];
        let mut tq = vec![0.0f32; self.template.len()];
        for &f in freqs {
            let w = 2.0 * std::f32::consts::PI * f / sample_rate;
            let mut t_energy = 0.0f32;
            for m in 0..self.template.len() {
                let (s, c) = (w * m as f32).sin_cos();
                ti[m] = self.template[m] * c - self.template_q[m] * s;
                tq[m] = self.template[m] * s + self.template_q[m] * c;
                t_energy += ti[m] * ti[m];
            }
            if let Some(r) = Self::search_with(samples, &ti, &tq, t_energy, bound, min_energy_frac)
            {
                if best.as_ref().is_none_or(|(b, _)| r.rho > b.rho) {
                    best = Some((r, f));
                }
            }
        }
        best
    }

    /// [`search_normalized`](Self::search_normalized) against an explicit template pair, so a
    /// frequency-rotated copy can reuse it.
    fn search_with(
        samples: &[f32],
        ti: &[f32],
        tq: &[f32],
        t_energy: f32,
        bound: usize,
        min_energy_frac: f32,
    ) -> Option<IqSearchResult> {
        let tlen = ti.len();
        let max_offset = (samples.len() - tlen).min(bound);
        let mut scored: Vec<(f32, f32)> = Vec::with_capacity(max_offset + 1);
        let mut energy_sum = 0.0f64;
        for offset in 0..=max_offset {
            let win = &samples[offset..offset + tlen];
            let mut dot_i = 0.0f32;
            let mut dot_q = 0.0f32;
            let mut energy = 0.0f32;
            for (m, &s) in win.iter().enumerate() {
                dot_i += s * ti[m];
                dot_q += s * tq[m];
                energy += s * s;
            }
            energy_sum += energy as f64;
            scored.push((dot_i * dot_i + dot_q * dot_q, energy));
        }
        let mean_energy = (energy_sum / (max_offset + 1) as f64) as f32;
        let floor = mean_energy * min_energy_frac;

        let mut best_offset = None;
        let mut best_rho = f32::NEG_INFINITY;
        for (offset, &(score, energy)) in scored.iter().enumerate() {
            if energy < floor {
                continue;
            }
            let rho = score.sqrt() / ((energy * t_energy).sqrt() + 1e-12);
            if rho > best_rho {
                best_rho = rho;
                best_offset = Some(offset);
            }
        }
        let offset = best_offset?;
        Some(IqSearchResult {
            offset,
            score: scored[offset].0,
            rho: best_rho,
        })
    }

    /// Normalised correlation ρ for every offset in `lo..=hi` (clamped).
    ///
    /// Used by multipath-aware acquisition (e.g. OFDM leading-path selection)
    /// that needs the full correlation profile rather than just the argmax.
    pub fn rho_profile(&self, samples: &[f32], lo: usize, hi: usize) -> Vec<f32> {
        if samples.len() < self.template.len() {
            return vec![];
        }
        let max_offset = samples.len() - self.template.len();
        let hi = hi.min(max_offset);
        if lo > hi {
            return vec![];
        }
        (lo..=hi)
            .map(|d| {
                let (score, energy) = self.score_at(samples, d);
                score.sqrt() / ((energy * self.t_energy).sqrt() + 1e-12)
            })
            .collect()
    }
}

/// Squared magnitude of the complex preamble correlation `|Σ r·conj(e)|²`.
///
/// The carrier-phase-invariant symbol-domain timing metric: at the correct
/// timing the magnitude is maximal for any carrier phase, where the bare real
/// part collapses at 90°/270°.  `received` and `expected` are (I, Q) pairs;
/// correlation runs over `min(len)` symbols.
pub fn preamble_corr_sq(received: &[(f32, f32)], expected: &[(f32, f32)]) -> f32 {
    let (re_sum, im_sum) = received
        .iter()
        .zip(expected.iter())
        .fold((0.0f32, 0.0f32), |(re, im), (&(ri, rq), &(ei, eq))| {
            (re + ri * ei + rq * eq, im + rq * ei - ri * eq)
        });
    re_sum * re_sum + im_sum * im_sum
}

/// M-th power carrier frequency offset estimator for M-PSK symbol streams.
///
/// Raising each symbol to the M-th power removes M-ary PSK modulation, leaving
/// a phasor rotating at `M·2π·Δf/baud` per symbol; the mean phase of
/// consecutive products gives `Δf`.  **Range: ±baud/(2·M).**
///
/// Only valid for (near-)constant-modulus constellations; QAM data symbols
/// add heavy self-noise — use a data-aided preamble estimator instead.
pub fn estimate_cfo_mth_power(i_syms: &[f32], q_syms: &[f32], baud_rate: f32, m: u32) -> f32 {
    if i_syms.len() < 2 || m == 0 {
        return 0.0;
    }

    let mut re_m = Vec::with_capacity(i_syms.len());
    let mut im_m = Vec::with_capacity(i_syms.len());
    for (&i, &q) in i_syms.iter().zip(q_syms.iter()) {
        let mut re = i;
        let mut im = q;
        for _ in 1..m {
            let next_re = re * i - im * q;
            let next_im = re * q + im * i;
            re = next_re;
            im = next_im;
        }
        re_m.push(re);
        im_m.push(im);
    }

    let mut re_sum = 0.0f32;
    let mut im_sum = 0.0f32;
    for k in 1..re_m.len() {
        re_sum += re_m[k] * re_m[k - 1] + im_m[k] * im_m[k - 1];
        im_sum += im_m[k] * re_m[k - 1] - re_m[k] * im_m[k - 1];
    }

    im_sum.atan2(re_sum) * baud_rate / (2.0 * std::f32::consts::PI * m as f32)
}

/// Data-aided carrier frequency offset estimator against a known preamble.
///
/// Removes the known preamble modulation by `y[k] = z[k]·conj(p[k])`, then
/// estimates the per-symbol rotation from consecutive products.
/// **Range: ±baud/2** — much wider than blind M-th-power estimation, with no
/// constellation-dependent self-noise.  Returns `None` for < 2 usable symbols.
pub fn estimate_cfo_data_aided(
    i_syms: &[f32],
    q_syms: &[f32],
    preamble: &[(f32, f32)],
    baud_rate: f32,
) -> Option<f32> {
    let n = i_syms.len().min(q_syms.len()).min(preamble.len());
    if n < 2 {
        return None;
    }

    let mut y_re = Vec::with_capacity(n);
    let mut y_im = Vec::with_capacity(n);
    for k in 0..n {
        let (pr, pi) = preamble[k];
        y_re.push(i_syms[k] * pr + q_syms[k] * pi);
        y_im.push(q_syms[k] * pr - i_syms[k] * pi);
    }

    let mut re_sum = 0.0f32;
    let mut im_sum = 0.0f32;
    for k in 1..n {
        re_sum += y_re[k] * y_re[k - 1] + y_im[k] * y_im[k - 1];
        im_sum += y_im[k] * y_re[k - 1] - y_re[k] * y_im[k - 1];
    }

    Some(im_sum.atan2(re_sum) * baud_rate / (2.0 * std::f32::consts::PI))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    /// `ddc_mix` must be executable in the dev profile — it was not, for its whole life.
    ///
    /// The guard read `n - ntap + 1` while the loop starts at `n == ntap - 1`, so the first
    /// iteration underflowed. Release builds wrap and land on the correct value, so every number
    /// ever measured through the DDC was right; the dev profile panics, and **no test in the
    /// workspace built this type** — the `ddc_correlation_equivalence` harness reimplements the
    /// mixer locally (carrying the same expression) and only ever ran `#[ignore]`d. Three layers,
    /// each of which alone would have hidden it.
    ///
    /// Asserts more than "does not panic": the kept-sample set is what the expression decides, so
    /// the first output sample (the one the panic sat on) must be present, and the count must be
    /// the full `(len - ntap + 1)` positions thinned by `decim`.
    #[test]
    fn ddc_mix_keeps_the_first_sample_and_every_decim_th_one() {
        let taps = lowpass_taps(1_000.0, 8_000.0, 129);
        let ntap = taps.len();
        // A ramp, so a dropped or shifted output is visible in the value and not only in the count.
        let x: Vec<f32> = (0..1_024).map(|i| i as f32).collect();

        for decim in [1usize, 2, 3, 8] {
            let out = ddc_mix(&x, 1_500.0, 8_000.0, decim, &taps);
            let positions = x.len() - ntap + 1;
            assert_eq!(
                out.len(),
                positions.div_ceil(decim),
                "decim {decim}: expected every {decim}th of {positions} filter positions"
            );
            assert!(
                out[0].0.is_finite() && out[0].1.is_finite(),
                "decim {decim}: the first output sample is where the underflow sat"
            );
        }
    }

    fn chirp_template(len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let t = i as f32 / len as f32;
                (2.0 * PI * (5.0 + 20.0 * t) * t * 8.0).sin()
            })
            .collect()
    }

    #[test]
    fn quadrature_shifts_sine_to_negative_cosine_phase() {
        let n = 256;
        let x: Vec<f32> = (0..n)
            .map(|i| (2.0 * PI * 8.0 * i as f32 / n as f32).sin())
            .collect();
        let q = quadrature(&x);
        // Hilbert of sin is -cos.
        for (i, &v) in q.iter().enumerate().skip(16).take(n - 32) {
            let expect = -(2.0 * PI * 8.0 * i as f32 / n as f32).cos();
            assert!((v - expect).abs() < 0.05, "idx {i}: {v} vs {expect}");
        }
    }

    #[test]
    fn matched_filter_finds_offset_for_any_carrier_phase() {
        let template = chirp_template(512);
        let filt = IqMatchedFilter::new(template.clone());
        let template_q = quadrature(&template);
        let true_offset = 137usize;

        for phase_deg in [0.0f32, 45.0, 90.0, 135.0, 180.0, 270.0] {
            let (s, c) = (phase_deg.to_radians()).sin_cos();
            // Rotate the analytic signal: cosφ·x + sinφ·x_q ≈ phase-shifted x.
            let rotated: Vec<f32> = template
                .iter()
                .zip(template_q.iter())
                .map(|(&i, &q)| c * i + s * q)
                .collect();
            let mut samples = vec![0.0f32; true_offset];
            samples.extend_from_slice(&rotated);
            samples.extend(vec![0.0f32; 256]);

            let r = filt.search(&samples, 8192).expect("search");
            assert_eq!(
                r.offset, true_offset,
                "phase {phase_deg}°: offset {} ≠ {true_offset}",
                r.offset
            );
            assert!(r.rho > 0.9, "phase {phase_deg}°: rho {} too low", r.rho);
        }
    }

    #[test]
    fn matched_filter_rho_low_on_noise() {
        let template = chirp_template(512);
        let filt = IqMatchedFilter::new(template);
        // Deterministic pseudo-noise.
        let mut state = 0x12345678u32;
        let noise: Vec<f32> = (0..4096)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state >> 16) as f32 / 32768.0 - 1.0
            })
            .collect();
        let r = filt.search(&noise, 8192).expect("search");
        assert!(r.rho < 0.5, "noise rho {} should be well below lock", r.rho);
    }

    /// Mix a real passband signal up by `hz` via its analytic companion.
    fn mix(x: &[f32], hz: f32, fs: f32) -> Vec<f32> {
        let q = quadrature(x);
        x.iter()
            .zip(q.iter())
            .enumerate()
            .map(|(k, (&i, &qq))| {
                let (s, c) = (2.0 * PI * hz * k as f32 / fs).sin_cos();
                i * c - qq * s
            })
            .collect()
    }

    /// A narrowband BPSK-like preamble: 32 symbols of alternating phase at 250 baud on a 1500 Hz
    /// carrier, 8 kHz — the geometry the engine's frame detection actually uses.
    ///
    /// Deliberately NOT [`chirp_template`]: a chirp has a large time-bandwidth product and is
    /// famously delay-Doppler tolerant, so it survives a carrier offset that destroys a narrowband
    /// correlation. Using one here would have made the test below pass while demonstrating nothing.
    fn narrowband_preamble(fs: f32) -> Vec<f32> {
        let sps = 32usize; // 8000 / 250
        (0..32 * sps)
            .map(|k| {
                let sign = if (k / sps).is_multiple_of(2) {
                    1.0
                } else {
                    -1.0
                };
                sign * (2.0 * PI * 1_500.0 * k as f32 / fs).cos()
            })
            .collect()
    }

    /// The frequency grid recovers what a fixed-frequency correlation loses to a carrier offset —
    /// and the fixed one must actually lose it, or this test proves nothing.
    #[test]
    fn frequency_grid_recovers_rho_a_fixed_correlation_loses_to_an_offset() {
        const FS: f32 = 8_000.0;
        let template = narrowband_preamble(FS);
        let filt = IqMatchedFilter::new(template.clone());
        let mut signal = vec![0.0f32; 300];
        signal.extend_from_slice(&template);
        signal.extend(std::iter::repeat_n(0.0, 300));

        // Control: no offset, both find it.
        let flat = filt.search_normalized(&signal, 2_000, 0.05).expect("clean");
        assert!(flat.rho > 0.9, "clean rho {} should lock", flat.rho);

        let offset = mix(&signal, 25.0, FS);
        // The falsifier: a fixed-frequency correlation must be BROKEN by the offset, otherwise the
        // grid below would be recovering nothing and the whole frequency dimension is unjustified.
        let fixed = filt
            .search_normalized(&offset, 2_000, 0.05)
            .expect("search");
        assert!(
            fixed.rho < 0.6,
            "a 25 Hz offset left the fixed correlation at rho {} — this test cannot show the grid \
             recovering anything",
            fixed.rho
        );

        let grid: Vec<f32> = (-20..=20).map(|k| k as f32 * 4.0).collect();
        let (r, f) = filt
            .search_normalized_over_frequency(&offset, 2_000, 0.05, FS, &grid)
            .expect("grid search");
        assert!(
            r.rho > 0.9,
            "grid search rho {} should recover the offset frame",
            r.rho
        );
        assert!(
            (f - 25.0).abs() <= 4.0,
            "grid picked {f} Hz for a 25 Hz offset — it is not estimating the frequency"
        );
    }

    /// A one-point grid at 0 Hz must reproduce `search_normalized` exactly: the added dimension is
    /// a generalisation, not a different measurement.
    #[test]
    fn a_single_zero_frequency_grid_point_matches_the_plain_search() {
        let template = chirp_template(256);
        let filt = IqMatchedFilter::new(template.clone());
        let mut signal = vec![0.0f32; 100];
        signal.extend_from_slice(&template);
        let plain = filt.search_normalized(&signal, 500, 0.05).expect("plain");
        let (grid, f) = filt
            .search_normalized_over_frequency(&signal, 500, 0.05, 8_000.0, &[0.0])
            .expect("grid");
        assert_eq!(f, 0.0);
        assert_eq!(plain.offset, grid.offset);
        assert!((plain.rho - grid.rho).abs() < 1e-4, "{plain:?} vs {grid:?}");
    }

    #[test]
    fn preamble_corr_sq_invariant_to_carrier_phase() {
        let expected: Vec<(f32, f32)> = (0..16)
            .map(|k| {
                let a = k as f32 * 2.4;
                (a.cos(), a.sin())
            })
            .collect();
        let base = preamble_corr_sq(&expected, &expected);
        for phase in [0.5f32, PI / 2.0, PI, 4.0] {
            let (s, c) = phase.sin_cos();
            let rotated: Vec<(f32, f32)> = expected
                .iter()
                .map(|&(i, q)| (i * c - q * s, i * s + q * c))
                .collect();
            let m = preamble_corr_sq(&rotated, &expected);
            assert!(
                (m - base).abs() / base < 1e-4,
                "phase {phase}: {m} vs {base}"
            );
        }
    }

    #[test]
    fn mth_power_estimates_qpsk_cfo() {
        let baud = 250.0f32;
        let cfo = 5.0f32;
        let n = 200;
        // Random-ish QPSK data with a CFO rotation.
        let mut i_syms = Vec::with_capacity(n);
        let mut q_syms = Vec::with_capacity(n);
        for k in 0..n {
            let data_phase = PI / 4.0 + (k % 4) as f32 * PI / 2.0;
            let total = data_phase + 2.0 * PI * cfo * k as f32 / baud;
            i_syms.push(total.cos());
            q_syms.push(total.sin());
        }
        let est = estimate_cfo_mth_power(&i_syms, &q_syms, baud, 4);
        assert!((est - cfo).abs() < 0.5, "estimated {est}, expected {cfo}");
    }

    #[test]
    fn data_aided_estimates_cfo_beyond_mth_power_range() {
        let baud = 500.0f32;
        let cfo = 100.0f32; // beyond ±baud/8 = 62.5 Hz 4th-power range
        let n = 16;
        let preamble: Vec<(f32, f32)> = (0..n)
            .map(|k| {
                let a = (k as f32 * 1.9).sin() * PI;
                (a.cos(), a.sin())
            })
            .collect();
        let mut i_syms = Vec::with_capacity(n);
        let mut q_syms = Vec::with_capacity(n);
        for (k, &(pi_, pq)) in preamble.iter().enumerate() {
            let rot = 2.0 * PI * cfo * k as f32 / baud;
            let (s, c) = rot.sin_cos();
            i_syms.push(pi_ * c - pq * s);
            q_syms.push(pi_ * s + pq * c);
        }
        let est = estimate_cfo_data_aided(&i_syms, &q_syms, &preamble, baud).expect("estimate");
        assert!((est - cfo).abs() < 2.0, "estimated {est}, expected {cfo}");
    }
}

/// Coarse wide-range carrier-offset scan via Goertzel on the M-th-power signal.
///
/// Raising the real passband signal to the M-th power strips M-ary PSK
/// modulation (M=2 for BPSK, 4 for QPSK and square QAM), leaving a stable
/// spectral line at `M·fc`.  A Goertzel probe sweeps `M·(fc + δf)` over
/// `δf ∈ ±range_hz` in `step_hz` increments and returns the best `δf`.
/// Probe frequencies above Nyquist are folded (real-signal spectrum is
/// symmetric), so `M·fc` may exceed `fs/2` — e.g. 4 × 1500 Hz at fs = 8 kHz
/// probes the 2 kHz alias.
///
/// Returns `None` for an empty buffer, `m == 0`, or a degenerate grid.
pub fn goertzel_carrier_scan(
    samples: &[f32],
    fs: f32,
    fc: f32,
    m: u32,
    range_hz: f32,
    step_hz: f32,
) -> Option<f32> {
    if samples.is_empty() || m == 0 || step_hz <= 0.0 || range_hz < 0.0 {
        return None;
    }

    let powered: Vec<f32> = samples
        .iter()
        .map(|&s| {
            let mut acc = s;
            for _ in 1..m {
                acc *= s;
            }
            acc
        })
        .collect();

    let fold = |f: f32| -> f32 {
        // Alias of a real tone: reflect into [0, fs/2].
        let f = f.rem_euclid(fs);
        if f > fs / 2.0 {
            fs - f
        } else {
            f
        }
    };

    // The powered signal also contains strong self-mixing lines at k·fc for
    // k < m (x⁴ has a large DC term and a 2·fc term, x² has DC).  A probe
    // whose alias lands on one of those fixed lines reports a huge spurious
    // power and captures the scan, so such probes are skipped — the wanted
    // m·fc line moves with δf while the interferers do not, leaving only
    // small holes in the scan grid.
    const INTERFERER_GUARD_HZ: f32 = 150.0;
    let mut interferers: Vec<f32> = vec![0.0]; // DC
    for k in 1..m {
        interferers.push(fold(k as f32 * fc));
    }

    let mut best_power = f32::NEG_INFINITY;
    let mut best_df = 0.0f32;
    let mut df = -range_hz;
    while df <= range_hz + step_hz / 2.0 {
        let probe = fold(m as f32 * (fc + df));
        if interferers
            .iter()
            .any(|&intf| (probe - intf).abs() < INTERFERER_GUARD_HZ)
            || probe > fs / 2.0 - INTERFERER_GUARD_HZ
        {
            df += step_hz;
            continue;
        }
        let omega = 2.0 * std::f32::consts::PI * probe / fs;
        let coeff = 2.0 * omega.cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &x in &powered {
            let s0 = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        if power > best_power {
            best_power = power;
            best_df = df;
        }
        df += step_hz;
    }

    Some(best_df)
}

#[cfg(test)]
mod goertzel_tests {
    use super::*;
    use std::f32::consts::PI;

    fn psk_signal(fs: f32, fc: f32, baud: f32, order: u32, n_syms: usize) -> Vec<f32> {
        let sps = (fs / baud) as usize;
        let mut out = Vec::with_capacity(n_syms * sps);
        let mut state = 7u32;
        for _ in 0..n_syms {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let phase = 2.0 * PI * ((state >> 8) % order) as f32 / order as f32;
            for k in 0..sps {
                let t = out.len() as f32 / fs;
                out.push((2.0 * PI * fc * t + phase).cos());
                let _ = k;
            }
        }
        out
    }

    #[test]
    fn scan_finds_bpsk_offset_m2() {
        let sig = psk_signal(8000.0, 1650.0, 250.0, 2, 64);
        let df = goertzel_carrier_scan(&sig, 8000.0, 1500.0, 2, 400.0, 25.0).expect("scan");
        assert!((df - 150.0).abs() <= 25.0, "df = {df}");
    }

    #[test]
    fn scan_finds_qpsk_offset_m4_with_alias_fold() {
        // 4·fc = 6 kHz > Nyquist at fs = 8 kHz: exercises the alias fold.
        let sig = psk_signal(8000.0, 1350.0, 250.0, 4, 64);
        let df = goertzel_carrier_scan(&sig, 8000.0, 1500.0, 4, 400.0, 25.0).expect("scan");
        assert!((df + 150.0).abs() <= 25.0, "df = {df}");
    }
}

// ── Decimating (DDC) matched filter ───────────────────────────────────────────

/// Matched filter that correlates at complex baseband after decimation.
///
/// [`IqMatchedFilter`] correlates in the passband, so its cost scales with the raw template length.
/// That is what puts the slow rungs over the engine's correlation budget: BPSK31's 32-symbol
/// preamble is 16 128 samples, so those modes are exempt from the #1049 veto entirely — not because
/// correlation cannot help them, but because it was too expensive. They are the modes that listen
/// longest and are most exposed to a noise settle.
///
/// This mixes to complex baseband, lowpasses, and decimates by `decim` before correlating. The
/// search then costs roughly `1/decim²` (fewer offsets over shorter vectors), while the grid count
/// is unchanged because the grid step scales with duration and duration is unchanged.
///
/// Equivalence on signals that are genuinely in-band is exact to four decimals at `decim` up to 32
/// (clean frame delta 0.0000, in-band noise 0.0016–0.0039).
///
/// **This is a cost trade, and it is paid for in detection margin.** An earlier version of this doc
/// claimed the anti-alias lowpass "rejects interference before the correlator sees it", citing an
/// out-of-band tone that costs the passband correlator ρ 1.000 → 0.945 while this path stays at
/// 1.000. That measurement is correct and the conclusion drawn from it was wrong: ρ is a *ratio*,
/// the lowpass removes out-of-band energy from its **denominator**, and so it raises ρ for signal
/// and noise alike — the noise by more. Measured in one environment (SSB-band noise), separation
/// `ρ_signal − ρ_noise_ceiling`:
///
/// | mode | passband | this filter |
/// |---|---|---|
/// | BPSK31 | 0.997 − 0.066 = **0.930** | 1.000 − 0.326 = **0.674** |
/// | BPSK250 | 0.997 − 0.176 = **0.820** | 0.999 − 0.351 = **0.648** |
///
/// The gap narrows sharply in the environment that actually sets a threshold — under a 200 Hz
/// receive filter the two are within 9 % (0.544 vs 0.591), because the passband correlator's low
/// wide-band ceiling is borrowed from out-of-band noise padding the denominator, which such a
/// filter removes. So the margin cost is real but smaller at the decision point than the table
/// above suggests.
///
/// **What it does buy is affordability, and that is hardware-dependent.** On a Pi 5 (the reference
/// class) this path is 2.8× cheaper than passband for BPSK31 — 15.2 ms against 42.7 ms per
/// invocation — where on a fast x86 dev host it was only 1.2×, because the wider SIMD and cache
/// there favour the long linear scan. Measuring cost on a dev host alone understates the benefit on
/// the machine that ships.
///
/// Bench: `openpulse-modem/tests/ddc_correlation_equivalence.rs` and
/// `tests/bpsk31_constant_derivation.rs` (R5–R9).
pub struct DdcMatchedFilter {
    template: Vec<(f32, f32)>,
    t_energy: f32,
    taps: Vec<f32>,
    decim: usize,
    center_hz: f32,
    sample_rate: f32,
}

impl DdcMatchedFilter {
    /// Build from a real passband template. `cutoff_hz` must pass the signal plus the residual
    /// grid; `decim` must keep the complex rate above twice that.
    pub fn new(
        template: &[f32],
        center_hz: f32,
        sample_rate: f32,
        cutoff_hz: f32,
        decim: usize,
    ) -> Self {
        let decim = decim.max(1);
        let taps = lowpass_taps(cutoff_hz, sample_rate, 129);
        let t = ddc_mix(template, center_hz, sample_rate, decim, &taps);
        let t_energy = t.iter().map(|c| c.0 * c.0 + c.1 * c.1).sum();
        Self {
            template: t,
            t_energy,
            taps,
            decim,
            center_hz,
            sample_rate,
        }
    }

    /// Decimated template length, i.e. the cost the correlation budget should be measured against.
    pub fn len(&self) -> usize {
        self.template.len()
    }

    /// Input samples the template SPANS, which is what its duration — and so its coherent
    /// bandwidth — is set by. That is the anti-alias FIR's valid region times `decim`: a little
    /// shorter than the raw template (7808 of 7936 samples for BPSK31's), because `ddc_mix` keeps
    /// only the positions where the filter has full support.
    ///
    /// Distinct from [`Self::len`] on purpose. Decimation changes the sample *count* and not the
    /// time *span*, so the two answer different questions and coincide only at `decim == 1`. A
    /// caller deriving a frequency resolution wants this one: `1 / (span / sample_rate)`. Using
    /// `len()` there understates the duration by `decim` and so overstates the resolution by the
    /// same factor — a residual-frequency grid `decim`x too coarse, whose worst-case coherent loss
    /// is `|sinc(decim / 8)|`: harmless at 1, and an exact null at 8.
    pub fn input_span(&self) -> usize {
        self.template.len() * self.decim
    }

    /// Returns `true` if the decimated template is empty.
    pub fn is_empty(&self) -> bool {
        self.template.is_empty()
    }

    /// Best normalised correlation over onsets and a residual-frequency grid.
    ///
    /// `freqs` are absolute residual offsets in Hz, as for
    /// [`IqMatchedFilter::search_normalized_over_frequency`] — each is folded into the mix, which is
    /// where a baseband correlator applies it most cheaply.
    pub fn search_normalized_over_frequency(
        &self,
        samples: &[f32],
        min_energy_frac: f32,
        freqs: &[f32],
    ) -> Option<(IqSearchResult, f32)> {
        if self.template.is_empty() {
            return None;
        }
        let mut best: Option<(IqSearchResult, f32)> = None;
        for &f in freqs {
            let w = ddc_mix(
                samples,
                self.center_hz + f,
                self.sample_rate,
                self.decim,
                &self.taps,
            );
            if w.len() <= self.template.len() {
                continue;
            }
            let span = w.len() - self.template.len();
            let mut energies = Vec::with_capacity(span + 1);
            for off in 0..=span {
                energies.push(
                    w[off..off + self.template.len()]
                        .iter()
                        .map(|c| c.0 * c.0 + c.1 * c.1)
                        .sum::<f32>(),
                );
            }
            let mean: f32 = energies.iter().sum::<f32>() / energies.len() as f32;
            let floor = mean * min_energy_frac;
            for (off, &energy) in energies.iter().enumerate() {
                if energy < floor {
                    continue;
                }
                let (mut ri, mut rq) = (0.0f32, 0.0f32);
                for (m, t) in self.template.iter().enumerate() {
                    let s = w[off + m];
                    ri += s.0 * t.0 + s.1 * t.1;
                    rq += s.1 * t.0 - s.0 * t.1;
                }
                let score = ri * ri + rq * rq;
                let rho = score.sqrt() / ((energy * self.t_energy).sqrt() + 1e-12);
                if best.as_ref().is_none_or(|(b, _)| rho > b.rho) {
                    best = Some((
                        IqSearchResult {
                            offset: off * self.decim,
                            score,
                            rho,
                        },
                        f,
                    ));
                }
            }
        }
        best
    }
}

/// Hamming-windowed sinc lowpass.
fn lowpass_taps(cutoff_hz: f32, sample_rate: f32, n: usize) -> Vec<f32> {
    let fc = cutoff_hz / sample_rate;
    let m = n as f32 - 1.0;
    (0..n)
        .map(|i| {
            let x = i as f32 - m / 2.0;
            let sinc = if x.abs() < 1e-6 {
                2.0 * fc
            } else {
                (2.0 * core::f32::consts::PI * fc * x).sin() / (core::f32::consts::PI * x)
            };
            let w = 0.54 - 0.46 * (2.0 * core::f32::consts::PI * i as f32 / m).cos();
            sinc * w
        })
        .collect()
}

/// Mix to complex baseband at `f_hz`, lowpass with `taps`, decimate by `decim`.
fn ddc_mix(x: &[f32], f_hz: f32, sample_rate: f32, decim: usize, taps: &[f32]) -> Vec<(f32, f32)> {
    let two_pi = 2.0 * core::f32::consts::PI;
    let mixed: Vec<(f32, f32)> = x
        .iter()
        .enumerate()
        .map(|(n, &s)| {
            let ph = -two_pi * f_hz * n as f32 / sample_rate;
            (s * ph.cos(), s * ph.sin())
        })
        .collect();
    let ntap = taps.len();
    let mut out = Vec::with_capacity(mixed.len() / decim + 1);
    for n in (ntap - 1)..mixed.len() {
        // `n + 1 - ntap`, never `n - ntap + 1`: the first iteration has `n == ntap - 1`, so the
        // latter underflows before the `+ 1` is applied. Under wrapping arithmetic it lands on the
        // right answer anyway, which is why release builds were correct and only debug panicked —
        // and why nothing noticed, since no gate ever built this type in the dev profile.
        if !(n + 1 - ntap).is_multiple_of(decim) {
            continue;
        }
        let (mut i, mut q) = (0.0f32, 0.0f32);
        for (k, &t) in taps.iter().enumerate() {
            let s = mixed[n - k];
            i += t * s.0;
            q += t * s.1;
        }
        out.push((i, q));
    }
    out
}
