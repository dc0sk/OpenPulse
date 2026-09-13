---
project: openpulsehf
doc: docs/dev/reviews/artifacts/soft-demod-conformance.md
status: review
last_updated: 2026-09-13
---

# Review — sweeping the soft-demod contract over every mode

Design review, before implementation, of replacing `llr_convention_conformance`'s hand-written list
of 14 (plugin, mode) pairs with a derived sweep.

## Prompt

Sent to Fable as a falsification request with the measurements and the apparatus, closing with "flag
anything wrong or unproven in my framing, including premises I did not notice I was asserting".

Given: the census (70 modes declare soft support, the test covers 14), a conformance sweep of all 70
(69 conform, 0 violations, 1 not drivable), an advertisement sweep (66 agree, 1 apparent
disagreement, 6 not modulatable at 8 kHz), and my reading that the one disagreement — `fsk4/FSK4-ACK`
advertising `false` while `demodulate_soft` returns `Ok` — is by design, because the trait documents
the default `false` as meaning "the ±1.0 fallback, no iteration gain".

Asked specifically: (a) is the one-directional invariant right, given that dropping the two-way
equality would lose what the qpsk test asserts; (b) the skip path is where this goes vacuous — a
count floor passes while the same 7 modes are skipped forever, and they are the hardest ones; (c) what
sabotage proves the sweep discriminates, and should it be committed rather than run once; (d) is
convention coverage even the right thing to gate, versus LLR *magnitude* calibration, which no
frame-success metric can see; (e) twins and prior art, so I do not build a second mirror; (f)
anything wrong in the framing.

## Verdict

**Accept the sweep, reject the proposed invariant shape.** Two premises were wrong in ways that would
have made the new gate measure the wrong thing.

1. **The test's slicer was not the product's slicer.** The retired test sliced with `bit = llr <= 0`.
   Production uses `fec::hard_decide` (`is_sign_negative`); `ldpc.rs` and `turbo.rs` use `l < 0.0`.
   Three conventions disagreeing at ±0.0, and the harness pinned a **fourth** that nothing consumes —
   one that disagrees with both production slicers at `+0.0`. A harness that re-implements the
   decision it is checking is this repo's banned construct (verification rule 5). The sweep must call
   `fec::hard_decide` and separately fail on any exactly-zero LLR.
2. **"For every mode where `supports_soft_demod` is true" was the wrong quantifier.** The engine's
   LDPC, turbo and soft-concatenated arms call `demodulate_soft` **unconditionally**, warn when the
   plugin advertises `false`, and feed the result to the decoder. So the contract binds every mode
   whose `demodulate_soft` returns `Ok`, advertised or not — and the proposal would have exempted
   exactly the class where a future override could go wrong unnoticed.

Accepted invariant, two implications rather than an equality:

- **(A)** advertised `true` ⇒ `demodulate_soft` is `Ok`. This is the safety-relevant half:
  `receive_from_samples` treats a soft error on an advertised mode as a terminal decode failure and
  never falls back to `demodulate()`.
- **(B)** `demodulate_soft` is `Ok` ⇒ `fec::hard_decide(llrs)` equals `demodulate()`'s bytes.

`false ⇒ Err` is **not** required; my reading of fsk4 was confirmed. The qpsk test keeps its stronger
equality as a plugin-local documented fact.

## Corrections to my framing

- **The qpsk test did not catch #923's defect.** It was added in `34c1c751` (PR **#996**) in the same
  commit as the fix; the rig found the defect and the test pins it. I had cited it as the catcher.
- **Convention bugs are loud, not silent.** `receive_from_samples` hard-slices `demodulate_soft` on
  every plain receive of a soft-capable mode, so `demodulate()` is effectively dead on the engine path
  for advertised modes and a sign error fails every engine loopback of that mode. That lowers the
  marginal value of (B) — see the ranking below.
- **`MFSK16-ACK` is drivable**, with the 13-byte payload its own layout declares; my 48-byte probe
  made it look undrivable. The real undrivable set is **5**, all `ModemError::Configuration`.
- **Those 5 are unreachable in production, not "the hardest"** — the engine only ever builds 8 kHz
  configs and never sets `pulse_shape`. Filed as **#1359**.
