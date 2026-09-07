//! Shared, watchdog-protected PTT state (issue #863; relocated from the daemon 2026-07-19).
//!
//! The PTT controller and its max-duration deadline live behind one `Arc<Mutex<_>>` ([`SharedPtt`]) so
//! an **independent watchdog thread** ([`SharedPtt::spawn_watchdog`]) can force-release the transmitter
//! on its deadline even while a caller's single async command loop is blocked inside a long handler (a
//! QSY scan or an OTA send-retry burst) — a `select!` arm cannot, because the loop never re-enters
//! `select!` during such a handler.
//!
//! **Why it lives here.** A stuck transmitter is a §97 violation and a PA-damage risk, and it is the
//! worst outcome this system can produce. This safety core originally sat in the daemon, so the ARDOP
//! TNC, the KISS TNC and the cross-band repeater — which all key real hardware — had no watchdog at
//! all (audit 2026-07-19, findings #1–#3). It belongs beside [`PttController`], the lowest layer every
//! transmit path already depends on.
//!
//! **Lock discipline:** the mutex is only ever held for the brief duration of a hardware assert/release
//! or a deadline read/write — *never* across an RF burst — so the watchdog can acquire it and preempt at
//! any point. Poisoned locks are recovered (`into_inner`) so a panicked TX path can never wedge the
//! watchdog.

use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::{PttController, PttError};

/// Default max continuous keyed time before the watchdog force-releases (Part 97 duty-cycle guidance).
pub const DEFAULT_PTT_MAX: Duration = Duration::from_secs(180);

/// How often the watchdog thread checks the deadline. Granularity is immaterial against a 180 s deadline.
const WATCHDOG_TICK: Duration = Duration::from_millis(100);

/// Sink for keyed/unkeyed edges.
///
/// Kept as a trait rather than a concrete channel so this crate needs no async runtime and each caller
/// can map the edge onto its own event type. Implementations are invoked **while the PTT lock is held**
/// (so a concurrent watchdog release cannot interleave its `false` inside another caller's `true`), and
/// must therefore not block or re-enter [`SharedPtt`].
pub trait PttObserver: Send + Sync {
    /// Called on a real keyed→unkeyed or unkeyed→keyed transition.
    fn ptt_changed(&self, active: bool);
}

/// Result of [`SharedPtt::unkey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnkeyOutcome {
    /// Hardware released and the watchdog was disarmed (a real keyed→unkeyed transition).
    Released,
    /// Nothing was armed (already released, e.g. the watchdog fired mid-burst) — no event emitted.
    NotKeyed,
    /// The hardware release failed; the watchdog is left **armed** so it can force-release later.
    Failed,
}

struct PttInner {
    controller: Option<Box<dyn PttController + Send>>,
    asserted_at: Option<Instant>,
    max_duration: Duration,
    /// Set once when the watchdog's force-release fails on a stuck rig, so the 100 ms retry loop logs
    /// the stuck transmitter once instead of every tick. Cleared on the next successful key/release.
    stuck_warned: bool,
    /// Bumped on every key. A [`PttKeyGuard`] records the value it acquired, and its release is a
    /// no-op once they differ (#1263).
    ///
    /// **The owner is the guard.** Not a thread id — ARDOP's `PTT TRUE` and `PTT FALSE` each run on a
    /// different `spawn_blocking` pool thread, so an operator could not release their own key, and the
    /// daemon's task migrates across tokio workers between awaits. Not a caller-supplied enum either:
    /// that is a label any site can claim, which is the same "only as good as every call site" hole
    /// the first design had. Holding a live guard IS ownership and this counter IS the identity.
    ///
    /// Without it, a guard that outlives the deadline — a hung transmit, released by the watchdog —
    /// drops later and releases whoever keyed in the meantime. That is the #1263 defect in its most
    /// damaging form, and it is present under every nesting policy, so the token comes first.
    generation: u64,
    /// Diagnostic label for whoever holds the current key (#1263). Never used to DECIDE anything —
    /// the owner is the guard and the identity is the generation. A caller-supplied label that
    /// decided access would be a claim any site could make, which is the hole the first design had.
    held_by: &'static str,
}

/// PTT hardware + watchdog deadline behind a shared lock. Cheap to `clone` (shares the same lock).
#[derive(Clone)]
pub struct SharedPtt(Arc<Mutex<PttInner>>);

