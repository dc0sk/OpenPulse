---
project: openpulsehf
doc: docs/dev/reviews/artifacts/1345-doc-merge-issue.md
status: review
last_updated: 2026-09-12
---

# Review — #1345 issue text (doc-comment merge census)

Written while the review happened (2026-09-11) and posted as the first comment on #1345. It is
committed here with the repair PR, unchanged except for this sentence and the dropped footer.

## Consumer

None in production: this is an issue text and a proposed check. The check's future consumer is the
pre-push hook and/or `scripts/gate.sh`, which is the design question the issue leaves open.

## Prior art

`clippy::empty_line_after_doc_comments` (warn, `clippy::suspicious`) covers the blank-line variant.
Checked with `clippy-driver -W help` (clippy 0.1.98), with `needless-return` as the positive
control. No lint was found that covers the contiguous form. That was searched in the lint list,
not proven absent. The measured statement is `GATE: PASS` on `9e2af985` with 29 instances present.

## Twins

The two forms: doc-merges-into-doc, and a bare item stealing a doc. There are also three hunk
shapes: plain, slid, and attribute-interposed. All of them are listed in the issue as cases the
check must refuse.

## Prompt

Sent to the lesson-batch-4 reviewer (the same agent that built `doc_steal_check.py`). It is
condensed here; the full text is in the session transcript.

- Write-up review of `issue-doc-merge.md`, verbatim as it would be posted. Try to falsify it rather
  than confirm it.
- Re-count every number against the census files. I had already caught myself restating "5 in
  daemon lib.rs" from eyeballing, when the count is 4.
- Test these:
  1. whether `gate.sh` really runs `clippy -D warnings --all-targets`;
  2. the four hand-checked examples, on `origin/main`;
  3. the E5 / `afc_mini_settle` account — its owner and its direction;
  4. "a whole-tree scan is not an option", and "years ago";
  5. any overclaim, including the title's "nothing in the gate sees it".
- Return: FILE AS-IS / FILE WITH FIXES / DO NOT FILE, with must-fixes as exact corrected sentences.

## Verdict

**FILE WITH FIXES.** The finding is larger than drafted, the E5 example was mis-described, and two
of the proposed repairs would break the gate.

- **M1** — "27 live" was an undercount. Exact-text adjacency misses reworded neighbours and
  interposed attributes: `daemon/src/lib.rs:2327`, `engine.rs:350` and `engine.rs:6911` are live.
  Three pairs are genuinely repaired. **≥30 of 33.**
- **M2** — The E5 paragraph opens a triple block, `engine.rs:6891-6916`, made of the docs of
  `afc_mini_settle`, `preamble_rho` and `build_preamble_veto`. It heads `build_preamble_veto`, not
  the "correlation-search helper". It was orphaned by `d13a5df7` (#1049).
- **M3** — "or add the missing separation" would fail `clippy::empty_line_after_doc_comments`
  under `-D warnings`.
- **M4** — "years ago" was wrong: the first commit is 2026-04-23. "A whole-tree scan is not an
  option" was half earned. A textual scan cannot discriminate (211 heuristic candidates), but the
  history replay IS the census.
- **M5** — `panic_message` carries ALL of `spawn_repeater`'s doc, not its tail.

Confirmed:

- the counts for the exact-match census;
- the gate claim (`gate.sh:191`, 2538/0, the `20260911T134213Z` stamp);
- examples 2–4 (`receiver.rs:190`, `runtime.rs:134`, `radio/src/lib.rs:1`), with the note that the
  radio line's `///` predates `band_levels`.

Nice-to-haves: the prior-art point above; name the toolchain the verdict records, `rustc 1.98.0`,
not the pin; call out the bare-item form; scope the false-positive question to what was measured.

## Record

Before filing, I re-checked M1, M2, M3 and M5, the radio note, and the `d13a5df7` provenance
against `origin/main` (`25cb7b4a`) myself; all held. Every must-fix was applied. Filed as #1345.
