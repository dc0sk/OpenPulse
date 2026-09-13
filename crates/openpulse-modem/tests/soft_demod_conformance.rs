//! Every mode's soft demodulator obeys the engine's LLR contract — swept, not listed.
//!
//! Two implications, checked for **every mode every registered plugin claims**:
//!
//! - **(A)** a mode that advertises `supports_soft_demod` must return `Ok` from `demodulate_soft`.
//!   `receive_from_samples` treats a soft error on an advertised mode as a terminal decode failure
//!   and never falls back to `demodulate()`, so the advertisement is a promise the receiver keeps.
//! - **(B)** whenever `demodulate_soft` returns `Ok`, hard-deciding those LLRs must reproduce
//!   `demodulate()`'s bytes exactly. Note the quantifier: **not** "for advertised modes". The
//!   engine's LDPC, turbo and soft-concatenated arms call `demodulate_soft` unconditionally and feed
//!   the result to the decoder, warning but not refusing when the plugin advertises `false`. So the
//!   contract binds the ±1.0 trait fallback too.
//!
//! The reverse of (A) is deliberately NOT asserted. `supports_soft_demod` defaults to `false`
//! meaning "the ±1.0 hard-decision fallback, no iteration gain" — not "this call will fail" — so
//! fsk4 and js8 legitimately advertise `false` and still return `Ok`.
//!
//! **This replaces a hand-written list of 14 (plugin, mode) pairs** that had drifted to cover 14 of
//! 70 soft-capable modes: every `-RRC` variant, all four `OFDM52-*` higher-order modes, all nine
//! `SCFDMA*` modes and 8 of 12 `PILOT-*` were unpinned.
//!
//! **Hard-decide through the product's own function.** The list it replaced sliced with
//! `bit = llr <= 0`, which is a fourth convention: production uses `fec::hard_decide`
//! (`is_sign_negative`), while `ldpc.rs` and `turbo.rs` use `l < 0.0`. Those three disagree only at
//! ±0.0 — and the retired test's `<= 0` disagrees with BOTH at `+0.0`. A harness that
//! re-implements the decision it is checking cannot see a change in the real one, so this calls
//! `fec::hard_decide` and separately fails on any exactly-zero LLR, which has no
//! convention-independent hard decision.
//!
//! **What this does NOT cover**: the GPU soft paths. `Plugin::new()` is the CPU constructor and the
//! daemon builds `with_gpu` by default; #1084 was a psk8 GPU demodulator emitting per-symbol LLRs
//! bit-reversed, which this sweep cannot see and the `--no-default-features` gate cannot build.
//! `plugins/{psk8,64qam}/tests/gpu_cpu_equivalence.rs` is the gate for that arm. It also says
//! nothing about LLR *magnitude* calibration — `llr_reliability.rs` and `llr_calibration.rs` own
//! that, and bpsk/qpsk/psk8 still have no bin-calibration test.

use openpulse_core::error::ModemError;
use openpulse_core::fec::hard_decide;
use openpulse_core::plugin::{ModulationConfig, ModulationPlugin, PluginInfo};

/// Modes that refuse to modulate at the production sample rate, with the reason.
///
/// Pinned as a set rather than skipped silently: a count floor passes happily while the same modes
/// are skipped forever, and these are advertised in `supported_modes` while being unreachable —
/// the engine builds every `ModulationConfig` from `AudioConfig::default()` (8 kHz) and never sets
/// `pulse_shape`, so no shipping binary can run them. Tracked separately; if one becomes drivable
/// it must leave this list, and if a new mode starts refusing, this fails.
const UNDRIVABLE_AT_8K: [&str; 5] = [
    "8PSK2000",
    "8PSK9600",
    "8PSK9600-RRC",
    "QPSK9600",
    "QPSK9600-RRC",
];

/// The production sample rate — read from the default config the engine builds, never a literal,
/// so a change to the shipped rate moves this sweep with it.
fn production_sample_rate() -> u32 {
    ModulationConfig::default().sample_rate
}

/// The production config, with only the mode substituted.
fn config_for(mode: &str) -> ModulationConfig {
    ModulationConfig {
        mode: mode.to_string(),
        ..ModulationConfig::default()
    }
}

