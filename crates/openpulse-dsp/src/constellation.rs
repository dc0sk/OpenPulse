//! Gray-coded constellation mapping, hard slicing, and max-log-MAP soft LLRs.
//!
//! Shared by the OFDM and SC-FDMA plugins: both place one constellation symbol
//! per data subcarrier, differing only in whether the symbols are DFT-precoded
//! (SC-FDMA) or not (OFDM).  All constellations are Gray coded and normalised to
//! unit average power.
//!
//! **LLR sign convention**: positive = bit more likely 0, matching every other
//! plugin and codec in this codebase.

use num_complex::Complex32;

const INV_SQRT2: f32 = std::f32::consts::FRAC_1_SQRT_2;
const QAM16_SCALE: f32 = 0.316_227_77; // 1/sqrt(10)
const QAM64_SCALE: f32 = 0.154_303_35; // 1/sqrt(42)
/// 1/√20 — normalisation scale for cross-32QAM (unit mean power).
pub const QAM32_SCALE: f32 = 0.223_606_8;

/// Cross-32QAM constellation as a **direct label→point table** (index = 5-bit label). Optimised for
/// 2D-Gray: the labels minimise the total Hamming distance between Euclidean-adjacent points
/// (avg **1.36** bits/nearest-neighbour vs 2.04 for the old 1D-Gray-over-2D-raster mapping), which is
/// what the soft demod's LLRs and the bit-error rate depend on. Derived by simulated annealing in
/// `tests/qam32_gray_optimizer.rs` — re-run that to regenerate. The four corners (|I|=|Q|=5) are
/// absent (36 − 4 = 32). Bit 4 (MSB) cleanly separates the I<0 / I>0 half-planes.
pub const QAM32_BY_LABEL: [(i8, i8); 32] = [
    (-1, 3),  // 00000
    (-1, 5),  // 00001
    (-1, 1),  // 00010
    (-1, -1), // 00011
    (-3, 3),  // 00100
    (-3, 5),  // 00101
    (-3, 1),  // 00110
    (-3, -1), // 00111
    (-3, -5), // 01000
    (-3, -3), // 01001
    (-1, -5), // 01010
    (-1, -3), // 01011
    (-5, 3),  // 01100
    (-5, -3), // 01101
    (-5, 1),  // 01110
    (-5, -1), // 01111
    (1, 3),   // 10000
    (1, 5),   // 10001
    (1, 1),   // 10010
    (1, -1),  // 10011
    (3, 3),   // 10100
    (3, 5),   // 10101
    (3, 1),   // 10110
    (3, -1),  // 10111
    (3, -5),  // 11000
    (3, -3),  // 11001
    (1, -5),  // 11010
    (1, -3),  // 11011
    (5, 3),   // 11100
    (5, -3),  // 11101
    (5, 1),   // 11110
    (5, -1),  // 11111
];

// ── Gray-code helpers ──────────────────────────────────────────────────────────

/// Convert a 3-bit Gray code to a natural (binary) index.
pub fn gray3_to_natural(g: u8) -> u8 {
    let g = g & 0x7;
    let b2 = (g >> 2) & 1;
    let b1 = ((g >> 1) ^ b2) & 1;
    let b0 = (g ^ b1) & 1;
    (b2 << 2) | (b1 << 1) | b0
}

/// Convert a natural (binary) 3-bit index to its Gray code.
pub fn natural3_to_gray(n: u8) -> u8 {
    (n ^ (n >> 1)) & 0x7
}

/// Convert a 5-bit Gray code to a natural (binary) index.
pub fn gray5_to_natural(g: u8) -> u8 {
    let g = g & 0x1f;
    let b4 = (g >> 4) & 1;
    let b3 = ((g >> 3) ^ b4) & 1;
    let b2 = ((g >> 2) ^ b3) & 1;
    let b1 = ((g >> 1) ^ b2) & 1;
    let b0 = (g ^ b1) & 1;
    (b4 << 4) | (b3 << 3) | (b2 << 2) | (b1 << 1) | b0
}

/// Convert a natural (binary) 5-bit index to its Gray code.
pub fn natural5_to_gray(n: u8) -> u8 {
    (n ^ (n >> 1)) & 0x1f
}

// ── Modulation (Gray label → point) ─────────────────────────────────────────────

/// Map a Gray-coded `bits_per_sc`-bit label to its constellation point.
///
/// `bits_per_sc`: 2=QPSK, 3=8PSK, 4=16QAM, 5=cross-32QAM, 6=64QAM. Other values
/// fall back to QPSK.
pub fn map_symbol(bits: u8, bits_per_sc: usize) -> Complex32 {
    match bits_per_sc {
        3 => psk8(bits),
        4 => qam16(bits),
        5 => qam32(bits),
        6 => qam64(bits),
        _ => qpsk(bits),
    }
}

