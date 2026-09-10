//! BPSK modulator.
//!
//! The modulation pipeline is:
//!
//! ```text
//! bytes → bits (LSB-first) → NRZI encode → symbols (+1/−1)
//!       → overlapping half-Hann crossfade → carrier mix → audio samples
//! ```

use std::f32::consts::PI;

use openpulse_core::error::ModemError;
use openpulse_core::plugin::{ModulationConfig, PulseShape};
use openpulse_dsp::filter::FirFilter;
use openpulse_dsp::rrc::generate_rrc_coefficients;

use crate::parse_baud_rate;

/// Number of preamble symbols prepended to every transmission.
pub const PREAMBLE_SYMS: usize = 32;
/// Number of tail symbols appended after data to let the signal decay.
pub const TAIL_SYMS: usize = 8;
pub(crate) const RRC_SPAN_SYMBOLS: usize = 8;

// ── Public entry point ────────────────────────────────────────────────────────

fn rrc_alpha_for(config: &ModulationConfig) -> Option<f32> {
    if let PulseShape::Rrc { alpha } = config.pulse_shape {
        Some(alpha)
    } else if config.mode.ends_with("-RRC") {
        Some(0.35f32)
    } else {
        None
    }
}

/// Compute the shaped baseband amplitude envelope for BPSK.
///
/// Returns the Hann-crossfaded (or RRC-impulse) baseband signal before
/// carrier multiplication.  For Hann path, the carrier would be
/// `out[k] * cos(2π·fc·k/fs)`.  For RRC path, caller must apply the RRC
/// FIR filter and then upconvert.
fn bpsk_baseband(data: &[u8], config: &ModulationConfig) -> Result<Vec<f32>, ModemError> {
    bpsk_baseband_with_preamble(data, config, &preamble_bits(PREAMBLE_SYMS))
}

/// The preamble bit pattern the BPSK wire format prepends to every frame.
///
/// Single source of truth: the Hann/RRC modulator, the GPU modulator and the
/// demodulator's expected-symbol table all derive from this function, so a
/// sequence change cannot desynchronise one end from the other.
pub fn preamble_bits(len: usize) -> Vec<bool> {
    (0..len).map(|i| i % 2 == 0).collect()
}

/// [`bpsk_baseband`] with the preamble supplied, for wire-format vetting.
fn bpsk_baseband_with_preamble(
    data: &[u8],
    config: &ModulationConfig,
    preamble: &[bool],
) -> Result<Vec<f32>, ModemError> {
    let baud = parse_baud_rate(&config.mode)?;
    let fs = config.sample_rate as f32;
    let n = samples_per_symbol(fs, baud)?;

    let mut bits: Vec<bool> = preamble.to_vec();
    bits.extend(bytes_to_bits(data));
    bits.extend(std::iter::repeat_n(false, TAIL_SYMS));

    let symbols = nrzi_encode(&bits);
    let total = symbols.len() * n;
    let mut out = vec![0.0f32; total];

    for (sym_idx, &phase_neg) in symbols.iter().enumerate() {
        let a_curr = if phase_neg { -1.0f32 } else { 1.0f32 };
        let sym_start = sym_idx * n;

        if rrc_alpha_for(config).is_some() {
            out[sym_start] = a_curr;
        } else {
            let a_next = symbols
                .get(sym_idx + 1)
                .map(|&neg| if neg { -1.0f32 } else { 1.0f32 })
                .unwrap_or(0.0f32);
            for i in 0..n {
                let w_tail = 0.5 * (1.0 + (PI * i as f32 / n as f32).cos());
                let w_head = 1.0 - w_tail;
                out[sym_start + i] = a_curr * w_tail + a_next * w_head;
            }
        }
    }
    Ok(out)
}

/// Modulate `data` bytes to a vector of normalised PCM samples.
pub fn bpsk_modulate(data: &[u8], config: &ModulationConfig) -> Result<Vec<f32>, ModemError> {
    bpsk_modulate_with_preamble(data, config, &preamble_bits(PREAMBLE_SYMS))
}

