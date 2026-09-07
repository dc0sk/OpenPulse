//! Daemon adapter over the shared PTT safety core.
//!
//! The watchdog, the deadline bookkeeping and the RAII key guard live in
//! [`openpulse_radio::shared_ptt`] so the ARDOP TNC, the KISS TNC and the cross-band repeater — which
//! all key real hardware and had no watchdog at all — can use the same core (audit 2026-07-19,
//! findings #1–#3). A stuck transmitter is a §97 violation and a PA-damage risk; that guarantee
//! belongs at the lowest layer every transmit path already depends on, not in the daemon.
//!
//! This module keeps the daemon's original `Option<&broadcast::Sender<ControlEvent>>` signatures so
//! the call sites are unchanged; it only maps that channel onto the core's
//! [`PttObserver`](openpulse_radio::PttObserver) sink.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::broadcast;

use openpulse_radio::{PttController, PttError, PttObserver};

use crate::protocol::ControlEvent;

pub use openpulse_radio::{PttKeyGuard, UnkeyOutcome, DEFAULT_PTT_MAX};

/// Maps a keyed/unkeyed edge onto the daemon's control-event channel.
struct ChannelObserver(broadcast::Sender<ControlEvent>);

impl PttObserver for ChannelObserver {
    fn ptt_changed(&self, active: bool) {
        let _ = self.0.send(ControlEvent::PttChanged { active });
    }
}

/// Wrap a control-event sender as a PTT observer.
///
/// One small allocation per keyed transition. These happen at transmit rate (a handful per second at
/// worst), never in a sample loop, so the clarity of leaving the call sites untouched is worth more
/// than avoiding the `Arc`.
fn observer(tx: &broadcast::Sender<ControlEvent>) -> Arc<dyn PttObserver> {
    Arc::new(ChannelObserver(tx.clone()))
}

/// PTT hardware + watchdog deadline behind a shared lock. Cheap to `clone` (shares the same lock).
///
/// Thin wrapper over [`openpulse_radio::SharedPtt`]; see that type for the lock discipline and the
/// single-fire / stays-armed-on-failure contracts.
#[derive(Clone, Default)]
pub struct SharedPtt(openpulse_radio::SharedPtt);

impl SharedPtt {
    /// Build from a PTT controller (`None` = no hardware) and the max keyed duration.
    pub fn new(controller: Option<Box<dyn PttController + Send>>, max_duration: Duration) -> Self {
        Self(openpulse_radio::SharedPtt::new(controller, max_duration))
    }

    /// Key the transmitter and arm the watchdog. `event_tx = None` keys silently (the beacon path).
    pub fn key(&self, event_tx: Option<&broadcast::Sender<ControlEvent>>) -> Result<(), PttError> {
        self.0.key(event_tx.map(observer).as_ref())
    }

    /// Release the transmitter and disarm the watchdog. A failed hardware release leaves the watchdog
    /// armed and returns [`UnkeyOutcome::Failed`].
    pub fn unkey(&self, event_tx: Option<&broadcast::Sender<ControlEvent>>) -> UnkeyOutcome {
        self.0.unkey(event_tx.map(observer).as_ref())
    }

    /// Key and return an RAII guard that releases on drop, including on an early return or a
    /// panic/unwind (REQ-PTT-01).
    pub fn keyed(
        &self,
        event_tx: Option<&broadcast::Sender<ControlEvent>>,
    ) -> Result<PttKeyGuard, PttError> {
        self.0.keyed(event_tx.map(observer).as_ref())
    }

    /// Hardware assert only — no deadline change, no event.
    pub fn hw_assert(&self) -> Result<(), PttError> {
        self.0.hw_assert()
    }

    /// Key as the operator's manual hold (#1263) — an owned key, so an automatic emission arriving
    /// mid-hold is refused rather than stealing the deadline.
    pub fn key_as_manual(&self) -> Result<(), PttError> {
        self.0.key_as("manual", None)
    }

