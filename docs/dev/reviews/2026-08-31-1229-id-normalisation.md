---
project: openpulsehf
doc: docs/dev/reviews/2026-08-31-1229-id-normalisation.md
status: review
last_updated: 2026-08-31
---

# Review — #1229: seven requirements the enforced registry could not hold

Reviewer: Fable (adversarial), two rounds. Date: 2026-08-31. Reviewed **before** implementation, and
the first round overturned the issue's own direction.

## Prompt

Two rounds. **Round 1** submitted #1229's own framing — "the hand-written matrix duplicates the yaml
and has drifted" — with a column-by-column mapping, a drift census, and four candidate designs
(delete the REQ table; trim the CAP table; add a header disclaimer; make the matrix checkable),
asking which and why, whether making it checkable was a trap, what to do about 14 referring
documents, and what was wrong in the framing. **Round 2**, after the maintainer chose normalisation,
submitted the target names, the rename scope including historical artifacts, the detector design,
the detector's own blind spot, the sequencing, and the fate of `REQ-SEC-CTL-06`.

## Verdict

### Round 1 — the issue was pointed the wrong way, and my measurement is why

#1229 was filed as "the hand-written matrix duplicates the yaml and has drifted" — the matrix being
the stale one. The census behind that used `REQ-[A-Z]+-\d+`, which cannot match `REQ-SEC-CTL-01`
(two category segments) or `REQ-DCD-ADAPT` (no trailing digits) — **precisely the divergent ids, on
both sides**. It reported "152 in the matrix, strict subset, 0 divergent"; 159 − 7 = 152 exactly.

Corrected, the drift is **bidirectional and the defective side is the yaml**: seven ratified,
shipped, tested requirements existed only in the matrix. `REQ-SEC-CTL-01` is referenced in 16 files
and is `openpulse-linksec`'s charter in CLAUDE.md's crate map; `REQ-DCD-ADAPT` is in the
acceptance-criteria table, gated by `daemon_squelch_noise_floor`. `CAP-68.satisfies` was `[]` while
the matrix row held the only structured record of what it covered.

**Executing the plan as filed would have deleted the only record of the content that falsifies its
premise.** The deliverable became *complete the yaml first, then trim the matrix*.

The mechanism matters more than the number: a zero from a filter is a claim about the filter, and
here the filter's blind spot and the finding's subject were the same set. **A conventional positive
control would not have caught it** — any well-formed id passes a filter that is blind only to
odd-shaped ones. What works is diffing parsed sets from both artifacts with the odd-format ids as
the deliberate probe.

### Round 2 — the design, after the maintainer chose normalisation

Four falsifications of my framing, each adopted:

1. **The argument for `REQ-CTL-*` is mechanical, not semantic.** "Preserves the grouping" is weak —
   grouping lives in the yaml's `category` field and in `covered_by`, not in the id. The real
   discriminator is that a prefix substitution preserves every trailing number, so the eight
   `/`-shorthand sites (`REQ-SEC-CTL-01/02`) rewrite in one expression. Renumbering into
   `REQ-SEC-14…19` changes the *second* number too — the shape where a first-occurrence replace
   edits the wrong one, which happened twice in one session during #1191.
2. **The detector was not a new checker.** `DANGLING-BINDING` already fails the gate for a
   well-shaped unregistered binding; the gap was a *lexer* plus a docs/membership layer.
3. **`REQ-SEC-CTL-06` is not uncited** — `decoder_robustness.rs` names it. But that test verifies
   decoders do not panic on malformed input, which is not what an exemption states. Binding it would
   launder a different requirement through a convenient id. Recognising it as a *different*
   requirement is what makes retirement clean rather than lossy.
4. **The CAP-68 side was missing from my scope entirely** — `satisfies: []` plus five orphaned
   linksec/keystore sources would have produced `BIDIR-DRIFT` and incoherence had the REQ side gone
   enforced alone.

**Negation is the load-bearing idea, and the reason is exact.** The old pattern was a *selector*:
anything it did not match silently left the checked set. The new conformance regex is a *validator*
over a deliberately looser tokenizer: anything collected and not validated fails. Same machinery,
opposite failure direction. You cannot write a regex without a blind spot; you can choose whether it
fails loud or silent. The residual — a token containing no literal `REQ-` at all — is closed
structurally rather than lexically: ids are minted only in the yaml, and the membership layer binds
prose to it.

**Exclusions are corpus definitions, not per-item waivers.** A path class ("dated records are not
scanned") cannot grow one incident at a time and is justified by what the class *is*. The lexical
vocabulary is safe for an asymmetric reason: a missing entry is a loud false failure, fixed in one
line, never a silent pass.

**Sequencing was prescribed and followed**: new names only, never letting an old name enter the yaml
even transiently (it would be enforced, and its `MISSING-BINDING` unresolvable because the scanner
cannot tokenize an old-shape binding); yaml entries and their bindings in the same commit, since
either alone fails; and each `// VERIFIES:` immediately above its `#[test]`, because the scanner
binds to the *next* `fn` and a header comment binds to whatever comes first.

**Do not widen the scanners.** A scanner that accepts odd shapes makes nonconforming bindings
functional and removes the pressure toward conformance, creating a second laxer definition. Strict
scanners plus a loud validator plus one shared constant is the single enforcement point.

### What shipped, and what it found

Five requirements registered with run-confirmed bindings; `REQ-CTL-06` retired; `REQ-CTL-03`
blocked (its code is not compiled under the gate's `--no-default-features` — #1234). CAP-68 wired
both ways and five sources paid off the orphan baseline. A pre-existing `DANGLING-CODE` in CAP-68
(a path missing its `crates/` prefix) was fixed in passing.

The detector found **`REQ-FT-01…07`** on its first run — a superseded draft scheme in
`file-transfer-plan.md` against the registered `REQ-FX-01…06`, mapping not 1:1 (#1235). A shape
check could never have found those: they are perfectly well-formed. Only membership sees them.

Two self-referential faults surfaced while wiring the probes, both worth recording because they are
the shape this whole change is about: the checker's own file is scanned, so a literal probe id makes
the detector fail on its own fixture — **it caught my explanatory comment doing exactly that** — and
the id check was first placed before the vocabulary gate, where its early return pre-empted three
existing probes.
