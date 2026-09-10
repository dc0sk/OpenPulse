//! Deriving BPSK31's OWN rho constants — the work that would let the slow rungs have a veto.
//!
//! Phase 0 of #1062 removed the *cost* barrier to correlating BPSK31's 7936-sample template. It did
//! not remove the correctness barrier: `PREAMBLE_RHO_THRESHOLD` and `PREAMBLE_RHO_GRID_HZ` were both
//! derived on BPSK250 and both are wrong here — the threshold because rho is normalised and this
//! template is 8x longer, the grid because BPSK31's spectral lines are 15.6 Hz apart against a
//! +/-20 Hz default that reaches every one of them.
//!
//! This measures the two columns that a grid derivation needs. The threshold's decode column is
//! separate and expensive (BPSK31 frames run ~12 s), so it is only worth running if a usable gap
//! appears here.
//!
//! **Grid first, because it bounds everything else.** A tone landing exactly ON a line scores
//! rho ~= sqrt(0.5) whatever the grid — narrowing the grid narrows the vulnerable *bands* around
//! each line, it cannot remove them. So the question is not "is there a safe grid" but "how much of
//! the frequency axis stays vulnerable", and the answer is set by the settle residual: the grid need
//! only be wide enough to cover what the settle leaves behind.

//!
//! ## Why this is in the tree, `#[ignore]`d (landed 2026-08-19)
//!
//! This harness produced the numbers quoted on #1062 — the Pi 5 reference-hardware ratio (1.85x,
//! not the 5-10x that had been assumed) and the withdrawal of "drop the DDC" — while living on an
//! unmerged branch for two weeks. The conclusions were published and the apparatus was not, which
//! is the shape a later reader cannot check: a measurement whose harness is unavailable is a claim,
//! not a result. It is landed `#[ignore = "verification"]`, so it costs the gate nothing and can be
//! re-run with `--ignored` when a constant it bounds is revisited.
//!
//! **What has moved under it since the measurements were taken.** #1060 has been closed by the
//! #1157 CFAR calibration, which derives the threshold from the station's own correlation stream at
//! runtime. That retires the *question* R5 asks ("would an unconditional DDC fix #1060 for
//! BPSK250") but not its *answer*: the DDC raises BPSK250's noise ceiling, and R6's doc comment
//! still rests on that number. The published constants R1-R4 bound are unchanged on `main` —
//! `PREAMBLE_RHO_THRESHOLD = 0.40` and `PREAMBLE_RHO_GRID_HZ = 20.0` are still BPSK250-derived, so
//! the slow rungs still have no veto and #1059/#1062 still need exactly these columns.

use openpulse_core::plugin::{ModulationConfig, ModulationPlugin};
use openpulse_dsp::acquisition::DdcMatchedFilter;

mod common;
use std::f32::consts::PI;

const FS: f32 = 8_000.0;
const FC: f32 = 1_500.0;

fn cfg(mode: &str) -> ModulationConfig {
    ModulationConfig {
        mode: mode.into(),
        sample_rate: 8_000,
        center_frequency: FC,
        ..Default::default()
    }
}

/// The engine's derivation, mirrored so this measures what would ship.
fn cutoff_and_decim(occupied_bw: f32, grid_hz: f32, tmpl_len: usize) -> (f32, usize) {
    let cutoff = occupied_bw / 2.0 + grid_hz + occupied_bw * 0.15;
    let by_budget = tmpl_len.div_ceil(2_048);
    let by_bandwidth = ((FS / 2.0) / (cutoff * 1.25)).floor().max(1.0) as usize;
    (cutoff, by_budget.min(by_bandwidth).max(1))
}

fn grid_for(tmpl_len: usize, decim: usize, grid_hz: f32) -> Vec<f32> {
    // Grid step from the DECIMATED template's coherent bandwidth, which is the resolution the
    // correlator actually has after the DDC.
    let eff = (tmpl_len / decim).max(1);
    let step = (0.25 * (FS / decim as f32) / eff as f32).max(0.05);
    let n = (grid_hz / step).round() as i32;
    (-n..=n).map(|k| k as f32 * step).collect()
}

#[test]
#[ignore = "verification"]
fn r1_how_narrow_can_bpsk31_grid_be() {
    let template = bpsk_plugin::modulate::bpsk_preamble_template(&cfg("BPSK31")).expect("template");
    let baud = 31.25f32;
    let occ = 2.0 * baud;
    let first_line = baud / 4.0;

    println!(
        "\nR1: BPSK31 grid derivation. template {} samples, lines at odd multiples of {:.2} Hz",
        template.len(),
        first_line
    );
    println!("    spacing between adjacent lines {:.2} Hz\n", baud / 2.0);
    println!(
        "{:>8} {:>8} {:>6} {:>12} {:>14} {:>18}",
        "grid", "cutoff", "D", "worst tone", "vulnerable %", "verdict"
    );

    for grid_hz in [20.0f32, 6.0, 4.0, 2.0, 1.0, 0.5] {
        let (cutoff, d) = cutoff_and_decim(occ, grid_hz, template.len());
        let mf = DdcMatchedFilter::new(&template, FC, FS, cutoff, d);
        let grid = grid_for(template.len(), d, grid_hz);

        // Sweep one full line spacing; the pattern repeats in position.
        let mut worst = 0.0f32;
        let mut over = 0usize;
        let mut pts = 0usize;
        let mut f = 0.0f32;
        while f <= baud / 2.0 {
            let tone: Vec<f32> = (0..template.len() + 400)
                .map(|k| 0.25 * (2.0 * PI * (FC + f) * k as f32 / FS).cos())
                .collect();
            if let Some((r, _)) = mf.search_normalized_over_frequency(&tone, 0.05, &grid) {
                worst = worst.max(r.rho);
                if r.rho >= 0.40 {
                    over += 1;
                }
                pts += 1;
            }
            f += (baud / 2.0) / 60.0;
        }
        let vulnerable = 100.0 * over as f32 / pts.max(1) as f32;
        let verdict = if grid_hz >= first_line {
            "grid reaches the line"
        } else if vulnerable > 40.0 {
            "usable, wide bands"
        } else {
            "usable"
        };
        println!(
            "{:>7.1}  {:>7.1} {:>6} {:>12.3} {:>13.0}% {:>18}",
            grid_hz, cutoff, d, worst, vulnerable, verdict
        );
    }
    println!("\n  'vulnerable %' is the fraction of the frequency axis where a steady tone would");
    println!("  clear 0.40. It cannot reach zero -- a tone ON a line scores ~0.70 at any grid --");
    println!("  so the grid is sized from the settle residual, and the residual bands are the");
    println!("  irreducible cost of a two-line sync word. BPSK250 at +/-20 Hz sits at ~32%.");
}

