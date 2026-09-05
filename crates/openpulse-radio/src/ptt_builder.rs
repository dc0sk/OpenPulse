//! The one place a `[modem] ptt_backend` string becomes a `PttController` (#1258).
//!
//! Before this there were **four** hand-rolled backend matches, drifted apart: the daemon's seven
//! backends, the CLI's eight (it alone has `generic`), ARDOP's three — `rts`, `dtr`, `cm108` and
//! `gpio` fell through to `NoOpPtt` there — and the cross-band repeater's rigctld-only one. KISS had
//! none at all and never read `ptt_backend`, so an operator running the APRS path with
//! `ptt_backend = "rigctld"` played audio into an unkeyed transceiver, silently (#1259).
//!
//! Two design points worth keeping:
//!
//! **The error type separates "cannot honour this" from "the attempt failed."** `PttError::Config`
//! means an unknown name, a feature not compiled in, or a missing required device path — a typo the
//! operator must fix. Everything else means the backend is real and the attempt failed, which may
//! just be a rig that is not powered up yet. Callers must not collapse the two: doing so is what let
//! a mistyped `ptt_backend` start a daemon that then transmitted into an unkeyed rig (#1285).
//!
//! **No `#[cfg]` in this file.** `GpioPtt::open` and `SerialRtsDtrPtt::open` each return
//! `PttError::Config` when their feature is absent, so the not-compiled-in path is ordinary code
//! that runs — and is testable — in the default `--no-default-features` build. Gating here would put
//! the cfg in a crate whose feature the *callers* must forward, which is precisely the trap that
//! made `rts`/`dtr`/`gpio` inert on every documented build recipe.

use crate::{Cm108Ptt, GpioPtt, NoOpPtt, PttController, PttError, RigctldPtt, VoxPtt};

/// Everything the backends need, flattened. Deliberately plain strings rather than a config type:
/// `openpulse-radio` depends on neither `openpulse-config` nor anything modem-side, and adding that
/// edge to get a typed field would invert the layering for no gain.
#[derive(Debug, Clone, Default)]
pub struct PttSpec<'a> {
    /// `[modem] ptt_backend`.
    pub backend: &'a str,
    /// `[radio] rigctld_addr`, for the `rigctld` backend.
    pub rigctld_addr: &'a str,
    /// `[modem] ptt_device` — a `/dev/hidrawN` path for `cm108`, a serial port for `rts`/`dtr`, a
    /// `chip:line` spec for `gpio`.
    pub device: &'a str,
    /// `[modem] ptt_gpio`, the CM108 GPIO pin (1..=8).
    pub gpio_pin: u8,
}

/// Build the configured PTT controller.
///
/// `Ok(None)` means **and only means** the operator asked for no PTT (`"none"`). `Err` splits into
/// `PttError::Config` (unusable configuration) and the rest (a real backend that failed); see the
/// module docs for why callers must keep those apart.
pub fn build_ptt(spec: &PttSpec<'_>) -> Result<Option<Box<dyn PttController + Send>>, PttError> {
    match spec.backend {
        "none" | "" => Ok(None),
        "vox" => Ok(Some(Box::new(VoxPtt::new()))),
        "rigctld" => Ok(Some(Box::new(RigctldPtt::connect(spec.rigctld_addr)?))),
        "cm108" => Ok(Some(Box::new(Cm108Ptt::open(spec.device, spec.gpio_pin)?))),
        "gpio" => Ok(Some(Box::new(GpioPtt::open(spec.device)?))),
        "rts" | "dtr" => {
            if spec.device.is_empty() {
                return Err(PttError::Config(format!(
                    "the `{}` PTT backend requires [modem] ptt_device (the serial port path)",
                    spec.backend
                )));
            }
            let pin = if spec.backend == "rts" {
                crate::serial::SerialPin::Rts
            } else {
                crate::serial::SerialPin::Dtr
            };
            Ok(Some(Box::new(crate::serial::SerialRtsDtrPtt::open(
                spec.device,
                pin,
            )?)))
        }
        other => Err(PttError::Config(format!(
            "unknown PTT backend `{other}`; expected one of none, vox, rigctld, cm108, gpio, rts, dtr"
        ))),
    }
}

/// A controller that never keys, for callers that have decided to carry on without PTT.
///
/// Named rather than inlined so the decision is greppable: handing this to `SharedPtt` after a
/// *failure* is the fail-open shape #1285 is about, and it should be visible at the call site.
pub fn no_ptt() -> Box<dyn PttController + Send> {
    Box::new(NoOpPtt::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(backend: &str) -> PttSpec<'_> {
        PttSpec {
            backend,
            ..Default::default()
        }
    }

    /// Only `"none"` yields `Ok(None)`. Everything else is a controller or an error — never the
    /// "no PTT configured" state, which is what a caller keys off.
    #[test]
    fn only_none_means_no_ptt_was_configured() {
        assert!(build_ptt(&spec("none")).expect("none is legal").is_none());
        assert!(build_ptt(&spec("")).expect("empty is legal").is_none());
        assert!(build_ptt(&spec("vox")).expect("vox is legal").is_some());
    }

    /// The #1258 defect: ARDOP accepted three backends and silently degraded four documented ones.
    ///
    /// Each of the four must now be *recognised* — reaching an open/connect attempt, or a
    /// `Config` error naming a real reason. What must NOT happen is the old outcome: falling through
    /// the match as if the name were unknown.
    #[test]
    fn the_four_backends_ardop_used_to_drop_are_recognised() {
        for backend in ["rts", "dtr", "cm108", "gpio"] {
            match build_ptt(&spec(backend)) {
                // No device is configured in this fixture, so success is not expected; what matters
                // is WHICH error comes back.
                Err(PttError::Config(msg)) => assert!(
                    !msg.contains("unknown PTT backend"),
                    "`{backend}` is a documented backend and must not be reported as unknown — \
                     that is the #1258 defect: it fell through to NoOpPtt. Got: {msg}"
                ),
                Err(_) => {} // reached a real open/connect attempt and it failed: recognised
                Ok(_) => {}  // a machine that actually has the device
            }
        }
    }

    /// An unknown name is a `Config` error, and names the alternatives.
    #[test]
    fn an_unknown_backend_is_a_config_error_not_a_silent_noop() {
        let Err(err) = build_ptt(&spec("rigctl")) else {
            panic!("a typo must not succeed");
        };
        assert!(
            matches!(err, PttError::Config(_)),
            "an unknown backend is a configuration error, distinct from a backend that exists and \
             failed — the daemon needs that distinction to decide whether to refuse start (#1285)"
        );
        assert!(
            err.to_string().contains("rigctld"),
            "the message must name the alternatives"
        );
    }

    /// `rts`/`dtr` without a device path is a config error, not an open attempt on "".
    #[test]
    fn serial_without_a_device_path_is_a_config_error() {
        let Err(err) = build_ptt(&spec("rts")) else {
            panic!("rts with no device path must not succeed");
        };
        assert!(matches!(err, PttError::Config(_)));
    }
}
