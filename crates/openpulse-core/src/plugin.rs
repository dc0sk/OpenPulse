use serde::{Deserialize, Serialize};

use crate::error::{ModemError, PluginError};

/// Current plugin trait version.
/// Format: `<major>.<minor>.<patch>`
///
/// 1.1.0 added [`ModulationPlugin::preamble_template`] — additive, with a `None` default, so every
/// plugin declaring `1.0.0` keeps working unchanged.
///
/// 3.0.0 added [`PreambleTemplate::for_mode`], which is breaking because the constructor gained a
/// parameter. The engine refuses a template whose `for_mode` differs from the mode being received.
/// Migration: pass the mode the constants were measured on — not the mode being modulated, when
/// those differ, because that difference is the defect this exists to surface.
///
/// 2.0.0 changed that method's return type from `Option<Vec<f32>>` to [`Option<PreambleTemplate>`],
/// which is breaking: the samples now travel with the correlation constants measured for that
/// waveform. Migration for a plugin that published a template is to wrap the samples in
/// `PreambleTemplate::new(samples, rho_threshold, rho_grid_hz)` with values re-derived for the mode
/// (see the type's docs); a plugin that did not is unaffected beyond declaring the new major.
///
/// Why the bundling had to be breaking rather than a second optional method: the constants are
/// waveform-specific and the failure mode of getting them wrong is silent. BPSK's 0.40 threshold
/// sits *below* QPSK500's recorded idle-noise ceiling of 0.429, so a QPSK template inheriting it
/// would corroborate settles on pure noise — the exact defect the check exists to prevent, and one
/// that produces no error, only a receiver that stops acquiring. A single method makes publishing a
/// template without its own measured constants unrepresentable.
pub const PLUGIN_TRAIT_VERSION: &str = "3.0.0";

// ── Plugin metadata ───────────────────────────────────────────────────────────

/// Static metadata that every plugin must provide.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    /// Short identifier, e.g. `"BPSK"`.
    pub name: String,
    /// Semver version string of the plugin itself.
    pub version: String,
    /// Human-readable description.
    pub description: String,
    /// Plugin author(s).
    pub author: String,
    /// List of mode strings this plugin handles, e.g. `["BPSK31", "BPSK100"]`.
    pub supported_modes: Vec<String>,
    /// Plugin trait version requirement, e.g. `"1.0"` (format: `<major>.<minor>`).
    /// The plugin is compatible with the framework if:
    /// - framework major version == plugin major version, AND
    /// - framework minor version >= plugin minor version
    pub trait_version_required: String,
}

// ── Pulse shaping ─────────────────────────────────────────────────────────────

/// Amplitude envelope applied during symbol modulation.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum PulseShape {
    /// 50% overlapping raised-cosine crossfade between adjacent symbols.
    /// Default for all modes; equivalent to PSK31 shaping for pure BPSK.
    #[default]
    Hann,
    /// Independent sin² amplitude envelope per symbol (0 → 1 → 0 per period).
    /// Forces amplitude zero at every symbol boundary; achieves null-to-null BW ≈ 2×Rs.
    /// Used by `-HF` mode aliases for HF-legal operation at high baud rates.
    CosineOverlap,
    /// Square-root raised-cosine (SRRC) FIR pulse shaping.
    /// Occupied bandwidth ≈ (1 + alpha) × Rs Hz; requires a matched RRC RX filter.
    /// Used by `-RRC` mode aliases.
    Rrc {
        /// RRC rolloff factor α ∈ [0, 1]; 0.35 is the default for `-RRC` modes.
        alpha: f32,
    },
}

// ── Modulation configuration ──────────────────────────────────────────────────

/// Runtime configuration passed to a plugin for each encode/decode call.
#[derive(Debug, Clone)]
pub struct ModulationConfig {
    /// Centre (audio) frequency in Hz (typically 1 500 Hz for HF work).
    pub center_frequency: f32,
    /// PCM sample rate of the audio stream in Hz.
    pub sample_rate: u32,
    /// Mode string that selects parameters inside the plugin, e.g. `"BPSK31"`.
    pub mode: String,
    /// Pulse-shaping envelope; plugins select this based on the mode string.
    pub pulse_shape: PulseShape,
    /// AFC correction already applied to `center_frequency` by the engine, in Hz.
    ///
    /// Non-zero when the engine ran AFC settling before this decode attempt.
    /// Plugins may use this to decide whether carrier-phase drift correction
    /// is appropriate (e.g. QPSK only corrects drift when AFC is active).
    pub afc_correction_hz: f32,
}

