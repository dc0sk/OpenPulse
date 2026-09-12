---
project: openpulsehf
doc: docs/features.md
status: living
last_updated: 2026-06-24
---

# OpenPulseHF — Feature Reference

This document describes every major feature of OpenPulseHF: what is implemented, what
is pending, and — for non-obvious algorithms — the mathematics behind the design choices.

**Legend:**  ✅ Implemented   🔄 In progress   🔲 Planned

---

## Table of Contents

1. [Waveform Modes](#waveform-modes)
2. [Pulse Shaping, Sidelobe Suppression, and Envelope Conditioning](#pulse-shaping-sidelobe-suppression-and-envelope-conditioning)
3. [Automatic Frequency Control (AFC)](#automatic-frequency-control-afc)
4. [Forward Error Correction](#forward-error-correction)
5. [Adaptive Rate Control](#adaptive-rate-control)
6. [Session Security and Signing](#session-security-and-signing)
7. [Post-Quantum Cryptography](#post-quantum-cryptography)
8. [Data Compression](#data-compression)
9. [Segmentation and Reassembly (SAR)](#segmentation-and-reassembly-sar)
10. [Data Carrier Detect and CSMA](#data-carrier-detect-and-csma)
11. [Multi-Hop Relay and Trust-Weighted Routing](#multi-hop-relay-and-trust-weighted-routing)
12. [Winlink / B2F Integration](#winlink--b2f-integration)
13. [TNC Interfaces (ARDOP and KISS)](#tnc-interfaces-ardop-and-kiss)
14. [GPU Compute Acceleration](#gpu-compute-acceleration)
15. [Channel Simulation and Benchmarking](#channel-simulation-and-benchmarking)
16. [Diagnostics and Observability](#diagnostics-and-observability)
17. [Pending and Far-Future Features](#pending-and-far-future-features)

---

## Waveform Modes

OpenPulseHF ships modulation modes across 8 plugin families (BPSK, QPSK, 8PSK,
64QAM, FSK4, OFDM, SC-FDMA, Pilot), including `-RRC`/`-HF` single-carrier variants and the
OFDM/SC-FDMA higher-order ladders. The authoritative per-mode table — baud,
bits/symbol, gross bps, occupied bandwidth — is the
[README modulation-modes table](../README.md#modulation-types); the HF mode/FEC
selection ladder is in [mode-fec-ladder.md](mode-fec-ladder.md). `openpulse modes`
prints the live registry. (Plain rectangular `QPSK2000`/`8PSK2000` are registered but
RRC-superseded — use `-RRC`.)

The **Pilot** family (`PILOT-{QPSK,8PSK,16QAM,32APSK}{500,1000}` plus their `-RRC`
variants and `2000-RRC`, `plugins/pilot`) is a pilot-framed single-carrier waveform:
known in-band pilot symbols drive carrier recovery instead of a decision-directed
Costas loop, making it cycle-slip-immune on dense constellations and robust to
soundcard sample-rate offset. It is soft-capable, and spans four adaptive profiles
(`hpx_pilot{,_rrc,_fast,_fast_rrc}`) trading bandwidth against throughput. See the
[pilot-framed waveform](dev/design/hpx-waveform-design.md#pilot-framed-waveform) note and the
`hpx_pilot` adaptive profile.

All modes target an 8 kHz audio sample rate and a nominal 1500 Hz carrier, fitting
within a standard SSB passband.  Effective throughput figures are approximate; overhead
includes a per-frame preamble (32 symbols for BPSK, 16 symbols for QPSK/8PSK/64QAM)
plus an 8-symbol tail.  Actual throughput also depends on frame length, FEC mode, and
channel conditions.  RRC modes use α = 0.35; their bandwidth is ~35% wider than the
non-RRC equivalent at the same baud rate.

QPSK1000 (used in the `hpx_wideband` adaptive profile, formerly HPX2300) and QPSK1000-HF are distinct registered
mode names with the same baud rate and similar bandwidth; the same applies to 8PSK1000
and 8PSK1000-HF.  The -HF suffix variants are tuned for HF path conditions.

64QAM uses a Gray-coded 8×8 PAM-8 constellation (rectangular) with a soft max-log-MAP
demodulator, providing 6 bits per symbol.  64QAM2000-RRC is the highest-throughput
single-carrier HF mode and occupies the full 2700 Hz SSB passband.

### BPSK encoding

Each byte is expanded to 8 bits LSB-first, then NRZI-encoded: a `1` bit flips the
carrier phase, a `0` bit keeps it.  NRZI ensures that long runs of identical symbols
do not require a DC-level reference at the receiver.

### QPSK and 8PSK encoding

QPSK maps two bits per symbol to 45°/135°/225°/315°.  8PSK maps three bits per
symbol to one of eight equally-spaced phases using Gray coding, which minimises
the Hamming distance between adjacent constellation points — a single-phase
decision error produces at most one bit error rather than two or three.

---

## Pulse Shaping, Sidelobe Suppression, and Envelope Conditioning

✅ **Implemented** — `plugins/bpsk/src/modulate.rs`, `demodulate.rs`

### The problem with rectangular pulses

A symbol stream with hard transitions between symbols produces a `sinc`-shaped spectrum.
The first sidelobes are only −13 dB below the main lobe, causing interference to
adjacent channels.  Classical Hann windowing per symbol (multiply each symbol's samples
by a full raised-cosine window) improves this to approximately −32 dB, but also
introduces inter-symbol interference (ISI) because the window tapers to zero at the
edges of each symbol period.

### The overlapping half-Hann crossfade

OpenPulseHF uses a **50 % overlapping crossfade** design.  For each symbol of `n`
samples, the sample at position `i` within the symbol is:

```
w_tail(i) = 0.5 × (1 + cos(π·i/n))      # fades 1 → 0 over the symbol
w_head(i) = 1 − w_tail(i)               # fades 0 → 1 over the symbol

sample[i] = (A_curr × w_tail(i) + A_next × w_head(i)) × cos(2πf_c·t)
```

where `A_curr` and `A_next` are the amplitudes (+1 or −1) of the current and next
symbols.  When adjacent symbols share the same phase the two terms add to exactly 1 at
every sample — there is no amplitude droop.  When phases differ the envelope reaches
zero at the midpoint, providing a clean, controlled transition.

### ISI analysis

The crossfade deliberately introduces residual ISI: at the decision point for symbol k,
the tail energy from symbol k−1 has decayed to w_tail(n/2) = 0.5 × (1 + cos(π/2)) =
0.5.  However, the matched filter at the receiver — a half-Hann window accumulating
from the start of the symbol — integrates both the current symbol's head and the
previous symbol's tail.  For a BPSK decision the worst-case ISI contribution is exactly
1/3 of the symbol energy, which is well below the decision threshold of 1 at even
moderate SNR (typically SNR > 3 dB is sufficient).

The spectrum benefit is roughly −32 dB first-sidelobe suppression, comparable to full
Hann windowing but without the ISI penalty that full windowing creates.

### GPU-accelerated modulation

✅ When compiled with `--features gpu`, modulation sample rendering is offloaded to the
GPU via `wgpu` compute shaders (WGSL).  The CPU still handles bit-to-symbol mapping
(NRZI encoding); only the per-sample carrier multiplication is GPU-accelerated.

### CE-SSB transmit envelope conditioning

✅ **Implemented** — `crates/openpulse-dsp/src/cessb.rs`; `ModemEngine::cessb_benefits`
/ `cessb_condition_tx` in `crates/openpulse-modem/src/engine.rs`

High-PAPR multicarrier waveforms (OFDM, SC-FDMA) spend most of their time well below
their peak envelope — the peak-to-average power ratio is ~10–12 dB. Because an SSB
transmitter is **peak-power (PEP) limited**, that idle headroom is wasted average
power. Controlled-Envelope SSB (Hershberger, W9GR, *QEX* Nov/Dec 2014; public domain)
recovers it with a **look-ahead peak-stretcher** that gently compresses the complex
envelope toward a target level, raising average power at a fixed peak without the
overshoot a naive limiter would produce.

OpenPulseHF applies CE-SSB as an **optional, default-on, per-mode TX conditioner**,
gated by `ModemEngine::cessb_benefits` to the high-PAPR multicarrier families
(`OFDM*`, `SCFDMA*`). It is a no-op for single-carrier PSK/QAM — whose
amplitude-sensitive constellations would only pay EVM for no power gain — and for
near-constant-envelope BPSK (nothing to clip). It runs in `stage_emit_output` after TX
attenuation and before the tanh limiter, with a peak-restore rescale so the freed
headroom becomes average power at the same PEP. A `[modem] cessb_enabled` config flag,
the `SetCessb` control command (`openpulse daemon set-cessb`), and a panel "CE-SSB"
toggle expose the master switch at runtime.

**Algorithm.** From the analytic envelope `e[n] = |hilbert(s)[n]|`, `peak_stretch_gain`
derives a per-sample gain `g[n] ≤ 1` that holds the windowed-maximum envelope at the
clip level `L = ratio × rms(e)` over a look-ahead window (16 samples); `apply_gain`
multiplies it into the passband. The look-ahead makes the gain **band-limited** rather
than a hard sample-by-sample clip, which is what keeps spectral regrowth low. The
engine operating ratio is 2.0×rms.

**Measured benefit (channel-sim, `tests/cessb_power_evm.rs`).** On OFDM52, CE-SSB
recovers **+1.6 / +2.7 / +3.8 dB** average power at 2.5 / 2.0 / 1.5×rms clip, at **zero
BER cost** (QPSK subcarriers absorb the clip). Single-carrier 64QAM gains ~0 dB and
pays real EVM; constant-envelope BPSK gains 0 dB — confirming the per-mode gate. The
dense OFDM-HOM variants (`OFDM52-{8PSK,16QAM,32QAM,64QAM}`) and all SC-FDMA modes are
**excluded**: an earlier measurement read only raw BER and concluded the EVM cost was
FEC-absorbable, but the coded path collapses (OFDM52-32QAM 0/20, -64QAM 3/20, SCFDMA52-32/64QAM
5/30 — versus ≥20/20 with CE-SSB off). Locked by `cessb_benefits_hold_on_low_order_ofdm_hom`,
which asserts both the benefit for `OFDM52` and the exclusion of the denser modes.

**On-air confirmation.** On a real FT-991A (2 m, 20 W into a 20 dB / 20 W attenuator),
an interleaved CE-SSB OFF/ON A/B of gapless OFDM52 measured **+1.18 dB** average-power
gain on the rig's PO meter — matching the +1.2 dB digital-domain prediction for that
signal — with the ALC meter unchanged between ON and OFF, i.e. the gain arrives at the
same peak.

**Spectral cleanliness.** Because the peak-stretch gain is band-limited, the
conditioner adds negligible out-of-band regrowth: a software ACPR / occupied-bandwidth
measurement (`cessb_acpr_spectral_regrowth`, Welch PSD) shows **−0.46 dB** out-of-band
and no 99% OBW widening at the operating ratio, versus **+5 dB** out-of-band and a
~960 Hz OBW blow-up for a naive hard clip at the same level. This was also **confirmed
on real RF** (SDRplay RSP2pro off-air capture of the FT-991A's OFDM52, CE-SSB OFF/ON
A/B at 20 W): on QPSK OFDM, CE-SSB ON does not worsen the spectral mask (ACPR lower
Δ−1.0 dB, upper Δ−0.1 dB, 99% OBW Δ−46 Hz — all negligible), with front-end linearity
validated across a 32 dB level change. Together with the +1.18 dB on-air average-power
gain, on QPSK OFDM CE-SSB **raises average power without increasing PA splatter**.

**Dense OFDM-HOM and drive backoff.** The denser variants
(`OFDM52-{8PSK,16QAM,32QAM,64QAM}`) are peakier, so CE-SSB raises their average power
much more (+2.4…+7 dB digital). On-air this *does* show added regrowth (ACPR-lower
Δ +1.2…+4.2 dB) — but it is **PA compression, not the conditioner** (whose digital
output stays clean on all HOM modes, ±0.7 dB). It is therefore drive-dependent: a
32QAM-ON drive sweep went from −21.7 dBc ACPR at slammed ALC (~61) to **−29.1 dBc at
moderate ALC (~31)** with essentially the same output power — the over-drive above
proper level is pure splatter. **Practical guidance:** with CE-SSB on dense HOM, set
audio drive for moderate ALC (standard data-mode practice, don't slam it); CE-SSB then
keeps its power gain while staying spectrally clean.

To our knowledge this is the first open-source HF **data** modem to apply CE-SSB — a
technique previously confined to voice SSB — as an adaptive per-mode conditioner on
digital multicarrier waveforms.

---

## Automatic Frequency Control (AFC)

✅ **Implemented** — `plugins/bpsk/src/demodulate.rs`, `crates/openpulse-modem/src/engine.rs`

### IQ-squaring estimator

A BPSK signal at carrier frequency `f_c + Δf` with data phase `φ_k ∈ {0, π}` is:

```
s(t) = cos(2π(f_c + Δf)t + φ_k)
```

Squaring the complex analytic signal removes the data phase (since `e^{j2φ_k} = 1` for
φ_k ∈ {0, π}), yielding a phasor rotating at `2(f_c + Δf)`.  Taking the differential
phase between consecutive squared samples gives a phasor rotating at `2Δf` per sample.

Concretely, for a sample window of length `n` with sample rate `f_s` and baud rate `B`:

1. Compute complex samples `z[k] = I[k] + jQ[k]`.
2. Square: `z²[k].re = I² − Q²`, `z²[k].im = 2IQ`.
3. Form inter-symbol product: `D[k] = z²[k] × conj(z²[k−1])`.
4. Accumulate `re_sum = Σ D[k].re`, `im_sum = Σ D[k].im`.
5. Estimate: **`Δf = B × atan2(im_sum, re_sum) / (4π)`**.

The factor `4π` accounts for the double squaring (×2) and the per-symbol phase step
(×2π/B).  The unambiguous tracking range is `|Δf| ≤ B/4`:

| Mode | Tracking range |
|------|----------------|
| BPSK31 | ±7.8 Hz |
| BPSK63 | ±15.6 Hz |
| BPSK100 | ±25 Hz |
| BPSK250 | ±62.5 Hz |

### First-order correction loop

`ModemEngine` accumulates the correction across frames:

```
afc_correction_hz += step × estimated_offset_hz
```

with a default step of **0.1** (10 % of the estimated residual per frame).  This is a
first-order leaky integrator.  After `N` frames the residual is:

```
residual_N = offset × (1 − step)^N
```

For step = 0.1 and a 15 Hz initial offset, the residual after 25 frames is
15 × 0.9²⁵ ≈ 1.1 Hz, verified by the integration test `afc_converges_within_25_frames`.

The loop can be disabled (`disable_afc()`) and reset (`reset_afc()`).  Demodulation
uses `f_c + afc_correction_hz`; modulation always uses the nominal `f_c`.  The
`AfcUpdate` engine event reports both the residual `offset_hz` and the accumulated
`correction_hz` so observers can reconstruct the total offset as
`correction_hz + offset_hz`.

---

## Forward Error Correction

### Reed-Solomon (RS255,223)

✅ **Implemented** — `crates/openpulse-core/src/fec.rs`

The codec uses GF(2⁸) Reed-Solomon with:

| Parameter | Value |
|-----------|-------|
| Block size | 255 bytes |
| Data bytes per block | 223 bytes |
| Parity (ECC) bytes | 32 bytes |
| Max correctable errors | **16 byte errors** per block |
| Max detectable errors | 32 byte errors per block |

A payload is split into 223-byte chunks, each independently encoded to 255 bytes.  At
16 correctable errors per 255-byte block, the burst-error capacity is approximately
6.3 % of the block.  Combined with the stride interleaver (see below), this covers
burst channel errors significantly longer than one block.

The wire format prepends a 4-byte big-endian original-length field so the decoder can
trim padding after correction without an out-of-band length.

### Stride (block) interleaver

✅ **Implemented** — `crates/openpulse-core/src/fec.rs`

After RS encoding, a stride interleaver with configurable depth `d` (default 100)
reorders bytes so that any burst error of up to `d` consecutive corrupt bytes is
spread across `d` different RS blocks.  Each block then sees at most 1 corrupt byte
from the burst — well within the 16-error capacity.

The forward permutation maps position `i` to:

```
out_pos = (i × d) mod (n − 1)    for i < n−1
out_pos = n−1                     for i = n−1
```

This is a standard coprime-stride cyclic interleaver; the inverse uses the same
formula with the modular multiplicative inverse of `d` modulo `n−1`.

### Convolutional FEC (rate-1/2, K=3)

✅ **Implemented** — `crates/openpulse-core/src/conv.rs`

An alternative FEC for AWGN-dominant channels where burst errors are rare:

| Parameter | Value |
|-----------|-------|
| Code rate | 1/2 |
| Constraint length | K = 3 (4-state trellis) |
| Generator G0 | `0b111` (octal 7) |
| Generator G1 | `0b101` (octal 5) |
| Free distance d_f | 5 |
| Max correctable isolated errors | 2 per constraint length |

The encoder shifts each input bit through a 2-bit shift register and outputs two
parity bits per input bit.  The Viterbi decoder performs a forward pass to accumulate
path metrics (Hamming distance against received bits), then a traceback from the
minimum-metric terminal state.

**Trade-off vs RS:** At 1 % random bit-error rate, `ConvCodec` achieves a post-decode
BER of ~0.04 %, while RS fails because random errors exceed the 16-byte-per-block
capacity.  At HF burst-error profiles the opposite is true — RS + interleaver is
superior.  The engine exposes both; `ConvCodec` is recommended for VHF/UHF paths;
RS is the default for HF.

---

## Adaptive Rate Control

✅ **Implemented** — `crates/openpulse-core/src/{ack,rate,profile}.rs`

### ACK taxonomy

Each received frame is acknowledged with one of eight ACK types, conveyed via a 5-byte
FSK4 frame (20 symbols at 100 baud = **200 ms on air**):

| Code | Type | Meaning |
|------|------|---------|
| `000` | AckOk | Decoded correctly — hold current speed |
| `001` | AckUp | Decoded correctly, SNR margin high — request speed up |
| `010` | AckDown | Marginal decode — request speed down |
| `011` | Nack | Uncorrectable errors — retransmit |
| `100` | Break | Request transfer direction changeover |
| `101` | Req | Request repeat of last frame |
| `110` | Qrt | Graceful session end |
| `111` | Abort | Abnormal teardown |

The ACK frame wire format: 1 byte (ACK type in bits 2:0) + 2 bytes session hash
(FNV-1a, detects stale ACKs from a previous session) + 1 reserved byte + 1 byte
CRC-8/SMBUS.

### Rate ladder

The `RateAdapter` state machine maps ACK events to speed level transitions across
20 levels (SL1–SL20):

- **AckUp**: step up (ceiling SL20)
- **AckDown**: step down (floor SL2; SL1 only reached after 3 consecutive NACKs at SL2
  as a chirp-fallback emergency)
- **Nack**: increment NACK counter; after `nack_threshold` (default 3) consecutive
  NACKs, decrement speed level (NackDecrement) or fall to SL1 (ChirpFallback)

### Adaptive profiles

Several `SessionProfile` ladders map speed levels to modulation modes; the
[README profiles table](../README.md#adaptive-rate-profiles) is the authoritative
list. Examples:

**HPX500** (narrowband, ~500 Hz occupied bandwidth):

| Speed Level | Mode | Eff. throughput |
|-------------|------|-----------------|
| SL1 | Chirp fallback | — |
| SL2 | BPSK31 | ~19 bps |
| SL3 | BPSK63 | ~38 bps |
| SL4 | BPSK250 | ~150 bps |
| SL5 | QPSK250 | ~300 bps |
| SL6 | QPSK500 | ~600 bps |

**`hpx_wideband`** (wideband, ~2300 Hz occupied bandwidth; formerly named HPX2300):

| Speed Level | Mode | Eff. throughput |
|-------------|------|-----------------|
| SL8 | QPSK500 | ~600 bps |
| SL9 | QPSK1000 | ~1200 bps |
| SL10 | QPSK2000-RRC | ~2400 bps |
| SL11 | 8PSK2000-RRC | ~3600 bps |

The 8PSK1000 waveform at SL11 was chosen over OFDM for its lower Peak-to-Average
Power Ratio (PAPR ≈ 0 dB for single-carrier vs ≈ 6–10 dB for OFDM), simpler AFC
(single-carrier phase tracking), and no cyclic-prefix overhead.

**HPX Wideband HD** (full SSB passband, up to 2700 Hz; 64QAM):

| Speed Level | Mode |
|-------------|------|
| SL9 | SCFDMA26-8PSK |
| SL10 | SCFDMA26-16QAM |
| SL11 | SCFDMA26-32QAM |
| SL12 | SCFDMA52-16QAM |
| SL13 | SCFDMA52-32QAM |
| SL14 | SCFDMA52-64QAM |
| SL15 | 64QAM2000-RRC |
| SL14 | 64QAM2000-RRC | ~7200 bps |

`hpx_wideband_hd` is intended for clear VHF/UHF paths or quiet 10 m conditions; 64QAM
requires an SNR of approximately 20–25 dB for reliable operation.

**HPX Pilot** (`hpx_pilot`, pilot-framed single-carrier, ~550 Hz):

| Speed Level | Mode | Bits/sym |
|-------------|------|----------|
| SL2 | PILOT-QPSK500 | 2 |
| SL3 | PILOT-8PSK500 | 3 |
| SL4 | PILOT-16QAM500 | 4 |
| SL5 | PILOT-32APSK500 | 5 |

`hpx_pilot` climbs a cycle-slip-immune, sample-rate-offset-robust ladder
(see the [pilot-framed waveform](dev/design/hpx-waveform-design.md#pilot-framed-waveform) note).

---

## Session Security and Signing

✅ **Implemented** — `crates/openpulse-core/src/{handshake,manifest,trust}.rs`

### Ed25519 handshake

The HPX session handshake uses two signed **binary** wire frames (v2, #1147):

- **CONREQ** (`HSCQ` magic): initiator sends its Ed25519 public key, an ephemeral X25519
  key, the modes it offers, a session ID, the station it is addressing, and a mandatory
  timestamp.
- **CONACK** (`HSAK` magic): responder selects a signing mode from those offered and binds
  its reply to the CONREQ by SHA-256 over the complete transmitted request.

**The signature covers the transmitted prefix** — `magic || version || length || body`,
i.e. exactly the bytes on the air minus the trailing signature. There is no separate
"canonical" representation to disagree with the wire, which is what makes malleability a
non-question rather than something to defend against.

Both frames fit **one SAR fragment** by construction (maximal CONREQ 236 B, CONACK 237 B,
against 251 B) — a handshake costs one acquisition instead of the three the previous
~752 B JSON format needed.

> **Corrected 2026-08-23.** This section previously said the signature covered "a
> deterministic canonical JSON serialisation (keys sorted recursively)". The keys were
> never sorted — the code emitted fields in serde declaration order. Determinism was the
> property the security argument actually needed, and it held, which is why the wrong
> claim went unchallenged for so long.

### Trust levels

| Trust Level | Meaning |
|-------------|---------|
| Verified | Key found in trust store with full trust |
| PskVerified | PSK challenge passed; key not in trust store |
| Unknown | No prior association |
| Reduced | Key in trust store but flagged marginal |
| Revoked | Explicitly revoked — connection rejected |

### Policy profiles

| Profile | Minimum required trust |
|---------|----------------------|
| Strict | Verified |
| Balanced | PskVerified |
| Permissive | Reduced |

### Transfer manifest

Each data transfer is accompanied by a `TransferManifest` that contains a SHA-256
hash of the payload, the sender's peer ID, a timestamp, and an Ed25519 signature.
The receiver verifies the manifest before passing data to the application layer.

---

## Post-Quantum Cryptography

✅ **Implemented** — `crates/openpulse-core/src/pq_handshake.rs`

### Algorithms

| Algorithm | Role | Key/signature sizes |
|-----------|------|---------------------|
| ML-DSA-44 (Dilithium) | Digital signature | PK 1312 B, Sig 2420 B |
| ML-KEM-768 (Kyber) | Key encapsulation | EK 1184 B, CT 1088 B, SS 32 B |
| Ed25519 (classical, retained) | Backward-compatible signing | PK 32 B, Sig 64 B |

### Hybrid mode

In `SigningMode::Hybrid`, both Ed25519 and ML-DSA-44 signatures are computed and
embedded in the handshake frame.  The receiver verifies both independently.  An
attacker would need to break both schemes — providing harvest-now-decrypt-later
resistance while maintaining interoperability with classical-only peers.

### Key encapsulation mechanism (KEM)

1. Initiator generates an ML-KEM-768 encapsulation key pair and includes the
   encapsulation key in CONREQ.
2. Responder generates a random ciphertext using the encapsulation key and includes it
   in CONACK: `(ciphertext, shared_secret) = ML_KEM_768::encapsulate(ek)`.
3. Initiator decapsulates: `shared_secret = ML_KEM_768::decapsulate(dk, ciphertext)`.

Both parties arrive at the same 32-byte shared secret without it ever transiting the
channel.  The shared secret is available for session key derivation.

### SAR transport for large PQ frames

The combined PQ CONREQ frame (classical key + ML-DSA-44 key + ML-KEM-768 key + both
signatures + JSON body) exceeds the 255-byte frame payload limit.  The SAR layer
(see below) fragments it automatically; the integration tests `pq_conreq_serialized_size_fits_in_sar_capacity` and `sar_roundtrip_of_pq_conreq` verify the full PQ handshake survives SAR fragmentation and reassembly.

---

## Data Compression

✅ **Implemented** — `crates/openpulse-core/src/compression.rs`, `crates/openpulse-b2f/src/compress.rs`

| Algorithm | Use case | Implementation |
|-----------|----------|----------------|
| LZ4 | Session-layer payload compression | `lz4_flex` (pure Rust) |
| Zstd + HPX dictionary | Session-layer payload compression; the `u32` is the dictionary ID, carried to catch version skew | `zstd`, `CompressionAlgorithm::Zstd(u32)` |
| Gzip | B2F Type-D message compression | `flate2` |
| ~~LZHUF LH5~~ | B2F Type-C — **not supported**, removed in PR #948 | — |

### Session-layer LZ4

The `compress_if_smaller()` function compresses the payload and returns the original
bytes unchanged if the compressed form is not smaller.  This prevents negative
compression on short or already-compressed payloads.

The compression algorithm selected is negotiated in the Ed25519-signed CONREQ/CONACK
handshake — it is part of the signed body, so a man-in-the-middle cannot downgrade the
compression selection without invalidating the signature.

### Type C (LZHUF) — not supported

An LHA `LH5` implementation shipped here and was **removed in PR #948**. It never had a production
caller, and its external Winlink compatibility was never verified — nor could it have held: FBB
historically uses the classic Okumura LZHUF, a *different bitstream* from `LH5`, and the
length-prefix convention was also unconfirmed. Keeping it implied a capability the crate did not
have.

An inbound Type C proposal is now answered `Reject` — an honest "cannot decode this" rather than a
silent corrupt decode. The CMS gateway path is unaffected: it uses Type D (Gzip). Restoring Type C
requires a captured RMS Express / RMS Gateway Type C blob to validate the bitstream and the prefix
against.

---

## Segmentation and Reassembly (SAR)

✅ **Implemented** — `crates/openpulse-core/src/sar.rs`

The SAR sub-layer transports objects larger than a single frame payload (255 bytes).

**Header:** 4 bytes per fragment — `segment_id (u16) | fragment_index (u8) | fragment_total (u8)`.

**Capacity:** Up to 255 fragments × 251 data bytes = **64,005 bytes** per segment.
Larger inputs are rejected with `SarError::DataTooLarge`.

**Reassembly:** Keyed on `(session_id, segment_id)`.  Duplicate fragments are
idempotent (safe to retransmit).  Incomplete segments expire after a configurable
timeout.  `SarReassembler::ingest()` returns `Some(payload)` when all fragments of a
segment have arrived.

---

## Data Carrier Detect and CSMA

✅ **Implemented** — `crates/openpulse-core/src/dcd.rs`, `crates/openpulse-modem/src/engine.rs`

### DCD

The `DcdState` struct computes the RMS energy of each received sample block.  If
energy exceeds the configured threshold, the channel is marked busy.  The busy flag
is held for a configurable window (default **100 ms** at 8 kHz = 800 samples) to
prevent false clears during brief fades.

### 0.3-persistence CSMA

When `enable_csma()` is called, every `transmit()` call checks the DCD state first.
If the channel is busy, `ModemError::ChannelBusy` is returned immediately.  If the
channel is clear, a uniform random draw `p ∈ [0, 1)` is compared against the
persistence parameter (default 0.3): the transmission proceeds only if `p < 0.3`.
This randomisation staggers simultaneous transmissions from multiple stations that
all sense a clear channel at the same instant.

---

## Multi-Hop Relay and Trust-Weighted Routing

✅ **Implemented** — `crates/openpulse-core/src/{relay,peer_cache,wire_query,query_propagation}.rs`

### Relay forwarding

The `RelayForwarder` processes incoming `WireEnvelope` frames and, if policy allows,
emits a cloned envelope with `hop_index` incremented.  Guards:

- **Hop limit**: drops frames where `hop_index ≥ hop_limit`.
- **Duplicate suppression**: `(session_id, nonce)` pairs are cached with a TTL; a
  repeated frame within the TTL is silently dropped without forwarding.
- **Trust policy**: the source peer's trust level is evaluated against the relay's
  configured minimum; below-threshold sources are rejected with `PolicyRejected`.

### Trust-weighted path scoring

The `score_route()` function computes the **bottleneck score** of a candidate route:
the minimum single-hop score across all intermediate relays:

```
hop_score(hop) = trust_weight(hop.trust_level) × hop.route_quality

score_route(route) = min(hop_score(h) for h in intermediate_hops)
```

Trust weights:

| Trust level | Weight |
|-------------|--------|
| Verified | 4 |
| PskVerified | 3 |
| Unknown | 2 |
| Reduced | 1 |

Direct routes (no intermediate hops) receive score `u32::MAX` — they are always
preferred over any relayed route when reachable.  Among relayed routes, `select_best_scored_route()` picks the highest score; ties are broken by shorter path length.

The bottleneck model matches the intuition that a chain is only as strong as its
weakest link: routing through one Verified and one Reduced relay is no better than
routing through two Reduced relays.

### Route discovery and propagation

The `QueryForwarder` propagates `RouteDiscoveryRequest` frames through the network,
enforcing the same hop-limit and duplicate-suppression rules as the relay.  Responses
carry the discovered route's hop list and a route signature.

Wire message types:

| Type | Code | Direction |
|------|------|-----------|
| PeerQueryRequest | 0x01 | Client → network |
| PeerQueryResponse | 0x02 | Network → client |
| RouteDiscoveryRequest | 0x03 | Originator → network |
| RouteDiscoveryResponse | 0x04 | Network → originator |
| RelayDataChunk | 0x05 | Relay TX |
| RelayHopAck | 0x06 | Relay ACK |
| RelayRouteUpdate | 0x07 | Route update notification |
| RelayRouteReject | 0x08 | Route rejection notification |

---

## Winlink / B2F Integration

✅ **Implemented** — `crates/openpulse-b2f/`, `crates/openpulse-b2f-driver/`, `crates/openpulse-gateway/`

### B2F protocol state machine

The `B2fSession` state machine handles both Initiating Station (ISS) and Receiving
Station (IRS) roles, implementing the Winlink B2F (Binary 2 Forward) protocol:

1. **Handshake**: WL2K connection banner exchange; FNV-1a session key derivation.
2. **Proposal exchange**: ISS sends `FC` (file proposal) frames listing pending
   messages; IRS responds with `FS` (file select) frames accepting or deferring each.
3. **Transfer**: ISS sends accepted message blobs; each blob is compressed (Gzip or
   LZHUF depending on type flag) and prefixed with an RFC-5322-like ASCII header.
4. **Completion**: ISS sends `FF` (finished) after all blobs; session role reverses
   for IRS→ISS transfer.

### Direct TCP gateway

`openpulse-gateway` connects directly to `cms.winlink.org:8772` (or any Winlink CMS)
using the same u16-BE framed `DataPort` abstraction as the B2F driver.  No TNC or
radio hardware is required.  The gateway performs:

- **Phase 1 (ISS)**: reads CMS banner, sends FC+FF proposals, reads FS, sends blobs.
- **Phase 2 (IRS)**: new `B2fSession(Irs)` on the same TCP connection, receives CMS
  proposals, sends FS, receives and decompresses reply blobs.

---

## TNC Interfaces (ARDOP and KISS)

### ARDOP-compatible command/data port

✅ **Implemented** — `crates/openpulse-ardop/` (`openpulse-tnc` binary)

The TNC presents two TCP ports (default 8515 cmd / 8516 data) speaking the ARDOP
ASCII line protocol.  Implemented commands:

`VERSION` · `MYID` · `LISTEN` · `CONNECT` · `DISCONNECT` · `ABORT` · `STATE` ·
`BUFFER` · `PTT` · `GRIDSQUARE` · `ARQBW` · `ARQTIMEOUT` · `CWID` · `SENDID` ·
`FECSEND` · `FECRCV` · `PING` · `CLOSE`

`FECSEND` and `FECRCV` enable FEC-framed unconnected transmissions used by `pat` for
Winlink message delivery.  `BUFFER` accurately tracks the pending TX byte count so
`pat` can poll for drain confirmation.

### KISS / AX.25

✅ **Implemented** — `crates/openpulse-kiss/` (`openpulse-kisstnc` binary)

Full KISS byte-stuffing (`FEND`/`FESC`/`TFEND`/`TFESC`) and AX.25 UI frame
encoding/decoding.  Compatible with any KISS-capable application (Dire Wolf, aprx,
Xastir).

---

## GPU Compute Acceleration

✅ **Implemented** (opt-in, per-plugin `--features gpu`) — `crates/openpulse-gpu/`,
wired into **5 plugins**: `bpsk`, `qpsk`, `psk8`, `64qam`, `scfdma` (OFDM has no GPU
path). Six WGSL compute kernels run on any wgpu-compatible GPU (Vulkan, Metal, DX12,
WebGPU): `bpsk_modulate`, `bpsk_demodulate`, `timing_search` (the BPSK trio),
`rrc_fir` (the RRC matched filter, BPSK/QPSK/8PSK/64QAM), `soft_demod` (8PSK/64QAM
soft LLRs), and `fft256` (SC-FDMA batched FFT). The GPU is API-only (`with_gpu`),
exercised by tests; a CI job (`gpu-feature-gates`) builds + lints the feature so it
doesn't rot. The BPSK modulate path, as one example, is three stages:

1. **Byte-to-bit expansion**: 64-thread workgroups extract LSB-first bits from input
   bytes in parallel.
2. **Symbol mapping**: maps NRZI-encoded bits to +1/−1 amplitude values.
3. **Sample rendering**: computes the overlapping half-Hann crossfade and carrier
   multiplication for each output sample.

An important correctness detail: WGSL's `select(a, b, cond)` evaluates both branches
unconditionally (unlike C's ternary).  The out-of-bounds read on the last symbol's
lookahead required an explicit `if (sym_idx + 1u < params.n_syms)` guard rather than
a nested `select()`.

All GPU functions return `Option<T>`; a `None` result causes automatic fallback to the
CPU path, so GPU absence is never a hard failure.

CPU/GPU equivalence is verified by integration tests that compare sample-by-sample
output to within 1 × 10⁻⁴ absolute error.

---

## Channel Simulation and Benchmarking

✅ **Implemented** — `crates/openpulse-channel/`, `crates/openpulse-modem/src/channel_sim.rs`

### Channel models

| Model | Parameters | Typical use |
|-------|-----------|-------------|
| AWGN | SNR dB | Baseline noise floor |
| Watterson Good (F1) | Doppler 0.1 Hz, delay 0.5 ms | Low-latitude quiet HF |
| Watterson Moderate (F2) | Doppler 1.0 Hz, delay 1.0 ms | Mid-latitude HF |
| Watterson Poor | Doppler 3.0 Hz, delay 2.0 ms | Disturbed HF |
| Gilbert-Elliott Light | p=0.01, r=0.1, burst BER 0.3 | Mild packet loss |
| Gilbert-Elliott Burst | p=0.05, r=0.05, burst BER 0.5 | Heavy packet loss |
| QRN/QRM/QSB/Chirp | Configurable | Noise, interference, fading, Doppler chirp |

The Watterson model implements correlated Rayleigh fading by shaping a Gaussian noise
process with a Doppler-spread filter, then applying the resulting complex envelope
to the signal.  At the Good F1 profile (Doppler spread 0.1 Hz), the shaping filter
is sub-bin at the 1024-sample FFT size (7.8 Hz/bin at 8 kHz), so the envelope
approximates constant amplitude — an acceptable approximation at this spread.

### ChannelSimHarness

Two `ModemEngine` instances are wired together through a `ChannelModel` in a single
test fixture.  The TX engine modulates, the channel model corrupts, and the RX engine
demodulates — providing an end-to-end loopback that exercises the full signal path
without audio hardware.

### Benchmark regression gate

The CI benchmark gate verifies 100 % scenario pass rate and `mean_transitions ≤ 20.0`
per scenario.  A failed gate blocks merges to `main`.

---

## Diagnostics and Observability

### Structured JSON event stream

✅ **Implemented** — `crates/openpulse-modem/src/event.rs`, `crates/openpulse-cli/src/commands/monitor.rs`

`ModemEngine` broadcasts `EngineEvent` values on a `tokio::sync::broadcast` channel
after every significant state change.  Events are serialisable as NDJSON
(`{"type":"afc_update","offset_hz":1.2,"correction_hz":3.7,"mode":"BPSK250"}`).

| Event | Trigger |
|-------|---------|
| `AfcUpdate` | Every receive call (if plugin supports AFC); includes `offset_hz` and `correction_hz` |
| `RateChange` | ACK applied to an active adaptive session |
| `DcdChange` | Channel busy/clear transition |
| `HpxTransition` | HPX state machine step |
| `FrameTransmitted` | Successful TX |
| `FrameReceived` | Successful RX |
| `SessionStarted` | Secure HPX session handshake accepted |
| `SessionEnded` | Session closed or cancelled |

`AfcUpdate.correction_hz` uses `#[serde(default)]` so older NDJSON streams that
predate the field deserialise without error.

### TUI

✅ **Implemented** — `crates/openpulse-tui/` (`openpulse-tui` binary)

Three-panel ratatui interface: HPX state (colour-coded), AFC offset + correction +
DCD energy bar, scrollable transition log.  Keyboard: `q` quit, `p` pause, ↑↓ scroll.

---

## Deferred Features

### Release packaging ✅ Done (PR #206)

GitHub Actions release workflow ships static x86-64 musl binaries and aarch64 `.deb`
packages for `openpulse-tnc`, `openpulse-kisstnc`, and `openpulse-server` on `v*` tag push.

### 6.3 — Network mesh daemon ✅ Done (PR #120, #121)

`openpulse-mesh` daemon ships `RelayForwarder`, `QueryForwarder`, peer discovery beacons,
store-and-forward relay, and peer cache population from beacon responses.  `CONNECT_MESH`
ARDOP extension command directs `ModemEngine` into mesh mode.

### FF-1 — Operator-initiated QSY negotiation ✅ Done (PR #140, #141)

Five-frame Ed25519-signed, freshness-stamped ASCII protocol (QSY_REQ / QSY_LIST / QSY_VOTE / QSY_ACK /
QSY_REJECT); `QsySession` state machine and `QsyScanner` in `crates/openpulse-qsy`.
Disabled by default; enabled per `[qsy]` config section.

### FF-2 — IQ output mode for direct SDR integration ✅ Done (PR #150)

`ModulationPlugin::modulate_iq()` produces complex baseband samples (I/left, Q/right)
for direct SDR upconversion.  `AudioBackend::open_iq_output()` opens a stereo output
stream; `ModemEngine::transmit_iq()` dispatches via it with CPU fallback.

### Open code stubs (not blocking any current work)

All previously listed stubs are now closed:

- **8PSK soft demapping** ✅ Done — `demodulate_soft()` implements Gray-coded max-log-MAP demapping.
- **CLI manifest verify** ✅ Done — `manifest verify` is wired to `verify_manifest()` in the library.

### V1.x — Wider-than-3 kHz channel support (10 m HF / VHF / UHF) 🔲 Planned

v1.0 targets standard HF SSB channels (≤ ~2.7 kHz occupied, 8 kHz audio). The 9600-baud
modes — `QPSK9600`, `QPSK9600-RRC`, `8PSK9600`, `8PSK9600-RRC` — occupy ~12–13 kHz and need
≥ 38.4 kHz audio (≥ 4 samples/symbol), so they cannot run on the HF path. Their plugin code
is retained for a post-v1.0 wideband transport (10 m HF, VHF/UHF FM channels). Scope:

- A wideband transport (≥ 48 kHz audio device path) for loopback and on-air testing.
- Test-matrix coverage for the 9600-baud modes at the higher sample rate (they are listed in
  `WIDEBAND_POST_V1_MODES` in `apps/openpulse-testmatrix/src/cases.rs`, deferred and enforced
  by the coverage regression test until then).
- Revisit the `hpx_narrowband_hd` profile naming, which currently maps 9600-baud (wideband)
  modes — see `crates/openpulse-core/src/profile.rs`.

Related v1.0 known limitation: plain rectangular `8PSK2000` closes the eye at 4 samples/symbol
on the 8 kHz path (use `8PSK2000-RRC`); tracked in `KNOWN_LIMITATION_MODES`.
