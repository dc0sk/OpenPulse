//! Ablation: is SC-FDMA's flat ~0.35 `moderate_f1` decode plateau information-theoretic outage, or a
//! SC-FDE *receiver* limit? (Fable flagged that a flat-across-SNR number is this repo's bug signature.)
//!
//! The repo's rule: delete the mechanism the explanation depends on. Here the explanation is "noise +
//! deep-fade outage", so we remove the **noise** (60 dB SNR) and compare SC-FDMA against OFDM on the
//! *same* `moderate_f1` fade realisations. Both carry the identical CP/pilot geometry, so if OFDM
//! decodes noiselessly where SC-FDMA does not, the gap is the SC-FDE receiver (DFT-spread noise
//! smearing + the ±10-sample CE reach), not an erased-subcarrier information limit that would sink
//! both. We also freeze the channel (0 Hz Doppler) to rule out time-variation.
//!
//! FINDING (60 draws, noiseless): SC-FDMA is **not** outage-limited on moderate_f1 — it decodes 0.90
//! with the channel FROZEN (0 Hz Doppler) and collapses to 0.50 under 1 Hz Doppler; OFDM is immune
//! (1.00 dynamic). So the flat-across-SNR plateau is **intra-frame Doppler that SC-FDMA's channel
//! estimate cannot track** (its per-frame Wiener solver + EMA smoothing lag a moving channel), while
//! OFDM re-estimates from pilots every symbol. A recoverable SC-FDE *receiver* limit, not an
//! information-theoretic erased-subcarrier outage — and a mechanistic reason the OFDM re-seat wins.
//! (Distinct from moderate_f2's 0.03, which is the ±10-sample CE-reach delay-cliff.) SC-FDMA is
//! retired from the ladder, so this is recorded, not fixed.
//!
//! Run: cargo test -p openpulse-modem --no-default-features --test scfdma_plateau_ablation -- --ignored --nocapture

use ofdm_plugin::OfdmPlugin;
use openpulse_channel::{watterson::WattersonChannel, WattersonConfig};
use openpulse_core::fec::FecMode;
use openpulse_modem::channel_sim::ChannelSimHarness;
use scfdma_plugin::ScFdmaPlugin;

const PAYLOAD: &[u8] = b"SC-FDMA plateau ablation payload, sixty-four bytes for a coded run AAA";
const DRAWS: u32 = 60;

fn decode_rate(waveform: &str, mode: &str, doppler_hz: f32) -> f32 {
    let mut ok = 0u32;
    for f in 0..DRAWS {
        let mut h = ChannelSimHarness::new();
        for eng in [&mut h.tx_engine, &mut h.rx_engine] {
            match waveform {
                "ofdm" => eng.register_plugin(Box::new(OfdmPlugin::new())).unwrap(),
                _ => eng.register_plugin(Box::new(ScFdmaPlugin::new())).unwrap(),
            }
        }
        if h.tx_engine
            .transmit_with_fec_mode(PAYLOAD, mode, FecMode::SoftConcatenated, None)
            .is_err()
        {
            continue;
        }
        // moderate_f1 geometry (1 ms / 8-sample delay), but noiseless and optionally frozen.
        let mut cfg = WattersonConfig::moderate_f1(Some(50_000 + f as u64));
        cfg.snr_db = 60.0;
        cfg.doppler_spread_hz = doppler_hz;
        let Ok(mut ch) = WattersonChannel::new(cfg) else {
            continue;
        };
        h.route(&mut ch);
        if h.rx_engine
            .receive_with_fec_mode(mode, FecMode::SoftConcatenated, None)
            .map(|got| got == PAYLOAD)
            .unwrap_or(false)
        {
            ok += 1;
        }
    }
    ok as f32 / DRAWS as f32
}

#[test]
#[ignore = "recorded ablation, not a gate (SC-FDMA is retired from the ladder); run with --ignored --nocapture"]
fn ablation_noiseless_moderate_f1_scfdma_vs_ofdm() {
    println!("\n=== noiseless (60 dB) moderate_f1 decode: SC-FDMA vs OFDM, {DRAWS} draws ===");
    println!("{:<16} {:>10} {:>10}", "mode", "1Hz-dopp", "0Hz(frozen)");
    for (waveform, mode) in [
        ("scfdma", "SCFDMA52-16QAM"),
        ("ofdm", "OFDM52-16QAM"),
        ("scfdma", "SCFDMA52-8PSK"),
        ("ofdm", "OFDM52-8PSK"),
    ] {
        let dyn_rate = decode_rate(waveform, mode, 1.0);
        let frozen_rate = decode_rate(waveform, mode, 0.0);
        println!("{mode:<16} {dyn_rate:>10.2} {frozen_rate:>10.2}");
    }
    println!(
        "\nReading: if OFDM ≫ SC-FDMA with noise removed, the ~0.35 plateau is the SC-FDE receiver \
         (DFT-spread smearing + CE reach), not an information-theoretic outage that would sink both."
    );
}
