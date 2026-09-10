//! RESEARCH HARNESS — the two measurements that falsified the first QPSK threshold table. No asserts.
//!
//! Kept because both results are reusable instruments, not one-off checks:
//!
//! * `f1_fade_decode_cliff` — the decode column on the channel a rung actually exists for. QPSK250-D
//!   on `moderate_f1` at its own 7 dB floor decodes frames down to ρ = 0.276 (seed 127 of 150),
//!   which is *below* that mode's recorded idle-noise ceiling of 0.291. The decodable-frame and
//!   noise distributions overlap, so no threshold separates them — a fact an AWGN-only decode column
//!   cannot show.
//! * `f2_noise_colour` — the noise ceiling is set by the overlap of the noise spectrum with the
//!   TEMPLATE's spectrum, not by template length alone. A 500 Hz receive filter (an ordinary rig
//!   setting for these modes) lifts idle ρ above every threshold measured so far, including BPSK250's
//!   shipped 0.40. Note the band-limiting here is a brick-wall FFT mask, sharper than any real
//!   filter, so its numbers are a worst case.
//!
//! * `f7_duration_is_the_lever` — in-band discrimination `ρ' = ρ_noise/ρ_signal` follows
//!   `1/√(T·B)`: doubling template duration drops it by ~1/√2 while a 29× change in spectral
//!   occupancy at fixed duration moves it by nothing. Duration, not spreading, sets the noise
//!   floor. (Merged result, #1087.)
//! * `f8_faded_frame_rho_tail` — ρ of a real faded frame, band-limited by the SAME mask as the
//!   ceiling it is compared against. Exists because an earlier version compared an *unfiltered*
//!   tail against a *filtered* ceiling and inverted its own conclusion.
//! * `f9_decode_conditioned_rho_tail` — **produced a NEGATIVE result; read this before using it.**
//!
//! ## `f9`'s decoded-only column is CIRCULAR as it stands (2026-08-06)
//!
//! `f9` asks "would a higher ρ threshold discard frames the channel delivered?" by measuring the
//! miss rate among frames that decoded. **The shipped 0.40 veto runs inside that decode**
//! (`engine.rs`, `rho < veto.rho_threshold → continue`), so a frame scoring under 0.40 is vetoed,
//! fails to decode, and leaves the conditioned set — making the miss rate at 0.40 zero *by
//! construction*. Its own tripwire proves the contamination is live rather than theoretical:
//! 1010–1696 settle rejections per 30 seeds, and **0 of the seeds carrying a rejection ever
//! decoded**, in every cell measured.
//!
//! So the numbers it prints are not evidence about the channel. Making it sound needs a
//! veto-disabled arm — a plugin wrapper returning `None` from `preamble_template`, so
//! `build_preamble_veto` yields `None` and the decode runs ungated. Until then, treat the
//! decoded-only column as a tautology and the all-frames column as unconditioned (it counts frames
//! lost in fade nulls against the threshold, which overstates a threshold's cost).
//!
//! Second known contaminant, unresolved: the decode verdict is **wall-clock bounded** unless
//! `set_deterministic_scan_positions`/`_max_iterations` are set (#1066 — the determinism is
//! opt-in). The budget is swept via `F9_POS`/`F9_ITERS` rather than chosen, because picking one
//! makes the constant the answer; note that even `pos=16, iters=500` does ~8.5× the scan work of
//! the shipped path (14 580 vs 1 696 rejections per 30 seeds) and does not finish in 25 min.
//!
//! ## What none of these can settle
//!
//! Every band-limited figure here uses a **brick-wall FFT mask**, sharper than any real filter.
//! #1060 records the true 500 Hz value as lying between 0.196 (SSB) and 0.441 (brick-wall), against
//! a shipped threshold of 0.40 — so where it actually falls decides whether there is a defect at
//! all, and that is a rig measurement (one ~45 s idle capture with a real 500 Hz filter engaged),
//! not a simulation. Six conclusions were drawn from this file's synthetic columns and six were
//! overturned in review; do not propose a constant change from them alone.
//!
//! Cross-check on the harness itself: synthetic SSB-shaped noise (300–2700 Hz) gives BPSK250
//! ρ = 0.196 against 0.205 measured on the real recorded captures — so this reproduces the corpus,
//! and the corpus is SSB-bandwidth reception.

use openpulse_channel::watterson::WattersonChannel;
use openpulse_channel::{ChannelModel, WattersonConfig};
use openpulse_core::error::ModemError;
use openpulse_core::fec::FecMode;
use openpulse_core::plugin::{
    FrameGeometry, ModulationConfig, ModulationPlugin, PluginInfo, PreambleTemplate,
};
use openpulse_dsp::acquisition::IqMatchedFilter;
use openpulse_modem::capture_replay::{load_corpus, load_wav};
use openpulse_modem::channel_sim::ChannelSimHarness;
use std::time::Duration;

const FS: f32 = 8_000.0;
const PI_F: f32 = std::f32::consts::PI;

fn cfg(mode: &str) -> ModulationConfig {
    ModulationConfig {
        mode: mode.into(),
        sample_rate: 8_000,
        center_frequency: 1_500.0,
        ..Default::default()
    }
}

/// The engine's residual-frequency grid for a template of `tlen` samples and half-width `grid_hz`.
///
/// One definition, used by every measurement in this file. Duplicating the engine's step formula
/// per call site is how a probe silently stops measuring what the receiver does.
fn engine_grid(tlen: usize, grid_hz: f32) -> Vec<f32> {
    let step = (0.25 * FS / tlen as f32).max(0.5);
    let n = (grid_hz / step).round() as i32;
    (-n..=n).map(|k| k as f32 * step).collect()
}

/// Peak rho with the engine's exact template, window and grid.
fn rho_engine(mode: &str, window: &[f32]) -> Option<f32> {
    let t = plugin_template(mode)?;
    let grid = engine_grid(t.0.len(), t.2);
    let mf = IqMatchedFilter::new(t.0);
    if window.len() <= mf.len() {
        return None;
    }
    mf.search_normalized_over_frequency(window, window.len() - mf.len(), 0.05, FS, &grid)
        .map(|(r, _)| r.rho)
}

/// (samples, threshold, grid_hz) for either plugin.
fn plugin_template(mode: &str) -> Option<(Vec<f32>, f32, f32)> {
    if mode.starts_with("BPSK") {
        let t = bpsk_plugin::BpskPlugin::new().preamble_template(&cfg(mode))?;
        Some((t.samples, t.rho_threshold, t.rho_grid_hz))
    } else {
        let t = qpsk_plugin::QpskPlugin::new().preamble_template(&cfg(mode))?;
        Some((t.samples, t.rho_threshold, t.rho_grid_hz))
    }
}

fn win_len(mode: &str) -> usize {
    let t = plugin_template(mode).expect("mode publishes a preamble template");
    let syms = if mode.starts_with("BPSK") {
        bpsk_plugin::modulate::PREAMBLE_SYMS - 1
    } else {
        qpsk_plugin::modulate::PREAMBLE_SYMS - 1
    };
    win_len_for(&t.0, t.0.len() / syms)
}

/// Window length for a template: its own span plus two symbols of slack.
///
/// Takes samples-per-symbol rather than a mode string. The previous form divided by the *shipped*
/// preamble symbol count, which is only correct for a shipped template — applied to a 110-chip PN
/// template it divided by 31 and produced a window shorter than the template it was sizing.
fn win_len_for(samples: &[f32], sps: usize) -> usize {
    samples.len() + 2 * sps
}

// ── F2: does noise COLOUR move the rho ceiling? ───────────────────────────────

