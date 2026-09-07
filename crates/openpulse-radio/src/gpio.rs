//! Linux GPIO-line PTT (REQ-PTT-03), e.g. a Raspberry Pi header pin.
//!
//! Keying drives a single GPIO output line via the kernel character-device uAPI (`gpiocdev`, pure Rust —
//! no libgpiod C linkage, so the `cross`-compile gate stays clean). The real line request is behind the
//! `gpio` feature; the keying *logic* (active-low inversion, assert/release/idempotency) sits behind a
//! small `PttLine` trait so it is fully unit-testable with a mock, with **zero** hardware and no feature.
//!
//! Config spec (in `[modem] ptt_device`): `chip:line[:active_low]`, e.g. `gpiochip0:17` or
//! `gpiochip0:17:active_low` (many PTT interface boards pull the rig down through an inverting driver).

use crate::{PttController, PttError};

/// A single GPIO output line. `set` writes the **physical** level (active-low inversion is applied by
/// [`GpioPtt`], so a mock records the electrical level and the polarity logic is testable).
trait PttLine: Send {
    fn set(&mut self, high: bool) -> Result<(), PttError>;
}

/// Parse a `chip:line[:active_low]` spec into `(chip, line_offset, active_low)`.
fn parse_gpio_spec(spec: &str) -> Result<(String, u32, bool), PttError> {
    let bad = || {
        // Config, not Serial (#1285): a spec that does not PARSE can never self-heal, so it must
        // refuse startup rather than be retried forever. A well-formed spec whose device is simply
        // absent is the opposite case — a USB adapter can be plugged in later — and stays a
        // transient error the retrying controller keeps attempting.
        PttError::Config(format!(
            "GPIO spec '{spec}' must be chip:line[:active_low], e.g. gpiochip0:17"
        ))
    };
    let mut parts: Vec<&str> = spec.split(':').collect();
    let mut active_low = false;
    if matches!(parts.last(), Some(&"active_low") | Some(&"al")) {
        active_low = true;
        parts.pop();
    }
    if parts.len() < 2 {
        return Err(bad());
    }
    let offset_str = parts.pop().ok_or_else(bad)?;
    let offset: u32 = offset_str
        .parse()
        .map_err(|_| PttError::Serial(format!("GPIO line '{offset_str}' is not a number")))?;
    let chip = parts.join(":");
    if chip.is_empty() {
        return Err(bad());
    }
    Ok((chip, offset, active_low))
}

/// GPIO-line PTT controller.
pub struct GpioPtt {
    line: Box<dyn PttLine>,
    active_low: bool,
    asserted: bool,
}

impl GpioPtt {
    /// Open a GPIO line from a `chip:line[:active_low]` spec (see the module docs) and key PTT on it.
    /// Requires the `gpio` feature; without it, returns an error. Leaves PTT released.
    pub fn open(spec: &str) -> Result<Self, PttError> {
        let (chip, offset, active_low) = parse_gpio_spec(spec)?;
        #[cfg(feature = "gpio")]
        {
            let line = CdevLine::request(&chip, offset)?;
            let mut ctrl = Self::with_line(Box::new(line), active_low);
            ctrl.release_ptt()?; // ensure the physical line starts in the released state
            Ok(ctrl)
        }
        #[cfg(not(feature = "gpio"))]
        {
            let _ = (chip, offset, active_low);
            Err(PttError::Serial(
                "GPIO PTT not compiled in; rebuild with --features gpio".into(),
            ))
        }
    }

    // Used by the `gpio`-feature `open` path and by tests; dead in a plain feature-off lib build.
    #[cfg_attr(not(feature = "gpio"), allow(dead_code))]
    fn with_line(line: Box<dyn PttLine>, active_low: bool) -> Self {
        Self {
            line,
            active_low,
            asserted: false,
        }
    }

    /// Map a logical assert/release to the physical level and drive the line.
    fn drive(&mut self, asserted: bool) -> Result<(), PttError> {
        // active-low: an asserted (keyed) PTT pulls the line low.
        let physical_high = asserted != self.active_low;
        self.line.set(physical_high)?;
        self.asserted = asserted;
        Ok(())
    }
}

impl PttController for GpioPtt {
    fn assert_ptt(&mut self) -> Result<(), PttError> {
        self.drive(true)
    }

