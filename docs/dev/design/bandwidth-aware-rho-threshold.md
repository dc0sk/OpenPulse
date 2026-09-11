---
project: openpulsehf
doc: docs/dev/design/bandwidth-aware-rho-threshold.md
status: resolved
last_updated: 2026-08-17
---

# Bandwidth-aware preamble-correlation threshold (#1060)

Work plan and design for making the preamble-correlation veto's threshold track the station's own
receive bandwidth instead of being a per-mode constant.

## Why

`PreambleVeto.rho_threshold` is a constant published by the plugin (BPSK250: 0.40, derived from its
decode cliff and validated on two SSB-bandwidth, 3-second idle captures). Measured on a real IC-9700
2026-08-17, the idle ρ ceiling depends strongly on receive bandwidth:

| receive filter (measured −20 dB band) | per-window p50 | p99 | max over 45 s | windows ≥ 0.40 |
|---|---|---|---|---|
| wide (2470 Hz) | 0.131 | 0.197 | 0.227 | 0.00 % |
| 500 Hz (554 Hz) | 0.236 | 0.334 | 0.413 | 0.07 % |
| 250 Hz (309 Hz) | 0.351 | 0.492 | 0.579 | 18.8 % |

A 500 Hz filter is an ordinary setting for these modes, and it is what an operator reaches for in
exactly the hot-floor scenario the veto exists for. At 250 Hz the veto corroborates noise on nearly
a fifth of windows, which is the failure mode it was built to prevent.

## Requirements

| ID | Statement | Status |
|---|---|---|
| `REQ-RX-02` | When per-window correlation statistics are available, the receiver shall derive the preamble-correlation threshold from them rather than using the published constant alone. | draft |
| `REQ-RX-03` | When the derived threshold would exceed the mode's published delivered-frame ρ bound, the receiver shall stand down to energy-only frame detection rather than veto. | draft |

## Design

**Shape: CFAR against the station's own correlation statistics** — chosen over two alternatives after
adversarial review (see *Alternatives*).

1. **The sample stream is the veto's own query stream.** Every ρ the veto already computes at a
   settle attempt is a sample. This is the exact statistic being decided on, and it costs nothing:
   no extra correlations, which matters because correlation cost is what #1138 got wrong by
   assuming it. Samples are **thinned** to one per window length of onset advance, because
   consecutive queries advance ~129 samples over a ~1000-sample window and are not independent
   draws.
2. **Anchor on the median.** A robust *location* estimator, poison-resistant in the same sense as
   `NoiseFloorTracker` is for energy: a *rare* frame lands in the upper tail and cannot move a
   median. This is what removes the need for a signal-free oracle — and there is no honest one,
   because in the hot-floor regime the energy gate fires continuously, which is exactly when
   calibration is needed. It is **not** immunity in general: heavy traffic that fails to decode is a
   real population and does move the median, which is what item 5's stand-down contains.
3. **Reach the decision level through a measured family factor.** `f11_quantile_ratio_across_bandwidth`
   measured p99/p50 = 1.29–1.50 across a 4.3× move in p50, over five recorded captures and four
   synthetic bands. The factor is a property of the *distribution's shape*, which is what CFAR
   extrapolation needs, rather than a constant fitted to one artifact. **It promises a bounded
   exceedance rate, not a ceiling**: a maximum over N windows grows with N, so any claim of the form
   "above every observed maximum" would be duration-scoped.
4. **The derived threshold can only rise, never fall**: `effective = max(published, anchor × factor)`.
   The published constant encodes decode-cliff knowledge the noise statistics know nothing about, so
   it stays a floor. This makes the change monotone-safe — never weaker than today's behaviour.
5. **Stand down when there is no separation.** If `effective` would exceed the mode's published
   delivered-frame ρ bound, the veto stops vetoing (energy-only, i.e. pre-#1049 behaviour) and says
   so, rather than rejecting every settle. This is #1053's "publish nothing rather than pick a
   number", moved to runtime. The latch has **hysteresis** (engage at the bound, release a clear
   margin below it) so a median wandering near the boundary cannot flap the veto.
   Stand-down is also the containment for the poison case: a station carrying heavy traffic that
   fails to decode pushes many high samples, and the resulting rise ends in energy-only detection
   rather than in silently over-vetoing the traffic that caused it.
6. **Cold start**: until `MIN_SAMPLES` queries exist, the published constant is used unchanged.

