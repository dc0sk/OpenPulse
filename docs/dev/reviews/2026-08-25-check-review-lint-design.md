---
project: openpulsehf
doc: docs/dev/reviews/2026-08-25-check-review-lint-design.md
status: review
last_updated: 2026-08-25
---

# Adversarial review — `check-review.sh` design (PR #1196)

**Reviewer:** Fable · **Date:** 2026-08-25 · **Covers:** the design of the review-trailer lint,
reviewed BEFORE implementation. Requested by the maintainer.

## Prompt

Sent with the full apparatus: the problem statement, the two documented failures (#1147 reviewed
not at all; #1193 design-reviewed twice but implementation and prose unreviewed until the maintainer
asked), the proposed three-part design, `scripts/check-trailer.sh` as the precedent to copy, and a
measurement that undermined my own proposal — 145 commits in 60 days touching the candidate paths,
98 of them also touching `src`, i.e. ~1.6/day that would each demand a committed review artifact.

Six questions, each asking for falsification rather than agreement:

1. Is the diagnosis ("reviews leave no artifact, so a skipped one is undetectable") the root cause,
   or a symptom — and is there a cheaper intervention needing no gate step?
2. Attack the 98-commit blast radius. Five narrowing options offered; which survives, or is
   path-based classification the wrong axis?
3. Can the git-ancestry check (review artifact must precede implementation) work in practice, given
   that the agent often commits both together and the repo squash-merges?
4. The rubber-stamp problem: is "requiring artifact content converts forgetting into fabricating"
   really the ceiling, or is there a construct making the review itself verifiable?
5. Is a gate step the right host, given the gate does not run at merge (#1144)?
6. Anything unasked — in particular, is this proposal itself a defect archetype, a checker fitted
   to two incidents the way a constant gets fitted to two fixtures?

## Verdict

**Build a narrowed version. Part 3 should not be built at all.**

**Numbers re-derived independently.** 145 confirmed; 98 came back as 99 (the reviewer's pattern
included `apps/tools/pki-tooling`); 25 review docs with none for #1147 or #1193 confirmed; gate.sh
not running at merge confirmed against `ci.yml` lines 53/113/154/179/226. It also corrected a date
I had asserted ("two days ago" for the #1194 trailer failure — it was the same day).

**The diagnosis is half right.** "Reviews leave no artifact" is the *audit* problem. The *skipping*
problem is that nothing interrupts the run at the decision point, which my own memory already says
("my failure mode is SEQUENCING"). Post-hoc detection hours later — or never, given the gate's
scoping — does not fix skipping. What demonstrably fixes it in this repo: a blocking PR-level lint.
The #1194 trailer failure was corrected in ten minutes because it arrived while the PR was open.

**The blast radius decomposes.** 128 of the 145 commits touch `CLAUDE.md`, which this repo's
conventions cause to be edited in nearly every substantive change — it is a *conclusions ledger*,
not a decision site. Removing it leaves `docs/dev/design/**` at 33 commits/60d. The classifier
looked unworkable because it was misclassifying.

**The right denominator is PRs, not commits:** 585 PRs in 60 days, ~90% squash-merged. Per-commit
enforcement lints messages that are discarded. A trailer burden lands on ~10 PRs/day and must cost
one line; an artifact burden lands on ~1/day.

**Part 3 (git ancestry) fails on both sides.** False pass: ancestry proves commit order, not event
order — an artifact written after the fact can simply be committed first. False fail: an honest
review committed alongside the code fails. And ~90% squash-merging destroys the evidence anyway.
This is my own Q4 argument correctly turned back on my own Q3.

**The wire-format path list would re-commit #1193's defect** — a hand-enumerated list inside the
checker written because a hand-enumerated list rotted four times. Use structural classifiers only.

**Host: `traceability.yml`, not `gate.sh`.** It runs on every PR including the `edited` event,
needs no compile, already fetches full history and the base SHA, and its stated reason for existing
protects it from cost-motivated scoping-down. gate.sh as a secondary local check is fine, never
sufficient alone.

**Recommended shape — two tiers.** Tier 1: every PR body carries `Review: <artifact>` or
`Review: none — <reason>`, hard fail if absent, making omission an explicit greppable claim. Tier 2:
PRs touching a decision site must name a real artifact passing a structure check; `Review: none`
fails there.

**Escalated to the maintainer before shipping:** the checker enforces less than the rule as written.
Either the rule is meant literally (~2 artifacts/day is its intended cost) or it should be narrowed
— otherwise the checker is what gets disabled in the collision. **Maintainer chose the narrow
checker with the rule kept as aspiration**, matching how `check-trailer.sh` relates to traceability
today.

## Applied

All of it. Part 3 dropped; `CLAUDE.md` removed from the classifier; classification is structural
(design docs, NEW `src` modules, `requirements.yaml`) with no file list; host is `traceability.yml`
with gate.sh secondary; two tiers as recommended; `--self-test` covers three rejections and two
positive controls. Validated against history: PR #1194 (the incident) classifies design-class,
#1195 (docs-only) ordinary, and 2 of the last 40 merges are design-class.
