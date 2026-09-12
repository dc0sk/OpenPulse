//! Calibration + monotonicity gate for the decoder's per-decode SNR estimate (2500 Hz ref BW).
//!
//! The estimate must track the injected SNR — the same calibrated-AWGN model as the B-6 floor sweep
//! (`σ² = Ps·(fs/2)/2500 / 10^(snr/10)`). `SNR_CAL_OFFSET_DB` in `decoder.rs` is fitted so the mean
//! estimate matches truth; `characterize` (ignored) prints the fit, `tracks_injected_snr` locks it in.

use js8_plugin::costas::CostasKind;
use js8_plugin::decoder::{decode_window, DecodeCfg};
use js8_plugin::message::js8_info_bits;
use js8_plugin::modulate::{modulate_tones, GfskParams};
use js8_plugin::submode::{params, Submode};
use js8_plugin::tones::message_to_tones;

fn payload9(seed: u64) -> [u8; 9] {
    let mut s = seed;
    let mut p = [0u8; 9];
    for b in p.iter_mut() {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
        *b = (s >> 40) as u8;
    }
    p
}

/// White Gaussian noise via Box–Muller over an LCG (deterministic), matching the B-6 harness.
struct Rng(u64);
impl Rng {
    fn u(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((self.0 >> 11) as f64 / (1u64 << 53) as f64) as f32
    }
    fn gauss(&mut self) -> f32 {
        let u1 = self.u().max(1e-7);
        let u2 = self.u();
        (-2.0 * u1.ln()).sqrt() * (std::f32::consts::TAU * u2).cos()
    }
}

/// Mean estimated SNR over the successful decodes among `trials` at a true `snr_db` (2500 Hz ref BW),
/// plus the number of successful decodes. Base tone 1500 Hz, NORMAL submode.
fn mean_estimate(snr_db: f32, trials: u32) -> (f32, u32) {
    let sm = params(Submode::Normal);
    let base = 1500.0;
    let mut acc = 0.0f64;
    let mut n = 0u32;
    for t in 0..trials {
        let want = payload9(t as u64 + 1);
        let info = js8_info_bits(&want, (t % 8) as u8);
        let tones = message_to_tones(&info, CostasKind::Original);
        let sig = modulate_tones(&tones, base, &GfskParams::from_submode(&sm));
        let ps: f32 = sig.iter().map(|x| x * x).sum::<f32>() / sig.len() as f32;
        let sigma = (ps * (4000.0 / 2500.0) / 10f32.powf(snr_db / 10.0)).sqrt();
        let mut rng = Rng(0x51ed_u64.wrapping_add(t as u64).wrapping_mul(2654435761));
        let mut audio = sig;
        for v in audio.iter_mut() {
            *v += sigma * rng.gauss();
        }
        let cfg = DecodeCfg {
            base_min: base - 15.0,
            base_max: base + 15.0,
            base_step: 3.125,
            ..DecodeCfg::default()
        };
        for d in decode_window(&audio, &sm, &cfg) {
            if d.payload == want {
                acc += d.snr_db as f64;
                n += 1;
            }
        }
    }
    (
        if n > 0 {
            (acc / n as f64) as f32
        } else {
            f32::NAN
        },
        n,
    )
}

/// Print the raw estimate vs truth across the sweep — used to fit `SNR_CAL_OFFSET_DB`.
#[test]
#[ignore = "characterization: prints estimate vs truth, used to fit SNR_CAL_OFFSET_DB; run with --ignored --nocapture"]
fn characterize() {
    println!("  true(dB)   est(dB)  err(dB)  decodes");
    for &snr in &[-18.0f32, -15.0, -12.0, -9.0, -6.0, -3.0, 0.0, 3.0, 6.0, 9.0] {
        let (est, n) = mean_estimate(snr, 12);
        println!("  {snr:7.1}  {est:8.2}  {:7.2}  {n:>2}/12", est - snr);
    }
}

/// The estimate tracks the injected SNR: within 2 dB of truth across the JS8 weak-signal band where
/// discovery operates (−12…+3 dB, decodes consistent) and strictly monotone across the wider sweep.
/// Above +3 dB the non-coherent estimate compresses (guard-bin pulse leakage), which is immaterial —
/// `route_quality` saturates there anyway — so accuracy is gated only on the weak-signal band.
#[test]
fn tracks_injected_snr() {
    // Accuracy band: within 2 dB where it matters.
    for &snr in &[-12.0f32, -9.0, -6.0, -3.0, 0.0, 3.0] {
        let (est, n) = mean_estimate(snr, 12);
        assert!(n >= 8, "need reliable decodes at {snr} dB (got {n}/12)");
        assert!(
            (est - snr).abs() <= 2.0,
            "estimate {est:.2} dB should be within 2 dB of injected {snr} dB"
        );
    }
    // Monotonicity across the full sweep, including the compressed high-SNR tail.
    let mut prev = f32::NEG_INFINITY;
    for &snr in &[-12.0f32, -9.0, -6.0, -3.0, 0.0, 3.0, 6.0, 9.0] {
        let (est, _) = mean_estimate(snr, 12);
        assert!(
            est > prev,
            "estimate must increase with SNR: {est:.2} after {prev:.2} (at {snr} dB)"
        );
        prev = est;
    }
}