impl SharedPtt {
    /// Build from a PTT controller (`None` = no hardware) and the max keyed duration.
    pub fn new(controller: Option<Box<dyn PttController + Send>>, max_duration: Duration) -> Self {
        Self(Arc::new(Mutex::new(PttInner {
            controller,
            asserted_at: None,
            max_duration,
            stuck_warned: false,
            generation: 0,
            held_by: "nobody",
        })))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PttInner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Key the transmitter and arm the watchdog. On hardware failure the deadline is left disarmed and no
    /// event is emitted (the caller skips the burst). `observer = None` keys silently (the beacon path,
    /// which historically emits no change event).
    pub fn key(&self, observer: Option<&Arc<dyn PttObserver>>) -> Result<(), PttError> {
        self.key_as("automatic", observer)
    }

    /// As [`Self::key`], labelling the holder for diagnostics (#1263).
    ///
    /// **Refuses while anyone holds a live key.** The check and the assert happen under ONE lock
    /// acquisition: a check-then-key across two would race the watchdog thread, the one thing that
    /// can preempt a live key at any instant.
    ///
    /// A refusal touches no hardware and emits no `PttChanged` — a caller that saw an event for a
    /// key it never got would leave a client's indicator stuck on.
    pub fn key_as(
        &self,
        who: &'static str,
        observer: Option<&Arc<dyn PttObserver>>,
    ) -> Result<(), PttError> {
        let mut g = self.lock();
        if g.asserted_at.is_some() {
            return Err(PttError::AlreadyKeyed { held_by: g.held_by });
        }
        if let Some(ptt) = g.controller.as_mut() {
            ptt.assert_ptt()?;
        }
        g.held_by = who;
        g.asserted_at = Some(Instant::now());
        g.stuck_warned = false;
        g.generation = g.generation.wrapping_add(1);
        // Notify under the lock so a concurrent watchdog force-release can't interleave its `false`
        // between this arm and its `true` (which would show unkeyed during a live keyed burst).
        if let Some(obs) = observer {
            obs.ptt_changed(true);
        }
        Ok(())
    }

    /// The current key generation — the identity a [`PttKeyGuard`] holds (#1263).
    fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Release only if `generation` is still the live one; otherwise do nothing.
    ///
    /// This is what a guard's `Drop` calls. A guard whose key the watchdog already force-released, or
    /// which some other holder has superseded, must not release the transmitter somebody else is
    /// using — and must not emit a `false` for a transition it did not make.
    fn unkey_owned(
        &self,
        observer: Option<&Arc<dyn PttObserver>>,
        generation: u64,
    ) -> UnkeyOutcome {
        // Check and act under ONE lock acquisition: a check-then-release across two would race the
        // watchdog thread, which is the one thing that can preempt a live key at any instant.
        let g = self.lock();
        if g.generation != generation || g.asserted_at.is_none() {
            return UnkeyOutcome::NotKeyed;
        }
        drop(g);
        self.unkey(observer)
    }

    /// Re-stamp the deadline of the key `generation` owns, so the watchdog measures **silence since
    /// the last transmission** rather than session length. Returns whether it extended.
    ///
    /// Owner-scoped on purpose (#1260). `arm()` would do the same re-stamping, but it is callable by
    /// anyone and cannot tell a liveness signal from the holder apart from an unrelated re-assert —
    /// #1263 closed exactly that on the manual path, where re-asserting every 170 s would have
    /// defeated the 180 s watchdog. Only the live holder can extend, and a hung holder cannot.
    fn extend_owned(&self, generation: u64) -> bool {
        let mut g = self.lock();
        if g.generation != generation || g.asserted_at.is_none() {
            return false;
        }
        g.asserted_at = Some(Instant::now());
        true
    }

    /// Release the transmitter and disarm the watchdog. A failed hardware release leaves the watchdog
    /// **armed** (so it force-releases later) and returns [`UnkeyOutcome::Failed`]. The `false` edge is
    /// emitted only on a real transition (so a burst whose deadline the watchdog already fired emits
    /// nothing — single-fire).
    pub fn unkey(&self, observer: Option<&Arc<dyn PttObserver>>) -> UnkeyOutcome {
        let mut g = self.lock();
        if let Some(ptt) = g.controller.as_mut() {
            if let Err(e) = ptt.release_ptt() {
                tracing::warn!(error = %e, "PTT release failed; leaving the watchdog armed");
                return UnkeyOutcome::Failed;
            }
        }
        g.stuck_warned = false;
        g.held_by = "nobody";
        if g.asserted_at.take().is_some() {
            // Notify under the lock (see `key`) so this `false` is ordered against any concurrent key.
            if let Some(obs) = observer {
                obs.ptt_changed(false);
            }
            UnkeyOutcome::Released
        } else {
            UnkeyOutcome::NotKeyed
        }
    }

    /// Key the transmitter and return an RAII guard that releases it on drop — including on an early
    /// return or a panic/unwind (REQ-PTT-01). Prefer this over paired `key`/`unkey` in any automatic-TX
    /// scope that can early-return or panic between them, so an unexpected key-down is bounded to the
    /// current stack scope instead of up to the 180 s watchdog. The observer is cloned into the guard so
    /// the release edge is still emitted on unwind; `None` keys silently (the beacon path).
    pub fn keyed(&self, observer: Option<&Arc<dyn PttObserver>>) -> Result<PttKeyGuard, PttError> {
        self.key(observer)?;
        Ok(PttKeyGuard {
            ptt: self.clone(),
            observer: observer.cloned(),
            released: false,
            generation: self.generation(),
        })
    }

    /// Hardware assert only — no deadline change, no event. For a manual assert command path that arms
    /// the deadline and emits its event separately.
    pub fn hw_assert(&self) -> Result<(), PttError> {
        let mut g = self.lock();
        match g.controller.as_mut() {
            Some(ptt) => ptt.assert_ptt(),
            None => Ok(()),
        }
    }

    /// Hardware release only — no deadline change, no event.
    pub fn hw_release(&self) -> Result<(), PttError> {
        let mut g = self.lock();
        match g.controller.as_mut() {
            Some(ptt) => ptt.release_ptt(),
            None => Ok(()),
        }
    }

    /// Arm the watchdog deadline (deadline only, no hardware). For a manual assert command.
    pub fn arm(&self) {
        let mut g = self.lock();
        g.asserted_at = Some(Instant::now());
        g.stuck_warned = false;
    }

    /// Disarm the watchdog deadline (deadline only, no hardware). For a manual release command.
    pub fn disarm(&self) {
        self.lock().asserted_at = None;
    }

    /// Release the transmitter whoever holds it — the control operator's hard override (#1263).
    ///
    /// Kept at the maintainer's decision. `PttRelease` has always dropped the hardware regardless of
    /// who keyed, and that is a control point: without it an operator watching a runaway automatic
    /// burst would have to wait out the 180 s watchdog. Making the manual key an owned guard would
    /// otherwise have turned `PttRelease` into a no-op during an automatic burst — a UI defect traded
    /// for a lost control point.
    ///
    /// Bumps the generation, so the displaced holder's guard becomes stale and its later `Drop` is a
    /// no-op rather than releasing whoever keyed next.
    pub fn force_release(&self, observer: Option<&Arc<dyn PttObserver>>) -> UnkeyOutcome {
        let mut g = self.lock();
        // The hardware release is attempted UNCONDITIONALLY — deliberately, and unlike `unkey`.
        // This is the operator's override, and it must not be gated on the daemon's BELIEF about the
        // state: that belief can be wrong (a rig left keyed by VOX, by a previous process, or by a
        // release this daemon thinks succeeded), which is the same reason the watchdog exists. An
        // override that only works when we already agree the rig is keyed is not an override.
        let armed = g.asserted_at.is_some();
        if let Some(ptt) = g.controller.as_mut() {
            if let Err(e) = ptt.release_ptt() {
                tracing::warn!(error = %e, "forced PTT release failed; leaving the watchdog armed");
                return UnkeyOutcome::Failed;
            }
        }
        let displaced = g.held_by;
        g.asserted_at = None;
        g.stuck_warned = false;
        g.generation = g.generation.wrapping_add(1);
        g.held_by = "nobody";
        if !armed {
            // The hardware was released anyway (see above), but there was no logical transition, so
            // no event — emitting `false` for a state nobody was in is the spurious-edge class #836
            // exists to prevent.
            return UnkeyOutcome::NotKeyed;
        }
        if displaced != "manual" {
            tracing::warn!(
                displaced,
                "PTT force-released by the operator while an automatic emission held it"
            );
        }
        if let Some(obs) = observer {
            obs.ptt_changed(false);
        }
        UnkeyOutcome::Released
    }

    /// Who currently holds the key, for diagnostics and for deciding whether a force is a takeover.
    pub fn held_by(&self) -> &'static str {
        self.lock().held_by
    }