impl Default for ModulationConfig {
    fn default() -> Self {
        Self {
            center_frequency: 1500.0,
            sample_rate: 8000,
            mode: "BPSK100".to_string(),
            pulse_shape: PulseShape::Hann,
            afc_correction_hz: 0.0,
        }
    }
}

// ── Frame geometry ────────────────────────────────────────────────────────────

/// Mode-specific frame dimensions used by the receive engine to size its scan
/// step, energy-gate window, and per-attempt demodulation slice.
///
/// All values are in samples at the config's sample rate.  Before this struct
/// existed the engine guessed these from trailing digits of the mode name —
/// wrong for every mode whose name does not end in its baud rate (OFDM52's 52
/// is a subcarrier count; SCFDMA52-64QAM-P4 parsed as 4 baud) — and assumed a
/// 32-symbol preamble (true only for BPSK).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameGeometry {
    /// Scan step: one symbol period (serial-tone modes) or one block-symbol
    /// length (OFDM/SC-FDMA).
    pub symbol_period_samples: usize,
    /// Acquisition span the demodulator needs near the slice front (preamble
    /// or sync sequence).
    pub preamble_samples: usize,
    /// Minimum slice length that can hold one decodable minimal frame.
    pub min_frame_samples: usize,
    /// Slice length that bounds one demodulation attempt: the largest frame
    /// this mode emits (255-byte RS block) plus margin.
    pub max_frame_samples: usize,
}

// ── Preamble correlation template ─────────────────────────────────────────────

/// A mode's modulated preamble together with the correlation constants measured for it.
///
/// They travel together because a threshold is a property of the waveform, never of the receiver.
/// A threshold generous for one template sits *underneath* the noise floor of another, so a
/// receiver-wide constant silently corroborates settles on noise for some modes. Deployed practice
/// agrees: codec2's `timing_mx_thresh` is per-mode config spanning 0.08–0.5 (`src/ofdm_mode.c`), and
/// modem73 gates its known-sequence probes per geometry (`robust_modem.hh:913`).
///
/// A threshold has to sit between two measured quantities:
///
/// 1. the ρ ceiling of band noise, which it must clear, and
/// 2. the weakest ρ that still decodes, which it must stay under.
///
/// **Both are easy to measure too narrowly, and #1053 shipped nothing because of it.** Measure the
/// decode column on the channel the mode exists for, not on AWGN: `QPSK250-D` — the `hpx_hf` fade
/// rung — decodes `moderate_f1` frames down to ρ = 0.276, *below* its own recorded idle ceiling of
/// 0.291, so the two distributions overlap and no threshold exists. And measure the noise column
/// across receive bandwidths: the ceiling is set by the overlap of the noise spectrum with the
/// *template's* spectrum, not by template length, and a 500 Hz filter lifts idle ρ above every
/// threshold derived from wideband captures.
///
/// A mode with no usable gap publishes no template at all (`None`) and keeps the energy-only settle,
/// which is worse but honest. Never widen a gap by picking a threshold from another mode's table.
#[derive(Debug, Clone, PartialEq)]
pub struct PreambleTemplate {
    /// The mode these constants were **derived for**, which the engine checks against the mode it
    /// is receiving.
    ///
    /// Bundling samples with constants (2.0.0) stopped a template being published *without*
    /// constants; it did not stop one being published with **another mode's** constants, and that
    /// is what happened latently: `bpsk` returned BPSK250's threshold for every BPSK mode, and only
    /// a cost limit — the raw-sample correlation cap, which discarded the slow rungs before the
    /// engine could use them — kept it from mattering. A cost limit enforcing a correctness
    /// property is not a guard; when the cap became a post-decimation budget the property vanished
    /// with it.
    ///
    /// So the binding is in the type now. Inheriting another mode's constants requires naming that
    /// mode here, which the engine rejects — accidental inheritance becomes a deliberate lie.
    pub for_mode: String,
    /// The *modulated* preamble at the config's carrier and sample rate, so a correlator can use it
    /// directly. Keep it to the preamble: a template running into the data symbols correlates
    /// against payload that differs frame to frame.
    pub samples: Vec<f32>,
    /// Normalised-correlation floor below which a candidate window is not this preamble.
    pub rho_threshold: f32,
    /// Half-width, in Hz, of the residual-frequency grid the correlation searches around the
    /// settled carrier correction.
    ///
    /// Bounded below by what the AFC settle leaves behind (≤ 0.3 Hz measured, so ±20 Hz is already
    /// generous) and above by the preamble's own line structure. The upper bound is waveform
    /// -specific and can be brutal: BPSK's preamble *bits* alternate, but NRZI flips phase only on a
    /// `1`, so the *symbols* are `--++` repeating — a square wave of period **four** symbols, with
    /// lines at `fc ± baud/4` and odd harmonics (measured at BPSK250: ±62.5 Hz at 0 dB, ±187.5 at
    /// −14 dB, ±312.5 at −31 dB, and nothing at ±125). A grid reaching a line rotates it onto plain
    /// carrier and a steady tone starts scoring like a preamble (0.017 at ±20 Hz, 0.659 at ±160 Hz).
    ///
    /// This said `fc ± baud/2` until 2026-08-03 — twice the true spacing, so the documented safe
    /// bound sat *above* the first line. The shipped ±20 Hz is well inside it either way, but
    /// `fc ± baud/4` is where the interference that this veto cannot refuse actually lives (#1062).
    pub rho_grid_hz: f32,
    /// The ρ a **delivered** frame is known to reach on this mode's own operating channel, or
    /// `None` while unmeasured.
    ///
    /// It bounds the runtime calibration from above (#1060): a derived threshold that climbed past
    /// this would discard frames the channel actually delivered, so the veto stands down to
    /// energy-only detection instead. `None` means "never stand down", which is where every mode
    /// starts — a bound must be *measured decode-conditioned on the channel the rung exists for*
    /// before it can carry a decision, which is #1053's lesson in the type.
    pub delivered_frame_rho_bound: Option<f32>,
}

