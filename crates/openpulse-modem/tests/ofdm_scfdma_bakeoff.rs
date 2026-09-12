//! OFDM vs SC-FDMA bake-off: coded frame-success at matched rate and matched fading draws.
//!
//! The Fable full-chain audit claimed OFDM decisively beats SC-FDMA on frequency-selective fading
//! (OFDM52-16QAM 0.90 vs SCFDMA 0.40 @20 dB on `moderate_f1`), tracing it to SC-FDMA's sync+CE only
//! covering a delay spread of ~8–10 samples while OFDM's cyclic prefix rides through it. If true, the
//! dense HF rungs (`hpx_hf` SL10–SL19) are on the wrong waveform.
//!
//! This is the measurement that decides it: the same 52-subcarrier constellation (equal gross rate),
//! the SAME Watterson realisations (paired seeds), the same SoftConcatenated FEC, across an SNR sweep
//! and two selective profiles. Ignored by default (minutes of decode work); run explicitly:
//!
//!   cargo test -p openpulse-modem --no-default-features --test ofdm_scfdma_bakeoff -- --ignored --nocapture

use std::time::Duration;

use ofdm_plugin::OfdmPlugin;
use openpulse_channel::{
    awgn::AwgnChannel, watterson::WattersonChannel, AwgnConfig, ChannelModel, WattersonConfig,
};
use openpulse_core::fec::FecMode;
use openpulse_core::profile::SessionProfile;
use openpulse_core::rate::SpeedLevel;
use openpulse_modem::channel_sim::ChannelSimHarness;
use scfdma_plugin::ScFdmaPlugin;

const PAYLOAD: &[u8] =
    b"OFDM vs SC-FDMA bake-off payload, sixty-four bytes for a fair coded run AA";
const DRAWS: u32 = 40;

fn ofdm_success_ch(mode: &str, ch: &mut dyn ChannelModel) -> bool {
    let mut h = ChannelSimHarness::new();
    h.tx_engine
        .register_plugin(Box::new(OfdmPlugin::new()))
        .unwrap();
    h.rx_engine
        .register_plugin(Box::new(OfdmPlugin::new()))
        .unwrap();
    if h.tx_engine
        .transmit_with_fec_mode(PAYLOAD, mode, FecMode::SoftConcatenated, None)
        .is_err()
    {
        return false;
    }
    h.route(ch);
    h.rx_engine
        .receive_with_fec_mode_timeout(
            mode,
            FecMode::SoftConcatenated,
            None,
            Duration::from_millis(4000),
        )
        .map(|rx| rx == PAYLOAD)
        .unwrap_or(false)
}

fn scfdma_success_ch(mode: &str, ch: &mut dyn ChannelModel) -> bool {
    let mut h = ChannelSimHarness::new();
    h.tx_engine
        .register_plugin(Box::new(ScFdmaPlugin::new()))
        .unwrap();
    h.rx_engine
        .register_plugin(Box::new(ScFdmaPlugin::new()))
        .unwrap();
    if h.tx_engine
        .transmit_with_fec_mode(PAYLOAD, mode, FecMode::SoftConcatenated, None)
        .is_err()
    {
        return false;
    }
    h.route(ch);
    h.rx_engine
        .receive_with_fec_mode_timeout(
            mode,
            FecMode::SoftConcatenated,
            None,
            Duration::from_millis(4000),
        )
        .map(|rx| rx == PAYLOAD)
        .unwrap_or(false)
}

fn ofdm_success(mode: &str, cfg: &WattersonConfig) -> bool {
    ofdm_success_ch(mode, &mut WattersonChannel::new(cfg.clone()).unwrap())
}

fn scfdma_success(mode: &str, cfg: &WattersonConfig) -> bool {
    scfdma_success_ch(mode, &mut WattersonChannel::new(cfg.clone()).unwrap())
}

fn cfg_at(profile: &str, snr_db: f32, seed: u64) -> WattersonConfig {
    let mut c = match profile {
        "moderate_f1" => WattersonConfig::moderate_f1(Some(seed)),
        "moderate_f2" => WattersonConfig::moderate_f2(Some(seed)),
        "poor_f1" => WattersonConfig::poor_f1(Some(seed)),
        _ => unreachable!(),
    };
    c.snr_db = snr_db;
    c
}

/// Re-seat gate (non-ignored): the `hpx_hf` dense rung SL12 must now decode on `moderate_f1` selective
/// fading, where the SC-FDMA mode it replaced sat at ~0.35. Reads the mode straight from the profile,
/// so it fails if the SL12 rung is ever reverted to a delay-cliffed single-carrier mode.
#[test]
fn reseated_sl12_decodes_on_moderate_f1() {
    let p = SessionProfile::hpx_hf();
    let mode = p.mode_for(SpeedLevel::Sl12).expect("SL12 mapped");
    assert!(
        mode.starts_with("OFDM"),
        "SL12 must be an OFDM rung; got {mode}"
    );
    let draws = 20u32;
    let ok = (0..draws)
        .filter(|&s| ofdm_success(mode, &cfg_at("moderate_f1", 22.0, 30_000 + s as u64)))
        .count();
    let rate = ok as f32 / draws as f32;
    assert!(
        rate >= 0.70,
        "re-seated SL12 ({mode}) decoded only {rate:.2} on moderate_f1 @22 dB (SC-FDMA sat ~0.35)"
    );
}

