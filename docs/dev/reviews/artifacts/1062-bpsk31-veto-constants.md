# #1062 design pass — can BPSK31 publish its own ρ constants today, and what does the PN column need?

**Status:** proposal, unimplemented. Sent for adversarial review before any code is written.

My instinct is that **route A below is shippable on evidence already in the tree** and that route B
is a measurement blocked behind a question nobody has asked yet. Test both instincts rather than
confirming them, and flag anything wrong or unproven in the framing — particularly the sample-size
argument in A, which is the same shape that withdrew #1053.

## Consumer

`ModulationPlugin::preamble_template` → `ModemEngine::build_preamble_veto`, reached on the **daemon's
production capture path**, verified by reading the chain rather than grepping for the name:

```
accumulate_capture          crates/openpulse-modem/src/engine.rs:2125   (daemon rx_ticker entry)
  -> ota_decode_burst                                          engine.rs:2585
  -> ota_decode_and_ack_inner                                  engine.rs:2639
  -> acquire_burst_correction                                  engine.rs:2857  (call site)
  -> build_preamble_veto                                       engine.rs:2427
  -> BpskPlugin::preamble_template                    plugins/bpsk/src/lib.rs:176
```

Also reached from `scan_burst_onsets` (engine.rs:2530, itself called from `decode_burst_phase1`
engine.rs:2315) and from `receive_with_timeout_fec_inner` (engine.rs:3755).

BPSK31 is `hpx_hf` **SL2 and its `initial_level`** — the rung every session starts on — and is in the
OTA candidate set. It publishes no template today (`DERIVED_FOR = "BPSK250"`,
plugins/bpsk/src/lib.rs:187), so frame start on that rung is **energy-only**, which is the #1021 /
#1045 class the veto exists to remove.

## Prior art

Swept before proposing anything; every hit changes the proposal.

- **A candidate-preamble seam already exists in the BPSK plugin**, end to end, and I was about to
  propose building one:
  - `bpsk_modulate_with_preamble(data, cfg, &[bool])` — plugins/bpsk/src/modulate.rs:101
  - `bpsk_demodulate_with_expected(samples, cfg, &expected)` — plugins/bpsk/src/demodulate.rs:186,
    with `preamble_syms = expected.len()` (:194) so it is **length-agnostic**
  - `afc_estimate_hz_with_expected` — demodulate.rs:303/309
  - `find_timing_offset_with_expected`, `expected_symbols_for`
  - exercised end-to-end already: `crates/openpulse-modem/tests/demod_parity.rs` column E
    (`bpsk_modulate_with_preamble` → channel → `bpsk_demodulate_with_expected`), and
    `plugins/bpsk/tests/preamble_seam_identity.rs`
  - **explicit refusal on the -RRC path**: demodulate.rs:91 errors rather than honour a candidate
    preamble, so any PN column is Hann-path only.
- **A wrapper `ModulationPlugin` registered into a real engine is an established pattern**:
  `NoVetoBpsk`, crates/openpulse-modem/tests/preamble_rho_fade_and_filter_probe.rs:1303 — forwards
  everything, overrides `preamble_template` to `None`.
- **PN generators already shared**: `common::preamble::{pn_template, m_sequence_of_len}`,
  crates/openpulse-modem/tests/common/mod.rs:77-104, balance-checked, built at the plugin's own
  symbol rate (`the_pn_generator_uses_the_shipped_symbol_rate` asserts it in the default gate).
- The per-mode constants question is already answered by the **type**: `PreambleTemplate::new(mode,
  samples, threshold, grid)` carries threshold and grid per instance, so no mode can inherit
  another's (CLAUDE.md acceptance row, #1053's outcome). What is missing is only the table.

## Twins

- **BPSK63 and BPSK100** — same class, same blocked state, same measurement shape. R1–R4 measured
  **BPSK31 only**, so this proposal covers one of three and says so. baud/4 is 15.6 Hz at BPSK63 and
  25 Hz at BPSK100, so their grids differ and neither inherits BPSK31's.
