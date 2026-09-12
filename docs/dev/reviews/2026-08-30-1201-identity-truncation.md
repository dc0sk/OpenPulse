---
project: openpulsehf
doc: docs/dev/reviews/2026-08-30-1201-identity-truncation.md
status: review
last_updated: 2026-08-30
---

# Review — #1201: filexfer signed a truncated identity

Reviewer: Fable (adversarial), two rounds. Date: 2026-08-30. Design reviewed **before**
implementation, and again mid-implementation when the build falsified the approved shape.

## Prompt (round 1)

`openpulse-filexfer` capped the station identity at `SENDER_ID_MAX = 16` and `write_string`
**truncated** on a char boundary rather than refusing; `encode_signed_fields` then signed the
truncated form. The issue left one thing explicitly unverified — whether a truncated `sender_id` is
compared against a full callsign and fails, or is accepted as the peer's identity — noting that this
decides whether the defect is a correctness annoyance or a trust defect.

Submitted with measurements (`caps::STATION_ID = 18` and erroring both directions; filexfer's reader
errors above max while its writer truncates; the receiver looks `sender_id` up in `verified_peers`),
a three-part design, and four questions: whether a truncating writer should exist at all, whether
raising the cap is a wire break belonging in the pre-1.0 package, whether the demotion is worth
fixing separately, and what the test must be.

## Verdict (round 1)

**My headline was wrong, and the leg I had flagged as unread is what made it wrong.** I had read
`peer_allowed` and the policy struct but not `decide()`, and concluded "silent trust demotion —
processed as an anonymous unknown peer, no error anywhere". In fact `decide()` gates again on
`policy.require_verified_peer`, which **defaults to `true`** in the crate, the config schema and the
template. So under defaults the behaviour is a **false rejection**: a verified peer with a 17–18
byte callsign has its offers rejected on air as `UntrustedPeer`, between two stations that had just
handshaken successfully. The demotion variant exists only where an operator has opted out of
requiring verification — and there, treating unverified peers as unverified is the posture they
chose.

So the issue's dichotomy resolves to its **first** branch: the truncated id is compared and fails.
Correctness defect, with a diagnosability sting — the reject reason and `signature_valid: false`
both point at *trust*, and nothing in the chain names truncation. Two further corrections: it is the
**offer** signature inside the signed span, not the `TransferManifest` (which signs the full local
callsign); and "no error anywhere" is literally false — a rejection does go on air, what is missing
is attribution. The reachable window is exactly 17–18 bytes: ≤16 truncates nothing, >18 fails the
handshake loudly and never enters `verified_peers`. Real, narrow, not to be oversold.

Also established: `verified_peers` has exactly one production population site (the handshake verify
path), and a truncated prefix colliding with a different station's callsign still cannot borrow its
trust, because verification runs against *that* station's key.

## Round 2 — the build falsified the approved design

The approved shape was `write_string -> Result` (refusal as the wire-layer default) with a
`write_string_truncating` sibling for the cosmetic fields. Implementing it forced the `Result` up
through `encode_signed_fields`, `signing_bytes`, `encode_body` and finally `FxFrame::encode()` —
which is called from **eight-plus sites inside `Vec<FxAction>`-returning state-machine functions
with no error channel**. And only the `FileOffer` variant can fail to encode; every other frame type
is infallible. The realistic outcomes were `.expect()` (banned in this repo's library production
paths) or threading `Result` through the whole session API for one field.

Re-reviewed rather than decided unilaterally. **Verdict: make the invalid state unrepresentable, and
go one step further than a private `String` — a validated newtype.**

- `SenderId::new(&str) -> Result<Self, FxError>` is the only construction door; `Reader::string`
  rejecting above the same cap is the other. A decoded offer is therefore always in-domain.
- `SenderId::write_to` is **honestly infallible** — its domain contains no failing input. That is
  parse-don't-validate, not a swallowed error, which is what dissolves the question a private
  `String` would have left open (a `debug_assert` there would have been the comment-that-cannot-fail
  shape).
- `encode_signed_fields`, `signing_bytes`, `encode_body` and `FxFrame::encode` all stay infallible.
- **`write_string` is deleted**, having zero callers afterwards: a refusing writer with no reachable
  error path is the defined-but-unconsumed construct the reachability ratchet exists to catch. The
  rule it carried now lives on `write_string_truncating`'s doc, where the two remaining callers are.

The bypass doors were checked, not assumed: `FileOffer` derives only `Debug, Clone, PartialEq, Eq` —
no `Default`, no `Deserialize` — and Rust refuses functional-record-update when any field is
invisible, so there is no struct-literal or `..old` path. All six construction sites already go
through `from_manifest`.

**Cap shared by reference** (`SENDER_ID_MAX = caps::STATION_ID`) with a `const _: () = assert!(…)`
making the `≥` invariant mechanical, and a sentence at the definition recording that raising
`STATION_ID` is thereby a filexfer wire change — the #1120 shape, said out loud rather than left to
be discovered.

**Standalone, not queued behind the wire-break package.** It is a wire-contract change (a
pre-change receiver `FieldTooLong`-rejects an 18-byte id), but the direct precedent is #1191, which
raised the handshake caps under the frozen `WIRE_VERSION 0x01` and shipped on its own.

**Diagnosability: one log line, no new mechanism.** A `debug!` naming the claimed `sender_id` at the
lookup miss is what would have made the original defect readable in one pass of the log. A
prefix-match heuristic was explicitly rejected as a detector fitted to a bug this change makes
unreachable.

## Tests, and why the obvious one is worthless

A codec round-trip **passes trivially against the defect**, because encode and decode truncated
consistently. The assertions are against the *input*, and the test that pins the bug is the
daemon-level one, since a field-codec test never touches the trust binding where the damage was.

Both sabotages were run and are **complementary**, which is what makes them evidence: restoring the
16-byte cap fails the three cap tests while the refusal test correctly still passes; making the
constructor truncate instead of refuse fails only the refusal test. Neither fails everything. The
daemon test, run against the old truncating behaviour, reproduces the defect verbatim — *"the
offer's identity must be the station's own, not a prefix"*.

The single-fragment assert on a maximal offer is new here and has no predecessor in this crate; the
handshake got its twin in #1147. The doc's arithmetic was re-derived (208 body + 6 header = 214
against 251) rather than edited from 212 in place.