- **The one convention defect that actually shipped is on an axis this sweep does not have**: #1084,
  the psk8 **GPU** soft demodulator emitting per-symbol LLRs bit-reversed. `Plugin::new()` is the CPU
  constructor and the `--no-default-features` gate cannot build the GPU path;
  `plugins/{psk8,64qam}/tests/gpu_cpu_equivalence.rs` is the gate for that arm. Stated as a limit in
  the new test's header rather than implied away.
- Calibration coverage is wider than I claimed: `llr_reliability.rs` exists in **5** plugins, not 3.
  Uncovered for bin calibration: bpsk, qpsk, psk8 — which are `hpx_hf` SL2–SL9.
- Two advertised-true modes (`8PSK1000-HF-RRC`, `QPSK1000-HF`) appear in **no** file under any
  `tests/` directory, and 13 more in exactly one; the filter control was `"BPSK250"` at 102 files.

## Ranking (d)

**Calibration coverage ranks higher, but do this too because it is nearly free.** A convention error
is a total decode failure already gated indirectly wherever a mode has an engine loopback; the sweep's
marginal value is the ~15 thinly-tested modes plus the discriminators. A calibration error is
invisible to every frame-success metric — the decoders are scale-invariant — and costs measured dB in
HARQ. The higher-value follow-on is extending `llr_reliability` bins to bpsk/qpsk/psk8 and deriving
`llr_calibration.rs`'s 7-entry list the same way; that is a separate PR because it needs per-plugin
floors.

## Consumer

Who runs this in production, by `file:line` — `grep -rn "supported_modes" --include='*.rs' crates/`
and reading the engine's soft path.

- **`crates/openpulse-modem/src/engine.rs`** — the LDPC, turbo and soft-concatenated receive arms
  call `demodulate_soft` and hard-decide the result through `fec::hard_decide`; those four sites are
  the reason (B) binds. `receive_from_samples` is the caller that turns a soft refusal into a
  terminal failure, which is the reason (A) binds.
- **`PluginRegistry::get(mode)`** dispatches by mode name, so the plugin the sweep tests for a mode is
  the one production would pick, shadowing included.

## Prior art

The sweep for an existing mechanism, with hits — `grep -rn "info().supported_modes" --include='*.rs'`
and `grep -rn "const MODES" crates/*/tests plugins/*/tests`.

- **`crates/openpulse-modem/tests/veto_membership_pin.rs`** already derives its universe from
  `info().supported_modes` and pins a membership set with a count floor. This change copies that
  pattern rather than inventing one; the `UNDRIVABLE_AT_8K` list is its `MODES_WITH_VETO`.
- **`plugins/qpsk/tests/differential_soft_capability.rs`** already derives per-plugin, and its own
  docstring says the invariant is general — it was scoped to one plugin.
- `fec::hard_decide` already existed and is `pub`; nothing new was written to slice LLRs.

## Twins

Sibling paths sharing the shape.

- **Same defect, not fixed here**: `crates/openpulse-modem/tests/llr_calibration.rs` — a hand list of
  7 modes for a cross-plugin invariant. Deferred to its own PR because it needs per-plugin floors.
- **Same defect, fixed here**: the engine's byte-identical private `hard_decide`, and `plugin.rs`'s
  unit test open-coding `llr <= 0.0`.
- **Same shape, different subject, filed not fixed**: the four production registration lists disagree
  (ardop and kiss omit mfsk16). The new source-scan test binds only the CLI's and the daemon's.
- **Not the same defect, left alone**: `carrier_offset_matrix.rs` (17 modes, but `#[ignore]`d and
  asserts nothing), `plugins/pilot/tests/{engine_loopback,sro_robustness}.rs` and
  `plugins/bpsk/tests/preamble_seam_identity.rs` — behavioural gates on deliberately chosen modes,
  where sweeping would be a runtime decision rather than a correctness one.

## Follow-ups filed rather than folded in

**#1358** (three slicer conventions still disagree at ±0.0 — the 2026-07-16 audit's finding 8, partly
applied) and **#1359** (five advertised modes unreachable at the only sample rate the engine uses).
