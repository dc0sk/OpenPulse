# Review — #1345 repair (docs re-homed by an edit)

These are write-up reviews of the repair on branch `fix/1345-doc-merge-repair`: its ledger entry,
commit message, PR body, and a comment for #1345. I wrote this record during the reviews,
2026-09-11, from each verdict as it arrived.

The repair's scope also draws on the design review of the #1345 check, which found shapes the first
census could not see. That design review is recorded with the check's own PR, not here.

## Consumer

None in production. The change is to doc comments only, and every attribute binds to the same item
before and after. Readers are rustdoc and IDE hovers.

## Prior art

No clippy or rustdoc lint covers doc attachment. `clippy::empty_line_after_doc_comments` covers only
the blank-line variant (checked with `clippy-driver -W help`, clippy 0.1.98). The repair follows the
#1345 issue text, which was reviewed before filing (`1345-doc-merge-issue.md`).

## Twins

- **Doc re-homing** takes three shapes: insertion under a doc, an insertion that overwrites a
  summary, and a deletion that leaves a doc behind. That applies at item level and at struct-field
  level. All three are repaired here.
- **Attribute re-homing** is the fourth twin. It is reported on #1345, not repaired, because moving
  an attribute changes compilation.

## Prompt

Round 1 went to a fresh reviewer. This is condensed; the full text is in the session transcript.

- **Texts.** Falsify the four texts, verbatim as they would be published.
- **Every judgement call.** For each move, check that it lands above the item it describes. For each
  deletion, check that nothing true and unsaid was lost. Also check the BURST merge, the
  `transmit_with_fec` rejoin, the `transmit_handshake_frame` append, the restored
  `apply_command_to_engine` summary and the radio `//!`.
- **Doc-only.** Check that the diff is doc-only, mechanically.
- **Counts and provenance.** Re-derive every count and every provenance SHA.
- **wasm32.** Test the wasm32 claims.
- **The census.** Check whether the census's 2 remaining hits are really non-merges.
- **Trailers.** Check whether `Verification-objective:` and `Review:` are right.
- **The overall pass.** Look for overclaims and hedges that hardened into claims, check whether a
  secondary point had been promoted to the headline, and check for closing keywords.

Return SHIP AS-IS, SHIP WITH FIXES or REWORK, with exact corrected sentences.

## Verdict

### Round 1: SHIP WITH FIXES

The diff is what it says. The reviewer checked this mechanically: the non-doc line sequence is
identical in all 8 files. Every move lands on its item, and the counts 29 / 2 / 23 (4+5+2+1+10+1) /
33 were re-derived. Must-fix items:

- **M1.** "Fixes the 29" overclaimed by one. `set_tx_attenuation_db` is one of the 29 and is not
  changed: 28 are repaired.
- **M2.** `set_tx_attenuation_db` was **never** a merge, not merely "not a defect today". `19b5a986`
  rewrote a doc block in place. That is a detector false-positive class #1345 did not list, and it
  makes the issue's "5 of 5 true" and "the only FP is `//!`" wrong by one.
- **M3.** "15 aarch64 hits" was not reproducible. The fix is to cite the command and its count. I
  re-ran it: 3 wasm32 files, 10 aarch64 files and 34 lines, 5 `--target aarch64` invocations. The reviewer said six, counting with `CLAUDE.md` in scope; for
  `scripts .github docs` the count is 5.
- **M4.** "Plausibly does not compile on wasm32" understated a certainty. `apply_command_to_engine`
  takes `&mut RuntimeControlState`, and that type is cfg-gated at `lib.rs:200`. What stays
  unverified is whether the crate compiled before `3a571769`.
- **M5.** Both review artifacts must be committed. `check-review.sh` runs `artifact_ok` even for an
  ordinary PR: the file must be at least 1200 bytes and carry Prompt, Verdict, Consumer, Prior art
  and Twins.
- **M6.** "NOT RUN" cannot merge. The gate's verdict line must be in place first.

Nice-to-haves, all taken:

- keep `drain_filexfer_tx`'s two still-true sentences;
- scope the `Verification-objective:` trailer;
- fold the double "Floor for…" summary;
- say that the 29 is this branch's census, not the issue's;
- drop the unverifiable "one guard caught a range" line.

### Round 2: SHIP WITH FIXES

This round reviewed the delta: part 2 of the repair, and the rewritten texts. The part-2 edits were
all correct against history, the diff was still doc-only, and the round-1 record above was accurate.
Must-fix items:

- **M1.** The census parser drops a record. Both census scripts accepted only single-quoted doc
  lines, so the one record that `repr` double-quoted (`54552936`, `server.rs:894`) was skipped. That
  makes 34 pairs, not 33, and 30 live on `9341f110`, not 29. This PR repairs 29 of them.
- **M2.** "Three instances the census could not see" named none of them and misattributed them. One
  came from the design review; two were found during the repair.
- **M3.** `differential_decode`'s summary was *deleted* in `391bf87f`, not overwritten. The category
  is "summary line lost to an edit".
- **M4.** This file's sibling, `1345-doc-merge-issue.md`, said "verbatim" but had one changed
  sentence and a dropped footer.

Nice-to-haves, all taken:

- label `create_pq_conack` as "kept, reworded" everywhere;
- note that the check's design has been reviewed;
- restore the census-provenance sentence to the commit.

## Record

Before applying any finding, I re-checked M3, M4 and the `c03817dd` summary deletion against
`9341f110` myself, and all held. For M3, I use my own count of `--target aarch64` invocations (5).
The design review's additional shapes were verified the same way before they were added.

Round 2's M1 was re-derived before any text changed. Widening the parser's quote class gives 34 unique
pairs, 30 live on `9341f110` and 2 on the branch. The shipped parser still gives 33 and 29 on the
same base, which is the control. Every fix was applied as specified.

One of them did not survive: `tone_reservation`. Its doc has had a leading blank line since it was
introduced in `391bf87f`, so it is not the overwritten-summary shape, and I excluded it.