fn qpsk(bits: u8) -> Complex32 {
    match bits & 0x3 {
        0 => Complex32::new(INV_SQRT2, INV_SQRT2),
        1 => Complex32::new(-INV_SQRT2, INV_SQRT2),
        2 => Complex32::new(INV_SQRT2, -INV_SQRT2),
        _ => Complex32::new(-INV_SQRT2, -INV_SQRT2),
    }
}

fn psk8(bits: u8) -> Complex32 {
    let k = gray3_to_natural(bits);
    let angle = k as f32 * std::f32::consts::FRAC_PI_4;
    Complex32::new(angle.cos(), angle.sin())
}

fn pam4(g: u8) -> f32 {
    match g & 0x3 {
        0b00 => -3.0,
        0b01 => -1.0,
        0b11 => 1.0,
        _ => 3.0, // 0b10
    }
}

fn qam16(bits: u8) -> Complex32 {
    Complex32::new(
        pam4((bits >> 2) & 0x3) * QAM16_SCALE,
        pam4(bits & 0x3) * QAM16_SCALE,
    )
}

fn qam32(bits: u8) -> Complex32 {
    let (i, q) = QAM32_BY_LABEL[(bits & 0x1f) as usize];
    Complex32::new(i as f32 * QAM32_SCALE, q as f32 * QAM32_SCALE)
}

fn pam8(g: u8) -> f32 {
    let raw: i8 = match g & 0x7 {
        0b000 => -7,
        0b001 => -5,
        0b011 => -3,
        0b010 => -1,
        0b110 => 1,
        0b111 => 3,
        0b101 => 5,
        _ => 7, // 0b100
    };
    raw as f32 * QAM64_SCALE
}

fn qam64(bits: u8) -> Complex32 {
    Complex32::new(pam8((bits >> 3) & 0x7), pam8(bits & 0x7))
}

// ── Hard-decision demapping ──────────────────────────────────────────────────────

/// Hard-decision demap: the Gray label of the nearest constellation point.
pub fn demap_symbol(c: Complex32, bits_per_sc: usize) -> u8 {
    match bits_per_sc {
        3 => psk8_demod(c),
        4 => qam16_demod(c),
        5 => qam32_demod(c),
        6 => qam64_demod(c),
        _ => qpsk_demod(c),
    }
}

fn qpsk_demod(c: Complex32) -> u8 {
    let i_bit = if c.re >= 0.0 { 0u8 } else { 1u8 };
    let q_bit = if c.im >= 0.0 { 0u8 } else { 1u8 };
    i_bit | (q_bit << 1)
}

fn psk8_demod(c: Complex32) -> u8 {
    use std::f32::consts::{FRAC_PI_4, TAU};
    let angle = c.im.atan2(c.re).rem_euclid(TAU);
    let k = ((angle / FRAC_PI_4) + 0.5).floor() as u8 % 8;
    natural3_to_gray(k)
}

fn qam16_demod(c: Complex32) -> u8 {
    pam4_slice(c.re) << 2 | pam4_slice(c.im)
}

fn qam64_demod(c: Complex32) -> u8 {
    pam8_slice(c.re) << 3 | pam8_slice(c.im)
}

fn qam32_demod(c: Complex32) -> u8 {
    // The table is label-indexed, so the nearest point's index IS its label.
    let mut best_label = 0u8;
    let mut best_d = f32::INFINITY;
    for (label, &(i, q)) in QAM32_BY_LABEL.iter().enumerate() {
        let d = (c.re - i as f32 * QAM32_SCALE).powi(2) + (c.im - q as f32 * QAM32_SCALE).powi(2);
        if d < best_d {
            best_d = d;
            best_label = label as u8;
        }
    }
    best_label
}

/// Nearest PAM-4 Gray code for a real amplitude (thresholds at 0 and ±2×scale).
fn pam4_slice(x: f32) -> u8 {
    const T1: f32 = 2.0 * QAM16_SCALE;
    if x < -T1 {
        0b00
    } else if x < 0.0 {
        0b01
    } else if x < T1 {
        0b11
    } else {
        0b10
    }
}

/// Nearest PAM-8 Gray code for a real amplitude (thresholds at even multiples of scale).
fn pam8_slice(x: f32) -> u8 {
    const T1: f32 = 2.0 * QAM64_SCALE;
    const T2: f32 = 4.0 * QAM64_SCALE;
    const T3: f32 = 6.0 * QAM64_SCALE;
    if x < -T3 {
        0b000
    } else if x < -T2 {
        0b001
    } else if x < -T1 {
        0b011
    } else if x < 0.0 {
        0b010
    } else if x < T1 {
        0b110
    } else if x < T2 {
        0b111
    } else if x < T3 {
        0b101
    } else {
        0b100
    }
}