### Path inventory (cross-cutting concern)

The threshold is consumed on the settle path only. Traced top-down from the shipped binaries:

| entry | reaches the veto? | evidence |
|---|---|---|
| CLI `receive --listen-ms` → `receive_with_timeout_fec` | **yes** — the surface #1060 is about | `capture_replay_corpus`, `preamble_correlation_settle` |
| on-air scripts (`run-onair-tests.sh`) | yes, via the CLI path | `scripts/run-onair-tests.sh:192` |
| daemon `accumulate_capture` (`server::run` rx tick) | **yes, since #1118** — phase 2 settles and vetoes when phase 1 fails, feeding this calibration from that call site | `daemon_runs_acquisition_chain`, `daemon_frequency_acquisition` |
| ARDOP / KISS bridges, TUI, monitor | no — same reason as the daemon | #1118 |

So there is exactly **one** consumption site (`engine.rs`, the settle comparison), and the
calibration is fed from that same site, and since #1118 from the daemon's phase-2 veto as well —
so the daemon benefit this section deferred is now real.

### Observability

`rho_calibration_samples()`, `rho_effective_threshold(mode)` and `rho_stand_down()` on the engine, so
a gate asserts the calibration ran rather than trusting it, and an operator can see which regime a
station is in. Stand-down transitions are logged at `warn` — on both acquisition paths since
#1342; before it the daemon's phase-2 veto decided stand-downs (the hysteresis in `RhoCalibration`
ran) but never set the engine's announce latch, logged, or counted them, because #1168 wired only its
accept/reject counters. A control-plane event is deliberately not added here, because a new `EngineEvent` variant is a workspace-wide change (exhaustive matches in the
app crates) and does not belong in the same change as the mechanism.

## Alternatives considered

