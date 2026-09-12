---
project: openpulsehf
doc: docs/dev/reviews/2026-08-25-reachability-comment-stripping.md
status: review
last_updated: 2026-08-25
---

# Adversarial review — reachability comment/string stripping (#1192)

**Reviewer:** Fable · **Date:** 2026-08-25 · **Covers:** the lexer design and the measurement,
reviewed BEFORE the fix was written into `reachability.py`.

## Prompt

Sent with the apparatus: the issue, the exact defect in `scripts/lib/reachability.py`
(`IDENT.findall` over text with only `#[cfg(test)]` stripped, so comments and string literals are
indexed — and `PUB_ITEM` matching the same text, so a `pub fn` inside a comment could count as a
declaration), a throwaway Rust-aware stripper, a measurement harness importing the real module, and
the numbers below.

Six questions, several on points I was not confident about:

1. Attack the lexer. I listed constructs I had NOT handled — nested raw strings containing `*/`,
   `//` inside a string, doc fences with quotes, `#[doc = "..."]`, macro bodies with unbalanced
   braces, `'\''`, `b'x'`, a raw string whose closing delimiter appears inside it — and asked which
   actually occur here and whether the stripper mishandles them. **A stripper bug manufactures
   FALSE orphans, which is worse than the inflation it fixes.**
2. Is the 54 right, and is any of it a stripper artifact rather than a genuine prose-only reference?
3. What should happen to the 54 — baseline them, work them down, or split?
4. Should an intra-doc link count as a reference? (I leaned no; asked for the counter-argument.)
5. Is the fix worth it, or does it just make one number honest while the ratchet behaves the same?
6. Anything unasked.

## Verdict

**Proceed — but the probe missed a third defect, and the obvious integration order is a measured
regression without it.**

**The lexer survives**, verified three ways: an invariant scan over all 315 production files. **The reviewer's scan also checked delimiter
balance; the COMMITTED check does not** — it tests only for leftover `"`, `//` and `/*`, which is
weaker, and this artifact previously implied otherwise; pub-item multiset identical before and after; and direct adversarial constructs including
`b'"'` (which really exists here, at `signing_domain.rs:353`), `r##"…"#…"##`, raw identifiers
`r#type`, `#[doc = "..."]`, labels `'outer:`, and EOF truncations.

**One real flaw:** the escaped-char branch searched from `i + 2`, so `'\''` matches its own escaped
quote and leaves a stray `'` that can blank up to three characters of real code. Zero occurrences
in this repo; fixed anyway (search from `i + 3` — the escape consumes two characters, so the
closing quote can never be at `i + 2`).

**My "0 items were declared inside a comment" claim was true but my instrument could not have shown
it.** `gone` only surfaces a comment-declared item that was *also* an orphan. The check that
actually proves it is pub-multiset equality — which the review ran, and which is now asserted in
the committed self-check.

**The missed defect.** `_strip_cfg_test` assumed `#[cfg(test)]` precedes a brace block. Two
counterexamples exist here: `#[cfg(test)] pub(crate) use …::quadrature;`
(`scfdma/demodulate.rs:27`) and `#[cfg(test)] mod robust_ack;` (`mfsk16/lib.rs:434`). Today the
brace matcher grabs a brace inside a doc-comment formula and survives only because that maths
happens to balance. **Strip comments first and it stops balancing: the matcher swallows
`mix_to_nominal` entirely, public items 2387 → 2386.** So the fix must also cut at the first `;`
when it precedes the first `{`. Orphan sets are otherwise identical between orders, so the `;` rule
costs nothing.

**The 54 reproduced exactly** (2387/468 → 2387/522, gone 0) with no false orphan attributable to a
stripper bug — but it is **three populations**, not one, and deleting the first would break the
build. Triaged in #1197: (i) ~14 live via a same-file string dispatcher; (ii) English-word
collisions that were never references; (iii) genuine pub-but-internal APIs.

**Intra-doc links should not count** — and the counter-argument's premise is false here: nothing
runs `cargo doc`, there is no rustdoc gate and no `deny(rustdoc::broken_intra_doc_links)`, so a
dangling link fails nothing and is not a maintained relationship. Counting them would also create a
laundering path cheaper than `pub(crate)`: one doc line silences the ratchet.

**Worth it, for three behavioural changes**, not just a tidier number: it closes a false-PASS
evasion class (before this, an item with a common English name could *never* fail the ratchet); it
kills the false-FAIL class that trains people to distrust the gate; and it makes the
"production-reachable" figure mean what it says. Cost ~0.8 s per pass.

**Also required:** the fix belongs in `reachability.py`, not a parallel harness (the repo's
own rule 5); commit the self-check as something that can fail, plus a sabotage check that a run
with the stripper disabled *changes* the count, since a refactor silently dropping it is otherwise
invisible.

## Applied

All of it. `i + 3` fix; `_strip_cfg_test` made `;`-aware and the pipeline reordered (public items
verified still 2387); self-check committed with both halves — lexer behaviour and a wiring test
that fails if the stripper is unwired — and sabotage-verified in two directions; baseline
regenerated with provenance; #1197 filed with the three-way triage.

One further defect found while applying: **regenerating the baseline destroyed every annotation in
it**, including a `DORMANT(#1118)` block recording why three items are deliberately unreferenced.
`write_baseline` now preserves comment lines. Precisely: 5 annotation lines are preserved and the 3 header lines are re-emitted — 'verified 8/8 kept' overstated a straight preservation of all eight. A later review then showed the first fix STILL destroyed annotations silently (it filtered by substring, so a note reading 'grandfathered pending #9999' vanished); it now matches the emitted header by full-line equality and prints anything dropped. Sabotage-verified with exactly that annotation.


## Second review — the implementation and prose (same day)

The design review above ran BEFORE implementation. A separate review of the *implementation and its
write-up* then found six things that had to change before push, which is the #1193 lesson repeating:
the two reviews fail independently.

- **A LibreOffice lock file was in the commit** (`.~lock.…odp#`) — unrelated contamination.
- **`Refactors: CAP-77` was wrong**: CAP-77 is #1193's signing-domain registry, which this change
  does not touch. The lint passed because it validates that an ID *exists*. Dropped — this commit
  touches no production source, so `Verification-objective:` alone is correct.
- **The `DORMANT(#1118)` annotation shipped SCRAMBLED.** It had been reordered by a locale sort
  during #1147, and `write_baseline` faithfully preserved the fragments in scrambled order, so the
  narrative "regenerating destroyed annotations, now fixed" shipped with its flagship annotation
  unreadable. Restored verbatim from `94889c87`; "the getters below" was also positionally false
  once entries are sorted, and now reads "listed in this file".
- **Nothing ran `self-check`.** `reachability.sh` accepted only `report|check|baseline|--self-test`,
  so the committed self-check was defined-but-not-consumed — the archetype, in the commit that
  headlines it. Now dispatched, and `--self-test` runs it first.
- **Two counts were wrong**: the baseline has 19 comment lines, not 17; and "all 8 kept" conflated
  5 preserved annotations with 3 re-emitted headers.
- **Issue #1197 still quoted the retracted 305/75** while the commit message narrated the
  retraction — "a retraction only in the ledger is not a retraction", inside the very change that
  says so. Edited.

It also found the `[u8; N]` edge in `_strip_cfg_test` is **live, not hypothetical** (two
`#[cfg(test)] const` declarations in `signing_domain.rs` hit it), and that its leak is in the
false-PASS direction; the comment now says so rather than presenting it as theoretical.
