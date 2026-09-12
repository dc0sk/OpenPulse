---
project: openpulsehf
doc: docs/dev/reviews/2026-09-05-1268-traceability-join.md
status: review
last_updated: 2026-09-05
---

# Review — #1268: the traceability matrix joins by membership, not by property

Reviewer: Fable. Reviewed **before implementation**. The proposal was rejected; what survives is
smaller and differently aimed.

## Prompt

Sent with the census I had measured and the three motivating rows, framed for falsification:

> `trace.py` clears `REQ-GAP` for any requirement whose `covered_by` is non-empty; it never asks
> whether the capability implements the property. Three rows were wrong this way in a month.
>
> My read is that the strongest fix — require a `VERIFIES` marker per requirement claiming coverage —
> cannot ship as a hard gate, because it would fail 120 rows on day one and this project has already
> paid for a red-on-arrival gate (#1074). So: make it a ratchet like `scripts/reachability.sh`;
> retire the "covered" vocabulary; hard-fail the 5 capabilities with empty `code` AND `tests`;
> REQ-SEC-09 becomes a gap row.
>
> Is the ratchet the right instrument or am I pattern-matching to what is nearby? Is a `VERIFIES`
> marker a property check or a second membership join one level down? Check my census. Would each of
> the three known-bad rows actually have been caught? And flag anything wrong or unproven.

## Verdict — do not build the ratchet; it already exists, and it would not have worked

**1. The ratchet exists under another name, and I did not look before proposing an imitation.**
`trace-grandfathered-ids.txt` (221 ids, shrink-only, #1158) plus `NOT-GRANDFATHERED` as a hard error
means a *new* requirement cannot be `baseline`, so it must be `enforced` — which already requires a
`VERIFIES` marker, whose test must have passed in the last real gate, in a non-dormant package
(#1237). "Fail any requirement that newly claims coverage without a binding" is **already true**. The
only uncovered path is an existing grandfathered requirement whose `covered_by` grows from `[]`
later, and closing that needs the gap list frozen as a third baseline — not a new instrument.

**2. The premise that a ratchet pays down is false here.** Measured from git, non-comment lines per
commit:

| baseline | created | now | trend |
|---|---|---|---|
| `reachability-baseline.txt` | 456 (08-09) | **511** (09-04) | grew 12 % |
| `trace-orphan-baseline.txt` | 91 (08-09) | 86 | −5 in four weeks |
| `trace-grandfathered-ids.txt` | 221 (08-30) | 221 | unchanged |

The one I wanted to imitate has grown, with exactly one shrink — #1272, the day before. A second
120-row file would hold the same ids as the existing 146-row grandfathered list under a second
lifecycle: two lists saying the same thing, both "managed". That is the laundering I asked the
reviewer to argue for, and the record supports it.

**3. It catches 0 of the 3 motivating rows.** Concretely, which is what made this decisive:

- **REQ-MAC-02 / CAP-31** — `csma_loopback.rs` holds `csma_blocks_broadcast_when_dcd_busy`, which
  calls `enable_csma()` and asserts the hold. A developer told "this needs a marker" puts it there in
  a minute: named for the requirement, passing, depends on the CSMA code (survives mutation),
  non-dormant package. The actual property — broadcast/relay *sessions* sensing before transmit under
  default config — stays untested. A wrong row with a green binding.
- **REQ-SEC-09 / CAP-47** — `relay_integration.rs:180` is literally `fn trust_policy_rejects_at_hop()`.
  Its name *is* the requirement's sentence; it tests a deny-list. That is the test a marker lands on.
- **QSY signing** — **not the same defect**, and grouping it made my fix look broader than it was.
  REQ-REG-13 is about documentation recommending IARU band-plan frequencies; REQ-REG-14 about
  wide-band segment choice. Neither mentions signing. No join was wrong because no requirement asked
  for the property — the false claim lived in CAP-45's prose *description*. What closed it was #1252
  creating REQ-SEC-14 and binding a daemon-level test to it, i.e. turning the claim into a
  requirement. The generalisable rule is on the capability side and is prose review, not a checker.

**4. A `VERIFIES` marker is a membership join one level down — and `trace.py` already says so.** Its
own docstring: *"Whether a binding is germane to its requirement remains a manual norm, unchecked by
anything here."* The marker adds a specific test *function*, a run-status check, and a
rename-breaking join key. It adds nothing about the property.

**5. Item 2 has no target.** There is no generated matrix — `render` and
`traceability-matrix.generated.md` were deleted in #1226. `traceability-matrix.md` is a hand
document, a frozen import source, with a hand-typed status column. What that document *is* was
already deferred as "the real defect" by the #1226 review, and does not belong bolted onto this issue.
The yaml has no "covered" vocabulary to retire either: it has `covered_by` and a `REQ-GAP` check.

## The census decomposes in a way my framing hid

- The 16 requirements under the five empty capabilities have **no other coverage** — each is covered
  *only* by an empty cap. And **REQ-DCD-01 is `enforced` with a passing binding while sitting under a
  capability that lists no code and no tests.** That is the finding worth keeping: the requirement
  layer and the capability layer can disagree about evidence today with nothing noticing.
- The 120 split into **88** with a cited test file at capability level but no requirement-level
  binding, and **32** where no covering capability lists any test at all.
- **~30 of the 120 are process, platform, documentation or strategic posture** where a marker is the
  wrong instrument and a ratchet can *never* retire them. Lumping them in overstates the debt by a
  quarter — and that alone decides the instrument, because a list containing 30 permanently
  unretirable rows is a classification, not a debt.

## Item 3 is right, and was fitted to the inventory I happened to count

Seven more capabilities have `code` but `tests: []` — CAP-43, 49, 58, 67, 68, 70, 71 — and CAP-70/71
cover 13 requirements with suites that exist. "No tests listed" is the same vacuity for the
requirement-side claim. So the rule must be stated as *a capability that satisfies a requirement must
list at least one test* (failing 12, all fillable today) rather than my accidental five. CAP-69 is
genuinely unimplemented; failing it forces the right outcome — delete it, REQ-BW-* become gap rows —
and that is a stated intent, not a surprise when the check goes red.

## Plan

- **PR A (data, mechanical):** REQ-SEC-09 → `covered_by: []` **and** out of `CAP-47.satisfies` in the
  same edit, or `BIDIR-DRIFT` warns instead of `REQ-GAP`. Fill CAP-72/73/74/75 from the acceptance
  table. Delete CAP-69.
- **PR B (checker):** `EMPTY-CAP` as a hard error with its boundary stated explicitly, a
  `trace.sh --self-test` probe that plants the condition and requires `EMPTY-CAP` **by name**, and a
  doc sweep of the artifacts describing what `trace check` checks (`git grep -ln
  'trace\.sh\|trace\.py\|trace check'` → 16 files, 6 frozen reviews). **Not** a `gate.sh` semantics
  change, so the 14-file `gate.sh` sweep does not apply.
- **PR C (separate decision):** the hand matrix's status column and a per-requirement `verification:
  test | inspection | analysis | demonstration` field, so a documentation requirement's evidence is a
  doc link and never reads as a missing test. That reclassifies ~30 rows out of the debt honestly and
  gives the *existing* ratchet a denominator that can move. Reopen the deferred half of #1229.

Filed separately: **#1279** — `scripts/req-mutation.sh` is the project's only tier-2 instrument (does
a bound test *depend on* the bound code?) and runs in no workflow at all.
