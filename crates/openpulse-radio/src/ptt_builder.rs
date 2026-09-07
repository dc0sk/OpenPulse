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

/// Seconds between reconnect attempts for a backend that is configured but currently unusable.
///
/// Bounded so a dead rigctld is not hammered: with a 50 ms rx tick, an unbounded retry would attempt
/// a TCP connect 20 times a second for as long as the rig stays down.
const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// A configured backend that is not currently usable — refuses every key, and keeps trying (#1285).
///
/// **This exists because `None` meant two different things.** `build_ptt` used to collapse every
/// failure to "no controller", and `SharedPtt::key` skips the hardware assert entirely when there is
/// no controller — so it *succeeds*, arms the watchdog, and the caller transmits. That made
/// `ptt_backend = "none"` (the operator wants VOX or a manual key) indistinguishable from "rigctld
/// is down", and in the second case the daemon played audio into an unkeyed rig believing it had
/// transmitted.
///
/// Refusing is what makes the existing machinery correct without any new policy: `keyed_transmit`
/// already skips an emission whose assert fails, so **nothing is ever radiated unkeyed**; and the
/// failure is a hardware `Assert`, not `AlreadyKeyed`, so the station ID marks rather than defers and
/// a broken rig is not re-attempted at the tick rate (#1263 F3).
///
/// The retry lives **inside the controller** rather than in a background thread or a hot-swap of
/// `SharedPtt`'s field. That was the first design and it was worse: the controller sits behind the
/// same mutex the watchdog takes, so swapping it needs lock surgery on the one path that must stay
/// preemptible. Here the reconnect is attempted lazily, at the moment an emission wants the
/// transmitter — which is exactly when it matters — and rate-limited so a dead rig costs one connect
/// attempt per `RETRY_INTERVAL`.
pub struct RetryingPtt {
    spec: OwnedPttSpec,
    inner: Option<Box<dyn PttController + Send>>,
    last_attempt: Option<std::time::Instant>,
}

/// An owned [`PttSpec`], so a controller can rebuild itself later.
#[derive(Debug, Clone, Default)]
pub struct OwnedPttSpec {
    pub backend: String,
    pub rigctld_addr: String,
    pub device: String,
    pub gpio_pin: u8,
}

impl OwnedPttSpec {
    fn as_spec(&self) -> PttSpec<'_> {
        PttSpec {
            backend: &self.backend,
            rigctld_addr: &self.rigctld_addr,
            device: &self.device,
            gpio_pin: self.gpio_pin,
        }
    }
}

impl RetryingPtt {
    /// Wrap a spec whose backend is real but currently unreachable.
    pub fn new(spec: OwnedPttSpec) -> Self {
        Self {
            spec,
            inner: None,
            last_attempt: Some(std::time::Instant::now()),
        }
    }

    /// Try to (re)build the controller, at most once per [`RETRY_INTERVAL`].
    fn ensure(&mut self) -> Result<(), PttError> {
        if self.inner.is_some() {
            return Ok(());
        }
        if let Some(t) = self.last_attempt {
            if t.elapsed() < RETRY_INTERVAL {
                return Err(PttError::Rigctld(format!(
                    "PTT backend `{}` is unavailable; retrying",
                    self.spec.backend
                )));
            }
        }
        self.last_attempt = Some(std::time::Instant::now());
        match build_ptt(&self.spec.as_spec()) {
            Ok(Some(c)) => {
                tracing::info!(backend = %self.spec.backend, "PTT backend reconnected");
                self.inner = Some(c);
                Ok(())
            }
            // `Ok(None)` cannot happen: this wrapper is only built for a backend that is not
            // `"none"`. Treat it as unavailable rather than as success, so a future change to
            // `build_ptt` cannot silently turn a refusal into a permitted unkeyed transmit.
            Ok(None) => Err(PttError::Config(format!(
                "PTT backend `{}` resolved to no controller",
                self.spec.backend
            ))),
            Err(e) => {
                tracing::warn!(backend = %self.spec.backend, error = %e, "PTT reconnect failed");
                Err(e)
            }
        }
    }
}