    /// Who holds the key, for diagnostics (#1263).
    pub fn held_by(&self) -> &'static str {
        self.0.held_by()
    }

    /// The operator's hard override (#1263): release whoever holds the key.
    pub fn force_release_manual(&self) -> openpulse_radio::shared_ptt::UnkeyOutcome {
        self.0.force_release(None)
    }

    /// Hardware release only — no deadline change, no event.
    pub fn hw_release(&self) -> Result<(), PttError> {
        self.0.hw_release()
    }

    /// Arm the watchdog deadline (deadline only, no hardware).
    pub fn arm(&self) {
        self.0.arm()
    }

    /// Disarm the watchdog deadline (deadline only, no hardware).
    pub fn disarm(&self) {
        self.0.disarm()
    }

    /// Whether the transmitter is currently considered keyed (the watchdog is armed).
    pub fn is_keyed(&self) -> bool {
        self.0.is_keyed()
    }

    /// Time since the transmitter was keyed, or `None` when not keyed.
    pub fn elapsed(&self) -> Option<Duration> {
        self.0.elapsed()
    }

    /// The configured max keyed duration.
    pub fn max_duration(&self) -> Duration {
        self.0.max_duration()
    }

    /// Override the max keyed duration (tests / config).
    pub fn set_max_duration(&self, d: Duration) {
        self.0.set_max_duration(d)
    }

    /// Force-release the transmitter if the deadline has elapsed. Idempotent and single-fire.
    pub fn force_release_if_expired(&self, event_tx: &broadcast::Sender<ControlEvent>) -> bool {
        self.0.force_release_if_expired(Some(&observer(event_tx)))
    }

    /// Spawn the independent watchdog thread. Holds only a `Weak`, so it exits once the last
    /// [`SharedPtt`] is dropped.
    pub fn spawn_watchdog(&self, event_tx: Arc<broadcast::Sender<ControlEvent>>) -> JoinHandle<()> {
        self.0.spawn_watchdog(Some(observer(&event_tx)))
    }
}

impl std::fmt::Debug for SharedPtt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The core's own contracts are tested in `openpulse-radio::shared_ptt`. These cover the piece
    /// that is unique to this adapter: that edges reach the daemon's `ControlEvent` channel with the
    /// same single-fire behaviour.
    #[derive(Default)]
    struct FakePtt {
        releases: Arc<AtomicUsize>,
    }
    impl PttController for FakePtt {
        fn assert_ptt(&mut self) -> Result<(), PttError> {
            Ok(())
        }
        fn release_ptt(&mut self) -> Result<(), PttError> {
            self.releases.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn is_asserted(&self) -> bool {
            false
        }
    }

    #[test]
    fn key_and_unkey_emit_control_events() {
        let releases = Arc::new(AtomicUsize::new(0));
        let ptt = SharedPtt::new(
            Some(Box::new(FakePtt {
                releases: releases.clone(),
            })),
            DEFAULT_PTT_MAX,
        );
        let tx = broadcast::channel(16).0;
        let mut rx = tx.subscribe();

        ptt.key(Some(&tx)).unwrap();
        assert!(matches!(
            rx.try_recv(),
            Ok(ControlEvent::PttChanged { active: true })
        ));

        assert_eq!(ptt.unkey(Some(&tx)), UnkeyOutcome::Released);
        assert!(matches!(
            rx.try_recv(),
            Ok(ControlEvent::PttChanged { active: false })
        ));

        // Single-fire: a second unkey emits nothing.
        assert_eq!(ptt.unkey(Some(&tx)), UnkeyOutcome::NotKeyed);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn force_release_emits_a_control_event() {
        let ptt = SharedPtt::new(Some(Box::new(FakePtt::default())), Duration::from_nanos(1));
        let tx = broadcast::channel(16).0;
        let mut rx = tx.subscribe();

        ptt.arm();
        assert!(ptt.force_release_if_expired(&tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(ControlEvent::PttChanged { active: false })
        ));
        assert!(!ptt.force_release_if_expired(&tx), "single-fire");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn guard_releases_and_emits_at_scope_end() {
        let ptt = SharedPtt::new(Some(Box::new(FakePtt::default())), DEFAULT_PTT_MAX);
        let tx = broadcast::channel(16).0;
        let mut rx = tx.subscribe();
        {
            let _g = ptt.keyed(Some(&tx)).unwrap();
            assert!(matches!(
                rx.try_recv(),
                Ok(ControlEvent::PttChanged { active: true })
            ));
        }
        assert!(!ptt.is_keyed());
        assert!(matches!(
            rx.try_recv(),
            Ok(ControlEvent::PttChanged { active: false })
        ));
    }
}