impl PreambleTemplate {
    /// Bundle a modulated preamble with its measured correlation constants.
    pub fn new(
        for_mode: impl Into<String>,
        samples: Vec<f32>,
        rho_threshold: f32,
        rho_grid_hz: f32,
    ) -> Self {
        Self {
            for_mode: for_mode.into(),
            samples,
            rho_threshold,
            rho_grid_hz,
            delivered_frame_rho_bound: None,
        }
    }

    /// Publish the delivered-frame ρ bound that lets the runtime calibration stand down (#1060).
    ///
    /// Additive by construction: a template without it never stands down, which is exactly the
    /// behaviour of every mode before this existed.
    pub fn with_delivered_frame_bound(mut self, bound: f32) -> Self {
        self.delivered_frame_rho_bound = Some(bound);
        self
    }
}

// ── Plugin trait ──────────────────────────────────────────────────────────────

/// A modulation / demodulation plugin.
///
/// Implement this trait to add a new waveform to OpenPulse.  Plugins are
/// registered with [`PluginRegistry`] at startup.
pub trait ModulationPlugin: Send + Sync {
    /// Return this plugin's static metadata.
    fn info(&self) -> &PluginInfo;

    /// Encode `data` bytes into a vector of normalised audio samples (`-1.0 …
    /// +1.0`).
    fn modulate(&self, data: &[u8], config: &ModulationConfig) -> Result<Vec<f32>, ModemError>;

    /// Decode audio samples back to the original bytes.
    fn demodulate(&self, samples: &[f32], config: &ModulationConfig)
        -> Result<Vec<u8>, ModemError>;