/// [`bpsk_modulate`] with the preamble supplied, for wire-format vetting.
pub fn bpsk_modulate_with_preamble(
    data: &[u8],
    config: &ModulationConfig,
    preamble: &[bool],
) -> Result<Vec<f32>, ModemError> {
    let baud = parse_baud_rate(&config.mode)?;
    let fs = config.sample_rate as f32;
    let fc = config.center_frequency;
    let n = samples_per_symbol(fs, baud)?;

    let bb = bpsk_baseband_with_preamble(data, config, preamble)?;

    // Apply carrier: real output = I_bb * cos(2π·fc·t), Q = 0.
    let cps = fc as f64 / fs as f64;
    if let Some(alpha) = rrc_alpha_for(config) {
        let num_taps = RRC_SPAN_SYMBOLS * n + 1;
        let coeffs = generate_rrc_coefficients(fs, baud, alpha, num_taps);
        let group_delay = (num_taps - 1) / 2;
        let mut fir = FirFilter::new(coeffs);
        let padded: Vec<f32> = bb
            .iter()
            .copied()
            .chain(std::iter::repeat_n(0.0, group_delay))
            .collect();
        let filtered = fir.apply(&padded);
        Ok(filtered[group_delay..]
            .iter()
            .enumerate()
            .map(|(k, &bb)| bb * carrier(k, cps))
            .collect())
    } else {
        Ok(bb
            .iter()
            .enumerate()
            .map(|(k, &amp)| amp * carrier(k, cps))
            .collect())
    }
}

/// Carrier `cos(2π·fc·k/fs)`, with the phase reduced to a single cycle before the cosine.
///
/// Evaluating `(2π·fc·k/fs).cos()` directly lets the argument reach ~2e5 rad on a slow-rung
/// frame (BPSK31 runs ~2.2e5 samples), where an f32 argument resolves only ~0.015 rad — so
/// the carrier drifts from truth by an amount that grows with frame length and is worst on
/// the longest, slowest rungs. Reducing the *cycle count* in f64 first keeps the fractional
/// part exact: at 1e6 samples the count is ~2e5 cycles, where f64 resolves ~3e-11 of a cycle.
#[inline]
fn carrier(k: usize, cycles_per_sample: f64) -> f32 {
    let cycles = cycles_per_sample * k as f64;
    (std::f64::consts::TAU * (cycles - cycles.floor())).cos() as f32
}

/// Return baseband I and Q samples for BPSK (Q is always zero).
///
/// BPSK is a purely in-phase modulation: the baseband I channel carries the
/// shaped amplitude envelope (±1 after NRZI) and the Q channel is identically
/// zero.
pub fn bpsk_modulate_iq(
    data: &[u8],
    config: &ModulationConfig,
) -> Result<(Vec<f32>, Vec<f32>), ModemError> {
    let baud = parse_baud_rate(&config.mode)?;
    let fs = config.sample_rate as f32;
    let n = samples_per_symbol(fs, baud)?;

    let mut i_bb = bpsk_baseband(data, config)?;
    if let Some(alpha) = rrc_alpha_for(config) {
        let num_taps = RRC_SPAN_SYMBOLS * n + 1;
        let coeffs = generate_rrc_coefficients(fs, baud, alpha, num_taps);
        let group_delay = (num_taps - 1) / 2;
        let mut fir = FirFilter::new(coeffs);
        let padded: Vec<f32> = i_bb
            .iter()
            .copied()
            .chain(std::iter::repeat_n(0.0, group_delay))
            .collect();
        let filtered = fir.apply(&padded);
        i_bb = filtered[group_delay..].to_vec();
    }
    let q_bb = vec![0.0f32; i_bb.len()];
    Ok((i_bb, q_bb))
}