// ── Soft demapping (max-log-MAP) ─────────────────────────────────────────────────

/// All `(gray_label, point)` pairs for the constellation.
pub fn constellation_points(bits_per_sc: usize) -> Vec<(u8, Complex32)> {
    let order = match bits_per_sc {
        3 => 8u16,
        4 => 16,
        5 => 32,
        6 => 64,
        _ => 4,
    };
    (0..order)
        .map(|b| (b as u8, map_symbol(b as u8, bits_per_sc)))
        .collect()
}

/// Per-bit max-log-MAP LLRs for one received `symbol`.
///
/// `points` must be `constellation_points(bits_per_sc)`.  Returns `bits_per_sc`
/// LLRs (positive = bit more likely 0).
pub fn symbol_llrs(
    symbol: Complex32,
    bits_per_sc: usize,
    noise_var: f32,
    points: &[(u8, Complex32)],
) -> Vec<f32> {
    let inv_noise = 1.0 / noise_var.max(1e-6);
    let mut out = Vec::with_capacity(bits_per_sc);
    for bit in 0..bits_per_sc {
        let mut min0 = f32::INFINITY;
        let mut min1 = f32::INFINITY;
        for (label, pt) in points {
            let d = (symbol - *pt).norm_sqr() * inv_noise;
            if (label >> bit) & 1 == 0 {
                if d < min0 {
                    min0 = d;
                }
            } else if d < min1 {
                min1 = d;
            }
        }
        out.push(min1 - min0);
    }
    out
}

/// Effective symbol amplitude and per-real-dimension noise variance of a **constant-modulus** PSK
/// block (`bits_per_sc` 2 or 3), measured from the component of each symbol *orthogonal* to its hard
/// decision.
///
/// `e = z · conj(ŝ)` with `|ŝ| = 1`: `Re(e)` carries the amplitude, `Im(e)` carries only noise.
/// Splitting them this way matters because a demodulator's residual is not all thermal noise —
/// pulse-shaping ISI and equalizer misadjustment vary the symbol *amplitude* with no dependence on
/// SNR. A moment estimator (M2/M4) or a distance-to-nearest-point estimator folds that in and its
/// output stops tracking SNR; the orthogonal component does not.
///
/// Returns `(amplitude, noise_var_per_dimension)`. The 2-D noise variance is `2 ×` the second value.
///
/// Decision-directed, so it saturates at the residual-EVM floor once the channel is clean: LLRs come
/// out UNDER-confident, which is safe for same-mode summing. **The saturation is not safe in both
/// directions, and this said it was until 2026-09-13.** Once symbol errors are common the residual is
/// measured against the WRONG decision, which folds error energy out of the estimate, under-reads σ²
/// and makes the LLRs OVER-confident — the dangerous direction, because `combine_llrs_map` treats
/// magnitudes as probabilities. Measured on 8PSK500: calibrated to within 1.25× at 6–8 dB,
/// over-confident below its carrier-tracking cliff at 4 dB. No σ² estimator can represent a lost
/// lock, which is a separate failure from a mis-estimated variance.
pub fn psk_symbol_noise_var(symbols: &[Complex32], bits_per_sc: usize) -> (f32, f32) {
    if symbols.is_empty() {
        return (1.0, 1e-6);
    }
    let n = symbols.len() as f32;
    let (mut re_sum, mut im2_sum) = (0.0f32, 0.0f32);
    for &z in symbols {
        let s = map_symbol(demap_symbol(z, bits_per_sc), bits_per_sc);
        let e = z * s.conj();
        re_sum += e.re;
        im2_sum += e.im * e.im;
    }
    (re_sum / n, (im2_sum / n).max(1e-12))
}

/// Convert a constant-modulus PSK block's `(amplitude, noise_var_per_dimension)` — as returned by
/// [`psk_symbol_noise_var`] — into a symbol SNR in dB: `10·log10(A² / 2σ²)`.
///
/// `2σ²` is the total two-dimensional noise power (the second return value is per real dimension).
/// Because the underlying estimator is decision-directed it saturates at the block's residual-EVM
/// floor, so this reads a large-but-bounded dB when the noise is negligible. Returns a floored value
/// (never `-inf`/`NaN`) for a degenerate all-zero block.
pub fn snr_db_from_amp_noise(amp: f32, noise_var_per_dim: f32) -> f32 {
    let signal = amp * amp;
    let noise = (2.0 * noise_var_per_dim).max(1e-12);
    10.0 * (signal / noise).max(1e-12).log10()
}

