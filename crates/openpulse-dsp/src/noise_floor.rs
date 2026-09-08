//! Passband noise-floor estimation that an in-band signal cannot poison.
//!
//! **Why a spectral estimate rather than a time-domain one.** The obvious way to find a noise floor
//! is a low percentile of recent *block energies*, which is what the receive scan's `EnergyGate`
//! does. That works only while most blocks are noise. A carrier that stays on — a long frame, a
//! tuning signal, a permanently-busy band — raises every block, so the percentile follows the signal
//! up and the "floor" becomes the thing it was supposed to measure against.
//!
//! A **spectral** floor cannot be captured that way. A narrowband signal occupies a minority of the
//! passband's bins, so a low percentile *across bins* still lands on noise-only bins no matter how
//! long the signal persists or how strong it is. This is the estimator Mercury uses for its
//! channel-busy decision (`modem/channel_busy.c`), and it is the reason its floor survives a
//! transmission where ours does not.
//!
//! It is deliberately waveform-agnostic: a noise floor is a property of the band, not of the mode
//! being received, so nothing here takes a mode.

use rustfft::num_complex::Complex32;
use rustfft::FftPlanner;

/// Analysis window length — and the contract callers depend on: **the adaptive squelch engages
/// after this many samples of audio, however the caller chunked them** (#1254).
///
/// 512 samples at 8 kHz is 64 ms and 15.6 Hz per bin — fine enough that a
/// 500–1000 Hz signal covers well under half the passband's bins, which is what keeps the low
/// percentile on noise.
pub const WINDOW: usize = 512;

/// Percentile of passband bin powers taken as the noise level, before bias correction.
///
/// Low enough that a signal filling nearly half the passband cannot reach it, high enough to be a
/// stable statistic (the minimum of many bins is a wildly variable order statistic).
const QUANTILE: f32 = 0.25;

/// Hann window coherent power gain, `Σw²/N` = 3/8. Divides out of the periodogram scaling below.
const HANN_POWER_GAIN: f32 = 0.375;

/// `-ln(1 - QUANTILE)`: the 25th-percentile point of an exponential distribution in units of its
/// own mean. Periodogram bin powers of Gaussian noise are exponentially distributed, so the
/// quantile sits at this fraction of the mean and must be divided back out. **Derived, not fitted**
/// — `noise_floor_recovers_a_known_variance` checks the whole chain against synthetic noise of known
/// variance rather than against any recording.
const EXP_QUANTILE_SCALE: f32 = 0.287_682_07;

/// Estimate the **mean-square** level of the noise underlying `samples`, from the distribution of
/// power across passband bins in `lo_hz..hi_hz`.
///
/// Returns `None` when there is not a full analysis window, or when the requested band holds too few
/// bins for a percentile to mean anything.
pub fn spectral_noise_floor_mean_sq(
    samples: &[f32],
    sample_rate: f32,
    lo_hz: f32,
    hi_hz: f32,
) -> Option<f32> {
    if samples.len() < WINDOW || sample_rate <= 0.0 || hi_hz <= lo_hz {
        return None;
    }
    let lo_bin = ((lo_hz / sample_rate) * WINDOW as f32).round().max(1.0) as usize;
    let hi_bin = (((hi_hz / sample_rate) * WINDOW as f32).round() as usize).min(WINDOW / 2 - 1);
    if hi_bin <= lo_bin + 8 {
        return None;
    }

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(WINDOW);

    // POOL every (window, bin) power into one sample set rather than averaging the periodograms
    // first. Averaging M periodograms turns each value from Exponential into Gamma(M), whose 25th
    // percentile sits near its mean instead of at 0.288 of it — so the bias correction below would
    // over-divide by ~M/…, measured as a flat 3.06x overestimate at M = 32. Pooled values stay
    // exponential, which is what `EXP_QUANTILE_SCALE` assumes, and a low percentile over more
    // samples is the more stable statistic anyway.
    let windows = samples.len() / WINDOW;
    let mut powers: Vec<f32> = Vec::with_capacity(windows * (hi_bin - lo_bin + 1));
    for w in 0..windows {
        let seg = &samples[w * WINDOW..(w + 1) * WINDOW];
        let mut buf: Vec<Complex32> = seg
            .iter()
            .enumerate()
            .map(|(n, &x)| {
                let hann =
                    0.5 * (1.0 - (2.0 * std::f32::consts::PI * n as f32 / WINDOW as f32).cos());
                Complex32::new(x * hann, 0.0)
            })
            .collect();
        fft.process(&mut buf);
        powers.extend(buf[lo_bin..=hi_bin].iter().map(|c| c.norm_sqr()));
    }
    powers.sort_by(f32::total_cmp);
    let q = powers[((powers.len() - 1) as f32 * QUANTILE) as usize];

    // Undo the exponential-quantile bias, then the periodogram scaling: for white noise of variance
    // σ², E[|X_k|²] = σ² · N · (Σw²/N). Hence σ² = mean_bin_power / (N · HANN_POWER_GAIN).
    let mean_bin_power = q / EXP_QUANTILE_SCALE;
    Some(mean_bin_power / (WINDOW as f32 * HANN_POWER_GAIN))
}

