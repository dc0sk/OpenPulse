# Recovering the cross-band repeater from a thread PANIC (follow-on to #1324)

## Where this sits

#1324 made the repeater thread hand its repeater back so a stopped session can restart, on both the
clean and error-exit paths. One path still loses it: a **panic**. `join()` returns the panic payload
rather than the value, so `reap_finished_repeater` logs

    the cross-band repeater thread panicked; the repeater is gone until restart

and the operator is back to needing a daemon restart — the exact condition #1324 exists to remove.
The maintainer has asked for this path covered too.

## Facts established before designing

1. **Unwinding actually happens.** `grep -rn 'panic *=' --include=Cargo.toml .` finds nothing, and
   there is no `[profile]` panic setting in the workspace manifest, so both dev and release unwind.
   A `catch_unwind` design would be inert under `panic = "abort"`, so this is the load-bearing check.
2. **The transmitter is released during unwind, and that is already gated.** `PttKeyGuard` has a
   `Drop` calling `release_inner`, and `shared_ptt.rs:870` is an existing test that panics inside a
   keyed scope and asserts `!ptt.is_keyed()` plus "hardware released exactly once during unwind". So
   a panic mid-relay does not strand rig_b keyed — the property that would otherwise make catching a
   panic reckless.
3. **`catch_unwind` has exactly ONE production use in this workspace**, `openpulse-cli`'s benchmark
   gate (`benchmark.rs:23`), and it does not use `AssertUnwindSafe`. The two `AssertUnwindSafe` uses
   (`audio/src/fault.rs:139`, `shared_ptt.rs:870`) are both **tests**. So this design introduces the
   `AssertUnwindSafe`-in-production pattern rather than following it. Stated plainly because my first
   reading of the grep counted all three as precedent, which they are not.

## Proposal

Wrap the session in `catch_unwind(AssertUnwindSafe(...))` inside the thread closure, return the
repeater on all three outcomes (clean, `Err`, panic), and report a panic the way an error exit is
reported — `CommandError` carrying the panic message, plus one `RepeaterChanged { false }` edge,
setting the same `repeater_exit_reported` flag so the reap does not add a second.

`AssertUnwindSafe` is required because `CrossBandRepeater` owns `ModemEngine`s and a `SharedPtt`
(interior mutability), so it is not auto-`UnwindSafe`.

## What I am unsure about, and want tested rather than confirmed

1. **Is asserting unwind safety honest here?** My argument: the caught object is handed to a *new*
   session, which drains the queue and resets `sense_faults`; the PTT is released by `Drop`; and
   `id_timer.tx_since_id` is a bool that is meaningful whichever value it holds. The counter-argument
   I cannot dismiss: a panic inside `engine_tx.transmit` could leave engine-internal state
   (sequence numbers, HARQ buffers, accumulators) logically half-updated, and I have not enumerated
   what `ModemEngine` keeps across calls. If something there is order-dependent, "not memory-unsafe"
   is not the same as "safe to reuse".
2. **Does catching a panic hide the bug?** A panicking repeater is a defect, not a channel
   condition. Today the thread dies and the log says so. Catching it arguably makes it *more*
   visible (the payload reaches a `CommandError` the operator sees) — but it also makes an
   indefinitely-restartable crash loop possible, bounded only by an operator who keeps pressing
   enable. Whether that needs a "panicked N times, refusing" limit is a real question, and I lean
   no because each enable is already an explicit control-point act with no auto-retry.
3. **Should a panic and a clean `Err` be distinguishable to the operator?** #1328's
   `MAX_SENSE_FAULTS` exit is a *known, designed* stop with a diagnosis. A panic is a bug. Reporting
   both as `CommandError { command: "repeater" }` flattens that distinction.
4. **Is the ID timer trustworthy after a panic?** If a panic can occur between "keyed and
   transmitted" and "`note_tx` recorded it", the recovered timer under-reports an un-IDed
   transmission — which is worse than losing the repeater. I have not checked the ordering.