/// Scale that turns a differential correlation `dot_k = Re(z_k · conj(z_{k−1}))` into a
/// log-likelihood ratio, given the quadrature companion `cross_k = Im(z_k · conj(z_{k−1}))`.
///
/// `dot` is antipodal with mean `±A²`; `cross` has mean 0 and variance `≈ 2A²σ²`. So
/// `2·mean|dot| / var(cross) → 1/σ²` and the signal amplitude cancels — which is what makes this
/// immune to the symbol-amplitude variation that defeats a variance-of-`|dot|` estimate.
///
/// Multiply the `dot` values by the result. Returns 0 for an empty input.
pub fn differential_llr_scale(dots: &[f32], crosses: &[f32]) -> f32 {
    if dots.is_empty() || crosses.is_empty() {
        return 0.0;
    }
    let mu = dots.iter().map(|v| v.abs()).sum::<f32>() / dots.len() as f32;
    let var_cross = crosses.iter().map(|v| v * v).sum::<f32>() / crosses.len() as f32;
    2.0 * mu / var_cross.max(1e-12)
}

/// Additive SNR (dB) of a symbol block, with the *multiplicative* channel removed first.
///
/// This is the symbol-domain twin of `openpulse_channel::estimate_additive_snr_db`, and it exists for
/// the same reason: on a fading channel `z[k] ≈ h[k]·s[k] + n[k]`, and any estimator that measures the
/// raw residual `z − s` (or its orthogonal component) folds the *multiplicative* `h` into the
/// "noise". The result stops tracking SNR entirely — measured on Watterson `moderate_f1`, the
/// M2M4 fallback reads a **flat ≈ −6.6 dB from 15 dB of true SNR upward**, which then drives the rate
/// controller to the bottom rung on frames that decoded perfectly well (issue #934).
///
/// Removing a per-window least-squares complex gain `g_w = ⟨z, ŝ*⟩ / ⟨ŝ, ŝ*⟩` takes out both the fade
/// amplitude and its phase rotation, leaving the additive residual. Unlike the raw-audio estimator,
/// no time alignment is needed: `decisions[k]` corresponds to `rx[k]` by construction.
///
/// `window` symbols per gain estimate — small enough that `h` is ~constant across it (a 1 Hz Doppler
/// fade has a coherence time of hundreds of symbols at 250 baud), large enough that the LS gain does
/// not absorb the noise it is meant to measure (the gain soaks up ≈ 1/window of it, so ≥ 8).
///
/// Decision-directed, so it saturates once symbol errors are common — the safe direction for a rate
/// decision. Callers supply their own decisions, so this works for any constellation, including ones
/// [`map_symbol`] does not model (BPSK) and differentially-encoded streams (the per-window gain
/// absorbs the arbitrary global phase/sign).
pub fn additive_snr_db_windowed(rx: &[Complex32], decisions: &[Complex32], window: usize) -> f32 {
    let n = rx.len().min(decisions.len());
    let w = window.max(8);
    if n == 0 {
        return 0.0;
    }
    let (mut sig, mut noise) = (0.0f64, 0.0f64);
    let mut start = 0usize;
    while start < n {
        let end = (start + w).min(n);
        let mut num = Complex32::new(0.0, 0.0);
        let mut den = 0.0f32;
        for i in start..end {
            num += rx[i] * decisions[i].conj();
            den += decisions[i].norm_sqr();
        }
        // A window with no signal contributes nothing rather than a spurious gain.
        if den > 1e-9 {
            let g = num / den;
            for i in start..end {
                let s = g * decisions[i];
                sig += s.norm_sqr() as f64;
                noise += (rx[i] - s).norm_sqr() as f64;
            }
        }
        start = end;
    }
    if sig <= 0.0 {
        return 0.0;
    }
    10.0 * (sig / noise.max(1e-12)).max(1e-12).log10() as f32
}

/// Estimate decision-directed noise variance from a block of equalised symbols
/// (mean squared distance to the nearest constellation point).
///
/// Symbols must already be on the [`constellation_points`] scale. Divide max-log-MAP distance
/// differences by this to get true LLRs — see [`symbol_llrs`]'s `noise_var`.
pub fn estimate_decision_noise_var(symbols: &[Complex32], bits_per_sc: usize) -> f32 {
    if symbols.is_empty() {
        return 1e-6;
    }
    let points = constellation_points(bits_per_sc);
    let sum_min_dist: f32 = symbols
        .iter()
        .map(|s| {
            points
                .iter()
                .map(|(_, pt)| (*s - *pt).norm_sqr())
                .fold(f32::INFINITY, f32::min)
        })
        .sum();
    (sum_min_dist / symbols.len() as f32).max(1e-6)
}

