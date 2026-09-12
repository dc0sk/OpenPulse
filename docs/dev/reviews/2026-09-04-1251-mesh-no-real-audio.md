---
project: openpulsehf
doc: docs/dev/reviews/2026-09-04-1251-mesh-no-real-audio.md
status: review
last_updated: 2026-09-04
---

# Review — #1251: openpulse-mesh beacons and relays with no station ID, PTT or carrier sense

Reviewer: Fable (adversarial). Date: 2026-09-04. Reviewed **before implementation**. Verdict: *do not wire it — remove its ability to reach real audio instead.* The review corrected three claims in the issue itself and rejected the fix I was leaning toward.

## Prompt

Carried the defect (three absences, each verified with a positive control, plus a beacon carrying no callsign field), the four gates that bound it (`enabled` defaults false, `cpal` opt-in, `N0CALL` refused, VOX-only), and an explicit fork: give mesh what its siblings have — `SharedPtt` + watchdog, CSMA, a `StationIdTimer`, mirroring what had just landed for ARDOP in #1250 — or decide it is not an on-air transmitter yet and make it refuse a real backend. I said my real uncertainty was which, and asked for evidence rather than a preference. I also flagged the regulatory reading and the relay-versus-beacon distinction as the parts I understood least and asked for hedges rather than confident answers.

## Three corrections to the issue

1. **"Mesh is the only transmitting front-end without a `StationIdTimer`" is false.** The match I used as a positive control in `openpulse-kiss` is a doc comment stating the KISS TNC *deliberately* has none, because AX.25 carries the source callsign in every frame. The true statement is that ARDOP (before #1250), KISS, mesh **and the daemon's own relay/handshake/QSY paths** all had the same hole — the blind-sibling archetype, not a mesh peculiarity. That observation is what became #1262, which outranked this issue because the daemon is the shipping on-air path.
2. **"Mirror the daemon" was the wrong template**, because the daemon's relay path was itself unkeyed at the time.
3. **Engine CSMA would make mesh drop traffic, not defer it.** `next_beacon` advances `last_sent_ms` before the transmit is attempted, and `RelayForwarder::forward` inserts its `(session_id, nonce)` dedup key *before* returning the envelope — so a `ChannelBusy` on a forward is a **permanent** drop, since any retry is "duplicate". On a clear channel 0.3-persistence CSMA would discard roughly 70 % of traffic. The daemon explicitly rejects engine CSMA for its own beacon for a related reason. What this actually needs is a deferred-TX queue drained when the channel is clear — not a flag flip.

## The evidence that settled the fork

Documented as an on-air transmitter: roadmap 6.3 done, README rows, CHANGELOG, and literal `--features cpal` + `--backend cpal` recipes in both the book and the manual; `release-1.0-criteria.md` says 1.0 ships a mesh relay.

Never actually on air: no mention in `onair-twin-ota.md`, `ota-hardware-validation.md`, `loopback-revalidation-plan.md`, `virtual-loopback.md` or `scripts/` (control: `openpulse-daemon` appears throughout); the matrix row for its capability says "Not separately run this session"; the crate's entire TX-touching history is protocol logic.

Decisive: **`regulatory.md` lists this project's automatically-controlled stations under §97.221 as the repeater and the JS8 beacon, and omits mesh** — while stating elsewhere that unattended relay nodes *are* automatically controlled stations. The JS8 beacon was deliberately held until that document existed as its prerequisite gate. **Mesh skipped a gate the project already imposed on its own sibling.** It also has no control point at all — no control port — so "terminate transmissions" means SIGKILL and hoping VOX drops.

## Verdict and reasoning

Do not write a fourth hand-rolled copy of a keying pattern that has been wrong four times (#1250 ARDOP, #1259 KISS, #1260 repeater, #1262 the daemon), on a crate with no §97.221 mapping, no control point, and no on-air record. Remove the capability instead: a capability that does not exist cannot be mis-invoked, which is the project's own bias toward banning a construct over adding a guard.

On-air mesh should restart from the regulatory mapping and the automatic-control sub-band question — an unattended station that *originates* transmissions is restricted to the §97.221(b) sub-bands, and nothing in the bandplan code knows that — then the deferred-queue DCD design, then a shared keyed-emission helper.

## Hedges the review kept, rather than resolving

On identification form: the existing `StationIdTimer` model (armed by TX, interval ID, sign-off ID) fits a beacon-only station, and a separate `DE <call>` frame is the accepted form here — but whether `DE CALL` in OpenPulse's own framing counts as a "specified digital code" under §97.309 is genuinely ambiguous, which is why digital modes carry a CW ID; for an unattended station the review would default CW ID on. The response's `callsign_hash` is **not** identification — a SHA-256 is unreadable by design.

On relaying third-party traffic: keying and §97.119 obligations are identical to originating, since the emission is this station's. What differs is accountability — a store-and-forward digital relay is plausibly a §97.219 message forwarding system, where the first forwarding station must authenticate the originating station's identity. That is exactly what #1253 defeats by importing a self-asserted `trust_state = 0x00` as `Verified`, and it is why #1253 should land before any on-air mesh work.

## Gate

A static scan, in the #1250 style: fail on any `cpal` feature or `CpalBackend` reference, validated against planted inputs so a rotted pattern fails rather than passes. There is no runtime fixture that can observe a `cfg` arm that no longer compiles.

## Split out

The daemon's unkeyed relay path (**#1262**, since fixed and merged); **#1253** before any on-air mesh work; on-air mesh as a design item starting with the §97.221 row; and the REQ-MAC-02 matrix row that marks carrier sense covered while `csma_enabled` defaults false and only KISS enables it (**#1267**).
