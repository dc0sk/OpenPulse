---
project: openpulsehf
doc: docs/dev/reviews/2026-08-25-1193-writeup-and-implementation.md
status: review
last_updated: 2026-08-25
---

# Adversarial review — #1193 implementation and write-up (PR #1194)

**Reviewer:** Fable · **Date:** 2026-08-25 · **Covers:** the implementation and ALL prose for the
signing-domain registry — PR body, four commit messages, doc changes, ledger entry.

> **Reconstructed after the fact.** This artifact was written from the session transcript once the
> review-artifact convention was adopted, not committed at review time. That is exactly the
> provenance weakness the design review flagged; it is recorded here rather than hidden, and the
> convention exists so later artifacts are written as the review happens.

## Prompt

The full package: PR body, four commit messages, the complete doc diff (normative wire spec, the
book, the design doc, the traceability ledger, requirements), and the implementation diff. Ten
numbered attacks, several on points I was genuinely unsure of — whether the swallowed domain errors
create a silent failure, whether `RESERVED_MAGICS` being `#[cfg(test)]` is honest about what it
guarantees, whether the `verify_strict` change could reject legitimate traffic, whether the KAT
inference is sound, and whether every number in the prose survives checking against its source.

Context given: two prior reviews had covered the inventory analysis and the construction design,
both before code. Neither covered the implementation or any prose. `main` was unpushed, so
everything was still changeable.

## Verdict

**Do not push yet.** One implementation defect, one corrupted gate verdict, three false claims.

**1. The gate's clippy PASS was a corrupted verdict — the headline.** `pki-tooling` had six raw
ed25519 sign/verify sites and no allow, so the wall did not hold on the tree. **Changing
`clippy.toml` does not invalidate cargo's cached lint fingerprints:** every crate the change edited
was re-linted and genuinely clean, but the one crate it never touched replayed a stale clean
verdict. Reproduced three ways (`-p pki-tooling` → rc=101; `--workspace` → rc=0; `touch` one file
then `--workspace` → rc=101). The claim "workspace clippy rc=0" was already quoted in a commit
message, the ledger and the PR body.

Why the sabotage missed it: it used the 19 violation sites in the crate being edited as positive
control, which only ever proved the wall fires where recompilation was happening anyway.

**2. The reserved-magic list rotted a fourth time, inside the commit condemning list rot.** `OPZ1`,
a transmitted magic in the registry's own crate, was missing — found by one grep. The doc comment
also overclaimed what a hand list can guarantee.

**3. Three false claims in the prose**, each checked against its source: the issue inventoried
**7** contexts (5 table rows, hedged "at least"), not 5; the blast radius went 13 → **8**, not → 5
(stated backwards); and #1147's case was **three SAR fragments**, not "752 B ≈ 23 s" — its own
ledger entry says verbatim "not seconds — fragments", so the prose attributed to it the framing it
explicitly rejected.

**4. Smaller findings.** `signature::Signer::try_sign` was an open wall bypass (byte-identical
signatures, not on the disallow list). The wall polices *trait* paths and is therefore
algorithm-agnostic, so its Ed25519-specific advice is wrong for another algorithm. The two route
signers fail closed but **silently on the sender**, presenting as "the peer never accepts my
routes" with nothing logged. The in-band `version()` values were unenforced convention. "Cannot
compile" overstated what is a lint. `REQ-SEC-13` lacked the `status:` field its siblings carry.

**5. Confirmed correct, and worth recording:** the fail-closed reasoning for swallowed errors
(empty/all-zero signatures never verify, and a placement flip fails every round-trip test at test
time); `verify_strict` acceptance is a strict subset of `verify` and rejects no legitimate peer;
the PQ hybrid's A1 `has_classical` separability is intact and the allows are scoped to exactly four
functions; the KAT inference is sound and stronger than claimed (a PQ CONREQ KAT also exists); the
route-collision arithmetic; the ledger's SUPERSEDED annotation being honest.

**6. Scope correction on the headline claim.** "There is no reachable cross-context confusion" was
categorical; only 2 of ~28 context pairs are argued. Softened to what was established — neither
author nor review could construct one.

## Applied

All of it, before push. `pki-tooling` given a scoped allow (service key, not station identity) and
the workspace re-linted under force; `OPZ1` registered and a sabotage-verified source scan added so
the list is checked rather than trusted; the three numbers corrected across the PR body, four
commit messages, the merge commit, the ledger, the book, the design doc and `CLAUDE.md`;
`try_sign` added to the wall; sender-side logging added; `version()` pinned against `WIRE_VERSION`;
wording corrected to "fails the workspace clippy gate". Re-gated: `GATE: PASS 131ae024`, 2407
tests, 0 failed, every crate force-relinted.