    /// Whether the transmitter is currently considered keyed (the watchdog is armed).
    pub fn is_keyed(&self) -> bool {
        self.lock().asserted_at.is_some()
    }

    /// Time since the transmitter was keyed, or `None` when not keyed.
    pub fn elapsed(&self) -> Option<Duration> {
        self.lock().asserted_at.map(|t| t.elapsed())
    }

    /// The configured max keyed duration.
    pub fn max_duration(&self) -> Duration {
        self.lock().max_duration
    }

    /// Override the max keyed duration (tests / config).
    pub fn set_max_duration(&self, d: Duration) {
        self.lock().max_duration = d;
    }

    /// Force-release the transmitter if the deadline has elapsed. Idempotent and single-fire: the
    /// deadline is `take()`n under the lock, so exactly one `false` edge is emitted per keying even if
    /// several callers (the watchdog thread, an rx-tick, a `select!` arm) race. Returns `true` only when
    /// the transmitter was actually released.
    ///
    /// On a **failed** hardware release (a stuck rig) the deadline is left **armed** and no `false` is
    /// emitted — the transmitter really is still keyed, so telling clients otherwise would be a lie. The
    /// 100 ms watchdog thread then retries every tick until the rig releases; the stuck condition is
    /// logged once (`stuck_warned`). This matches [`SharedPtt::unkey`]'s "failed release stays armed"
    /// contract.
    pub fn force_release_if_expired(&self, observer: Option<&Arc<dyn PttObserver>>) -> bool {
        let mut g = self.lock();
        let expired = g
            .asserted_at
            .map(|t| t.elapsed() >= g.max_duration)
            .unwrap_or(false);
        if !expired {
            return false;
        }
        let hw_ok = match g.controller.as_mut() {
            Some(ptt) => ptt.release_ptt().is_ok(),
            None => true,
        };
        if !hw_ok {
            if !g.stuck_warned {
                g.stuck_warned = true;
                tracing::error!(
                    max_secs = g.max_duration.as_secs(),
                    "PTT watchdog: hardware release failed past max duration — transmitter may be \
                     stuck keyed; retrying every tick until it releases"
                );
            }
            return false; // stay armed so the next tick retries
        }
        g.asserted_at = None;
        g.stuck_warned = false;
        g.held_by = "nobody";
        // End the key's identity too (#1263). Clearing `asserted_at` already makes a stale guard's
        // release a no-op, since ownership is "my generation is live AND the deadline is armed" —
        // there is deliberately no separate owner field to go stale, which is what would otherwise
        // let a released key keep refusing later ones while the rig sits idle. Bumping as well
        // states the invariant positively: the key you held is over.
        g.generation = g.generation.wrapping_add(1);
        tracing::warn!(
            max_secs = g.max_duration.as_secs(),
            "PTT watchdog fired — transmitter keyed beyond max duration; released"
        );
        // Notify under the lock (see `key`) so this `false` is ordered against any concurrent re-key.
        if let Some(obs) = observer {
            obs.ptt_changed(false);
        }
        true
    }

