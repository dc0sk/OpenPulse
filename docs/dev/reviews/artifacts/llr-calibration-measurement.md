---
project: openpulsehf
doc: docs/dev/reviews/artifacts/llr-calibration-measurement.md
status: review
last_updated: 2026-09-13
---

# Review — LLR bin calibration for bpsk, qpsk and psk8

A measurement review: three findings went out, **two came back falsified**. Both were artefacts of my
own apparatus, and neither reached an issue.

## Prompt

Sent to Fable as a falsification request with the full apparatus, opening with "my dominant failure
mode in this repo is reading a measurement-setup artefact as a property of the signal, so attack the
apparatus first" and closing with "flag anything wrong or unproven in my framing".

Supplied: the method (the `llr_reliability` binning, with quintile bins because ofdm's fixed edges
left most bins empty and every ratio read 0.00); the alignment control comparing
`fec::hard_decide(demodulate_soft(rx))` against `demodulate(rx)` per draw; the instrument controls
(a synthetic source calibrated by construction, and two plugins already bounded at 4× by their own
tests); and the three candidate findings — 8PSK500 grossly over-confident, QPSK250 suggestive but
under-sampled, BPSK fine on calibration but its soft and hard paths disagreeing under noise.

Asked specifically: (a) is the estimator legitimate for these plugins at all, given that the trait's
*Scale* paragraph requires only monotonicity while its *Calibration* paragraph claims true LLRs —
"the two appear to me to say different things, and I may be picking the one that makes my result
interesting"; (b) is the 4× bound the right comparator at an SNR I chose to produce errors; (c) is
the BPSK soft/hard divergence a defect or expected; (d) attack the apparatus — whole-buffer AWGN,
quintile binning, 8 draws of one fixed payload, and "a ratio of 5×10⁴ against a predicted probability
of ~2e-7 may be arithmetic on a number too small to estimate"; (e) anything else unproven.

## Verdict

**Apparatus sound and deterministic — both headline numbers reproduced to the last digit — but the
interpretation wrong in three ways, and the ranking inverted: the only finding on a shipped path was
the one I ranked last.**

- **QPSK250's 24.9× — RETRACTED.** Jensen's inequality in my own binning. The bottom quintile spanned
  `|L| ∈ [0.01, 9.99]` and held all 71 errors; `p` is convex in `|L|`, so `p(mean|L|)` is not the mean
  of `p_i`. Bin-mean prediction with random payloads: **1.28**. Inside the bound at every SNR that
  produces errors. **The reference `llr_reliability.rs` tests share the flaw** and overstate
  over-confidence by up to ~7× in their `[8,16)` bin — conservative, so not a false pass.
- **8PSK500's 52 576× — RETRACTED.** Frame-level mixing. Per-draw BER at 4 dB was
  `0.034 0.045 0.032 0.039 0.043 0.256 0.039 0.033`; one draw carried 522 of 1063 errors, its
  high-`|L|` errors 99 % adjacent in runs with two wrong Gray bits dominant — 90° rotations landing
  on the wrong constellation point, i.e. a decision-directed carrier tracker losing lock below its
  cliff. No σ² estimator can see that: inside a slipped run the orthogonal residual is *small*. Above
  the cliff, 8PSK500 is calibrated — global ratio 1.23 at 6 dB, 1.00 at 8 dB, under-confident above.
- **BPSK — KEPT, and re-ranked first.** The soft path deliberately skips `cancel_crossfade_isi`
  (#832) while the hard path applies it. 15× worse raw BER at 0 dB with a **4× asymmetry toward flip
  bits on the soft path and none on the hard path** — the fingerprint of the uncancelled `+β`, whose
  cost the hard path's own comment states. Sequence-dependent, therefore identical in every
  retransmission, therefore `combine_llrs_map` reproduces it exactly while noise shrinks by √N. Filed
  as **#1361**.

## Premise corrections

- **"bpsk/qpsk/psk8 carry `hpx_hf` SL2–SL9" is false** — asserted by both of us. `profile.rs:379-393`:
  SL2–SL5 are BPSK + `Rs`; **SL6 is `QPSK250-D`, which has no soft path**; SL7–SL14 are OFDM.
- **No shipped session ever sums a psk8 LLR.** Plain `8PSK500` is on no coded rung; the profiles
  carrying the other psk8 modes are `fec_modes: [None; 21]`, and the HARQ arm admits only
  `SoftConcatenated | Ldpc | LdpcHighRate | Rs`.
- **BPSK's soft path IS shipped** — `Rs` is admitted to the combine, so every `hpx_hf` SL2–SL5
  retransmission goes through it.
- **On (a): the code claims the strong bar even where the docs only test the weak one.**
  `psk8/src/demodulate.rs` says "calibrate the soft values into *true* log-likelihood ratios" and
  divides by `2σ²`. So the estimator is legitimate against the code's intent; the doc conflation is
  real and independent of the result. Corrected in this change.
- **On (e): "invisible to frame-success metrics" was wrong here.** The 8PSK regime I measured is
  6–25 % BER, which every decode gate at that SNR sees.

## Consumer

Who consumes these LLRs in production, by `file:line`.

- **`engine.rs`** — the soft-combine arm admits `SoftConcatenated | Ldpc | LdpcHighRate | Rs`, and
  `combine_llrs_map` sums LLRs as probabilities. This is the only place LLR *magnitude* (rather than
  sign) changes an outcome, which is why calibration matters at all and why a sequence-dependent bias
  is worse than a noisy one.
- **`hpx_hf` SL2–SL5** (`profile.rs:379-393`) are the rungs whose retransmissions reach it via BPSK.

## Prior art

- The method is `plugins/ofdm/tests/llr_reliability.rs`, copied rather than reinvented — including
  its 4× bound and its justification that max-log-MAP is itself optimistic.
- The failure mode has a precedent in this repo: SC-FDMA's `mmse_llr_noise_var` (#690), where bits at
  `|L| ≈ 12` were wrong 71× more often than promised, on a flat channel, invisible to every decode
  metric. That is the case that makes this class worth measuring.
- `llr_calibration.rs` already existed for the weak bar; nothing new was built to check growth.

## Twins

- **Same flaw, in the reference tests**: all five `llr_reliability.rs` files predict from
  `p(mean|L|)` rather than the bin-mean of `p_i`. Conservative, so they cannot pass a bad plugin —
  but a quoted 3.9× from one of them may be a 0.6×. Not fixed here; recorded so the next reader knows.
- **Same shape, different subject**: `llr_calibration.rs`'s 7-mode hand list is the hardcoded-mirror
  archetype the soft-demod sweep (#1360) just retired for convention coverage.
- **Not a twin**: psk8's lock-loss below its cliff is a carrier-tracking property, already the
  documented reason coherent 8PSK left the HF ladder (#923) — not a calibration question.

## What was NOT filed, and why

No issue on 8PSK or QPSK calibration: both readings were mine, both are retracted. The residual is a
documentation problem, fixed in this change, plus an optional regression pin (extending
`llr_reliability` to psk8 at 6/8 dB with bin-mean prediction and ≥256 random-payload draws), which is
a pin rather than a defect and is not urgent.