/// Minimum normalised preamble correlation ρ for a settle to be believed (#1049).
///
/// **Derived from the decode cliff, not from two captures.** The number in issue #1049 (0.40 from a
/// real frame at ρ = 0.811 against a hot idle floor at 0.182) came from one capture whose carrier
/// offset happened to be +1.2 Hz; taken alone it is an artifact-calibrated constant. Measured
/// in-repo 2026-07-31, BPSK250 + `Rs` through AWGN, ρ against decode outcome:
///
/// | SNR | −1 dB | −3 dB | −5 dB | −7 dB | −9 dB |
/// |---|---|---|---|---|---|
/// | ρ | 0.646 | 0.561 | 0.455 | 0.392 | 0.322 |
/// | decode | OK | OK | fail | fail | fail |
///
/// The decode dies between −3 and −5 dB while ρ is still ≈ 0.5, and ρ only reaches this threshold
/// **two SNR steps past** the last frame the demodulator can actually decode — so the gate cannot
/// reject a decodable frame before the channel already has. On the false-accept side, recorded idle
/// audio settled and correlated the same way peaks at ρ = 0.205 (`ic9700-idle-hot.wav`) and 0.205
/// (`ft991a-idle.wav`), so this sits ~2× above measured noise and ~1.4× below the weakest decodable
/// frame. Watterson `moderate_f1` frames measured ρ = 0.58–0.84 across 10–30 dB, including runs that
/// failed to decode — a fade does not push a real preamble under this line before it stops being a
/// frame.
///
/// **Margin, stated honestly.** The reference point is the real on-air frame at ρ = 0.654 measured
/// the way the engine measures it, not the 0.811 in issue #1049 (that came from a whole-capture
/// search rather than a settled onset). Against recorded idle that is 3.2×, not the issue's 4.4×.
///
/// **This is a broadband-noise discriminator and nothing more.** It says "a preamble is here",
/// against a *noise* floor. It cannot rule out a structured interferer in general, so it does not
/// retire the settle-condemnation recovery (#1021, #1040), which remains the backstop for anything
/// that correlates but does not decode — and which ablation confirms is load-bearing: removing it
/// fails all three leads of the saturating-floor reproduction.
///
/// What would falsify it: a mode or channel where a frame decodes at ρ below this. Re-measure the
/// table per waveform family before extending the template beyond BPSK.
pub const PREAMBLE_RHO_THRESHOLD: f32 = 0.40;

/// The ρ a **delivered** BPSK250 frame is known to reach — the bound above which the runtime
/// calibration stands the veto down instead of raising the threshold further (#1060, REQ-RX-03).
///
/// **Provisional, and marked as such deliberately.** Measured 2026-08-17 by
/// `preamble_rho_fade_and_filter_probe::f9_decode_conditioned_rho_tail` with the veto disabled on
/// the receiver (`F9_VETO=off`, so the conditioned set is the channel's choice and not the
/// measurement's own subject — the arm #1088 recorded as missing, which made every earlier
/// decoded-only column a tautology): through a 1250–1750 Hz mask on `moderate_f1`, 60 seeds at each
/// of 5/10/20 dB, **no delivered frame scored below 0.50** and the lowest was 0.618.
///
/// It is set at the *measured floor of the delivered population*, not at that 0.618 minimum, because
/// a min-of-60 is an extreme value whose size is set by the observation budget rather than by the
/// signal.
///
/// **What would falsify it:** a delivered frame below 0.50 on any channel this mode runs on. The
/// measurement behind it is one channel model, one band, one payload size, and a brick-wall mask
/// sharper than a real receive filter. A decode-conditioned sweep across bands is tracked in
/// **#1337** (it was #1059, which is titled for the QPSK thresholds and does not own this). #1337
/// also records that 600 UNCONDITIONED seeds of the shipped template already reach 0.491 through a
/// 1250-1750 Hz mask at 6 dB — not a falsification, since those windows' decode is unknown, but the
/// 0.618 support figure is a min-of-180 and does not bound what it is used to bound;
/// until then a station whose noise pushes the derived threshold past this stands the veto down,
/// which is the conservative direction — it reverts to energy-only detection rather than discarding
/// frames.
pub const DELIVERED_FRAME_RHO_BOUND: f32 = 0.50;