    /// Decode audio samples and return per-bit soft log-likelihood ratios.
    ///
    /// # LLR convention
    ///
    /// - **Sign**: positive = bit more likely 0, negative = bit more likely 1.
    ///   Hard-slicing every LLR with [`fec::hard_decide`](crate::fec::hard_decide)
    ///   (`is_sign_negative`, LSB-first within each byte) MUST reproduce exactly
    ///   the byte stream returned by [`demodulate`](Self::demodulate) on the same
    ///   input.  Slice with that function, not with a re-implementation: this doc
    ///   said `bit = llr <= 0` until 2026-09-13, which disagrees with it at `+0.0`
    ///   — as does `ldpc.rs`/`turbo.rs`'s `l < 0.0`.  An LLR of exactly zero has no
    ///   convention-independent decision and no plugin should emit one.  Enforced
    ///   for every mode of every registered plugin by `soft_demod_conformance` in
    ///   `openpulse-modem`, which also fails on a zero-valued LLR.
    /// - **Scale**: per-plugin and NOT normalised across plugins — BPSK emits
    ///   raw differential dot products, OFDM emits |H|²-weighted projections,
    ///   8PSK emits max-log-MAP distance differences.  Within one plugin the
    ///   scale is monotone in reliability (required by per-frame soft
    ///   combining).  Cross-MODE soft combining (e.g. ARQ
    ///   retransmission in a different mode) must therefore weight per frame
    ///   — `combine_llrs_weighted` with per-frame noise metrics — rather than
    ///   adding raw LLRs from different plugins.
    /// - **Calibration**: a plugin whose LLRs are *true* log-likelihood ratios divides every distance
    ///   by its estimated σ² (SC-FDMA and OFDM do; `symbol_llrs`' `noise_var` argument).  Repeated
    ///   observations of the same bits in the same mode are then combined by summing —
    ///   `combine_llrs_map` — never by weighting again with `1/σ²`, which would apply σ⁻² twice.
    ///   Every shipped plugin has been calibrated (PR #687) — 64QAM, BPSK and QPSK included — so a
    ///   plugin reaching this trait default is the only remaining noise-blind case: it emits ±1.0,
    ///   its `mean(|LLR|)` is flat in SNR, and a weight derived from it conveys nothing.
    ///   `crates/openpulse-modem/tests/llr_calibration.rs` fails any plugin that regresses to that.
    ///
    /// Plugins that know their internal soft values (BPSK I-channel
    /// correlation, QPSK I/Q projections) should override this for maximum
    /// coding gain (~1–2 dB).
    ///
    /// The default falls back to [`demodulate`](Self::demodulate) and maps each
    /// hard-decided bit to ±1.0.
    fn demodulate_soft(
        &self,
        samples: &[f32],
        config: &ModulationConfig,
    ) -> Result<Vec<f32>, ModemError> {
        let bytes = self.demodulate(samples, config)?;
        let llrs = bytes
            .iter()
            .flat_map(|&b| (0..8u8).map(move |i| if (b >> i) & 1 == 0 { 1.0f32 } else { -1.0f32 }))
            .collect();
        Ok(llrs)
    }

    /// Frame geometry for `config.mode`, used by the receive engine to size
    /// its scan step, energy-gate window, and demodulation slices.
    ///
    /// Returns `None` (the default) when the plugin does not describe its
    /// geometry; the engine then falls back to a mode-name heuristic that is
    /// only correct for modes named after their baud rate with a 32-symbol
    /// preamble.  Every production plugin should override this.
    fn frame_geometry(&self, _config: &ModulationConfig) -> Option<FrameGeometry> {
        None
    }

    /// Return `true` if this plugin produces genuine soft LLRs from
    /// [`demodulate_soft`](Self::demodulate_soft).
    ///
    /// Plugins that override `demodulate_soft` with proper LLR computation
    /// (e.g. matched-filter projections, per-subcarrier FFT magnitude) should
    /// override this to return `true`.  The default `false` indicates the
    /// fallback ±1.0 hard-decision output, which provides no iteration gain
    /// to soft-input FEC decoders such as LDPC and turbo.
    ///
    /// The modem engine logs a warning when a soft-FEC mode is paired with a
    /// plugin that returns `false`.
    fn supports_soft_demod(&self, mode: &str) -> bool {
        let _ = mode;
        false
    }

    /// Return `true` when this plugin can handle `mode` (case-insensitive).
    fn supports_mode(&self, mode: &str) -> bool {
        self.info()
            .supported_modes
            .iter()
            .any(|m| m.eq_ignore_ascii_case(mode))
    }

    /// Estimate the carrier frequency offset in Hz from the given samples.
    ///
    /// Returns `None` if the plugin does not support AFC or the buffer is too
    /// short.  The default implementation returns `None`.
    fn estimate_afc_hz(&self, _samples: &[f32], _config: &ModulationConfig) -> Option<f32> {
        None
    }

