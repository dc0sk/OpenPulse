---
project: openpulsehf
doc: docs/dev/reviews/2026-08-30-1224-gate-log-evidence.md
status: review
last_updated: 2026-08-30
---

# Review — #1224: the checker read a half-written gate log as evidence

Reviewer: Fable (adversarial). Date: 2026-08-30. Subject: a design decision reviewed **before**
implementation, after I had already falsified my own first proposal.

## Prompt

`_passed_tests()` supplies the evidence for `CITED-BUT-DIDN'T-RUN` — the check that an enforced
requirement's cited test actually ran and passed. It took the newest `target/gate-*.log`
**unconditionally, complete or not**, so a manual check during an in-flight gate read a truncated
log and reported two false failures. Since #1222 made new requirements enforced by default, the
population inheriting that false-FAIL grows from ~10 to everything added from now on.

The prompt submitted my **own first fix as probably wrong** and asked for that to be checked
against the control flow: a *gate-end* sentinel cannot work, because the trace step runs before the
gate finishes and the `GATE:` line is stdout-only. The replacement — mark **test-step completion**
in `run_step`, independent of exit status — was submitted with five questions: is `run_step` the
right seam; what is the transition cost and can the resulting NOTE be misread as "fine"; is
matching a marker string the same class of fragility it replaces (with three alternatives to argue
against); what makes the sabotage sufficient, given that one of mine came back *inconclusive rather
than confirming* the day before and another corrupted the tree while printing `ok`; and what was
wrong or unproven in the framing.

## Verdict

**The falsification of proposal 1 is correct** — verified against the control flow: `gate.sh:100`
(test) → `:104` (trace, with `GATE_LOG` set) → `:155–169` (verdict). A gate-end marker would be
absent exactly when the in-gate check reads the log, degrading the check to the NOTE path inside
the gate, which is the one context where it can turn the gate red. The proposal would have disabled
the check in the same stroke as fixing it.

**The replacement is workable with five amendments, all adopted:**

1. **Match positionally, not by the step's display name.** `=== end cargo test (workspace)` is a
   constant hand-transcribed across bash and python, which cannot be shared by reference — a step
   rename would silently return `None` forever behind a NOTE that can never fail. The shipped
   matcher requires an end-marker *after the last* `test result:` line: "the step that produced the
   test output finished", without naming it.
2. **Hard-error, do not NOTE, when `GATE_LOG` is set and the marker is absent.** Inside the gate the
   test step has finished by construction, so a missing marker means the *mechanism* broke
   (reordering, format drift). `EvidenceChannelBroken` → exit 2, red gate, on the first run after
   any drift.
3. **Do not discard good evidence.** As proposed, a manual check during an in-flight gate returned
   `None` while the previous completed gate's log sat right there — turning the observed false FAIL
   into an unnecessary NOTE rather than a correct answer. The shipped code prefers the log named by
   `target/gate-verdict.json` (written only at the end of a full gate, so complete by construction,
   and excluding `--quick` and self-test logs), then falls back to the newest *complete* log.
4. **Four probes, not two**, with three named traps: cut the truncation fixture *before* the cited
   test's `ok` line or it passes for the wrong reason; `_passed_tests()` keys on the **bare fn
   name**, so a same-named test in another binary vouches for the deleted one; and a hand-written
   fixture marker re-creates the transcribed constant, so format authenticity must come from
   `gate.sh` itself. The shipped self-test takes the marker format out of `gate.sh`'s source and
   requires this module's matcher to accept it — a two-anchor check that fails if either side
   drifts.
5. **Fold in two further live defects** found while reading, both the same evidence-channel class:
   - a completed `--quick` log has **no** `test result:` lines, so `_passed_tests()` returned an
     **empty set, not `None`**, and every enforced binding reported `CITED-BUT-DIDN'T-RUN` — the
     same false FAIL with **no race required**. Confirmed by measurement: a quick-shaped log
     returned `set of 0` while a complete log returned `set of 2389`;
   - `self_test()` writes `gate-selftest-*.log`, which matches the `gate-*.log` glob. That is a full
     run of a **deliberately sabotaged tree**. Two such logs were sitting in `target/`, one of them
     truncated by a self-test I interrupted.

**Alternatives argued down.** Trusting only `GATE_LOG` and never globbing is strictly worse: it does
not fix truncation reached through an explicitly-set `GATE_LOG`, and it makes every manual check
evidence-free. An mtime/liveness heuristic is wrong on the case that matters most — a **killed**
gate leaves a truncated log with a quiescent mtime and no process holding it, which mtime blesses
after a delay and the marker correctly never does.

**Transition cost, corrected.** Smaller than I assumed and asymmetric: **zero** in-gate (the first
post-change gate writes markers into its own log before the trace step reads it) and **zero** in
per-PR CI (which has no `target/` and is already permanently on the NOTE path — `traceability.yml`
says so). Only the manual glob path has a gap, until the next full gate. The one real risk there was
that the old NOTE text — "no gate run found" — becomes a **lie** while eleven logs sit in `target/`;
the shipped text distinguishes "no gate log" from "logs found, none with a completed test step".

**Out of scope, stated so it is not re-filed.** Completeness is not currency: a complete-but-old log
still reports `CITED-BUT-DIDN'T-RUN` for a test added since that gate ran. That is literally correct
("did not pass in the last real run") and feels identical to #1224. Likewise, under
`--no-fail-fast` a compile failure means some binaries never ran, so a *completed* run can have
structurally absent evidence — extra noise on an already-red gate, never a false verdict on a green
one.

## What shipped

The `run_step` marker (exit-status-independent), positional matching, `EvidenceChannelBroken` under
`GATE_LOG`, verdict-indirection then newest-complete for the glob path, a specific NOTE, and a
five-probe `evidence-self-test` wired into `scripts/trace.sh --self-test`. Both sabotages were
watched failing: forcing the completeness check to `True` reproduces the exact defect
("a truncated log yielded `{'t'}` instead of raising"), and drifting `gate.sh`'s marker format trips
the two-anchor probe.