/// Half-width of the residual-frequency grid the preamble correlation searches, in Hz.
///
/// **Bounded from both sides, and the upper bound is the interesting one.**
///
/// Below: the AFC settle is what estimates frequency, so this only has to cover what the settle
/// leaves behind. Measured residual after `afc_mini_settle` over a 1056-sample window is ≤ 0.3 Hz
/// for every true offset the engine can reach (0 to 400 Hz; past that `AFC_MAX_CORRECTION_HZ`
/// rejects the settle before this check runs at all). ±20 Hz is already generous.
///
/// Above: **the grid must stay well inside ±baud/4, or the gate stops discriminating.** The 32
/// preamble *bits* alternate, but `nrzi_encode` flips phase only on a `1`, so the *symbols* are
/// `--++` repeating — a square wave of period **four symbols**, not two. Its energy therefore sits
/// in lines at `fc ± baud/4` plus odd harmonics: measured at BPSK250, ±62.5 Hz at 0 dB, ±187.5 at
/// −14 dB, ±312.5 at −31 dB, and **nothing at ±125**. Rotate the template onto a line and it lands
/// on plain carrier, so a steady tone starts scoring like a preamble. Measured against a pure tone
/// (`the_gate_is_not_fooled_by_a_steady_tone`):
///
/// | grid half-width | tone BETWEEN lines | tone ON a line (`fc ± 62.5`) |
/// |---|---|---|
/// | ±20 Hz (shipped) | 0.017–0.042 | **0.700** |
/// | ±160 Hz | **0.659** | 0.700 |
/// | ±450 Hz (the full acquisition range) | **0.696 at every frequency** | 0.700 |
///
/// **The second column is the one that was missing, and it does not depend on the grid at all.**
/// A tone landing on one of the two dominant lines captures roughly half the template's energy —
/// ρ ≈ √0.5 — however narrow the grid is, because no rotation is needed. Narrowing the grid narrows
/// the *vulnerable bands* around each line (measured ±25 Hz at the shipped width: 1415–1465 and
/// 1535–1585 Hz for BPSK250 at fc = 1500) but cannot remove them.
///
/// This matters less than it looks in the deployed chain and more than it looks on the bench: for a
/// *lone* tone the AFC settle lands on the tone first, which parks it ~baud/4 from both rotated
/// lines, and the veto then refuses it (`preamble_veto_interference`, 5/5 decodes). Interference
/// whose apparent carrier sits at fc with sidebands *on* the lines — hum-modulated carriers,
/// suppressed-carrier pairs, birdie combs — gets no such protection. See #1062.
///
/// A birdie at 0.66 outscores this receiver's best real on-air frame (0.654). That kills the
/// otherwise-attractive design of running the grid over the whole ±450 Hz acquisition range as a
/// *detector* and seeding the settle from it (codec2's ordering): our sync word is two spectral
/// lines, not a pseudo-random sequence, so it cannot survive being searched over its own line
/// spacing. Widening this constant is not a cost/benefit trade against compute — past ~baud/4 it
/// destroys the thing being bought. Re-derive it from `baud` before extending to another waveform.
pub const PREAMBLE_RHO_GRID_HZ: f32 = 20.0;

