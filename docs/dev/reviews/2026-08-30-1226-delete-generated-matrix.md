---
project: openpulsehf
doc: docs/dev/reviews/2026-08-30-1226-delete-generated-matrix.md
status: review
last_updated: 2026-08-30
---

# Review — #1226: a generated matrix nobody read, whose only self-claim was false

Reviewer: Fable (adversarial). Date: 2026-08-30. Design decision reviewed **before** implementation.

## Prompt

`traceability-matrix.generated.md` asserted in its own header *"CI requires this file to be in sync
(`git diff --exit-code`)"*. Nothing did. `git log -S'trace.sh render' -- .github/ scripts/gate.sh`
returns nothing — no commit ever wired it — with `trace.sh check` as the positive control that the
search works. The file had drifted from 152/75 to the yaml's 155/77 and had not been regenerated
since the commit that created it.

Inventory found the situation was not what the issue described. Two matrix files with **opposite**
consumer counts: the hand-written `traceability-matrix.md` is referenced by 14 files, while the
generated one is referenced by exactly one thing — `trace.py`'s own `RENDER_OUT` constant — with no
doc linking it at all.

Three options were submitted: **(A)** wire the enforcement the header claims, **(B)** delete the
file and `render`, **(C)** give it a reader first, then enforce. My instinct — (B) — was submitted
*for falsification*, together with: is the two-matrix situation the real defect rather than one
file's header; what makes enforcement real if (A)/(C) win; and what is wrong in the framing.

## Verdict

**(B), and the strongest counter-argument was constructed and then failed on value rather than
logic.** That counter is (C)'s premise: *"no reader" is not an observed property of the artifact —
a file nothing points at gets zero readers by construction, so 21 readerless days prove nothing.*
Logically sound. It fails because the questions a human would take to that table are already
answered better elsewhere: "what covers REQ-X?" by `grep REQ-X requirements.yaml`, and "what is
uncovered?" by `trace.sh check` **failing**. The enforcement is the consumer. A browsable view is a
convenience whose demand has never been demonstrated, bought at a permanent price — every yaml edit
would forever require regenerate-and-commit, plus merge conflicts on a generated file across
concurrent PRs.

**Scope corrected, and it is wider than deleting a file.** The false sentence is not in the `.md` —
it is emitted by `render`'s template (`trace.py:471`), so editing the file would be overwritten on
the next render. Delete the file **and** `render` **and** the template string: a kept-but-callerless
`render` is precisely the defined-but-unconsumed construct the reachability ratchet exists to ban.

**Two corrections to my evidence, both confirmed against the tree:**

1. *"The hand-written matrix is actively maintained by hand, last edited 2026-08-24"* is overstated.
   Its own frontmatter says `last_updated: 2026-07-30`, and its three most recent commits are fix-PR
   doc sweeps (a handshake defect, a notch re-derivation, a toolchain bump) — not matrix upkeep. It
   is alive as a **linked reference**, not as a maintained one. That weakens my rejection of the
   #1223 review's orphan guess, and strengthens the case that its structural tables are rotting too,
   just more slowly and less visibly.
2. **My drift figures are historical, not current.** The file was regenerated and committed in
   #1227 earlier the same day, so it reads 155/77 and is in sync at the moment of writing. Stated
   as history in the issue, or anyone opening the file concludes the issue is wrong.

## Deferred, deliberately

**The two-matrix situation is the real defect, and is not this issue.** The hand-written matrix's
REQ/CAP tables duplicate the yaml's structure in un-diffable prose — the exact rot `trace.py`'s own
header names as the reason the importer died. The generated matrix was plainly created as their
replacement, and the replacement was never carried through: no links moved, no enforcement wired.
#1226 is the abandoned half of that migration rather than an independent defect. The honest end
state is that the yaml owns structure (enforced by `check`) and the hand-written doc keeps only what
the yaml cannot hold — rationale, results, prose — deferring structural claims to the yaml. Filed
separately. If that reconciliation ever concludes "replace the tables with a generated, linked
view", that is the single condition under which `render` returns — with a reader, i.e. (C) done at
the right target. A 15-line renderer is trivially resurrected from git history.

**Recorded although moot under (B):** had (A)/(C) won, the obvious sabotage would have been
inconclusive in the now-familiar way. A yaml mutation that `check` also rejects fails the *earlier*
workflow step, so the render-sync step never executes and the probe proves nothing. The sabotage has
to be **check-invisible and render-visible** — mutate a capability's `name:`, which `check` does not
validate and `render` prints.