/// R2: the false-REJECT side — does a real frame still correlate at the narrow grid?
///
/// R1 picks the grid from the false-accept side alone, and that is half a derivation. The grid
/// exists to absorb whatever frequency error the settle leaves behind; make it too narrow and real
/// frames stop being corroborated, which is the failure the veto's whole placement was chosen to
/// avoid (checking correlation *before* the settle was rejected for exactly this — a matched filter
/// integrates coherently, so rho collapses with residual offset).
///
/// A pass here needs rho to stay high across the residual the settle actually leaves. Measured for
/// BPSK250 at <= 0.3 Hz over a 1056-sample window; BPSK31's settle window is far longer, so its
/// residual should be no worse — but "should" is why this is measured rather than assumed.
#[test]
#[ignore = "verification"]
fn r2_does_a_real_frame_survive_the_narrow_grid() {
    let template = bpsk_plugin::modulate::bpsk_preamble_template(&cfg("BPSK31")).expect("template");
    let frame = bpsk_plugin::BpskPlugin::new()
        .modulate(b"grid derivation", &cfg("BPSK31"))
        .expect("modulate");
    let occ = 2.0 * 31.25f32;

    println!("\nR2: BPSK31 real-frame rho vs residual carrier offset, per candidate grid");
    println!("    (the settle leaves <= 0.3 Hz on BPSK250; anything the grid must absorb)\n");
    print!("{:>10}", "residual");
    for g in [20.0f32, 4.0, 2.0, 1.0, 0.5] {
        print!("{:>12}", format!("±{g} Hz"));
    }
    println!();

    for residual in [0.0f32, 0.1, 0.3, 0.5, 1.0, 2.0] {
        print!("{:>9.1}", residual);
        for grid_hz in [20.0f32, 4.0, 2.0, 1.0, 0.5] {
            let (cutoff, d) = cutoff_and_decim(occ, grid_hz, template.len());
            let mf = DdcMatchedFilter::new(&template, FC, FS, cutoff, d);
            let grid = grid_for(template.len(), d, grid_hz);
            // Frame arriving with a residual carrier error the settle did not remove.
            let shifted: Vec<f32> = frame
                .iter()
                .enumerate()
                .map(|(k, &s)| s * (2.0 * PI * residual * k as f32 / FS).cos())
                .collect();
            let rho = mf
                .search_normalized_over_frequency(&shifted, 0.05, &grid)
                .map(|(r, _)| r.rho)
                .unwrap_or(f32::NAN);
            print!("{rho:>12.3}");
        }
        println!();
    }
    println!("\n  A grid is usable only if rho stays well above threshold across the residual the");
    println!(
        "  settle really leaves. Collapse at a residual the settle can produce means the grid"
    );
    println!("  is too narrow and real frames would stop being corroborated.");
}

/// R3: the NOISE column — BPSK31's own rho ceiling, through the correlator it would really use.
///
/// The threshold must clear this. It cannot be inherited: rho is normalised, so its noise floor is
/// set by template length, and BPSK31's template is 8x BPSK250's. Two effects push in opposite
/// directions and only measurement settles the net — the longer template lowers the ceiling, while
/// the DDC's anti-alias stage lowers it further by removing noise outside the mode's own 60 Hz
/// passband before the correlation happens.
///
/// Reported as an exceedance RATE at a stated observation length, not a 45 s peak. A maximum over
/// many windows is an extreme value that grows with how long you look, and a threshold derived from
/// one says nothing about how often a settle is actually corroborated by noise.
#[test]
#[ignore = "verification"]
fn r3_bpsk31_noise_ceiling() {
    let template = bpsk_plugin::modulate::bpsk_preamble_template(&cfg("BPSK31")).expect("template");
    let occ = 2.0 * 31.25f32;
    let grid_hz = 2.0f32; // derived in R1/R2
    let (cutoff, d) = cutoff_and_decim(occ, grid_hz, template.len());
    let mf = DdcMatchedFilter::new(&template, FC, FS, cutoff, d);
    let grid = grid_for(template.len(), d, grid_hz);

    println!(
        "\nR3: BPSK31 noise ceiling, DDC path (cutoff {cutoff:.1} Hz, D {d}), grid ±{grid_hz} Hz"
    );
    println!("    exceedance rate over independent windows, not a peak\n");
    println!(
        "{:<20} {:>10} {:>10} {:>10} {:>12} {:>12}",
        "band", "windows", "median", "p99", "max", ">=0.40"
    );

    let win = template.len() + 400;
    for (name, lo, hi) in [
        ("white 0-4k", 0.0f32, 4_000.0),
        ("ssb 300-2700", 300.0, 2_700.0),
        ("filter 1250-1750", 1_250.0, 1_750.0),
        ("filter 1470-1530", 1_470.0, 1_530.0),
    ] {
        // 40 windows made p99 == max, i.e. the percentile was the maximum wearing a
        // percentile's name. A tail estimate needs enough samples that the quantile is interior.
        let noise = band_noise(win * 400, lo, hi, 0.05, 4242);
        let mut rhos: Vec<f32> = Vec::new();
        let mut s = 0usize;
        while s + win <= noise.len() {
            if let Some((r, _)) =
                mf.search_normalized_over_frequency(&noise[s..s + win], 0.05, &grid)
            {
                rhos.push(r.rho);
            }
            s += win;
        }
        rhos.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = rhos.len().max(1);
        let median = rhos[n / 2];
        let p99 = rhos[(n * 99 / 100).min(n - 1)];
        let max = *rhos.last().unwrap_or(&f32::NAN);
        let over = rhos.iter().filter(|&&r| r >= 0.40).count();
        println!(
            "{name:<20} {:>10} {median:>10.3} {p99:>10.3} {max:>12.3} {:>12}",
            rhos.len(),
            format!("{}/{}", over, rhos.len())
        );
    }
    println!("\n  A threshold must clear the ceiling AND stay under the weakest decodable frame.");
    println!("  The narrow-filter rows are the ones that killed the QPSK table in #1053 — a 60 Hz");
    println!("  receive filter is not exotic for a 31-baud mode, it is how the mode is operated.");
}