/// Smoothed passband noise floor, for driving a squelch that follows the band.
///
/// Asymmetric on purpose: it rises slowly and falls quickly. A floor that jumps up on one noisy
/// block would desensitise the receiver exactly when a burst arrives, while a floor that lingers
/// high after the band quietens keeps it deaf; the safe direction for a *detector* is to be slow to
/// believe the band got worse.
#[derive(Debug, Clone)]
pub struct NoiseFloorTracker {
    mean_sq: Option<f32>,
    /// Samples not yet consumed by a full analysis window (#1254).
    ///
    /// Without this the tracker was a function of the CALLER'S CHUNKING, not of the audio: `update`
    /// discarded any buffer under `WINDOW` outright. The daemon's rx tick hands it whatever one
    /// `read()` drained — period-quantized, ~200-600 samples at 8 kHz — so observations happened
    /// only on the reads that happened to straddle 512, and the EMA retained a value derived from
    /// that biased subset indefinitely. Bounded: never holds `WINDOW` samples or more.
    carry: Vec<f32>,
    rise: f32,
    fall: f32,
    lo_hz: f32,
    hi_hz: f32,
}

impl Default for NoiseFloorTracker {
    fn default() -> Self {
        Self::new(300.0, 2_700.0)
    }
}

impl NoiseFloorTracker {
    /// Track the floor over `lo_hz..hi_hz` (the SSB passband, by default 300–2700 Hz).
    pub fn new(lo_hz: f32, hi_hz: f32) -> Self {
        Self {
            mean_sq: None,
            carry: Vec::new(),
            rise: 0.05,
            fall: 0.30,
            lo_hz,
            hi_hz,
        }
    }

    /// Fold captured audio into the estimate; returns the current floor if one exists.
    ///
    /// Accepts any block length. Samples are buffered across calls and consumed one `WINDOW` at a
    /// time, so the result depends only on the sample stream and not on how the caller chunked it —
    /// pinned by `the_floor_is_invariant_to_the_caller_s_chunking` (#1254).
    ///
    /// The EMA steps **per window**, not per call: pooling an arbitrary number of windows into one
    /// observation made a 64 ms read and a 5 s read each worth one step, so the smoothing rate
    /// tracked the driver's period rather than elapsed audio.
    pub fn update(&mut self, samples: &[f32], sample_rate: f32) -> Option<f32> {
        if sample_rate <= 0.0 || self.hi_hz <= self.lo_hz {
            return self.mean_sq;
        }
        self.carry.extend_from_slice(samples);
        let mut consumed = 0;
        while self.carry.len() - consumed >= WINDOW {
            let obs = spectral_noise_floor_mean_sq(
                &self.carry[consumed..consumed + WINDOW],
                sample_rate,
                self.lo_hz,
                self.hi_hz,
            );
            consumed += WINDOW;
            let Some(obs) = obs else { continue };
            self.mean_sq = Some(match self.mean_sq {
                None => obs,
                Some(prev) => {
                    let a = if obs > prev { self.rise } else { self.fall };
                    prev + a * (obs - prev)
                }
            });
        }
        self.carry.drain(..consumed);
        self.mean_sq
    }

    /// Current floor as mean-square, or `None` before the first full window.
    pub fn mean_sq(&self) -> Option<f32> {
        self.mean_sq
    }