/// The outcome of checking one mode.
enum Outcome {
    Checked,
    Undrivable,
}

/// THE CHECK for one mode. Returns Err with the reason so the sabotage tests can require a failure.
fn check_mode(plugin: &dyn ModulationPlugin, mode: &str) -> Result<Outcome, String> {
    let cfg = config_for(mode);
    // Most modes carry an arbitrary payload; the ACK frames are fixed-size, so fall back to their
    // size rather than skipping them — a payload we cannot size is a harness bug, not a skip.
    let mut tx = None;
    for len in [48usize, 13, 12] {
        let payload: Vec<u8> = (0..len as u8).map(|v| v.wrapping_mul(37) ^ 0x5C).collect();
        match plugin.modulate(&payload, &cfg) {
            Ok(samples) => {
                tx = Some((payload, samples));
                break;
            }
            Err(ModemError::Configuration(_)) => return Ok(Outcome::Undrivable),
            Err(ModemError::Modulation(_)) => continue,
            Err(e) => return Err(format!("{mode}: modulate failed unexpectedly: {e}")),
        }
    }
    let Some((payload, tx)) = tx else {
        return Err(format!(
            "{mode}: no payload size in [48, 13, 12] could be modulated"
        ));
    };

    let advertised = plugin.supports_soft_demod(mode);
    let llrs = match plugin.demodulate_soft(&tx, &cfg) {
        Ok(llrs) => llrs,
        // (A) an advertised mode must not refuse.
        Err(e) if advertised => {
            return Err(format!(
                "{mode}: advertises supports_soft_demod but demodulate_soft refused ({e}). The \
                 engine treats that as a terminal decode failure — it does not fall back to \
                 demodulate()."
            ))
        }
        // Unadvertised and refusing is legal: `false` means "no genuine LLRs", not "this works".
        Err(_) => return Ok(Outcome::Checked),
    };

    // An exactly-zero LLR has no convention-independent hard decision: `is_sign_negative` (the
    // engine and fec), `l < 0.0` (ldpc, turbo) and `l <= 0` (the retired harness) all disagree on it.
    let zeros = llrs.iter().filter(|l| **l == 0.0).count();
    if zeros != 0 {
        return Err(format!(
            "{mode}: {zeros} LLRs are exactly zero, where the engine's slicer, ldpc/turbo's and the \
             retired harness's all decide differently"
        ));
    }

    let hard = plugin
        .demodulate(&tx, &cfg)
        .map_err(|e| format!("{mode}: hard demodulate failed: {e}"))?;
    let soft_bytes = hard_decide(&llrs);
    let n = hard.len().min(soft_bytes.len());
    if n == 0 {
        return Err(format!(
            "{mode}: nothing to compare (hard {} bytes, soft {} bytes)",
            hard.len(),
            soft_bytes.len()
        ));
    }
    // (B) the whole overlap, not just the payload prefix.
    for i in 0..n {
        if hard[i] != soft_bytes[i] {
            return Err(format!(
                "{mode}: byte {i} differs — hard 0x{:02x}, hard-decided soft 0x{:02x}. The LLR sign \
                 convention or bit order does not match demodulate().",
                hard[i], soft_bytes[i]
            ));
        }
    }
    // The payload round-trip only binds modes that claim genuine soft output: js8's demodulate is
    // not byte-transparent (10 bytes back for 48 in), and that is not a convention failure.
    if advertised && hard.len() >= payload.len() && hard[..payload.len()] != payload[..] {
        return Err(format!(
            "{mode}: hard demodulate did not round-trip its own payload"
        ));
    }
    Ok(Outcome::Checked)
}

fn plugins() -> Vec<Box<dyn ModulationPlugin>> {
    vec![
        Box::new(bpsk_plugin::BpskPlugin::new()),
        Box::new(fsk4_plugin::Fsk4Plugin::new()),
        Box::new(mfsk16_plugin::Mfsk16Plugin::new()),
        Box::new(ofdm_plugin::OfdmPlugin::new()),
        Box::new(psk8_plugin::Psk8Plugin::new()),
        Box::new(qam64_plugin::Qam64Plugin::new()),
        Box::new(qpsk_plugin::QpskPlugin::new()),
        Box::new(scfdma_plugin::ScFdmaPlugin::new()),
        Box::new(pilot_plugin::PilotPlugin::new()),
    ]
}

