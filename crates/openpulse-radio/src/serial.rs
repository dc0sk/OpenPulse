#[cfg(feature = "serial")]
use crate::PttController;
use crate::PttError;

/// Serial RTS/DTR PTT controller. Requires the `serial` feature.
///
/// `pin` selects which serial control line drives PTT:
/// - `"rts"` — Request To Send
/// - `"dtr"` — Data Terminal Ready
#[cfg(feature = "serial")]
pub struct SerialRtsDtrPtt {
    port: Box<dyn serialport::SerialPort>,
    pin: SerialPin,
    asserted: bool,
}

/// Which serial control line drives PTT.
///
/// Unconditional: it is a plain two-variant enum with no dependency on `serialport`, and gating the
/// TYPE on the feature meant a caller could not even name the pin in a build without it — which is
/// why the feature-off `open` below could not be written until now.
#[derive(Debug, Clone, Copy)]
pub enum SerialPin {
    Rts,
    Dtr,
}

#[cfg(feature = "serial")]
impl SerialRtsDtrPtt {
    pub fn open(path: &str, pin: SerialPin) -> Result<Self, PttError> {
        let port = serialport::new(path, 9600)
            .open()
            .map_err(|e| PttError::Serial(e.to_string()))?;
        Ok(Self {
            port,
            pin,
            asserted: false,
        })
    }
}

#[cfg(feature = "serial")]
impl PttController for SerialRtsDtrPtt {
    fn assert_ptt(&mut self) -> Result<(), PttError> {
        match self.pin {
            SerialPin::Rts => self
                .port
                .write_request_to_send(true)
                .map_err(|e| PttError::Serial(e.to_string()))?,
            SerialPin::Dtr => self
                .port
                .write_data_terminal_ready(true)
                .map_err(|e| PttError::Serial(e.to_string()))?,
        }
        self.asserted = true;
        Ok(())
    }

    fn release_ptt(&mut self) -> Result<(), PttError> {
        match self.pin {
            SerialPin::Rts => self
                .port
                .write_request_to_send(false)
                .map_err(|e| PttError::Serial(e.to_string()))?,
            SerialPin::Dtr => self
                .port
                .write_data_terminal_ready(false)
                .map_err(|e| PttError::Serial(e.to_string()))?,
        }
        self.asserted = false;
        Ok(())
    }

    fn is_asserted(&self) -> bool {
        self.asserted
    }
}

// Stub so the module compiles without the feature. It carries an `open` that ERRORS rather than
// not existing, mirroring `GpioPtt::open` — so callers need no `#[cfg]` of their own, and the
// not-compiled-in path is reachable (and testable) in the default `--no-default-features` build
// instead of being invisible to it.
#[cfg(not(feature = "serial"))]
pub struct SerialRtsDtrPtt {
    _priv: (),
}

#[cfg(not(feature = "serial"))]
impl SerialRtsDtrPtt {
    /// Always an error without the `serial` feature.
    pub fn open(path: &str, pin: SerialPin) -> Result<Self, PttError> {
        let _ = (path, pin);
        Err(PttError::Config(
            "serial PTT not compiled in; rebuild with --features serial".into(),
        ))
    }
}

// The stub satisfies the trait so callers need no `#[cfg]` of their own. Every method is
// unreachable — `open` above is the only constructor and it always errors — but they return `Err`
// rather than panicking, because this is a library production path and an `unreachable!()` here
// would be a panic on runtime data if that ever stopped being true.
#[cfg(not(feature = "serial"))]
impl crate::PttController for SerialRtsDtrPtt {
    fn assert_ptt(&mut self) -> Result<(), PttError> {
        Err(PttError::Config("serial PTT not compiled in".into()))
    }
    fn release_ptt(&mut self) -> Result<(), PttError> {
        Err(PttError::Config("serial PTT not compiled in".into()))
    }
    fn is_asserted(&self) -> bool {
        false
    }
}

#[cfg(all(test, feature = "serial"))]
mod tests {
    use super::*;

    /// Audit H2: exercise the `SerialRtsDtrPtt` backend's construction/error path (it was previously
    /// never instantiated in any test). Opening a non-existent serial device must return an error,
    /// not panic.
    #[test]
    fn open_on_a_nonexistent_device_errors() {
        let r = SerialRtsDtrPtt::open("/dev/nonexistent-openpulse-tty-xyz", SerialPin::Rts);
        assert!(r.is_err(), "opening a missing serial device must error");
    }
}