## Consumer

`crates/openpulse-daemon/src/lib.rs` — the `spawn_repeater` closure (the only `thread::spawn` that
owns a repeater) and `reap_finished_repeater`'s `Err(_)` arm, which is the branch that currently
logs the loss. Driven by CLI `openpulse daemon enable-repeater` / `disable-repeater` and the panel
toggle.

## Prior art

- **`shared_ptt.rs:870`** — the existing unwind test proving the key is released during a panic.
  This is the safety precondition, and it is already gated, which is why this design is even
  arguable.
- **`benchmark.rs:23`** — the workspace's only production `catch_unwind`, without
  `AssertUnwindSafe`.
- **#1324** established that handing the object back is preferable to rebuilding, because a rebuilt
  `StationIdTimer` re-seeds `last_id_ms = now` and defers the next ID by up to a full interval.
  (#1324 first gave the reason as `tx_since_id` being forgotten; that field turned out to be
  unobservable across a pause — corrected in that note and the ledger on 2026-09-10. The clock is
  the part that carries.) That argument applies here too.

## Twins

- **`reap_finished_repeater` and the `DisableRepeater` arm** both `join()` and both have the same
  `Err(_)` arm; a fix must cover both or the panic is recovered on one path only — the #1252 shape
  that has already bitten this feature twice.
- **The PTT watchdog thread** (`daemon/src/ptt.rs:139`) is the daemon's other long-lived thread. It
  owns no reusable object, so it is not a twin for the recovery, but it IS the thing that would
  bound a stuck key if `Drop` somehow did not run — worth confirming it survives a repeater panic.
- **`twin.rs:187,230`** spawn threads in the test harness only.

## Review outcome (Fable, 2026-09-10) — the §97.119 defect is SHIPPED, and my design had a regression

**1. The under-report is live today on the `Err` path, not a panic hypothetical.** I framed it as a
window that catching the panic would newly expose, and said it was "masked because losing the
repeater also loses the timer". **That is false.** `CpalOutputStream::flush`
(`cpal_backend.rs:424`) returns `Err("flush timeout …")` *after* the samples have been queued to the
soundcard and are playing — the buffer failing to drain is precisely the case where audio went out.
So `transmit` returns `Err` for a transmission that reached the air, the repeater's `?` skips
`note_tx`, and **#1324 already hands that repeater back with `tx_since_id == false`**. The station
then skips an ID it owes. Verified in the source, and it needs no panic at all.

The fix is the reordering I proposed, for a stronger reason than I gave: `note_tx` moves to
immediately after `acquire_key()?` and before `transmit`. Nothing else reads the repeater's timer;
its `signoff_idle_ms` is 0 so `last_tx_ms` is inert; and `maybe_identify` already has the safe
ordering (transmit the ID, *then* `mark_identified`). Over-arming on a failed transmit costs at most
an extra ID under a later key, which is legal.

**2. My proposal introduced a stuck-key REGRESSION, and my own supporting fact was the reason I
missed it.** In full duplex the live `PttKeyGuard` is stored in `self.session_guard` (`:278`), not on
the stack, and unwind does not run the `session_guard = None` at `:409`. rig_b is released today only
because the closure's `repeater` is **dropped** during unwind, dropping the struct and thus the
guard. My design stops dropping it — so a panic anywhere between `:278` and the next `acquire_key`
(the idle `recv_timeout` and the whole `decode_burst` DSP surface, i.e. most of a session) would hand
back a repeater still holding rig_b **keyed**, bounded only by the 180 s silence watchdog, and a
re-enable inside that window would `extend()` the stale key and carry on.

My "fact 2" — the existing `shared_ptt.rs:870` unwind test — is **true for a stack guard and false as
generalised**. It is the artificially-easy fixture for exactly this design: I quoted a property of
today's drop-everything behaviour in support of a change that removes it. The panic arm must clear
`session_guard` (and `sense_faults`) explicitly before handing the repeater back.