    fn release_ptt(&mut self) -> Result<(), PttError> {
        self.drive(false)
    }

    fn is_asserted(&self) -> bool {
        self.asserted
    }
}

#[cfg(feature = "gpio")]
struct CdevLine {
    req: gpiocdev::Request,
    offset: u32,
}

#[cfg(feature = "gpio")]
impl CdevLine {
    fn request(chip: &str, offset: u32) -> Result<Self, PttError> {
        let chip_path = if chip.starts_with('/') {
            chip.to_string()
        } else {
            format!("/dev/{chip}")
        };
        let req = gpiocdev::Request::builder()
            .on_chip(chip_path.as_str())
            .with_line(offset)
            .as_output(gpiocdev::line::Value::Inactive)
            .request()
            .map_err(|e| PttError::Serial(format!("gpio request failed: {e}")))?;
        Ok(Self { req, offset })
    }
}

#[cfg(feature = "gpio")]
impl PttLine for CdevLine {
    fn set(&mut self, high: bool) -> Result<(), PttError> {
        let v = if high {
            gpiocdev::line::Value::Active
        } else {
            gpiocdev::line::Value::Inactive
        };
        self.req
            .set_value(self.offset, v)
            .map_err(|e| PttError::Serial(format!("gpio set failed: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    /// Records the physical level written on each `set`.
    #[derive(Clone, Default)]
    struct MockLine(Arc<Mutex<Vec<bool>>>);
    impl PttLine for MockLine {
        fn set(&mut self, high: bool) -> Result<(), PttError> {
            self.0.lock().expect("lock").push(high);
            Ok(())
        }
    }

    fn gpio_with(active_low: bool) -> (GpioPtt, MockLine) {
        let mock = MockLine::default();
        (GpioPtt::with_line(Box::new(mock.clone()), active_low), mock)
    }

    #[test]
    fn parse_valid_specs() {
        assert_eq!(
            parse_gpio_spec("gpiochip0:17").unwrap(),
            ("gpiochip0".into(), 17, false)
        );
        assert_eq!(
            parse_gpio_spec("gpiochip0:17:active_low").unwrap(),
            ("gpiochip0".into(), 17, true)
        );
        assert_eq!(
            parse_gpio_spec("/dev/gpiochip1:4:al").unwrap(),
            ("/dev/gpiochip1".into(), 4, true)
        );
    }

    #[test]
    fn parse_rejects_bad_specs() {
        assert!(parse_gpio_spec("gpiochip0").is_err()); // no line
        assert!(parse_gpio_spec("gpiochip0:notanumber").is_err());
        assert!(parse_gpio_spec(":17").is_err()); // empty chip
        assert!(parse_gpio_spec("").is_err());
    }

    #[test]
    fn active_high_drives_the_line_directly() {
        let (mut ptt, mock) = gpio_with(false);
        ptt.assert_ptt().unwrap();
        assert!(ptt.is_asserted());
        ptt.release_ptt().unwrap();
        assert!(!ptt.is_asserted());
        // assert → physical high, release → physical low
        assert_eq!(*mock.0.lock().unwrap(), vec![true, false]);
    }

    #[test]
    fn active_low_inverts_the_physical_level() {
        let (mut ptt, mock) = gpio_with(true);
        ptt.assert_ptt().unwrap();
        ptt.release_ptt().unwrap();
        // assert → physical low (pull down), release → physical high
        assert_eq!(*mock.0.lock().unwrap(), vec![false, true]);
    }

    #[test]
    fn assert_release_round_trip_under_50ms() {
        let (mut ptt, _mock) = gpio_with(false);
        let start = Instant::now();
        ptt.assert_ptt().unwrap();
        ptt.release_ptt().unwrap();
        assert!(
            start.elapsed().as_millis() < 50,
            "PTT round-trip exceeded 50 ms"
        );
    }

    #[test]
    fn open_without_the_feature_errors_cleanly() {
        // Spec parses, but with the `gpio` feature off `open` reports it's not compiled in (not a panic).
        // With the feature on, this instead attempts a real request (which fails without the chip) — still
        // an error, never a panic. Either way: Err.
        assert!(GpioPtt::open("gpiochip-nonexistent-openpulse:17").is_err());
    }
}