/// Symbol SNR in dB of a block of equalised QAM/PSK symbols already on the [`constellation_points`]
/// scale: `10·log10(P_const / σ²)`, where `P_const` is the constellation's average symbol power and
/// `σ²` is the decision-directed noise power from [`estimate_decision_noise_var`].
///
/// Unlike the constant-modulus [`psk_symbol_noise_var`], this works for non-constant-modulus
/// constellations (16/32/64QAM), which is what a multicarrier plugin (OFDM) needs. It is
/// decision-directed, so it saturates once symbol errors are common — the safe direction for a rate
/// decision — and, being a ratio of powers on the same scale, it is invariant to a uniform gain
/// (e.g. a ZF-equalizer scale). On a frequency-selective channel the ZF noise-enhancement on faded
/// subcarriers inflates `σ²`, so it *under*-reads SNR there — conservative, which is again safe.
pub fn qam_symbol_snr_db(symbols: &[Complex32], bits_per_sc: usize) -> f32 {
    if symbols.is_empty() {
        return 0.0;
    }
    let points = constellation_points(bits_per_sc);
    let sig_power =
        points.iter().map(|(_, p)| p.norm_sqr()).sum::<f32>() / points.len().max(1) as f32;
    let noise = estimate_decision_noise_var(symbols, bits_per_sc);
    10.0 * (sig_power / noise).max(1e-12).log10()
}

// ── 32APSK (DVB-S2 4+12+16 rings) ─────────────────────────────────────────────
//
// A 5-bit amplitude/phase constellation: inner 4PSK + mid 12PSK + outer 16PSK at
// the DVB-S2 radius ratios (γ1=2.53, γ2=4.3) with the validated DVB-S2 bit
// labeling (from daniestevez/qo100-modem). Lower envelope variance than
// cross-32QAM — better on nonlinear PAs and fading — for the same 5 bits/symbol.
// Distinct from `bits_per_sc = 5` (cross-32QAM); select it via these functions.

const APSK32_GAMMA1: f32 = 2.53;
const APSK32_GAMMA2: f32 = 4.3;
/// DVB-S2 bit labels indexed by geometric order (outer 0..15, mid 0..11,
/// inner 0..3); the value is the 5-bit label carried by that point.
const APSK32_LABELS: [u8; 32] = [
    24, 8, 25, 9, 13, 29, 12, 28, 30, 14, 31, 15, 11, 27, 10, 26, 16, 0, 1, 5, 4, 20, 22, 6, 7, 3,
    2, 18, 17, 21, 23, 19,
];

/// The 32 `(label, point)` pairs of the DVB-S2 32APSK constellation (unit average power).
pub fn apsk32_points() -> Vec<(u8, Complex32)> {
    use std::f32::consts::PI;
    let (g1, g2) = (APSK32_GAMMA1, APSK32_GAMMA2);
    let power = (1.0 / (g2 * g2) + 3.0 * g1 * g1 / (g2 * g2) + 4.0) / 8.0;
    let scale = 1.0 / power.sqrt();
    let mut pts = Vec::with_capacity(32);
    let mut idx = 0usize;
    for k in 0..16 {
        // Outer 16PSK at radius 1.
        let a = PI / 8.0 * k as f32;
        pts.push((APSK32_LABELS[idx], Complex32::from_polar(scale, a)));
        idx += 1;
    }
    for k in 0..12 {
        // Mid 12PSK at radius γ1/γ2.
        let a = PI / 6.0 * k as f32 + PI / 12.0;
        pts.push((
            APSK32_LABELS[idx],
            Complex32::from_polar(scale * g1 / g2, a),
        ));
        idx += 1;
    }
    for k in 0..4 {
        // Inner 4PSK at radius 1/γ2.
        let a = PI / 2.0 * k as f32 + PI / 4.0;
        pts.push((APSK32_LABELS[idx], Complex32::from_polar(scale / g2, a)));
        idx += 1;
    }
    pts
}

/// Map a 5-bit label to its 32APSK constellation point.
pub fn map_apsk32(bits: u8) -> Complex32 {
    let b = bits & 0x1f;
    apsk32_points()
        .into_iter()
        .find(|(label, _)| *label == b)
        .map(|(_, pt)| pt)
        .unwrap_or_else(|| Complex32::new(0.0, 0.0))
}