/// The modulated preamble alone, for correlation-based frame detection.
///
/// Built by modulating an empty payload and keeping the preamble span, so it is produced by the
/// **same** code path as a real transmission — pulse shaping, NRZI and carrier included. A
/// hand-rolled copy would drift out of step with the modulator the first time either changes, and
/// a template that no longer matches the wire silently stops detecting frames rather than failing.
///
/// The last preamble symbol is dropped on the crossfade path: the rectangular pulse blends each
/// symbol into the next (see the crossfade-ISI sharp edge), so the final symbol period carries a
/// third of the *first data* symbol, which differs frame to frame.
pub fn bpsk_preamble_template(config: &ModulationConfig) -> Result<Vec<f32>, ModemError> {
    let baud = parse_baud_rate(&config.mode)?;
    let n = samples_per_symbol(config.sample_rate as f32, baud)?;
    let full = bpsk_modulate(&[], config)?;
    let span = n * (PREAMBLE_SYMS - 1);
    if full.len() < span {
        return Err(ModemError::Demodulation(
            "modulated preamble shorter than its own symbol span".into(),
        ));
    }
    Ok(full[..span].to_vec())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Convert bytes to LSB-first bits.
pub(crate) fn bytes_to_bits(bytes: &[u8]) -> Vec<bool> {
    let mut bits = Vec::with_capacity(bytes.len() * 8);
    for &b in bytes {
        for shift in 0..8u8 {
            bits.push((b >> shift) & 1 == 1);
        }
    }
    bits
}

/// NRZI encoding: bit `true` ("1") → flip phase; `false` ("0") → keep phase.
/// Returns `true` for negative phase (180°), `false` for positive (0°).
pub(crate) fn nrzi_encode(bits: &[bool]) -> Vec<bool> {
    let mut phase_neg = false;
    bits.iter()
        .map(|&flip| {
            if flip {
                phase_neg = !phase_neg;
            }
            phase_neg
        })
        .collect()
}

/// Compute integer samples-per-symbol, returning an error when the ratio
/// would be less than 4 (DSP cannot work reliably below that).
pub(crate) fn samples_per_symbol(sample_rate: f32, baud: f32) -> Result<usize, ModemError> {
    let n = (sample_rate / baud).round() as usize;
    if n < 4 {
        return Err(ModemError::Configuration(format!(
            "sample rate {sample_rate} Hz is too low for {baud} baud \
             (need at least 4 samples/symbol)"
        )));
    }
    Ok(n)
}

/// GPU-accelerated modulation: byte→bit→NRZI on CPU, sample rendering on GPU.
#[cfg(feature = "gpu")]
pub fn bpsk_modulate_with_gpu(
    data: &[u8],
    config: &ModulationConfig,
    ctx: &openpulse_gpu::GpuContext,
) -> Result<Vec<f32>, ModemError> {
    // RRC path requires FIR filtering; fall back to CPU.
    if matches!(config.pulse_shape, PulseShape::Rrc { .. }) || config.mode.ends_with("-RRC") {
        return bpsk_modulate(data, config);
    }

    let baud = parse_baud_rate(&config.mode)?;
    let fs = config.sample_rate as f32;
    let fc = config.center_frequency;
    let n = samples_per_symbol(fs, baud)?;

    let mut bits: Vec<bool> = preamble_bits(PREAMBLE_SYMS);
    bits.extend(bytes_to_bits(data));
    bits.extend(std::iter::repeat_n(false, TAIL_SYMS));

    let symbols = nrzi_encode(&bits);
    match openpulse_gpu::bpsk_modulate_gpu(ctx, &symbols, n, fc, fs) {
        Some(out) => Ok(out),
        None => bpsk_modulate(data, config),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::{Pod, Zeroable};
    use openpulse_core::plugin::ModulationConfig;

    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable)]
    struct Params {
        input_len: u32,
        output_len: u32,
        _pad0: u32,
        _pad1: u32,
    }

    async fn gpu_bits_lsb_from_bytes(bytes: &[u8]) -> Option<Vec<u32>> {
        let instance = wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await?;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("bpsk-gpu-bits-device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                },
                None,
            )
            .await
            .ok()?;

        let input_u32: Vec<u32> = bytes.iter().map(|b| *b as u32).collect();
        let output_len = input_u32.len() * 8;

        let input_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bits-input"),
            size: (input_u32.len() * std::mem::size_of::<u32>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&input_buf, 0, bytemuck::cast_slice(&input_u32));

        let output_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bits-output"),
            size: (output_len * std::mem::size_of::<u32>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });

        let readback_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bits-readback"),
            size: (output_len * std::mem::size_of::<u32>()) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let params = Params {
            input_len: input_u32.len() as u32,
            output_len: output_len as u32,
            _pad0: 0,
            _pad1: 0,
        };
        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bits-params"),
            size: std::mem::size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&params_buf, 0, bytemuck::bytes_of(&params));

        let shader_src = r#"
struct Params {
    input_len: u32,
    output_len: u32,
    _pad0: u32,
    _pad1: u32,
};

