//! One held-open capture stream, read a tick at a time and folded into the engine's burst
//! accumulator (#1297).
//!
//! Its consumer is the cross-band repeater's CARRIER SENSE (#1325): the repeater no longer captures
//! its input — the daemon hands it bursts (#1308) — but it must listen to rig_b's *output* band
//! before keying it, and this is what holds that stream across relays. It was briefly dormant
//! between those two changes. #1310 (adopting it in the ARDOP and KISS front-ends, which still open
//! a stream, read once and drop it) remains open and is a second consumer, not the only one.
//!
//! **Why this exists rather than `ModemEngine::receive`.** `receive` opens an input stream, reads
//! once, and drops the stream — so on a callback backend each call sees a fresh, nearly-empty buffer
//! covering one poll interval. A frame is seconds long; a poll is tens of milliseconds. Any caller
//! that listens *continuously* therefore cannot receive at all on real audio, however long it runs,
//! and `capture_burst`'s own comment records the reason: a fresh cpal stream needs tens of ms to
//! start delivering, so reopening per tick never warms up.
//!
//! `LoopbackBackend` hides this completely — its `read` drains the whole buffer, so the buffer IS
//! the frame and `receive` works. That is why the defect survived a green suite.
//!
//! The daemon's `rx_ticker` already does the right thing by hand. This is that pattern, owned once:
//! open lazily, read, accumulate, and on a fault drop the stream so the next tick reopens it —
//! reporting the first failure at WARN, because a station that cannot hear is off the air and
//! `receive`'s error path made that indistinguishable from a quiet band.
//!
//! **Twin copies that should adopt this.** `server.rs`'s `rx_ticker` (`:886-1030`) is the original,
//! open-coded, and still carries its own `block_in_place` wrapper, discovery tee and logging — it is
//! left alone here because refactoring a working receive path inside a PR that fixes a broken one
//! trades risk for tidiness. `openpulse-ardop` (`bridge.rs:430,437,557,564`) and `openpulse-kiss`
//! (`bridge.rs:237,256`) call `receive` in free-running loops and have the SAME structural defect
//! this fixes — the assumption that their TCP-driven shape makes a per-call window correct is false;
//! both poll continuously. Tracked as a follow-up rather than fixed blind.

use openpulse_core::audio::AudioInputStream;
use openpulse_core::error::ModemError;

use crate::engine::ModemEngine;
use crate::pipeline::AudioSamples;

/// What one tick produced.
pub struct Tick {
    /// A burst, when the accumulator flushed one this tick.
    pub burst: Option<AudioSamples>,
    /// The raw samples this tick read, before the RX front end — for callers that tee the audio
    /// (the daemon's JS8 discovery dwell). Empty when the read produced nothing.
    pub raw: Vec<f32>,
}

/// A capture stream held across ticks, feeding [`ModemEngine::accumulate_capture`].
pub struct CaptureTicker {
    stream: Option<Box<dyn AudioInputStream>>,
    device: Option<String>,
    /// Whether the last read or open failed, so the recurring case logs at DEBUG and the first
    /// failure and the recovery each log at WARN.
    failed: bool,
}

impl CaptureTicker {
    /// Capture from `device`, or the engine's default when `None`.
    pub fn new(device: Option<String>) -> Self {
        Self {
            stream: None,
            device,
            failed: false,
        }
    }

    /// Whether the stream is currently faulted; the next tick will try to reopen.
    pub fn is_faulted(&self) -> bool {
        self.failed
    }

    /// Read one tick and fold it into `engine`'s burst accumulator.
    ///
    /// Never returns the open/read error: a capture fault is reported and retried, not propagated,
    /// because a caller that treats it as fatal stops listening for good. Decode-side errors are the
    /// caller's business and reach it through the returned burst.
    pub fn tick(&mut self, engine: &mut ModemEngine, mode: &str) -> Tick {
        if self.stream.is_none() {
            match engine.open_capture_stream(self.device.as_deref()) {
                Ok(s) => {
                    if self.failed {
                        self.failed = false;
                        tracing::warn!("audio capture recovered");
                    }
                    self.stream = Some(s);
                }
                Err(e) => {
                    self.note_fault(&e, "cannot open the capture device");
                    return Tick {
                        burst: None,
                        raw: Vec::new(),
                    };
                }
            }
        }

        let read = match self.stream.as_mut() {
            Some(s) => s.read(),
            None => {
                return Tick {
                    burst: None,
                    raw: Vec::new(),
                }
            }
        };

        match read {
            Ok(samples) => {
                if self.failed {
                    self.failed = false;
                    tracing::warn!("audio capture recovered");
                }
                let raw = samples.clone();
                let burst = engine
                    .accumulate_capture(Some(mode), samples)
                    .unwrap_or_else(|e| {
                        // The front end failed on this block; not a capture fault, and not worth
                        // dropping the stream for.
                        tracing::debug!(error = %e, "capture accumulate failed for one block");
                        None
                    });
                Tick { burst, raw }
            }
            Err(e) => {
                // Drop the stream so the next tick reopens it.
                self.stream = None;
                self.note_fault(
                    &ModemError::Audio(e.to_string()),
                    "audio capture read failed",
                );
                Tick {
                    burst: None,
                    raw: Vec::new(),
                }
            }
        }
    }

    fn note_fault(&mut self, e: &ModemError, what: &str) {
        if self.failed {
            tracing::debug!(error = %e, "{what}; still failing, will retry");
        } else {
            self.failed = true;
            // WARN, not DEBUG: this is the state in which a station hears nothing at all, and it
            // must not look like a quiet band.
            tracing::warn!(error = %e, "{what}; retrying on the next tick");
        }
    }
}
