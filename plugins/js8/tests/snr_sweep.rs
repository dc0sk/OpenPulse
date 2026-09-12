//! JS8 NORMAL weak-signal decode floor (SNR in the 2500 Hz reference bandwidth).
//!
//! The Phase-B go/no-go: the native decoder must reach the −18 dB class (plan §11), else the D1
//! fallback (external JS8Call for RX) is triggered. Measured: 12/12 down to −15 dB, 11/12 at −18 dB,
//! floor at −21 dB — so **native decode passes** and the fallback is not needed. `gate_at_minus_18_db`
//! locks that in as a regression test; `characterize_decode_floor` (ignored) prints the full sweep.

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

/// White Gaussian noise via Box–Muller over an LCG (deterministic).
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

/// Decode rate over `trials` at `snr_db` (2500 Hz ref BW), base tone 1500 Hz.
fn decode_rate(snr_db: f32, trials: u32) -> u32 {
    let sm = params(Submode::Normal);
    let base = 1500.0;
    let mut ok = 0;
    for t in 0..trials {
        let want = payload9(t as u64 + 1);
        let info = js8_info_bits(&want, (t % 8) as u8);
        let tones = message_to_tones(&info, CostasKind::Original);
        let sig = modulate_tones(&tones, base, &GfskParams::from_submode(&sm));
        let ps: f32 = sig.iter().map(|x| x * x).sum::<f32>() / sig.len() as f32;
        // σ² for a target SNR in the 2500 Hz ref BW at fs = 8000: Ps·(fs/2)/2500 / 10^(snr/10).
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
            base_step_coarse: 0.0,
            min_offset: 0,
            max_offset: 0,
            offset_step: 1,
            min_sync_score: 6.0,
            max_candidates: 8,
            bp_iterations: 60,
        };
        if decode_window(&audio, &sm, &cfg)
            .iter()
            .any(|d| d.payload == want)
        {
            ok += 1;
        }
    }
    ok
}

#[test]
fn gate_at_minus_18_db() {
    // The plan's −18 dB Phase-B gate. Measured 11/12 (~92%); require ≥ 6/8 with margin against flake.
    let ok = decode_rate(-18.0, 8);
    assert!(ok >= 6, "JS8 NORMAL decoded {ok}/8 at −18 dB (gate: ≥ 6/8)");
}

#[test]
#[ignore = "characterization: prints the decode rate across the SNR sweep; the gate is gate_at_minus_18_db. Run with --ignored --nocapture"]
fn characterize_decode_floor() {
    println!("SNR(dB)  decode_rate");
    for snr in [0.0, -3.0, -6.0, -9.0, -12.0, -15.0, -18.0, -21.0] {
        println!("{snr:6}   {}/12", decode_rate(snr, 12));
    }
}