@group(0) @binding(0) var<storage, read> in_bytes: array<u32>;
@group(0) @binding(1) var<storage, read_write> out_bits: array<u32>;
@group(0) @binding(2) var<uniform> params: Params;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let idx = gid.x;
    if (idx >= params.output_len) {
        return;
    }
    let byte_idx = idx / 8u;
    let bit_idx = idx % 8u;
    out_bits[idx] = (in_bytes[byte_idx] >> bit_idx) & 1u;
}
"#;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("bits-kernel"),
            source: wgpu::ShaderSource::Wgsl(shader_src.into()),
        });

        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("bits-pipeline"),
            layout: None,
            module: &shader,
            entry_point: "main",
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bits-bind-group"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: input_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: output_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params_buf.as_entire_binding(),
                },
            ],
        });

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("bits-encoder"),
        });

        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("bits-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            let workgroups = (output_len as u32).div_ceil(64);
            pass.dispatch_workgroups(workgroups, 1, 1);
        }

        encoder.copy_buffer_to_buffer(
            &output_buf,
            0,
            &readback_buf,
            0,
            (output_len * std::mem::size_of::<u32>()) as u64,
        );
        queue.submit(Some(encoder.finish()));

        let slice = readback_buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        device.poll(wgpu::Maintain::Wait);
        rx.recv().ok()?.ok()?;

        let data = slice.get_mapped_range();
        let out: Vec<u32> = bytemuck::cast_slice::<u8, u32>(&data).to_vec();
        drop(data);
        readback_buf.unmap();
        Some(out)
    }

    async fn gpu_symbols_from_bits(bits: &[u32]) -> Option<Vec<f32>> {
        let instance = wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await?;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("bpsk-gpu-syms-device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                },
                None,
            )
            .await
            .ok()?;

        let input_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("syms-input"),
            size: std::mem::size_of_val(bits) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&input_buf, 0, bytemuck::cast_slice(bits));

        let output_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("syms-output"),
            size: (bits.len() * std::mem::size_of::<f32>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });

        let readback_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("syms-readback"),
            size: (bits.len() * std::mem::size_of::<f32>()) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let params = Params {
            input_len: bits.len() as u32,
            output_len: bits.len() as u32,
            _pad0: 0,
            _pad1: 0,
        };
        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("syms-params"),
            size: std::mem::size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&params_buf, 0, bytemuck::bytes_of(&params));

        let shader_src = r#"
struct Params {
    input_len: u32,
    output_len: u32,
    _pad0: u32,
    _pad1: u32,
};