    /// Spawn the independent watchdog **thread**: every 100 ms it force-releases the transmitter if the
    /// deadline has passed, regardless of what the caller's loop is doing. A plain OS thread (not an
    /// async task) so it is immune to runtime flavor / worker starvation / a missing `block_in_place`. It
    /// holds only a `Weak` reference, so it exits on its own once the last [`SharedPtt`] is dropped — no
    /// stop-flag plumbing, no leaked threads.
    pub fn spawn_watchdog(&self, observer: Option<Arc<dyn PttObserver>>) -> JoinHandle<()> {
        let weak: Weak<Mutex<PttInner>> = Arc::downgrade(&self.0);
        std::thread::Builder::new()
            .name("ptt-watchdog".into())
            .spawn(move || loop {
                std::thread::sleep(WATCHDOG_TICK);
                let Some(arc) = weak.upgrade() else {
                    break; // last SharedPtt dropped → nothing left to guard
                };
                SharedPtt(arc).force_release_if_expired(observer.as_ref());
            })
            .expect("failed to spawn ptt-watchdog thread")
    }
}

/// RAII guard from [`SharedPtt::keyed`] that releases the transmitter (and disarms the watchdog) when it
/// drops — including on an early return or a panic/unwind (REQ-PTT-01). Under the default `panic=unwind`
/// profile, `Drop` runs during unwinding, so the release happens *before* the panic reaches the task
/// boundary or crashes the process — the transmitter never stays keyed waiting for the 180 s watchdog.
#[must_use = "dropping the guard releases PTT; bind it for the transmit scope"]
pub struct PttKeyGuard {
    ptt: SharedPtt,
    observer: Option<Arc<dyn PttObserver>>,
    released: bool,
    /// The key generation this guard acquired (#1263). Its release applies only while this is still
    /// the live one — see [`SharedPtt::unkey_owned`].
    generation: u64,
}

impl PttKeyGuard {
    /// Release now instead of at scope end — for the half-duplex turnaround where PTT must drop before
    /// listening. Idempotent; after this the `Drop` is a no-op. Returns the underlying unkey outcome.
    ///
    /// DORMANT(#1260): the daemon's OTA send was its last production caller until #1262 routed every
    /// emission through a helper that owns the guard, so the drop now happens at the same point and
    /// the explicit call became redundant *there*. It is kept rather than deleted because the
    /// capability it provides — dropping PTT while the guard's scope continues — is exactly what the
    /// cross-band repeater's half-duplex path needs when it is moved onto `SharedPtt` (#1260), and
    /// what any future transmit-then-listen caller will need. `Drop` alone cannot express it.
    pub fn release(mut self) -> UnkeyOutcome {
        self.release_inner()
    }

    fn release_inner(&mut self) -> UnkeyOutcome {
        if self.released {
            return UnkeyOutcome::NotKeyed;
        }
        self.released = true;
        // Generation-scoped (#1263): release only the key this guard took. A guard that outlived its
        // key — the watchdog force-released a hung transmit, or another holder has since keyed —
        // must not drop a transmitter somebody else is using, and must not emit a `false` for a
        // transition it did not make.
        self.ptt
            .unkey_owned(self.observer.as_ref(), self.generation)
    }

    /// Whether this guard still owns the live key (#1263).
    ///
    /// Generation match **and** an armed deadline: a guard whose key the watchdog force-released
    /// matches on neither, and a caller that treats a dead guard as a live hold would refuse to key
    /// while the transmitter sits idle.
    pub fn is_live(&self) -> bool {
        let g = self.ptt.lock();
        !self.released && g.generation == self.generation && g.asserted_at.is_some()
    }

    /// Re-stamp this guard's deadline, turning the watchdog into a silence timer (#1260).
    ///
    /// For a holder that legitimately keeps the transmitter up across many emissions — the
    /// full-duplex cross-band repeater — where a fixed session deadline would force-release working
    /// traffic and no deadline would leave rig_b keyed for the life of the process. Returns `false`
    /// on a guard that no longer owns the live key, which is the caller's signal to re-key.
    pub fn extend(&self) -> bool {
        !self.released && self.ptt.extend_owned(self.generation)
    }
}

impl Drop for PttKeyGuard {
    fn drop(&mut self) {
        let _ = self.release_inner();
    }
}

impl Default for SharedPtt {
    fn default() -> Self {
        Self::new(None, DEFAULT_PTT_MAX)
    }
}

