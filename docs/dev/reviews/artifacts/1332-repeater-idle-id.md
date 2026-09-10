# #1332 — the cross-band repeater never IDs while idle

## Facts established first

1. **`maybe_identify` does not key.** It calls `engine_tx.transmit` directly and relies on the
   caller's guard — `relay_burst_at`'s comment says so explicitly ("ONE key covers the relayed frame
   AND the §97.119 ID that may follow it"). So calling it from an idle tick as-is would **transmit
   into an unkeyed rig**, which is the defect class this project has fixed repeatedly (#1260, the
   ARDOP/daemon keyed-transmit audits). Any idle path must take its own key.
2. **Sign-off is disabled in the repeater.** `StationIdTimer::new` sets `signoff_idle_ms = 0`, and
   the repeater never calls `with_signoff_idle_ms`. `signoff_due` therefore returns false always.
3. **The knob already exists and the repeater ignores it.** `[station] auto_id_signoff_idle_secs`
   (default 10) is read by the daemon at `server.rs:823`; the repeater's timer is built without it.
4. **The session loop's `Timeout` arm just `continue`s**, so there is a natural tick already running
   at `IDLE_POLL_MS`.

Consequence: a repeater that relays a frame and then hears silence never identifies. The interval
half only fires by accident of traffic (the ID rides the next relay that happens after the interval);
the end-of-communication half of §97.119(a) is unreachable.

## Proposal — mirror the daemon, which already does all of this

`server.rs:1240-1330` is the complete model and the repeater should not invent a second one:
compute `now_ms` from a monotonic origin, check `id_due` then `signoff_due`, transmit under an RAII
key guard, track deferral, and log a skipped ID at `error!` because it is an obligation not met.

Concretely: in the `Timeout` arm, if an ID is due, take a key, transmit `DE <callsign>`,
`mark_identified`, release. Feed the repeater's timer from `auto_id_signoff_idle_secs` so sign-off
works at all.

## What I am unsure about, and want tested rather than confirmed

1. **Carrier sense versus the ID obligation.** #1325 makes the repeater sense rig_b before keying.
   An ID keys rig_b, so consistency says sense first — but then a persistently busy output band
   defers a §97.119 ID indefinitely. The daemon already defers (`id_deferred_at`) and logs how long,
   which is precedent for deferring; what I cannot tell is whether anyone has decided that deferral
   may be *unbounded*. If it may not, what is the bound, and what does the station do when it
   expires — transmit over the QSO, or stop relaying?
2. **Full duplex.** `acquire_key` reuses a held session guard. An idle ID during a full-duplex
   session would ride the existing key, which seems right, but it also means the "silence" that
   `signoff_due` measures is TX silence while the key is still down. I have not worked out whether a
   sign-off ID can fire while the session guard is still held, or whether it should.
3. **Which clock.** `relay_burst` uses `self.start.elapsed()`; the idle tick must use the same origin
   or the two paths disagree about when the interval elapsed.
4. **Does an idle ID re-arm anything?** `mark_identified` clears `tx_since_id`, so an idle repeater
   that has signed off will not keep IDing — I believe that terminates correctly, but it is the kind
   of thing that reads right and loops in practice.

## The gate this issue should carry

Per #1332's own body: this is where the recently-shipped `note_tx`-before-`transmit` reorder finally
becomes observable. A transmit that emits audio and then errs (the cpal `flush` timeout) followed by
an **idle tick** discriminates, where nothing did before — the deleted `id_arming.rs` fixture belongs
here. That is the test that proves both changes at once.

## Consumer

`crates/openpulse-repeater/src/lib.rs` — `run_full_duplex`'s `RecvTimeoutError::Timeout` arm (the
tick that exists and currently does nothing), and `maybe_identify`, whose sole caller today is
`relay_burst_at`. Reached in production through `spawn_repeater`'s thread
(`crates/openpulse-daemon/src/lib.rs`).

## Prior art

- **`server.rs:1240-1330`** — the daemon's periodic + sign-off ID: `id_due` then `signoff_due`,
  `keyed_transmit` RAII, `id_deferred_at` tracking, and an `error!` log when the ID is skipped
  because "a skipped station ID is a §97.119 obligation not met". This is the shape to copy.
- **`[station] auto_id_signoff_idle_secs`** already exists (default 10) and is already wired for the
  daemon — the repeater needs to read it, not a new knob.
- **#1260** established that one key must cover the relayed frame *and* the ID that follows, rather
  than the ID asserting a second key underneath. An idle ID has no frame to ride, so it needs its
  own guard — the same rule, applied to a path that did not exist then.

## Twins

- **The daemon's own ID path** is the twin that works; the repeater is the one that does not. Note
  the daemon arms from a `frames_transmitted` delta that is bumped *after* `flush`, so the review of
  the panic change flagged it as possibly having the same cpal-flush blind spot — recorded there as
  `UNCHECKED`, and it should be measured while this work is in the same area.