/// Hard-decision demap: the 5-bit label of the nearest 32APSK point.
pub fn demap_apsk32(c: Complex32) -> u8 {
    apsk32_points()
        .into_iter()
        .min_by(|(_, a), (_, b)| (c - *a).norm_sqr().total_cmp(&(c - *b).norm_sqr()))
        .map(|(label, _)| label)
        .unwrap_or(0)
}

/// Per-bit max-log-MAP LLRs for a received 32APSK symbol (positive = bit 0).
pub fn soft_apsk32(symbol: Complex32, noise_var: f32) -> Vec<f32> {
    symbol_llrs(symbol, 5, noise_var, &apsk32_points())
}

/// Scale a symbol stream to unit RMS magnitude so a decision-directed carrier loop has a level-invariant
/// loop gain.
///
/// The Costas/DD phase-error magnitude scales with the symbol amplitude (`q·sgn(i)`, `Im(r·conj(d))`, …),
/// so a quiet station's small symbols give the loop a proportionally weaker effective bandwidth and it
/// cannot acquire even a ~1 Hz residual over a short frame (the no-AGC failure). This restores the loop
/// gain the loop was tuned for. It is a **no-op at nominal amplitude** — unit-energy PSK constellations
/// already sit at RMS ≈ 1 — and a single uniform scale, so it changes neither phase nor the calibrated
/// soft-LLR scale (∝ amp/σ², itself invariant to a common scale).
pub fn normalize_stream_rms(syms: &mut [(f32, f32)]) {
    if syms.is_empty() {
        return;
    }
    let ms = syms.iter().map(|&(i, q)| i * i + q * q).sum::<f32>() / syms.len() as f32;
    let rms = ms.sqrt();
    if rms > 1e-9 {
        let k = 1.0 / rms;
        for s in syms.iter_mut() {
            s.0 *= k;
            s.1 *= k;
        }
    }
}