    /// Current floor as RMS amplitude — the unit a squelch threshold is expressed in.
    pub fn rms(&self) -> Option<f32> {
        self.mean_sq.map(|m| m.sqrt())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE #1254 GATE: the floor is a function of the AUDIO, not of how the caller chunked it.
    ///
    /// `update` used to discard any block under `WINDOW`, so the estimate depended on the driver's
    /// period and the tick's phase. The daemon's rx tick hands over one `read()` — period-quantized,
    /// ~200-600 samples at 8 kHz — so observations landed only on reads that happened to straddle
    /// 512, from a biased subset (the slow ticks), and the EMA then retained that value forever.
    ///
    /// Neither the old code nor "raise the tick until reads exceed 512" can pass this: the second
    /// re-tunes one constant against another and still breaks whenever the driver chunks differently.
    #[test]
    fn the_floor_is_invariant_to_the_caller_s_chunking() {
        let audio = white(512 * 20, 0.05, 7);
        let reference = {
            let mut t = NoiseFloorTracker::default();
            t.update(&audio, 8000.0);
            t.mean_sq().expect("one shot warms")
        };

        // 200 is cpal's default ALSA period at 8 kHz; 400 is one nominal 50 ms daemon tick; 600 is a
        // tick that straddled two periods; 4096 is the read after a blocking decode (#1301).
        for chunk in [200usize, 400, 512, 600, 800, 4096] {
            let mut t = NoiseFloorTracker::default();
            let mut last = None;
            for block in audio.chunks(chunk) {
                last = t.update(block, 8000.0);
            }
            let got = last.expect("every chunking must warm on 20 windows of audio");
            assert_eq!(
                got.to_bits(),
                reference.to_bits(),
                "chunk {chunk}: floor {got:e} != one-shot {reference:e} — the estimate still \
                 depends on the caller's block size, so the driver's period decides the squelch"
            );
        }
    }

    /// The daemon's own read size must warm the tracker at all — the branch #1254 was filed on.
    #[test]
    fn a_sub_window_read_warms_the_tracker_once_enough_audio_has_arrived() {
        let audio = white(512 * 4, 0.05, 11);
        let mut t = NoiseFloorTracker::default();
        // 400 samples = 8000 Hz x the default `receive_tick_ms` of 50.
        let mut blocks = audio.chunks(400);
        assert!(
            t.update(blocks.next().expect("first"), 8000.0).is_none(),
            "400 samples is under one window, so nothing can be estimated yet"
        );
        for b in blocks {
            t.update(b, 8000.0);
        }
        assert!(
            t.mean_sq().is_some(),
            "four windows of audio delivered in 400-sample reads left the tracker cold — the \
             adaptive squelch never engages and the fixed threshold governs"
        );
    }

    fn white(n: usize, sigma: f32, seed: u64) -> Vec<f32> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                // xorshift + Box-Muller-ish sum of uniforms → near-Gaussian, deterministic.
                let mut acc = 0.0f32;
                for _ in 0..4 {
                    s ^= s >> 12;
                    s ^= s << 25;
                    s ^= s >> 27;
                    let u =
                        (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64;
                    acc += u as f32 - 0.5;
                }
                acc * sigma * 1.732
            })
            .collect()
    }

    /// The estimator must recover a KNOWN variance. This is what makes the constants derived rather
    /// than fitted: nothing here is tuned against a recording.
    #[test]
    fn noise_floor_recovers_a_known_variance() {
        for sigma in [0.003f32, 0.01, 0.05, 0.2] {
            let x = white(16_384, sigma, 0xC0FFEE);
            let est =
                spectral_noise_floor_mean_sq(&x, 8_000.0, 300.0, 2_700.0).expect("enough samples");
            let truth = x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32;
            let ratio = est / truth;
            assert!(
                (0.6..1.7).contains(&ratio),
                "sigma {sigma}: estimated floor {est:.6} vs true {truth:.6} (ratio {ratio:.2}) — \
                 the periodogram or quantile scaling is wrong"
            );
        }
    }

    /// THE PROPERTY THE WHOLE MODULE EXISTS FOR: a strong in-band carrier must not raise the floor.
    ///
    /// A time-domain percentile fails this by construction, which is why the receive scan's energy
    /// gate saturates on a hot band. Without this assertion the module would just be a slower way to
    /// compute the same broken number.
    #[test]
    fn a_strong_carrier_does_not_poison_the_floor() {
        let noise = white(16_384, 0.01, 0x5EED);
        let clean = spectral_noise_floor_mean_sq(&noise, 8_000.0, 300.0, 2_700.0).unwrap();

        // A loud 1500 Hz carrier on top — 30x the noise amplitude.
        let with_carrier: Vec<f32> = noise
            .iter()
            .enumerate()
            .map(|(n, &v)| {
                v + 0.3 * (2.0 * std::f32::consts::PI * 1_500.0 * n as f32 / 8_000.0).cos()
            })
            .collect();
        let polluted =
            spectral_noise_floor_mean_sq(&with_carrier, 8_000.0, 300.0, 2_700.0).unwrap();

        // The time-domain view for contrast: this is what a block-energy percentile would see.
        let time_domain =
            with_carrier.iter().map(|v| v * v).sum::<f32>() / with_carrier.len() as f32;

        assert!(
            polluted < clean * 2.0,
            "a carrier raised the spectral floor from {clean:.6} to {polluted:.6} — it is not \
             poison-resistant, which is the only reason to prefer it over a block-energy percentile"
        );
        assert!(
            time_domain > clean * 10.0,
            "the carrier did not actually dominate the time-domain level ({time_domain:.6} vs \
             {clean:.6}), so this test is not demonstrating the contrast it claims"
        );
    }

    #[test]
    fn tracker_follows_a_rising_floor_and_reports_rms() {
        let mut t = NoiseFloorTracker::default();
        for _ in 0..50 {
            t.update(&white(4_096, 0.01, 0xA1), 8_000.0);
        }
        let low = t.rms().expect("floor after updates");
        for _ in 0..200 {
            t.update(&white(4_096, 0.1, 0xB2), 8_000.0);
        }
        let high = t.rms().expect("floor");
        assert!(
            high > low * 3.0,
            "floor did not follow a 10x level change: {low:.5} → {high:.5}"
        );
    }

    #[test]
    fn short_input_yields_no_estimate() {
        assert!(spectral_noise_floor_mean_sq(&[0.0; 100], 8_000.0, 300.0, 2_700.0).is_none());
    }
}