/// Band-limited Gaussian noise via FFT bin masking.
fn band_noise(n: usize, lo_hz: f32, hi_hz: f32, seed: u64) -> Vec<f32> {
    use rustfft::{num_complex::Complex, FftPlanner};
    let mut state = seed | 1;
    let mut rnd = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        // `>> 33` leaves 31 bits, so the quotient is [0,1) and the old `- 1.0` produced [-1,0):
        // a DC offset of -0.5. On the white band (which keeps bin 0) that put ~75% of the power
        // in DC, where a DC-free template cannot correlate, and deflated every white-noise row
        // by about 2x. Scale to [-1,1) before centring.
        ((state.wrapping_mul(0x2545F4914F6CDD1D) >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
    };
    let mut buf: Vec<Complex<f32>> = (0..n).map(|_| Complex::new(rnd(), 0.0)).collect();
    let mut planner = FftPlanner::new();
    planner.plan_fft_forward(n).process(&mut buf);
    let bin_hz = FS / n as f32;
    for (k, v) in buf.iter_mut().enumerate() {
        let f = if k <= n / 2 {
            k as f32 * bin_hz
        } else {
            (n - k) as f32 * bin_hz
        };
        if f < lo_hz || f > hi_hz {
            *v = Complex::new(0.0, 0.0);
        }
    }
    planner.plan_fft_inverse(n).process(&mut buf);
    let out: Vec<f32> = buf.iter().map(|c| c.re).collect();
    let rms = (out.iter().map(|s| s * s).sum::<f32>() / n as f32)
        .sqrt()
        .max(1e-12);
    out.iter().map(|s| s / rms * 0.05).collect()
}

#[test]
#[ignore = "verification"]
fn f2_noise_colour() {
    let bands = [
        ("white 0-4k", 0.0f32, 4_000.0),
        ("ssb 300-2700", 300.0, 2_700.0),
        ("filter 1250-1750", 1_250.0, 1_750.0),
        ("filter 1400-1600", 1_400.0, 1_600.0),
    ];
    println!("\nF2: peak rho over 45 s of synthetic noise, engine window/grid");
    println!(
        "{:<18} {:>10} {:>10} {:>10} {:>10}",
        "band", "QPSK125", "QPSK250", "QPSK500", "BPSK250"
    );
    let n = 360_000; // 45 s
    for (name, lo, hi) in bands {
        let noise = band_noise(n, lo, hi, 12345);
        let mut row = vec![];
        for mode in ["QPSK125", "QPSK250", "QPSK500", "BPSK250"] {
            // QPSK published templates only on the withdrawn #1053 branch; at HEAD it returns the
            // trait default `None`. Skipping keeps the BPSK column runnable instead of panicking
            // the whole probe on the first QPSK cell.
            let Some((_, thr, _)) = plugin_template(mode) else {
                row.push("no tmpl".to_string());
                continue;
            };
            let w = win_len(mode);
            let mut peak = 0.0f32;
            let mut s = 0usize;
            while s + w <= noise.len() {
                if let Some(r) = rho_engine(mode, &noise[s..s + w]) {
                    peak = peak.max(r);
                }
                s += w / 4;
            }
            row.push(format!("{peak:.3}{}", if peak >= thr { "*" } else { " " }));
        }
        println!(
            "{name:<18} {:>10} {:>10} {:>10} {:>10}",
            row[0], row[1], row[2], row[3]
        );
    }
    println!("(* = at or above that mode's published threshold)");
    for mode in ["QPSK125", "QPSK250", "QPSK500", "BPSK250"] {
        match plugin_template(mode) {
            Some((_, thr, _)) => println!("  {mode} threshold {thr:.2}"),
            None => println!("  {mode} publishes no template at HEAD"),
        }
    }
}

// ── F8: idle rho from a REAL rig capture (#1060) ──────────────────────────────

/// Peak rho over a whole capture, using this file's engine window, grid and template — the same
/// `rho_engine` every synthetic row above uses, so a rig number is directly comparable to F2's
/// table rather than to a re-transcribed correlator.
fn peak_rho_over_capture(mode: &str, samples: &[f32]) -> Option<f32> {
    let v = rho_stream_over_capture(mode, samples);
    v.iter().copied().fold(None, |a: Option<f32>, r| {
        Some(a.map_or(r, |p: f32| p.max(r)))
    })
}

/// Every per-window value of the statistic the veto actually compares — not its maximum.
///
/// The distinction is load-bearing. Peak-over-capture is an **extreme-value** statistic: it grows
/// with how long you listen (measured: one 500 Hz capture reads 0.319 over 3 s and 0.413 over 45 s),
/// which is why it cannot be compared against a per-window quantile. A runtime calibration works on
/// this stream; the headline numbers in #1060 are its maxima.
fn rho_stream_over_capture(mode: &str, samples: &[f32]) -> Vec<f32> {
    let mut out = Vec::new();
    if plugin_template(mode).is_none() {
        return out; // modes with no template are skipped, not panicked on
    }
    let w = win_len(mode);
    let mut s = 0usize;
    while s + w <= samples.len() {
        if let Some(r) = rho_engine(mode, &samples[s..s + w]) {
            out.push(r);
        }
        s += w / 4;
    }
    out
}

/// Quantile of an unsorted sample by nearest-rank; `q` in 0..=1.
fn quantile(v: &mut [f32], q: f32) -> f32 {
    if v.is_empty() {
        return f32::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let i = ((q * (v.len() - 1) as f32).round() as usize).min(v.len() - 1);
    v[i]
}

/// #1060's decisive measurement: does a REAL rig's narrow receive filter lift idle rho above the
/// shipped threshold, as the brick-wall model predicts (0.441) or as the SSB row suggests (0.196)?
///
/// Positive-controlled by construction: the two recorded corpus captures are measured in the same
/// run with the same code path, and must reproduce the figures already on record (FT-991A ≈ 0.164,
/// IC-9700 hot ≈ 0.205). A new number from a probe that cannot reproduce the old ones means nothing.
///
/// Extra captures come from `OPHF_IDLE_WAV` (comma-separated paths). They must be 8 kHz — the
/// template geometry is sample-rate-specific, and a 48 kHz file would be measured against a template
/// six times too short while looking like a valid result.
///
/// **Peak rho is an extreme-value statistic: it grows with capture DURATION.** The corpus controls
/// are 3 s files; a 45 s capture of the same noise reads higher for that reason alone. Compare rows
/// of equal duration, and say which duration a quoted number came from. Measured on one IC-9700
/// 500 Hz capture: 0.319 over its first 3 s, 0.413 over the full 45 s.
///
/// **The BPSK31/63 columns are not deployed behaviour** if they ever publish templates: their raw
/// templates exceed `MAX_PREAMBLE_CORRELATION_SAMPLES`, so the engine would correlate them through
/// `DdcMatchedFilter` while this probe uses the full-rate `IqMatchedFilter`. Only the BPSK250 column
/// is correlator-identical to the shipped veto.
///
/// Run:
/// `OPHF_IDLE_WAV=/path/idle.wav cargo test -p openpulse-modem --no-default-features \
///   --test preamble_rho_fade_and_filter_probe -- --ignored --nocapture f8_`
#[test]
#[ignore = "verification"]
fn f8_rig_capture_idle_rho() {
    let modes = ["BPSK250", "BPSK63", "BPSK31"];
    println!("\nF8: peak rho over recorded rig idle audio, engine window/grid");
    println!(
        "{:<34} {:>10} {:>9} {:>9} {:>9}",
        "capture", "mean_sq", modes[0], modes[1], modes[2]
    );

    let mut rows: Vec<(String, Vec<f32>)> = Vec::new();
    for name in ["ft991a-idle.wav", "ic9700-idle-hot.wav"] {
        let c = load_corpus(name).unwrap_or_else(|e| panic!("corpus {name}: {e}"));
        rows.push((format!("corpus/{name}"), c.samples));
    }
    if let Ok(list) = std::env::var("OPHF_IDLE_WAV") {
        for p in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let c = load_wav(p).unwrap_or_else(|e| panic!("{p}: {e}"));
            assert_eq!(
                c.sample_rate, 8_000,
                "{p}: capture must be 8 kHz; decimate it (resample_poly) as the corpus files are"
            );
            rows.push((p.to_string(), c.samples));
        }
    }

    for (name, samples) in &rows {
        let ms: f32 = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
        let cells: Vec<String> = modes
            .iter()
            .map(
                |m| match (plugin_template(m), peak_rho_over_capture(m, samples)) {
                    (Some((_, thr, _)), Some(r)) => {
                        format!("{r:.3}{}", if r >= thr { "*" } else { " " })
                    }
                    (None, _) => "no tmpl".into(),
                    (_, None) => "short".into(),
                },
            )
            .collect();
        let short = name.rsplit('/').next().unwrap_or(name);
        println!(
            "{short:<34} {ms:>10.3e} {:>9} {:>9} {:>9}",
            cells[0], cells[1], cells[2]
        );
    }
    for m in modes {
        match plugin_template(m) {
            Some((t, thr, grid)) => println!(
                "  {m}: threshold {thr:.2}, template {} samples, grid ±{grid:.0} Hz",
                t.len()
            ),
            None => println!("  {m}: publishes no template at HEAD"),
        }
    }
    println!(
        "(* = at or above that mode's published threshold — the veto would corroborate noise)"
    );
}

// ── F11: is the QUANTILE RATIO stable across bandwidth? (#1060 fix, shape C) ──

/// The measurement that decides whether a CFAR-style calibration can work without a signal-free
/// oracle.
///
/// A runtime calibration cannot assume it ever sees noise-only windows: in the hot-floor regime the
/// energy gate fires continuously, which is exactly when the calibration is needed. Cell-averaging
/// CFAR solves this by estimating a robust *location* of the statistic and reaching the decision
/// quantile through an assumed distribution family — which works only if the family's shape, i.e.
/// the **ratio** of a high quantile to a robust low one, is stable across the conditions that move
/// the location.
///
/// Here the condition is receive bandwidth. If `p99/p50` holds roughly constant while `p50` moves
/// with bandwidth, the location can be tracked poison-resistantly and the tail extrapolated. If the
/// ratio moves as much as the location does, that extrapolation is a fitted constant in disguise and
/// the design has to change.
///
/// Run:
/// `cargo test -p openpulse-modem --no-default-features --test preamble_rho_fade_and_filter_probe \
///   -- --ignored --nocapture f11_quantile_ratio`
#[test]
#[ignore = "verification"]
fn f11_quantile_ratio_across_bandwidth() {
    let mode = "BPSK250";
    let Some((_, thr, _)) = plugin_template(mode) else {
        println!("{mode} publishes no template at HEAD");
        return;
    };
    println!(
        "\nF11: per-window rho distribution vs receive bandwidth ({mode}, threshold {thr:.2})"
    );
    println!("recorded captures first, then synthetic bands as the mechanism control");
    println!(
        "{:<34} {:>7} {:>7} {:>7} {:>7} {:>7} {:>8} {:>8} {:>9}",
        "source", "n", "p50", "p90", "p99", "max", "p99/p50", "p90/p50", ">=thr"
    );

    let mut rows: Vec<(String, Vec<f32>)> = Vec::new();
    for name in [
        "ft991a-idle.wav",
        "ic9700-idle-hot.wav",
        "ic9700-idle-wide-500hz-control.wav",
        "ic9700-idle-500hz.wav",
        "ic9700-idle-250hz.wav",
    ] {
        match load_corpus(name) {
            Ok(c) => rows.push((
                format!("corpus/{name}"),
                rho_stream_over_capture(mode, &c.samples),
            )),
            Err(e) => println!("  (skipping {name}: {e})"),
        }
    }
    // Synthetic bands: same masks as F2, so the mechanism can be read without rig-specific colour.
    let n = 120_000; // 15 s
    for (label, lo, hi) in [
        ("synth white 0-4k", 0.0f32, 4_000.0f32),
        ("synth ssb 300-2700", 300.0, 2_700.0),
        ("synth 1250-1750", 1_250.0, 1_750.0),
        ("synth 1400-1600", 1_400.0, 1_600.0),
    ] {
        rows.push((
            label.into(),
            rho_stream_over_capture(mode, &band_noise(n, lo, hi, 4_242)),
        ));
    }

    for (name, mut v) in rows {
        if v.is_empty() {
            println!("{name:<34} (no windows)");
            continue;
        }
        let n = v.len();
        let p50 = quantile(&mut v, 0.50);
        let p90 = quantile(&mut v, 0.90);
        let p99 = quantile(&mut v, 0.99);
        let max = quantile(&mut v, 1.0);
        let short = name.rsplit('/').next().unwrap_or(&name).to_string();
        // Windows at or above the SHIPPED constant: the false-corroboration rate the deployed veto
        // runs at today, in the units a CFAR knob would be specified in.
        let over = v.iter().filter(|&&r| r >= thr).count() as f32 / n as f32;
        println!(
            "{short:<34} {n:>7} {p50:>7.3} {p90:>7.3} {p99:>7.3} {max:>7.3} {:>8.2} {:>8.2} {:>8.2}%",
            p99 / p50,
            p90 / p50,
            100.0 * over
        );
    }
    println!(
        "\nRead the RATIO columns: stable ratio + moving p50 = a location tracker plus a family"
    );
    println!("factor is sound. Ratio moving with bandwidth = the factor is a fitted constant.");
}

// ── F3: is the SEQUENCE the variable, or the bandwidth? (#1062) ───────────────

/// A maximal-length LFSR sequence of length `2^bits - 1`, as ±1.
fn m_sequence(bits: u32, taps: &[u32]) -> Vec<f32> {
    let mut reg = vec![true; bits as usize];
    let mut out = Vec::with_capacity((1 << bits) - 1);
    for _ in 0..((1u32 << bits) - 1) {
        out.push(if reg[reg.len() - 1] { 1.0f32 } else { -1.0 });
        let fb = taps.iter().fold(false, |a, &t| a ^ reg[t as usize - 1]);
        reg.rotate_right(1);
        reg[0] = fb;
    }
    out
}

/// Build a PN-chip template through the REAL BPSK modulator.
///
/// The modulator's preamble bits are hardcoded, so the PN chips are placed in the *payload* span
/// instead and that span is returned. Everything downstream — NRZI, half-Hann crossfade, carrier —
/// is the shipped code path, so this differs from `bpsk_preamble_template` in the symbol sequence
/// and nothing else. The NRZI state entering the payload is `+1` (the 32 preamble bits contain 16
/// ones, an even number of flips), so `bit[k] = chip[k] != chip[k-1]` with `chip[-1] = +1` inverts
/// the encoder exactly. The final chip is dropped for the same crossfade reason the shipped
/// template drops its last symbol.
fn pn_template(mode: &str, chips: &[f32]) -> Option<Vec<f32>> {
    let mut bits = Vec::with_capacity(chips.len());
    let mut prev = 1.0f32;
    for &c in chips {
        bits.push(c != prev);
        prev = c;
    }
    let mut bytes = vec![0u8; bits.len().div_ceil(8)];
    for (k, &b) in bits.iter().enumerate() {
        if b {
            bytes[k / 8] |= 1 << (k % 8);
        }
    }
    let c = cfg(mode);
    let full = bpsk_plugin::BpskPlugin::new().modulate(&bytes, &c).ok()?;
    let baud: f32 = mode.trim_start_matches("BPSK").parse().ok()?;
    let n = (FS / baud).round() as usize;
    let start = n * bpsk_plugin::modulate::PREAMBLE_SYMS;
    let span = n * (chips.len() - 1);
    (full.len() >= start + span).then(|| full[start..start + span].to_vec())
}

/// Peak normalised correlation of `template` against `window`, engine-style.
fn rho_of_on_grid(template: &[f32], window: &[f32], grid: &[f32]) -> Option<f32> {
    let mf = IqMatchedFilter::new(template.to_vec());
    if window.len() <= mf.len() {
        return None;
    }
    mf.search_normalized_over_frequency(window, window.len() - mf.len(), 0.05, FS, grid)
        .map(|(r, _)| r.rho)
}

fn rho_of(template: &[f32], window: &[f32], grid_hz: f32) -> Option<f32> {
    let grid = engine_grid(template.len(), grid_hz);
    let mf = IqMatchedFilter::new(template.to_vec());
    if window.len() <= mf.len() {
        return None;
    }
    mf.search_normalized_over_frequency(window, window.len() - mf.len(), 0.05, FS, &grid)
        .map(|(r, _)| r.rho)
}

/// Apply the same brick-wall band mask `band_noise` uses, to an arbitrary signal.
///
/// Same mask for signal and noise, so the two columns of the table are comparable. Brick-wall is a
/// worst case for selectivity; a real rig filter has skirts and sits between this and the SSB row.
fn band_limit(x: &[f32], lo_hz: f32, hi_hz: f32) -> Vec<f32> {
    use rustfft::{num_complex::Complex, FftPlanner};
    let n = x.len().next_power_of_two();
    let mut buf: Vec<Complex<f32>> = x
        .iter()
        .map(|&v| Complex::new(v, 0.0))
        .chain(std::iter::repeat_n(Complex::new(0.0, 0.0), n - x.len()))
        .collect();
    let mut planner = FftPlanner::new();
    planner.plan_fft_forward(n).process(&mut buf);
    let bin_hz = FS / n as f32;
    for (k, v) in buf.iter_mut().enumerate() {
        let f = if k <= n / 2 {
            k as f32 * bin_hz
        } else {
            (n - k) as f32 * bin_hz
        };
        if f < lo_hz || f > hi_hz {
            *v = Complex::new(0.0, 0.0);
        }
    }
    planner.plan_fft_inverse(n).process(&mut buf);
    let scale = 1.0 / n as f32;
    buf.iter().map(|c| c.re * scale).collect()
}

/// Fraction of the DFT bins that actually carry the template's energy (participation ratio /
/// bin count). A two-line spectrum scores near zero however long it runs; flat noise scores 1.
fn band_occupancy(x: &[f32]) -> f32 {
    use rustfft::{num_complex::Complex, FftPlanner};
    let n = 4096;
    let mut buf: Vec<Complex<f32>> = x
        .iter()
        .map(|&v| Complex::new(v, 0.0))
        .chain(std::iter::repeat_n(
            Complex::new(0.0, 0.0),
            n - x.len().min(n),
        ))
        .take(n)
        .collect();
    FftPlanner::new().plan_fft_forward(n).process(&mut buf);
    let p: Vec<f32> = buf.iter().map(|c| c.norm_sqr()).collect();
    let tot: f32 = p.iter().sum();
    if tot <= 0.0 {
        return 0.0;
    }
    let s2: f32 = p.iter().map(|v| (v / tot) * (v / tot)).sum();
    1.0 / s2 / n as f32
}

/// Worst off-peak normalised correlation of a template against a REAL transmission carrying it —
/// how badly a misaligned window still matches. This is the #1052 onset-placement failure as a
/// number.
///
/// Deliberately not measured against a zero-padded copy of the template: `search_normalized`
/// divides by the *window's* energy, so a window half over padding scores ρ ≈ 0.7 by construction
/// and every sequence pins to that artifact regardless of its autocorrelation. Here the template
/// is followed by real modulated data, so every candidate lag has full overlap with real signal.
fn peak_sidelobe(template: &[f32], mode: &str, guard: usize) -> f32 {
    let payload: Vec<u8> = (0..200u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let data = bpsk_plugin::BpskPlugin::new()
        .modulate(&payload, &cfg(mode))
        .expect("modulate payload");
    let sig: Vec<f32> = template.iter().chain(data.iter()).copied().collect();

    // Lags are restricted to a shift within the template's own span, which bounds how much of the
    // filler frame a window can reach. It does NOT exclude the filler's own preamble — `modulate`
    // prepends one — so a lag near the end of the span is partly scoring against that. Harmless to
    // the maximum for these three cases, but the guard is a bound, not an exclusion.
    //
    // Searched over the SHIPPED grid, not at zero frequency: the deployed veto and the noise
    // columns in this same table both search +/-20 Hz, and a sidelobe that only appears under a
    // rotated template is one the receiver will still find. Measuring at zero frequency understates
    // the PN cases by ~25% and flatters them across the threshold.
    let grid = engine_grid(template.len(), 20.0);
    let mf = IqMatchedFilter::new(template.to_vec());
    let mut worst = 0.0f32;
    for lag in (guard + 1)..template.len() {
        if let Some((r, _)) = mf.search_normalized_over_frequency(&sig[lag..], 0, 0.05, FS, &grid) {
            worst = worst.max(r.rho);
        }
    }
    worst
}

#[test]
#[ignore = "verification"]
fn f3_pn_vs_alternating() {
    let alt = plugin_template("BPSK250").expect("BPSK250 template").0;

    // 31 chips at 250/s == the shipped template's duration AND bandwidth: sequence is the only
    // variable. 110 chips at 1000/s == same ~110 ms duration, 4x the chip bandwidth.
    let pn31 = pn_template("BPSK250", &m_sequence(5, &[5, 3])).expect("pn31");
    let mut long = m_sequence(7, &[7, 6]);
    long.truncate(110);
    let pn110 = pn_template("BPSK1000", &long).expect("pn110");

    // The fourth cell of the 2x2: same duration and chip rate as PN-110, but PERIODIC. Without it
    // the noise-ceiling win of PN-110 cannot be attributed to spreading rather than to chip rate,
    // because PN-110 changes both variables at once.
    let alt110: Vec<f32> = {
        let chips: Vec<f32> = (0..110)
            .map(|i| if (i / 2) % 2 == 0 { -1.0 } else { 1.0 })
            .collect();
        pn_template("BPSK1000", &chips).expect("alt110")
    };

    // The sidelobe guard must be half of the TEMPLATE'S OWN symbol period, not a shared constant:
    // BPSK1000 runs 8 samples/chip against BPSK250's 32, so one guard in samples would excise
    // real lag-1 sidelobes from the wideband case — the one whose number looks best.
    let cases: [(&str, &[f32], &str, usize); 4] = [
        ("BPSK250 alternating (shipped)", &alt, "BPSK250", 32),
        ("BPSK250 PN-31 (same BW)", &pn31, "BPSK250", 32),
        ("BPSK1000 PN-110 (4x BW)", &pn110, "BPSK1000", 8),
        ("BPSK1000 alt-110 (4x BW, periodic)", &alt110, "BPSK1000", 8),
    ];

    // Peak sidelobe is reported at three guards because the number is guard-sensitive and the
    // sensitivity is the point: at half a symbol every sequence is still inside the pulse's own
    // mainlobe, so that column measures pulse shape, not sequence. Only past ~1 symbol does the
    // sequence's autocorrelation dominate.
    println!("\nF3: template structure, real modulator path");
    println!(
        "{:<32} {:>8} {:>8} {:>7} {:>10} {:>26}",
        "template", "samples", "ms", "sps", "occupancy", "peak sidelobe @ guard"
    );
    println!(
        "{:<32} {:>8} {:>8} {:>7} {:>10} {:>26}",
        "", "", "", "", "", "0.5 sym   1 sym   2 sym"
    );
    for (name, t, m, sps) in cases {
        println!(
            "{name:<32} {:>8} {:>8.0} {sps:>7} {:>10.3} {:>9.3} {:>7.3} {:>7.3}",
            t.len(),
            t.len() as f32 / FS * 1000.0,
            band_occupancy(t),
            peak_sidelobe(t, m, sps / 2),
            peak_sidelobe(t, m, sps),
            peak_sidelobe(t, m, sps * 2),
        );
    }

    let bands = [
        ("white 0-4k", 0.0f32, 4_000.0),
        ("ssb 300-2700", 300.0, 2_700.0),
        ("filter 1250-1750", 1_250.0, 1_750.0),
        ("filter 1400-1600", 1_400.0, 1_600.0),
    ];
    // NOISE ceiling and SIGNAL response through the SAME filter. The ceiling alone cannot decide a
    // wire change: a template that is undetectable through a filter has a wonderful noise ceiling.
    // The design quantity is the separation between the two columns.
    println!(
        "\nF3: rho through band-limiting -- NOISE ceiling (45 s peak) and SIGNAL (own template)"
    );
    println!(
        "{:<18} {:>9} {:>34} {:>34}",
        "", "", "peak rho over noise", "rho of the template through the filter"
    );
    print!("{:<18}", "band");
    for (n, _, _, _) in cases {
        print!("{:>17}", n.split_whitespace().last().unwrap_or(n));
    }
    for (n, _, _, _) in cases {
        print!("{:>17}", n.split_whitespace().last().unwrap_or(n));
    }
    println!();
    let noise_len = 360_000;
    for (name, lo, hi) in bands {
        let noise = band_noise(noise_len, lo, hi, 12345);
        let mut row = vec![];
        for (_, t, _, sps) in cases {
            let w = win_len_for(t, sps);
            let mut peak = 0.0f32;
            let mut s = 0usize;
            while s + w <= noise.len() {
                if let Some(r) = rho_of(t, &noise[s..s + w], 20.0) {
                    peak = peak.max(r);
                }
                s += w / 4;
            }
            row.push(format!("{peak:.3}"));
        }
        // Signal column: the template itself, band-limited by the same mask, correlated against the
        // UNfiltered template the receiver holds. This is what a real frame looks like to a station
        // running that filter.
        for (_, t, _, sps) in cases {
            let filtered = band_limit(t, lo, hi);
            let w = win_len_for(t, sps).min(filtered.len());
            let rho = rho_of(t, &filtered[..w], 20.0).unwrap_or(f32::NAN);
            row.push(format!("{rho:.3}"));
        }
        print!("{name:<18}");
        for cell in &row {
            print!("{cell:>17}");
        }
        println!();
    }
    println!(
        "  (shipped BPSK250 threshold is 0.40; a template needs SIGNAL above it and NOISE below)"
    );
}

// ── F7: is DURATION the lever for a narrow filter, not spreading? (#1062) ─────

/// Lag hypotheses each template is given per stride, shared by every case.
///
/// F3 sized the window as `template + 2 symbols` and strode by `window/4`, so the lags actually
/// searched were `2·sps` out of every `window/4` — 64 of 264 for the shipped 32-sps template but
/// 16 of 222 for an 8-sps one. A peak-over-noise statistic scales with the number of hypotheses
/// drawn, so that alone biased the wideband cells low by ~3.4x in trial count. Here every template
/// gets the same bound AND strides by exactly that bound, so lag coverage is contiguous and equal.
const F7_LAG_BOUND: usize = 64;

/// Peak rho over one noise realisation, with the equalised lag budget above.
fn peak_rho_equalised(template: &[f32], noise: &[f32]) -> f32 {
    let w = template.len() + F7_LAG_BOUND;
    let mut peak = 0.0f32;
    let mut s = 0usize;
    while s + w <= noise.len() {
        if let Some(r) = rho_of(template, &noise[s..s + w], 20.0) {
            peak = peak.max(r);
        }
        s += F7_LAG_BOUND;
    }
    peak
}

/// F3 showed PN-110 has a lower absolute noise ceiling than the shipped template under a narrow
/// filter (0.347 vs 0.441 at 500 Hz). That reads as a win for spreading — and it is not one.
///
/// `NOISE = ρ' × SIGNAL`, where ρ' is the correlation of in-band noise against the template's
/// *in-band* part: the numerator only sees the in-band component while the normalising denominator
/// carries the template's full energy. Dividing F3's own columns gives ρ' ≈ 0.44 at 500 Hz for the
/// shipped template, PN-31 AND PN-110 alike — identical discrimination. PN-110's lower ceiling is
/// exactly its own signal loss through the filter (SIGNAL 0.796 vs 0.998), which is not a margin
/// you can spend.
///
/// The model says in-band discrimination is set by the number of in-band noise dimensions
/// ≈ duration × filter bandwidth, and all four F3 templates run ~110–124 ms. So the prediction is
/// that **duration**, not bandwidth, is the only lever — and doubling duration should drop ρ' by
/// ~1/√2.
///
/// `ρ' = 1/√(T·B)` is standard detection theory, not a hypothesis this measurement discovers, so
/// treat agreement as **validation of the harness** against a known result — which is exactly what
/// F3 lacked. Read the error bar off the bandwidth axis of the same table: it misses by 4–11 %,
/// which is the same order as the duration axis's headline agreement.
///
/// Template length is **not** a deployability limit either way: `MAX_PREAMBLE_CORRELATION_SAMPLES`
/// is a post-DDC budget and an oversized template is decimated, not refused (phase 0 of #1062), so
/// PN-220's 1 752 samples are unremarkable and a longer probe would be equally runnable.
#[test]
#[ignore = "verification"]
fn f7_duration_is_the_lever() {
    let alt = plugin_template("BPSK250").expect("BPSK250 template").0;
    let mut m110 = m_sequence(7, &[7, 6]);
    m110.truncate(110);
    let pn110 = pn_template("BPSK1000", &m110).expect("pn110");
    let mut m220 = m_sequence(8, &[8, 6, 5, 4]);
    m220.truncate(220);
    let pn220 = pn_template("BPSK1000", &m220).expect("pn220");

    let cases: [(&str, &[f32]); 3] = [
        ("BPSK250 alternating (shipped)", &alt),
        ("BPSK1000 PN-110 (~109 ms)", &pn110),
        ("BPSK1000 PN-220 (~219 ms)", &pn220),
    ];
    let bands = [
        ("ssb 300-2700", 300.0f32, 2_700.0),
        ("filter 1250-1750", 1_250.0, 1_750.0),
        ("filter 1400-1600", 1_400.0, 1_600.0),
    ];
    // Several seeds because a peak over one realisation is a sample of one, and F3's cells sat
    // within ~0.05 of each other. Reported as max and median across seeds so the spread is visible
    // rather than hidden inside a single number.
    let seeds: [u64; 5] = [12345, 777, 90210, 31337, 424242];
    let per_seed = 120_000usize; // 15 s

    println!(
        "\nF7: is duration the lever? equalised lag budget, {} seeds",
        seeds.len()
    );
    for (name, t) in cases {
        println!(
            "\n{name}: {} samples, {:.0} ms, occupancy {:.3}",
            t.len(),
            t.len() as f32 / FS * 1000.0,
            band_occupancy(t)
        );
        println!(
            "  {:<18} {:>12} {:>12} {:>10} {:>12}",
            "band", "NOISE max", "NOISE med", "SIGNAL", "ratio rho'"
        );
        for (bname, lo, hi) in bands {
            let mut peaks: Vec<f32> = seeds
                .iter()
                .map(|&sd| peak_rho_equalised(t, &band_noise(per_seed, lo, hi, sd)))
                .collect();
            peaks.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let (mx, med) = (peaks[peaks.len() - 1], peaks[peaks.len() / 2]);
            let filtered = band_limit(t, lo, hi);
            let w = (t.len() + F7_LAG_BOUND).min(filtered.len());
            let sig = rho_of(t, &filtered[..w], 20.0).unwrap_or(f32::NAN);
            println!(
                "  {bname:<18} {mx:>12.3} {med:>12.3} {sig:>10.3} {:>12.3}",
                med / sig
            );
        }
    }
    println!("\n  ratio rho' = in-band discrimination, the part a threshold can actually spend.");
    println!("  Model predicts rho' falls ~1/sqrt(2) from PN-110 to PN-220; spreading alone should not move it.");
}

// ── F1: does a DECODABLE fade frame ever score below the threshold? ───────────

#[test]
#[ignore = "verification"]
fn f1_fade_decode_cliff() {
    let mode = std::env::var("F1_MODE").unwrap_or_else(|_| "QPSK250-D".into());
    let snr: f32 = std::env::var("F1_SNR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7.0);
    let seeds: u64 = std::env::var("F1_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let thr = plugin_template(&mode).unwrap().1;
    let w = win_len(&mode);

    println!("\nF1: {mode} on moderate_f1 @ {snr} dB, threshold {thr:.2}, {seeds} seeds");
    let mut decodable_rhos: Vec<f32> = vec![];
    let mut below = 0usize;
    for seed in 0..seeds {
        // rho at the true onset, on the same realization the receiver will see
        let clean = qpsk_plugin::QpskPlugin::new()
            .modulate(b"fade cliff probe", &cfg(&mode))
            .unwrap();
        let mut c = WattersonConfig::moderate_f1(Some(seed));
        c.snr_db = snr;
        let faded = WattersonChannel::new(c).unwrap().apply(&clean);
        let r = match rho_engine(&mode, &faded[..w.min(faded.len())]) {
            Some(r) => r,
            None => continue,
        };

        let mut h = ChannelSimHarness::new();
        for eng in [&mut h.tx_engine, &mut h.rx_engine] {
            eng.register_plugin(Box::new(qpsk_plugin::QpskPlugin::new()))
                .unwrap();
        }
        h.tx_engine
            .transmit_with_fec_mode(b"fade cliff probe", &mode, FecMode::Rs, None)
            .expect("tx");
        let mut c2 = WattersonConfig::moderate_f1(Some(seed));
        c2.snr_db = snr;
        h.route(&mut WattersonChannel::new(c2).unwrap());
        let ok = h
            .rx_engine
            .receive_with_fec_mode_timeout(&mode, FecMode::Rs, None, Duration::from_millis(8_000))
            .is_ok();
        if ok {
            decodable_rhos.push(r);
            if r < thr {
                below += 1;
                println!("  DECODES at rho {r:.3} < threshold {thr:.2}  (seed {seed})");
            }
        }
    }
    decodable_rhos.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "  decodable frames: {} / {seeds}; min rho {:.3}; below threshold: {below}",
        decodable_rhos.len(),
        decodable_rhos.first().copied().unwrap_or(f32::NAN)
    );
    if decodable_rhos.len() > 4 {
        println!("  five lowest decodable rho: {:?}", &decodable_rhos[..5]);
    }
}

// ── F4: where does a steady tone actually defeat the BPSK veto? ───────────────

/// Fine tone sweep against the shipped BPSK250 template, grid and threshold.
///
/// The shipped `the_gate_is_not_fooled_by_a_steady_tone` samples 1250/1375/1500/1625/1750 — 125 Hz
/// steps. The preamble's lines sit at odd multiples of baud/4 (+/-62.5, +/-187.5, ...), so that
/// sweep lands every probe on an EVEN multiple, maximally far from every line. This sweeps finely
/// enough to hit them.
/// The four templates F3 compares, built once so F4 sweeps exactly what F3 measured.
fn f3_templates() -> Vec<(String, Vec<f32>, usize)> {
    let alt = plugin_template("BPSK250").expect("BPSK250 template").0;
    let pn31 = pn_template("BPSK250", &m_sequence(5, &[5, 3])).expect("pn31");
    let mut long = m_sequence(7, &[7, 6]);
    long.truncate(110);
    let pn110 = pn_template("BPSK1000", &long).expect("pn110");
    let alt110 = {
        let chips: Vec<f32> = (0..110)
            .map(|i| if (i / 2) % 2 == 0 { -1.0 } else { 1.0 })
            .collect();
        pn_template("BPSK1000", &chips).expect("alt110")
    };
    vec![
        ("alt250 (shipped)".into(), alt, 32),
        ("PN-31".into(), pn31, 32),
        ("PN-110".into(), pn110, 8),
        ("alt-110".into(), alt110, 8),
    ]
}

#[test]
#[ignore = "verification"]
fn f4_tone_sweep() {
    let (samples, threshold, grid_hz) = plugin_template("BPSK250").expect("BPSK250 template");
    let tlen = samples.len();
    let step = (0.25 * FS / tlen as f32).max(0.5);
    let n = (grid_hz / step).round() as i32;
    let grid: Vec<f32> = (-n..=n).map(|k| k as f32 * step).collect();
    let mf = IqMatchedFilter::new(samples);

    println!("\nF4: pure-tone rho vs shipped BPSK250 template (threshold {threshold:.2}, grid +/-{grid_hz} Hz)");
    let mut worst = (0.0f32, 0.0f32);
    let mut over = vec![];
    let mut f = 1_200.0f32;
    while f <= 1_800.0 {
        let tone: Vec<f32> = (0..tlen + 200)
            .map(|k| (2.0 * std::f32::consts::PI * f * k as f32 / FS).cos())
            .collect();
        if let Some((r, _)) = mf.search_normalized_over_frequency(&tone, 200, 0.05, FS, &grid) {
            if r.rho > worst.1 {
                worst = (f, r.rho);
            }
            if r.rho >= threshold {
                over.push((f, r.rho));
            }
            if (f / 12.5).round() * 12.5 == f {
                let flag = if r.rho >= threshold {
                    " <== DEFEATS VETO"
                } else {
                    ""
                };
                println!(
                    "  {f:8.1} Hz  (fc{:+7.1})  rho {:.3}{flag}",
                    f - 1_500.0,
                    r.rho
                );
            }
        }
        f += 2.5;
    }
    println!("\n  worst tone: {:.1} Hz at rho {:.3}", worst.0, worst.1);
    println!(
        "  tones at/over threshold: {} of the 2.5 Hz sweep 1200-1800",
        over.len()
    );
    if let (Some(lo), Some(hi)) = (over.first(), over.last()) {
        println!("  span: {:.1} .. {:.1} Hz", lo.0, hi.0);
    }
    println!("\n  the shipped test's own sweep points:");
    for f in [1_250.0f32, 1_375.0, 1_500.0, 1_625.0, 1_750.0] {
        let tone: Vec<f32> = (0..tlen + 200)
            .map(|k| (2.0 * std::f32::consts::PI * f * k as f32 / FS).cos())
            .collect();
        let (r, _) = mf
            .search_normalized_over_frequency(&tone, 200, 0.05, FS, &grid)
            .expect("search");
        println!("    {f:8.1} Hz  rho {:.3}", r.rho);
    }
}

/// Does a spread sequence actually fix the steady-tone hole F4 found?
///
/// A tone landing on one of the shipped preamble's two dominant lines captures ~half its energy
/// and scores rho ~= sqrt(0.5). A spread sequence should give a tone only ~1/N of its energy. This
/// is the measurement that decides whether the #1062 direction closes the vulnerability, as opposed
/// to merely moving it.
#[test]
#[ignore = "verification"]
fn f5_tone_vs_sequence() {
    println!("\nF5: worst pure tone in 1200-1800 Hz, per template (grid +/-20 Hz)");
    println!(
        "{:<20} {:>10} {:>12} {:>14} {:>16}",
        "template", "worst rho", "at Hz", "vs 0.40 thr", "own signal rho"
    );
    for (name, t, _sps) in f3_templates() {
        let mut worst = (0.0f32, 0.0f32);
        let mut f = 1_200.0f32;
        while f <= 1_800.0 {
            let tone: Vec<f32> = (0..t.len() + 200)
                .map(|k| (2.0 * std::f32::consts::PI * f * k as f32 / FS).cos())
                .collect();
            if let Some(r) = rho_of(&t, &tone, 20.0) {
                if r > worst.1 {
                    worst = (f, r);
                }
            }
            f += 2.5;
        }
        let sig = rho_of(
            &t,
            &t.iter()
                .copied()
                .chain(std::iter::repeat_n(0.0, 200))
                .collect::<Vec<_>>(),
            20.0,
        )
        .unwrap_or(f32::NAN);
        println!(
            "{name:<20} {:>10.3} {:>12.1} {:>14} {:>16.3}",
            worst.1,
            worst.0,
            if worst.1 >= 0.40 { "DEFEATS" } else { "ok" },
            sig
        );
    }
    println!("\n  reference: this receiver's best real on-air frame scores rho 0.654");
}

// ── F6: could a spread sequence REFUSE what the shipped one must corroborate? ─

/// The #1062 payoff, measured rather than argued.
///
/// `preamble_veto_interference::g5` shows the veto is protective exactly where it can refuse an
/// interferer, and useless where it cannot: a lone tone is refused and the frame survives 5/5,
/// while a sideband comb is corroborated and the frame dies 0/5 with or without the veto. What the
/// veto can refuse is set by the template's spectrum. So the question a sequence change has to
/// answer is not "is PN prettier" but: would a spread template score the comb and DSB shapes BELOW
/// threshold, where the shipped two-line template scores them above?
///
/// This needs no wire change to answer — only the templates and the interferer shapes.
#[test]
#[ignore = "verification"]
fn f6_would_a_spread_template_refuse_the_interferers() {
    let fc = 1_500.0f32;
    let n = 4_000;
    let mk = |shape: &str, amp: f32| -> Vec<f32> {
        (0..n)
            .map(|k| {
                let t = k as f32 / FS;
                match shape {
                    "tone +62.5" => amp * (2.0 * PI_F * (fc + 62.5) * t).cos(),
                    "AM 60 m=1" => {
                        amp * (1.0 + (2.0 * PI_F * 60.0 * t).cos()) * (2.0 * PI_F * fc * t).cos()
                    }
                    "DSB x60" => amp * (2.0 * PI_F * 60.0 * t).cos() * (2.0 * PI_F * fc * t).cos(),
                    "DSB x62.5" => {
                        amp * (2.0 * PI_F * 62.5 * t).cos() * (2.0 * PI_F * fc * t).cos()
                    }
                    _ => {
                        amp * (2.0 * PI_F * (fc - 60.0) * t).cos()
                            + 0.8 * amp * (2.0 * PI_F * (fc + 65.0) * t).cos()
                    }
                }
            })
            .collect()
    };

    let shapes = [
        "tone +62.5",
        "AM 60 m=1",
        "DSB x60",
        "DSB x62.5",
        "comb -60/+65",
    ];
    println!(
        "\nF6: COMPONENT-LEVEL peak rho, template vs interferer (grid +/-20 Hz, CENTRED AT 0)"
    );
    println!("    This is NOT the deployed response, and the LONE TONE row is where they diverge.");
    println!(
        "    The engine centres this grid on the AFC settle; for a lone tone the settle lands"
    );
    println!("    on the tone, parking it ~baud/4 from both rotated lines, so the deployed chain");
    println!("    REFUSES the tone this table scores at 0.701 (measured: g5, 5/5 decodes with the");
    println!("    veto on). The SIDEBAND rows carry over unchanged, because their settle lands at");
    println!("    ~0 Hz -- g2a reads 0.0 Hz correction on every one -- so for those the grid the");
    println!("    engine actually builds is centred where this table centres it.\n");
    print!("{:<16}", "interferer");
    for (name, _, _) in f3_templates() {
        print!("{name:>20}");
    }
    println!();
    for shape in shapes {
        let sig = mk(shape, 0.3);
        print!("{shape:<16}");
        for (_, t, _) in f3_templates() {
            let r = rho_of(&t, &sig, 20.0).unwrap_or(f32::NAN);
            print!("{:>17.3}{}", r, if r >= 0.40 { " !" } else { "  " });
        }
        println!();
    }
    println!(
        "\n  ! = at or above threshold, i.e. the veto would CORROBORATE this interferer and the"
    );
    println!(
        "  receiver would anchor on it. Note the grid is centred at 0 here; for the lone tone"
    );
    println!("  the deployed chain settles onto the tone first, which is a separate protection.");
}

// ── F8: does a FADED frame's rho tail clear the noise ceiling? (#1062, #1059) ──

/// A template embedded in a real transmission, as the receiver would meet it.
///
/// The shipped template already sits at the head of `bpsk_modulate`'s output, so that case is the
/// unmodified modulator. A candidate template is prepended to a real modulated frame instead, so
/// the correlation window has genuine signal after the preamble rather than padding — the same
/// construction `peak_sidelobe` uses, and for the same reason: `search_normalized` divides by the
/// window's energy, so padding manufactures a score.
fn frame_carrying(template: &[f32], mode: &str, shipped: bool) -> Vec<f32> {
    let payload: Vec<u8> = (0..200u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let data = bpsk_plugin::BpskPlugin::new()
        .modulate(&payload, &cfg(mode))
        .expect("modulate payload");
    if shipped {
        data
    } else {
        template.iter().chain(data.iter()).copied().collect()
    }
}

/// The quantity every threshold claim in #1062/#1059/#1060 has been missing: what ρ does a real
/// frame score **through a fade**, per candidate template, at the low end of its distribution.
///
/// A threshold has to sit above the noise ceiling and below the weakest frame that must still be
/// detected. Two earlier attempts to construct that window used the *best* on-record frame
/// (ρ = 0.654) scaled multiplicatively, which is refuted by data already in the repo:
/// `plugins/bpsk/src/modulate.rs` records `moderate_f1` frames at ρ = 0.58–0.84, i.e. above the
/// ceiling that proxy builds. The design-relevant number is the **low tail**, not the best case.
///
/// Cross-check on the harness, and the reason the shipped template is included as a case rather
/// than assumed: it must reproduce that recorded 0.58–0.84 band. If it does not, the apparatus is
/// wrong and no candidate column from the same run means anything.
///
/// Note what this can and cannot say. Decodability is a property of the payload path, which is
/// identical across these cases — swapping the preamble does not change whether the data decodes.
/// So this measures the *detection* side only, which is precisely #1062's stated open question for
/// #1059: whether the ρ tail of a real faded frame clears the ceiling its own template sets.
#[test]
#[ignore = "verification"]
fn f8_faded_frame_rho_tail() {
    let alt = plugin_template("BPSK250").expect("BPSK250 template").0;
    let mut m110 = m_sequence(7, &[7, 6]);
    m110.truncate(110);
    let pn110 = pn_template("BPSK1000", &m110).expect("pn110");
    let mut m220 = m_sequence(8, &[8, 6, 5, 4]);
    m220.truncate(220);
    let pn220 = pn_template("BPSK1000", &m220).expect("pn220");
    // The two controls f3 built and an earlier version of this test dropped: PN-31 holds bandwidth
    // fixed while changing the sequence, alt-110 holds the sequence periodic while changing
    // bandwidth. Without both, "spreading did it" cannot be separated from "chip rate did it".
    let pn31 = pn_template("BPSK250", &m_sequence(5, &[5, 3])).expect("pn31");
    let alt110: Vec<f32> = {
        let chips: Vec<f32> = (0..110)
            .map(|i| if (i / 2) % 2 == 0 { -1.0 } else { 1.0 })
            .collect();
        pn_template("BPSK1000", &chips).expect("alt110")
    };

    let cases: [(&str, &[f32], &str, bool); 5] = [
        ("shipped alt  (250, 124 ms)", &alt, "BPSK250", true),
        ("PN-31        (250, 120 ms)", &pn31, "BPSK250", false),
        ("alt-110      (1k,  109 ms)", &alt110, "BPSK1000", false),
        ("PN-110       (1k,  109 ms)", &pn110, "BPSK1000", false),
        ("PN-220       (1k,  219 ms)", &pn220, "BPSK1000", false),
    ];
    let seeds: u64 = std::env::var("F8_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);

    println!("\nF8: rho of a REAL faded frame at the true onset, {seeds} seeds/cell");
    println!("  The frame is band-limited by the SAME mask as the noise ceiling it is compared");
    println!(
        "  against. An earlier version compared an UNFILTERED tail against a FILTERED ceiling:"
    );
    println!("  a station running a 500 Hz filter filters the frame too, which for an in-band");
    println!("  template removes most of the window's noise and lifts the tail by ~9 dB of SNR.");
    println!("  Reported as a MISS RATE at candidate thresholds, not as a min: one side of a");
    println!("  min-vs-max gap is an extreme value whose size is set by the observation budget.");
    for (label, t, mode, shipped) in cases {
        println!("\n{label}");
        println!(
            "  {:<16} {:>5} {:>7} {:>7} {:>7} {:>26}",
            "band", "snr", "min", "p5", "median", "miss rate at theta"
        );
        println!(
            "  {:<16} {:>5} {:>7} {:>7} {:>7} {:>26}",
            "", "", "", "", "", "0.30   0.40   0.50"
        );
        for (bname, lo, hi) in [
            ("unfiltered", 0.0f32, 4_000.0),
            ("ssb 300-2700", 300.0, 2_700.0),
            ("filter 1250-1750", 1_250.0, 1_750.0),
        ] {
            for snr in [10.0f32, 20.0] {
                let clean = frame_carrying(t, mode, shipped);
                let mut rhos: Vec<f32> = Vec::new();
                for seed in 0..seeds {
                    let mut c = WattersonConfig::moderate_f1(Some(seed));
                    c.snr_db = snr;
                    let faded = WattersonChannel::new(c).expect("channel").apply(&clean);
                    let limited = band_limit(&faded, lo, hi);
                    let w = (t.len() + F7_LAG_BOUND).min(limited.len());
                    if let Some(r) = rho_of(t, &limited[..w], 20.0) {
                        rhos.push(r);
                    }
                }
                if rhos.is_empty() {
                    continue;
                }
                rhos.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let n = rhos.len() as f32;
                let miss = |th: f32| rhos.iter().filter(|&&r| r < th).count() as f32 / n;
                println!(
                    "  {bname:<16} {snr:>5.0} {:>7.3} {:>7.3} {:>7.3} {:>7.2} {:>6.2} {:>6.2}",
                    rhos[0],
                    rhos[(n * 0.05) as usize],
                    rhos[rhos.len() / 2],
                    miss(0.30),
                    miss(0.40),
                    miss(0.50)
                );
            }
        }
    }
    println!(
        "\n  Miss rate = fraction of real faded frames a threshold would veto. Compare against"
    );
    println!("  the same band's noise ceiling from f7 to see whether any theta separates them.");
    println!("  NOT decode-conditioned: a frame inside a multi-second good_f1 null scores low and");
    println!("  would not have decoded either, so an unconditioned tail overstates the miss rate.");
}

// ── F9: the tail that matters — rho of frames that DECODED (#1060, #1059) ─────

/// Watterson followed by the same brick-wall mask the noise ceiling is measured through, as one
/// `ChannelModel`.
///
/// Composing them here rather than filtering afterwards is what lets ρ and the decode verdict come
/// from **bit-identical samples**: `route_tapped` returns the very buffer it hands the receiver.
/// Measuring ρ on one realisation and decoding another seeded the same way is not the same
/// experiment.
///
/// **CORRECTED 2026-09-07: the reason given here was wrong, and it would have misled the next
/// design — it nearly misled mine.** It said "the two signals differ in length, so the fade they
/// see differs". For `moderate_f1` that is false within a power-of-two bracket: `continuous: false`
/// takes the one-shot path, and `doppler_envelope` draws `fft_size` complex Gaussians where
/// `fft_size` is a power of two of `ceil(n/160)+2`, so two lengths in the same bracket consume the
/// same draws in the same order and see an **identical** envelope from index 0. At 8 kHz / 1 Hz the
/// 512-bin bracket holds n ≤ 81 600 (10.2 s); a 200-B RS frame is 66 560. The ledger already carried
/// the evidence: `f8` (uncoded, 52 480 samples) and the veto-off `f9` (RS, 66 560) both report min
/// **0.618** in the 1250–1750 cell — two different waveforms, same seeds, same preamble-window fade.
///
/// Bit-identical samples remain the right discipline, for the reasons that actually hold: the
/// Hilbert transform is a whole-buffer FFT so the outputs differ everywhere once the inputs do, and
/// `noise_sigma` is scaled by whole-buffer RMS, so padding one arm silently moves its SNR.
struct FadeThenFilter {
    inner: WattersonChannel,
    lo: f32,
    hi: f32,
}

impl ChannelModel for FadeThenFilter {
    fn apply(&mut self, input: &[f32]) -> Vec<f32> {
        let faded = self.inner.apply(input);
        band_limit(&faded, self.lo, self.hi)
    }
    fn generate_noise(&mut self, length: usize) -> Vec<f32> {
        vec![0.0; length]
    }
}

/// `BpskPlugin` with the preamble veto switched OFF — the arm #1088 named as the fix for f9's
/// circularity. (STALE as written: the arm now EXISTS — `NoVetoBpsk` below — and `modulate.rs`
/// records that the shipped bound was derived with it. What remains impossible is a
/// decode-conditioned run at 64 symbols, since no 64-symbol receiver exists: `PREAMBLE_SYMS` is
/// consumed at six-plus receiver sites.)
///
/// f9's decoded-only column is a tautology while the shipped 0.40 veto runs inside the decode: a
/// frame scoring under 0.40 is vetoed, fails, and leaves the conditioned set, so the miss rate at
/// 0.40 among decoded frames is zero by construction. Its own tripwire proved that is live rather
/// than theoretical (1010-1696 settle rejections per 30 seeds, none of which decoded). Returning
/// `None` here makes `build_preamble_veto` yield `None`, so the decode verdict is the channel's
/// alone and "would this threshold discard a delivered frame?" becomes answerable.
struct NoVetoBpsk(bpsk_plugin::BpskPlugin);

impl ModulationPlugin for NoVetoBpsk {
    fn info(&self) -> &PluginInfo {
        self.0.info()
    }
    fn modulate(&self, d: &[u8], c: &ModulationConfig) -> Result<Vec<f32>, ModemError> {
        self.0.modulate(d, c)
    }
    fn demodulate(&self, s: &[f32], c: &ModulationConfig) -> Result<Vec<u8>, ModemError> {
        self.0.demodulate(s, c)
    }
    fn demodulate_soft(&self, s: &[f32], c: &ModulationConfig) -> Result<Vec<f32>, ModemError> {
        self.0.demodulate_soft(s, c)
    }
    fn frame_geometry(&self, c: &ModulationConfig) -> Option<FrameGeometry> {
        self.0.frame_geometry(c)
    }
    fn estimate_snr_db(&self, s: &[f32], c: &ModulationConfig) -> Option<f32> {
        self.0.estimate_snr_db(s, c)
    }
    fn supports_soft_demod(&self, m: &str) -> bool {
        self.0.supports_soft_demod(m)
    }
    fn estimate_afc_hz(&self, s: &[f32], c: &ModulationConfig) -> Option<f32> {
        self.0.estimate_afc_hz(s, c)
    }
    fn occupied_bandwidth_hz(&self, m: &str) -> Option<f32> {
        self.0.occupied_bandwidth_hz(m)
    }
    fn modulate_iq(
        &self,
        d: &[u8],
        c: &ModulationConfig,
    ) -> Result<(Vec<f32>, Vec<f32>), ModemError> {
        self.0.modulate_iq(d, c)
    }
    fn preamble_template(&self, _config: &ModulationConfig) -> Option<PreambleTemplate> {
        None
    }
}

/// The quantity #1060 actually turns on: among frames that **decode**, how many would a candidate
/// threshold veto?
///
/// Every earlier version of this measurement reported the tail of *all* faded frames. That
/// overstates the cost of a threshold, because a frame sitting inside a multi-second fade null
/// scores low and does not decode either — vetoing it forfeits nothing. The shipped threshold's own
/// invariant is stated in those terms (`plugins/bpsk/src/modulate.rs`): the gate must not reject a
/// decodable frame before the channel already has.
///
/// Scoped to the shipped BPSK250 template deliberately. There the template *is* the frame's
/// preamble, so ρ and decodability are properties of one object and conditioning is meaningful. A
/// prepended candidate template would be extra audio in front of a frame carrying its own preamble,
/// and conditioning on that frame's decode would answer a different question. This is also where
/// the actionable claim lives — whether the shipped 0.40 is right.
#[test]
#[ignore = "verification"]
fn f9_decode_conditioned_rho_tail() {
    let t = plugin_template("BPSK250").expect("BPSK250 template").0;
    let seeds: u64 = std::env::var("F9_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let payload: Vec<u8> = (0..200u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();

    println!("\nF9: BPSK250+Rs, rho vs DECODE on identical samples, {seeds} seeds/cell");
    println!(
        "  {:<16} {:>5} {:>7} {:>9} {:>25} {:>25}",
        "band", "snr", "decode", "min rho", "miss rate, ALL frames", "miss rate, DECODED only"
    );
    println!(
        "  {:<16} {:>5} {:>7} {:>9} {:>25} {:>25}",
        "", "", "rate", "decoded", "0.30   0.40   0.50", "0.30   0.40   0.50"
    );
    // Each cell costs ~12-15 min because a frame that fails to decode burns the full receive
    // timeout, so the bands are selectable: a single run of all nine cells exceeds any sane
    // wall-clock bound and gets truncated mid-table, which is worse than not running it.
    let want = std::env::var("F9_BAND").unwrap_or_else(|_| "all".into());
    let veto_off = std::env::var("F9_VETO")
        .map(|v| v == "off")
        .unwrap_or(false);
    println!(
        "  receiver veto: {}",
        if veto_off {
            "OFF (F9_VETO=off) — the decoded-only column is sound"
        } else {
            "ON (shipped) — the decoded-only column is CIRCULAR, see the tripwire per cell"
        }
    );
    for (bname, lo, hi) in [
        ("unfiltered", 0.0f32, 4_000.0),
        ("ssb 300-2700", 300.0, 2_700.0),
        ("filter 1250-1750", 1_250.0, 1_750.0),
        // The 250 Hz-class band. It is the cell that decides the stand-down question: at this width
        // the measured noise ceiling (real rig, 45 s: p99 0.492, max 0.579) climbs into the region
        // where a frame tail might no longer clear it, and a veto with no separation must stand down
        // rather than reject everything.
        ("filter 1400-1600", 1_400.0, 1_600.0),
    ] {
        if want != "all" && !bname.starts_with(&want) {
            continue;
        }
        for snr in [5.0f32, 10.0, 20.0] {
            let mut all: Vec<f32> = Vec::new();
            let mut decoded: Vec<f32> = Vec::new();
            let (mut veto_rejections, mut veto_seeds_decoded, mut veto_seeds_failed) =
                (0u64, 0u32, 0u32);
            for seed in 0..seeds {
                let mut h = ChannelSimHarness::new();
                h.tx_engine
                    .register_plugin(Box::new(bpsk_plugin::BpskPlugin::new()))
                    .expect("register tx");
                // F9_VETO=off registers the veto-disabled wrapper on the RECEIVER only, so the
                // decoded set is chosen by the channel rather than partly by the subject of the
                // measurement. The transmitter is untouched: the wire is identical either way.
                if veto_off {
                    h.rx_engine
                        .register_plugin(Box::new(NoVetoBpsk(bpsk_plugin::BpskPlugin::new())))
                        .expect("register rx (no veto)");
                } else {
                    h.rx_engine
                        .register_plugin(Box::new(bpsk_plugin::BpskPlugin::new()))
                        .expect("register rx");
                }
                // #1066: the receive scan's budget is WALL CLOCK unless these are set, and the
                // determinism is opt-in. Left unset, a loaded machine truncates the retry passes —
                // which preferentially loses the low-rho frames whose absence this test then
                // reports as "the tail is empty". F9_DETERMINISTIC=0 restores shipped behaviour so
                // the two can be compared.
                // The budget is swept rather than chosen: picking one and reporting its verdict
                // makes the constant the answer. An earlier version hardcoded 64/4000 and turned a
                // ~10 min cell into one that had not finished in 2 h, which is itself evidence the
                // shipped wall-clock path bounds the work far tighter than that.
                let pos: usize = std::env::var("F9_POS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let iters: usize = std::env::var("F9_ITERS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                if pos > 0 {
                    h.rx_engine.set_deterministic_scan_positions(Some(pos));
                }
                if iters > 0 {
                    h.rx_engine.set_deterministic_max_iterations(Some(iters));
                }
                if h.tx_engine
                    .transmit_with_fec_mode(&payload, "BPSK250", FecMode::Rs, None)
                    .is_err()
                {
                    continue;
                }
                let mut c = WattersonConfig::moderate_f1(Some(seed));
                c.snr_db = snr;
                let mut chan = FadeThenFilter {
                    inner: WattersonChannel::new(c).expect("channel"),
                    lo,
                    hi,
                };
                let (_, rx) = h.route_tapped(&mut chan);
                let w = (t.len() + F7_LAG_BOUND).min(rx.len());
                let Some(r) = rho_of(&t, &rx[..w], 20.0) else {
                    continue;
                };
                all.push(r);
                let ok = h
                    .rx_engine
                    .receive_with_fec_mode_timeout(
                        "BPSK250",
                        FecMode::Rs,
                        None,
                        Duration::from_millis(8_000),
                    )
                    .is_ok();
                // The shipped 0.40 veto runs INSIDE this decode. If it rejected any settle here,
                // then "frames that decoded" is a set this test's own subject helped choose, and a
                // miss rate at 0.40 measured over that set is circular. Counting the rejections is
                // what distinguishes "the veto accepted everything" from "the veto shaped the set".
                let vetoed = h.rx_engine.rho_rejected_settles();
                veto_rejections += vetoed;
                if vetoed > 0 {
                    if ok {
                        veto_seeds_decoded += 1;
                    } else {
                        veto_seeds_failed += 1;
                    }
                }
                if ok {
                    decoded.push(r);
                }
            }
            if all.is_empty() {
                continue;
            }
            let rate = |v: &Vec<f32>, th: f32| {
                if v.is_empty() {
                    f32::NAN
                } else {
                    v.iter().filter(|&&r| r < th).count() as f32 / v.len() as f32
                }
            };
            let min_dec = decoded.iter().cloned().fold(f32::INFINITY, f32::min);
            println!(
                "  {bname:<16} {snr:>5.0} {:>7.2} {:>9.3} {:>8.2} {:>6.2} {:>6.2} {:>10.2} {:>6.2} {:>6.2}",
                decoded.len() as f32 / all.len() as f32,
                if decoded.is_empty() { f32::NAN } else { min_dec },
                rate(&all, 0.30),
                rate(&all, 0.40),
                rate(&all, 0.50),
                rate(&decoded, 0.30),
                rate(&decoded, 0.40),
                rate(&decoded, 0.50),
            );
            println!(
                "      veto: {veto_rejections} settle rejections over {} seeds; seeds with a \
                 rejection that still decoded: {veto_seeds_decoded}, that failed: \
                 {veto_seeds_failed}",
                all.len()
            );
        }
    }
    println!("\n  'miss rate, DECODED only' is the design quantity: frames the channel delivered");
    println!("  that a threshold would throw away. Compare against f7's noise ceiling per band");
    println!("  (500 Hz: 0.443) — a theta is usable only if it sits above the ceiling AND has a");
    println!("  miss rate of ~0 in this column.");
}

// ── F10: how fast does the rho ceiling fall as the filter SKIRT softens? (#1060) ──

/// Band-limit with a raised-cosine skirt of a given **shape factor**, zero phase.
///
/// Shape factor is the −60 dB : −6 dB bandwidth ratio, which is how receiver selectivity is
/// specified on rig datasheets — so a curve swept on this axis can be compared directly against a
/// hardware-filtered recording by fitting that recording's effective shape factor, rather than
/// being a separate un-relatable fact. `sf = 1.0` is a brick wall and reproduces [`band_limit`].
///
/// Zero phase is deliberate, not a convenience. For the noise ceiling only the **magnitude**
/// response can matter: a stationary Gaussian process is characterised by its power spectrum, and
/// filtering gives `|H(f)|²·S(f)` — the phase term cancels exactly. So an IIR with the same
/// magnitude response would produce the same ρ distribution while adding non-linear group delay
/// and an alignment problem for no gain. (The exception is non-Gaussian content — birdies and
/// impulses — where phase does affect the waveform; tonal birdies stay magnitude-dominated, which
/// is why the real captures are swept here alongside synthetic noise rather than instead of it.)
fn skirted_band_limit(x: &[f32], lo_hz: f32, hi_hz: f32, shape_factor: f32) -> Vec<f32> {
    use rustfft::{num_complex::Complex, FftPlanner};
    let n = x.len().next_power_of_two();
    let mut buf: Vec<Complex<f32>> = x
        .iter()
        .map(|&v| Complex::new(v, 0.0))
        .chain(std::iter::repeat_n(Complex::new(0.0, 0.0), n - x.len()))
        .collect();
    let mut planner = FftPlanner::new();
    planner.plan_fft_forward(n).process(&mut buf);
    let bin_hz = FS / n as f32;
    // -6 dB bandwidth is (hi-lo); the -60 dB bandwidth adds one transition on each side, so
    // sf = (bw + 2w)/bw gives w = (sf-1)*bw/2.
    let w = ((shape_factor - 1.0).max(0.0)) * (hi_hz - lo_hz) / 2.0;
    for (k, v) in buf.iter_mut().enumerate() {
        let f = if k <= n / 2 {
            k as f32 * bin_hz
        } else {
            (n - k) as f32 * bin_hz
        };
        // Raised-cosine transition on each edge; real and symmetric, hence zero phase.
        let g = if f >= lo_hz && f <= hi_hz {
            1.0
        } else if w <= 0.0 {
            0.0
        } else if f < lo_hz && f >= lo_hz - w {
            0.5 * (1.0 + (std::f32::consts::PI * (lo_hz - f) / w).cos())
        } else if f > hi_hz && f <= hi_hz + w {
            0.5 * (1.0 + (std::f32::consts::PI * (f - hi_hz) / w).cos())
        } else {
            0.0
        };
        *v *= g;
    }
    planner.plan_fft_inverse(n).process(&mut buf);
    let scale = 1.0 / n as f32;
    buf.iter().map(|c| c.re * scale).collect()
}

/// Peak ρ of the shipped BPSK250 template over a noise buffer, engine window and grid.
fn peak_rho_over(noise: &[f32], template: &[f32]) -> f32 {
    let w = template.len() + F7_LAG_BOUND;
    let mut peak = 0.0f32;
    let mut s = 0usize;
    while s + w <= noise.len() {
        if let Some(r) = rho_of(template, &noise[s..s + w], 20.0) {
            peak = peak.max(r);
        }
        s += F7_LAG_BOUND;
    }
    peak
}

/// #1060's open question is whether a REAL 500 Hz filter lifts idle ρ above the shipped 0.40. The
/// only figure we have is 0.441 from a **brick wall**, which is sharper than any real filter — so
/// the answer depends entirely on how fast ρ falls as the skirt softens.
///
/// Sweeping the skirt converts an unknown point value into a bounded curve, and the two ends are
/// each decisive: if ρ stays above 0.40 across every plausible shape factor, the defect is real and
/// the hardware capture becomes confirmation; if ρ drops under 0.40 as soon as the edge softens at
/// all, the brick wall *was* the effect and #1060 is a documented limit rather than a defect.
///
/// Real captures are swept beside synthetic noise so the two can be compared like-for-like at the
/// same length. **The absolute values here are NOT comparable to f7's 0.441**: ρ_ceiling is a peak
/// statistic and the corpus idle captures are 3.00 s against f7's 45 s, which biases the peak low
/// by roughly √(ln N) — about 20 %. The *shape* of the curve is what this measures; the crossing
/// point needs the longer recordings. Cycling a 3 s capture would not fix that — the extra windows
/// repeat the same noise and the peak simply saturates.
#[test]
#[ignore = "verification"]
fn f10_skirt_sweep() {
    let t = plugin_template("BPSK250").expect("BPSK250 template").0;
    let (lo, hi) = (1_250.0f32, 1_750.0f32);
    let corpus = ["ic9700-idle-hot.wav", "ft991a-idle.wav"];

    println!("\nF10: peak rho vs filter shape factor, 500 Hz passband, BPSK250 template");
    println!("  shipped threshold 0.40; f7's brick-wall 45 s figure was 0.441");
    println!(
        "  {:>6} {:>12} {:>14} {:>14} {:>14}",
        "sf", "transition", "ic9700-idle", "ft991a-idle", "synthetic 3s"
    );

    let loaded: Vec<(&str, Vec<f32>)> = corpus
        .iter()
        .filter_map(|n| {
            openpulse_modem::capture_replay::load_corpus(n)
                .ok()
                .map(|c| (*n, c.samples))
        })
        .collect();
    assert_eq!(
        loaded.len(),
        corpus.len(),
        "corpus captures must load; a missing one would silently drop a column"
    );
    // Same length as the real captures, so the peak statistics are comparable.
    let synth = band_noise(24_000, 300.0, 2_700.0, 12345);

    for sf in [1.0f32, 1.1, 1.25, 1.5, 2.0, 3.0, 4.0] {
        let w = ((sf - 1.0).max(0.0)) * (hi - lo) / 2.0;
        let mut cells: Vec<String> = Vec::new();
        for (_, s) in &loaded {
            cells.push(format!(
                "{:.3}",
                peak_rho_over(&skirted_band_limit(s, lo, hi, sf), &t)
            ));
        }
        cells.push(format!(
            "{:.3}",
            peak_rho_over(&skirted_band_limit(&synth, lo, hi, sf), &t)
        ));
        println!(
            "  {sf:>6.2} {:>10.0} Hz {:>14} {:>14} {:>14}",
            w, cells[0], cells[1], cells[2]
        );
    }
    println!("\n  sf = 1.0 is a brick wall. Real DSP filters are typically sharp (low sf); an");
    println!("  analog crystal or ceramic filter is softer. When the hardware-filtered recordings");
    println!("  arrive, fit their effective shape factor and read the curve at that point — that");
    println!("  is what makes the model and the hardware the same axis instead of two facts.");
}

// ── F11: does spectral subtraction help frame DETECTION? (side-task, 2026-09-07) ──────────────
//
// The maintainer asked whether spectral-subtraction noise reduction could improve acquisition and
// data extraction. The assessment declined it for the demod path on mechanism — under white Gaussian
// noise the matched filter already computes the sufficient statistic and a half-wave-rectified
// per-bin gain is non-invertible, so by data processing it cannot improve detection or SER there;
// and for the amplitude-bearing rungs it is a fast bin-wise AGC, which this repo has already
// measured flipping SCFDMA52-16QAM from 2/2 pass to 2/2 fail.
//
// The DETECTOR path was left open, because a real counter-mechanism exists: a weak preamble's few
// spectral lines might survive while off-line noise is suppressed. This probe settles it.
//
// **The metric is ρ′ = ρ_noise / ρ_signal, never ρ_noise alone.** Three measurements in this repo
// show that removing energy raises ρ on a noise-only capture — the DDC lowpass "raises ρ for signal
// and noise alike, the noise by more" (`acquisition.rs`), a narrower rig filter moves idle ρ
// 0.227 → 0.413 → 0.579 (#1060), and notching a birdie out of an idle capture RAISED ρ. ρ is a
// ratio, so shrinking the denominator of a noise-only window flatters it. Anything that quotes an
// absolute ρ improvement here is measuring the wrong thing, and the CFAR calibration (#1157)
// absorbs a shift in ρ_noise anyway — only ρ′ can matter.
//
// Research harness: `#[ignore]`, asserts nothing, prints a table. It shares `rho_engine`,
// `win_len` and `plugin_template` with every row above by reference, so its numbers are directly
// comparable rather than produced by a re-transcribed correlator.

/// Textbook magnitude spectral subtraction over an STFT, resynthesised by overlap-add.
///
/// `alpha` = over-subtraction factor, `beta` = spectral floor. Noise PSD is the per-bin 25th
/// percentile across frames — the same estimator `noise_floor.rs` uses across bins, applied per bin,
/// which is precisely the input the shipped scalar floor cannot provide.
fn spectral_subtract(samples: &[f32], alpha: f32, beta: f32) -> Vec<f32> {
    use rustfft::{num_complex::Complex32, FftPlanner};
    const N: usize = 512;
    const HOP: usize = N / 2;
    if samples.len() < N {
        return samples.to_vec();
    }
    let win: Vec<f32> = (0..N)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / N as f32).cos())
        .collect();
    let mut planner = FftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(N);
    let inv = planner.plan_fft_inverse(N);

    // Pass 1: per-bin magnitude history.
    let frames: Vec<Vec<Complex32>> = (0..)
        .map(|k| k * HOP)
        .take_while(|&s| s + N <= samples.len())
        .map(|s| {
            let mut buf: Vec<Complex32> = (0..N)
                .map(|i| Complex32::new(samples[s + i] * win[i], 0.0))
                .collect();
            fwd.process(&mut buf);
            buf
        })
        .collect();
    let mut noise_mag = vec![0.0f32; N];
    for (bin, nm) in noise_mag.iter_mut().enumerate() {
        let mut col: Vec<f32> = frames.iter().map(|f| f[bin].norm()).collect();
        col.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        *nm = col[col.len() / 4]; // 25th percentile
    }

    // Pass 2: subtract, floor, resynthesise.
    let mut out = vec![0.0f32; samples.len()];
    let mut wsum = vec![0.0f32; samples.len()];
    for (k, f) in frames.iter().enumerate() {
        let s = k * HOP;
        let mut buf: Vec<Complex32> = f
            .iter()
            .enumerate()
            .map(|(bin, c)| {
                let mag = c.norm();
                let kept = (mag - alpha * noise_mag[bin]).max(beta * mag);
                if mag > 1e-12 {
                    c * (kept / mag)
                } else {
                    *c
                }
            })
            .collect();
        inv.process(&mut buf);
        for i in 0..N {
            out[s + i] += buf[i].re / N as f32 * win[i];
            wsum[s + i] += win[i] * win[i];
        }
    }
    for (o, w) in out.iter_mut().zip(wsum.iter()) {
        if *w > 1e-6 {
            *o /= *w;
        }
    }
    out
}

/// **RESULT, measured 2026-09-07 — spectral subtraction makes detection WORSE. 12 of 12 cells.**
///
/// ```text
/// capture                     baseline rho'   best rho' after   delta
/// ic9700-idle-wide-500hz       0.241           0.269            +0.027
/// ic9700-idle-500hz            0.438           0.498            +0.059
/// ic9700-idle-250hz            0.615           0.681            +0.067
/// ```
///
/// Both kill criteria fire on every capture and every (alpha, beta): rho' RISES, and peak rho_noise
/// rises with it (0.413 -> 0.524 at 500 Hz; 0.579 -> 0.648 at 250 Hz). More over-subtraction is
/// monotonically worse (alpha=2 beats alpha=1 nowhere).
///
/// **The mechanism is in the numbers.** `rho_signal` barely moves (0.941 -> 0.936 at worst), so the
/// entire effect is on the noise side. Half-wave rectification keeps the exponential tail and
/// sparsifies broadband noise into tone-like survivors; when one lands on the alternating preamble's
/// few spectral lines, rho jumps — the same reason a lone tone already scores rho ~0.70 against this
/// template (#1062). Subtraction MANUFACTURES tones out of noise, which is precisely the input this
/// detector is worst against.
///
/// **Scope of the elimination — do not over-read it.** What is refuted is *magnitude spectral
/// subtraction as a pre-detector stage for the alternating BPSK preamble, on these three rig
/// captures*. It is NOT a refutation of: noise reduction in general; a *linear* pre-whitening filter
/// (which preserves Gaussianity and would leave rho' unchanged under WGN by construction, so it is
/// uninteresting for a different reason); a time-domain blanker for impulsive QRN, which this probe
/// says nothing about; or WSJT-X-style successive cancellation of already-DECODED signals, which is
/// an unrelated technique that happens to share the word "subtraction".
///
/// The demod path was declined on mechanism without measurement and stays declined: a per-bin
/// per-frame gain is a fast bin-wise AGC, and a milder version of that already flipped
/// SCFDMA52-16QAM from 2/2 pass to 2/2 fail (CLAUDE.md, "A rig setting you did not verify").
///
/// Consistent with the operator rule already in the tree: `onair-signal-chain-verification.md` sets
/// rig DSP NR **off** ("distorts BPSK signal") and `run-onair-ic9700-ft991a.sh` clears NR/NB by CAT
/// before every run. Same algorithm family; this puts a number on it.
#[test]
#[ignore = "research harness (side-task 2026-09-07): asserts nothing, prints the rho' table. \
            RESULT: rho' rises on all 3 captures x 4 params — spectral subtraction makes frame \
            detection worse. Kept so the elimination travels with its apparatus."]
fn f11_does_spectral_subtraction_lower_rho_prime() {
    const MODE: &str = "BPSK250";
    let params = [(1.0f32, 0.10f32), (1.0, 0.01), (2.0, 0.10), (2.0, 0.01)];

    println!("\n=== F11: spectral subtraction, detector path ===");
    println!("metric is rho' = rho_noise / rho_signal; lower is better. rho_noise ALONE is not the metric.\n");

    // ρ_signal: a real on-air frame capture.
    let signal = match load_corpus("ic9700-frame-bpsk250-rs-whitened.wav") {
        Ok(c) => c.samples,
        Err(e) => {
            println!("  signal capture unavailable ({e}); cannot form rho' — aborting");
            return;
        }
    };

    for noise_name in [
        "ic9700-idle-wide-500hz-control.wav",
        "ic9700-idle-500hz.wav",
        "ic9700-idle-250hz.wav",
    ] {
        let noise = match load_corpus(noise_name) {
            Ok(c) => c.samples,
            Err(e) => {
                println!("  {noise_name}: unavailable ({e})");
                continue;
            }
        };
        let base_n = peak_rho_over_capture(MODE, &noise);
        let base_s = peak_rho_over_capture(MODE, &signal);
        let (Some(bn), Some(bs)) = (base_n, base_s) else {
            println!("  {noise_name}: no rho (no template?)");
            continue;
        };
        println!("  {noise_name}");
        println!(
            "    baseline          rho_noise={bn:.3}  rho_signal={bs:.3}  rho'={:.3}",
            bn / bs
        );
        for (alpha, beta) in params {
            let sn = peak_rho_over_capture(MODE, &spectral_subtract(&noise, alpha, beta));
            let ss = peak_rho_over_capture(MODE, &spectral_subtract(&signal, alpha, beta));
            match (sn, ss) {
                (Some(n), Some(s)) => println!(
                    "    a={alpha:.0} b={beta:.2}       rho_noise={n:.3}  rho_signal={s:.3}  \
                     rho'={:.3}   (delta rho' {:+.3})",
                    n / s,
                    n / s - bn / bs
                ),
                _ => println!("    a={alpha:.0} b={beta:.2}       no rho"),
            }
        }
    }
    println!("\nKill criterion: rho' not lower than baseline, or peak rho_noise RISES.");
    println!(
        "Keep criterion: rho' falls by ~0.7x — what doubling the preamble duration buys (f7).\n"
    );
}

// ── F12: does LENGTH help the ALTERNATING preamble? (#1062's untested premise) ─────────────────
//
// **STATUS 2026-09-07: the first run of this probe MEASURED THE WRONG SEQUENCE and its numbers are
// withdrawn.** It fed `pn_template` a hand-written `+1,-1,+1,…` chip run described in a comment as
// "the shipped sync word's structure". The shipped preamble is alternating **bits**; NRZI flips
// phase only on a `1`, so the symbols are `--++` — period four. Correlation between what it measured
// and what the modem transmits: **0.035**. Its lines sat at fc ± baud/2, not fc ± baud/4, which is
// also why its "3 % retention through a 500 Hz filter" had nothing to do with filter edges — the
// lines were simply outside the mask entirely.
//
// `shipped_preamble_symbols` now derives from `preamble_bits` by reference and
// `f12_synthesised_template_matches_the_shipped_one` asserts the equality in the DEFAULT gate. Any
// figure quoted from this probe must postdate that assertion.
//
// #1062's central claim is that the alternating sync word has **O(1) time-bandwidth regardless of
// duration**, so "lengthening it adds energy but no spreading, and the correlation gain against band
// noise does not improve the way template length suggests." Everything the issue proposes — a PN or
// chirp successor, i.e. a WIRE-FORMAT change — rests on that.
//
// **It has never been measured.** `f7` varies duration only *within* PN (PN-110 → PN-220) and
// compares the shipped alternating template at one length. So the tree shows that spreading at fixed
// duration buys nothing, and that duration works *for PN* — but not whether duration works for the
// sequence we actually ship.
//
// This is the controlled version: the SAME `pn_template` machinery, the same baud, the same
// durations as f7's PN rows, the same bands and the same five seeds — only the sequence differs.
// Read the two tables side by side.
//
// **Run at BPSK250, 32 vs 64 symbols** — deliberately, not the BPSK1000 the first version used.
// BPSK250 is the only mode that publishes a template (`DERIVED_FOR`), and its lines at ±62.5 Hz
// retain 0.998 / 0.980 through 500 / 200 Hz masks. At BPSK1000 the shipped structure's lines sit at
// ±250 Hz — exactly the 1250–1750 brick-wall edges — so those cells would measure edge leakage
// rather than the deployed mode. Retention is the criterion: a veto threshold must sit between
// ρ_signal and ρ_noise in ABSOLUTE terms, and ρ_signal ≤ retention even at infinite SNR, so a cell
// whose retention is below the 0.40 floor cannot be armed at any SNR.
//
// **What a fall of ~1/√2 does and does not mean.** It refutes exactly one sentence: that a longer
// periodic preamble buys no noise-floor margin because its time-bandwidth is O(1). Template
// time-bandwidth governs ONSET AMBIGUITY and INTERFERER REJECTION; the in-band noise floor is set by
// the NOISE's time-bandwidth in the correlator window, which is why every template obeys 1/√T there
// — a pure tone included. So it is the textbook null rather than a discovery, and it is **not**:
//
// * *a way to avoid a wire change* — `PREAMBLE_SYMS` is consumed by the receiver in at least six
//   places (data-symbol start, range start and the 4×PREAMBLE_SYMS AFC window in `demodulate.rs`;
//   `frame_geometry`; the engine's step geometry; `ScanPlanner`'s 33-symbol minimum; #1142's SNR
//   lock over "the first 32 symbols"). An old receiver demodulates the extra symbols as data and
//   reports invalid magic — the same class of break as PN.
// * *a fix for #1060* — closed by #1157. The live residual is the CFAR STAND-DOWN at narrow filters,
//   and whether 2× keeps it armed depends on the DELIVERED-FRAME ρ at 252 ms on a fade, which is
//   unmeasured (`DELIVERED_FRAME_RHO_BOUND = 0.50` was derived at 124 ms).
// * *a change of direction for #1062* — it retracts one bullet and leaves the two that name the open
//   defects, both duration-independent: onset placement (peak sidelobe 0.997 vs PN's 0.234) and a
//   steady tone scoring ρ ≈ 0.70 on a line at any grid width.
//
// Either way the PN case must then rest on what f7 already showed it buys — onset placement (peak
// sidelobe 0.997 → 0.234) and interferer refusal — rather than on noise-floor margin, because at
// comparable duration a 29× occupancy increase made ρ′ slightly WORSE (0.431 → 0.468 at 500 Hz).

/// The SHIPPED preamble's symbols, `n` of them, derived from `preamble_bits` BY REFERENCE.
///
/// **This function exists because the first version of f12 measured the wrong sequence.** It used a
/// hand-written `+1, -1, +1, …` chip run under a doc comment asserting that was "the shipped sync
/// word's structure". It is not: the shipped preamble is alternating **bits**, and NRZI flips phase
/// only on a `1`, so the **symbols** are `--++` repeating — period four, not period two. Normalised
/// correlation between the two templates is **0.040**, and f12's lines sat at fc ± baud/2 rather
/// than fc ± baud/4. Every number that version produced was about a template this modem never
/// transmits.
///
/// That is CLAUDE.md's banned construct verbatim: *"A doc-comment fidelity claim with
/// hand-transcribed parameters is banned — a comment cannot fail."* The comment claimed fidelity and
/// nothing checked it. `f12_synthesised_template_matches_the_shipped_one` is the check, and it runs
/// in the DEFAULT gate rather than only under `--ignored`, so the claim cannot rot again.
fn shipped_preamble_symbols(n: usize) -> Vec<f32> {
    // NRZI: start at +1, flip on every `1`. `pn_template` inverts this exactly
    // (`bits.push(c != prev)`), so feeding these symbols back reproduces `preamble_bits`.
    let mut sym = 1.0f32;
    bpsk_plugin::modulate::preamble_bits(n)
        .into_iter()
        .map(|b| {
            if b {
                sym = -sym;
            }
            sym
        })
        .collect()
}

/// The synthesised template IS the shipped one at the shipped length — asserted, not claimed in prose.
///
/// Deliberately NOT `#[ignore]`d. A research probe measuring the wrong artifact is worse than no
/// probe, because it produces numbers that look like evidence; this one produced a table I was ready
/// to reframe a wire-format issue around.
#[test]
fn f12_synthesised_template_matches_the_shipped_one() {
    let shipped = plugin_template("BPSK250")
        .expect("BPSK250 publishes a template")
        .0;
    let built = pn_template(
        "BPSK250",
        &shipped_preamble_symbols(bpsk_plugin::modulate::PREAMBLE_SYMS),
    )
    .expect("synthesised template");
    assert_eq!(
        built.len(),
        shipped.len(),
        "synthesised template is a different length from the shipped one"
    );
    // `rho_of` needs a window strictly longer than the template (it searches lags), so pad the
    // shipped template with silence rather than comparing equal lengths.
    let mut window = shipped.clone();
    window.extend(std::iter::repeat_n(0.0f32, 64));
    let r = rho_of(&built, &window, 0.0).expect("correlation");
    assert!(
        r > 0.999,
        "the synthesised template does not reproduce the shipped preamble (rho = {r:.4}). The first \
         version of f12 scored 0.040 here — it measured alternating SYMBOLS while the wire carries \
         alternating BITS, which NRZI turns into a period-four `--++` run."
    );
}

#[test]
#[ignore = "verification (#1062): does length help the ALTERNATING preamble, or only PN?"]
fn f12_does_length_help_the_alternating_preamble() {
    let alt32 = pn_template("BPSK250", &shipped_preamble_symbols(32)).expect("alt32");
    let alt64 = pn_template("BPSK250", &shipped_preamble_symbols(64)).expect("alt64");

    let cases: [(&str, &[f32]); 2] = [
        ("BPSK250 shipped --++ 32 sym (124 ms)", &alt32),
        ("BPSK250 shipped --++ 64 sym (252 ms)", &alt64),
    ];
    // Identical to f7's bands and seeds ON PURPOSE — the comparison is against f7's PN rows, and a
    // different seed set or band list would make the two tables incomparable.
    let bands = [
        ("ssb 300-2700", 300.0f32, 2_700.0),
        ("filter 1250-1750", 1_250.0, 1_750.0),
        ("filter 1400-1600", 1_400.0, 1_600.0),
    ];
    let seeds: [u64; 5] = [12345, 777, 90210, 31337, 424242];
    let per_seed = 120_000usize;

    println!(
        "\nF12: does length help the ALTERNATING preamble? same machinery as F7, {} seeds",
        seeds.len()
    );
    println!(
        "compare against F7's PN-110 -> PN-220, which fell 0.735x (500 Hz) and 0.723x (200 Hz)."
    );
    for (name, t) in cases {
        println!(
            "\n{name}: {} samples, {:.0} ms, occupancy {:.3}",
            t.len(),
            t.len() as f32 / FS * 1000.0,
            band_occupancy(t)
        );
        println!(
            "  {:<18} {:>12} {:>12} {:>10} {:>12}",
            "band", "NOISE max", "NOISE med", "SIGNAL", "ratio rho'"
        );
        for (bname, lo, hi) in bands {
            let mut peaks: Vec<f32> = seeds
                .iter()
                .map(|&sd| peak_rho_equalised(t, &band_noise(per_seed, lo, hi, sd)))
                .collect();
            peaks.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let (mx, med) = (peaks[peaks.len() - 1], peaks[peaks.len() / 2]);
            let filtered = band_limit(t, lo, hi);
            let w = (t.len() + F7_LAG_BOUND).min(filtered.len());
            let sig = rho_of(t, &filtered[..w], 20.0).unwrap_or(f32::NAN);
            println!(
                "  {bname:<18} {mx:>12.3} {med:>12.3} {sig:>10.3} {:>12.3}",
                med / sig
            );
        }
    }
    println!("\n  A ratio near 1/sqrt(2) = 0.707 retracts ONE sentence — that a longer periodic");
    println!("  preamble buys no noise-floor margin. It is NOT a way around a wire change, NOT a");
    println!("  fix for #1060 (closed by #1157), and does not touch the onset-placement or");
    println!("  tone-on-a-line halves of #1062, which are duration-independent.");
}

// ── F13: what does DOUBLING the preamble cost on a fade? (#1062, the unmeasured half) ─────────
//
// f7/f12 measured what length BUYS: the idle-noise ρ ceiling falls ×0.68–0.73, matching 1/√T. This
// is the other side of the subtraction, and without it the buy is arithmetic on one column: a longer
// COHERENT template spans more of the fade, so it scores DELIVERED frames lower, and part of the
// noise-side margin is spent before anyone can bank it. `DELIVERED_FRAME_RHO_BOUND = 0.50` was
// derived at 124 ms.
//
// **The cheap arm, run first on purpose.** No modem, no decode, no receive timeout — fade the two
// templates themselves at high SNR over many seeds and look at the ρ64/ρ32 ratio. Two reasons this
// can settle it without the expensive conditioned run:
//
// * The mean penalty is derivable and small: with Gaussian shaping `R(τ) = exp(−(πστ)²)` at σ = 1 Hz
//   gives R(0.124) = 0.86 and R(0.252) = 0.53, so the mean coherent-sum factor goes 0.975 → 0.907 —
//   a 3.5 % amplitude penalty. **That is not the question.** The bound is set by a low quantile, and
//   ρ is scale-normalised, so a flat null costs ρ only through local SNR while an in-window PHASE
//   ROTATION costs coherence at any SNR. The rotation is the new mechanism at 252 ms, and it lives
//   in the tail, not the mean.
// * Unconditioned is the PESSIMISTIC side — it includes null frames no receiver would deliver — so
//   a miss rate near zero here is an upper bound on the conditioned one and ends the question.
//
// **RESULT, 2026-09-07 (400 seeds, 30 dB, 67 s — not the 1–1.5 h a conditioned decode run costs):**
//
// ```text
// template   n     min     p01     p10   median      miss rate at theta 0.40 .. 0.55
// 32 sym   400   0.665   0.693   0.870   0.965      0.000 everywhere
// 64 sym   400   0.630   0.679   0.794   0.950      0.000 everywhere
// ```
//
// The fade cost of doubling is **real but small, and it lives in the tail exactly as the mechanism
// predicts**: median −1.5 %, p10 −9 %, min −5 %. It reaches no threshold the stand-down decision
// uses — the worst window in 400 seeds still scores 0.630 against a top candidate of 0.55.
//
// **Scope, stated narrowly.** Unconditioned is the pessimistic side, so 0/400 upper-bounds the
// conditioned miss rate at 3/400 = 0.75 % (95 %), and the expensive decode run is confirmation
// rather than discovery. But this is envelope-only: it says nothing about whether a 64-symbol
// RECEIVER acquires correctly, only what the correlation would score. And it runs at 30 dB on
// purpose, to isolate the COHERENCE penalty — the noise penalty is the other column, measured by
// f7/f12, and it moves the other way.
//
// Same-END alignment, not same-start: on a real 64-symbol wire the preamble ENDS where the data
// begins, so the 64 window is `[L−1024, L+1024)` against the 32's `[L, L+1024)`. Same start would
// have let the 64 window overlap a span the 32 never saw, biasing the comparison toward it.

/// The `--++` template for `n` preamble symbols, built by hand.
///
/// `bpsk_preamble_template` hardcodes `PREAMBLE_SYMS - 1`, so it cannot express n = 64; this takes
/// the last `n-1` symbols' worth of a synthesised run, matching that function's convention.
fn alt_template_n(mode: &str, n: usize) -> Vec<f32> {
    let full = pn_template(mode, &shipped_preamble_symbols(n)).expect("template");
    let per_sym = full.len() / n;
    full[..per_sym * (n - 1)].to_vec()
}

/// Mechanical checks on the n=64 template, mirroring the n=32 assertion (#1062).
///
/// Not `#[ignore]`d: f12 measured a template the modem never transmits, and the lesson was that the
/// fixture's fidelity has to be asserted where it runs by default. n=64 is a *new* fixture and gets
/// the same treatment rather than inheriting trust from n=32.
#[test]
fn f13_the_64_symbol_template_extends_the_shipped_one() {
    let t32 = alt_template_n("BPSK250", 32);
    let t64 = alt_template_n("BPSK250", 64);
    let per_sym = t32.len() / 31;

    // The shipped run is period-4 and 32 bits leave the NRZI state where it started, so the second
    // 32 symbols repeat the first — a 64-symbol preamble really is "32 more of the same".
    let head = &t64[..per_sym * 31];
    let r = rho_of(
        &t32,
        &{
            let mut w = head.to_vec();
            w.extend(std::iter::repeat_n(0.0f32, 64));
            w
        },
        0.0,
    )
    .expect("correlation");
    assert!(
        r > 0.999,
        "the 64-symbol template's first 31 symbols are not the shipped template (rho = {r:.4})"
    );
    assert_eq!(
        t64.len(),
        per_sym * 63,
        "n=64 must yield 63 symbols' worth, matching bpsk_preamble_template's PREAMBLE_SYMS-1"
    );
    // 63 x 32 = 2016 sits 32 UNDER MAX_PREAMBLE_CORRELATION_SAMPLES (2048), so the engine still runs
    // the passband correlator on it and this probe's rho remains the engine's rho. Anything longer
    // flips it to the decimated DDC arm and the two stop being comparable.
    assert!(
        t64.len() < 2048,
        "a 64-symbol template ({} samples) would cross MAX_PREAMBLE_CORRELATION_SAMPLES and change \
         which correlator the engine uses",
        t64.len()
    );
}

#[test]
#[ignore = "verification (#1062): the fade cost of doubling the preamble, envelope-only"]
fn f13_fade_cost_of_doubling_the_preamble() {
    let t32 = alt_template_n("BPSK250", 32);
    let t64 = alt_template_n("BPSK250", 64);
    let seeds: u64 = std::env::var("F13_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400);
    let snr: f32 = std::env::var("F13_SNR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30.0);

    // `F13_BAND=lo:hi` applies the same brick-wall mask `band_noise` uses. Default OFF preserves
    // the historical cells, but see the header: unfiltered is not the regime that reads the bound.
    let band: Option<(f32, f32)> = std::env::var("F13_BAND").ok().and_then(|v| {
        let (a, b) = v.split_once(':')?;
        Some((a.parse().ok()?, b.parse().ok()?))
    });
    let grid32 = std::env::var("F13_GRID32").is_ok();
    let dump = std::env::var("F13_DUMP").is_ok();

    println!("\nF13: fade cost of 32 -> 64 preamble symbols, {seeds} seeds @ {snr} dB, same-END alignment");
    println!(
        "  band={}  grid={}",
        band.map(|(l, h)| format!("{l}-{h} Hz"))
            .unwrap_or_else(|| "unfiltered (NOT a receiver's regime)".into()),
        if grid32 {
            "32-arm grid on both (ablation)"
        } else {
            "engine-derived per template"
        }
    );
    println!(
        "  unconditioned = the PESSIMISTIC side: it includes null windows no receiver delivers."
    );

    let lead = t64.len() - t32.len(); // the extra span the 64 template reaches back over
    let mut r32: Vec<f32> = Vec::new();
    let mut r64: Vec<f32> = Vec::new();
    for seed in 0..seeds {
        // ONE buffer, ONE fade: the 64 window is [0, len64) and the 32 window is [lead, len64), so
        // they share an end and one noise realisation. Two passes would need "same seed => same
        // fade" as a premise; this construction has no premise to prove.
        let mut clean = t64.clone();
        clean.extend(std::iter::repeat_n(0.0f32, 64)); // lag room for the correlator
        let mut c = WattersonConfig::moderate_f1(Some(seed));
        c.snr_db = snr;
        let faded = WattersonChannel::new(c).expect("channel").apply(&clean);

        // The receive filter is the whole point of the masked arm: the stand-down is only ever
        // consulted when the derived threshold exceeds the bound, which the #1157 rig data puts at
        // <= 500 Hz filters. An UNFILTERED cell is therefore outside the regime that reads the bound
        // — and it is no receiver's regime either, since even a wide station sits at SSB 300-2700.
        let faded = match band {
            Some((lo, hi)) => {
                let mut m = band_limit(&faded, lo, hi);
                m.truncate(faded.len());
                m
            }
            None => faded,
        };

        // `engine_grid` derives its step from the template length (engine.rs:6993,
        // `step = (0.25 * fs / tlen).max(0.5)`), so the 64-symbol template gets 41 hypotheses at
        // 0.99 Hz where the 32 gets 21 at 2.02 Hz. That is engine-faithful — a real 64-symbol
        // receiver would use the finer grid — but it is a SECOND difference between the arms, and
        // it turned out to carry most of the apparent tail gain in the unfiltered cells. `F13_GRID32`
        // gives the 64 arm the 32 arm's grid so the two effects can be told apart.
        let r64_v = if grid32 {
            rho_of_on_grid(&t64, &faded, &engine_grid(t32.len(), 20.0))
        } else {
            rho_of(&t64, &faded, 20.0)
        };
        if let Some(v) = r64_v {
            r64.push(v);
        }
        if let Some(v) = rho_of(&t32, &faded[lead..], 20.0) {
            r32.push(v);
        }
        if dump {
            // Per-seed PAIRED dump. The arms share one buffer, one fade and one noise realisation,
            // so the difference is paired and an unpaired summary understates the resolution.
            println!(
                "  PAIR {seed} {:.4} {:.4}",
                r32.last().copied().unwrap_or(f32::NAN),
                r64.last().copied().unwrap_or(f32::NAN)
            );
        }
    }

    let q = |v: &mut Vec<f32>, p: f32| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v[((v.len() as f32 - 1.0) * p).round() as usize]
    };
    let (mut a, mut b) = (r32.clone(), r64.clone());
    println!(
        "  {:<8} {:>6} {:>8} {:>8} {:>8} {:>8}",
        "template", "n", "min", "p01", "p10", "median"
    );
    for (name, v) in [("32 sym", &mut a), ("64 sym", &mut b)] {
        println!(
            "  {name:<8} {:>6} {:>8.3} {:>8.3} {:>8.3} {:>8.3}",
            v.len(),
            q(v, 0.0),
            q(v, 0.01),
            q(v, 0.10),
            q(v, 0.50)
        );
    }
    // The decision input is a MISS RATE at a candidate threshold, not a ceiling: `stands_down` is
    // `derived > bound`, and the bound is a level delivered frames exceed with a bounded miss rate.
    println!("\n  miss rate at candidate bound theta (fraction of windows scoring BELOW it):");
    println!(
        "  {:<8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "template", "0.40", "0.44", "0.46", "0.48", "0.50", "0.55"
    );
    for (name, v) in [("32 sym", &r32), ("64 sym", &r64)] {
        let miss = |t: f32| v.iter().filter(|&&x| x < t).count() as f32 / v.len().max(1) as f32;
        println!(
            "  {name:<8} {:>8.3} {:>8.3} {:>8.3} {:>8.3} {:>8.3} {:>8.3}",
            miss(0.40),
            miss(0.44),
            miss(0.46),
            miss(0.48),
            miss(0.50),
            miss(0.55)
        );
    }
    println!(
        "\n  If the 64 miss rate at ~0.46 is ~0 here, the conditioned run is confirmation, not"
    );
    println!(
        "  discovery — unconditioned already includes the null windows a receiver never delivers."
    );
    println!(
        "\n  RESULT, CORRECTED 2026-09-10 — the original block reported the 30 dB UNFILTERED cell"
    );
    println!(
        "  and quoted `min`, which is one draw and not resolvable at these seed counts. Use p10."
    );
    println!(
        "  Run masked (F13_BAND): the stand-down is only consulted at <= 500 Hz filters, so an"
    );
    println!("  unfiltered cell is outside the regime that reads the bound — and is no receiver's");
    println!(
        "  regime either. Masked, 600 seeds, p10 32 -> 64: 0.808 -> 0.764 (6 dB, 1250-1750) and"
    );
    println!("  0.847 -> 0.797 (10 dB) — the coherence penalty is REAL and in-band, paired CI");
    println!(
        "  [-0.064,-0.016]. Unfiltered the tail appears to RISE, but F13_GRID32 shows most of"
    );
    println!("  that is the finer frequency grid the longer template earns, not noise averaging.");
    println!("  thresholds the CFAR stand-down decision uses.");
}
