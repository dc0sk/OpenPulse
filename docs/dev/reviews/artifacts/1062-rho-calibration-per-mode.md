# Should `RhoCalibration` be per-mode? (#1062, step 3 of the agreed order)

**Status (2026-09-11 evening): reviewed, then DEPRIORITISED.** The verdict below says "proceed now",
on the premise that a second preamble template was imminent via #1062's route A or B. #1062's thread
had closed route A on 2026-08-04 and argues against route B for the BPSK31-class rungs (see the
correction in `1062-bpsk31-veto-constants.md`), so no second template is scheduled and this change
waits. Committed now because its verdict is already cited. Line numbers are as of branch
`fix/1062-veto-grid-step-domain` before its commits were reworded; `engine.rs` lines past ~415 sit
about 12 lines further down than on `main` at `a7412113`.

My instinct is **per-mode, keyed by the template's `for_mode`, falling back to the published constant
until each mode's own stream fills**. Test that instinct rather than confirming it, and flag anything
wrong or unproven in the framing — especially the claim that starvation is "safe", which I think is
weaker than it sounds (point 1 under *Weakest*).

## The defect this addresses

`RhoCalibration` (`crates/openpulse-modem/src/rho_calibration.rs:73-80`) holds one `VecDeque<f32>`,
one `last_onset`, one `standing_down` — no mode key — and the engine holds one instance
(`engine.rs:851`). Its median × `FAMILY_FACTOR` (1.8) raises each mode's published threshold. ρ is
normalised per template, so its noise population is a property of **audio × template**: the review
measured a BPSK31 DDC-arm ceiling around 0.33 against a BPSK250 passband p50 of 0.13–0.24. Two
templates feeding one median produce a level that describes neither, compared against each mode's
own bound. The CFAR's stated premise — "the stream this compares against is the stream it is built
from" (engine.rs:4228-4230) — is false the moment a second template exists.

**Latent, like the grid-step defect:** `push_at` runs only where a veto exists (both sites below sit
inside a `veto`/`v` binding) and `MODES_WITH_VETO = ["BPSK250"]`. The first second template
activates it.

`FAMILY_FACTOR`'s own justification also stops holding under mixing: it is "the p99/p50 ratio's upper
end with margin", measured on *single* noise populations (1.29–1.50 across 5 captures + 4 synthetic
bands, `rho_calibration.rs:56-71`). A mixture of two populations with different locations is a
different distribution shape — bimodal — so that ratio does not transfer to it.

## Consumer

Every reader of the calibration, by `file:line` at `0112ff1d`:

- **Daemon/OTA phase-2 acquisition** — `engine.rs:2504` `push_at`, `:2505` `effective_threshold`,
  `:2506-2508` `stands_down`, deciding `accepted` at `:2509`. Reached from `accumulate_capture`
  (`:2125`) → `ota_decode_burst` → phase 2. Per the #1062 review, on this path the veto gates
  phase-2 acquisition only, not frame start.
- **CLI `receive_with_timeout_fec_inner`** — `engine.rs:4231` `push_at`, `:4232-4233`
  `effective_threshold`, `:4234-4237` `stands_down`, and the engine-wide `rho_stand_down` announce
  latch at `:4238-4250`.
- **Public getters with test-only callers** — `rho_effective_threshold(&self, mode)` (`engine.rs:1155`)
  and `rho_calibration_samples()` (`:1150`), read by `tests/rho_calibration_receive.rs:84-93`. Note
  `rho_effective_threshold` **already takes a mode** and applies the shared calibration to that mode's
  published constant — the type signature is per-mode over per-engine state.

## Prior art

- **The engine already keys state by mode:** `ota_retained_llrs: HashMap<String, Vec<Vec<f32>>>`
  (`engine.rs:646`), filled with `entry(mode.clone())` (`:3028`), bounded per mode, cleared on OTA
  session start/stop (`:1697`, `:1715`) and on a successful decode (`:2919`, `:3056`). A per-mode
  calibration would follow that shape exactly.
