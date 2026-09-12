---
project: openpulsehf
doc: docs/dev/reviews/2026-08-30-1158-traceability-default.md
status: review
last_updated: 2026-08-30
---

# Review — #1158: the `traceability` default the checker never implemented

Reviewer: Fable (adversarial). Date: 2026-08-30. Subject: a design decision reviewed **before**
implementation, per the standing rule.

## Prompt

`requirements.yaml`'s `meta.note` promises "New requirements and new code default to enforced".
`scripts/lib/trace.py` decides enforcement by `entry.get("traceability") == "enforced"` at two
sites, so an entry with **no** field is warn-only — the opposite, for exactly the entries the note
is about. Live consequence: `REQ-RX-02`/`REQ-RX-03` (PR #1157) were added without the field on the
strength of the note, so `MISSING-BINDING` and `CITED-BUT-DIDN'T-RUN` never fired and a binding
layout that could not survive enforcement passed silently — a `// VERIFIES:` on an `#[ignore]`d
test, and one in production code where the scanner's next-`fn` heuristic binds it to whatever
follows. Both were caught in review, not by the checker.

Census measured at HEAD and submitted with the prompt: 155 requirements / 9 enforced / **0** absent;
77 capabilities / 1 enforced / **1** absent (`CAP-76`).

Two options were put up for falsification — **(A)** absent field defaults to `enforced`, **(B)**
absent field is a hard error ("say which you mean") — with six questions: which is right and whether
(A) actually closes the self-consistent-checker gap or merely relocates it; whether capabilities
should share the default given they never reach the binding checks; what breaks shipping (A) today;
what would make the self-test vacuous (noting a sabotage earlier the same day had come back
*inconclusive rather than confirming*); whether a "defaulted to enforced" notice is a third option;
and what was wrong or unproven in the framing, census included.

## Verdict

**The choice as posed was falsified: neither option implements the sentence in the note.**

**(A)'s stated virtue is false.** The note is about *new* entries; (A) delivers *absent-field*
entries. Those sets differ exactly where it matters — a new entry that **carries**
`traceability: baseline`. That is the *likely* genesis, not an edge case: **221 of 232 entries say
`baseline`**, and yaml entries get written by copying a neighbour. (A) fixes the omission genesis
and leaves the copy-paste genesis broken. It also keeps a second silent downgrade: a typo
(`enforcd`, `Enforced`) still routes to warn-only unless the vocabulary is validated — at which
point most of (B) has been built anyway.

**(B) strictly dominates (A)** — it is the construct-ban shape this repo prefers over an
exhortation, its message explains itself, and vocabulary validation closes absent *and* typo in one
check. But (B) alone still cannot tell a new entry from a grandfathered one, because the author
"says which they mean" by copying `baseline`.

**What actually implements the sentence:** "new" is a **membership** property, not a spelling, and
the repo already owns the pattern in `trace-orphan-baseline.txt`. Freeze the grandfathered id set
once; require the field and validate its vocabulary; allow `baseline` only for a frozen id; the list
only shrinks. Then "new entries are enforced" is mechanically true regardless of what was typed or
copied. Without it, (B) still leaves the note unimplemented and the note should be rewritten to stop
promising a default nothing enforces.

**Capabilities share the rule.** The semantic is identical ("drift in this entry fails the build")
and capability enforcement is *cheaper* — no binding requirement — so there is no argument for a
laxer default. **`CAP-76` is not a live latent failure**, verified by simulation rather than by
reading: set `enforced` in a temporary copy, the checker produced zero new findings. Set it
explicitly.

**The self-test's vacuity trap is live today.** An rc-only assertion ("plant, require exit != 0")
passes *without the fix*, because the checker is already red for an unrelated reason: running
`check` during a gate reads that gate's partial log and reports a false `CITED-BUT-DIDN'T-RUN`. So
assertions must be at **message level** — the planted id must appear in FAIL with the expected check
name. The sharpest discriminator for the absent-field case is a requirement with no field *and* no
binding: the current code emits **nothing at all** for it (`MISSING-BINDING` has no warn arm), so
nothing-vs-line is unambiguous and independent of every other failure in the tree. The existing
self-test plants `traceability: enforced` explicitly, which proves `flag()` routes the literal — not
that any default works; its green must not be read as coverage. A fail-only test is also
insufficient for (B): pair it with a known-pass control, or it cannot distinguish "errors on absent
field" from "errors on everything".

**The notice option is rejected** — unread stdout on a passing build, redundant on a failing one.
The exhortation shape.

**Three adjacent defects found while reading**, none of which were asked about:

1. **`do_import` is destructive and its docstring lies.** *(Corrected 2026-08-30 by the #1223
   review, which measured it: the enforcement reset is 7 errors not "all nine", and is LOUD since
   #1222, not silent; the REQ deletions are also loud via `DANGLING-BINDING`. The genuinely silent
   damage is that import also rewrites `trace-orphan-baseline.txt`, laundering the deleted
   capabilities' code into the orphan allowlist. See
   `docs/dev/reviews/2026-08-30-1223-delete-import.md`. The command was deleted, not guarded.)* `REQ-RX-02/03`, `CAP-76`, `CAP-77` appear
   **zero** times in both import sources, so a re-run deletes them and silently resets all nine
   enforced requirements to `baseline`. "One-shot migration; safe to re-run" was true at migration
   and is false now.
2. **`_passed_tests()` takes the newest `target/gate-*.log` unconditionally**, complete or not —
   the false-FAIL mode above, which every entry promoted to enforced inherits.
3. **Enforcement is decided at two sites** (lines 231 and 255). One shared helper, or it is the
   fixed-two-of-five-arms trap.

**Framing audit.** The census was confirmed exactly (the issue body's 154/8 is stale). But
"prospective except CAP-76" *understates* the case — CAP-76 is not even a live failure — while the
claim that (A) "matches the note exactly" *overstates* it, and the framing omitted the copy-paste
hole, the typo hole, the two-site decision, the destructive import, and the blank Trace column
`do_render` would print for entries the checker enforces.

## What shipped

(B) plus the frozen list, `CAP-76` set `enforced`, one `is_enforced()` helper, and a self-test
asserting check names with a positive control. Findings 1 and 2 are filed as separate issues rather
than bundled into a checker-semantics change.

**One thing the review did not predict, recorded because it validates its own advice.** The first
draft of the self-test deleted its backup inside the per-probe restore, so every probe after the
first restored nothing and the plants accumulated in the working tree — while all three probes still
printed `ok`. The positive control the review insisted on is what caught it.
