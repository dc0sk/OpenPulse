---
project: openpulsehf
doc: docs/dev/reviews/artifacts/1147-doc-status.md
status: review
last_updated: 2026-09-12
---

# Review — the binary-handshake design's status, and the sweep behind it (#1147)

Two adversarial rounds on 2026-09-12: the conclusion first, then the prose. Both were sent asking for
falsification, with the apparatus rather than a summary.

## Consumer

Who reads this doc in production, by `file:line` —
`grep -rn "handshake-binary-encoding" --include='*.rs' --include='*.md' --include='*.yaml' .`

- **`crates/openpulse-core/tests/handshake_kat.rs:198`** — a *failing test message* points the reader
  at this doc to re-derive the PQ airtime claim if the frame ever shrinks to four fragments or fewer.
  That makes the doc a live pointer target, which is why the PQ paragraph had to stay correct rather
  than merely be marked historical — and why re-pointing it at spec §4 is recorded as a follow-up
  now that the doc declares itself not-updated.
- **`docs/dev/project/traceability.md`** — the #1147 ledger entry and this change's own entry.

Nothing else in the tree references it. No code path depends on it.

## Prior art

The sweep for an existing mechanism, with its hits —
`grep -rln "^status: approved-plan" docs/`, `sed -n '25p' scripts/lib/docfront.py`,
`git log --oneline --grep '#1193'`.

- The mechanism already exists: `docs/.frontmatter-baseline.txt` is a **grandfathering ratchet**
  (71 offenses), so the check fails only on NEW offenders. Nothing new needed to be built here — the
  fix was to clear the one new entry, not to add a rule.
- The precedent for the sweep is **#1193** (`f5a43514`), which corrected the "no domain-separation
  tags" claim in four docs at once. Its own miss — the `handshake_wire.rs` module header — is fixed
  in this change, which is the argument for sweeping code comments and docs in the same pass.
- **#1350** established the shape used here: fix the mechanical frontmatter offenders in bulk, split
  out the one that lands on a decision site.

## Twins

Sibling paths sharing the shape — `grep -rln "^status: approved-plan" docs/`

- **`docs/dev/design/file-transfer-plan.md`** and **`docs/dev/design/js8-discovery-rendezvous-plan.md`**
  carry the same illegal free-text `approved-plan (…)` status. Both are **grandfathered in the
  baseline**, and — flagged explicitly in round 1 — **must not be flipped to `resolved` by analogy**:
  unlike this doc they describe work that is genuinely unfinished (FF-16 Phase F and FF-15 Phase H
  are both deferred on-air work), so `resolved` would be the same false claim in the other direction.
  Left alone deliberately.
- The other twin is the **class**, not a file: a doc truthful about capability and stale about
  status. That is the shape of this whole change, and the reason the sweep covered six documents
  rather than one.

## Verdict

- **Round 1 → APPROVED WITH CHANGES.** `resolved` is the correct value, but only together with body
  corrections; a status flip alone would have put a machine-read `resolved` on a body still
  specifying `0x02`, `STATION_ID` 12, `session_id` ≤24 B, `dst_station` on the CONACK and 241/244 B.
  Two of my stated findings were falsified (the version byte was met then superseded, and #1191 is
  the issue rather than the ruling), and the doc sweep the design prescribed for itself was found
  unfinished across four living docs plus two self-contradictions in the wire spec.
- **Round 2 → CONCLUSIONS HOLD, TEXT NOT SHIPPABLE AS WRITTEN.** Six blocking errors, listed below:
  three files swept one section short, and four sentences contradicted by the tree — two of them in
  the outcome-note text the reviewer had supplied in round 1. All six are fixed in this change.
- **Net:** every rule change and every correction below is applied; the follow-ups at the end are
  recorded rather than silently dropped.

## Prompt

Both rounds were sent to Fable as falsification requests, with the artefacts rather than a summary of
them, and both closed with "flag anything wrong or unproven in my framing, including premises I did
not notice I was asserting".

**Round 1 (the conclusion).** Given: the illegal/false frontmatter line, the measured evidence that
#1147 shipped (`gh issue view 1147`; `114f8e5d`; `handshake_wire.rs` exists with the magics, caps and
`signed_prefix`; `verify_conreq`/`verify_conack` take frame bytes), the two deviations I had found,
and my proposal (`status: resolved`, `last_updated: 2026-09-12`, a short outcome note, nothing else
changed). Asked specifically: (a) is `resolved` the right value against how `docfront.py` and the
sibling `resolved` docs use the word, or is `archive`/`living` more honest; (b) is "implemented as
designed apart from those two deviations" TRUE — check the design's 8 gate rows and its doc-sweep list
against the tree, since a gate row whose test does not exist would mean my note laundered an
incomplete change into a closed one; (c) is my deviation-1 framing right, or was `0x02` met and later
superseded by a ruling I should cite instead; (d) does the BODY contain other claims shipping
falsified, including how many `file:line` anchors still resolve; (e) anything wrong in the framing.

