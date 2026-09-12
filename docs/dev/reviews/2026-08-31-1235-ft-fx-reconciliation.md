---
project: openpulsehf
doc: docs/dev/reviews/2026-08-31-1235-ft-fx-reconciliation.md
status: review
last_updated: 2026-08-31
---

# Review — #1235: reconciling the draft `REQ-FT-*` scheme, and two requirements I minted badly

Reviewer: Fable (adversarial). Date: 2026-08-31. Reviewed after the first implementation and
**before merge**; it held the commit, and the held item was the right one.

## Prompt

Submitted the full mapping (seven draft ids against six registered), both newly minted
requirements with their bindings, the `REQ-CTL-04` restatement, and the `RENAMED_IDS` change — with
instructions to verify the mapping from `requirements.yaml` rather than from my table, to test
whether `sender_happy_path_three_blocks` actually verifies "surfaced to the operator", and to
attack the mint-vs-widen reasoning. I disclosed a slip I had just found in my own rewrite (two
sites substituted `REQ-FT-05 → REQ-FX-05` where the mapping says `REQ-FX-07`) and asked it to
assume more of the same.

## Verdict

### Held the commit: I repeated the very defect the other half of the commit corrects

`REQ-FX-07`'s statement claims progress is "surfaced to the operator" at "both ends". Its binding —
`sender_happy_path_three_blocks` — asserts only that the *sender state machine* emits
`FxAction::Progress`. **Delete the daemon's forwarding at `daemon/src/filexfer.rs:538` and `:635`
and the `enforced` binding stays green.** No test anywhere observed `ControlEvent::FileProgress`.
`REQ-FX-08` had the same shape: its statement's load-bearing clause is "before any disk write", and
its binding tested `sanitize_filename` as a pure function, which survives a caller dropping the
call and survives a *sixth* write path being added unsanitised.

My evidence for both was a callers-grep — which this repo's own cross-cutting checklist bans as
proof ("never claim 'covers all paths' from a callers-grep — prove it with a test that fails
without the wiring"). The difference from the `REQ-CTL-04` case I was correcting in the same commit
is only that the wiring here *exists*: the statements were true-now, but the enforcement was the
same illusion. Registering them on grep evidence, in the commit whose other half corrects exactly
that, is the finding.

Fixed by binding both to seams that can fail, and watching each one fail:

| requirement | binding | sabotage |
|---|---|---|
| `REQ-FX-07` | `twin_daemon_bridge::a_file_crosses…` — tx- and rx-direction `FileProgress` on **both daemons' real control streams** | neutralise both `event_tx.send(FileProgress)` → `rc=101`, "sending daemon never surfaced FileProgress" |
| `REQ-FX-08` | `daemon::filexfer::tests::a_hostile_offer_name_and_peer_cannot_escape_the_download_dir` — calls `write_file`, the function that writes | `sanitize_filename(name)` → `name.to_string()` → `rc=101` |

The weak markers were removed rather than kept alongside, so the requirement rests only on a test
that can fail.

### Sustained

- **The mapping is right, checked row-by-row against the pre-commit draft and the registered
  statements.** `FT-02 → FX-02 + FX-05` and `FT-07 → FX-05` both hold; `REQ-FX-05` says "with
  resume from the last completed block" verbatim, so **#1235's own claim that resume was a gap was
  wrong**. The resulting double-load of `FX-05` (delivery + resume in one statement) is inherited,
  not introduced — it will cost checkability only when `FX-05` leaves `baseline`.
- **Mint-vs-widen was the right call, for a weaker reason than I gave.** Widening a `baseline`
  statement does put the new clause under warn-only drift. But sanitisation was never plausibly
  part of "Operator-controlled acceptance", so the real alternative was widen-*and-promote*
  `FX-04`, and the honest reason to mint is statement cleanliness.
- **`REQ-CTL-04`'s three factual claims verified independently** (no dependent crates; the daemon
  reads `OPENPULSE_CONTROL_PSK`; `server.rs:1746` already called it a follow-up).

### Also caught, all fixed

- The restated `REQ-CTL-04` named "Argon2id KDF, ChaCha20-Poly1305 AEAD" — **verified by nothing**;
  swap in scrypt+AES-GCM and every bound test passes. A milder form of the same defect. Removed
  from the statement, which now claims only what the four keystore tests hold.
- Two content mismatches the substitution left behind: `REQ-FX-08` glossed as
  "sanitization/quota" (quota is `FX-04`), and G4 attributing quota **and** per-peer directories to
  `FX-08` — the latter registered by no requirement at all, now said so in the doc rather than
  back-derived into a statement.
- **Four of the seven commands in the acceptance table did not exist** (`--test loopback_roundtrip`,
  `--test integrity_failure`, `--test filexfer_twin`, `--test filexfer_station_id`). Pre-existing,
  but refreshing the id column dated them to 2026-08-31 while leaving them unrunnable. Rewritten
  against targets that compile, with the station-ID-during-transfer row marked not implemented.
- `RENAMED_IDS`' comment claimed a scope its code does not have — it is an **anywhere** allowlist,
  not a mapping-table one. Comment corrected to match the code (self-consistent-checker family, in
  the checker itself).
- The `/NN` shorthand expander is **structurally blind to the slip class I actually committed**:
  `REQ-FX-02/05/05` expands to registered ids and passes. Recorded as a known blind spot at the
  code, not papered over.
- "**Planning only — nothing here is implemented**" sat one paragraph above a note whose premise is
  that these requirements are shipped and tested. Corrected.
- Category drift: minted ids said `File transfer`, the others `File transfer (FF-16)`.

## Not taken

The sender's terminal `progress(block_count)` (`sender.rs:93,116`) fires on completion, which races
`pair.shutdown()` closing the control stream — so "the last progress reads `n/n`" is a
load-dependent verdict of the kind #1066 exists to remove. Left unasserted, with the reason at the
code.
