---
project: openpulsehf
doc: docs/dev/reviews/2026-08-27-1206-authbeacon-version-token.md
status: review
last_updated: 2026-08-27
---

# Review — #1206: give the FreeDV auth beacon a magic and version token

Reviewer: Fable (adversarial second opinion), 2026-08-27, **before implementation**.
Outcome: inclusion confirmed; **two of my supporting facts corrected**, one design option killed.

## Prompt

Asked for falsification, not agreement, of a decision between two viable options on issue #1206:
**(a)** bring `AuthBeacon` into the pre-1.0 wire-format break package with a magic + version token,
matching what #1162 (QSY) and #1163 (rendezvous) had just shipped, or **(b)** record an explicit
exemption in `release-1.0-criteria.md`.

Apparatus sent with the question — all verified against the source before asking, not quoted from
the issue:

1. **The issue's own evidence looked wrong.** #1206 says the beacon is sent as "bare JSON… no
   framing added", quoting `encode()` as returning canonical JSON. The code emits
   `[u16 BE len][JSON]`, and `git log -S` puts that prefix in 453aaa20, the original FF-11 commit
   (#162) — predating the issue. Asked the reviewer to check both halves.
2. **The crate looked unwired**, on two independent anchors: every `Cargo.toml` in the tree, and
   `cargo metadata` over all 41 workspace packages — zero dependents, no `[[bin]]`. Flagged my own
   risk of repeating a false-census error (four in recent sessions) and asked for that to be checked
   specifically.
3. It is nonetheless a **signed identity assertion** (`SigningDomain::AuthBeacon`, tag `OPAB`,
   `Prepended`, version `0x01` — and a prepended tag is hashed, never transmitted, so that version
   is invisible on the wire).
4. It rides **FreeDV's codec2 data channel**, not the OpenPulseHF modem waveform, so a later break
   does not re-open the recorded on-air corpus — the thing that makes the other package items
   expensive to defer.
5. `FF-11` appears **nowhere** in `release-1.0-criteria.md`; the package table lists 8 items and the
   beacon is not among them.

Stated inclination (offered for falsification): include, because the change is small, has no on-air
consequence today, and an exemption costs comparable effort while leaving a signed identity
assertion unversioned. Explicitly asked: does "unwired" argue FOR exemption rather than inclusion?
Is there a better third option? Is squatting the version on the length prefix's high byte a trap?

## Verdict

**Include — confirmed, and the repo's own documents close the exemption door harder than I argued.**

Corrections to my framing, all of which changed the shipped work:

- **My Q4 premise was falsified.** The length high byte is `0x01`, never `0x00`: a representative
  beacon is 356 B of JSON and the hex nonce/pubkey/signature fields alone floor it near 341 B.
  Squatting on it was dead on its own premise, and would have been a trap regardless — a length
  digit that flips at 512 B, and old decoders parsing a magic as a length. Independently measured
  afterwards: **356/357 B of JSON, 363/364 B on the wire.**
- **The issue is wrong twice, not once.** Beyond the length prefix, its `// canonical JSON`
  annotation describes `BeaconBody` (the signed subset) while `encode()` serialises `BeaconWire`
  (all fields plus signature) — two different serialisations.
- **The census holds, with a positive control** (0 dependents against 27 for `openpulse-core`), but
  "unwired" conflates three senses. No in-repo dependent: true. No binary: true — and the crate is
  *incomplete against its own design doc*, which specified one. No **external** consumer: unknowable,
  and `openpulse-manual.md:797-805` actively instructs operators to build companion processes around
  this crate. That third sense *strengthens* inclusion.
- **"Unwired" argues FOR inclusion**, by the registry's own precedent: `signing_domain.rs` records
  for QsyLine that the conversion was free exactly once *because* the signed path had no production
  caller. Unwired is the window, not the excuse.
- **The strongest argument was one I had not made:** the shipped format already exceeds its own
  feasibility budget by ~2.5× (144 B binary designed, ~356 B JSON shipped; ~12 s vs ~30 s of
  FreeDV-1600 text channel). A JSON→binary re-encode is therefore likely if FF-11 is ever wired, and
  the version token is precisely what makes that a **branch instead of a break**. This, not symmetry
  with the siblings, is the load-bearing reason.
- **One overstatement corrected:** "unversioned forever after the tag" is too strong. Because it
  rides FreeDV rather than the modem waveform, a later break is *cheaper* than a `Frame` break, just
  not free. The honest sentence is "frozen into a break-requiring format for any external consumer".
- **"~10 lines" was understated.** Following the registry's precedent makes it a #1162-class change
  (registry edit, placement flip, tests, package-table row, ledger entry) — the same size #1162 was
  judged worth for an equally uncalled path, but it should be presented at its real size.

Format directed by the review and implemented as specified: `[OPAB][version: u8][u16 BE len][JSON]`;
`SigningDomain::AuthBeacon` flipped `Prepended` → `InBand` (otherwise two version bytes exist, one
signed-invisible and one on the wire, free to drift); the transmitted byte **is**
`SigningDomain::AuthBeacon.version()` rather than a mirrored literal, which is unfailable by
construction and strictly better than either sibling's shape, because this crate can depend on
`openpulse-core` where QSY's text format could not.

Also directed and done: record the minimal-token-over-full-re-encode choice as a **decision** rather
than an omission, and do not lose the SAR 4-byte sub-header line from #1206's closing note.

Third options considered: **retiring the crate** — the only real alternative, incomplete against its
own design, but contradicting the roadmap's ✅ and a product call above this issue's scope; named as
considered-and-declined. A **feature flag** does nothing for a library with zero consumers.