**Round 2 (the prose).** Sent the diff itself — `git -C <worktree> diff main` — as a write-up review,
on the standing rule that the text which will be committed goes to review even when the conclusions
behind it are already cleared. Asked specifically about: the numbers I put into prose and which of
them are my arithmetic rather than an assertion in the tree (the 21-fragment figure, and whether 251
is even the right divisor for a PQ frame); whether the blank I left for the PQ CONACK is better than
a derived estimate; whether the NEW security sentence I wrote into `architecture.md` is true for
every field of both frames and belongs where a deleted-field claim used to be; whether the book
rewrite mischaracterises the test it cites or softens the honest-status paragraph; whether narrating
version history belongs in a SPEC; whether my `handshake_wire.rs` header over-claims what #1193's
registry provides; and whether the ledger entry claims any result I did not run.

## Round 1 — is `resolved` honest?

**Proposal sent:** set `status: resolved`, add a short "what shipped" note, change nothing else.
**Verdict: `resolved` is the right vocabulary value, but the proposal as written commits the failure
it names** — a machine-read `resolved` on a body still specifying `0x02`, `STATION_ID` 12,
`session_id` ≤24 B, `dst_station` on the CONACK and 241/244 B budgets. Approved only together with
body corrections; the reviewer supplied the outcome-note text.

**Two factual corrections to my framing**, both of which I had stated as findings:

- I said the version byte "stayed `0x01` rather than the designed `0x02`". False. **#1189 shipped
  `0x02` as designed**; `eb662cdd` (PR #1204) **reset** it. Met and then superseded by a documented
  decision is a different claim from unmet, and the note must cite the ruling rather than describe a
  gap.
- I attributed that ruling to "#1191". #1191 is the **issue**; the ruling lives in **PR #1204**, the
  `handshake_wire.rs` comment and the ledger entry of 2026-08-26.

**What it checked that I had not:** all 8 gate rows of the design have real tests (the KAT row and
the "unaddressed CONREQ is not answered" row were the two most likely to have been quietly dropped —
both exist, and the PQ determinism caveat is resolved affirmatively by
`pq_signing_is_deterministic_in_this_build`). The **doc sweep** the design prescribed was *not*
finished: four living docs plus two self-contradictions inside the wire spec, with three stale code
comments as side findings. So a status flip alone would have laundered an unfinished change into a
closed one.

## Round 2 — the write-up

Sent the actual diff. **Verdict: conclusions hold; the sweep stops one section short in three of the
files it touched, and four sentences state facts the tree contradicts.** The failure mode was named
precisely: *partial sweep presented as complete* — not a hedge hardening into a claim.

Blocking, all fixed here:

1. `protocol-wire-spec.md`'s container diagram still said `version 0x02`, eleven lines above the
   paragraph I had just rewritten to reconcile the version story.
2. The book's **§2B.3.1**, its primary handshake section, was never swept: v1 diagram (`JSON body`,
   `4 B BE u32`), "any version other than `0x02`", 241/244 B, and a CONACK described as differing
   only by `session_id`/`conreq_hash` (also `dst_station`, since #1204).
3. "#1189 closes #1147/#1166/#1178" — `gh pr view 1189 --json closingIssuesReferences` returns
   **1147, 1166**; #1189 lands the #1178 addressing, and that issue closed 2026-09-01.
4. "already false at merge: `3c710d7c` landed three hours earlier" compared the wrong pair — the
   design merged at `59e6ff5a` 17:17 against `3c710d7c` 14:40 (2 h 37 min), while the sentence read
   as being about #1189, 33 hours later.
5. My claim that the ≈6 700 B figure "was the pre-#1147 JSON encoding" — false: the measured JSON was
   ≈18 000 B, and 6 700 ≈ 5 012 × 4/3 is an estimate that matched neither encoding.
6. My README claim that Zstd is "selected locally" — unproven: it has **no** production selector at
   all, only the testmatrix/linksim/testbench harnesses.

Items 3 and 4 are in the outcome-note text the reviewer itself supplied, and it said so.

**Also caught:** `architecture.md`'s compression section still opened "compression is negotiated
during the HPX handshake" two lines above the bullet I had edited, and my replacement bullet — a true
statement about the signing span — was the wrong sentence in a compression list, so the bullet is
deleted instead; the book's §2B.7.2 claim that the PQ path "adds a check the classical path lacks",
false since #1147 added `UnofferedSigningMode`; `references.md` (finished on 08-23, re-staled on
08-26); `traceability-matrix.md` CAP-01; a second daemon comment I had missed; and the
`handshake_wire.rs` header, where my rewrite **kept the argument #1193 refuted** and over-claimed the
clippy wall — `ml_dsa_sign` sits outside it under a scoped `#[allow]`, so on the PQ path the registry
binds only the optional Ed25519 co-signature.

**Corrected in the arithmetic:** 21 SAR fragments is right and 251 is the right divisor
(`SAR_MAX_FRAGMENT_DATA = 255 − 4`, a SAR constant, not a classical-frame one) — but **no production
path transmits a PQ frame at all**, so "21 acquisitions" is a projection and the book now says
"would be". The CONACK's blank was replaced by 4 970 B derived from the encoder layout, explicitly
labelled derived rather than measured.

**Left as follow-ups, filed as #1353 rather than silently dropped:** `the_pq_vector_is_not_evidence_that_pq_is_deployable`
points the reader at this design doc, which the outcome note now declares not-updated, so the test's
pointer should move to spec §4; a length-only KAT would pin the PQ CONACK; and
`handshake_kat.rs` carries a pre-existing "5060 B ≈ 2.7 min" beside `PQ_CONREQ_KAT_LEN = 5049`.