* **A — predict the ceiling from an estimated bandwidth** via the `1/√(T·B)` law (#1062's F7).
  Rejected: `k` would be fitted to three rig points, and it is a property of the alternating
  preamble's time-bandwidth pathology, so a #1062 PN preamble would orphan it. C is
  template-agnostic.
* **B — calibrate in ρ units** (`quantile + fixed margin`). Rejected: the ρ distribution's *width*
  scales with bandwidth just as its location does, so a fixed ρ margin is a different false-alarm
  rate at every bandwidth — B re-imports the bandwidth dependence at second order.
* **D — whiten the statistic** (coloured-noise matched filter) so a fixed constant becomes valid
  again. **Deferred, not rejected**: it is the complete textbook answer and composes with C, but it
  re-derives every measured ρ constant and widens the DSP surface. C is statistic-agnostic, so
  nothing here blocks D later.
* **Reading the rig's filter width over CAT.** Rejected: it makes a DSP property depend on a control
  path that is optional (`cat_backend = "none"` is supported), often absent, and stale the moment the
  operator turns the knob.

## Findings ledger (this iteration)

Newest first. Every finding reaches `fixed`, `deferred` (with a tracking ID) or `rejected` before the
iteration closes.

| ID | Finding | State |
|---|---|---|
| F-1060-06 | The calibration only accumulates while the receiver is making settle attempts, i.e. when the energy gate fires. Replayed as recorded, the 250 Hz capture (mean-square 2.1e-3, under the gate's clamped threshold) produces **zero** settles and zero samples. Self-consistent — no settles means no veto decisions to calibrate — but it means a quiet narrow-filter station is never calibrated. | rejected as a defect: the uncalibrated state is also the state in which the threshold is never consulted |
| F-1060-05 | A station whose veto is already broken makes far **fewer** settle queries per unit of audio than a healthy one (measured: 159 in 45 s on the 250 Hz capture against 963 in 20 s on the 500 Hz one, same budget), because it spends the scan budget decoding the noise it wrongly corroborates. So the station that most needs calibrating is the slowest to calibrate. | deferred — recorded on #1060; it sets the cost of the production-path stand-down gate, which is `#[ignore]`d for that reason |
| F-1060-04 | ~~At the 250 Hz-class band BPSK250 decodes 0 of 180 trials, so that configuration cannot work for this mode at all~~ — **FALSIFIED**. A frame decodes cleanly through the same 1400–1600 Hz mask with no fade and no noise (`narrow_mask_decode_check`, and independently by the reviewer at 92 % energy passed). The 0/180 measured **margin erosion under fade and noise**, not impossibility, so a 250 Hz-class station on a calm channel is a live configuration — and the stand-down rule's motivating case survives. | fixed — the reading is retracted here and on #1060 |
| F-1060-03 | The design as reviewed fed the calibration from a bounded-rate all-windows sampler; the implementation feeds it from the veto's own query stream instead (zero extra correlation, population-matched by construction). A deviation from the reviewed design, accepted on review, but it arrived without decoded-span exclusion. | fixed — thinning, hysteresis and a poison-direction gate added; exclusion deferred as F-1060-02 |
| F-1060-02 | Retroactive exclusion of samples belonging to spans that later decoded is specified in the design review but not implemented: with a median anchor, rare delivered frames cannot move the estimate. | deferred — revisit if a high-duty-cycle deployment appears (#1060) |
| F-1060-01 | The delivered-frame ρ bound used for stand-down is a min-of-60 on one channel model with a brick-wall mask, at one band. | deferred — the bound ships marked provisional; a decode-conditioned measurement across bands is tracked in #1059 |

## Requirement enforcement

Both requirements are `status: draft`, `traceability: enforced`. Enforced is deliberate and was not
the default: the checker treats an **absent** `traceability` field as warn-only, while
`requirements.yaml`'s own note says new requirements default to enforced — so a requirement added
without the field silently gets no MISSING-BINDING or CITED-BUT-DIDN'T-RUN enforcement at all. That
contradiction predates this work and is a checker seam worth its own issue.

REQ-RX-03 is bound to the **in-suite mechanism tests**, not to the `#[ignore]`d production-path one:
a binding on an ignored test fails the did-it-actually-run arm, and would be a citation to a run that
never happened. `draft` + shipped code will fail `trace.sh check --release` as DRAFT-SHIPPED, which is
correct — ratification waits for the budget-fixed delivered-frame measurement.

## Verification

| Objective | Gate | Result |
|---|---|---|
| The calibration is fed by the **production** receive path (tripwire) | `rho_calibration_receive::the_calibration_is_fed_by_the_production_receive_path` | pass — 98 samples from a 20 s replay of the 500 Hz capture |
| A narrower receive filter raises the threshold, and a wide one does not | `rho_calibration_receive::a_narrow_receive_filter_raises_the_threshold_above_the_published_constant` | pass — 0.449 derived at 554 Hz, 0.400 held at 2470 Hz |
| Stand-down engages on a real narrow-filter capture, and is observable (run by `scripts/onair-preflight.sh`, not the unit gate) | `rho_calibration_receive::the_veto_stands_down_when_no_threshold_separates` | pass, `#[ignore]`d for cost (628 s at a 12 000/4 000 budget — see F-1060-05): 96 samples, derived **0.618**, stood down, 146 settles let through in that state |
| Below `MIN_SAMPLES` the published constant is returned unchanged | `rho_calibration::tests::below_min_samples_...` | pass |
| A quiet station is never weakened below the published floor | `rho_calibration::tests::a_quiet_station_is_never_weakened...` | pass |
| A narrow filter's derived level clears its measured noise ceiling | `rho_calibration::tests::a_narrow_filter_raises_the_threshold_above_its_measured_noise_ceiling` | pass |
| The median anchor is not moved by a *rare* frame | `rho_calibration::tests::the_median_anchor_is_not_moved_by_frames_in_the_stream` | pass |
| Heavy undecodable traffic drives **stand-down**, not over-vetoing | `rho_calibration::tests::heavy_undecodable_traffic_drives_stand_down_rather_than_over_vetoing` | pass |
| Overlapping queries over the same audio are thinned | `rho_calibration::tests::overlapping_queries_over_the_same_audio_are_thinned` | pass |
| The stand-down latch has hysteresis | `rho_calibration::tests::the_stand_down_latch_has_hysteresis` | pass |
| BPSK250 still decodes through a 200 Hz brick-wall on a clean channel (F-1060-04) | `narrow_mask_decode_check` | pass, with a 500 Hz control that caught an apparatus bug first |

**Sabotage verification** (each mechanism broken deliberately, gate watched to fail):

| sabotage | gates that went red |
|---|---|
| `effective_threshold` returns the published constant | 4 — the two raise gates, the poison gate, the stand-down gate |
| hysteresis removed from the latch | `the_stand_down_latch_has_hysteresis` |
| thinning removed from `push_at` | `overlapping_queries_over_the_same_audio_are_thinned` |
