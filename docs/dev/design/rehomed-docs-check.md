# Design — a diff-time check against re-homed docs and attributes (#1345)

**Status:** revision 3, implemented in `scripts/lib/rehomed_docs.py` and `scripts/check-rehomed-docs.sh`. Design review has run twice:
- revision 1 → IMPLEMENT WITH CHANGES (C1–C5);
- revision 2 → IMPLEMENT WITH CHANGES (M1–M5). Revision 2 added field-level deletion, and
  round 2 confirmed that addition on the three #1191 instances.

Both rounds' changes are applied here, and nothing from review is open.

The repair of the live instances ships separately, on branch `fix/1345-doc-merge-repair`, and does
not depend on this design.

## Problem

Rustdoc attaches every consecutive outer doc line, and an outer attribute, to the next item. A diff
can therefore move a doc or an attribute onto the wrong item without touching either one. Four acts
do it:

1. **Insertion under a doc or an attribute.** A new item, or its doc, is inserted directly below the
   last `///` line of another item's doc, or directly below another item's `#[…]`.
2. **Insertion over a summary.** The new item is anchored ON the owner's first doc line, which the
   diff rewrites. The owner keeps a doc that begins mid-thought.
3. **Deletion of an item, a struct field or an enum variant, leaving its doc or attribute behind.**
   What was left attaches to whatever follows.
4. **A rewrite that anchors a hunk on a coincidental blank `///` context line.** This is a
   false-positive source for any hunk-shape rule. It is not a defect.

**Measured on `9341f110`** (the union of two liveness methods, then read by hand): 28 of case 1 live,
plus two attribute-only instances (`qsy_wire_magic`, `ClientWriter`), three of case 2
(`apply_command_to_engine`, `broadcast`, `differential_decode`) and four of case 3 (three #1191 fields
and `demodulate_with_params`). Together that is 37.

**What the gate sees today:** the gate passes with all of these present. The only lint touching doc
placement is `clippy::empty_line_after_doc_comments`, which catches a *blank-line* separation.

**Why a whole-tree scan fails:** it cannot discriminate. A heuristic returned 211 candidates on
`25cb7b4a`, mostly ordinary multi-paragraph docs.

## Detection rule — judge the new file, not the hunk (C1)

The input is `git diff -U0 -M "$BASE"...HEAD -- '*.rs'` together with the old and new versions of
each touched file.

- **Insertion (cases 1 and 4).** For every added line that starts an item, a struct field, an enum
  variant, a `use`, an `extern crate` or a macro invocation, walk upward through lines that are added doc, attribute or blank lines, or a *blank* `///`
  context line; an added **code** line stops the walk. Look at the first line that is neither.
  (Changed after implementation. The review of this change is recorded in
  `docs/dev/reviews/artifacts/1345-rehomed-docs-check.md`.)
  - **Flag it** when it is a non-blank outer `///`, or an outer `#[…]` that existed before the
    change.
  - **Skip it** when the same hunk also removed an item start above it, because that is a rewrite of
    the same item.

  This one rule covers pure insertions, slid hunks, attribute-interposed insertions and modifying
  hunks, and it reports the item, not the hunk.
- **Bare attribute (C2).** The rule above flags an insertion directly under a pre-existing outer
  attribute, whether or not a doc sits above it. Live examples: `qsy_wire_magic` took
  `apply_command_to_engine`'s cfg in `3a571769`, and `ClientWriter` took `handle_client`'s in
  `6139037b`.
- **Deletion (C3, case 3).** Flag a hunk that meets all three conditions:
  - its removed lines contain an item start, a struct field or an enum variant;
  - its preceding new-file line is a non-blank `///` or an outer `#[`;
  - its following new-file line is a `///`, an `#[`, or an item, field or variant start.

  The item-level instance is `dda5bbe8`. The field-level instances are `eb662cdd` ×3.
- **Overwritten summary (case 2).** Flag an item the diff did not otherwise change when both hold:
  - (a) its new leading doc block is a **non-empty** proper suffix of its old one;
  - (b) the missing head lines occur **nowhere** in the new file.

  (b) excludes moves and repairs, which are the owner side of an insertion or deletion hit and are
  reported there. It is also what keeps a repair PR from failing its own check. (a) excludes deleting
  a doc outright, which is not a re-homing.

  Measured on history: 47 candidates reduce to 3 under (a)+(b), and all 3 are defects: `c03817dd`,
  `19b5a986` and `391bf87f`. `391bf87f` *removed* the summary rather than replacing it, so the rule
  must not require an added line.