/// Band-limited noise, shared with the equivalence bench's method.
fn band_noise(n: usize, lo: f32, hi: f32, a: f32, seed: u64) -> Vec<f32> {
    use rustfft::{num_complex::Complex, FftPlanner};
    let mut st = seed | 1;
    let mut buf: Vec<Complex<f32>> = (0..n)
        .map(|_| {
            st ^= st >> 12;
            st ^= st << 25;
            st ^= st >> 27;
            let u = (st.wrapping_mul(0x2545F4914F6CDD1D) >> 33) as f32 / (1u64 << 31) as f32;
            Complex::new(u * 2.0 - 1.0, 0.0)
        })
        .collect();
    let mut pl = FftPlanner::new();
    pl.plan_fft_forward(n).process(&mut buf);
    let bin = FS / n as f32;
    for (k, v) in buf.iter_mut().enumerate() {
        let f = if k <= n / 2 {
            k as f32 * bin
        } else {
            (n - k) as f32 * bin
        };
        if f < lo || f > hi {
            *v = Complex::new(0.0, 0.0);
        }
    }
    pl.plan_fft_inverse(n).process(&mut buf);
    let out: Vec<f32> = buf.iter().map(|c| c.re).collect();
    let rms = (out.iter().map(|s| s * s).sum::<f32>() / n as f32)
        .sqrt()
        .max(1e-12);
    out.iter().map(|s| s / rms * a).collect()
}

