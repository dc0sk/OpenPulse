//! Fixture parameters shared between a gate and anything claiming to reproduce it.
//!
//! Included with `mod common;` from each test binary that needs it — integration tests are separate
//! crates and cannot import each other's items, so a shared source file is the only way to make
//! "this reproduction uses the gate's parameters" a claim the compiler checks.
//!
//! That is the point. `CLAUDE.md`'s verification mechanics ban a doc-comment fidelity claim over
//! hand-transcribed parameters by name, because a comment cannot fail — the origin story is a
//! harness that claimed to reproduce a QPSK500 gate while defaulting to QPSK1000, which inverted
//! the conclusion drawn from it. Every constant here is used by the gate itself, so a reproduction
//! that drifts stops compiling rather than quietly measuring something else.

#![allow(dead_code)]

/// The saturating-floor fixture: a recorded idle floor hot enough to clamp the energy gate, with a
/// frame embedded at a chosen lead. Used by `the_receiver_never_settles_on_a_saturating_noise_floor`
/// and by reproductions of it.
pub mod saturating_floor {
    /// Recorded idle capture whose floor saturates the energy gate.
    pub const CORPUS: &str = "ic9700-idle-hot.wav";
    /// Below this the capture no longer saturates the gate and the fixture's premise is gone.
    pub const GATE_CEILING_MEAN_SQ: f32 = 0.0032;
    /// Leads, in samples, at which the frame is embedded. The lead is the variable: a short one
    /// passes even on broken code because the recovery walk is short enough to finish.
    pub const LEADS: [usize; 3] = [40_000, 80_000, 120_000];
    /// Trailing idle after the frame.
    pub const TRAIL: usize = 40_000;
    /// Frame amplitude relative to the embedded capture.
    pub const EMBED_LEVEL: f32 = 0.3;
    /// Mode and FEC the fixture transmits.
    pub const MODE: &str = "BPSK250";
    /// Payload, also the expected decode.
    pub const PAYLOAD: &[u8] = b"correlation gate probe";
    /// Listen window.
    pub const TIMEOUT_MS: u64 = 40_000;
}

/// Preamble sequence generators shared by the #1062 harnesses.
///
/// Here for this module's stated reason: two harnesses need the SAME sequences, and #1062 has
/// already paid once for a hand-written stand-in. `f12` fed its correlator a `+-+-` chip run under a
/// comment calling it "the shipped sync word's structure", when the wire carries alternating *bits*
/// which NRZI turns into `--++`. Correlation between the two was 0.035, and a wire-format argument
/// was built on it before a reviewer caught it.
pub mod preamble {
    use openpulse_core::plugin::{ModulationConfig, ModulationPlugin};

    pub const FS: f32 = 8000.0;
    pub const FC: f32 = 1500.0;

    pub fn cfg(mode: &str) -> ModulationConfig {
        ModulationConfig {
            mode: mode.to_string(),
            sample_rate: FS as u32,
            center_frequency: FC,
            ..Default::default()
        }
    }

    /// A maximal-length LFSR sequence of length `2^bits - 1`, as ±1.
    pub fn m_sequence(bits: u32, taps: &[u32]) -> Vec<f32> {
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
    /// The modulator's preamble bits are hardcoded, so the PN chips go in the *payload* span and
    /// that span is returned. Everything downstream — NRZI, half-Hann crossfade, carrier — is the
    /// shipped code path, so this differs from `bpsk_preamble_template` in the symbol sequence and
    /// nothing else. The NRZI state entering the payload is `+1` (the 32 preamble bits contain 16
    /// ones, an even number of flips), so `bit[k] = chip[k] != chip[k-1]` with `chip[-1] = +1`
    /// inverts the encoder exactly. The final chip is dropped for the same crossfade reason the
    /// shipped template drops its last symbol.
    pub fn pn_template(mode: &str, chips: &[f32]) -> Option<Vec<f32>> {
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
        // Samples per symbol comes from the SHIPPED template, not from parsing digits out of the
        // mode string. `parse_baud_rate` maps "BPSK31" to 31.25 and "BPSK63" to 62.5
        // (plugins/bpsk/src/lib.rs), so the naive parse gives 258 sps where the plugin uses 256:
        // the slice then starts a quarter-symbol late and drifts two samples per symbol. It was
        // invisible at BPSK250 because 8000/250 is exact, which is how it survived being copied.
        let shipped = bpsk_plugin::modulate::bpsk_preamble_template(&c).ok()?;
        let n = shipped.len() / (bpsk_plugin::modulate::PREAMBLE_SYMS - 1);
        let start = n * bpsk_plugin::modulate::PREAMBLE_SYMS;
        let span = n * (chips.len() - 1);
        (full.len() >= start + span).then(|| full[start..start + span].to_vec())
    }

    /// The maximal-length sequence of exactly `n` chips, for n in {31, 63, 127, 255}.
    ///
    /// **A PREFIX of a longer m-sequence is not an m-sequence** — truncation destroys the flat
    /// autocorrelation that makes PN worth choosing, so a prefix measures a worse sequence than the
    /// one the design proposes and would understate PN. #1062's thread vetted and rejected N=31 and
    /// set N >= 63 using proper m-sequences; anything comparing against those numbers must do the
    /// same. Panics on an unsupported length rather than silently truncating.
    pub fn m_sequence_of_len(n: usize) -> Vec<f32> {
        let (bits, taps): (u32, &[u32]) = match n {
            31 => (5, &[5, 3]),
            63 => (6, &[6, 5]),
            127 => (7, &[7, 6]),
            255 => (8, &[8, 6, 5, 4]),
            _ => panic!(
                "no maximal LFSR configured for {n} chips; add its taps rather than truncating"
            ),
        };
        let s = m_sequence(bits, taps);
        assert_eq!(s.len(), n, "LFSR degree {bits} did not yield {n} chips");
        // Length alone does not prove maximality — a non-primitive tap set still fills the register
        // and would defeat the guard above. A maximal sequence is balanced: exactly 2^(m-1) ones.
        let ones = s.iter().filter(|&&c| c > 0.0).count();
        assert_eq!(
            ones,
            1 << (bits - 1),
            "taps {taps:?} are not primitive: {ones} ones in {n} chips, expected {}",
            1 << (bits - 1)
        );
        s
    }
}