- **QPSK (#1059)** — does **not** carry over. Its 16-symbol preamble is a *designed* sequence with
  all four constellation points 4× each, not an alternating run, so the `baud/4` line argument is
  BPSK-only (stated at plugins/bpsk/src/lib.rs:199).
- **The duplicated round number** — `r6_what_decimation_costs_the_noise_ceiling` (line 502) and
  `r6_does_a_pn_template_remove_the_grid_constraint` (line 896) are **both R6** in one file, and R7's
  doc comment says "R6 then found the noise ceiling rising by more", which now reads ambiguously.
  Mechanical rename to R11; out of review scope, listed so it is not lost.

## Route A — publish BPSK31's own constants, no wire change

The constants R1–R4 already measured, all `#[ignore]`d in
`crates/openpulse-modem/tests/bpsk31_constant_derivation.rs`:

| column | source | BPSK31 result |
|---|---|---|
| grid, false-accept | R1 / R11 | ±2 Hz: worst tone ρ **0.696**, 8 % of the ±60 Hz axis over 0.40 (±20 Hz: 0.700, 47 %) |
| grid, false-reject | R2 | ρ holds across the residual the settle leaves (≤ 0.3 Hz measured on BPSK250) |
| noise ceiling | R3 | **0.319** wide-filter, **0.426** at 60 Hz; white / SSB / 500 Hz **byte-identical through the DDC** |
| decode | R4 | 48 seeds, `moderate_f1` @ 3 dB, 40 decoded, weakest decodable ρ **0.625**, next 0.652/0.687/0.688/0.692 |

The change: turn `preamble_template`'s single `DERIVED_FOR` string into a two-entry per-mode table,
adding `BPSK31 -> { threshold ~0.51, grid 2.0 }`. The existing mechanical `baud/4` guard
(plugins/bpsk/src/lib.rs:196) then passes on its own terms: 2.0 < 31.25/4 = 7.81.

**Why I think it is safe:** 0.426 → 0.625 is a 1.47× gap and 0.51 sits 1.20× above the narrow-filter
ceiling and 1.22× below the weakest decodable frame. For contrast the **shipped BPSK250 position is
worse** — its measured 500 Hz-filter idle ceiling (0.441, real IC-9700, 45 s) *exceeds* its own 0.40
threshold. And #1053's specific killer does not reach BPSK31: there the decodable and noise
distributions **overlapped** (QPSK250-D decoded to ρ 0.276, below its own 0.291 noise ceiling),
whereas BPSK31's separate. BPSK31's 7936-sample template also exceeds
`MAX_PREAMBLE_CORRELATION_SAMPLES`, so it runs the **DDC**, whose anti-alias stage makes the noise
ceiling band-independent (R3) — the receive-filter axis that falsified #1053's table is closed here
by construction, not by assumption.

**What I think is weakest, and want attacked:**

1. **R4 is 48 seeds, one SNR, one channel model, one payload.** Its own doc records that 6 seeds gave
   0.693 and 48 gave 0.625 — "the small sample flattered the bound". A 96-seed run could plausibly
   land under 0.51 and falsify the threshold outright. I do not think 48 is enough to publish on, and
   the cheap-looking answer (publish and see) is the #1053 shape.
2. **0.51 is a number I picked between two measurements, not a derived constant.** Every DSP constant
   here is supposed to arrive with "what inventory was this fitted to, and what would falsify it" —
   0.51 was fitted to exactly two points from one run.
3. **The 8 % vulnerable axis is a real residual I am proposing to accept.** Justified by parity with
   the shipped BPSK250 posture, which is an argument from precedent, not from safety. A birdie inside
   one of those bands would corroborate a non-frame on the rung every session starts on.
4. **The #1157 CFAR interacts and I have not traced it.** `derived = max(published, anchor × 1.8)`
   and the veto stands down when `derived > DELIVERED_FRAME_RHO_BOUND`. That bound is a **BPSK250**
   0.50. Publishing BPSK31 without its own bound would hand it a borrowed one — the exact practice
   this whole guard exists to prevent. R4's weakest decodable 0.625 is the natural candidate, from
   the same 48-seed sample as (1).

**So my actual recommendation is: widen R4 first** (more seeds, and 3 dB plus at least one higher
cell), and publish only if 0.51 survives with a bound derived in the same run. Cost is the reason
this has not happened: `Rs` makes a BPSK31 frame ~65 s of audio against a 180 s receive timeout, so a
failed decode burns the timeout and a 96-seed two-cell run is hours.

## Route B — the PN decode column, and what it is really blocked on

R11 confirmed the issue body's prediction on the **false-accept** side at equal length and equal
decimation: shipped 0.700 / 47 % vulnerable versus **PN-31 0.332 / 0 %**, flat across grid widths
where the shipped template's response is not. The open columns are R2's false-reject side and R4's
decode column for a PN preamble.

I expected the decode column to need a wire-format change. **It does not** — the seam under *Prior
art* means a `PnBpsk` wrapper plugin gets a PN preamble onto the wire and back off it with **zero
production changes**, registered into a real `ChannelSimHarness` engine so `FecMode::Rs` and frame
validation run exactly as R4 runs them.

Two specifics that decide whether that is sound:

1. **PN must live on the SYMBOLS, not the bits.** `bpsk_baseband_with_preamble` takes bits and calls
   `nrzi_encode` (modulate.rs:57-70), and the template correlates against the wire, i.e. symbols.
   Feeding an m-sequence as *bits* puts NRZI(PN) on the wire, which is not PN and does not have PN's
   autocorrelation. The inverse map is `bits[k] = (sym[k] != sym[k-1])` with the modulator's initial
   state. **This is the fourth time this issue has offered this exact trap** — `f12` hand-wrote an
   alternating chip run for "the shipped sync word's structure" and measured a template correlating
   **0.035** with the wire, and two written conclusions rested on it before a reviewer caught it. So
   the mitigation is mechanical and runs by default: assert the template recovered from
   `bpsk_modulate_with_preamble(…, &pn_bits)` correlates ρ > 0.999 with
   `common::preamble::pn_template(…)`, mirroring `f12_synthesised_template_matches_the_shipped_one`.
2. **The wrapper must override more than `modulate`/`demodulate`.** `estimate_afc_hz` forwards to
   `afc_estimate_hz` (shipped expectation) and would estimate frequency on PN audio against the
   wrong sequence; `frame_geometry.preamble_samples` would be wrong; `demodulate_soft` and the -RRC
   path have no candidate-preamble form at all (demodulate.rs:91 refuses it). I believe the correct
   scope is **Hann path, hard demod, `estimate_afc_hz` overridden to the `_with_expected` form**, and
   that `demodulate_soft` should error rather than silently use shipped expectations.

There is also a scope note from `f9` that I think this design satisfies where a prepended template
would not: "a prepended candidate template would be extra audio in front of a frame carrying its own
preamble, and conditioning on that frame's decode would answer a different question". With
`bpsk_modulate_with_preamble` the PN sequence **is** the frame's preamble, so ρ and decodability are
properties of one object and conditioning is meaningful.

## Questions I want answered

1. Is route A's 48-seed decode column sufficient to publish a production threshold, or is widening it
   first the only defensible order? If widening — what size and which cells actually bound the claim,
   given that the quantity is a *minimum* over a sample?
2. Is "0.51 between two measured points" a legitimate derivation, or is it the
   artifact-calibrated-constant archetype with two fixtures instead of three?
3. Is accepting an 8 %-of-axis tone vulnerability on `hpx_hf`'s entry rung defensible on parity with
   BPSK250, or does it need its own argument?
4. Does publishing BPSK31 require deriving its own `DELIVERED_FRAME_RHO_BOUND` in the same change,
   and is R4's weakest-decodable the right estimator for it given it is a min-of-48?
5. For route B, is a wrapper plugin at the existing seam genuinely equivalent to a wire change for
   the purposes of a decode column, or does the override list above leave something that makes the
   comparison to R4's shipped column invalid?
6. Which order? A-then-B, B-then-A, or is one of them not worth doing at all?

## Prompt

Three messages were sent, in order. **The first is reproduced as sent. The second and third are
condensed** — their tables folded into prose and repetition cut — with no question or claim added or
removed. Two figures in the second were later found wrong; each is marked `[sic — …]` in place rather
than silently fixed.

### 1. Initial request

```text
You are reviewing a design proposal in the OpenPulseHF repository at /home/dc0sk/git/OpenPulseHF (Rust HF software modem). Your job is to FALSIFY it, not to confirm it. Read the actual code and the actual measurement harnesses; do not accept the proposal's summaries of them.

Read this first, in full:
    docs/dev/reviews/artifacts/1062-bpsk31-veto-constants.md

Then read the repo's own rules, which set the bar you are holding this to:
    CLAUDE.md — especially "Adversarial review (standing rule)", "Verification mechanics (mandatory)",
    and the sharp edges about the #1053 QPSK threshold withdrawal and the artifact-calibrated-constant
    archetype.

Primary source material, which you should read rather than trust the proposal about:
  - crates/openpulse-modem/tests/bpsk31_constant_derivation.rs   (R1-R11; the R2/R3/R4 doc comments
    carry the recorded measurement tables the proposal quotes)
  - plugins/bpsk/src/lib.rs:150-230                              (preamble_template, DERIVED_FOR, the
                                                                  mechanical baud/4 guard)
  - plugins/bpsk/src/modulate.rs:200-350                         (PREAMBLE_RHO_THRESHOLD,
                                                                  DELIVERED_FRAME_RHO_BOUND,
                                                                  PREAMBLE_RHO_GRID_HZ doc derivations,
                                                                  bpsk_modulate_with_preamble, nrzi)
  - plugins/bpsk/src/demodulate.rs:40-200, 280-340               (the candidate-preamble seam and its
                                                                  -RRC refusal)
  - crates/openpulse-modem/src/engine.rs                         (build_preamble_veto ~6906, and the
                                                                  consumer chain the artifact claims:
                                                                  2125, 2585, 2639, 2857, 2427, 2530, 3755)
  - the #1157 CFAR runtime calibration (grep for the anchor/median/1.8 logic and for
    DELIVERED_FRAME_RHO_BOUND's readers) — the artifact admits it has not traced this
  - crates/openpulse-modem/tests/preamble_rho_fade_and_filter_probe.rs — f9's scope note about
    prepended vs. own-preamble conditioning, and NoVetoBpsk (~line 1303)

You may run targeted greps and read files freely. Do NOT run the workspace gate or any of the
#[ignore]d measurement harnesses (they cost 35-80 minutes each). If a claim can only be settled by
running one, say so explicitly and say which.

Answer the six numbered questions at the end of the artifact, and independently attack these, which
are the places I most expect the proposal to be wrong:

1. Does the claimed consumer chain actually hold? Verify each hop yourself. In particular: does
   BPSK31 actually reach build_preamble_veto in the OTA/daemon path, or is there a gate earlier
   (a candidate-rung set, a mode filter, a template-length cap, MAX_PREAMBLE_CORRELATION_SAMPLES)
   that means publishing BPSK31 constants would change nothing or something different?
2. Is the "DDC makes the noise ceiling band-independent, so #1053's receive-filter axis is closed by
   construction for BPSK31" argument sound? This is the load-bearing asymmetry in the proposal's
   safety case. Check what R3 actually measured and whether "byte-identical" means what the proposal
   says it means, and whether the real IC-9700 narrow-filter result (rho 0.413 at 500 Hz, 0.579 at
   250 Hz, measured on the rig) contradicts it.
3. Does publishing a second mode actually work given the CFAR? Trace what reads
   DELIVERED_FRAME_RHO_BOUND and PreambleTemplate's fields at runtime. If BPSK31 gets a 0.51
   threshold and inherits a 0.50 delivered bound, what does the runtime do — stand the veto down
   immediately (making the change inert), or run it? An inert change is a different finding from an
   unsafe one.
4. Is route B's wrapper-plugin equivalence real? The override list is the risk. Enumerate every
   ModulationPlugin method the engine calls during acquisition and decode on the path R4 uses
   (ChannelSimHarness + receive_with_fec_mode_timeout), and say which ones would silently use the
   SHIPPED preamble against PN audio. Also check whether the engine's own acquisition stages
   (refine_onset, afc_mini_settle, the scan planner) use anything preamble-shaped that a plugin
   wrapper cannot override.
5. Is the proposal's recommended ORDER right? Consider whether there is a cheaper falsifier for
   route A than a wider R4 — something that could kill it in minutes rather than hours.

Finally: flag anything wrong or unproven in my framing, including anything I have asserted as
established that the code does not support, and any question I should have asked and did not.

Return a structured verdict: for each of the six artifact questions and each of your five attack
points, state CONFIRMED / REFUTED / UNCERTAIN with the file:line evidence you actually read, then a
short overall recommendation on what to do next and in what order. Be specific and concrete; do not
hedge with generic advice.
```

### 2. Follow-up, sent while the review was running (F1–F3)

```text
Additional material, found while you were reviewing. Three findings, two of which bear directly on route A's safety case. **Check my arithmetic and my reading — do not take these as established.** If any is wrong, say so plainly; I would rather retract now than after it reaches the issue.

F1 — R2's carrier-offset fixture is amplitude modulation, not a frequency shift. `bpsk31_constant_derivation.rs:167-172` applies the residual offset as `s * (2.0 * PI * residual * k as f32 / FS).cos()`. Multiplying a real bandpass frame by `cos(2πΔt)` is DSB-AM: copies at `fc−Δ` and `fc+Δ`, each at half amplitude; the grid can rotate onto only one while the other sits in ρ's denominator, so ρ is capped near 1/√2. Numerical check with a positive control (synthetic `--++` template, ±20 Hz grid at 0.5 Hz): R2's fixture 1.000 / 0.998 / 0.784 / 0.704 / 0.704 / 0.704 at Δ = 0 / 0.1 / 0.3 / 1 / 2 / 5 Hz, against a true offset 1.000 / 0.937 / 0.760 / 1.000 / 1.000 / 1.000. The real harness has printed 0.998 and 0.788 identical across grid half-widths spanning 40× [sic — later corrected to 9× in hypothesis count]. R2 has no RESULT block, so nothing published rests on it [later found false: see the Correction section] — but my artifact cited it as evidence.

F2 — the engine's residual-frequency grid step is D× too coarse on the DDC path, and route A is what activates it. `engine.rs:6993`: `let step = (0.25 * fs / tlen as f32).max(0.5);` with `tlen = veto.filter.len()` — the quarter-cycle criterion, correct only when `tlen/fs` is the duration. `DdcMatchedFilter::len()` returns the decimated length, so the step comes out D× too coarse: BPSK31 1.008 Hz [later corrected to 1.0246] vs the intended 0.252; worst-case coherent loss |sinc(ΔT)| 0.637 against the criterion's 0.900. Grid values are not rescaled downstream (`search_normalized_over_frequency` mixes at `center_hz + f` at the full sample rate). Latent: BPSK250 (992 samples) is passband. The test's own `grid_for` computes the physically-correct step; the sibling probe's `engine_grid` is faithful but only used on the passband arm. Attack: (a) is `.max(0.5)` a deliberate floor that makes the D-scaling moot? (b) a compensating factor I missed? (c) is |sinc(ΔT)| the right loss model for a `--++` template? (d) has any mode ever been on the DDC arm in production?

F3 — this answers my own question 5: if F2 holds, the cheaper falsifier for route A is five lines of arithmetic, and the grid fix plus its regression test come first.

Please fold these into your verdict: CONFIRMED / REFUTED / UNCERTAIN for each of F1/F2/F3 with the lines you read, and revise question 6 (ordering) if F2 holds. Keep attacking the original six as well.
```

### 3. Second follow-up (retracting attack point (c))

```text
Correction and sharpening of F2 — please drop attack point (c), I have resolved it against myself. I had used a real-valued dot product, which conflates carrier phase with frequency offset; the repo's ρ is a complex matched filter. Redone with a complex correlator, measured ρ matches |sinc(ΔT)| to three decimals, so my earlier 0.760 was instrument error. F2's severity therefore has a closed form: the engine's grid is `settled_hz + k·step`, worst-case residual `step/2`, `step = 0.25·fs/tlen_dec`, `T = tlen_raw/fs`, `D = tlen_raw/tlen_dec`, so worst-case ΔT = 0.125·D and worst-case loss = |sinc(D/8)|: 0.974 at D=1, 0.900 at 2, 0.637 at 4, 0.000 at 8 — and engine.rs:398 contemplates a 16 128-sample template, which is D=8 at this budget. Also verified, for you to check rather than take: (d) `veto_membership_pin.rs:59` pins `MODES_WITH_VETO = ["BPSK250"]` with a positive control, so no mode has been on the DDC arm in production; and the pin's own checklist (:53-58) names the threshold and grid half-width but not the grid step the engine derives. Still want your verdict on the original six plus F1/F2/F3.
```

Four later write-up reviews checked the PR body, the commit messages, the ledger entry and this
file's correction section. Their findings are applied in place and summarised in the Correction
section below; the last of them read #1062's full thread.

---

## Verdict (Fable, 2026-09-11) — route A cannot ship, and not for the reason I proposed

Recorded here rather than summarised elsewhere, because the proposal above contains errors a later
reader would otherwise inherit. **Four framing errors of mine, corrected:**

1. **"Frame start on that rung is energy-only, the #1021/#1045 class."** True on the CLI path
   (`receive_with_timeout_fec_inner`, engine.rs:3641) and **false for the production consumer I named
   in the Consumer section**. On the daemon path the veto gates only *phase-2 acquisition*, reached
   only after every candidate failed at every onset at the current correction (engine.rs:2814-2822);
   frame start is the DCD/burst accumulator (:2137-2145), and the phase-2 correction is committed
   only on decode success (:2858-2862). I named the right function and the wrong consequence. The
   honest daemon-path benefit of route A is small: off-frequency phase-2 acquisition on SL2.
2. **"Also reached from `scan_burst_onsets` (engine.rs:2530)"** — that call site is **unreachable**,
   though the function itself is live: all three callers pass `settle = false` (:2321, :2359, :2381),
   so its veto-building arm — the `build_preamble_veto` call — never runs.
3. **"R2: ρ holds across the residual the settle leaves."** R2 had never been run, its fixture cannot
   measure it (F1), and BPSK31's own settle residual is unmeasured — the `≤ 0.3 Hz` is BPSK250's.
   **That row of my table is struck.**
4. **"Byte-identical through the DDC … #1053's receive-filter axis closed by construction."** The
   byte-identity is a property of the *fixture*: `band_noise` (:256-291) draws one stream per seed,
   FFT-masks it and renormalises to fixed RMS, so the bands share in-band bins up to a scalar and any
   lowpass narrower than the narrowest mask makes a *ratio* identical by construction. The physical
   claim that survives is narrower: the realised DDC response (129-tap Hamming sinc, two-sided ENBW
   ≈ 95 Hz at grid 2.0) is narrower than any rig filter ≥ ~250 Hz, so those do not change what the
   correlator sees. Still open: filters under ~100 Hz, and coloured in-band noise (50/100 Hz hum
   sidebands sit on the −16 dB skirt, not the stopband). And the DDC does not make the ceiling *low*
   — it makes it permanently the *narrow-filter* ceiling (BPSK31 passband 0.066 → DDC 0.326).

**Two findings I did not have, both of which outrank the sample-size question I asked about:**

- **R4's ρ is not the engine's ρ**, so widening it tightens the wrong quantity. R4 scores at the
  *true* onset, on a grid centred at *zero* residual, at 0.252 Hz. The engine scores at the settle's
  onset, centred on `settle.fine`, at the plan's step. So 0.625 is an **upper bound**: the operative
  value is `0.625 × |sinc(Δ·T)|` for Δ the settle's distance from the nearest grid point. R4 also ran
  through `route()`, the buffer-is-the-frame fixture with no lead-in.
- **`RhoCalibration` is engine-wide, not per-mode** — one `VecDeque`, no mode key
  (`rho_calibration.rs:84-94`, single field at engine.rs:839), fed from whatever mode is being
  acquired (:2492, :4219). In the OTA phase-2 loop the candidate list is the rungs **plus the active
  mode as an uncoded fallback**, so a BPSK31 DDC stream (ceiling ~0.33) and a BPSK250 passband stream
  (p50 ~0.13–0.24) would land in one median, ×1.8, compared against each mode's own bound. The CFAR's
  stated premise — "the stream this compares against is the stream it is built from" (:4216-4218) —
  is false the moment a second template exists. Not inert: it *runs* at the published threshold on
  wide-filter stations and *stands down* on narrow ones, station-dependently.

Also: the operative threshold is `max(published, p50 × 1.8)`, so **R3's median is the number that
decides**, and neither R3's printout nor my table records it. My 0.51 was fitted between a
max-or-p99 of one synthetic seed and a min-of-40 of an idealised ρ — two incommensurable fixtures,
which is the artifact-calibrated-constant archetype with the usual two points.

**F1/F2/F3 all CONFIRMED**, F3 more strongly than I put it: a PN-63 preamble at BPSK31 is 16 128 raw
samples → `by_budget` = 8 → `|sinc(1)| = 0`, a complete worst-case null, so F2 blocked **route B**
too. Independently reproduced: AM fixture 0.998 / 0.781 / 0.708 / 0.708 at Δ = 0.1 / 0.3 / 0.5 / 1.0;
`|sinc(D/8)|` loss 0.635 measured against 0.637 predicted.

### Agreed order (superseding both routes above)

1. **Fix the grid step** — derive from the template's span, with a test that fails on current code.
   Decide the `.max(0.5)` floor explicitly: it does not bind today and *will* bind after the fix
   (0.256 → 0.5 Hz), leaving |sinc(0.25)| = 0.900, the criterion's own edge. **DONE** on
   `fix/1062-veto-grid-step-domain`, the branch carrying this artifact: floor kept, both halves of
   the gate sabotage-verified to fail independently.
2. **Make the harnesses faithful by reference** (CLAUDE.md rule 5): `grid_for` and `cutoff_and_decim`
   assert equality against the engine's plan in the default gate, or move ρ scoring into an engine
   unit test beside `ddc_veto_arm`. Fix R2's fixture and add a BPSK31 settle-residual measurement.
3. **Send the `RhoCalibration` per-mode question to review before any code**, and add the grid step
   and the CFAR stream to `veto_membership_pin.rs`'s "To add one" checklist — it names only the
   two constants that travel *with* the template.
4. **Re-run R3 alone and record p50**, the operative number.
5. **Only then** decide route A vs B. Expectation from what is in the tree: B is worth more (it
   removes the line bands F2 cannot), A's honest daemon-path benefit is small, and neither should be
   re-measured until 1–2 are merged.

---

## Correction (2026-09-11, after reading #1062's full thread) — the proposal's premise was stale

The proposal above was drafted from `bpsk31_constant_derivation.rs`'s R4 doc comment (48 seeds,
weakest decodable 0.625) and the last two comments on #1062. The thread had recorded more, on
2026-08-04 ("Phase-0 result: BPSK31 publishes no preamble template"):