- **The engine already records the opposite decision, explicitly:** `noise_floor` (`engine.rs:659`) is
  documented *"Mode-independent by design"* — correct for it, because an energy floor is a property of
  the **audio** alone. `rho_calibration` sits nine fields away with no such note and no mode key.
- **The template already carries its mode:** `PreambleTemplate.for_mode`, enforced by
  `build_preamble_veto` (`engine.rs:~6925`), which refuses a template whose constants were derived for
  a different mode. The key is therefore already present on every `PreambleVeto`... **UNCHECKED**
  whether `PreambleVeto` retains `for_mode` after construction (`PreambleVeto::new` copies threshold,
  grid and bound; I did not see it copy `for_mode`).

## Twins

- **The hysteresis state.** `RhoCalibration.standing_down` and the engine's `rho_stand_down` announce
  latch are both engine-wide. Per-mode calibration implies per-mode hysteresis; if the announce latch
  stays engine-wide, alternating OTA candidates flap it and emit a warning per flip — the thing the
  release margin exists to stop.
- **`MIN_ONSET_ADVANCE` thinning** uses one `last_onset`. Keyed per mode, two modes querying the same
  onset each keep one sample — correct per mode, but worth confirming it is not a double count of one
  window of audio *within* a mode.
- **`rx_snr_estimate`** (`engine.rs:638`) and **`last_afc_offset_hz`** (`:622`) are also engine-wide.
  They are overwritten last-values rather than accumulated statistics, so they cannot mix two
  populations the way a median does — but the SNR one is read at `:3079`, and CLAUDE.md records that
  SNR scales are per-waveform-family ("never compare one mode's reading against another mode's
  floor"). **UNCHECKED** whether that reader compares across modes.
- **The counters** `rho_accepted_settles` / `rho_rejected_settles` are engine-wide and read by gates.
  Counters are legitimately aggregate; listed so the decision to leave them is explicit.

## Proposal

`HashMap<String, RhoCalibration>` keyed by the mode whose template produced the ρ. Each entry fills
independently; below `MIN_SAMPLES` it returns the published constant unchanged. Hysteresis per entry;
the announce latch per mode. Cleared on the same lifecycle events as `ota_retained_llrs`? — **I am not
sure it should be**: a calibration describes the station's noise, which does not change when a session
ends, and clearing it would re-open the cold window every session.

A test that fails on current code: an engine with two templates (the `LONGTMPL` stub in
`engine.rs`'s `ddc_veto_arm` module plus a second stub at a different length), each mode's ρ stream
held at a different level, asserting each mode's effective threshold derives from its own stream only.

## Weakest points — attack these

1. **"Starvation is safe" is only true relative to today, not relative to #1060.** `MIN_SAMPLES = 64`
   at one sample per 1024 input samples is ≥ 65 536 samples (~8 s) of *queried* audio per mode. A rung
   reached only in phase 2 — after every candidate failed at every onset — may never fill. Below
   `MIN_SAMPLES` it uses the published constant, which is exactly the behaviour #1060 found wrong on a
   narrow-filter station (idle ρ 0.413 against BPSK250's 0.40 on a real 500 Hz filter). So per-mode
   keying trades a *wrong* shared threshold for an *uncalibrated* own one on rarely-queried modes. Is
   that better, and should starvation be surfaced (counted, announced) rather than silent?
2. **Should it be done now, or bundled with the first second template?** The grid-step precedent says
   fix a latent defect before the change that activates it, with a test that fails on current code.
   But nothing can be *measured* here until a second template exists — the test above uses stubs. Is a
   stub-only gate enough to justify the change now?
3. **Is the mode string the right key?** Two modes could in principle share a template, or one mode
   could be correlated on two arms (passband vs DDC) under different budgets. The population is really
   a property of the correlator — template, arm, decimation, grid. Keying by mode is simpler; is it
   correct?
4. **Is there a cheaper design that keeps one stream?** E.g. normalise each sample by its template's
   own reference level before pushing. I think this needs a per-template noise reference that does not
   exist, i.e. it re-imports the constant the CFAR exists to avoid — but I have not tested that.
5. **`FAMILY_FACTOR` may itself be per-template.** It was measured across bands on single populations
   but, as far as I can find, only for BPSK250's template. If p99/p50 differs for a DDC-arm template,
   keying the median per mode without re-deriving the factor is half a fix.

---

## Prompt

**Condensed from the prompt as sent:** `[...]` marks elided repetition, and the list of files to read
is paraphrased into one paragraph (the original gave line ranges per file). Every question, A–F and
the five weakest points, is as asked.

```text
You are adversarially reviewing a design proposal in the OpenPulseHF repository (/home/dc0sk/git/OpenPulseHF, a Rust HF software modem). Your job is to FALSIFY it, not to agree with it.

HARD CONSTRAINT: a workspace gate (`scripts/gate.sh`) is running on this checkout right now. [...] You must NOT modify, create, or delete ANY file [...], must NOT run git commands that change state [...], and must NOT run cargo [...]. Reading files and running grep/sed/awk/python3 on them is fine. If a claim can only be settled by running a test, say so and name the test.

Read the proposal in full first (the sections above this Prompt).

Context you should read rather than trust the proposal about: rho_calibration.rs (whole file); engine.rs — the struct fields, PreambleVeto / PreambleVeto::new, the two calibration call sites, rho_effective_threshold, ota_retained_llrs and its lifecycle, and the OTA phase-2 candidate loop; tests/rho_calibration_receive.rs and tests/veto_membership_pin.rs; docs/dev/reviews/artifacts/1062-bpsk31-veto-constants.md (its VERDICT section); CLAUDE.md's "Adversarial review", "Verification mechanics", and the #1053, #1060 and rate-ladder SNR-scale sharp edges.

Answer the five "Weakest points" in the proposal, and also:
A. Is the defect real as described? Trace both push_at call sites and which modes can reach each. Can two modes' ρ values actually interleave within one station's stream in practice? An unreachable mixing defect is a different finding from a live-on-activation one.
B. Does PreambleVeto retain for_mode after construction? If not, what is the least invasive way for the call sites to know which calibration entry to use?
C. Should a per-mode calibration be cleared on OTA session start/stop like ota_retained_llrs? Decide it, with the reason.
D. Is FAMILY_FACTOR (1.8) measured for any template other than BPSK250's? Find the measurement it cites and say exactly which template(s) it ran on.
E. Check the twin list. Is rx_snr_estimate compared against a mode's floor from a different mode? Report what you actually find; do not speculate.
F. Recommend the ORDER relative to the rest of #1062's agreed plan: now with a stub-only failing test, or bundled with the first second template?

Flag anything wrong or unproven in my framing, including any question I should have asked and did not. Return a structured verdict: CONFIRMED / REFUTED / UNCERTAIN with the file:line evidence you actually read for each point, then a short overall recommendation.
```

## Verdict (Fable, 2026-09-11) — proceed now, per-mode, but my test was vacuous and I missed a consumer

**Decision:** key `RhoCalibration` by the `mode` already in scope at both call sites
(`acquire_at_onset(&mut self, mode, …)` and `receive_with_timeout_fec_inner`'s `mode`). That IS keying
by `for_mode`, because `build_preamble_veto` refuses any template whose `for_mode != mode`, so within
one engine mode ≡ template identity by construction. `PreambleVeto` does **not** retain `for_mode`
(fields: `filter`, `rho_threshold`, `rho_grid_hz`, `delivered_frame_rho_bound`) and does not need to.
**No clearing on session start/stop** — a calibration describes audio × template, not a message, and
on the daemon path the cold window is already ≥ 13 fully-failed bursts. `FAMILY_FACTOR` stays
engine-wide pending a recorded per-template p99/p50 ratio.

**Errors in my proposal, each verified against the code by me after the verdict:**

1. **My Consumer section missed `MonitorRuntime::decode_all`** (`crates/openpulse-daemon/src/monitor.rs:84-99`),
   which drives ONE engine through `decode_burst(mode)` for every configured mode on every burst.
   Verified: `decode_burst_inner` (engine.rs:2341) reaches `acquire_burst_correction` (:2384), and
   `reset_afc` (:1500-1503) clears only `afc_correction_hz` and `last_afc_offset_hz` — the calibration
   survives across modes. Opt-in (`[monitor] modes` empty by default), but shipped — and it mixes
   faster than the OTA path.
2. **My test sketch was vacuous.** The `LONGTMPL` stub has no `estimate_afc_hz`, so `fine = 0`,
   `worth_retrying` is false and `acquire_at_onset` returns before `push_at`; and it has no
   `frame_geometry`, so `afc_window` falls back to 1056 < the DDC arm's ~8065 and `preamble_rho`
   returns `None`, accepting the settle unmeasured. Both modes would sit at their published constants
   and the assertion would pass on unfixed code. **What can fail:** real templates (`BpskPlugin` for
   BPSK250 plus a forwarding wrapper publishing BPSK31's real template, the `NoVetoBpsk` pattern)
   driven through `decode_burst` on recorded idle, with per-mode sample counts > 0 as tripwires.
3. **"Starvation is safe" rested on the CLI arithmetic.** On the OTA path a phase-2 pass contributes
   ≤ 5 thinned samples per burst for BPSK250 and only on bursts where every cheaper stage failed, so
   `MIN_SAMPLES = 64` needs ≥ 13 such bursts. The daemon calibration is already starving; per-mode
   keying does not create that. Count it per mode; do not announce it.
4. **Mixing severity is asymmetric**: a BPSK31 phase-2 pass yields up to 33 accepted samples per burst
   against BPSK250's ≤ 5, so a shared median becomes BPSK31's within a few bursts. Whether that stands
   BPSK250's veto down everywhere (p50 × 1.8 > 0.50) or merely raises it depends on R3's median — the
   unrecorded number step 4 exists to capture.

**Two defects found in passing, both verified by me:**

- **The OTA call site is half-wired for stand-down** (engine.rs:2504-2518): it calls `stands_down`
  but never updates `self.rho_stand_down` or `rho_stand_down_settles`, so on the daemon a stand-down
  is silent and counted as an accept. Fix in the same change — it touches the same lines.
- **Phase 2 repeats a settle pass whenever two of its entries share a mode** — the fallback sharing a
  rung's mode (`hpx_hf` SL5 `BPSK250+Rs` on a station whose active mode is BPSK250), or two rungs on
  one mode with different FEC (`hpx_hf` SL9/SL12, SL10/SL13, SL11/SL14; `hpx_modcod` SL2/SL3 …). The
  settle takes its bounds from the mode alone and `afc_mini_settle` zeroes the AFC itself, so the
  second pass settles over the same windows; on BPSK250, the only template mode, `push_at` re-keeps
  every window the first pass kept (it treats only a later onset as overlap). The duplicate runs only
  when the first same-mode entry FAILED (the scan breaks on first success), and adds one settle plus
  one fine scan to a two- or three-entry stage — not a general doubling. Filed separately; fix is a
  design choice (cache the correction per mode within a burst covers both routes). Citations for it
  live in the issue, taken at `main`; the branch lines this section once carried were wrong for `main`.

**Also:** `rx_snr_estimate` has no live cross-mode comparison — `set_rx_snr_estimate` has test-only
callers, so production always measures SNR on the decoded mode's own estimator (resolves that twin).
The two call sites condition the stream differently for one mode (OTA pushes only past
`AFC_SETTLE_DEADBAND_HZ`; CLI has no deadband) — small on noise, unmeasured. REQ-RX-02's statement is
written mode-agnostically; the ledger entry should say it is satisfied per template.

**Order:** step 3 now, ahead of R3's p50 (which gives severity, not existence) and ahead of route
A vs B (both activate it). Step 4 must record p99/p50 per template; publishing a second template is
gated on that ratio ≤ 1.8, added to `veto_membership_pin.rs`'s checklist.