/// Fable's due-diligence: the dense rungs must not trade down on BENIGN channels (flat AWGN, mild
/// good_f1 fading) if we re-seat them to OFDM. If SC-FDMA meaningfully beat OFDM here, a blanket
/// re-seat would cost benign-channel throughput.
#[test]
#[ignore = "bake-off measurement (minutes of decode work); run with --ignored --nocapture"]
fn bakeoff_benign() {
    let constellations = ["16QAM", "64QAM"];
    let snrs = [14.0f32, 18.0, 22.0, 26.0];
    let draws = 25u32;
    println!("\n=== BENIGN: OFDM vs SC-FDMA coded frame-success ({draws} paired draws) ===");
    println!(
        "{:<10} {:<8} {:>5}  {:>6}  {:>6}  {:>6}",
        "channel", "constell", "SNR", "OFDM", "SCFDMA", "delta"
    );
    for con in constellations {
        let ofdm_mode = format!("OFDM52-{con}");
        let scfdma_mode = format!("SCFDMA52-{con}");
        for snr in snrs {
            // AWGN (flat).
            let (mut o_ok, mut s_ok) = (0u32, 0u32);
            for d in 0..draws {
                let seed = 60_000 + d as u64;
                if ofdm_success_ch(
                    &ofdm_mode,
                    &mut AwgnChannel::new(AwgnConfig::new(snr, Some(seed))).unwrap(),
                ) {
                    o_ok += 1;
                }
                if scfdma_success_ch(
                    &scfdma_mode,
                    &mut AwgnChannel::new(AwgnConfig::new(snr, Some(seed))).unwrap(),
                ) {
                    s_ok += 1;
                }
            }
            println!(
                "{:<10} {con:<8} {snr:>5.0}  {:>6.2}  {:>6.2}  {:>+6.2}",
                "awgn",
                o_ok as f32 / draws as f32,
                s_ok as f32 / draws as f32,
                (o_ok as f32 - s_ok as f32) / draws as f32
            );
            // good_f1 (0.5 ms / 0.5 Hz — mild selective, within SC-FDMA's delay reach).
            let (mut o_ok, mut s_ok) = (0u32, 0u32);
            for d in 0..draws {
                let seed = 70_000 + d as u64;
                let mut cfg = WattersonConfig::good_f1(Some(seed));
                cfg.snr_db = snr;
                if ofdm_success(&ofdm_mode, &cfg) {
                    o_ok += 1;
                }
                if scfdma_success(&scfdma_mode, &cfg) {
                    s_ok += 1;
                }
            }
            println!(
                "{:<10} {con:<8} {snr:>5.0}  {:>6.2}  {:>6.2}  {:>+6.2}",
                "good_f1",
                o_ok as f32 / draws as f32,
                s_ok as f32 / draws as f32,
                (o_ok as f32 - s_ok as f32) / draws as f32
            );
        }
    }
}

#[test]
#[ignore = "bake-off measurement (minutes of decode work); run with --ignored --nocapture"]
fn bakeoff() {
    let constellations = ["16QAM", "64QAM"];
    let profiles = ["moderate_f1", "moderate_f2"];
    let snrs = [16.0f32, 20.0, 24.0, 28.0];

    println!(
        "\n=== OFDM vs SC-FDMA coded frame-success (SoftConcatenated, {DRAWS} paired draws) ==="
    );
    println!(
        "{:<10} {:<12} {:>5}  {:>6}  {:>6}  {:>6}",
        "profile", "constell", "SNR", "OFDM", "SCFDMA", "delta"
    );
    for profile in profiles {
        for con in constellations {
            let ofdm_mode = format!("OFDM52-{con}");
            let scfdma_mode = format!("SCFDMA52-{con}");
            for snr in snrs {
                let mut ofdm_ok = 0u32;
                let mut scf_ok = 0u32;
                for d in 0..DRAWS {
                    let seed = 20_000 + d as u64;
                    let cfg = cfg_at(profile, snr, seed);
                    if ofdm_success(&ofdm_mode, &cfg) {
                        ofdm_ok += 1;
                    }
                    if scfdma_success(&scfdma_mode, &cfg) {
                        scf_ok += 1;
                    }
                }
                let o = ofdm_ok as f32 / DRAWS as f32;
                let s = scf_ok as f32 / DRAWS as f32;
                println!(
                    "{profile:<10} {con:<12} {snr:>5.0}  {o:>6.2}  {s:>6.2}  {:>+6.2}",
                    o - s
                );
            }
        }
    }
}
