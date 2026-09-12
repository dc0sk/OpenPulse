---
project: openpulsehf
doc: docs/dev/reviews/2026-08-30-1151-gate-drift-guard.md
status: review
last_updated: 2026-08-30
---

# Review — #1151: the gate's verdict named a state the run never had

Reviewer: Fable (adversarial). Date: 2026-08-30. Design decision reviewed **before** implementation.

## Prompt

`gate.sh` captures `COMMIT` and `DIRTY` once, before step 1, and echoes those start-time values at
the end, while every step resolves HEAD or scans the tree at its own step time. A ~2 h run gives
both plenty of time to move. #1151 records a verdict `GATE: FAIL a7a8d88f ... clean` whose failing
step named a commit made 39 minutes in **on a different branch**. I hit the tree half twice in one
day: editing two files in the running checkout, and a subagent mutating `requirements.yaml` for
under a second.

Proposed: fingerprint HEAD and the tree at start, re-read at the end, emit `GATE: INVALID` instead
of PASS/FAIL on drift. Submitted with six questions — is INVALID the right third state; how to
fingerprint cheaply and correctly; does `--self-test` (which dirties the tree on purpose) need an
exemption; is start/end enough or must it be per-step; what makes the sabotage honest given a 2 h
iteration cost; and what is wrong in the framing.

## Verdict

**The mechanism is right and INVALID over FAIL is right, but the proposal as written does not catch
two of the three incidents that motivated it.** That is the finding.

**Scope, corrected.** An endpoint — or even per-step — fingerprint *samples*: it detects drift that
persists to a sample point. The subagent's sub-second mutation hashes identically at both
boundaries, so the guard would print a confident verdict over exactly the event that prompted it.
Only the persistent shapes are caught. The options were to scope the claim honestly or add a
filesystem watcher whose own liveness is verified — a watcher that died at minute 10 and reports
"no events" being archetype #1 inside the mechanism built to prevent it. Shipped: the honest scope,
in a comment that says plainly what is not detected, with the watcher filed as #1231. The sentence
"the gate detects mid-run mutation" must not be written anywhere; it detects *persistent* mid-run
mutation.

**Fingerprint: both of my candidates were wrong, for the same reason.** A hash of
`git status --porcelain` + `git diff` and `git stash create` are each **blind to the content of
untracked files** — status prints `?? path`, diff prints nothing — and an untracked file is the
likeliest mutation, the self-test's own fixture being one. Verified directly before building:
editing an untracked file changed neither output. Shipped instead: a temp-index `write-tree`, with
the real index copied first so stat-clean files reuse cached hashes, and **HEAD compared
separately** — a mid-run `git commit` changes no working-tree content but moves HEAD, and the
trailer and review lints read commits.

**No `--self-test` exemption is needed, because the question presupposed a structure the script does
not have:** `--self-test` exits at line 108, before any step and before the guard's capture. The
exemption would have been the hole; there is nothing to exempt. The genuine self-test defect — an
interrupted run leaving its sabotage fixture in the tree, which happened — is fixed here with a
`trap`.

**Per-step with early abort, not endpoint.** Endpoint-only spends a 2 h test step on a run already
void. Per-step also converts the full-path sabotage from miserable to cheap: mutate during the
seconds-long fmt step and the gate aborts before clippy. The check lives in its own `drift_check`
function called by each caller, **not** inside `run_step`, whose contract is running one command;
drift policy is sequencing, not step execution.

**INVALID must delete authority, not data** — the JSON and the verdict line carry `steps_result`, so
a real failure hiding under the drift is not lost. **Exit 3**, distinct from FAIL's 1 and usage's 2,
so a caller can tell "rerun, the run is void" from "fix the code".

**`gate-verdict.json` is evidence infrastructure, not a receipt.** `trace.py` prefers the log it
names as proof that cited tests ran, and did not check `result`. Measured before fixing: an INVALID
verdict naming a *complete* log **was accepted** as evidence. `trace.py` now refuses it, with a PASS
control proving the refusal is not blanket.

## What shipped, and how it was verified

Four layers, cheapest first, because a 2 h iteration would otherwise have tempted a `--quick` test
of code `--quick` does not run:

1. **The primitive**, via a new `--fingerprint` flag, in seconds: content edit changes it; `touch`
   alone does not; writes under `target/` do not; a new untracked file changes it; **editing an
   untracked file's content changes it** — the case the rejected candidates were blind to.
2. **Tree sabotage on `--quick`**, honest only because `--quick` calls the same `drift_check`:
   planted an untracked probe mid-run → `GATE: INVALID`, exit 3, aborted after fmt. The mutation was
   chosen so that *only* this guard can see it — no test, lint or ledger check reads an unknown
   untracked file — so the failure cannot be a downstream step in disguise.
3. **HEAD-only drift**: an empty commit mid-run, tree byte-identical → `INVALID ... HEAD`.
4. **The negative control**: a quiet run still reports `PARTIAL`/PASS, never INVALID — which is what
   proves no gate step self-trips the fingerprint. Without it, a guard that fires on every run would
   have looked like a working guard.

## Not fixed here

`gate.sh:136` feeds the verdict JSON through a pipeline (`grep -c ... | head -1`) in the script
whose own header bans pipelines in the verdict path. `grep -c` prints exactly one line, so the
`head -1` is inert and the existing comment explains why the obvious alternative was worse — real,
but a separate change from a drift guard.