/// R4: the DECODE column — the weakest rho that still decodes, on the channel this rung exists for.
///
/// The half that #1053 got wrong, and the reason its QPSK table was withdrawn: an AWGN decode
/// column reported a comfortable margin while the fade column showed the decodable-frame and
/// noise distributions overlapping, so no threshold existed at all. BPSK31 is `hpx_hf`'s SL2 and
/// its `initial_level` — the rung every session starts on — with a 3 dB floor and `Rs`, and the
/// channel it exists for is a fade, not AWGN.
///
/// Expensive by construction: `Rs` emits a 255-byte block, so a BPSK31 frame is ~65 s of audio.
/// That cost is the reason this column gets skipped, and skipping it is what withdrew #1053.
///
/// **Measured 2026-08-04, 48 seeds (`R4_SEEDS=48`), 40 decoded:**
///
/// | | ρ |
/// |---|---|
/// | weakest decodable | **0.625** |
/// | next lowest | 0.652, 0.687, 0.688, 0.692, 0.692 |
/// | noise ceiling (R3) | 0.319 wide-filter / 0.426 at 60 Hz |
///
/// The distributions separate — 0.426 → 0.625 is a 1.47× gap — so a threshold near **0.51** sits
/// 1.20× above the narrow-filter ceiling and 1.22× below the weakest decodable frame. For contrast
/// that is *better* than the shipped BPSK250 position, where the narrow-filter ceiling (0.441)
/// exceeds its own 0.40 threshold outright.
///
/// Six seeds gave 0.693 and 48 gave 0.625: the small sample flattered the bound, which is the whole
/// reason this is run at a size that can see a roughly 1-in-40 tail — the order that withdrew
/// #1053. It cannot exclude a rarer fade. The decodable values cluster 0.62–0.85 with no long lower
/// tail, which is weak evidence the bound is real rather than the edge of the sample.
///
/// One row worth keeping: a frame at ρ 0.689 did **not** decode while one at 0.625 did. ρ measures
/// the preamble, decode measures the whole frame, so the threshold is not predicting decode — its
/// job is only to avoid *rejecting* decodable frames, which is why the weakest decodable value is
/// the bound that matters and not any correlation between the two.
#[test]
#[ignore = "verification"]
fn r4_bpsk31_decode_column() {
    use openpulse_channel::watterson::WattersonChannel;
    use openpulse_channel::{ChannelModel, WattersonConfig};
    use openpulse_core::fec::FecMode;
    use openpulse_modem::channel_sim::ChannelSimHarness;
    use std::time::Duration;

    let template = bpsk_plugin::modulate::bpsk_preamble_template(&cfg("BPSK31")).expect("template");
    let occ = 2.0 * 31.25f32;
    let grid_hz = 2.0f32;
    let (cutoff, d) = cutoff_and_decim(occ, grid_hz, template.len());
    let mf = DdcMatchedFilter::new(&template, FC, FS, cutoff, d);
    let grid = grid_for(template.len(), d, grid_hz);

    let snr_db: f32 = std::env::var("R4_SNR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3.0);
    let seeds: u64 = std::env::var("R4_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);

    println!("\nR4: BPSK31 + Rs on moderate_f1 @ {snr_db} dB — its own floor, its own channel");
    println!("    grid ±{grid_hz} Hz, DDC cutoff {cutoff:.1} Hz, {seeds} seeds\n");
    println!("{:>6} {:>10} {:>10}", "seed", "rho", "decoded");

    let mut decodable: Vec<f32> = Vec::new();
    let mut all: Vec<f32> = Vec::new();
    for seed in 0..seeds {
        let mut h = ChannelSimHarness::new();
        for e in [&mut h.tx_engine, &mut h.rx_engine] {
            e.register_plugin(Box::new(bpsk_plugin::BpskPlugin::new()))
                .unwrap();
        }
        h.rx_engine.set_deterministic_scan_positions(Some(3_000));
        h.rx_engine.set_deterministic_max_iterations(Some(4_000));
        h.tx_engine
            .transmit_with_fec_mode(b"decode column", "BPSK31", FecMode::Rs, None)
            .expect("tx");
        let mut c = WattersonConfig::moderate_f1(Some(seed));
        c.snr_db = snr_db;

        // rho on the same realisation the receiver will see, at the true onset.
        let clean = bpsk_plugin::BpskPlugin::new()
            .modulate(b"decode column", &cfg("BPSK31"))
            .unwrap();
        let mut c2 = WattersonConfig::moderate_f1(Some(seed));
        c2.snr_db = snr_db;
        let faded = WattersonChannel::new(c2).unwrap().apply(&clean);
        let rho = mf
            .search_normalized_over_frequency(
                &faded[..(template.len() + 400).min(faded.len())],
                0.05,
                &grid,
            )
            .map(|(r, _)| r.rho)
            .unwrap_or(f32::NAN);

        h.route(&mut WattersonChannel::new(c).unwrap());
        let ok = h
            .rx_engine
            .receive_with_fec_mode_timeout(
                "BPSK31",
                FecMode::Rs,
                None,
                Duration::from_millis(180_000),
            )
            .is_ok();
        println!("{seed:>6} {rho:>10.3} {ok:>10}");
        all.push(rho);
        if ok {
            decodable.push(rho);
        }
    }
    decodable.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    println!(
        "\n  decoded {}/{}; weakest decodable rho {:?}",
        decodable.len(),
        all.len(),
        decodable.first()
    );
    println!(
        "  A threshold must sit BELOW that and ABOVE the R3 noise ceiling (0.319 wide-filter,"
    );
    println!("  0.426 at 60 Hz). If the weakest decodable rho falls into that range, the two");
    println!("  distributions overlap and BPSK31 publishes NO template — the #1053 outcome.");
}

/// R5: would an UNCONDITIONAL DDC fix #1060 for BPSK250? (Measured, because reasoning is cheap.)
///
/// R3 found the DDC makes white / SSB / 500 Hz noise ceilings byte-identical at BPSK31 — the
/// correlator never sees outside its own passband, so wider receive filters stop mattering. The
/// tempting generalisation is "#1060 is dissolved on the DDC path", and the tempting design move is
/// to make the DDC unconditional so every mode gets that.
///
/// But #1060 was filed about **BPSK250**, whose 992-sample template fits the budget and therefore
/// runs the passband correlator — the immunity does not reach it. And BPSK250's DDC passband would
/// be ±345 Hz (occupied 500/2 + grid 20 + shaping) while #1060's case is a **500 Hz receive filter**
/// = ±250 Hz, which is *narrower*. A lowpass cannot undo filtering that has already happened inside
/// its own passband.
///
/// So the prediction is that an unconditional DDC does NOT fix #1060, and the design move would be
/// justified by a benefit it does not deliver. Measured rather than argued.
#[test]
#[ignore = "verification"]
fn r5_would_unconditional_ddc_fix_1060_for_bpsk250() {
    use openpulse_dsp::acquisition::IqMatchedFilter;

    let template =
        bpsk_plugin::modulate::bpsk_preamble_template(&cfg("BPSK250")).expect("template");
    let occ = 2.0 * 250.0f32;
    let grid_hz = 20.0f32;
    let (cutoff, _) = cutoff_and_decim(occ, grid_hz, template.len());
    // Unconditional form: decimate as far as the bandwidth allows, ignoring the budget branch.
    let d = ((FS / 2.0) / (cutoff * 1.25)).floor().max(1.0) as usize;
    let ddc = DdcMatchedFilter::new(&template, FC, FS, cutoff, d);
    let pb = IqMatchedFilter::new(template.clone());
    let ddc_grid = grid_for(template.len(), d, grid_hz);
    let pb_step = (0.25 * FS / template.len() as f32).max(0.5);
    let pb_n = (grid_hz / pb_step).round() as i32;
    let pb_grid: Vec<f32> = (-pb_n..=pb_n).map(|k| k as f32 * pb_step).collect();

    println!("\nR5: BPSK250 noise ceiling, passband vs unconditional DDC");
    println!(
        "    DDC passband ±{cutoff:.0} Hz, D {d}, template {} -> {} complex\n",
        template.len(),
        template.len() / d
    );
    println!(
        "{:<22} {:>12} {:>12} {:>10}",
        "receive filter", "passband max", "DDC max", "helps?"
    );

    let win = template.len() + 400;
    for (name, lo, hi) in [
        ("white 0-4k", 0.0f32, 4_000.0),
        ("ssb 300-2700", 300.0, 2_700.0),
        ("500 Hz (the #1060 case)", 1_250.0, 1_750.0),
        ("200 Hz", 1_400.0, 1_600.0),
    ] {
        let noise = band_noise(win * 120, lo, hi, 0.05, 909);
        let (mut pmax, mut dmax) = (0.0f32, 0.0f32);
        let mut s = 0usize;
        while s + win <= noise.len() {
            let w = &noise[s..s + win];
            if let Some((r, _)) =
                pb.search_normalized_over_frequency(w, w.len() - pb.len(), 0.05, FS, &pb_grid)
            {
                pmax = pmax.max(r.rho);
            }
            if let Some((r, _)) = ddc.search_normalized_over_frequency(w, 0.05, &ddc_grid) {
                dmax = dmax.max(r.rho);
            }
            s += win;
        }
        let helps = if dmax < pmax - 0.02 { "yes" } else { "no" };
        println!("{name:<22} {pmax:>12.3} {dmax:>12.3} {helps:>10}");
    }
    println!("\n  BPSK250's shipped threshold is 0.40. A filter NARROWER than the DDC passband is");
    println!("  already inside it, so the lowpass cannot undo it — if the 500 Hz row shows no");
    println!("  improvement, an unconditional DDC does not fix #1060 and must be justified on");
    println!("  other grounds (cost, or uniformity) rather than on that benefit.");
}

/// R6: what does decimation cost the noise ceiling? (The DDC is not free, and R3 didn't price it.)
///
/// R5 found the DDC *raises* BPSK250's noise ceiling — 0.145 → 0.365 on white noise at D = 9. The
/// mechanism is that ρ is normalised over the template's samples, so its noise floor scales roughly
/// as 1/√N, and decimating by D removes a factor D of them. Two effects compete and both push the
/// same way for wide noise: fewer samples raises the floor, and removing out-of-band noise from the
/// window *energy* (the denominator) raises it too.
///
/// R3 measured BPSK31's ceiling through the DDC and reported the bands as byte-identical, which is
/// true and is about band-*independence*. It said nothing about the level, and the level is what a
/// threshold has to clear. This prices it.
#[test]
#[ignore = "verification"]
fn r6_what_decimation_costs_the_noise_ceiling() {
    use openpulse_dsp::acquisition::IqMatchedFilter;

    println!("\nR6: noise ceiling, passband vs DDC, per mode (max over independent windows)\n");
    println!(
        "{:<10} {:>6} {:>8} {:>14} {:>12} {:>12} {:>10}",
        "mode", "D", "post-DDC", "band", "passband", "DDC", "cost"
    );
    for (mode, baud) in [("BPSK31", 31.25f32), ("BPSK250", 250.0)] {
        let template = bpsk_plugin::modulate::bpsk_preamble_template(&cfg(mode)).expect("template");
        let occ = 2.0 * baud;
        let grid_hz = if mode == "BPSK31" { 2.0 } else { 20.0 };
        let (cutoff, d) = cutoff_and_decim(occ, grid_hz, template.len());
        let ddc = DdcMatchedFilter::new(&template, FC, FS, cutoff, d);
        let pb = IqMatchedFilter::new(template.clone());
        let ddc_grid = grid_for(template.len(), d, grid_hz);
        let step = (0.25 * FS / template.len() as f32).max(0.5);
        let n = (grid_hz / step).round() as i32;
        let pb_grid: Vec<f32> = (-n..=n).map(|k| k as f32 * step).collect();

        let win = template.len() + 400;
        for (bname, lo, hi) in [
            ("white 0-4k", 0.0f32, 4_000.0),
            ("ssb 300-2700", 300.0, 2_700.0),
        ] {
            let noise = band_noise(win * 60, lo, hi, 0.05, 555);
            let (mut pmax, mut dmax) = (0.0f32, 0.0f32);
            let mut s = 0usize;
            while s + win <= noise.len() {
                let w = &noise[s..s + win];
                if let Some((r, _)) =
                    pb.search_normalized_over_frequency(w, w.len() - pb.len(), 0.05, FS, &pb_grid)
                {
                    pmax = pmax.max(r.rho);
                }
                if let Some((r, _)) = ddc.search_normalized_over_frequency(w, 0.05, &ddc_grid) {
                    dmax = dmax.max(r.rho);
                }
                s += win;
            }
            println!(
                "{:<10} {:>6} {:>8} {:<14} {:>12.3} {:>12.3} {:>+10.3}",
                mode,
                d,
                template.len() / d,
                bname,
                pmax,
                dmax,
                dmax - pmax
            );
        }
    }
    println!("\n  A positive 'cost' means decimation RAISED the ceiling a threshold must clear.");
    println!("  The DDC buys band-independence and affordability, and pays for both here. That is");
    println!("  a trade to state, not a benefit to claim.");
}

/// R7: SEPARATION, which is the quantity that decides detection — not signal ρ or noise ρ alone.
///
/// The DDC's anti-alias lowpass removes out-of-band energy from the correlation window. That energy
/// sits in ρ's **denominator**, so removing it raises ρ — for the signal *and* for the noise. I
/// recorded the signal half as "an interference-rejection stage you get paid for" (P2: an
/// out-of-band tone costs the passband correlator 1.000 → 0.945 while the DDC path stays at 1.000)
/// without measuring the noise half, and R6 then found the noise ceiling rising by *more*.
///
/// If noise rises further than signal, the DDC is not rejecting interference in any useful sense —
/// it is renormalising both, and the detector is worse off. What matters is
/// `ρ_signal − ρ_noise_ceiling` measured in the *same* environment, which is what this does. Values
/// combined across P2 and R6 cannot answer it: different noise levels, different fixtures.
#[test]
#[ignore = "verification"]
fn r7_separation_passband_vs_ddc() {
    use openpulse_dsp::acquisition::IqMatchedFilter;

    println!("\nR7: signal ρ and noise ceiling in ONE environment; separation is the verdict\n");
    println!(
        "{:<10} {:>6} {:<14} {:>9} {:>9} {:>11} {:>9} {:>9} {:>11} {:>10}",
        "mode",
        "D",
        "band",
        "pb sig",
        "pb noise",
        "pb SEP",
        "dd sig",
        "dd noise",
        "dd SEP",
        "better"
    );
    for (mode, baud) in [("BPSK31", 31.25f32), ("BPSK250", 250.0)] {
        let template = bpsk_plugin::modulate::bpsk_preamble_template(&cfg(mode)).expect("template");
        let frame = bpsk_plugin::BpskPlugin::new()
            .modulate(b"separation", &cfg(mode))
            .expect("modulate");
        let occ = 2.0 * baud;
        let grid_hz = if mode == "BPSK31" { 2.0 } else { 20.0 };
        let (cutoff, d) = cutoff_and_decim(occ, grid_hz, template.len());
        let ddc = DdcMatchedFilter::new(&template, FC, FS, cutoff, d);
        let pb = IqMatchedFilter::new(template.clone());
        let ddc_grid = grid_for(template.len(), d, grid_hz);
        let step = (0.25 * FS / template.len() as f32).max(0.5);
        let n = (grid_hz / step).round() as i32;
        let pb_grid: Vec<f32> = (-n..=n).map(|k| k as f32 * step).collect();

        let win = template.len() + 400;
        for (bname, lo, hi) in [("ssb 300-2700", 300.0f32, 2_700.0)] {
            // Same noise process for both the signal case and the ceiling.
            let level = 0.05f32;
            let sig_noise = band_noise(frame.len(), lo, hi, level, 31);
            let noisy: Vec<f32> = frame.iter().zip(sig_noise).map(|(a, b)| a + b).collect();
            let pb_sig = pb
                .search_normalized_over_frequency(
                    &noisy,
                    noisy.len() - pb.len(),
                    0.05,
                    FS,
                    &pb_grid,
                )
                .map(|(r, _)| r.rho)
                .unwrap_or(f32::NAN);
            let dd_sig = ddc
                .search_normalized_over_frequency(&noisy, 0.05, &ddc_grid)
                .map(|(r, _)| r.rho)
                .unwrap_or(f32::NAN);

            let noise = band_noise(win * 60, lo, hi, level, 555);
            let (mut pmax, mut dmax) = (0.0f32, 0.0f32);
            let mut s = 0usize;
            while s + win <= noise.len() {
                let w = &noise[s..s + win];
                if let Some((r, _)) =
                    pb.search_normalized_over_frequency(w, w.len() - pb.len(), 0.05, FS, &pb_grid)
                {
                    pmax = pmax.max(r.rho);
                }
                if let Some((r, _)) = ddc.search_normalized_over_frequency(w, 0.05, &ddc_grid) {
                    dmax = dmax.max(r.rho);
                }
                s += win;
            }
            let psep = pb_sig - pmax;
            let dsep = dd_sig - dmax;
            println!(
                "{:<10} {:>6} {:<14} {:>9.3} {:>9.3} {:>11.3} {:>9.3} {:>9.3} {:>11.3} {:>10}",
                mode,
                d,
                bname,
                pb_sig,
                pmax,
                psep,
                dd_sig,
                dmax,
                dsep,
                if dsep > psep { "DDC" } else { "passband" }
            );
        }
    }
    println!("\n  If passband wins on separation, the DDC is a COST compromise that degrades");
    println!("  detection — affordable correlation of a long template, paid for in margin — and");
    println!("  the 'interference rejection is a free benefit' framing is wrong.");
}

/// R8: derive the correlation budget instead of inheriting it.
///
/// `MAX_PREAMBLE_CORRELATION_SAMPLES = 2048` has never been derived. Its doc reasons qualitatively
/// — "hundreds of correlations of an 8000-sample template" — and that reasoning was written when
/// the grid was ±20 Hz. Round 17 derived BPSK31's grid at **±2 Hz**, which is 9 points instead of
/// 81, so passband correlation of that template is ~9× cheaper than when the cap's argument was
/// made. The cap may now be excluding something affordable.
///
/// This matters beyond tidiness: the DDC exists *only* to fit that cap, and it costs detection
/// margin (R5–R7). If passband fits a real-time budget on the reference hardware, phase 0's shape
/// is wrong — the answer is a derived cap, not a decimating correlator, and the DDC becomes an
/// artifact of asking the cost question after building the answer.
///
/// Measured as wall time per settle, and as a fraction of the audio the settle covers. A receiver
/// that spends more than 1.0× real time on correlation can never catch up.
#[test]
#[ignore = "verification"]
fn r8_is_passband_affordable_at_the_derived_grid() {
    use openpulse_dsp::acquisition::IqMatchedFilter;
    use std::time::Instant;

    println!("\nR8: correlation cost per settle, passband vs DDC, at each mode's DERIVED grid\n");
    println!(
        "{:<10} {:>7} {:>8} {:>7} {:>12} {:>12} {:>12} {:>10}",
        "mode", "tmpl", "grid pts", "D", "pb ms", "ddc ms", "audio s", "pb/real"
    );
    for (mode, baud, grid_hz) in [("BPSK31", 31.25f32, 2.0f32), ("BPSK250", 250.0, 20.0)] {
        let template = bpsk_plugin::modulate::bpsk_preamble_template(&cfg(mode)).expect("template");
        let occ = 2.0 * baud;
        let (cutoff, d) = cutoff_and_decim(occ, grid_hz, template.len());
        let ddc = DdcMatchedFilter::new(&template, FC, FS, cutoff, d);
        let pb = IqMatchedFilter::new(template.clone());
        let ddc_grid = grid_for(template.len(), d, grid_hz);
        let step = (0.25 * FS / template.len() as f32).max(0.5);
        let n = (grid_hz / step).round() as i32;
        let pb_grid: Vec<f32> = (-n..=n).map(|k| k as f32 * step).collect();

        // The window a settle actually correlates over: preamble plus timing slack.
        let win_len = template.len() + 400;
        let w = band_noise(win_len, 300.0, 2_700.0, 0.05, 77);

        const REPS: usize = 20;
        let t0 = Instant::now();
        for _ in 0..REPS {
            let _ = pb.search_normalized_over_frequency(&w, w.len() - pb.len(), 0.05, FS, &pb_grid);
        }
        let pb_ms = t0.elapsed().as_secs_f64() * 1000.0 / REPS as f64;
        let t1 = Instant::now();
        for _ in 0..REPS {
            let _ = ddc.search_normalized_over_frequency(&w, 0.05, &ddc_grid);
        }
        let ddc_ms = t1.elapsed().as_secs_f64() * 1000.0 / REPS as f64;

        let audio_s = win_len as f64 / FS as f64;
        println!(
            "{:<10} {:>7} {:>8} {:>7} {:>12.2} {:>12.2} {:>12.3} {:>10.4}",
            mode,
            template.len(),
            pb_grid.len(),
            d,
            pb_ms,
            ddc_ms,
            audio_s,
            (pb_ms / 1000.0) / audio_s
        );
    }
    println!("\n  'pb/real' is passband correlation time divided by the audio it covers, on THIS");
    println!(
        "  host. Well under 1.0 means the cap is not protecting against what its doc says, and"
    );
    println!("  the DDC's margin cost is being paid for nothing. The reference target is slower —");
    println!("  rpi53-class — so the margin needed here is the ratio between the two, unmeasured.");
}

/// R9: the denominator — how often does the veto actually fire per second of audio?
///
/// R8 divided per-invocation cost by the template's own duration, which silently assumes one veto
/// per template-length of audio. That is the quiet channel. The veto exists for the opposite case:
/// on a saturated floor the settle/veto cycle re-fires as the scan crawls, so the rate is set by
/// the condemnation stride (4 symbol periods) and the energy gate's re-trigger, not by the template
/// length. At BPSK31 four symbols is 128 ms, i.e. ~8 vetoes per audio-second — which turns R8's
/// 2.2 % into ~18 % on this host before any reference-hardware ratio is applied.
///
/// This is the third time in this investigation that a conclusion ran past its denominator, so the
/// denominator gets measured rather than assumed.
#[test]
#[ignore = "verification"]
fn r9_veto_rate_on_a_hot_floor() {
    use openpulse_audio::loopback::LoopbackBackend;
    use openpulse_core::fec::FecMode;
    use openpulse_modem::capture_replay::load_corpus;
    use openpulse_modem::engine::ModemEngine;
    use std::time::Duration;

    let hot = load_corpus("ic9700-idle-hot.wav").expect("corpus");
    let audio_s = hot.samples.len() as f64 / FS as f64;

    println!("\nR9: veto invocations per second of audio, recorded hot idle floor");
    println!(
        "    capture {:.2} s; the regime the veto exists for, not the quiet one\n",
        audio_s
    );
    println!(
        "{:<10} {:>10} {:>10} {:>12} {:>14} {:>12} {:>12}",
        "mode", "accepted", "rejected", "total", "per audio-s", "pb ms each", "pb/real"
    );

    // Per-invocation cost from R8, at this mode's derived grid. BPSK250 only: it is the one mode
    // that publishes a template, so it is the only one whose rate the engine will actually produce
    // without defeating the publishing guard.
    {
        let (mode, pb_ms) = ("BPSK250", 6.61f64);
        let backend = LoopbackBackend::new();
        let mut e = ModemEngine::new(Box::new(backend.clone_shared()));
        e.register_plugin(Box::new(bpsk_plugin::BpskPlugin::new()))
            .expect("register");
        e.set_deterministic_scan_positions(Some(3_000));
        e.set_deterministic_max_iterations(Some(4_000));
        backend.fill_samples(&hot.samples);
        let _ =
            e.receive_with_fec_mode_timeout(mode, FecMode::Rs, None, Duration::from_millis(60_000));
        let acc = e.rho_accepted_settles();
        let rej = e.rho_rejected_settles();
        let total = acc + rej;
        let rate = total as f64 / audio_s;
        println!(
            "{:<10} {:>10} {:>10} {:>12} {:>14.1} {:>12.2} {:>12.3}",
            mode,
            acc,
            rej,
            total,
            rate,
            pb_ms,
            rate * pb_ms / 1000.0
        );
    }
    println!("\n  'pb/real' here is the honest budget fraction: invocation rate x cost per");
    println!("  invocation. R8's figure divided by template duration instead, which is the");
    println!("  quiet-channel rate and the most favourable one. A value near or above 1.0 means");
    println!("  the correlation cannot keep up in the regime it was built for.");
}

/// R10: in the band that SETS the threshold, do the two correlators converge?
///
/// The limits note on the BPSK31 elimination said passband would reopen the gap, citing 0.066 vs
/// 0.326 — **wide-band** numbers. Those do not transfer to the band that decides anything. In the
/// 60 Hz worst case (±30 Hz) both BPSK31's template band (±~20 Hz) and the DDC passband (±42.6 Hz)
/// *contain* the noise entirely: the lowpass removes nothing, and the passband correlator's
/// denominator holds the same energy. The two should converge — the same mechanism that made the
/// passband advantage collapse from 2.5× to 9 % under a 200 Hz filter in R5.
///
/// If they do, the elimination is **correlator-independent** and its headline needs no scoping.
#[test]
#[ignore = "verification"]
fn r10_do_correlators_converge_in_the_deciding_band() {
    use openpulse_dsp::acquisition::IqMatchedFilter;

    let template = bpsk_plugin::modulate::bpsk_preamble_template(&cfg("BPSK31")).expect("template");
    let occ = 2.0 * 31.25f32;
    let grid_hz = 2.0f32;
    let (cutoff, d) = cutoff_and_decim(occ, grid_hz, template.len());
    let ddc = DdcMatchedFilter::new(&template, FC, FS, cutoff, d);
    let pb = IqMatchedFilter::new(template.clone());
    let ddc_grid = grid_for(template.len(), d, grid_hz);
    let step = (0.25 * FS / template.len() as f32).max(0.5);
    let n = (grid_hz / step).round() as i32;
    let pb_grid: Vec<f32> = (-n..=n).map(|k| k as f32 * step).collect();

    println!(
        "\nR10: BPSK31 noise ceiling by correlator, per band (DDC passband ±{cutoff:.1} Hz)\n"
    );
    println!(
        "{:<22} {:>12} {:>12} {:>12}",
        "band", "passband max", "DDC max", "difference"
    );
    let win = template.len() + 400;
    for (name, lo, hi) in [
        ("white 0-4k", 0.0f32, 4_000.0),
        ("ssb 300-2700", 300.0, 2_700.0),
        ("60 Hz (decides)", 1_470.0, 1_530.0),
    ] {
        let noise = band_noise(win * 120, lo, hi, 0.05, 4242);
        let (mut pmax, mut dmax) = (0.0f32, 0.0f32);
        let mut s = 0usize;
        while s + win <= noise.len() {
            let w = &noise[s..s + win];
            if let Some((r, _)) =
                pb.search_normalized_over_frequency(w, w.len() - pb.len(), 0.05, FS, &pb_grid)
            {
                pmax = pmax.max(r.rho);
            }
            if let Some((r, _)) = ddc.search_normalized_over_frequency(w, 0.05, &ddc_grid) {
                dmax = dmax.max(r.rho);
            }
            s += win;
        }
        println!("{name:<22} {pmax:>12.3} {dmax:>12.3} {:>12.3}", dmax - pmax);
    }
    println!("\n  Convergence in the deciding band means the elimination does not depend on which");
    println!(
        "  correlator runs. Divergence there would mean the headline must be scoped to the DDC."
    );
}

/// R6: does a PN successor actually remove the grid constraint that blocks BPSK31? (#1062)
///
/// **This tests a PREDICTION the issue body explicitly refuses to assume.** The `baud/4` grid bound
/// exists *because* the shipped preamble is a period-4 run with discrete spectral lines: a grid wide
/// enough to rotate some line onto an arbitrary tone lets the veto corroborate that tone. The body
/// says a spread sequence "has no such line structure, so the constraint that blocks BPSK31/63 would
/// not carry over in that form — treat that as a prediction to test in the design pass, not as a
/// settled benefit. The replacement's own grid bound has to be derived, not assumed absent."
///
/// So this runs R1's sweep unchanged over BOTH templates at BPSK31. The shipped column is the
/// positive control: if it does not reproduce R1's published numbers (worst tone ~0.70, 100 %
/// vulnerable at ±20 Hz) then this harness is not measuring what R1 measured and the PN column means
/// nothing.
///
/// Templates are proper maximal-length sequences (balance-checked), built at the plugin's own symbol
/// rate — see `the_pn_generator_uses_the_shipped_symbol_rate`, which fails if that drifts.
///
/// **The sweep is +/-60 Hz, not R1's one line spacing.** R1 sweeps 0..baud/2 and justifies it with
/// "the pattern repeats in position" — true for a period-4 run, whose spectrum IS periodic lines
/// every baud/2, and false for PN, whose spectrum is a continuous main lobe out to +/-baud. Reusing
/// R1's range for PN would report the maximum over a 15.6 Hz sliver and call it the worst tone.
///
/// Two grids rather than R1's four: the narrow sweep already showed PN's response is flat across
/// 20/6/2/0.5 Hz, which is the grid-independence this measures; the widest and a narrow one keep
/// that check while the wide sweep costs 4x the correlations.
/// Runtime ~78 min: eight rows, each sweeping 241 tone frequencies through a DDC correlation over a
/// template up to 32 k samples. `#[ignore]`d accordingly.
#[test]
#[ignore = "verification (#1062), ~78 min"]
fn r6_does_a_pn_template_remove_the_grid_constraint() {
    let baud = 31.25f32;
    let occ = 2.0 * baud;
    let first_line = baud / 4.0;

    let shipped =
        bpsk_plugin::modulate::bpsk_preamble_template(&cfg("BPSK31")).expect("shipped template");
    let sym = shipped.len() / (FS / baud).round() as usize;
    // Same length as the shipped template: that isolates the SEQUENCE, which is the variable this
    // prediction is about. But 31 symbols is the N=31 the thread vetted and REJECTED — the design
    // sets N >= 63 — so the deployed length is measured too, and the two answer different questions.
    // Proper maximal-length sequences, not prefixes of a longer one: truncation destroys the flat
    // autocorrelation that is the reason to choose PN at all, and would understate it. N is the CHIP
    // count, matching the thread's notation — a 31-chip sequence yields a 30-symbol template.
    let pn31 = common::preamble::pn_template("BPSK31", &common::preamble::m_sequence_of_len(31))
        .expect("pn31 template");
    let pn63 = common::preamble::pn_template("BPSK31", &common::preamble::m_sequence_of_len(63))
        .expect("pn63 template");
    // A third length, so any scaling statement rests on more than two points.
    let pn127 = common::preamble::pn_template("BPSK31", &common::preamble::m_sequence_of_len(127))
        .expect("pn127 template");
    // N=255 is deliberately NOT measured. It was added to separate two candidate scaling laws in N,
    // and that question died when the D column went in: `cutoff_and_decim` sets D = ceil(len/2048),
    // so D runs 4/8/16 across these rows and the decimated template stays ~2000 samples. The rows
    // are not a clean sweep in N, so no law can be read off them however many points are added — and
    // a 65k-sample template costs about an hour per grid to sweep. Holding D constant is a different
    // experiment; this one answers the grid question.

    println!("\nR6: does a spread sequence remove BPSK31's grid constraint?");
    println!(
        "    shipped {} samples ({sym} sym), PN-31 {}, PN-63 {}, PN-127 {}; lines at odd multiples of {first_line:.2} Hz",
        shipped.len(),
        pn31.len(),
        pn63.len(),
        pn127.len()
    );
    println!(
        "\n{:>10} {:>8} {:>6} {:>8} {:>12} {:>14} {:>12}",
        "template", "grid", "D", "dec len", "worst tone", "vulnerable %", "verdict"
    );

    for (name, template) in [
        ("shipped", &shipped),
        ("PN-31", &pn31),
        ("PN-63", &pn63),
        ("PN-127", &pn127),
    ] {
        for grid_hz in [20.0f32, 2.0] {
            let (cutoff, d) = cutoff_and_decim(occ, grid_hz, template.len());
            let mf = DdcMatchedFilter::new(template, FC, FS, cutoff, d);
            let grid = grid_for(template.len(), d, grid_hz);

            let mut worst = 0.0f32;
            let (mut over, mut pts) = (0usize, 0usize);
            let mut f = -60.0f32;
            while f <= 60.0 {
                let tone: Vec<f32> = (0..template.len() + 400)
                    .map(|k| 0.25 * (2.0 * PI * (FC + f) * k as f32 / FS).cos())
                    .collect();
                if let Some((r, _)) = mf.search_normalized_over_frequency(&tone, 0.05, &grid) {
                    worst = worst.max(r.rho);
                    if r.rho >= 0.40 {
                        over += 1;
                    }
                    pts += 1;
                }
                f += 0.5;
            }
            let vulnerable = 100.0 * over as f32 / pts.max(1) as f32;
            let verdict = if worst < 0.40 {
                "no tone clears 0.40"
            } else if grid_hz >= first_line {
                "grid reaches a line"
            } else {
                "residual bands"
            };
            println!(
                "{name:>10} {grid_hz:>7.1} {d:>6} {:>8} {worst:>12.3} {vulnerable:>13.0}% {verdict:>12}",
                template.len() / d,
            );
            let _ = cutoff;
        }
    }
    println!("\n  D and 'dec len' are printed because they are NOT constant across these rows.");
    println!("  `cutoff_and_decim` sets D = ceil(tmpl_len / 2048), so the DECIMATED template is");
    println!(
        "  ~2000 samples for every N: the correlator's sample budget is fixed and the time it"
    );
    println!(
        "  spans grows. That is what would ship, but it means these rows are not a clean sweep"
    );
    println!("  in N, and no scaling law should be read off them without holding D constant.");
    println!(
        "\n  The shipped rows check the LOOP is unchanged from R1 (~0.70 worst tone). They are"
    );
    println!("  a self-consistency check, NOT a control for the PN rows: neither the symbol-rate");
    println!("  defect nor R1's narrow sweep touched the shipped template, so reproduction there");
    println!("  could not have revealed either. 0.40 is a BPSK250 32-symbol `--++` decode-cliff");
    println!("  constant; no PN template has derived one, so worst-tone rho is the primary figure");
    println!(
        "  and the percentage is derivative. The interferer class is a single noise-free tone."
    );
}

/// The PN generator builds at the SHIPPED symbol rate — asserted in the default gate, at BPSK31.
///
/// `pn_template` used to derive samples-per-symbol by parsing digits out of the mode string, which
/// gives 258 at "BPSK31" where the plugin uses 256 (`parse_baud_rate` maps "31" to 31.25). Every PN
/// template at this baud was then a quarter-symbol late and drifting two samples per symbol. It was
/// invisible at BPSK250 because 8000/250 is exact — so the check has to run at a mode where it is
/// not, and it has to run by DEFAULT: a comment cannot fail, and this is the construct CLAUDE.md
/// rule 5 bans.
///
/// A lag-search correlation would NOT catch it: the offset is exactly the 64-sample lag pad, so a
/// peak search finds the template anyway. The length is the falsifiable thing.
#[test]
fn the_pn_generator_uses_the_shipped_symbol_rate() {
    let c = cfg("BPSK31");
    let shipped = bpsk_plugin::modulate::bpsk_preamble_template(&c).expect("shipped");
    let syms = bpsk_plugin::modulate::PREAMBLE_SYMS - 1;
    let sps = shipped.len() / syms;
    assert_eq!(sps, 256, "BPSK31 runs at 31.25 baud = 256 sps, not 8000/31");

    // A 32-chip run yields 31 symbols, the shipped template's own symbol count.
    let built = common::preamble::pn_template("BPSK31", &[1.0f32; 32]).expect("pn");
    assert_eq!(
        built.len(),
        shipped.len(),
        "the PN generator built {} samples where the shipped template is {} — it is not using the \
         plugin's symbol rate, so every PN measurement at this baud is a drifting slice",
        built.len(),
        shipped.len()
    );
}
