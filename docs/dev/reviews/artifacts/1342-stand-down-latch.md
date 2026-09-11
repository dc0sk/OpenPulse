# #1342 — record the correlation veto's stand-down on the daemon path

**Status:** reviewed 2026-09-11; the verdict (below) is applied in the change carrying this artifact.
Line numbers are at `a7412113`, the base this was reviewed against; on `25cb7b4a` (which the change
branches from) `engine.rs` lines between ~415 and ~6890 — every line cited in the sections I wrote
— sit 12 lines further down; the verdict's `:7513` and `:8241` shift by more. The gate below now drives
the announce latch true on both paths in the default run.

My instinct is **one private helper that both call sites use for the whole
decide-and-record step**, plus an in-crate gate that primes the calibration. Test that instinct rather
than confirming it, and flag anything wrong or unproven in the framing.

## Record

- **#1342**: 0 comments; the body, which I wrote on 2026-09-11, read in full. **The body is wrong in
  two places** — its third consequence and the word "instead" — and its correction is part of this
  review (below).
- Read in full as context, because they define and create the call site this changes: **#1157**
  (PR body and its 1 comment — the origin of REQ-RX-03's stand-down), **#1118** (body and 5 comments —
  the issue whose fix, #1168, created the daemon-path veto), and **#1062** (body and 18 comments,
  earlier today).

## The defect, restated correctly

The veto's decide-and-record step exists twice.