A `///` directly above a field-shaped line occurs only inside a struct or enum body: 0 exceptions
among the 2005 such lines on `9341f110`. An outer `#[…]` also occurs above struct-*literal* fields in
expression position: 10 instances, all `#[cfg(feature = "gpu")]`. Both are in scope, because moving a
`#[cfg]` onto a neighbouring literal field is the same defect. The field pattern is
`^\s*(pub(\(…\))?\s+)?[a-z_]\w*\s*:(?!:)`, where `(?!:)` excludes paths.

**Slide normalisation is required, not defence in depth.** Before classifying, rotate a pure-insertion
hunk upward while the line above it equals the hunk's last inserted line, then compute the added-line
set. `acdb1a0d` slid a new enum variant under the previous variant's `#[arg]` and produced a false
hit at `cli.rs:446`. Struct and enum bodies are exactly where neighbouring members share trailing
lines, so covering fields makes slides more likely, not less.

**Reporting.** Each hit prints the stolen line and the item that now carries it. It also prints the
**owner**: the item that lost the doc, found by walking down from the stolen line in the old file.
With both in hand, the repair is mechanical.

## Wiring

- **`scripts/lib/rehomed_docs.py`** is the detector. **`scripts/check-rehomed-docs.sh`** wraps it,
  in the shape of `check-trailer.sh` and `check-review.sh`. It prints `REHOMED-DOCS: PASS|FAIL` and
  exits non-zero on a hit.
- **Base resolution (C4).** The wrapper resolves `git merge-base HEAD origin/main` itself and
  **fails with a reason when it cannot**. There is no fallback ref: `git diff HEAD HEAD` is empty,
  so a fallback to `HEAD` would pass vacuously. It diffs with three dots from the merge-base, never
  with two dots against a branch tip. The self-test requires that an unresolvable base FAILS and a
  resolvable one still lints, following the pattern at `check-trailer.sh:123-135`.
