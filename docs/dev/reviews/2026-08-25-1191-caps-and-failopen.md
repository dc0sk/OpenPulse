---
project: openpulsehf
doc: docs/dev/reviews/2026-08-25-1191-caps-and-failopen.md
status: review
last_updated: 2026-08-25
---

# Adversarial review — #1191 handshake caps, and a fail-open finding (#1199)

**Reviewer:** Fable · **Date:** 2026-08-25 · **Covers:** a FINDING and a DESIGN DECISION, both
reviewed before anything was written into an issue, commit or PR.

## Prompt

Sent with the apparatus: issue #1191, a measured budget from a throwaway `examples/cap_probe.rs`
calling the real `ConReq::create`/`ConAck::create` with every field at its cap, and a probe test run
inside `openpulse-daemon` and then deleted.

The finding put up for attack: **the F1 fail-open is still live, reachable through the callsign.**
Evidence — `local_callsign_valid()` checks only non-empty and not-`N0CALL`; `ConReq::create` refuses
a 13-byte `station_id`; the daemon's arm is `Err(e) => tracing::warn!(…)`; and a comment four lines
above describes that behaviour in the past tense.

The design question: since no cap covers every legal callsign form, does the failure behaviour
matter more than the number? Tentative ordering offered for falsification — fix the fail-open,
validate at config load, then choose a cap (15, or 16 with a `PROFILE_NAME` trim).

Asked explicitly: what is the defensible inventory, given I could find no authoritative maximum and
did not want to assert one I could not source?

## Verdict

**Numbers confirmed** independently, field-by-field, plus the in-repo budget gates re-run: CONREQ
224 / CONACK 244 at cap 12; 2 bytes per extra id byte; 7 bytes of CONACK headroom; cap 15 the
ceiling without a trim.

**The finding is confirmed, and is worse and wider than I claimed:**

1. **More silent than stated.** By the time `ConReq::create` runs, the handler has already succeeded
   `begin_secure_session` (unknown peers get **Full** trust deliberately), emitted
   `RfConnectionChanged { connected: true }`, opened the logbook QSO and set the QSY token. On
   failure: no rollback, no `CommandError` — **and `pending_handshake` is never set, so the 30 s
   handshake-timeout `CommandError` cannot fire either.** A lost CONACK reports a timeout; a build
   failure reports *nothing, ever*. No CONREQ is transmitted, so the peer never learns the session
   exists: a one-sided local fiction that can still key the transmitter.
2. **I missed a route.** `dst_station` is the *dialled peer's* callsign, so a long PEER callsign
   trips the same arm with a short local one — and **config-load validation cannot close that
   route.** A third route is an empty `dst_station`.
3. **The defect is the arm's failure policy, not the cap**, and "#1190 never fixed the arm" is
   unfair as framing: F1 was the cap *contradiction* plus the silence; #1190 removed the
   contradiction and named the silence, then left it. File the residual as its own finding.
4. **`local_callsign_valid` is not the gate on this path — it is not consulted at all** (its call
   sites are §97.119 guards on autonomous paths), so my probe's `valid_gate=PASS` was doubly true
   and beside the point. `main.rs` already refuses `N0CALL` at startup and is the natural home for
   validation.

**My filexfer claim was true but mischaracterised.** `SENDER_ID_MAX = 16` exists, but `write_string`
**silently truncates** and `encode_signed_fields` then signs the truncated identity — so filexfer
never rejects any length; it ships a validly-signed mangled identity. Three policies for one datum
(silent downgrade, silent truncation, hard error at AX.25's 6). That argues for one number **and one
policy — reject loudly — across both**, not for inheriting 16, since `SENDER_ID_MAX` is itself an
example-free constant. "Do not launder one unjustified cap through another."

**On the inventory:** there is no authoritative maximum, and saying so is the correct answer. ITU RR
Article 19 bounds ordinary calls (~7) but lets administrations authorise longer special-event calls,
so the base is unbounded in principle and compound decoration stacks on top. The cap is a **policy
number**; its justification is the documented inventory it covers, loud rejection of the rest, and
one shared constant.

**Ordering upheld, with the step-1 shape changed:** a `CommandError` at the old position is
insufficient — it leaves the announcement, the QSO and the token behind. **Reorder the handler**:
build the CONREQ first, and on failure open nothing.

**On the cap:** 16 beats 15. `PROFILE_NAME` 24 → 20 is better than 24 → 23 (the longest generator is
`hpx_pilot_fast_rrc` = 18, from the closed `by_name` set). **Better still: remove `dst_station` from
the CONACK** — no consumer found; the initiator binds by `conreq_hash` + `station_id`, and #1178's
spent-RF rationale does not transfer because a CONACK has exactly one consumer, self-selected by
hash. That frees ~17 B and removes the trim entirely, but is a separate wire decision needing its
own review. **Two-fragment variable length rejected outright** — it reverses #1147 (p vs p³).

## Verified before writing anything down

All eight items the review listed. Two results are worth recording:

- **An empty callsign passes the startup gate** — it checked the `N0CALL` sentinel only. Confirmed.
- **The autonomous route is NOT live.** The review asked me to check whether a JS8-decoded callsign
  can exceed the cap before claiming rendezvous can fire the silent arm unattended.
  `unpack_alphanumeric50` builds an **11-character** array, so a decoded callsign is at most 11 —
  under the cap. This **narrows** the finding, and the narrowing is recorded in #1199.
- `ack.dst_station` has no consumer outside tests (positive control: the filter finds
  `req.dst_station` in the daemon). No app or UI reads it.

## Applied

Split into #1199 (the fail-open residual) and the cap decision on #1191. #1199 implemented here:
the handler is reordered, failure emits `CommandError` and opens nothing, and startup validation now
refuses an empty callsign and one over `caps::STATION_ID` — **taken by reference, never
transcribed**. The regression test asserts the whole property (error emitted, connection NOT
announced, no QSO, no pending handshake) across all three routes, with a positive control, and is
sabotage-verified against the restored old policy.

Maintainer chose **cap 16 via removing `dst_station` from the CONACK**; that is a separate change
and gets its own review pass.