    /// Estimate the absolute receive SNR in dB from `samples`, using a waveform-aware
    /// symbol-domain measurement, for the adaptive rate decision.
    ///
    /// Returns `None` when the plugin has no symbol-domain estimator — the engine then
    /// falls back to the waveform-blind M2M4 moment estimator. The default returns `None`.
    ///
    /// Why a plugin override beats M2M4: M2M4 assumes a constant-modulus envelope, so on a
    /// pulse-shaped or multicarrier waveform its output stops tracking SNR and caps the rate
    /// ladder. A plugin measures noise from the component of each equalized symbol *orthogonal*
    /// to its decision, so its estimate keeps rising with SNR up to the mode's residual-ISI (EVM)
    /// floor. It is decision-directed, so it saturates once symbol errors are common — the safe
    /// direction for a rate decision.
    fn estimate_snr_db(&self, _samples: &[f32], _config: &ModulationConfig) -> Option<f32> {
        None
    }

    /// The passband preamble this mode transmits, as audio samples, for a receiver that must decide
    /// whether a candidate window contains a frame *before* committing to it.
    ///
    /// `None` (the default) means the plugin offers no template, and the receiver falls back to
    /// deciding on energy alone — which is what every mode did before this existed, and which
    /// cannot work when the band noise floor rises above the gate: energy says "something is here",
    /// never "this is a preamble". Five separately-diagnosed defects (#1020, #1021, #1039, #1040,
    /// #1045) were that one gap. codec2/FreeDV detects frames on a normalised correlation ratio
    /// instead, with no absolute receive-energy threshold anywhere (`src/ofdm.c`: `timing_mx_thresh`,
    /// normalised by `av_level`; per-mode 0.08-0.5 in `src/ofdm_mode.c`) — read directly, unlike the other reference modems, whose
    /// approach is recorded second-hand in `docs/dev/research/references.md`.
    ///
    /// The returned [`PreambleTemplate`] carries the samples *and* the ρ threshold and search grid
    /// measured for that mode — see its docs for why those cannot be receiver-wide constants.
    fn preamble_template(&self, _config: &ModulationConfig) -> Option<PreambleTemplate> {
        None
    }

    /// Best acquisition (sync) sample offset of a frame within `samples`, for a multi-copy receiver that
    /// needs to *anchor* copy slots on acquisition rather than broadband energy. `None` (the default) when
    /// the plugin exposes no such hook. Used by the MFSK16 K=3 sub-floor ACK union decoder.
    fn acquire_copy_offset(&self, _samples: &[f32], _config: &ModulationConfig) -> Option<usize> {
        None
    }

    /// Occupied RF bandwidth (Hz) of `mode`, used to size a receiver notch's protected band
    /// so it never notches this signal.  `None` if the plugin can't report it (the caller then
    /// falls back to a conservative default).  Default implementation returns `None`.
    fn occupied_bandwidth_hz(&self, _mode: &str) -> Option<f32> {
        None
    }

    /// Encode `data` bytes and return baseband I and Q sample vectors.
    ///
    /// The returned vectors have the same length.  `I` maps to the left
    /// channel and `Q` to the right channel of a stereo audio output, which
    /// an SDR upconverts directly to RF with exact sideband suppression.
    ///
    /// The default implementation wraps [`modulate`](Self::modulate) via a
    /// Hilbert-transform baseband shift.  Plugins with a native complex-baseband
    /// path (BPSK, QPSK) override this for efficiency and accuracy.
    fn modulate_iq(
        &self,
        data: &[u8],
        config: &ModulationConfig,
    ) -> Result<(Vec<f32>, Vec<f32>), ModemError> {
        let real = self.modulate(data, config)?;
        let (i_bb, q_bb) =
            crate::iq::hilbert_iq(&real, config.center_frequency, config.sample_rate as f32);
        Ok((i_bb, q_bb))
    }
}

// ── Plugin registry ───────────────────────────────────────────────────────────

/// A runtime collection of modulation plugins.
///
/// Plugins are registered once at startup and then looked up by mode string.
#[derive(Default)]
pub struct PluginRegistry {
    plugins: Vec<Box<dyn ModulationPlugin>>,
}