- **Where it runs:**
  - **`scripts/gate.sh`**, as a `run_step` next to the other lints. These sit inside the
    `MODE = full` block.
  - **`.github/workflows/traceability.yml`**, on every PR. That job uses `fetch-depth: 0`, so the
    merge-base resolves.
  - **The pre-push hook.** It is the only thing that runs on every push (#1144), it already computes
    the upstream range, and the check takes under a second.

## Self-test fixtures

The fixtures are planted from one table that the covered-forms list above shares, so a form added to
one is added to both.

**Each must FAIL:**
- F1: a deleted item between two docs.
- F1b: a deleted struct field between two field docs, which is the #1191 shape.
- F2: a reflowed doc plus an inserted item.
- F3: a bare attribute with no doc.
- F4: a plain insertion under a doc.
- F5: an insertion under a doc with an attribute between them.
- F6: an insertion whose first line is a *new* blank line under a doc.
- F7: an overwritten summary.

**Each must PASS:**
- P1: a doc deleted together with its item.
- P2: a signature rewrite under an unchanged doc. This is the key false-positive control for C1.
- P3: an attribute line added on its own.
- P4: a paragraph appended to a doc.
- P5: the last item deleted together with its doc.
- P6: an insertion after an *existing* blank line.
- P7: an insertion after a `//!` inner doc.

**Base handling:**
- P8: an unresolvable base. The check must exit non-zero with a reason, never pass vacuously.

**Added after implementation:**
- F10 (FAIL): a `use` inserted directly under a pre-existing `#[cfg]`, taking it, from `5e80f296`.
- F11 (FAIL): a macro invocation inserted directly under a pre-existing `#[cfg]`.
- P12 (PASS): a block rewritten under an expression attribute, whose tail expression is
  start-shaped, from `6790d298`. Under the walk-over-any-added-line rule it flags ATTR; under the
  tightened rule it does not.

**Added in revision 3:**
- P9 (PASS): a slid insertion, where the new variant's leading lines duplicate the previous variant's
  trailing lines. The fixture must assert that the raw `-U0` hunk start is not the true insertion
  point, or it proves nothing.
- P10 (PASS): a whole doc deleted from an unchanged item. The overwritten-summary rule must not fire.
- P11 (PASS): a doc moved back to its owner, which is the repair shape. The overwritten-summary rule
  must not fire; this is the fixture that stops the check failing a repair PR.
- F8 (FAIL): a field inserted directly under the previous field's doc.
- F9 (FAIL): an item inserted under a `#[cfg]` that sits above a struct-literal field.
- F7 is built with a **non-empty** remainder: only the first paragraph is replaced, by an inserted
  item's doc. Built with an empty remainder, it tests P10's shape instead.
- F1b fires on all three #1191 sites and stays silent on the `ConAckParams` deletion in the same
  commit, which removed the doc together with its field.

**The pre-push hook's base is `@{u}`.** A branch with nothing new yields an empty diff and a PASS.
That is correct, but the wrapper prints the range it linted, so an empty range is visible.

## Consumer

`scripts/gate.sh` (the lint steps at `:214`, `:219` and `:224`), `.github/workflows/traceability.yml`
(`:73`, `:88` and `:106`), and `.cargo-husky/hooks/pre-push` (the range at `:37-42`).

## Prior art

- **Lints.** No clippy or rustdoc lint covers attachment. This was checked with
  `clippy-driver -W help` (24 `doc` lints, of which only `empty_line_after_doc_comments` concerns
  placement) and `rustdoc -W help` (11 lints, none about attachment), both at 1.98.0.
- **Doc forms.** There are no `#[doc = …]` attributes and no `/** */` block docs in the tree.
- **Diff-lint pattern.** `check-trailer.sh` and `check-review.sh` both carry `--self-test`. The
  review found `check-review.sh`'s `|| echo HEAD` fallback defect, and C4 avoids it.

## Twins not covered

- **A re-homing created during a below-threshold rename.** It appears as a whole-file add, so the
  walk-up reaches line 0. Accepted.
- **An introduce-then-repair inside one branch.** The three-dot diff shows only the net change.
  Accepted, since the net is what lands.
- **Items generated by macros.** Out of scope.

## Changed after implementation

**The walk-up stops at an added code line.** Revision 3 walked over *any* added line. The full
first-parent replay found one false positive under that rule. In `6790d298` (`qsy/bandplan.rs`), a
42-line hunk rewrote a `match` block, and its tail expression `Ok(warnings)` matches the variant
pattern. The walk crossed every added code line up to an `#[allow(deprecated)]` context line, which
still belongs to the block's first arm.

A doc attaches to the first item below it, never to one buried under new code, so only doc,
attribute and blank lines may sit between a stolen doc and its thief. Fixture P12 pins this: it
flags under the old rule and passes under the new one.

**Full first-parent replay:** 1201 commits, 43 hits in 38 commits (INS 32, MOD 1, ATTR 3, DEL 4, OVR 3). Reconciled record by record:
- 33 are the census's real insertion pairs: all 34 except the `set_tx_attenuation_db` false positive. `7eb36cae` is reported at its interposed cfg.
- 3 are insertion shapes the census could not see: `PqConAckParams` (MOD), `ClientWriter` (ATTR) and `e83a69e8`'s `#[default]` (ATTR).
- The 4 DEL and 3 OVR are the design review's list.

The first replay also found one false positive, `6790d298`, which the tightened walk-up removes. Nothing else changed between the two replays.

**Second change after implementation: `use`, `extern crate` and macro invocations count as item
starts.** Stopping the walk at an added code line (above) creates a false-negative class: a steal
whose first inserted line is code the item pattern does not match. Measured over the same 1201
commits, that class has exactly one instance, and it is real. `5e80f296` inserted
`use openpulse_core::handshake::{…}` directly under a pre-existing `#[cfg(not(target_arch =
"wasm32"))]`, taking that cfg off the `use openpulse_core::trust::{…}` below it. Counting those
lines as starts adds that one hit and no others: 44 instead of 43. Fixtures F10 (the `5e80f296`
shape) and F11 (a macro invocation) pin it. A doc above a macro invocation needs no rule of ours,
because rustc's `unused_doc_comments` already catches it.

**The replay also answered #1345's open question**, whether an insertion under a doc or attribute is
ever intended. The one attribute re-homing found outside the census was `e83a69e8`. It inserted
`Info, Config` under the panel `Tab` enum's `#[default]` and moved the default tab from `Messages`
to `Info`. `94b405ca` put `#[default]` back on `Messages` the same day. So it was a true positive,
the lint would have caught it, and no intended case was found in the replayed history.

## Review

- Revision 1 → IMPLEMENT WITH CHANGES (C1–C5).
- Revision 2 → IMPLEMENT WITH CHANGES (M1–M5). All of those are applied here; nothing further is open.
- The reviewer's scripts are `doc_steal_v4.py`, `replay_v4.txt` and `census_v4.py` (session
  scratchpad). They are to be committed with the check as the prior-art replay, or reproduced.