**3–5.** No restart limit (each enable is an explicit operator act; there is no retry), but the note
must explicitly reject the shape someone would "improve" toward — a per-burst catch *inside* the
relay loop, which would be a real hidden crash loop. A panic is distinguished from #1328's designed
exit by the `CommandError` reason prefix and an `error!` log level, not a new `ControlEvent` variant
(the panel destructures exhaustively). The default panic hook still prints before `catch_unwind`
returns, so catching does not hide the bug from the log. Facts 1 and 3 hold as written; the real
precondition for fact 1 is not the manifest but that `PttKeyGuard::drop` cannot itself panic (a panic
in a `Drop` during unwind aborts regardless of profile) — it cannot: poison-tolerant lock, `let _ =`.

**Filed rather than absorbed:**
- The same `frames_transmitted`-after-`flush` ordering (`engine.rs:6844-6853`) means the **daemon and
  ARDOP** arm their ID timers from a delta that the cpal flush timeout also suppresses — the identical
  §97.119 defect on the main rig, in two more front-ends.
- The repeater never IDs while idle: `maybe_identify` runs only inside a relay, there is no idle tick,
  and `signoff_idle_ms` is 0, so a repeater that relays once and then hears silence never sends the
  end-of-communication ID. Pre-existing, and it bounds what carrying the timer across a restart buys.

**Rebuild-instead-of-catch, recorded as declined:** panic → drop (which releases the key for free),
keep only the `StationIdTimer` outside the thread-owned object, rebuild the engines and PTT on the
next enable. It dissolves the unwind-safety question entirely and gives the DSP a clean slate after a
bug. Declined because `server.rs` builds the repeater inline and the burst `SyncSender` lives in
`RuntimeControlState`, so it needs a `build_repeater(cfg)` extraction plus a channel swap — and
because #1324's ID-timer argument for keeping the object still applies. Recorded so the next person
sees it was considered.

## Second review round — the "shipped defect" was retracted, and #1324's rationale corrected

I applied the reorder, wrote a gate for it, and **sabotage showed the gate could not fail**: restoring
the old ordering left it green. `maybe_identify` has exactly one caller, immediately after the
`transmit` that arms the bit, so `tx_since_id` is written and read inside a single call and its value
across a pause is unobservable. Several relay timings (0 / 300 / 601 s, and a second failing
transmit) produce identical ID behaviour either way.

The reviewer retracted the claim: **not** "the under-report is shipped on the Err path". The timer
STATE is wrong; the consequence is unreachable until something reads the timer without a preceding
successful transmit. The reorder ships as **state correctness, unobservable today** — not as a fix
for a live missed ID — and the non-discriminating test is deleted rather than kept, because a test
that passes with and without the change reads as evidence while proving only that the call exists.

**The same argument invalidated #1324's own stated rationale**, which is the more valuable catch: "a
rebuilt `StationIdTimer` forgets that un-IDed transmissions happened" is about `tx_since_id`, and is
unobservable for exactly the reason measured here. What the hand-back observably preserves is the ID
**clock** — `StationIdTimer::new` sets `last_id_ms = now`, so a rebuild defers the next ID by up to a
full interval. #1324 was right in effect and wrong about the field. Corrected in place in that
note and in the ledger, per the retraction rule; the merged commit message cannot be amended, so the
ledger carries it.

**Still standing from round one, and unaffected:** the full-duplex `session_guard` regression. That
is the finding that can hurt hardware, and it is what the panic arm must fix.

**`UNCHECKED`:** the daemon/ARDOP twin (they arm from `frames_transmitted`, bumped after `flush`).
The daemon's tick *does* poll its timer without a preceding transmit, so that one may be observable
where the repeater's is not — nobody has measured it, and it is recorded as a question, not a
finding.
