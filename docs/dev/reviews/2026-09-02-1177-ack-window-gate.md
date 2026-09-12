---
project: openpulsehf
doc: docs/dev/reviews/2026-09-02-1177-ack-window-gate.md
status: review
last_updated: 2026-09-03
---

# Review — #1177: covering the FSK4-ACK chain, and the window gate it exposed

Reviewer: Fable (adversarial). Date: 2026-09-02. Reviewed **before implementation**, then a second
round on the post-review measurements. Verdict: *the proposed gate falls as written; the real defect
is elsewhere and is live*. Three of my four claims were falsified in whole or in part.

## Prompt

Two rounds, both prompted for falsification rather than agreement.

**Round 1** carried the apparatus (a throwaway probe driving `receive_ota_ack_within` over two
`hpx_hf` engines with `CfoChannel` then `AwgnChannel`, 12 seeds per cell) and four claims: (1) the
issue's a-priori "±50 Hz is catastrophic" is wrong in magnitude both ways; (2) a cached AFC
correction makes the exposure narrower than "no coverage"; (3) a positive assertion through
`receive_ota_ack_within` is not wall-clock bounded in its *verdict*, so the triage's #1150 objection
does not apply; (4) a proposed gate asserting decode at ±25 Hz plus a cached-correction rescue.
It was asked specifically whether 12 seeds is a small-sample ceiling, whether `timeout_ms = 300`
against the production 4000 is a fidelity gap, whether a *stale-and-wrong* cached correction is worse
than an absent one, which of the three union-listen decoders was actually succeeding, and what the
probe made artificially easy.

**Round 2** carried a rebuilt fixed-sigma probe and asked three things: whether the
`FSK4_ACK_SEARCH_SAMPLES` cap is reachable in production or only latent; whether the `0.3 * peak`
gate does anything the CRC does not, since any replacement is constrained by whatever the gate's
comment was really guarding; and whether the noise generator biased the cells.

## Verdict

**Claim 1 — falls in half.** ±50 Hz is the exact FSK4 geometric decision boundary, not a pessimistic
guess: the correct-vs-neighbour energy ratio runs 6 dB at 0 Hz to 0 dB at 50 Hz, so "±45 Hz decodes
noiselessly" is 0.6 dB of margin, not tolerance. The optimistic half stands and was understated — the
one cleanly measured inter-rig offset on this project's hardware is 64 Hz, where the raw path is
0/12 at every SNR.

**Claim 2 — mechanism stands, my measurement did not test it.** My "cached AFC" row moved
`center_frequency`, which is a different field from `afc_correction_hz`. The review measured the real
transitive cover through `decode_burst` (30 / 60 / 64 Hz all commit a correction within 0.1 Hz and
the listen then succeeds; a fresh engine fails at 60 and 64), so F-1118-03's transitive cover is
real. The exposure window was also wrong: not "the first ACK" but the **whole session**, because
FSK4 has no `estimate_afc_hz` and an ACK decode never writes a correction.

**Claim 3 — stands, for the unpaced loopback only.** Verified identical at `timeout_ms` 1 / 300 /
4000; success returns in 4-5 ms; the loop order is read → decode → deadline. It falls the moment a
fixture paces or chunks. So the coverage genuinely needs no wall-clock-bounded verdict, and the
triage's #1150 objection does not apply — but the test must state the fill-once precondition.

**Claim 4 — falls as written.** Condition (b) was unwritable without a new public setter (barred by
the reachability-ratchet precedent), and condition (a) certified a path production never takes: my
cells were decoded by whole-buffer `decode_fsk4_ack` at perfect timing, while production uses
`decode_fsk4_ack_in_stream`.

## The defect this exposed, which was not what the issue was about

With a noise lead-in, the in-stream path decodes **35/100** at the ACK channel's operating point.
The window gate `rms >= 0.3 * peak` takes `peak` over the whole buffer, so the threshold lands within
a few percent of a true ACK window's RMS; the review's predicted `gate_ok` count matched the decode
count to within 2 in every cell. Independently reproduced at 38-45/100 with a fixed-sigma generator.

## What settled the fix shape

The gate's stated justification was constructed and found **dead**. Pre-#1027 the all-zero window
really did decode as a valid `AckOk` (an all-zero word is a codeword of any linear code, and CRC-8 of
zero content is zero), so #894's comment described a real defect. Post-#1027 whitening, the same word
descrambles to a non-codeword and RS refuses it; steady-tone windows likewise; and a MAC-keyed ACK
refused it 8/8 even pre-whitening. So the constraint on the fix is *not* "must still refuse a silent
window by energy" — the scrambler does that deterministically. The gate can be **deleted** rather
than replaced, provided the property it now leans on is pinned through the live path, because the
keystream has already changed once (#1148).

## Corrections to my own claims, made during the review

- "The gate tightens as the capture lengthens" — **refuted by my own rerun**. `AwgnChannel`
  normalises sigma to the whole input, so zero-padding raised the embedded signal's SNR and
  confounded duration with SNR. With sigma fixed, `rms/floor` is flat at 0.977-0.990 from 0.74 s to
  8.5 s.
- Gaussian noise is the **favourable** assumption for a peak-referenced threshold: real band noise is
  impulsive, and impulses inflate precisely the peak statistic. The on-air number will be worse.

## Deferred, deliberately

The `FSK4_ACK_SEARCH_SAMPLES` cap is a **distinct mechanism** (0/100 decoded at lead 32 000 where the
old gate would have admitted ~45/100). Ruled **latent, not live**: reaching it needs an IRS turnaround
over 2 s, and turnaround has never been measured on hardware. Filed as #1247 with that caveat, and
with the design contradiction — a 2 s scan inside a 9 s listen window — stated as fact.

Not covered by this change: the daemon's ISS wiring (`server.rs:1705`) and chunked/paced arrival.
