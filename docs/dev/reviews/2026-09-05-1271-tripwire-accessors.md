---
project: openpulsehf
doc: docs/dev/reviews/2026-09-05-1271-tripwire-accessors.md
status: review
last_updated: 2026-09-05
---

# Review — #1271: the tripwire-accessor collision

Reviewer: Fable. Reviewed **before implementation**, per the standing rule. The design that shipped
is not the one submitted — the review rejected the proposal's mechanism outright.

## Prompt

Sent with the seven accessors, their consumers, and the two contradicting rules, framed for
falsification:

> `CLAUDE.md`'s cross-cutting checklist REQUIRES "add a runtime tripwire and assert it increments on
> the production path". Seven `ModemEngine` accessors exist for that. Against it stands the rule that
> a probe needing access is a unit test, not an exported accessor. All seven are consumed only from
> `openpulse-modem/tests/` and from test modules in the daemon; none has a production caller.
>
> My proposal: split them. `is_agc_enabled`/`is_notch_enabled` are not instruments — the daemon
> accepts `SetAgc`/`SetNotch` and exposes no way to read the state back, which is a
> defined-but-not-consumed asymmetry; wire them into the control plane and they become
> production-reachable honestly. The four counters and `agc_gain_db` stay `pub` and baselined.
>
> Test that instinct rather than confirming it. Is the asymmetry real? Is adding a control-plane
> surface the right fix or scope creep dressed as one? Is `agc_gain_db` really not operator-facing?
> Is there a fourth option resolving the collision structurally rather than by exemption? And flag
> anything wrong or unproven in my framing.

## Verdict — the proposal's mechanism does not work

**The asymmetry is real and worse than I described; the fix I proposed would not have fixed it.**

`GetConfig` is handled in `handle_command` on the **client task**, whose `ClientCtx` holds
`Arc<Mutex>` mirrors and **no engine**. Extending `DaemonConfig` the way its existing fields work
means adding a daemon-side *shadow* written by the `SetNotch` dispatch. `is_notch_enabled()` would
still have zero production callers and stay in the baseline — the change would have paid none of the
debt it was proposed to pay. The only seam where the daemon reads engine state for clients is the
main loop's `MetricsSnapshot` (`server.rs`, e.g. `m.afc_correction_hz = engine.last_afc_offset_hz()`),
broadcast at 1 Hz.

**The defect is also bigger than "a reconnecting panel".** Shipped config defaults are
`notch_enabled: true` and `cessb_enabled: true`, applied at startup; the panel keeps shadow bools
initialised `false` and never seeds them. So a **default install paints Notch and CE-SSB OFF while
both are ON, from the first frame**, and the first click sends a no-op that flips the display. And it
is a class of **five** write-only toggles (`SetNotch`, `SetAgc`, `SetCessb`, `SetLogbook`,
`SetDcdSquelch`), not two. `agc_on: false` happens to be correct because `agc_enabled` defaults
false — so two invert, three are merely unreadable. Filed as #1276.

**It is not the #1252/#1123 class.** Those were dead inbound paths affecting on-air behaviour; this
is a display asymmetry. Nothing on air is wrong. Do not inflate it.

## Three claims in my issue text were wrong

1. **"The asserting test lives in another crate, so the accessor must be `pub`."** The other crate is
   *Cargo's integration-test model*, not the daemon. The collision is a **placement** fact, not a
   principle.
2. **"The daemon assertion is the one that proves the production path."** It does not exist:
   `crates/openpulse-daemon/tests/` holds no tripwire assertion, and **nothing asserts a tripwire
   through `server::run`**. One of the two daemon-side uses calls `engine.accumulate_capture`
   directly — a modem entry — and its `notch_blocks_processed() > 0` is **redundant**, since the
   counter increments immediately before `apply_rx_notch` and the interferer list is populated only
   inside it. Deleted at zero loss.
3. **`agc_gain_db` is misclassified.** Its own docstring calls it "a readout of the active-span loop
   state" where the counters' say "tripwire", and the tests calling it assert behaviour (gain > 6 dB)
   while using `agc_blocks_processed` as the tripwire in the same test.

Also: I wrote "#1270 fixed the cfg stripper". #1270 is the issue; **PR #1272** is the fix.

## The fourth option, and the one rejected

- **A Cargo `instruments` feature** (`#[cfg(feature = "instruments")]`, enabled via a self-dev-dep;
  `resolver = "2"` keeps dev-dep features out of `cargo build`) is the only option where "no
  production caller" is enforced by the **compiler** — a production caller cannot compile without a
  visible `Cargo.toml` diff. Filed as #1277, with classifying the ~65 `engine.rs` baseline entries
  (instrument vs dormant API vs should-be-private) as the first deliverable, because applying it to
  a dormant-API item would make its eventual production consumer impossible to write.
- **`#[doc(hidden)] pub` plus a ratchet exemption — REJECTED.** It is a real ecosystem idiom and the
  tree already has one instance, but it converts a gate into a convention: a future
  `#[doc(hidden)] pub fn measure_preamble_rho` would pass silently, where the baseline's "shrink this
  list" header makes it a visible decision. It removes precisely the friction that caught #1121.

## `add_trusted` / `add_revoked`

Delete, in a separate mechanical commit: both are `add_entry(id, key, Full|Revoked)` verbatim, with
**17** call sites (not the 16 I claimed). It decides nothing, so it is out of review scope by
`CLAUDE.md`'s own line — but leaving them costs a permanent baseline label for a ten-minute change.

## What the review did not anticipate, found by doing it

The rewrite is not quite free. `PublicKeyTrustLevel` lives in `openpulse_core::trust`, not
`::handshake` — which the convenience methods had hidden from every call site. The resulting error
reads **"enum `PublicKeyTrustLevel` is private"**, and I briefly concluded the enum really was
private, that the two methods were a deliberate façade over it, and that the deletion should be
reverted. It was my import path. A misleading compiler message nearly reversed a correct decision;
what settled it was asking how *other* consumers name the type, not re-reading the declaration.

## A correction to a stored memory

`feedback_no_public_api_for_instruments.md` asserted "`pub(crate)` does not help: `PUB_ITEM` matches
`pub(...)` too". **Stale since PR #1272**, which narrowed `PUB_ITEM` to bare `pub`. Corrected. A
memory that names a mechanism goes stale when the mechanism is fixed, and this one was caught by a
reviewer rather than by re-reading the script.

## One gap recorded rather than fixed

`CLAUDE.md`'s checklist item (5) accepts `accumulate_capture` *or* the twin harness as the production
entry. Every tripwire assertion uses the former; none goes through `server::run`. A defect where the
daemon's `rx_ticker` bypasses `accumulate_capture` — the #1118 shape — would evade every tripwire in
the tree. Recorded in #1277.
