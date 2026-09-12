---
project: openpulsehf
doc: docs/dev/reviews/2026-08-26-1164-envelope-version.md
status: review
last_updated: 2026-08-26
---

# Adversarial review — making `WireEnvelope`'s version authoritative (#1164)

**Reviewer:** Fable · **Date:** 2026-08-26 · **Covers:** a wire-format design decision, reviewed
BEFORE implementation.

## Prompt

Sent with the apparatus: the decode site and its "forward-compatible: parse but don't reject"
comment, my verification that `VERSION` is written once and read nowhere, that no v1 encoder exists
in the tree, and how the siblings reject. Three decisions put up for attack — reject vs real
forward compatibility; keep `VERSION = 2` or reset to 1 to match #1191's freeze; and whether the
probe should distinguish "not an envelope" from "an envelope this build cannot speak", given that
#1191's freeze removed the handshake version's ability to signal build divergence.

## Verdict

**Two of my stated claims were false, and one of them changed the design.**

1. **"The only production decoder is a probe" — FALSE.** There are **four**: the ardop probe, an
   identical kiss probe, a daemon relay probe, and **`openpulse-mesh`, which is not a probe at
   all.** The mesh is the format's primary consumer and its `Err(_)` arm does not return silently —
   it routes the bytes into `SarReassembler::ingest` on the theory that any decode failure means "not
   an envelope, therefore a SAR fragment". That assumption stops being true the moment the version
   becomes authoritative.
2. **"The probe runs on every received payload" — overstated.** All three probe sites early-return
   unless a relay forwarder is configured.

**F1 — forward compatibility is structurally impossible, which settles decision 1.** Three facts
compose: the version byte at offset 4 is inside the Ed25519-signed span (`signing_bytes` zeroes only
`HOP_INDEX_OFFSET`); `decode` does not carry the version into the struct; and a relay re-encodes via
`header_and_payload`, stamping the **local** constant. So a tolerated foreign envelope would be
re-stamped on forward and the originator's signature broken at the next hop. "Parse but don't
reject" was not weaker forward compatibility — it was none, plus a false comment. Rejection is
**forced**, not chosen.

**F2 — the mesh arm discards the signal by an accidental byte coincidence.** A version-rejected
envelope fed to `ingest` has `"OPHF"` parsed as a SAR header: `fragment_index = 'H' (0x48)`,
`fragment_total = 'F' (0x46)`, and it survives only because `0x48 >= 0x46` trips
`FragmentIndexOutOfRange`. Dropped today by coincidence, not by design.

**Decision 2 — keep `VERSION = 2`, for a concrete reason rather than taste.** Version 1 has a
historical meaning (16-byte `auth_tag`). A v1-era build is trivially reconstructed during a `git
bisect` on the twin rigs — exactly where old talks to new. Reset to 1, such a frame would **pass**
the version check and die in the trailer, reintroducing the garbled failure the check exists to
remove. Kept at 2, it is rejected by number. "The coherence you want with #1191 is the *policy*;
matching the numeral is cargo cult."

**Decision 3 — distinguish, but two premises corrected.** The warn is not noise: reaching the
version check requires the `OPHF` magic to have matched (~2⁻³² per random payload). But my framing
that this is "the one clean signal that builds differ" **overclaims in the same way as the comment
being deleted** — it fires only on relay and mesh stations, and only for control-plane traffic;
ordinary sessions, handshakes and filexfer never reach this decoder. And with four call sites, the
warn belongs **inside `decode`**, at the single shared seam, not per-caller.

**Decision 4 — the trailer logic is not dead.** It stops being a *version* discriminator but remains
v2's own signed/unsigned discriminator, which is a live feature. Only the comment's cross-version
claim dies. The `_ => BufferTooShort` arm is also misnamed for an over-long trailer.

## Applied

All of it, with one refinement the review did not specify. Renaming the trailer error wholesale to
`MalformedPayload` broke `envelope_rejects_missing_signature`, and correctly: a **truncated**
signature genuinely is too short. Both names are right for one direction each, so the error is now
reported **by direction** — `BufferTooShort` under 64, `MalformedPayload` over.

The warn sits at the decode seam; the mesh matches `UnsupportedVersion` explicitly **before** the SAR
fall-through; `VERSION` stays 2 with the bisect rationale recorded at the constant; the doc's
self-contradiction is resolved in favour of line 22, with the structural-impossibility reasoning
written down so nobody re-proposes it.

Sabotage-verified: with the check removed, a foreign-version frame decodes as `Ok(...)` — the defect
itself — and both new tests fail. Restored and re-verified.