- **R4 at 150 seeds, 118 decoded, weakest decodable 0.569.** The pre-registered rule
  (`T ≥ 1.15 × 0.426 = 0.490`, `T ≤ 0.85 × 0.569 = 0.484`) admits no threshold, and BPSK31 publishes
  `None`. **Route A was closed a month before this proposal**, for the reason the proposal itself
  named as its weakest point.
- **R3's median and p99** — 0.198 / 0.301 (white, SSB, 500 Hz) and 0.262 / 0.393 (60 Hz): the numbers
  the verdict's item 4 asked to record. p99/p50 = 1.52 / 1.50, under `FAMILY_FACTOR`'s 1.8 — on one
  synthetic fixture, at the probe's 0.252 Hz grid.
- **Route B at BPSK31 is argued against, and formally open.** The Phase-0 comment (14:04) and
  revision 3 (14:22) argue a same-bandwidth spread sequence cannot give the BPSK31-class rungs a
  threshold either: the noise ceiling is sequence-invariant when the noise covers the template's band,
  and the decode edge is set by the preamble's temporal locality in a fade. Revision 4 (16:40) takes
  that as established and adds that onset placement is unavailable without a template. The 18:50
  comment falsifies a draft asserting the same conclusion on partly-wrong grounds and declares
  derivability for a PN candidate at 31/63 "open" on the grid and tone obstacles — without engaging
  the two-column argument. The thread never reconciled the two.