#[test]
fn every_soft_demodulator_obeys_the_llr_contract() {
    let ps = plugins();
    let mut checked = 0usize;
    let mut undrivable = Vec::new();
    let mut failures = Vec::new();

    for p in &ps {
        for mode in &p.info().supported_modes {
            match check_mode(p.as_ref(), mode) {
                Ok(Outcome::Checked) => checked += 1,
                Ok(Outcome::Undrivable) => undrivable.push(mode.clone()),
                Err(e) => failures.push(e),
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} mode(s) break the LLR contract:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );

    undrivable.sort();
    let expected: Vec<String> = UNDRIVABLE_AT_8K.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(
        undrivable,
        expected,
        "the set of modes that refuse to modulate at {} Hz changed. A mode that became drivable \
         must leave UNDRIVABLE_AT_8K; one that started refusing is a regression, not a skip.",
        production_sample_rate()
    );

    // A sweep that silently stopped discovering modes would pass while checking nothing. 68 is
    // every mode the nine production-registered plugins declare, less the five undrivable ones.
    // js8 is deliberately absent: it is a discovery waveform and neither the CLI nor the daemon
    // registers it with the modem engine, so it is not part of "what production can demodulate".
    let declared: usize = ps.iter().map(|p| p.info().supported_modes.len()).sum();
    assert_eq!(
        checked + undrivable.len(),
        declared,
        "the sweep reached {} of {declared} declared modes — some mode left the loop without a \
         verdict",
        checked + undrivable.len()
    );
    assert!(
        checked >= 68,
        "only {checked} modes checked — the sweep is not reaching the registered plugins, so a mode \
         could break the contract without this test noticing"
    );
}

/// The plugin set above must be the one the product registers — proven by reading the production
/// registration functions, not by remembering to update both.
///
/// The two disagree today: the daemon and the CLI register the same nine, while `openpulse-ardop`
/// and `openpulse-kiss` build their own shorter sets. This binds to the two that carry the full
/// ladder; a plugin added to either without being added here fails.
#[test]
fn the_swept_plugin_set_is_the_one_production_registers() {
    let sources = [
        include_str!("../../openpulse-cli/src/plugins.rs"),
        include_str!("../../openpulse-daemon/src/monitor.rs"),
    ];
    let mut registered: Vec<String> = Vec::new();
    for src in sources {
        for line in src.lines() {
            let Some(rest) = line.split("Box::new(").nth(1) else {
                continue;
            };
            let Some(ctor) = rest.split("::new()").next() else {
                continue;
            };
            // The two sources spell it differently — `BpskPlugin` (imported) vs
            // `bpsk_plugin::BpskPlugin` (qualified) — so compare the last path segment.
            let name = ctor.trim().rsplit("::").next().unwrap_or("").trim();
            if name.ends_with("Plugin") && !registered.iter().any(|r| r == name) {
                registered.push(name.to_string());
            }
        }
    }
    registered.sort();
    assert!(
        registered.len() >= 9,
        "the source scan found only {} plugin registrations ({registered:?}) — the scan is broken, \
         not the product",
        registered.len()
    );

    // Every plugin production registers must be constructed by `plugins()` above. Checked against
    // this file's own text, because a `Box<dyn ModulationPlugin>` cannot report its concrete type:
    // the question is whether somebody added a plugin to the product and not to the sweep.
    let own_source = include_str!("soft_demod_conformance.rs");
    let sweep_body = own_source
        .split("fn plugins() -> Vec<Box<dyn ModulationPlugin>> {")
        .nth(1)
        .and_then(|s| s.split("\n}").next())
        .expect("this test must be able to read its own plugins() body");
    let missing: Vec<&String> = registered
        .iter()
        .filter(|ctor| !sweep_body.contains(ctor.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "production registers {missing:?}, which `plugins()` in this file does not construct — the \
         sweep would silently skip {} plugin(s) worth of modes",
        missing.len()
    );

    // And the reverse: the sweep must not carry a plugin the product stopped registering.
    let swept_count = plugins().len();
    assert_eq!(
        swept_count,
        registered.len(),
        "the sweep constructs {swept_count} plugins but production registers {} ({registered:?})",
        registered.len()
    );
}

// ---- discriminators ---------------------------------------------------------------------------
// A checker nobody has watched fail is the self-consistent checker it exists to prevent. These
// three wrap a real plugin and break the contract in the ways it has actually been broken here, and
// each asserts `check_mode` REJECTS it. They run by default: a discriminator under `#[ignore]`
// proves nothing, and a sabotage performed once by hand proves nothing tomorrow.

enum Sabotage {
    /// Every LLR sign flipped — the coarsest convention break.
    NegateLlrs,
    /// Bits reversed within each byte: the #1084 shape, where the psk8 GPU demodulator emitted
    /// per-symbol LLRs in the wrong bit order while every frame-success metric stayed green.
    ReverseBitsPerByte,
    /// Advertises soft support and then refuses: the #996 shape, where `QPSK250-D` promised a soft
    /// path that the engine then treated as a terminal decode failure.
    RefuseWhileAdvertising,
}

struct Sabotaged {
    inner: Box<dyn ModulationPlugin>,
    how: Sabotage,
}

impl ModulationPlugin for Sabotaged {
    fn info(&self) -> &PluginInfo {
        self.inner.info()
    }
    fn modulate(&self, data: &[u8], config: &ModulationConfig) -> Result<Vec<f32>, ModemError> {
        self.inner.modulate(data, config)
    }
    fn demodulate(
        &self,
        samples: &[f32],
        config: &ModulationConfig,
    ) -> Result<Vec<u8>, ModemError> {
        self.inner.demodulate(samples, config)
    }
    fn supports_soft_demod(&self, mode: &str) -> bool {
        match self.how {
            Sabotage::RefuseWhileAdvertising => true,
            _ => self.inner.supports_soft_demod(mode),
        }
    }
    fn demodulate_soft(
        &self,
        samples: &[f32],
        config: &ModulationConfig,
    ) -> Result<Vec<f32>, ModemError> {
        match self.how {
            Sabotage::RefuseWhileAdvertising => {
                Err(ModemError::Demodulation("planted refusal".into()))
            }
            Sabotage::NegateLlrs => Ok(self
                .inner
                .demodulate_soft(samples, config)?
                .into_iter()
                .map(|l| -l)
                .collect()),
            Sabotage::ReverseBitsPerByte => {
                let llrs = self.inner.demodulate_soft(samples, config)?;
                let mut out = Vec::with_capacity(llrs.len());
                for chunk in llrs.chunks(8) {
                    let mut c: Vec<f32> = chunk.to_vec();
                    c.reverse();
                    out.extend(c);
                }
                Ok(out)
            }
        }
    }
}

fn assert_rejects(how: Sabotage, label: &str) {
    // BPSK250 has the most independent coverage in this repo, so a failure here is attributable to
    // the sabotage rather than to a mode that was marginal to begin with.
    let mode = "BPSK250";
    let honest = bpsk_plugin::BpskPlugin::new();
    assert!(
        check_mode(&honest, mode).is_ok(),
        "{label}: the UNsabotaged plugin must pass, or the discriminator proves nothing"
    );
    let sab = Sabotaged {
        inner: Box::new(bpsk_plugin::BpskPlugin::new()),
        how,
    };
    assert!(
        check_mode(&sab, mode).is_err(),
        "{label}: the checker ACCEPTED a plugin that breaks the LLR contract — it cannot fail, so \
         its passing says nothing"
    );
}

#[test]
fn the_checker_rejects_negated_llrs() {
    assert_rejects(Sabotage::NegateLlrs, "negated LLRs");
}

#[test]
fn the_checker_rejects_bit_reversal_within_a_byte() {
    assert_rejects(
        Sabotage::ReverseBitsPerByte,
        "per-byte bit reversal (the #1084 shape)",
    );
}

#[test]
fn the_checker_rejects_a_mode_that_advertises_soft_and_then_refuses() {
    assert_rejects(
        Sabotage::RefuseWhileAdvertising,
        "advertise-then-refuse (the #996 shape)",
    );
}