- **ARDOP** (`openpulse-ardop`) has its own ID timer and its own keyed-transmit helper; not touched
  here, but it is the third copy of this logic and worth confirming it ticks rather than riding
  traffic.

## Review outcome (Fable, 2026-09-10) — code reading at HEAD, nothing measured

**1. Fact 1 is right but narrower than I wrote.** From the `Timeout` arm as-is, `maybe_identify`
transmits unkeyed in *half* duplex (`session_guard` is `None`) and after a watchdog force-release
(guard `Some` but dead); in full duplex with a *live* guard it would actually go out keyed. The
conclusion is unchanged because `acquire_key` already handles all three, including re-keying when
`extend()` reports the guard dead — but "would transmit unkeyed" is not unconditional.

**2. My precedent was for a different cause, and the answer removes the question.** `keyed_transmit`
does **no** DCD check; the only `is_channel_busy` consumers are the discovery beacon and a status
read. The daemon's `id_deferred_at` is triggered by `AlreadyKeyed` — *its own* transmitter held by
another emission — not by a busy band, and it is implicitly bounded because nothing there calls
`extend`, so any holder loses the key within `DEFAULT_PTT_MAX`. In the repeater that cause is
**unreachable**: rig_b's `SharedPtt` has one holder and `acquire_key` takes the session guard before
keying, so `AlreadyKeyed` cannot happen. Copying the daemon's deferral would copy a branch with no
trigger.

**Decision: the ID does not carrier-sense.** §97.119(a) has no busy-channel exemption, and stopping
relaying does not discharge it — the obligation is owed for transmissions already made. The
"polite ID" pattern (wait for a gap, force at the deadline) collapses here because `id_due` fires
*at* `last_id + interval` and the default interval is the legal maximum (600 s), so there is no
polite window left at the moment it becomes due: forcing at the deadline and not sensing are the same
behaviour. So the split is principled rather than a fitted constant — **carrier sense governs
discretionary relaying; a brief mandatory ID is not discretionary** — and it matches the fact that no
ID path anywhere in this tree senses the band.

**3. Full duplex: release the key after a sign-off ID.** Sign-off fires 10 s after the last TX while
the full-duplex carrier is held for 180 s of silence, so it lands 170 s before the watchdog would
drop it. Mirroring `relay_burst_at`'s `guard.extend()` would re-stamp the watchdog and prolong a
**dead** carrier to 190 s past the last relay. The sign-off ID *is* the end-of-communication marker,
so releasing after it ends the hold 10 s after traffic and returns the watchdog to being a backstop.
An interval ID under a live key still extends — traffic is continuing.

**4. Termination holds on the success path and LOOPS on the failure paths.** `id_due` and
`signoff_due` both require `tx_since_id`, `mark_identified` clears it, and the repeater arms
explicitly rather than by counter delta, so the ID transmit cannot re-arm itself. But
`maybe_identify`'s `?` returns before `mark_identified`, so an idle ID whose transmit errs would stay
armed and retry **every tick** — the daemon marks on transmit error precisely to prevent that.
Deferral logging must be edge-triggered for the same reason.

**5. Copy selectively.** Keep the repeater's explicit arming (better than the daemon's counter
delta). Do not copy the deferral branch. On a transmit error, mark and continue; on a PTT *assert*
failure, end the session.

### Framing defects

- **The gate I proposed cannot exist in one session.** A flush-failing relay transmit returns `Err`
  through `?` and `run_full_duplex` breaks on it, so "transmit errs, then an idle tick" spans two
  sessions. It is still testable — the timer and `start` live on the struct and survive #1324's
  hand-back — but the design has to say so.
- **`id_arming.rs` was never committed** (`git log --all -- '*id_arming*'` finds nothing), so "the
  deleted fixture belongs here" means *write it*, not *move it*.
- **"Which clock" is a non-issue; testability is the real gap.** `relay_burst_at` takes an injected
  `now_ms`, `run_full_duplex` does not. A deterministic idle-ID gate needs a clock seam.
- **Pre-existing #1325 hole, adjacent:** `relay_burst_at` skips sensing on `session_guard.is_none()`
  rather than on liveness, so after a watchdog force-release the guard is `Some`-but-dead, sensing is
  skipped, and `acquire_key` re-keys **blind**. `tests/carrier_sense.rs` covers a live guard only.
- Half duplex: the idle ID should honour `tx_hang_ms` as a relay does, or say why not.
- `docs/regulatory.md` rows 100 and 121 name only the daemon and ARDOP paths and need the relay entry.

### Filed rather than folded in

`record_tx_frame` runs only after a successful `flush()?` on both emit seams, so a cpal flush timeout
leaves `frames_transmitted` unbumped — the daemon and ARDOP never arm their ID timers, #1319's
post-transmit `rx_stream = None` keys off the same counter and keeps a TX-contaminated capture, and
the §97 TX-metadata log misses the frame entirely. Three consumers, one ordering bug, and it wants
the same fault-injecting output stream this gate needs.