impl PttController for RetryingPtt {
    fn assert_ptt(&mut self) -> Result<(), PttError> {
        self.ensure()?;
        match self.inner.as_mut() {
            Some(c) => c.assert_ptt().inspect_err(|_| {
                // A live controller that fails mid-session is dropped, so the next emission
                // reconnects rather than keying a handle the rig no longer honours.
                self.inner = None;
            }),
            None => Err(PttError::Rigctld("PTT backend unavailable".into())),
        }
    }

    fn release_ptt(&mut self) -> Result<(), PttError> {
        // A release with no working controller is Ok, not an error: nothing is keyed, so there is
        // nothing to drop, and returning Err here would make every guard's Drop log a failure.
        match self.inner.as_mut() {
            Some(c) => c.release_ptt(),
            None => Ok(()),
        }
    }

    fn is_asserted(&self) -> bool {
        self.inner
            .as_ref()
            .map(|c| c.is_asserted())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use crate::SharedPtt;
    use std::time::Duration;

    fn unreachable_spec() -> OwnedPttSpec {
        OwnedPttSpec {
            backend: "rigctld".into(),
            // Port 1 on loopback: reserved, nothing listens, connect fails fast.
            rigctld_addr: "127.0.0.1:1".into(),
            ..Default::default()
        }
    }

    /// The #1285 defect: an unreachable backend used to become `None`, and `None` KEYS SUCCESSFULLY.
    #[test]
    fn an_unreachable_backend_refuses_the_key_instead_of_permitting_an_unkeyed_transmit() {
        let ptt = SharedPtt::new(
            Some(Box::new(RetryingPtt::new(unreachable_spec()))),
            Duration::from_secs(180),
        );
        assert!(
            ptt.keyed(None).is_err(),
            "an unreachable PTT backend must REFUSE the key — the emission is then skipped by \
             keyed_transmit, so nothing is radiated unkeyed"
        );
        assert!(!ptt.is_keyed(), "and nothing may be left armed");
    }

    /// The control that makes the test above mean something: `"none"` still keys.
    ///
    /// Without this, a build where `SharedPtt` refused everything would pass the assertion above
    /// while breaking every VOX and manually-keyed station.
    #[test]
    fn no_ptt_configured_still_keys() {
        let ptt = SharedPtt::new(Some(no_ptt()), Duration::from_secs(180));
        assert!(
            ptt.keyed(None).is_ok(),
            "`ptt_backend = \"none\"` means the operator uses VOX or keys manually — it must not be \
             confused with a backend that is broken, which is exactly the conflation #1285 fixes"
        );
    }

    /// A dead rig costs one connect attempt per interval, not one per emission.
    #[test]
    fn the_retry_is_rate_limited() {
        let mut c = RetryingPtt::new(unreachable_spec());
        // The first attempt happens at construction time, so an immediate assert must NOT reconnect
        // — it reports unavailable from the rate limiter instead. With a 50 ms rx tick an unbounded
        // retry would attempt a TCP connect 20 times a second for as long as the rig stays down.
        let before = c.last_attempt;
        assert!(c.assert_ptt().is_err());
        assert_eq!(
            c.last_attempt, before,
            "a second assert inside RETRY_INTERVAL must not launch another connect"
        );
    }

    /// The refusal is an `Assert`-class fault, not `AlreadyKeyed` — so the station ID MARKS.
    ///
    /// If this ever became `AlreadyKeyed`, the ID would defer instead, and a broken rig would be
    /// re-attempted at the 50 ms tick rate for the whole 180 s watchdog window (#1263 F3).
    #[test]
    fn the_refusal_is_not_mistaken_for_a_busy_rig() {
        let mut c = RetryingPtt::new(unreachable_spec());
        let e = c.assert_ptt().expect_err("unreachable");
        assert!(
            !matches!(e, PttError::AlreadyKeyed { .. }),
            "an unreachable backend must not report AlreadyKeyed: the station ID defers on that and \
             would retry a dead rig every tick"
        );
    }
}
