---
project: openpulsehf
doc: docs/dev/reviews/artifacts/1345-rehomed-docs-check.md
status: review
last_updated: 2026-09-12
---

# Review — the re-homed docs lint (#1345)

This is the design review of `scripts/check-rehomed-docs.sh` and `scripts/lib/rehomed_docs.py`.
The design document is `docs/dev/design/rehomed-docs-check.md`. I wrote this record while the
reviews ran, 2026-09-11, from each verdict as it came back. The write-up review of this PR is
appended below when it returns.

## Consumer

- **`scripts/gate.sh`:** a `run_step` in the full-mode lint block, after the review-trailer lint.
- **`.github/workflows/traceability.yml`:** the step "Re-homed docs lint (branch diff)", run on every
  PR against `origin/$BASE_REF`.
- **`.cargo-husky/hooks/pre-push`:** it runs against the upstream (else `origin/main`) and blocks
  the push on a hit.

These locations were found with `grep -n 'check-.*\.sh' scripts/gate.sh`,
`grep -rn 'check-.*\.sh' .github` and a read of the hook's range computation (`:37-42`).

## Prior art

- **No toolchain lint covers doc attachment.** `clippy::empty_line_after_doc_comments` flags only
  the blank-line variant (clippy 0.1.98). This was checked with `clippy-driver -W help`: 24 doc lints,
  only that one about placement. `rustdoc -W help` lists 11 lints, none about attachment.
- **The wrapper shape follows `check-trailer.sh`** (#1219): it fails closed on `^{commit}` and has a
  `--self-test`. It deliberately does *not* copy `gate.sh`'s `check-review.sh --base "$(… || echo
  HEAD)"`: that fallback diffs HEAD against itself and passes vacuously. That twin stays, reported
  in the PR, not fixed here.
- **The detector is a port of the design reviewer's v4 probe** (`doc_steal_v4.py`), with revision 3
  applied.

## Twins

- **Doc re-homing** comes in three shapes: insertion under a doc or attribute, deletion leaving a doc
  behind, and an overwritten summary. It happens at item level and at struct-field and enum-variant
  level. All of these are covered.
- **Attribute re-homing** is the same act on `#[...]`, and it is covered as ATTR.
- **Not covered, stated in the script's docstring and the design:**
  - a steal whose stolen doc is entirely rewritten in the same hunk;
  - a re-homing created during a below-threshold rename;
  - macro-generated items;
  - `#[doc = …]` and `/** */` docs (there are none in this tree).

## Prompt

Two design-review rounds went to the same reviewer. This is condensed; the full text is in the
session transcript.

- **Round 1 (revision 1).** Try to falsify the pure-insertion design. Test:
  - the placement (gate, PR workflow, hook);
  - the twins list, including deletion and an attribute-only steal;
  - evasions: slides, merge commits, renames, CRLF, a blank first line, two items in one hunk;
  - the fixture list;
  - every UNCHECKED field.

  Negative claims need their command.
- **Round 2 (revision 2).** Falsify the new parts: the field and variant extension (a twin round 1's
  replay could not see); the overwritten-summary rule; the C4 wording; the fixture table; the counts.

## Verdict

### Round 1: IMPLEMENT WITH CHANGES

- **C1.** Judge the new file, not the hunk. The pure-insertion rule is fooled by git anchoring a hunk
  on a blank `///` line; five real false-positive commits were measured.
- **C2.** Flag an insertion under a bare attribute. `ClientWriter` took `handle_client`'s cfg.
- **C3.** Add the deletion mirror. `dda5bbe8` is a live instance.
- **C4.** No `|| echo HEAD` base fallback, because it passes vacuously. Fail on an unresolvable base,
  and diff from the merge-base.
- **C5.** Corrected fixtures. A new blank line is a FAIL; add F1, F2 and F3 and P1–P4; drop the
  undiscriminating "slid" FAIL fixture.
- **Also:** the overwritten-summary twin (four live, three after my own check), and yes to the
  pre-push hook.

### Round 2: IMPLEMENT WITH CHANGES

- **M1.** Slide normalisation is required. `acdb1a0d` slid a new variant under the previous one's
  `#[arg]`, and fields make slides likelier.