impl std::fmt::Debug for SharedPtt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let g = self.lock();
        f.debug_struct("SharedPtt")
            .field("keyed_for", &g.asserted_at.map(|t| t.elapsed()))
            .field("max_duration", &g.max_duration)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A PTT double that counts releases and can be made to fail, and reports whether it is keyed.
    #[derive(Default)]
    struct FakePtt {
        releases: Arc<AtomicUsize>,
        asserts: Arc<AtomicUsize>,
        fail_assert: bool,
        fail_release: bool,
    }
    impl PttController for FakePtt {
        fn assert_ptt(&mut self) -> Result<(), PttError> {
            if self.fail_assert {
                return Err(PttError::Serial("assert failed".into()));
            }
            self.asserts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn release_ptt(&mut self) -> Result<(), PttError> {
            if self.fail_release {
                return Err(PttError::Serial("stuck keyed".into()));
            }
            self.releases.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn is_asserted(&self) -> bool {
            false
        }
    }

    /// Records every edge so a test can assert both count and order.
    #[derive(Default)]
    struct SpyObserver(Mutex<Vec<bool>>);
    impl PttObserver for SpyObserver {
        fn ptt_changed(&self, active: bool) {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(active);
        }
    }
    impl SpyObserver {
        fn edges(&self) -> Vec<bool> {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    fn spy() -> (Arc<dyn PttObserver>, Arc<SpyObserver>) {
        let s = Arc::new(SpyObserver::default());
        (s.clone() as Arc<dyn PttObserver>, s)
    }

    /// #1260: a live holder's extend re-stamps the deadline, so the watchdog measures silence since
    /// the last transmission rather than the length of the session.
    #[test]
    fn a_live_guard_extends_its_deadline_and_survives_the_watchdog() {
        let releases = Arc::new(AtomicUsize::new(0));
        let ptt = SharedPtt::new(
            Some(Box::new(FakePtt {
                releases: releases.clone(),
                ..Default::default()
            })),
            Duration::from_millis(60),
        );
        let guard = ptt.keyed(None).expect("key");

        // Past the deadline without extending, the watchdog would force-release. Extend inside it.
        std::thread::sleep(Duration::from_millis(40));
        assert!(guard.extend(), "the live holder must be able to extend");
        assert!(
            !ptt.force_release_if_expired(None),
            "the re-stamped deadline has not elapsed, so the watchdog must not fire"
        );
        assert!(guard.is_live(), "extending must not disturb ownership");

        // And the deadline is a real bound, not removed: stop extending and it fires.
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            ptt.force_release_if_expired(None),
            "extend must re-stamp the deadline, never disarm it — silence has to end the key"
        );
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        assert!(
            !guard.is_live(),
            "the force-released holder must see its guard go dead so it can re-key"
        );
    }

    /// The whole reason `extend` is owner-scoped rather than `arm()` (#1263): a holder whose key the
    /// watchdog already took must not be able to keep the transmitter up.
    #[test]
    fn a_stale_guard_cannot_extend_the_key_that_displaced_it() {
        let ptt = SharedPtt::new(
            Some(Box::new(FakePtt::default())),
            Duration::from_millis(20),
        );
        let stale = ptt.keyed(None).expect("key");
        std::thread::sleep(Duration::from_millis(40));
        assert!(ptt.force_release_if_expired(None), "watchdog fires");

        // Somebody else now owns the transmitter.
        let live = ptt.keyed(None).expect("re-key");
        assert!(
            !stale.extend(),
            "a stale guard extending would hold a transmitter another caller is using"
        );
        assert!(live.extend(), "the live holder is unaffected");
    }

    /// An explicitly released guard is finished; extend must not resurrect the deadline.
    #[test]
    fn a_released_guard_cannot_extend() {
        let ptt = SharedPtt::new(Some(Box::new(FakePtt::default())), Duration::from_secs(5));
        let guard = ptt.keyed(None).expect("key");
        assert_eq!(guard.release(), UnkeyOutcome::Released);
        assert!(!ptt.is_keyed());
        // `release()` consumes the guard, so the reachable form of this is a guard whose key ended
        // some other way; assert the underlying rule directly.
        assert!(
            !ptt.extend_owned(0),
            "extend must refuse when nothing is armed"
        );
    }

    #[test]
    fn key_unkey_arms_and_disarms_with_single_events() {
        let releases = Arc::new(AtomicUsize::new(0));
        let ptt = SharedPtt::new(
            Some(Box::new(FakePtt {
                releases: releases.clone(),
                ..Default::default()
            })),
            DEFAULT_PTT_MAX,
        );
        let (obs, spy) = spy();

        ptt.key(Some(&obs)).unwrap();
        assert!(ptt.is_keyed());
        assert_eq!(spy.edges(), vec![true]);

        assert_eq!(ptt.unkey(Some(&obs)), UnkeyOutcome::Released);
        assert!(!ptt.is_keyed());
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        assert_eq!(spy.edges(), vec![true, false]);

        // A second unkey is a no-op (nothing armed) and emits nothing.
        assert_eq!(ptt.unkey(Some(&obs)), UnkeyOutcome::NotKeyed);
        assert_eq!(spy.edges(), vec![true, false], "no duplicate edge");
    }

    #[test]
    fn failed_release_leaves_the_watchdog_armed() {
        let ptt = SharedPtt::new(
            Some(Box::new(FakePtt {
                fail_release: true,
                ..Default::default()
            })),
            DEFAULT_PTT_MAX,
        );
        ptt.arm();
        assert_eq!(ptt.unkey(None), UnkeyOutcome::Failed);
        assert!(
            ptt.is_keyed(),
            "a failed release must leave the watchdog armed"
        );
    }

    #[test]
    fn force_release_is_single_fire_and_idempotent() {
        let releases = Arc::new(AtomicUsize::new(0));
        let ptt = SharedPtt::new(
            Some(Box::new(FakePtt {
                releases: releases.clone(),
                ..Default::default()
            })),
            Duration::from_nanos(1),
        );
        let (obs, spy) = spy();

        ptt.arm(); // deadline (1 ns) already elapsed
        assert!(ptt.force_release_if_expired(Some(&obs)));
        assert!(!ptt.is_keyed());
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        assert_eq!(spy.edges(), vec![false]);

        // Not armed → no second fire, no second event.
        assert!(!ptt.force_release_if_expired(Some(&obs)));
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        assert_eq!(spy.edges(), vec![false]);
    }

    #[test]
    fn not_expired_does_not_fire() {
        let ptt = SharedPtt::new(Some(Box::new(FakePtt::default())), DEFAULT_PTT_MAX);
        ptt.arm();
        let (obs, _spy) = spy();
        assert!(
            !ptt.force_release_if_expired(Some(&obs)),
            "180 s deadline not yet reached"
        );
        assert!(ptt.is_keyed());
    }

    /// The independent watchdog **thread** force-releases while nothing else touches the PTT (a blocked
    /// loop *is* the absence of cooperative calls). Proves the preemption a `select!` arm can't give.
    #[test]
    fn watchdog_thread_force_releases_a_blocked_loop() {
        let releases = Arc::new(AtomicUsize::new(0));
        let ptt = SharedPtt::new(
            Some(Box::new(FakePtt {
                releases: releases.clone(),
                ..Default::default()
            })),
            Duration::from_millis(20),
        );
        let (obs, spy) = spy();

        ptt.key(None).unwrap(); // keyed; deadline 20 ms
        let _wd = ptt.spawn_watchdog(Some(obs));

        // Simulate a blocked loop: make NO further SharedPtt calls; wait (deadline-bounded) for the
        // independent thread to force-release.
        let mut released = false;
        for _ in 0..200 {
            if !ptt.is_keyed() {
                released = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            released,
            "the watchdog thread must force-release without any loop cooperation"
        );
        assert_eq!(
            releases.load(Ordering::SeqCst),
            1,
            "exactly one hardware release"
        );
        assert_eq!(spy.edges(), vec![false]);
    }

    #[test]
    fn watchdog_thread_exits_when_the_last_shared_ptt_drops() {
        let ptt = SharedPtt::new(Some(Box::new(FakePtt::default())), DEFAULT_PTT_MAX);
        let handle = ptt.spawn_watchdog(None);
        drop(ptt); // last strong ref gone
                   // The thread upgrades a Weak each tick; with no strong ref it breaks within ~one tick.
        assert!(handle.join().is_ok());
    }

    /// A stuck rig (release keeps failing) must keep the watchdog armed and stay silent — the
    /// transmitter really is still keyed — and only release + notify once the hardware recovers.
    #[test]
    fn watchdog_retries_a_stuck_rig_and_stays_armed_until_release() {
        struct TogglePtt {
            fail: Arc<AtomicBool>,
            releases: Arc<AtomicUsize>,
        }
        impl PttController for TogglePtt {
            fn assert_ptt(&mut self) -> Result<(), PttError> {
                Ok(())
            }
            fn release_ptt(&mut self) -> Result<(), PttError> {
                if self.fail.load(Ordering::SeqCst) {
                    return Err(PttError::Serial("stuck keyed".into()));
                }
                self.releases.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn is_asserted(&self) -> bool {
                false
            }
        }
        let fail = Arc::new(AtomicBool::new(true));
        let releases = Arc::new(AtomicUsize::new(0));
        let ptt = SharedPtt::new(
            Some(Box::new(TogglePtt {
                fail: fail.clone(),
                releases: releases.clone(),
            })),
            Duration::ZERO,
        );
        let (obs, spy) = spy();
        ptt.arm(); // immediately expired

        // Stuck: the deadline is past but the hardware release fails → stays armed, no successful
        // release, no `false` edge (clients must not be told the still-keyed rig is down).
        assert!(!ptt.force_release_if_expired(Some(&obs)));
        assert!(ptt.is_keyed(), "a stuck rig keeps the watchdog armed");
        assert_eq!(releases.load(Ordering::SeqCst), 0);
        assert!(
            spy.edges().is_empty(),
            "no false edge while the rig is still keyed"
        );

        // Rig recovers → the next tick releases, disarms, and emits exactly one `false`.
        fail.store(false, Ordering::SeqCst);
        assert!(ptt.force_release_if_expired(Some(&obs)));
        assert!(!ptt.is_keyed());
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        assert_eq!(spy.edges(), vec![false]);
    }

    /// #863 mid-burst race: the watchdog thread force-releases *during* a long blocked burst; when the
    /// burst finally finishes and the loop calls `unkey`, it must be a single-fire no-op — one `false`
    /// total across the thread and the loop, and outcome `NotKeyed`.
    #[test]
    fn a_late_unkey_after_the_watchdog_fired_is_a_silent_noop() {
        let releases = Arc::new(AtomicUsize::new(0));
        let ptt = SharedPtt::new(
            Some(Box::new(FakePtt {
                releases: releases.clone(),
                ..Default::default()
            })),
            Duration::from_millis(10),
        );
        let (obs, spy) = spy();

        ptt.key(Some(&obs)).unwrap(); // true; deadline 10 ms
        assert_eq!(spy.edges(), vec![true]);

        let _wd = ptt.spawn_watchdog(Some(obs.clone()));
        // The "burst" runs long: no cooperative call while the thread force-releases.
        let mut fired = false;
        for _ in 0..200 {
            if !ptt.is_keyed() {
                fired = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(fired, "the watchdog thread fired mid-burst");
        assert_eq!(
            spy.edges(),
            vec![true, false],
            "exactly one false from the watchdog"
        );

        // Burst finishes → the loop's late unkey is a no-op: no second event. (The redundant hardware
        // release is defensive and idempotent, so the release count is not asserted here.)
        assert_eq!(ptt.unkey(Some(&obs)), UnkeyOutcome::NotKeyed);
        assert_eq!(
            spy.edges(),
            vec![true, false],
            "no duplicate false from the late unkey"
        );
    }

    #[test]
    fn key_guard_releases_at_scope_end() {
        let releases = Arc::new(AtomicUsize::new(0));
        let ptt = SharedPtt::new(
            Some(Box::new(FakePtt {
                releases: releases.clone(),
                ..Default::default()
            })),
            DEFAULT_PTT_MAX,
        );
        let (obs, spy) = spy();
        {
            let _g = ptt.keyed(Some(&obs)).unwrap();
            assert!(ptt.is_keyed());
            assert_eq!(spy.edges(), vec![true]);
        } // guard drops here
        assert!(!ptt.is_keyed(), "the guard releases at scope end");
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        assert_eq!(spy.edges(), vec![true, false]);
    }

    /// REQ-PTT-01: a panic inside a keyed scope must release the transmitter during unwinding — not leave
    /// it keyed for the 180 s watchdog.
    #[test]
    fn key_guard_releases_on_panic_unwind() {
        let releases = Arc::new(AtomicUsize::new(0));
        let ptt = SharedPtt::new(
            Some(Box::new(FakePtt {
                releases: releases.clone(),
                ..Default::default()
            })),
            DEFAULT_PTT_MAX,
        );
        let (obs, _spy) = spy();
        let ptt_in = ptt.clone();
        let obs_in = obs.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = ptt_in.keyed(Some(&obs_in)).unwrap();
            assert!(ptt_in.is_keyed());
            panic!("boom mid keyed scope");
        }));
        assert!(result.is_err(), "the keyed scope panicked");
        assert!(
            !ptt.is_keyed(),
            "the guard released the transmitter on unwind"
        );
        assert_eq!(
            releases.load(Ordering::SeqCst),
            1,
            "hardware released exactly once during unwind"
        );
    }

    #[test]
    fn key_guard_explicit_release_is_single_fire() {
        let releases = Arc::new(AtomicUsize::new(0));
        let ptt = SharedPtt::new(
            Some(Box::new(FakePtt {
                releases: releases.clone(),
                ..Default::default()
            })),
            DEFAULT_PTT_MAX,
        );
        let g = ptt.keyed(None).unwrap();
        assert_eq!(g.release(), UnkeyOutcome::Released); // consumes the guard; its Drop then no-ops
        assert!(!ptt.is_keyed());
        assert_eq!(
            releases.load(Ordering::SeqCst),
            1,
            "explicit release + the moved guard's Drop release once total"
        );
    }
}

/// A guard releases only the key it took (#1263).
#[cfg(test)]
mod ownership_token_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Counting {
        asserted: bool,
        asserts: Arc<AtomicUsize>,
        releases: Arc<AtomicUsize>,
    }

    impl PttController for Counting {
        fn assert_ptt(&mut self) -> Result<(), PttError> {
            self.asserted = true;
            self.asserts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn release_ptt(&mut self) -> Result<(), PttError> {
            self.asserted = false;
            self.releases.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn is_asserted(&self) -> bool {
            self.asserted
        }
    }

    fn ptt(max: Duration) -> (SharedPtt, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let asserts = Arc::new(AtomicUsize::new(0));
        let releases = Arc::new(AtomicUsize::new(0));
        let ctrl = Counting {
            asserted: false,
            asserts: asserts.clone(),
            releases: releases.clone(),
        };
        (SharedPtt::new(Some(Box::new(ctrl)), max), asserts, releases)
    }

    /// The #1263 defect in its most damaging form: a guard that outlived its key must not drop a
    /// transmitter somebody else is now using.
    #[test]
    fn a_guard_whose_key_the_watchdog_ended_does_not_release_a_later_key() {
        let (p, _asserts, releases) = ptt(Duration::from_nanos(1));
        let stale = p.keyed(None).expect("first key");

        // The watchdog force-releases the hung burst.
        assert!(p.force_release_if_expired(None), "deadline already elapsed");
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        assert!(!p.is_keyed());
        assert!(!stale.is_live(), "the guard no longer owns anything");

        // Somebody else keys.
        p.set_max_duration(Duration::from_secs(180));
        let _live = p.keyed(None).expect("second key");
        assert!(p.is_keyed());

        // The stale guard drops. Before #1263 this released the SECOND key's transmitter.
        drop(stale);
        assert!(
            p.is_keyed(),
            "a stale guard released a key it did not take — the transmitter of whoever keyed second \
             was dropped out from under them"
        );
        assert_eq!(
            releases.load(Ordering::SeqCst),
            1,
            "the stale drop must not touch the hardware at all"
        );
    }

    /// A live guard still releases normally — without this, the test above would also pass in a
    /// build where guards had stopped releasing anything.
    #[test]
    fn a_live_guard_still_releases() {
        let (p, _a, releases) = ptt(Duration::from_secs(180));
        {
            let g = p.keyed(None).expect("key");
            assert!(g.is_live());
            assert!(p.is_keyed());
        }
        assert!(!p.is_keyed(), "control: a live guard must release on drop");
        assert_eq!(releases.load(Ordering::SeqCst), 1);
    }

    /// `is_live` needs BOTH conditions: a matching generation is not enough once the deadline is gone.
    #[test]
    fn is_live_requires_an_armed_deadline_not_just_a_matching_generation() {
        let (p, _a, _r) = ptt(Duration::from_nanos(1));
        let g = p.keyed(None).expect("key");
        assert!(g.is_live());
        assert!(p.force_release_if_expired(None));
        assert!(
            !g.is_live(),
            "after the watchdog released it, the guard owns nothing — a caller that read this as a \
             live hold would refuse to key while the transmitter sits idle (#1263 F1)"
        );
    }

    /// A second key is REFUSED while somebody holds a live one (#1263), and touches nothing.
    #[test]
    fn a_second_key_is_refused_and_touches_no_hardware() {
        let (p, asserts, releases) = ptt(Duration::from_secs(180));
        let _held = p.keyed(None).expect("first key");
        assert_eq!(asserts.load(Ordering::SeqCst), 1);

        let refused = p.keyed(None);
        assert!(
            matches!(refused, Err(PttError::AlreadyKeyed { .. })),
            "a second key must be refused, not silently adopt the first one's deadline"
        );
        assert_eq!(
            asserts.load(Ordering::SeqCst),
            1,
            "a refusal must not touch the hardware"
        );
        assert_eq!(releases.load(Ordering::SeqCst), 0);
        assert!(p.is_keyed(), "the first holder still has it");
    }

    /// A refusal emits no `PttChanged` — an event for a key that was never granted would leave a
    /// client's indicator stuck on.
    #[test]
    fn a_refused_key_emits_no_event() {
        struct Counting(Arc<AtomicUsize>);
        impl PttObserver for Counting {
            fn ptt_changed(&self, _active: bool) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let (p, _a, _r) = ptt(Duration::from_secs(180));
        let events = Arc::new(AtomicUsize::new(0));
        let obs: Arc<dyn PttObserver> = Arc::new(Counting(events.clone()));

        let _held = p.keyed(Some(&obs)).expect("first key");
        assert_eq!(
            events.load(Ordering::SeqCst),
            1,
            "the granted key emitted true"
        );

        assert!(p.keyed(Some(&obs)).is_err());
        assert_eq!(
            events.load(Ordering::SeqCst),
            1,
            "a refused key must emit nothing at all"
        );
    }

    /// The operator's hard override survives (maintainer decision, #1263).
    ///
    /// `PttRelease` has always dropped the hardware regardless of who keyed. Without this an
    /// operator watching a runaway automatic burst would wait out the full 180 s watchdog.
    #[test]
    fn a_force_release_takes_the_key_from_an_automatic_holder() {
        let (p, _a, releases) = ptt(Duration::from_secs(180));
        let automatic = p.keyed(None).expect("automatic key");
        assert!(p.is_keyed());

        assert_eq!(p.force_release(None), UnkeyOutcome::Released);
        assert!(!p.is_keyed(), "the operator's override drops the rig");
        assert_eq!(releases.load(Ordering::SeqCst), 1);

        // And the displaced guard is now stale, so its later drop cannot release a NEW key.
        let _next = p.keyed(None).expect("somebody keys again");
        drop(automatic);
        assert!(
            p.is_keyed(),
            "the force-displaced guard released a later key — the force path must bump the              generation, or the override trades one defect for another"
        );
        assert_eq!(releases.load(Ordering::SeqCst), 1);
    }

    /// Forcing an idle transmitter still hits the hardware, but reports no transition.
    ///
    /// The two halves are deliberate and pull in opposite directions:
    ///
    /// * **The hardware release is unconditional**, because the daemon's belief about the state can
    ///   be WRONG — a rig left keyed by VOX, by a previous process, or by a release this daemon
    ///   thinks succeeded. That is the same reason the watchdog exists, and an override that only
    ///   works when we already agree the rig is keyed is not an override.
    /// * **No event and `NotKeyed`**, because there was no logical transition, and emitting `false`
    ///   for a state nobody was in is the spurious-edge class #836 exists to prevent.
    ///
    /// This test asserted the opposite when first written (no hardware call at all). It was changed
    /// deliberately after the daemon's `ptt_command_guard_reports_hardware_failure_to_skip_dispatch`
    /// showed that gating the override on our own state model silently dropped the "a failed release
    /// reports hard failure" contract — a changed intent, not a test bent to fit.
    #[test]
    fn a_force_release_with_nothing_keyed_still_releases_the_hardware() {
        let (p, _a, releases) = ptt(Duration::from_secs(180));
        assert_eq!(
            p.force_release(None),
            UnkeyOutcome::NotKeyed,
            "no logical transition, so no event"
        );
        assert_eq!(
            releases.load(Ordering::SeqCst),
            1,
            "the hardware release must be attempted anyway — the operator's override cannot be              gated on the daemon believing the rig is keyed"
        );
    }

    /// After any release the holder label is cleared, so nothing can be "held by" a finished key.
    ///
    /// This is failure mode F1 from the review: a stale holder record would refuse every later key
    /// while the transmitter sat idle, deferring the station ID indefinitely and silently.
    #[test]
    fn every_release_path_clears_the_holder() {
        for release in ["guard", "watchdog", "force"] {
            let (p, _a, _r) = ptt(Duration::from_nanos(1));
            let g = p.keyed(None).expect("key");
            assert_eq!(p.held_by(), "automatic");
            match release {
                "guard" => drop(g),
                "watchdog" => {
                    assert!(p.force_release_if_expired(None));
                    drop(g);
                }
                _ => {
                    p.force_release(None);
                    drop(g);
                }
            }
            assert_eq!(
                p.held_by(),
                "nobody",
                "after a {release} release nothing may still be recorded as holding the key"
            );
            p.set_max_duration(Duration::from_secs(180));
            assert!(
                p.keyed(None).is_ok(),
                "a later key was refused after a {release} release — F1: the station ID would                  defer forever with the transmitter idle"
            );
        }
    }

    /// An explicit `release()` is still generation-scoped, not only the `Drop`.
    #[test]
    fn an_explicit_release_is_also_scoped() {
        let (p, _a, releases) = ptt(Duration::from_nanos(1));
        let stale = p.keyed(None).expect("first key");
        assert!(p.force_release_if_expired(None));
        p.set_max_duration(Duration::from_secs(180));
        let _live = p.keyed(None).expect("second key");

        assert_eq!(stale.release(), UnkeyOutcome::NotKeyed);
        assert!(p.is_keyed(), "release() must be scoped exactly as Drop is");
        assert_eq!(releases.load(Ordering::SeqCst), 1);
    }
}