@group(0) @binding(0) var<storage, read> in_bits: array<u32>;
@group(0) @binding(1) var<storage, read_write> out_syms: array<f32>;
@group(0) @binding(2) var<uniform> params: Params;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let idx = gid.x;
    if (idx >= params.output_len) {
        return;
    }
    out_syms[idx] = select(1.0, -1.0, in_bits[idx] == 1u);
}
"#;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("syms-kernel"),
            source: wgpu::ShaderSource::Wgsl(shader_src.into()),
        });

        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("syms-pipeline"),
            layout: None,
            module: &shader,
            entry_point: "main",
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("syms-bind-group"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: input_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: output_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params_buf.as_entire_binding(),
                },
            ],
        });

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("syms-encoder"),
        });

        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("syms-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            let workgroups = (bits.len() as u32).div_ceil(64);
            pass.dispatch_workgroups(workgroups, 1, 1);
        }

        encoder.copy_buffer_to_buffer(
            &output_buf,
            0,
            &readback_buf,
            0,
            (bits.len() * std::mem::size_of::<f32>()) as u64,
        );
        queue.submit(Some(encoder.finish()));

        let slice = readback_buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        device.poll(wgpu::Maintain::Wait);
        rx.recv().ok()?.ok()?;

        let data = slice.get_mapped_range();
        let out: Vec<f32> = bytemuck::cast_slice::<u8, f32>(&data).to_vec();
        drop(data);
        readback_buf.unmap();
        Some(out)
    }

    #[test]
    fn bytes_to_bits_lsb_first() {
        let bits = bytes_to_bits(&[0b10110001]);
        assert_eq!(
            bits,
            vec![true, false, false, false, true, true, false, true]
        );
    }

    #[test]
    fn nrzi_flip_on_one() {
        // bits: 1,0,1,1 → phases: flip, same, flip, flip
        let phases = nrzi_encode(&[true, false, true, true]);
        assert_eq!(phases, vec![true, true, false, true]);
    }

    #[test]
    fn modulate_produces_correct_length() {
        let cfg = ModulationConfig {
            mode: "BPSK100".to_string(),
            sample_rate: 8000,
            center_frequency: 1500.0,
            ..ModulationConfig::default()
        };
        let data = b"Hi";
        let samples = bpsk_modulate(data, &cfg).unwrap();
        let n = samples_per_symbol(8000.0, 100.0).unwrap(); // 80
        let expected_syms = PREAMBLE_SYMS + data.len() * 8 + TAIL_SYMS;
        assert_eq!(samples.len(), expected_syms * n);
    }

    #[test]
    fn samples_within_range() {
        let cfg = ModulationConfig::default();
        let samples = bpsk_modulate(b"test", &cfg).unwrap();
        for &s in &samples {
            assert!((-1.0..=1.0).contains(&s), "sample {s} out of range");
        }
    }

    #[test]
    fn cpu_gpu_bits_kernel_equivalence() {
        let payload = [0xB1u8, 0x02, 0xFF, 0x00, 0x73];
        let cpu_bits: Vec<u32> = bytes_to_bits(&payload)
            .iter()
            .map(|bit| if *bit { 1 } else { 0 })
            .collect();

        let maybe_gpu_bits = pollster::block_on(gpu_bits_lsb_from_bytes(&payload));
        let Some(gpu_bits) = maybe_gpu_bits else {
            eprintln!("skipping GPU equivalence test: no compatible adapter/device");
            return;
        };

        assert_eq!(gpu_bits, cpu_bits);
    }

    #[test]
    fn cpu_gpu_symbol_map_kernel_equivalence() {
        let bits = [
            true, false, true, true, false, false, true, false, true, false,
        ];
        let nrzi = nrzi_encode(&bits);
        let cpu_syms: Vec<f32> = nrzi
            .iter()
            .map(|phase_neg| if *phase_neg { -1.0 } else { 1.0 })
            .collect();
        let nrzi_u32: Vec<u32> = nrzi
            .iter()
            .map(|phase_neg| if *phase_neg { 1 } else { 0 })
            .collect();

        let maybe_gpu_syms = pollster::block_on(gpu_symbols_from_bits(&nrzi_u32));
        let Some(gpu_syms) = maybe_gpu_syms else {
            eprintln!("skipping GPU equivalence test: no compatible adapter/device");
            return;
        };

        assert_eq!(gpu_syms.len(), cpu_syms.len());
        for (cpu, gpu) in cpu_syms.iter().zip(gpu_syms.iter()) {
            assert!((cpu - gpu).abs() <= 1e-6, "cpu={cpu}, gpu={gpu}");
        }
    }

    #[cfg(feature = "gpu")]
    #[test]
    fn gpu_modulate_matches_cpu() {
        use openpulse_core::plugin::ModulationConfig;
        let cfg = ModulationConfig {
            mode: "BPSK250".to_string(),
            sample_rate: 8000,
            center_frequency: 1500.0,
            ..ModulationConfig::default()
        };
        let payload = b"Hello";

        let cpu_out = bpsk_modulate(payload, &cfg).unwrap();

        let Some(ctx) = openpulse_gpu::GpuContext::init() else {
            eprintln!("skipping gpu_modulate_matches_cpu: no compatible adapter");
            return;
        };
        let gpu_out = bpsk_modulate_with_gpu(payload, &cfg, &ctx).unwrap();

        assert_eq!(cpu_out.len(), gpu_out.len(), "sample count mismatch");
        // 1e-3 absolute: GPU f32 (different FMA/rounding order than the CPU path) can
        // differ by ~1e-4 on near-zero RRC-tail samples; that is ~60 dB below the
        // unit-scale signal and harmless. A real kernel divergence is O(0.1+).
        for (i, (cpu, gpu)) in cpu_out.iter().zip(gpu_out.iter()).enumerate() {
            assert!(
                (cpu - gpu).abs() < 1e-3,
                "sample[{i}]: cpu={cpu:.6}, gpu={gpu:.6}"
            );
        }
    }
}