- **CLI** (`receive_with_timeout_fec_inner`, `engine.rs:4219-4290`): `push_at`; `effective_threshold`;
  `stands_down`; on a transition, set `rho_stand_down` and emit a `warn!` ("preamble veto STANDING
  DOWN …" / "re-engaged", `:4226-4242`). Then: stand-down → `rho_stand_down_settles += 1` (`:4245`)
  and fall through; reject → `rho_rejected_settles += 1`, reset the correction, `continue` (`:4255`);
  corroborated or stood-down → `rho_accepted_settles += 1` (`:4286`).
- **Daemon / OTA** (`acquire_at_onset`, `:2492-2506`): `push_at`; `effective_threshold`;
  `stands_down`; `accepted = stand_down || rho >= threshold`; `rho_accepted_settles` or
  `rho_rejected_settles`.

**Both paths count a stand-down as an accept.** What the daemon path lacks is the engine's *announce* latch `rho_stand_down`, its `warn!`,
and `rho_stand_down_settles` — the counter that tells a stand-down apart from agreement. #1157
specified stand-down as "latched with hysteresis, logged"; on the daemon path it is hysteretic but not
reported or logged (see below). #1168
wired the accept/reject counters to this call site after its own gate caught the veto "deciding
invisibly"; the stand-down latch was not part of that fix.

**Precisely (review MUST-FIX 1):** on the daemon path the *decision* IS hysteretic — that latch lives in
`RhoCalibration::standing_down` (`rho_calibration.rs:177-181`) and `acquire_at_onset` drives it
(`engine.rs:2494-2496`). What the daemon path lacks is the engine's *announce* latch `rho_stand_down`,
its `warn!`, and `rho_stand_down_settles`, which are written only at the CLI site (`:4227`,
`:4229-4241`, `:4245`). So the stand-down is neither reported nor logged there — the same half-wiring, half repaired, not a
design decision.

## Consumer

- **Operators**, through the two `warn!` lines in the daemon's tracing log. That is the only
  operator-visible signal of a stand-down today, and only the CLI path emits it.
- **`rho_stand_down()`** (`engine.rs:1150`), read only by tests (`tests/rho_calibration_receive.rs:86`,
  `:95`). Its intended production consumer, per #1157 and #1118, is the daemon's control plane — and
  `docs/dev/project/reachability-baseline.txt:63` still carries the `DORMANT(#1118)` note ("Wire them into the control plane when #1118 lands, and delete these three lines"), although #1118 closed on 2026-08-19. None of the three `rho_*` getters
  has a production reader (my filter, run against `main` excluding `tests/`, found none; the same
  pattern finds the test readers). **Out of scope here** — a control-plane API change — and recorded
  so it is not lost.
- The seam gate `tests/daemon_runs_acquisition_chain.rs`, which reads the accept/reject counters.

## Prior art

- The CLI site's latch block — the semantics to reproduce, verbatim, including its log text.
- `tests/daemon_runs_acquisition_chain.rs` — the fixture the gate reuses: a BPSK250+`Rs` frame shifted
  200 Hz with the shipped `CfoChannel`, embedded in 8000 samples of silence each side, driven through
  `accumulate_capture` in `DaemonConfig::default().receive_tick_ms` chunks, work-bounded
  (`set_deterministic_scan_positions` / `_max_iterations`). At 200 Hz phase 1 cannot decode, so phase 2
  runs by construction.
- `tests/rho_calibration_receive.rs::the_veto_stands_down_when_no_threshold_separates` — the CLI-path
  stand-down gate. `#[ignore]`d for cost (a 45 s recorded 250 Hz capture; 628 s at its budget). **So no
  default-gate test drives the latch TRUE on either path today** — `a_narrow_receive_filter_raises…`
  reads it in the default gate and asserts it *false* (`rho_calibration_receive.rs:153`).
- `src/rho_calibration.rs`'s unit tests cover the hysteresis and the stand-down arithmetic, not the
  engine latch.
- The repo's rule that a probe needing private access is a unit test, never an exported accessor —
  which decides where the gate lives.

## Twins

- **Exactly two production `stands_down(` call sites** — `engine.rs:2496` and `:4222` at `main`
  (`:2508` and `:4234` on the branch in review). Verified by grep, with `rho_calibration.rs:170`'s
  definition as the positive control.
- **Accept counting** is the same on both paths and is kept: changing it would change the CLI's
  observable counts.
- `accepted_settle_positions` is pushed only at the CLI site. A CLI diagnostic; out of scope.
- The two sites condition the calibration stream differently — the OTA site pushes only past
  `AFC_SETTLE_DEADBAND_HZ`. Unchanged here; noted on #1342.
- **#1341** changes the *caller* (`ota_decode_and_ack_inner:2835-2895`), not the veto site.
  Independent; not bundled.
- **A dead third arm (found in review):** `acquire_at_onset` has a second caller,
  `scan_burst_onsets(.., settle = true)` (`:2541`), and no caller ever passes `true` (`:2321`, `:2359`,
  `:2381`). The helper covers it by construction; deleting the parameter is separate work.

## Proposal

One private helper, called by both sites:

```rust
enum VetoVerdict { Corroborated, StoodDown, Rejected { threshold: f32 } }

fn decide_preamble_veto(&mut self, veto: &PreambleVeto, rho: f32, onset: usize) -> VetoVerdict
```

It owns `push_at`, the threshold, `stands_down`, the latch transition with its two `warn!` lines
(moved verbatim), and all three counters: stood down → `rho_stand_down_settles` and
`rho_accepted_settles`; corroborated → `rho_accepted_settles`; rejected → `rho_rejected_settles`. Each
site keeps only its path-specific work: the CLI site, on `Rejected`, logs with `settle.fine`, resets
the correction and `continue`s, and otherwise records the accepted position; the OTA site returns
`!matches!(verdict, Rejected { .. })`.

**Behaviour:** none changes on the CLI path — the same samples, counters and log lines. On the daemon
path the latch, its `warn!` and `rho_stand_down_settles` now update.

**Alternative considered:** patch the OTA site inline — about ten lines, no helper. Cheaper, but it
leaves two copies of a step that has already drifted twice (#1168's counters, then this latch). I
recommend the helper.

## Gate

An in-crate unit test in `engine.rs`, because priming the calibration needs private access.

- **Fixture:** the seam gate's, reproduced — BPSK250+`Rs`, 200 Hz CFO, silence either side, fed
  through `accumulate_capture` at the daemon's tick.
- **Prime:** 64 × `rho_calibration.push(0.40)` → anchor 0.40 → derived 0.72, above BPSK250's
  `DELIVERED_FRAME_RHO_BOUND` (0.50), so the first query stands down.
- **Daemon arm (`decode_burst`, the default):** assert `rho_stand_down() == (true, n)` with `n ≥ 1`,
  and that settle attempts moved (not vacuous). Predicted on current code: `(false, 0)`.
- **Positive control, CLI arm, same primed fixture:** assert the same. It passes today, which proves the
  fixture reaches a stand-down at all.
- **The OTA arm** (`ota_decode_burst` with a session) goes through the same helper. `server::run` has
  two arms and the seam gate tests both; include it if the cost allows.
- **Sabotage:** restore the OTA site's inline pre-fix code → the daemon arm fails, the CLI control
  still passes.

## Weakest points — attack these

1. **Is 64 × `push(0.40)` a faithful stand-in for a calibrated stand-down?** It skips how the median got
   there. The property under test is the *recording*, not the calibration — but that has to be said, and
   the gate must not be read as evidence that a real station reaches a stand-down on the daemon path.
2. **Does the gate depend on the decode succeeding after a stood-down accept?** It must not.
3. **Is moving the CLI's `rho_accepted_settles += 1`** from after its long comment into the helper
   truly equivalent — no path between them that skips it?
4. **One engine latch, two paths, one mode.** `standing_down` (the hysteresis) lives in
   `RhoCalibration`; `rho_stand_down` is the engine's announce latch. Both paths now write the same
   latch. Fine for one template; per-mode `RhoCalibration` (reviewed, deprioritised) would have to
   revisit it.
5. **Should the fix also raise a `ControlEvent`** so the control plane — #1157's intended consumer —
   sees it? I say out of scope. Is that a scope choice or an evasion?

## Prompt

Sent as below, verbatim.

````text
You are adversarially reviewing a small design proposal AND a correction to a public issue, in the OpenPulseHF repository (/home/dc0sk/git/OpenPulseHF, a Rust HF software modem). Your job is to FALSIFY, not agree.

HARD CONSTRAINTS: a `git push` with a pre-push hook running `cargo test` is in progress on this checkout. Do NOT modify, create or delete any file in /home/dc0sk/git/OpenPulseHF, do NOT run state-changing git commands, do NOT run cargo. Read-only git and `gh issue view` / `gh pr view` are fine. **Check every code citation at `main` = `a7412113` with `git show a7412113:<path>`**, not the working tree (the working tree is a branch that shifts engine.rs by ~12 lines).

## Read the record first, in full — not the body alone, not a tail

- `gh issue view 1342 --comments` — the issue being fixed (0 comments; the body is the coordinator's, and is known to contain two errors — see below).
- `gh pr view 1157 --comments` — the origin of the stand-down contract (REQ-RX-03): "latched with hysteresis, logged".
- `gh issue view 1118 --comments` — its fix (#1168) created the daemon-path veto call site and wired the accept/reject counters there after a gate caught the veto "deciding invisibly".

## The texts, in /tmp/claude-1000/-home-dc0sk-git-OpenPulseHF/95206a0f-ba16-49f6-b91d-ff9465e21d4b/scratchpad/

1. `1342-stand-down-latch.md` — the design artifact (Record / Consumer / Prior art / Twins / Proposal / Gate / Weakest points).
2. `1342-correction-comment.md` — a comment to be posted on #1342, withdrawing its third consequence and the word "instead".
3. `1342-body-edits.txt` — two struck-through edits to the #1342 body.

## Code to read (at a7412113)

`crates/openpulse-modem/src/engine.rs`: `acquire_at_onset` (~2464-2507), the CLI veto block in `receive_with_timeout_fec_inner` (~4210-4295), the `rho_*` fields (~838-857) and getters (~1125-1152), `decode_burst` / `decode_burst_inner` (~2278-2410), `ota_decode_burst`. `crates/openpulse-modem/src/rho_calibration.rs` (push, push_at, thinning, stands_down + hysteresis, MIN_SAMPLES). `crates/openpulse-modem/tests/daemon_runs_acquisition_chain.rs` (the fixture the gate reuses), `crates/openpulse-modem/tests/rho_calibration_receive.rs`, `crates/openpulse-modem/Cargo.toml` (dev-dependencies), `docs/dev/project/reachability-baseline.txt` (the DORMANT(#1118) note at ~63).

## Check

A. **The correction first — it is going public.** Is it right that the CLI path counts a stand-down as an accept (stand-down branch falls through to `rho_accepted_settles += 1`; the only `continue` between is the reject branch)? Are the cited lines 4245 / 4255 / 4286 exact at a7412113? Is anything ELSE in #1342's body wrong that the correction does not fix? Is the correction's own wording precise and not over-corrected?
B. **The design's five "Weakest points"** — answer each.
C. **The helper's equivalence claim**: moving the CLI's `rho_accepted_settles += 1` and the latch/warn into a shared helper — is the CLI path's observable behaviour (counters, log lines, `continue`, `afc_correction_hz` reset, `accepted_settle_positions`, `planner.note_settled`) truly unchanged? Enumerate anything that would move.
D. **The gate**: (1) can an in-crate `#[cfg(test)]` module in engine.rs build the fixture — are `bpsk_plugin`, `openpulse_channel::cfo::CfoChannel`, `openpulse_config::DaemonConfig` available to unit tests? (2) Does priming via 64 × `RhoCalibration::push(0.40)` survive the thinning when the real run's onsets then start near 0 (`push` advances `last_onset` by `MIN_ONSET_ADVANCE` each call; `push_at` accepts an EARLIER onset)? Will `stands_down` return true on the FIRST real query (hysteresis engages at `derived > bound`)? (3) Does `decode_burst` actually reach `acquire_at_onset` for this fixture (phase 1 must fail at 200 Hz)? (4) Would the assertion `rho_stand_down() == (true, n≥1)` really read `(false, 0)` on current code — i.e. is it a gate that can fail? (5) Is the CLI arm a real positive control, and which CLI entry would the unit test call?
E. **Consumer/Twins completeness**: any third site that decides the veto? Any reader of the latch or counters the artifact misses? Is the DORMANT(#1118) "stale note" claim right (note still present, #1118 closed, getters without production readers)?
F. Anything wrong or unproven in the framing, and any question not asked.

Return: for the correction, MUST-FIX / SHOULD-FIX with exact replacement text (or "sound"); for the design, CONFIRMED / REFUTED / UNCERTAIN per point with the file:line you read at a7412113, and a short recommendation (helper vs inline; gate shape; OTA arm in or out).
````

## Verdict (Fable, 2026-09-11)

Reproduced verbatim, except that HTML entities the transport introduced (`&lt;`, `&gt;`, `&amp;`) are restored to the characters sent. Two notes on how it was applied: the suggested title in SHOULD-FIX 2 ("…is not latched, logged or counted") contradicts MUST-FIX 1, so #1342 was retitled "…is not announced, logged or counted" instead; and the control-plane follow-up it asks to file is filed alongside this change as #1344.

````text
## Verdict summary

The correction's core claim is **right**, its three line citations are **exact**, and the design is **sound in shape** — but the correction (and the artifact's restatement) contains one over-correction that must not go public, and the issue title still asserts the consequence the correction withdraws.

---

## A. The correction — going public

**Core claim: sound.** At `a7412113`, `engine.rs:4244-4256`:
```
4244  if stand_down {
4245      self.rho_stand_down_settles += 1;
4246  } else if rho < threshold {
      ...
4255      continue;
4256  }
      ...(comment only)...
4286  self.rho_accepted_settles += 1;
```
The only exit between `:4245` and `:4286` is the `continue` at `:4255`, inside the `else if`, which a stood-down settle never enters. Lines 4245 / 4255 / 4286 are exact. OTA side: `:2497` `accepted = stand_down || rho >= threshold` → `:2502`. Both count a stand-down as an accept.

**MUST-FIX 1 — "the daemon path is neither" is an over-correction.** `stands_down` takes `&mut self` and updates the hysteresis latch `standing_down` (`rho_calibration.rs:177-181`); the daemon path calls it (`engine.rs:2494-2496`). So the daemon path *is* latched with hysteresis at the decision. What it lacks is the engine's *announce* latch `rho_stand_down` (`:842`, written only at `:4227`), the `warn!` (`:4229-4241`) and the counter (`:4245`). Your own artifact's weakest-point 4 draws exactly this distinction; the comment contradicts it. Replace:

> ~~#1157 specified the stand-down as "latched with hysteresis, logged"; the daemon path is neither.~~
> #1157 specified the stand-down as "latched with hysteresis, logged". On the daemon path the *decision* is hysteretic — that latch lives in `RhoCalibration::standing_down` (`rho_calibration.rs:177-181`) and `acquire_at_onset` drives it (`engine.rs:2494-2496`) — but the engine's announce latch `rho_stand_down`, its `warn!`, and `rho_stand_down_settles` are written only at the CLI site (`:4227`, `:4229-4241`, `:4245`), so the state is neither reported nor logged.

Make the same replacement in the artifact's "The defect, restated correctly".

**SHOULD-FIX 2 — the title.** "…is silent **and counted as an accept**" now names shared, intended behaviour as the defect. Retitle to e.g. *"On the daemon path, the correlation veto's stand-down is not latched, logged or counted"*; or add one line to the correction saying the title's second clause is retained only as description of both paths.

**Everything else in the body checks out** against `a7412113`: `:2497`, `:2498-2500`, `:843-845`, `:1150`, test readers at `rho_calibration_receive.rs:86,:95`, `MIN_SAMPLES=64` (`rho_calibration.rs:37`), bound 0.50 (`plugins/bpsk/src/modulate.rs:243`), "about five thinned samples per mode pass" (step 32 → coarse 128 → `scan_end ≤ 4·1024`, 33 queries thinned at 1024 → ≤5; `:2212-2218`, `:2428`, `rho_calibration.rs:45`), "twice that under #1341" (#1341 confirms BPSK250 enters phase 2 as SL5+Rs *and* as the uncoded fallback, and each pass restarts at onset 0, which `push_at` accepts as an earlier onset). Nit: the body's CLI range `:4219-4250` stops short of the `:4286` line the correction relies on.

Edits 1 and 2: sound as written.

---

## B. The five weakest points

1. **CONFIRMED, with a required caveat.** Arithmetic holds (`max(0.40, 0.40·1.8=0.72) > 0.50`; anchor needs exactly 64, `rho_calibration.rs:142`). The gate tests *recording*; its name and doc must say it is not evidence a real daemon station reaches a stand-down (the body's "unmeasured" stands).
2. **CONFIRMED.** Counters are written at `:2501-2505`, before any decode retry (`:2372-2386`). Decode outcome is irrelevant. The precondition is that ≥1 coarse onset passes the settle guards (`:2478-2481`) and `preamble_rho` returns `Some` — proven on this fixture by the existing default-gate test (see D3).
3. **CONFIRMED equivalent for the counters.** Nothing between `:4245` and `:4286` but the reject branch and a comment. What does move is in C.
4. **CONFIRMED as a hazard — and add one more.** `stands_down` sets `standing_down = false` when a mode has a template but no bound (`rho_calibration.rs:171-174`). With one shared engine latch, a second templated-but-unbounded mode would flip it off between BPSK250 queries and the warn would flap per query. Unreachable today (`DERIVED_FOR` = BPSK250 alone, and it carries the bound); record it beside the per-mode note.
5. **Scope choice, and a *recorded* one — cite it.** `docs/dev/design/bandwidth-aware-rho-threshold.md:91-93`: "a control-plane event is deliberately not added here, because a new `EngineEvent` variant is a workspace-wide change (exhaustive matches in the app crates)". That is your answer. But "recorded so it is not lost" is currently recorded only in this artifact — the #1118 comment thread is the precedent for exactly that rotting. File it (issue or #1342 comment).

---

## C. Helper equivalence — what moves

Counters, `continue`, the AFC reset (`:4254` stays at the site; the OTA site relies on the caller's rollback `:2436`), `accepted_settle_positions` (`:4287-4289`) and `planner.note_settled` (`:4293`, which also runs on the `preamble_rho == None` path `:4213` → `:4293`) are all unchanged if kept at the site as proposed. Borrow is fine: `preamble_matcher` is a local (`:3755`).

Things that do move, each needing one sentence in the PR:

- **Log order.** The CLI logs the reject `debug!` (`:4247-4252`, needs `settle.fine`) *before* counting (`:4253`); with the helper counting, the order inverts. Log-only.
- **The "corroborated" line (`:4290`)** currently fires for stood-down settles too. If the helper owns it, it becomes a new debug line per accepted coarse query on the daemon path; if you reword it for stand-down (you should — "corroborated" is false there), that is a CLI log-text change and "same log lines" is no longer literally true. Declare whichever.
- **Accept test.** CLI rejects iff `rho < threshold` (`:4246`); OTA accepts iff `rho >= threshold` (`:2497`). They differ only on NaN; `search_normalized` returns 0, never NaN, on a zero window (`acquisition.rs`, `rho = score.sqrt()/(sqrt(energy·t_energy)+1e-12)` with `energy ≥ floor`), so unreachable — but the helper must adopt the CLI form for "CLI unchanged" to hold.
- **Inventory item you missed:** `acquire_at_onset` has a **second caller**, `scan_burst_onsets(.., settle = true)` at `:2541` — and **no caller passes `true`** (`:2321`, `:2359`, `:2381` all `false`). A dead arm left from the first #1118 shape. The helper covers it by construction; note it, or delete the parameter separately.

---

## D. The gate

1. **CONFIRMED.** `bpsk-plugin`, `openpulse-channel` are `[dependencies]` (`Cargo.toml:13,15`); `pub mod cfo` (`openpulse-channel/src/lib.rs:10`); `openpulse-config` is a dev-dependency (`:25`), and dev-deps link into the lib's `cfg(test)` build. `mod tests` (`:7513`) and `idle_rho_probe` (`:8241`) already do this. `e.rho_calibration.push(..)` is legal from a child module.
2. **CONFIRMED.** After 64 `push`, `last_onset = 63·1024`; the first real `push_at(rho, 0)` fails `onset >= prev` (`rho_calibration.rs:109`) so it is kept; median stays 0.40 until >64 low samples arrive (one burst adds ≤5, ≤10 under #1341). `stands_down` engages on the first query (`standing_down` false → `derived > bound`, `:180`). Also: the verdict is computed on every query regardless of whether the push was thinned, so thinning cannot suppress it.
3. **CONFIRMED by a passing default-gate test, not by reading**: `the_daemon_decode_burst_arm_runs_the_acquisition_chain` asserts `rho_accepted+rho_rejected > 0` through `decode_burst`, and on that path those counters are written only inside `acquire_at_onset` (`:2502/:2504`). Phase 1 must have failed for `acquire_burst_correction` to run (`:2362-2372`).
4. **CONFIRMED — it can fail.** Writers of `rho_stand_down`: `:988` (init), `:4227`. Of `rho_stand_down_settles`: `:989`, `:4245`. Both CLI-only. Daemon arm reads `(false, 0)` today.
5. **A control for the instrument, not for daemon reachability** — which is fine, because D3 supplies the latter. Entry: `receive_with_fec_mode_timeout` (`:3606`, pub) exactly as `the_cli_path_runs_the_chain` calls it. **Sharpen the daemon-arm assertion** to `rho_stand_down_settles == rho_accepted_settles ≥ 1 && rho_rejected == 0`: with derived 0.72 every reached query is a stand-down accept, so this pins the shared "counted as accept" semantics and fails a helper that counts one but not the other. Reads `0 ≠ ≥1` on current code.

Also: a lib unit test cannot import the integration test's fixture, so the 200 Hz / 8000-sample / tick constants will be **duplicated**, not shared by reference (verification rule 5). Say so, and keep `afc_settle_attempts() > 0` as the executable pin that phase 2 ran.

---

## E. Consumer / Twins

- **No third decision site.** `stands_down(`: `:2494-2496`, `:4222` (+ def `rho_calibration.rs:170` as control). `preamble_rho(` production: `:2487`, `:4214` only (`:8232`, `:8285` are in-crate tests). `effective_threshold(`: `:2493`, `:4221`, + getter `:1145`.
- **No production reader of any `rho_*` getter.** Tree-wide grep excluding `tests/` and `engine.rs` finds nothing; daemon-crate grep for `rho_|stand_down|afc_settle_attempts|dcd_blocks_processed` finds nothing, with `accumulate_capture`/`decode_burst(` matching in the same tree (`daemon/src/lib.rs:4977`, `monitor.rs:92`) as the positive control.
- **DORMANT note: stale, confirmed.** `reachability-baseline.txt:63-67` present at `a7412113`, says "the daemon does not run the acquisition chain at all (#1118)"; #1168 merged 2026-08-19T05:58:59Z, #1118 closed one second later; the design doc `:84-85` already says the opposite. Note the baseline lists **five** `rho_*` names (`:469-473`), the note says "three". Do not delete the lines — the getters still have no production reader, so the ratchet would fail; rewrite the note to point at the control-plane follow-up.
- **Missed reader:** `a_narrow_receive_filter_raises…` reads `rho_stand_down().0` in the *default* gate and asserts it false (`rho_calibration_receive.rs:153`). So "no default-gate test covers the latch on either path" should read "drives it **true** on either path".
- **Missed twin:** the dead `settle = true` arm (C).
- Doc twins to touch in the same change: `bandwidth-aware-rho-threshold.md:89-93` (observability section).
- Wording: #1341 changes the **caller** (`ota_decode_and_ack_inner:2835-2895`), not the veto site. Independent, as you say.

---

## F. Framing

- **Unit of `rho_stand_down_settles` on the daemon path** is per coarse-grid query (≤33 per burst per mode pass), not "settles let through to a decode" as on the CLI. That is already the unit of `rho_accepted_settles` there (`:2502`); keep one unit, document it, do not invent a second.
- "Both paths count a stand-down as an accept … kept" — state *why*: `rho_accepted_settles`' own doc (`:813-819`) defines it as "the gate passed this input", which a stood-down settle did.
- The "neither latched nor logged" imprecision is in the artifact's restatement as well as the comment.

---

## Recommendation

- **Helper, not inline** — CONFIRMED. Two drifts plus a dead third arm is the shape the helper exists for. Adopt the CLI's `rho < threshold` reject form; decide the stood-down log wording explicitly.
- **Gate shape**: in-crate unit test, primed via `push`, named as a recording gate; equality assertion from D5; CLI arm as instrument control; duplicated fixture declared.
- **OTA arm: in.** Same fixture, same helper, and `ota_enabled` is the on-air configuration, so it is the arm a real station runs; the seam gate already pays this cost in the default run.
- **Before posting**: apply MUST-FIX 1 to both texts, retitle or annotate (SHOULD-FIX 2), and file the control-plane consumer as a tracked item citing the design doc's deliberate deferral.
````