#[cfg(test)]
mod tests {
    /// `differential_llr_scale` must recover 1/σ² independently of the signal amplitude — that
    /// cancellation is the whole point (a `var(|dot|)` estimate is defeated by amplitude variation).
    ///
    /// Only above the ~3 dB where `mean|dot| ≈ A²` holds; below it the estimator is decision-directed
    /// and saturates, which is the safe direction (under-confident LLRs).
    #[test]
    fn differential_llr_scale_recovers_inverse_noise_var_at_any_amplitude() {
        for amp in [0.2f32, 1.0, 5.0] {
            for snr_db in [10.0f32, 20.0] {
                let sigma2 = amp * amp / 10f32.powf(snr_db / 10.0);
                // dot ~ ±A² with var 2A²σ²; cross ~ 0 with the same variance.
                let n = 20_000;
                let mut state = 0x1234u64;
                let mut g = || {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let u1 = (((state >> 11) as f64) / ((1u64 << 53) as f64)).clamp(1e-12, 1.0);
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let u2 = ((state >> 11) as f64) / ((1u64 << 53) as f64);
                    ((-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()) as f32
                };
                let sd = (2.0 * amp * amp * sigma2).sqrt();
                let dots: Vec<f32> = (0..n)
                    .map(|i| {
                        let b = if i % 3 == 0 { -1.0 } else { 1.0 };
                        b * amp * amp + sd * g()
                    })
                    .collect();
                let crosses: Vec<f32> = (0..n).map(|_| sd * g()).collect();
                let k = differential_llr_scale(&dots, &crosses);
                let expected = 1.0 / sigma2;
                assert!(
                    (k / expected - 1.0).abs() < 0.15,
                    "amp={amp} snr={snr_db} dB (σ²={sigma2:.5}): scale {k:.2} vs expected {expected:.2}"
                );
            }
        }
    }

    /// The orthogonal residual must track σ² even when the symbol *amplitude* wanders — the failure
    /// mode that makes a moment or distance-to-nearest-point estimator stop responding to SNR.
    #[test]
    fn psk_symbol_noise_var_is_immune_to_amplitude_jitter() {
        let mut state = 0xBEEFu64;
        let mut g = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let u1 = (((state >> 11) as f64) / ((1u64 << 53) as f64)).clamp(1e-12, 1.0);
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let u2 = ((state >> 11) as f64) / ((1u64 << 53) as f64);
            ((-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()) as f32
        };
        for sigma2_per_dim in [1e-3f32, 1e-2] {
            let sd = sigma2_per_dim.sqrt();
            let syms: Vec<Complex32> = (0..20_000)
                .map(|i| {
                    let s = map_symbol((i % 4) as u8, 2);
                    // ±30 % deterministic amplitude jitter, plus AWGN.
                    let a = 1.0 + 0.3 * ((i as f32) * 0.7).sin();
                    s * a + Complex32::new(sd * g(), sd * g())
                })
                .collect();
            let (_, nv) = psk_symbol_noise_var(&syms, 2);
            assert!(
                (nv / sigma2_per_dim - 1.0).abs() < 0.2,
                "σ²/dim={sigma2_per_dim}: estimated {nv:.5}"
            );
        }
    }

    use super::*;

    fn order(bits_per_sc: usize) -> u8 {
        constellation_points(bits_per_sc).len() as u8
    }

    #[test]
    fn average_power_is_unit_all_constellations() {
        for bps in [2usize, 3, 4, 5, 6] {
            let n = order(bps);
            let total: f32 = (0..n).map(|b| map_symbol(b, bps).norm_sqr()).sum::<f32>() / n as f32;
            assert!(
                (total - 1.0).abs() < 0.01,
                "bits_per_sc={bps} avg power={total:.4}"
            );
        }
    }

    #[test]
    fn hard_demap_round_trips_all_constellations() {
        for bps in [2usize, 3, 4, 5, 6] {
            let n = order(bps);
            for b in 0..n {
                let recovered = demap_symbol(map_symbol(b, bps), bps);
                assert_eq!(recovered, b, "bits_per_sc={bps} label {b} round-trip");
            }
        }
    }

    #[test]
    fn apsk32_round_trips_all_labels() {
        for b in 0..32u8 {
            assert_eq!(
                demap_apsk32(map_apsk32(b)),
                b,
                "32APSK label {b} round-trip"
            );
        }
    }

    #[test]
    fn apsk32_average_power_is_unit() {
        let pts = apsk32_points();
        assert_eq!(pts.len(), 32);
        let avg = pts.iter().map(|(_, p)| p.norm_sqr()).sum::<f32>() / 32.0;
        assert!((avg - 1.0).abs() < 0.01, "32APSK avg power {avg:.4}");
    }

    #[test]
    fn apsk32_has_three_distinct_rings() {
        let pts = apsk32_points();
        let mut radii: Vec<f32> = pts.iter().map(|(_, p)| p.norm()).collect();
        radii.sort_by(f32::total_cmp);
        // 4 inner + 12 mid + 16 outer; the inner ring sits well inside the outer.
        assert!(
            radii[0] < radii[31] * 0.5,
            "inner radius {} should be << outer {}",
            radii[0],
            radii[31]
        );
    }

    #[test]
    fn apsk32_soft_llrs_hard_slice_to_label() {
        // Clean symbol: hard-slicing the soft LLRs (positive = bit 0) reproduces
        // the label — pins map/soft consistency and the cross-plugin LLR sign.
        for b in 0..32u8 {
            let llrs = soft_apsk32(map_apsk32(b), 0.01);
            assert_eq!(llrs.len(), 5);
            let mut decoded = 0u8;
            for (bit, &llr) in llrs.iter().enumerate() {
                if llr <= 0.0 {
                    decoded |= 1 << bit;
                }
            }
            assert_eq!(decoded, b, "32APSK soft hard-slice label {b}");
        }
    }

    #[test]
    fn soft_llrs_agree_with_hard_demap_clean() {
        // On a noiseless symbol, the sign of each soft LLR must select the same
        // bit as the hard demapper.
        for bps in [2usize, 3, 4, 5, 6] {
            let pts = constellation_points(bps);
            let n = order(bps);
            for b in 0..n {
                let sym = map_symbol(b, bps);
                let llrs = symbol_llrs(sym, bps, 0.01, &pts);
                for (bit, l) in llrs.iter().enumerate() {
                    let hard = (b >> bit) & 1;
                    let soft = if *l >= 0.0 { 0 } else { 1 };
                    assert_eq!(soft, hard, "bits_per_sc={bps} label {b} bit {bit}");
                }
            }
        }
    }

    #[test]
    fn gray_round_trips() {
        for n in 0u8..8 {
            assert_eq!(gray3_to_natural(natural3_to_gray(n)), n);
        }
        for n in 0u8..32 {
            assert_eq!(gray5_to_natural(natural5_to_gray(n)), n);
        }
    }

    #[test]
    fn all_points_distinct() {
        for bps in [2usize, 3, 4, 5, 6] {
            let pts = constellation_points(bps);
            for i in 0..pts.len() {
                for j in (i + 1)..pts.len() {
                    let a = pts[i].1;
                    let b = pts[j].1;
                    assert!(
                        (a - b).norm() > 1e-4,
                        "bits_per_sc={bps}: points {i},{j} collide"
                    );
                }
            }
        }
    }

    /// The property `additive_snr_db_windowed` exists for: it must read the *additive* SNR even when
    /// a multiplicative channel is rotating and fading the symbols. Fed a synthetic `h[k]·s[k] + n[k]`
    /// with a known noise power, it must recover the true SNR — and it must NOT be fooled by `h`.
    #[test]
    fn additive_snr_ignores_a_rotating_fading_channel() {
        let mut seed = 12345u64;
        let mut rng = move || {
            // xorshift → uniform in [-1, 1); good enough for a noise power test.
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed as i32 as f32) / (i32::MAX as f32)
        };
        for true_snr_db in [5.0f32, 10.0, 20.0] {
            // Noise std per dimension for a unit-power symbol: SNR = 1 / (2σ²).
            let sigma = (1.0 / (2.0 * 10f32.powf(true_snr_db / 10.0))).sqrt();
            let (mut rx, mut dec) = (Vec::new(), Vec::new());
            for k in 0..2048usize {
                let s = if rng() > 0.0 {
                    Complex32::new(1.0, 0.0)
                } else {
                    Complex32::new(-1.0, 0.0)
                };
                // A slow, deep, rotating fade — exactly what defeats an EVM/M2M4 estimator.
                let t = k as f32 / 2048.0;
                let mag = 0.15 + 0.85 * (1.0 + (6.0 * t).sin()) / 2.0;
                let ph = 9.0 * t;
                let h = Complex32::new(mag * ph.cos(), mag * ph.sin());
                // Box-Muller-free: sum of uniforms is close enough to Gaussian for a power check.
                let n = Complex32::new(
                    sigma * (rng() + rng() + rng()) * 0.816,
                    sigma * (rng() + rng() + rng()) * 0.816,
                );
                rx.push(h * s + n);
                dec.push(s);
            }
            let est = additive_snr_db_windowed(&rx, &dec, 16);
            assert!(
                (est - true_snr_db).abs() < 3.0,
                "true {true_snr_db} dB through a rotating fade → estimated {est:.1} dB; the windowed \
                 gain must remove the multiplicative channel, not count it as noise"
            );
        }
    }