impl PluginRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a plugin, validating trait version compatibility.
    /// Later registrations shadow earlier ones for the same mode string.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the plugin's `trait_version_required` is incompatible
    /// with the framework's `PLUGIN_TRAIT_VERSION`.
    pub fn register(&mut self, plugin: Box<dyn ModulationPlugin>) -> Result<(), PluginError> {
        let info = plugin.info();
        Self::validate_trait_version(info)?;
        self.plugins.push(plugin);
        Ok(())
    }

    /// Validate that a plugin's trait version is compatible with the framework.
    fn validate_trait_version(info: &PluginInfo) -> Result<(), PluginError> {
        let plugin_parts: Vec<&str> = info.trait_version_required.split('.').collect();
        if plugin_parts.len() != 2 {
            return Err(PluginError::InvalidTraitVersionFormat(
                info.trait_version_required.clone(),
            ));
        }

        let plugin_major = plugin_parts[0].parse::<u32>().map_err(|_| {
            PluginError::InvalidTraitVersionFormat(info.trait_version_required.clone())
        })?;
        let plugin_minor = plugin_parts[1].parse::<u32>().map_err(|_| {
            PluginError::InvalidTraitVersionFormat(info.trait_version_required.clone())
        })?;

        let (fw_major_str, fw_rest) = PLUGIN_TRAIT_VERSION.split_once('.').ok_or_else(|| {
            PluginError::InvalidTraitVersionFormat(PLUGIN_TRAIT_VERSION.to_string())
        })?;
        let framework_major = fw_major_str.parse::<u32>().map_err(|_| {
            PluginError::InvalidTraitVersionFormat(PLUGIN_TRAIT_VERSION.to_string())
        })?;
        let framework_minor = fw_rest
            .split_once('.')
            .map_or(fw_rest, |(m, _)| m)
            .parse::<u32>()
            .map_err(|_| {
                PluginError::InvalidTraitVersionFormat(PLUGIN_TRAIT_VERSION.to_string())
            })?;

        // Compatible if: framework major == plugin major AND framework minor >= plugin minor
        if plugin_major != framework_major || framework_minor < plugin_minor {
            return Err(PluginError::IncompatibleTraitVersion {
                plugin: info.name.clone(),
                required: info.trait_version_required.clone(),
                current: PLUGIN_TRAIT_VERSION.to_string(),
            });
        }

        Ok(())
    }

    /// Look up the first plugin that supports `mode`.
    pub fn get(&self, mode: &str) -> Option<&dyn ModulationPlugin> {
        self.plugins
            .iter()
            .rev() // later registrations take precedence
            .find(|p| p.supports_mode(mode))
            .map(|p| p.as_ref())
    }

    /// Return metadata for every registered plugin.
    pub fn list(&self) -> Vec<&PluginInfo> {
        self.plugins.iter().map(|p| p.info()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal plugin for exercising the registry + trait defaults.
    struct FakePlugin {
        info: PluginInfo,
        bytes: Vec<u8>,
    }

    impl FakePlugin {
        fn new(name: &str, modes: &[&str], trait_req: &str) -> Self {
            Self {
                info: PluginInfo {
                    name: name.to_string(),
                    version: "1.0.0".to_string(),
                    description: "test plugin".to_string(),
                    author: "test".to_string(),
                    supported_modes: modes.iter().map(|s| s.to_string()).collect(),
                    trait_version_required: trait_req.to_string(),
                },
                bytes: vec![0xA5, 0x3C],
            }
        }
        fn boxed(name: &str, modes: &[&str], trait_req: &str) -> Box<dyn ModulationPlugin> {
            Box::new(Self::new(name, modes, trait_req))
        }
    }

    impl ModulationPlugin for FakePlugin {
        fn info(&self) -> &PluginInfo {
            &self.info
        }
        fn modulate(&self, data: &[u8], _c: &ModulationConfig) -> Result<Vec<f32>, ModemError> {
            Ok(data.iter().map(|&b| b as f32).collect())
        }
        fn demodulate(&self, _s: &[f32], _c: &ModulationConfig) -> Result<Vec<u8>, ModemError> {
            Ok(self.bytes.clone())
        }
    }

    /// The framework's own major version, so fixtures cannot drift from it.
    ///
    /// These were hardcoded to `"2.0"` and silently became major-incompatible when
    /// `PLUGIN_TRAIT_VERSION` went to `"3.0.0"` (#1074). Three of the four failures
    /// were collateral — tests that merely register a plugin on the way to checking
    /// lookup or shadowing — so a version bump disabled unrelated coverage.
    fn fw_major() -> u32 {
        PLUGIN_TRAIT_VERSION
            .split('.')
            .next()
            .and_then(|m| m.parse().ok())
            .expect("PLUGIN_TRAIT_VERSION must start with a numeric major")
    }

    /// A version a plugin can declare that the current framework accepts.
    fn compatible_version() -> String {
        format!("{}.0", fw_major())
    }

    #[test]
    fn register_then_lookup_is_case_insensitive_and_misses_are_none() {
        let mut reg = PluginRegistry::new();
        reg.register(FakePlugin::boxed(
            "BPSK",
            &["BPSK31", "BPSK100"],
            &compatible_version(),
        ))
        .unwrap();
        assert!(reg.get("BPSK31").is_some());
        assert!(
            reg.get("bpsk100").is_some(),
            "lookup must be case-insensitive"
        );
        assert!(reg.get("QPSK500").is_none(), "unknown mode must miss");
        assert_eq!(reg.list().len(), 1);
    }

    #[test]
    fn later_registration_shadows_earlier_for_the_same_mode() {
        let mut reg = PluginRegistry::new();
        reg.register(FakePlugin::boxed("OLD", &["BPSK31"], &compatible_version()))
            .unwrap();
        reg.register(FakePlugin::boxed("NEW", &["BPSK31"], &compatible_version()))
            .unwrap();
        // `get` walks registrations in reverse, so the newer plugin wins.
        assert_eq!(reg.get("BPSK31").unwrap().info().name, "NEW");
        assert_eq!(reg.list().len(), 2);
    }

    #[test]
    fn compatible_trait_version_registers() {
        let mut reg = PluginRegistry::new();
        // A plugin declaring the framework's own major at minor 0 is compatible.
        let v = compatible_version();
        assert!(reg.register(FakePlugin::boxed("OK", &["X"], &v)).is_ok());
    }

    #[test]
    fn incompatible_major_trait_version_is_rejected() {
        let mut reg = PluginRegistry::new();
        // One major ABOVE the framework, derived — a literal here is what silently
        // turned into the *compatible* version when the framework caught up (#1074).
        let future = format!("{}.0", fw_major() + 1);
        let err = reg
            .register(FakePlugin::boxed("FUTURE", &["X"], &future))
            .unwrap_err();
        assert!(matches!(err, PluginError::IncompatibleTraitVersion { .. }));
    }

    #[test]
    fn higher_minor_than_framework_is_rejected() {
        let mut reg = PluginRegistry::new();
        // Framework minor is 0; a plugin requiring minor 5 needs a newer framework. The major must
        // MATCH here, or this passes on the major check and says nothing about the minor one — which
        // is exactly what the hardcoded "2.5" had been doing since the framework moved to 3.0.0.
        let higher_minor = format!("{}.5", fw_major());
        let err = reg
            .register(FakePlugin::boxed("NEWER", &["X"], &higher_minor))
            .unwrap_err();
        assert!(matches!(err, PluginError::IncompatibleTraitVersion { .. }));
    }

    #[test]
    fn malformed_trait_version_is_rejected() {
        let mut reg = PluginRegistry::new();
        for bad in ["1", "1.0.0", "x.y", ""] {
            let err = reg
                .register(FakePlugin::boxed("BAD", &["X"], bad))
                .unwrap_err();
            assert!(
                matches!(err, PluginError::InvalidTraitVersionFormat(_)),
                "version {bad:?} should be a format error"
            );
        }
    }

    #[test]
    fn default_demodulate_soft_hard_slices_back_to_demodulate() {
        // The default soft path maps each bit to ±1.0; hard-slicing must reproduce the
        // demodulate() byte stream exactly. Sliced with the PRODUCT's function rather than a
        // re-implementation — this test open-coded `llr <= 0.0`, which is not what the engine
        // uses and would not have noticed the engine's slicer changing.
        let p = FakePlugin::new("BPSK", &["BPSK31"], "1.0");
        let cfg = ModulationConfig::default();
        let hard = p.demodulate(&[], &cfg).unwrap();
        let llrs = p.demodulate_soft(&[], &cfg).unwrap();
        assert_eq!(llrs.len(), hard.len() * 8);
        assert_eq!(crate::fec::hard_decide(&llrs), hard);
        // Defaults for the opt-in trait hooks.
        assert!(!p.supports_soft_demod("BPSK31"));
        assert!(p.frame_geometry(&cfg).is_none());
        assert!(p.estimate_afc_hz(&[], &cfg).is_none());
        assert!(p.occupied_bandwidth_hz("BPSK31").is_none());
    }
}