- **M2.** The overwritten-summary rule needs a non-empty remainder, *and* the lost head must appear
  nowhere else in the new file. On history that cuts 47 candidates to 3, all defects. As first
  worded, the rule would have failed the repair PR.
- **M3.** A `#[…]` also sits above struct-literal fields (10 instances, all `gpu` cfgs), and they are
  in scope. The field pattern excludes paths.
- **M4.** The counts come to 37 live instances in total: 28 + 2 + 3 + 4.
- **M5.** Add P9 (slid), P10 (whole-doc delete), P11 (a doc moved back to its owner), F8 (a field
  under the previous field's doc) and F9 (an attribute above a literal field).

## Record

Both rounds' changes are applied in revision 3, and the implementation follows it.

**Self-test.** `scripts/check-rehomed-docs.sh --self-test` passes. It runs 23 fixtures in a scratch repo: F1–F11 must flag, and P1–P7 and P9–P12 must not. P8 checks base handling in the wrapper: a
40-hex non-object and a missing ref must both fail, and `HEAD` must lint.

**P9 is cut from the real `acdb1a0d` region**, not synthesised. A synthetic version did not make git
slide, so it proved nothing. The self-test now checks that P9 discriminates: with normalisation off,
the same diff flags ATTR.

**Controls replayed on real commits:**

- **Must flag, and each does:**
  - `d13a5df7` (the E5 steal);
  - `114f8e5d` (both PQ param structs);
  - `6139037b` (the `ClientWriter` attribute and `load_control_psk`);
  - `eb662cdd` (the three `dst_station` field docs);
  - `dda5bbe8` (scfdma);
  - `c03817dd`, `19b5a986` and `391bf87f` (the three lost summaries).

  `19b5a986` also flags `radio/src/lib.rs`, whose `///` crate doc now documents `band_levels`. That
  is a real steal, not the `set_tx_attenuation_db` false positive, which the walk-up correctly
  passes over.
- **Must not flag, and none does:** `acdb1a0d` (the slide) and `8a63d9bd`, `206e9433`, `e5f99430`
  and `995836f4` (blank-`///` anchoring).

**Full first-parent replay:** 1201 commits, 44 hits in 39 commits (INS 32, MOD 1, ATTR 4, DEL 4, OVR 3). Reconciled record by record:
- 33 are the census's real insertion pairs: all 34 except the `set_tx_attenuation_db` false positive. `7eb36cae` is reported at its interposed cfg.
- 3 are insertion shapes the census could not see: `PqConAckParams` (MOD), `ClientWriter` (ATTR) and `e83a69e8`'s `#[default]` (ATTR).
- The 4 DEL and 3 OVR are the design review's list.
- 1 is `5e80f296`'s `use`, found by the second change below.

The first replay also found one false positive, `6790d298`, which the tightened walk-up removes. Nothing else changed between the two replays.

**Dedup.** Hits are deduplicated by stolen line. An inserted struct's fields all walk up to the same
doc, so without this `6139037b` reported 17 records for 2 steals.

**Second change after implementation: `use`, `extern crate` and macro invocations count as item
starts.** Stopping the walk at an added code line (above) creates a false-negative class: a steal
whose first inserted line is code the item pattern does not match. Measured over the same 1201
commits, that class has exactly one instance, and it is real. `5e80f296` inserted
`use openpulse_core::handshake::{…}` directly under a pre-existing `#[cfg(not(target_arch =
"wasm32"))]`, taking that cfg off the `use openpulse_core::trust::{…}` below it. Counting those
lines as starts adds that one hit and no others: 44 instead of 43. Fixtures F10 (the `5e80f296`
shape) and F11 (a macro invocation) pin it. A doc above a macro invocation needs no rule of ours,
because rustc's `unused_doc_comments` already catches it.

**Changed after implementation; REVIEWED 2026-09-12, and the change did not survive — see
*Third review* below.** The full first-parent replay found one false
positive, `6790d298` (`qsy/bandplan.rs`). A rewritten `match` block's tail `Ok(warnings)` walked up
over added *code* lines to an `#[allow(deprecated)]` that still belongs to the block's first arm.
The walk-up now stops at an added code line. P12 reproduces the shape: it flags ATTR under the old
rule and passes under the new one. This deviates from revision 3's text, so it is sent to review
with that evidence.

**One replay hit was outside the census, and it is a true positive:** `e83a69e8`. It inserted
`Info, Config` under the panel `Tab` enum's `#[default]`, which moved the default tab. `94b405ca`
moved it back to `Messages` the same day. That answers #1345's open question: no intended re-homing
was found in the replayed history.

**Sabotage.** On this branch, a function was planted between a doc and its item
(`band_levels.rs:9`). Both wiring invocations exited 1, and the report named the stolen doc, the
thief and the owner (`freq_hz_to_band`). A reset restored the branch, and the lint passed again.

## Third review (2026-09-12) — the post-implementation changes

Verdict: **SHIP, BUT NOT THIS REVISION.** Sent with the two post-design deviations, the replay as
evidence, and the dogfood result. The reviewer's apparatus was an instrumented copy of the lint with
switches for each rule, a replay driver, and a recall harness; it reproduced the 23-fixture self-test
unchanged and showed the switches live before using them.

**The finding that mattered — my walk-up was unsound as a rule.** Stopping at any added line that is
not doc, attribute or blank makes the lint blind to what rustc's *lexer* discards. Proved with
`rustc -D missing_docs` that a `///` still attaches to the item below across a `//` comment and
across a wrapped `#[cfg_attr(…)]`, then measured the shipped rule at **0/150 recall** on each shape,
planted at 150 real sites. Both shapes are live here: 11 `//` lines sit directly under a `///` at
HEAD, and 16 outer attributes are wrapped. **The replay was silent because history contains no such
steal — that is a fact about the corpus, not about the rule**, which is the "corpus of one regime"
trap this project has paid for before (#1053).

**Four required changes, all applied** (walk the lexer's discards; match `macro_rules!` outside the
`\b` alternation; suppress DEL only on a move that took a *suffix* of its own block; require OVR to
have lost a paragraph or been overwritten). Two of my proposed fixes were **falsified with
measurements** before I could build them:

- the DEL fix I proposed on #1345 ("the deleted start does not reappear in the new file") suppresses
  **three real hits** — `dst_station` exists twice in `eb662cdd`'s new file, and
  `combine_llrs_weighted` returned as a `pub use`;
- "the remainder still reads like a summary" as the OVR test **misses 1 of 3** true positives
  (`19b5a986`).

My "possibly not decidable from the diff text alone" for OVR was called what it was — a premature
elimination — and a rule separating all six known instances was given instead. A suppression
annotation was rejected outright: a lint with a bypass is a lint people bypass.

**It also caught a defect in my implementation of its own verdict.** My first cut of the OVR
overwrite test was off by one in the old-file line arithmetic and silently dropped `19b5a986`, a true
positive. The A/B replay is what surfaced it — the self-test and the dogfood were both green.

**Write-up corrections required and applied:** the ledger entry said "Test results: NOT RUN at time
of writing", which is a blocker under this repo's own traceability rule; the design doc claimed the
post-implementation change's review "is recorded in" this artifact while this artifact said "pending
review" (the same sentence, opposite claims — the hedge-hardening shape); the commit message's first
bullet omitted the `use`/`extern crate`/macro starts it later relies on; and "of the census's 30 live
instances, the kind breakdown across the replay was {…}" conflated two populations, since the 44 span
all history including steals repaired before `9341f110`.

**Recommended and adopted:** commit the recall harness rather than leave it in a scratchpad. It is
`rehomed_docs.py --recall N`, wired into `check-rehomed-docs.sh --self-test`, and it carries a
control that requires the pre-review walk-up to miss shapes B and D — otherwise a pass proves
nothing about the rule.

**Noted, not fixed:** `owner_of` returns the first old-file line equal to the stolen text, so a
duplicated doc line can name the wrong owner (`/// true to enable, false to disable.` recurs in
`cli.rs`); and after a rebase the hook's `merge-base(@{u}, HEAD)` falls back to the old fork point,
so the lint covers main's intervening commits.