    /// The gain removal is the whole mechanism: without it (one global gain over the block, i.e. an
    /// enormous window) the same fading input must read far too low — that is the bug this guards.
    #[test]
    fn without_windowing_the_fade_is_counted_as_noise() {
        let mut seed = 999u64;
        let mut rng = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed as i32 as f32) / (i32::MAX as f32)
        };
        let (mut rx, mut dec) = (Vec::new(), Vec::new());
        for k in 0..2048usize {
            let s = if rng() > 0.0 {
                Complex32::new(1.0, 0.0)
            } else {
                Complex32::new(-1.0, 0.0)
            };
            let t = k as f32 / 2048.0;
            let ph = 9.0 * t;
            let h = Complex32::new(ph.cos(), ph.sin()); // pure rotation, NO noise at all
            rx.push(h * s);
            dec.push(s);
        }
        // Windowed: the rotation is tracked out → essentially noiseless → a large SNR.
        let windowed = additive_snr_db_windowed(&rx, &dec, 16);
        // One global window: the rotation cannot be tracked and is counted as "noise".
        let global = additive_snr_db_windowed(&rx, &dec, rx.len());
        assert!(
            windowed > 20.0,
            "a noiseless rotating channel must read as high SNR once the gain is removed, got {windowed:.1}"
        );
        assert!(
            windowed > global + 15.0,
            "windowing is the mechanism: windowed {windowed:.1} dB vs single-gain {global:.1} dB — \
             a single gain counts the rotation as noise, which is the #934 defect"
        );
    }

    #[test]
    fn qam32_nearest_neighbours_are_low_hamming() {
        // Lock in the 2D-Gray optimization of the cross-32QAM label→point table: adjacent points must
        // differ by few bits (the old 1D-Gray-over-raster mapping averaged ~2.0; the optimized table
        // ~1.36). This is what the soft LLRs / BER depend on; guards against regressing the mapping.
        let pts = constellation_points(5);
        let step = 2.0 * QAM32_SCALE; // nearest-neighbour spacing in normalized units
        let tol = step * 0.1;
        let (mut total, mut count) = (0.0f32, 0.0f32);
        for i in 0..pts.len() {
            for j in (i + 1)..pts.len() {
                if ((pts[i].1 - pts[j].1).norm() - step).abs() < tol {
                    total += (pts[i].0 ^ pts[j].0).count_ones() as f32;
                    count += 1.0;
                }
            }
        }
        let avg = total / count;
        assert!(
            avg < 1.6,
            "cross-32QAM nearest-neighbour avg Hamming {avg:.3} too high — mapping regressed?"
        );
    }
}