- **The ±2 Hz grid derivation's false-reject half** was "a real frame holding ρ ≥ 0.704 out to a 2 Hz
  residual" — R2's AM-fixture cap (F1), already in the harness's first commit on the
  branch that comment names (`22828192`, four hours earlier). That half is withdrawn; R1's
  false-accept half stands.

**What survives:** F1 (R2's fixture), F2 (the grid step, fixed on the branch carrying this artifact),
the `RhoCalibration` finding, and the verdict's corrections to my Consumer section. **What does not:**
the agreed order's items 4 and 5 as written — item 4's numbers were already on the thread, and item 5
("route A vs B") is closed for A and argued against for B at BPSK31. Items 2 and 3 stay correct as
latent-defect work, without the urgency their "before route A or B" framing gave them.

**Number corrections to the verdict above:** the old step is 1.0246 Hz, not 1.008 (the decimated
template is 1952 samples, spanning 7808, because `ddc_mix` keeps only the FIR's valid region); a PN-63 preamble at BPSK31 length is 15 872 raw samples — `pn_template` spans chips − 1 symbols, as the
shipped 31-symbol template does for 32 bits — so decim is 8 and the null stands; and the fix
roughly doubles the DDC arm's hypothesis count — the 0.5 Hz floor caps it — rather than multiplying
it by `decim`.
