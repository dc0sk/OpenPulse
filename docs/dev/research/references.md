---
project: openpulsehf
doc: docs/dev/research/references.md
status: living
last_updated: 2026-08-10
---

# External references and inspirations

Open-source modems and DSP libraries we study for technique and validation. This
is a living index — when a DSP problem stalls (carrier recovery, sync, equalization,
FEC, PAPR), come back here first and check whether one of these has solved it. Add
new sources and new "what we could take" notes over time.

We implement independently (OpenPulseHF is a from-scratch protocol); these inform
*technique*, not code lifted wholesale. Note each project's licence before porting
any code.

> **Source-level scan (2026-06-17):** a full read of these repos' code — a
> prioritized idea catalog (benefit/effort/licence/fit per idea) plus
> recommendations and the SC-FDMA-low-PAPR analysis — is in
> [reference-mining-plan.md](reference-mining-plan.md).

---

## gnuradio/gnuradio — the SDR reference toolkit

<https://github.com/gnuradio/gnuradio> · GPL-3.0

The canonical reference for physical-layer DSP blocks. Especially relevant:

- **FLL Band-Edge** (`gr::digital::fll_band_edge_cc`, <https://wiki.gnuradio.org/index.php/FLL_Band-Edge>)
  — a frequency-locked loop that derives a carrier-frequency error from the signal's
  upper/lower band edges (`e = Re{cc·ss*}`). It is **not** decision-directed (no
  cycle-slip on dense constellations) and uses **no preamble** (no ISI bias), but it
  **requires excess-bandwidth / RRC pulse shaping** (the band-edge filter is the
  derivative of the raised-cosine matched filter). Sits *before* the matched filter
  and Costas loop.
- **Canonical PSK receiver chain**: AGC → **FLL band-edge** (acquire frequency) →
  RRC matched filter → symbol sync (timing) → **Costas loop** (residual phase). The
  two-stage FLL-then-Costas split is the robust pattern.

**Taken / planned:** the FLL-then-Costas two-stage carrier recovery is the fix path
for our 8PSK carrier-offset gap (see `docs/...` / memory `8psk-carrier-offset-gap`).
Our single decision-directed Costas loop + biased data-aided preamble AFC is the
non-standard part.

**Revisit for:** symbol timing recovery (polyphase clock sync), the band-edge FLL
implementation details, LDPC/polar decoders, channel models, equalizer blocks.

---

## daniestevez/qo100-modem — QO-100 narrowband modem (Daniel Estévez)

<https://github.com/daniestevez/qo100-modem>

A high-quality GNU Radio modem for the QO-100 (Es'hail-2) narrowband transponder,
by a well-known SDR/DSP author. **32APSK** waveform in a **2.7 kHz SSB** bandwidth
(directly comparable to our HF channel), plus experiments with **differentially-
encoded 8PSK**.

**Inspirations:**
- Differential encoding to sidestep absolute carrier-*phase* recovery (helps the
  phase loop; does not by itself fix a frequency offset).
- A dense APSK constellation engineered for a 2.7 kHz voice-bandwidth channel —
  relevant to our high-throughput-in-2.7 kHz goal (cf. the OFDM HOM ladder).

**Revisit for:** APSK constellation/throughput design in 2.7 kHz, pilot/sync design
(the `gr-qo100_modem` directory + the Jupyter notebooks hold the DSP detail), and
Doppler/drift handling for satellite-grade carrier tracking.

---

## dj0abr/SSB_HighSpeed_Modem — deployed ham 8PSK/QPSK-over-SSB modem

<https://github.com/dj0abr/SSB_HighSpeed_Modem> · docs at <https://hsmodem.dj0abr.de>

A *fielded* amateur high-speed data modem over a 2.7 kHz SSB audio channel — the
closest analog to our exact use case (PSK between two radios that each have a
carrier offset). Built on **liquid-dsp** (BSD), `libsoundio`, `fftw3`.

**Inspirations:**
- **liquid-dsp `framesync`**: corrects gain/carrier/timing offsets via a known
  preamble — **coarse CFO from preamble correlation, fine CFO refined from the
  payload**. The standard two-stage burst-mode CFO. BSD-licensed C, so it is a
  *portable* reference for a Rust frame synchronizer.
- Proof that robust 8PSK/QPSK over real SSB radios with offsets is achievable with
  RRC shaping + a proper frame synchronizer.

**Revisit for:** burst-frame CFO (coarse+fine), preamble design, the liquid-dsp
modem/framesync primitives generally (it also has FEC, equalizers, resamplers).

---

## Rhizomatica/mercury — deployed HF OFDM data modem + ARQ (HERMES)

<https://github.com/Rhizomatica/mercury> · GPL-3.0 / LGPL-2.1 (vendored FreeDV codec) · C

A *fielded* HF data system — "a Digital Radio OFDM protocol for HF broadcast and
peer-to-peer ARQ connections" for store-and-forward email/file transfer over HF in
rural and emergency scenarios. Part of Rhizomatica's **HERMES** (High-frequency
Emergency and Rural Multimedia Exchange System), funded by ARDC. Unlike the
single-carrier DSP references above, Mercury is a full **OFDM + ARQ + application**
stack — the closest analog to OpenPulseHF's *system* (HPX ARQ + B2F/Winlink), not
just its DSP.

Built on **FreeDV's OFDM modem** (David Rowe). We already interface FreeDV for authenticated voice
(`openpulse-freedv-auth`), so the FreeDV DATAC modes are a shared reference point.

**Mercury has no acquisition code of its own** (verified at source 2026-08-02, `cc5bbc7`): its PHY is
a vendored codec2 tree under `modem/freedv/`, and `modem/modem.c` only *calls* the FreeDV API —
`freedv_rawdatapreambletx` to transmit (line 988), `freedv_nin`/`freedv_rawdatarx` to receive
(lines 1142, 1201). So everything in the codec2 section below **is** Mercury's frame detector. Two
consequences worth carrying:

- **It runs the burst path.** `freedv_set_frames_per_burst` is called for every pooled mode
  (`modem.c:459`), and that reaches `ofdm_set_packets_per_burst`, which sets `data_mode = "burst"`
  and enables the postamble detector. Detection is therefore the joint time × frequency search
  against a one-modem-frame PN preamble, thresholded on **ρ²**.
- **The shipped mode pool** (`modem.c:466–484`) is DATAC16, DATAC15, DATAC13, DATAC4, DATAC3, DATAC1,
  DATAC17, QAM16C2 — where **datac15/16/17 are Mercury-custom modes added inside the vendored tree**
  (`freedv/ofdm_mode.c:136, 315, 345`; datac16 is documented there as the robust control mode that
  "replaces datac13"). They follow upstream's per-mode threshold convention: 0.10 for datac17,
  0.45 for datac13/15/16.

**No wall clock reaches detection.** The vendored `ofdm.c` contains **zero** `clock_gettime`/
`gettimeofday`/`usleep`/`time()` calls, and RX is `freedv_nin`-driven. `modem.c` does use `usleep`
and `clock_gettime` — but only for idle-loop pacing, PTT and playback, never for a detection
decision. Relevant to #1058, where our own acquisition is bounded by wall clock. Mercury's one place
where timing nearly touches the signal path argues the same way, in the **virtual-clock simulation
transport** specifically: PTT must span the burst *"IN SIGNAL TIME. The wall-clock sleeps of the
else-branch would tear the keyed window away from the audio actually on the virtual cable"*
(`modem.c:1049–1060`).

**Inspirations:**
- **Adaptive ARQ "gear-shifting" driven by link quality *and* backlog**, with
  per-direction mode selection — comparable to our `RateAdapter`/HPX rate ladder, but
  the queue-backlog input and asymmetric per-direction rate are ideas we don't use yet.
- A connect/accept handshake with ACK/retry, keepalive, and controlled disconnect
  over HF — a deployed ARQ design to compare against our HPX session state machine.
- The FreeDV DATAC OFDM data modes as a proven HF-OFDM comparison for our OFDM
  higher-order ladder (cyclic-prefix + pilot design rather than RRC + FLL).

**Gear-shifting, verified in source** (`datalink_arq/arq_fsm.c` — `select_best_mode`,
`record_tx_outcome`; constants in `arq_protocol.h`) — **five** inputs, not one:
peer-reported *forward*-link SNR (a byte in every frame, EMA'd `(old*3+new)/4`); an **OLLA**
delivery-corrected offset (target first-try FER 0.30, `+FER/(1-FER)` dB per clean delivery, −1 dB per
first-try failure, clamped [−20,+3]); **TX backlog** floors per rung (no upgrade unless the backlog
exceeds the current rung's frame capacity); a hard-loss net (8 consecutive retries → robust floor);
and **reverse-loss discrimination** (a retry while peer SNR still clears the mode floor + 2 dB is
attributed to a lost ACK on the independently-faded reverse path, so the forward mode is *held* —
bounded at 3, because an unbounded hold froze every downgrade path above the fade cliff, their "S1"
bug). Upgrade hysteresis 5 dB, asymmetric. They **removed** a retry-count downgrade that ran
*alongside* OLLA: two controllers fought, measured 4–8× mode churn and −20 % goodput at 5–8 dB.
Thresholds derive from measured **goodput crossovers including the ACK cycle**, not decode
probability.

**Energy is only ever a CSMA question here.** Mercury's sole energy detector is
`modem/channel_busy.c` — passband spectral peak against an asymmetric-EMA noise floor, relative dB,
hysteresis and debounce — used to decide *transmit* deferral, never frame start. Its floor is the
passband **spectral minimum**, which an in-band signal cannot poison, unlike a time-domain
percentile history (cf. our `EnergyGate`). Frame start comes from the vendored FreeDV correlation.

**Revisit for:** backlog-aware climb gating (cheapest adoption); **OLLA as a replacement for**, never
alongside, our consecutive-count climb rules (they measured what "alongside" costs); reverse-path
ACK-loss discrimination plus the bounded-hold lesson; the SNR-byte-in-frame wire design; ARQ
rate-adaptation policy (backlog-aware, per-direction), HF
store-and-forward email protocol design (cf. B2F/Winlink), and the FreeDV DATAC OFDM
modem parameters (CP length, pilot scheme).

---

## drowe67/codec2 — FreeDV OFDM modem (`src/ofdm.c`)

<https://github.com/drowe67/codec2> · LGPL-2.1 · C

The acquisition reference *for* Mercury, and the direct counter-example to our energy-anchored
frame location. Verified by reading `src/ofdm.c` (2696 lines) on 2026-07-31:

- `est_timing()` correlates against known pilot samples and divides by
  `av_level = 1/(2·sqrt(timing_norm·acc/length))` (lines 807–808, applied line 895) — so `timing_mx`
  is a **dimensionless ρ**, and validity is the ratio test `timing_mx > timing_mx_thresh`
  (lines 915, 1356). There is **no absolute rx-energy threshold in the receiver** and **no receiver
  AGC** — pilots supply the per-carrier amplitude reference, which doubles as equalization.
- **The threshold is per-mode configuration, not a receiver constant** (re-read 2026-08-02, upstream
  and in Mercury's vendored copy). `ofdm.c:189`'s **0.30 is only the struct default**, overridden at
  `ofdm.c:225` from `OFDM_CONFIG`; `src/ofdm_mode.c` then sets datac0 **0.08**, datac1/datac3
  **0.10**, datac4 **0.5**, datac13/14 **0.45**. The burst preamble is the *same duration* for every
  mode, so the 6× spread tracks the template's **structure**: 3–4-carrier pilots are near-tone combs
  with a high noise/tone correlation floor and get 0.45–0.5, while 9–33-carrier templates get
  0.08–0.10. This is the deployed counterpart of our `PreambleTemplate`-carries-its-own-threshold
  API — **no deployed modem we have read uses one detection threshold across templates of differing
  structure.** ⚠ An earlier revision of this section quoted only the 0.30 default; that omission was
  later cited as "codec2 uses one global constant" while designing #1053, i.e. our own second-hand
  summary became the false premise. Read the mode table, not just the struct initialiser.
- **The same field is compared against two different statistics.** `ofdm_sync_search_core`
  (`ofdm.c:1466`) branches on `data_mode`: the streaming path thresholds the `av_level`-normalised
  **ρ** (line 915) while the burst path thresholds `max_corr²/(mag1·mag2)` — **ρ²** (line 1287,
  tested line 1356). `ofdm_set_packets_per_burst` flips `data_mode` to `"burst"` at *runtime*, so a
  mode's configured number means ρ or ρ² depending on the application. On the burst path (the one
  Mercury uses) 0.45 ≈ ρ 0.67 and 0.08 ≈ ρ 0.28 — a 2.5× spread in ρ terms. Never compare their
  constants to ours without first establishing which path a mode is deployed on.
- Burst acquisition (`est_timing_and_freq`) is a **joint time × frequency** correlation search,
  coarse then fine, returning `(t_est, foff_est)` from **one** measurement. A frequency estimate
  therefore cannot exist without a detection — "AFC settled on idle noise before the frame arrived"
  is impossible by construction, not by tuning.
- On a failed unique-word check the burst state machine returns to `search` **and zeroes the receive
  buffer** — `ofdm->rxbufst = ofdm->nrxbufhistory` with the comment *"reset rxbuf to make sure we
  only ever do a postamble loop once through same samples"* (lines 2187–2189). That is our #1021
  rule — *a recovery is not one until it changes the input to the failed decision* — as two lines
  of C.
- A **postamble** correlator lets a late-tuning receiver detect a burst and back up `np` frames.

**Revisit for:** replacing the engine's energy-anchored frame location with normalized joint
time×frequency preamble correlation; and a unique-word-**outside**-the-FEC frame format — our magic
`OPLS` lives in the frame header *inside* the FEC-protected payload, which is precisely why
validating an anchor currently costs a full frame decode (18 of them, per `SETTLE_FAILURE_LIMIT`).

---

## RFnexus/modem73 — multi-mode HF/VHF software modem, simultaneous RX

<https://github.com/RFnexus/modem73> · C++ · (license: see repo)

A KISS-compatible software modem for HF/VHF/UHF in a 2400 Hz channel that runs **three
modulation families at once** and decodes all of them from a single receiver — no mode
switching. **OFDM** (derived from the open-source COFDMTV modem: BPSK→QAM4096, code
rates 1/4–5/6, 790 bps–>13 kbps), **ROBUST** (five modes 1150–149 bps purpose-built for
fading HF/NVIS, in 2400 Hz *and* narrowband 600 Hz variants), and a non-coherent
**MFSK** weak-signal fallback. aicodix DSP libraries, miniaudio I/O; KISS-over-TCP plus a JSON
control API and VOX/rigctl/serial/CM108 PTT. License is **Unlicense/public domain**; the vendored
aicodix deps are ISC-style. ROBUST is now **13 modes** (RDM-800 punctured, short variants, RDM-QB
micro-burst — `robust_modem.hh:30–54`), not five.

**Three detectors, one per family, none energy-anchored and none clocked** (read at source
2026-08-02, v2.1.1):

- **OFDM** — Schmidl–Cox self-normalised autocorrelation (`schmidl_cox.hh:15–23`, per-sample
  threshold 0.05), confirmed by a frequency-domain **differential-MLS** kernel: adjacent-bin division
  cancels common carrier/channel phase, so one FFT resolves integer-bin CFO across the whole band,
  gated on peak/second-peak ≥ 4.
- **ROBUST** — CP autocorrelation (`m > 0.15`) plus an MLS pilot scan whose gates are **per
  geometry**: `robust_modem.hh:913` `gate = nc_ <= 8 ? 0.78 : 0.60`, `:1144` `qgate = nc_ <= 8 ?
  0.62 : 0.4`. Narrow templates get the higher gate — the same direction as codec2's per-mode
  thresholds and as our own measurements.
- **MFSK** — 8 *alternating* tones for cheap run detection, **terminated by a 2-symbol ordered unique
  word for timing** (`mfsk_modem.hh:34–36`). That is the aperiodic-terminator answer to the problem
  that killed our onset-snap in #1052: an alternating preamble is periodic and cannot place an onset,
  so they append something that can. Searched over 7 explicit coarse frequency hypotheses — *"the
  preamble tracker must search frequency as well as time: beyond about half a tone spacing of
  mistuning the nominal bins see nothing"* (`mfsk_modem.hh:717`).

**Every deadline is a sample count**, never a clock: `peak_deadline_ = n + 2·D` (`robust_modem.hh:512`),
`COLLECT_STALE`/`PREEMPT_STALE = 288000` samples, pilot scan every `SCAN_STRIDE = 240` samples. No
`<chrono>`/`clock_gettime`/`gettimeofday` exists anywhere in the phy layer — only in `rigctl_ptt`,
`kiss_tnc`, `miniaudio_audio` and `tnc_ui`. Detection is **O(1) per sample by construction** (running
-sum autocorrelators, strided FFT scans), so the "can I keep up?" question our wall-clock retry budget
answers (#1058) never arises for them.

**Inspirations:**
- **Simultaneous multi-family reception** — decode every registered waveform from one
  capture stream instead of committing to a mode. We switch modes on the ladder; a
  parallel-decode RX tap off our single `InputCapture` seam is a different design point
  worth studying for a discovery/monitor mode.
- **A dedicated ROBUST *narrowband* (600 Hz) fading family** as the weak-signal tier —
  the alternative to the frequency-diversity rung we measured-and-rejected (#864): a
  purpose-built robust low-rate waveform rather than dual-carrier repetition. If a
  sub-floor rung is ever revisited, ROBUST-style is the direction to compare against.
- The **COFDMTV OFDM lineage** (Schmidl–Cox + high-order QAM in 2400 Hz) as another
  HF-OFDM comparison for our OFDM higher-order ladder, alongside Mercury/FreeDV DATAC.
- A **JSON control API** decoupled from the KISS transport — parallels our daemon control
  port; a reference for control-surface design.

**Revisit for:** parallel multi-mode RX; a robust narrowband weak-signal waveform (vs.
the rejected diversity rung); OFDM parameter comparison.

---

**Detection is a dimensionless statistic with staged validation** (verified in
`schmidl_cox.hh`): Schmidl-Cox `M = |P|²/R²` — scale-invariant, so a capture AGC cannot break it —
with Schmitt-trigger hysteresis (0.05 on / 0.04 off) and a `min_R` floor that exists only to stop
silence dividing to infinity (the same role as the energy floor argument on our
`search_normalized`). A candidate then survives frequency-domain kernel correlation with a
peak-to-second-peak ≥ 4 rejection and a guard-bounded position check — **three rejections before any
payload decode**. One detection event yields timing, fractional CFO *and* integer-bin CFO. A false
sync then dies at a **CRC-checked polar-coded meta symbol** one symbol in, and an erasure budget (¾ of code
redundancy) aborts hopeless decodes early rather than grinding to the end. Net effect: no
anchor-condemnation machinery exists, because being wrong is cheap and known immediately.

---

## chrissnell/omnimodem — Rust multi-mode modem daemon (architecture mirror)

<https://github.com/chrissnell/omnimodem> · MIT · Rust (daemon + DSP) + Go (TUI)

Not an HF-ARQ modem — a **gRPC-driven orchestration daemon** multiplexing many amateur
modes (WSJT-X FT8/FT4/JT65/JT9/WSPR/FST4, fldigi PSK/RTTY/Olivia/Contestia/MFSK, AX.25
1200, image modes) from one process — but its **architecture is almost exactly
OpenPulseHF's**, arrived at independently, which makes it a valuable convergence
reference.

**Inspirations (architecture, not waveforms):**
- **Async control edge / synchronous DSP core — "no async on the sample path."**
  tonic+tokio gRPC handlers feed an `mpsc` into a plain-`std::thread` DSP core; events
  flow out on `tokio::broadcast`. This is *our* daemon (tokio control loop + the
  `worker_loop` OS thread sharing the engine) — independent validation the split is right.
- **LLR as the universal contract between detector/demapper and FEC decoder**, so
  "adding a new mode is an assembly job, not a from-scratch DSP project." Directly
  parallels our calibrated-soft-LLR plugin contract (`demodulate_soft`/`combine_llrs_map`);
  their framing of it as the *pluggability* boundary cleanly articulates what our
  `llr_calibration`/`llr_reliability` gates enforce.
- **Known-answer vectors + cross-decode against reference implementations** for every
  DSP/FEC block — the same discipline as our JS8 Qt5/boost ground-truth validation.
- **`unkey-on-Drop` safety + explicit RX/TX interlock** — the exact concern the B1 PTT
  watchdog (#863) addresses, in an alternative RAII framing.
- **Pure, daemon-independent DSP crate** — mirrors our `openpulse-core`/`openpulse-dsp`
  split; **SQLite device identity** surviving hotplug/rename is an idea we don't have.

**Revisit for:** transmitter-release RAII (unkey-on-Drop) as a companion to the PTT
watchdog; the LLR-contract framing when documenting the plugin API; hotplug-safe device
identity.

---

## chrissnell/graywolf — Rust AFSK modem + Go APRS stack (efficient-ARM DSP)

<https://github.com/chrissnell/graywolf> · Rust (modem) + Go (AX.25/APRS) + Svelte/Kotlin · (license: see repo)

A complete modern **APRS/packet** station (VHF/UHF AFSK): a Rust software modem + Go
digipeater/iGate + web UI + Android client, SQLite config. Not HF and not our waveforms,
but two things transfer.

**Inspirations:**
- **Benchmark-driven DSP that beats the reference.** Its AFSK demod ports Dire Wolf +
  Ion Todirel's libmodem (**decision-feedback AGC + hard-limiter correlator**) and
  reportedly beats Dire Wolf's best mode on every test track at ~19 % of one Pi 5 core.
  The ethos — a measured per-track benchmark suite as the DSP gate — is exactly our
  benchmark-harness/testmatrix discipline. Its **decision-feedback AGC** is a lever we
  *already* have (`openpulse_dsp::agc::Agc`, seam-wired since PR #583); the **hard-limiter
  correlator** was evaluated in the 2026-07-14 design review and **rejected** — it is
  constant-envelope and destroys the amplitude information our calibrated soft-LLR path
  needs, and our acquisition is already amplitude-invariant (`search_normalized` /
  relative `refine_onset`), so nothing motivates it.
- **A broad multi-interface PTT abstraction** (serial RTS/DTR, CM108 USB-HID, GPIO,
  rigctld, VOX, tone) — a superset of `openpulse-radio`'s `PttController` backends
  (CM108-HID and GPIO are ones we don't have; tracked as REQ-PTT-02/03).

**Revisit for:** CM108-HID and GPIO PTT backends (REQ-PTT-02/03).

---

## markqvist/Reticulum — cryptographic networking stack for minimal-bandwidth links

<https://github.com/markqvist/Reticulum> · manual: <https://markqvist.github.io/Reticulum/manual/whatis.html> · (license: see repo)

**Not a modem and not DSP** — a transport-agnostic layer-3+ cryptographic networking stack
(identity, addressing, routing, encrypted links) that runs over anything providing a half-duplex
channel ≥ 5 bps with a 500-byte MTU (LoRa, packet radio, serial). It sits *above* where our modem
ends. It is here for one question the maintainer asked (2026-08-10): whether its identity/announce/
link design can shrink our **cryptographic on-air signature surface** — the bytes our signed frames
cost at 31–250 baud. The answer is: it supplies *calibration numbers* proving the same security
property fits in a fraction of our bytes, plus two design patterns worth a future detailed
assessment. It says nothing about RF/spectral signatures — by design it has no opinion below its
interface.

**Their numbers (from the manual — their own claims, not independently verified):** X25519 ECDH +
Ed25519 identity; 16-byte truncated-SHA-256 destination addresses; per-packet overhead 19–35 B
(2 B header + 16/32 B addresses + 1 B context); **announce = 167 B** (destination hash + full
public keys + app data + random blob + Ed25519 signature); **encrypted-and-verified link
establishment = 3 packets, 297 B total** (83 B request carrying an ephemeral X25519 key, 115 B
proof carrying the identity signature over `(link_id, LKr)`); link keepalive 20 B ≈ 0.45 bps.

**Our numbers (measured 2026-08-10 at HEAD, scratch harness against `openpulse-core`).**

> **SUPERSEDED for the classical frames 2026-08-23 by #1147 — kept because they are the MOTIVATION,
> not an error.** (Figures corrected 2026-09-12: this note was written on 08-23 and re-staled on
> 08-26 by #1204, which raised the callsign cap to 18 and took `dst_station` off the CONACK.)
> The JSON handshake below is gone: a v2 CONREQ is **236 B** and a CONACK **237 B**,
> both inside one 251 B SAR fragment, so a handshake is one acquisition rather than three.
> The estimate in the very next paragraph — "~200–250 B a binary layout of the same fields would
> need" — was **right**: the built layout is 236 B, inside the predicted range. The 3.4× JSON
> inflation identified here was the whole defect, and removing it recovered what this analysis said
> it would.
>
> The **PQ** figures below are NOT superseded in substance. A binary PQ CONREQ is ~5 kB ≈ 2.7 min at
> BPSK250 — better than 17 939 B but still undeployable, exactly as this section concluded. PQ is in
> the v2 format so the format is finished, not because it became usable.


- **CONREQ 710 B / CONACK 718 B** (production `create_full` arguments) — the handshake encodes as
  serde JSON with every `Vec<u8>` field (pubkey, kex key, signature) as a *number array*, ~3.4×
  the ~200–250 B a binary layout of the same fields would need. Independently reproduced at 711 B
  for `ConReq::create_full(..).encode()` with production-shaped arguments (the 710/711 spread is
  just which boundary is measured — JSON body vs framed). On the wire it is 3 SAR fragments,
  752 B uncoded ≈ **192 s at BPSK31** (`hpx_hf`'s entry rung), 24 s at BPSK250; the frame alone,
  before SAR, is 182 s. Reticulum's
  announce carries comparable semantic content (identity keys + signature + metadata) in 167 B.
- **PQ CONREQ (Hybrid) 17 939 B** — 72 SAR fragments, ≈ 76 min at BPSK31. The binary content
  (ML-DSA-44 pubkey 1312 + sig 2420 + ML-KEM-768 ek 1184 + Ed25519 material) is ~5.0 KB ≈ 21.5 min
  at BPSK31, so JSON costs 3.6× even here. **Unwired**: `create_pq_conreq`/`encode_pq_conreq` have
  zero production callers today, so this is a latent cost, not a shipping one. Reticulum offers no
  help — it has no post-quantum story at all.
- **Signed `WireEnvelope` floor 168 B** (104 B header + 64 B Ed25519), of which 64 B is two full
  32-byte peer keys where Reticulum spends 16–32 B on truncated hashes. Mesh/relay transmit these.
- **Authenticated ACK: 5 B** with a 24-bit truncated keyed HMAC — already *tighter* than
  Reticulum's 20-B keepalive. Nothing to take on the ACK path.
- `SignedEnvelope` (OPSE, also JSON with the payload as a number array) has **zero production
  callers** — defined, not wired; listed so nobody wires it as-is.

**Worth a future detailed assessment (pointers, NOT validated conclusions):**

1. **Binary-pack the handshake.** The 3.4× JSON inflation is our defect, not their invention — but
   their 167 B/297 B figures are the external existence proof that identity + signature + metadata
   fits in ~¼ of our bytes with no security property traded. Cost: a wire-format break (permitted
   pre-1.0) and moving the signature from canonical-JSON to a defined canonical byte layout —
   arguably a hardening, since JSON canonicalisation is the fragile part.
2. **Send full identity once, address by hash after.** Reticulum transmits full public keys only in
   the announce; every subsequent packet carries a 16-B truncated hash, with keys learned from the
   announce cache. We re-send the full 32-B pubkey (and kex key, grid, profile, capability lists)
   in every CONREQ, and full 32-B src+dst ids in every envelope. We already own the two halves —
   `PeerCache` and the JS8 `@OPULSE` beacon (our announce) — the assessment is whether a cached-key
   fast path can shrink CONREQ/envelopes for known peers. Cost: truncated ids surrender the
   "peer_id IS the verifying key" self-authentication (`peer_descriptor.rs`) in wire form; a cache
   miss needs a full-identity fallback.
3. **Minimal link shape as a reference.** Their link = ephemeral X25519 in the request, identity
   signature only in the proof, per-link forward secrecy from ECDH. Our E7 `kex_pubkey` already
   does the ephemeral half; the delta worth studying is *not re-sending static identity material to
   a peer that has it*.

**Does NOT transfer:**

- **The routing/announce network model.** Announce flooding with a 2 %-of-bandwidth budget and
  128-hop retransmit caps presupposes a multi-node packet-switched network; we are point-to-point
  ARQ with explicit relay and JS8-slot discovery. Do not launder their mesh design into ours.
- **The crypto suite.** AES-256-CBC + HMAC-SHA256 and classical-only keys; nothing to adopt, and
  silence on the ML-DSA airtime problem, which is our actual hard one.
- **Ratchets.** Their per-destination ratchets serve link-less datagrams; our session-oriented ARQ
  already derives a per-session ephemeral key (E7).
- **RF/spectral signatures.** Nothing — the manual's only physical-layer demands are ≥ 5 bps and a
  500-B MTU. (Its periodic announces and 0.45 bps keepalives are channel-occupancy facts about
  running *their* network, not waveform guidance for ours.)

**Evidence tier:** manual pages only (`whatis` + `understanding`, read 2026-08-10) — no source
read, no deployment measurement; their byte counts are the project's own claims. Our-side byte
counts are measured at HEAD.

**Revisit for:** the binary-handshake calibration numbers when the pre-1.0 wire-format break is
scheduled; the announce/cache key-learning pattern if CONREQ shrinkage is taken up; the 297-B
3-packet link split as the minimal-handshake reference.

---

## CE-SSB and polar-SSB transmit conditioning

Sources studied for the TX signal-conditioning path (`openpulse_dsp::cessb`,
`ModemEngine::cessb_benefits`). These informed the *per-mode gate* — not code lifted
wholesale — and one was explicitly weighed and **rejected** for a data modem.

- **David L. Hershberger, W9GR — "Controlled Envelope Single Sideband"**, QEX
  Nov/Dec 2014 (pp. 3–13) + Jan/Feb 2016 external-processing follow-up. The origin of
  CE-SSB: a **baseband RF clipper → band-limit filter → overshoot compensator** chain
  that drives SSB modulator overshoot from ~61 % to ~1.3 % (~2.5× average power at
  fixed PEP). This is the method `openpulse_dsp::cessb` is named after.
  <http://www.arrl.org/files/file/QEX_Next_Issue/2014/Nov-Dec_2014/Hershberger_QEX_11_14.pdf>
- **Ron Economos, W6RZ — `drmpeg/gr-cessb`** (GNU Radio OOT, GPL-3.0). A concrete
  reference impl of the Hershberger chain: `clipper_cc` (memoryless magnitude clip,
  `mag ← min(mag, clip)`, phase preserved) → band-pass filter → `stretcher_cc`
  (overshoot compensator: windowed-max envelope over ±2 samples, then divide by
  `1 + 2·overshoot` where `overshoot = max(0, env·2√2 − 1)`), run at high oversampling
  and typically iterated twice. <https://github.com/drmpeg/gr-cessb>
  - **Considered and REJECTED for our data path.** The Hershberger/gr-cessb loop is
    tuned for *voice* SSB, where a few percent in-band distortion is inaudible and
    average-power/loudness is the objective. Its clip→filter→compensate loop injects
    **more** in-band EVM than our single-stage look-ahead limiter, which is exactly the
    quantity that breaks our dense data constellations (8PSK/QAM: tight decision
    regions). Adopting the aggressive iterative loop would *worsen* the very modes our
    gate already excludes. Our `cessb.rs` therefore stays a **single-pass look-ahead
    peak-stretch** (smooth gain from a windowed-max envelope, applied at passband, no
    hard-clip-then-refilter) — splatter-free by construction and gentler on EVM.
- **Kahn, 1952 — Envelope Elimination and Restoration (EER)**; **K1LI/K1KP — "The
  Polar Explorer"**, QEX Mar/Apr 2017; **PE1NNZ — "Direct SSB generation on a PLL"**
  (2013); **Dave's Hacks, Feb 2025 — polar modulation for uSDX/QMX**. The **polar/EER**
  family: split the signal into `A = √(I²+Q²)` and `φ = atan2(Q, I)`, differentiate φ
  for instantaneous frequency, and drive a switching (Class-E) PA's frequency +
  amplitude directly at RF. <https://www.pe1nnz.nl.eu.org/2013/05/direct-ssb-generation-on-pll.html>
  · <https://daveshacks.blogspot.com/2025/02>
  - **Not applicable to the current soundcard→linear-SSB-rig path** (the rig's linear PA
    already does this). Relevant only if we ever add a **direct-RF backend** for
    Class-E radios (QMX/uSDX) — a new hardware target, not a modem-DSP change.
  - **What we DID take — the theoretical basis for the per-mode gate.** Dave's Hacks
    derives, for a two-tone sum, `A = √(a² + 2ab·cos(Δω·t) + b²)`: as the two amplitudes
    approach equality the envelope passes through zero and the **instantaneous frequency
    goes discontinuous/unbounded**, so faithful reproduction needs the phase/amplitude
    paths to carry ~5× the signal bandwidth. This is the *equal-amplitude singularity*,
    and it is precisely why envelope conditioning helps high-PAPR OFDM-QPSK (a
    near-Gaussian envelope that rarely nulls hard) but hurts single-carrier QAM and
    higher-order OFDM subcarriers (constellations that transit near the origin, where
    the envelope nulls and the phase jumps). It converts our empirically-tuned
    `cessb_benefits` gate into a **principled** one: benefit ⇔ high-PAPR envelope **and**
    loose decision margins. See `ModemEngine::cessb_benefits`.

- **FreeDV 700D symbol diversity** (`drowe67/codec2`) — transmit each carrier's symbol
  twice across the band for a weak-signal mode below the current SL floor. **Measured and
  rejected for OpenPulseHF (#864, 2026-07-14):** the ρ=0 ideal cleared the kill-gate
  (~4 dB on slow fade) but the real dual-carrier waveform's ~2.6 dB two-tone PAPR consumes
  the ~1–2.6 dB matched-power gain → net on-air ≈ break-even at 2× bandwidth, dominated by
  baud-drop and HARQ. See `docs/dev/research/weak-signal-diversity-measurement.md`. A
  purpose-built *robust narrowband* waveform (cf. MODEM73's ROBUST family, below) is the
  better direction if a sub-floor rung is ever revisited.

---

## Recurring lesson — energy is not a frame detector

From the 2026-07-31 adversarial comparison, prompted by a defect family that is really **one
mechanism patched five times** (#1020 → #1021 → #1039 → #1040 → #1045):

**Every reference modem decides "a frame starts here" on preamble correlation — normalized in three
of four — and derives its frequency estimate from the same measurement that declared the detection.**
Energy appears in these systems only as a CSMA/occupancy question (Mercury `channel_busy.c`) or as a
divide-by-zero floor (modem73 `min_R`). **Corrected 2026-08-02:** modem73 *does* carry
condemnation-adjacent machinery — a failed-collect position memory that refuses to re-sync within
half a symbol of a position that just failed (`mfsk_modem.hh:520`), spent-anchor suppression, lock
preemption by a better-quality candidate, and backward rescue off a trail sequence. The difference is
not that being wrong never needs recovery; it is that every recovery keys off a *normalised-quality
detection* and a *sample count*, never energy and never a wall clock. The claim below overstates it.
None of them has any analog of our settle → micro-sweep →
condemn → re-anchor apparatus, because false detections are *rare* under a ρ threshold, *validated
cheaply and early* (FreeDV unique word, modem73 polar/CRC meta symbol), and *recovered from by discarding
the samples that caused them* (FreeDV zeroes `rxbuf`).

The uncomfortable part: **we already own the primitive.** `IqMatchedFilter::search_normalized`
(`crates/openpulse-dsp/src/acquisition.rs`) is used by the SC-FDMA and pilot plugins, and our own
playbook already states the rule — *"acquire on the normalised correlation, not the unnormalised
score"*. The engine's receive loop calls it **zero times**. Single-carrier frame location is
anchored on `EnergyGate` → `refine_onset` (itself an energy test) → AFC settle → decode. Nothing in
the code or the docs marks that as a considered choice; it predates the correlation work and survived
because the in-process fixtures handed the receiver a buffer that *was* the frame (the same blind
spot `route_embedded` was added to close).

**Where the energy gate is defensible:** as an O(1) compute gate and DCD — deciding whether to spend
the expensive settle, and whether the channel is busy enough to defer a transmission. Mercury keeps
an energy detector for exactly that. What four deployed implementations and five of our own patch
rounds agree is *not* defensible is using it as the frame-start decision, and worse as the trigger
for frequency estimation.

**One shared weakness, worth knowing:** qo100-modem's acquisition threshold is also absolute rather
than normalized — but on *correlation magnitude*, so it fails **closed** (a frame is missed) where an
absolute *energy* threshold fails **open** (noise is accepted and the AFC is poisoned). The
failure-polarity difference is the whole ballgame. And Mercury's own "S1" bug — an OLLA hold frozen
by healthy SNR, blocking every downgrade above the fade cliff — is the same *shape* as our #1021
recovery livelock, which is independent confirmation that *a recovery must change the input to the
failed decision* is a law rather than a local lesson.

### Outcome (#1049, 2026-07-31) — and the limit the comparison did not show

Shipped: the AFC settle is now corroborated by normalised preamble correlation, which removes the
settle-on-**noise** class outright. It does **not** improve hot-floor acquisition — the residual
settles there are leading-edge, not noise, and the onset-snap stage that would fix them was measured
and rejected (an alternating preamble is periodic, so the same search misplaces a correct onset by
two symbols). Implementing it surfaced two constraints that reading the references could not, and
both qualify the recommendation above.

**We imported the detection *statistic* without the sync-word *property* that makes it work for
them.** codec2's sync sequence is designed for correlation detection. Ours is 32 BPSK symbols whose *bits*
alternate — NRZI makes the symbols `--++` repeating, period 4 — a square-wave-modulated carrier
whose energy sits in lines at `fc ± baud/4` and odd harmonics (corrected 2026-08-03; this said
`baud/2`, twice the true spacing). Measured
ρ of a pure tone against that template, by residual-frequency search width:

| grid half-width | ρ of a pure tone |
|---|---|
| ±20 Hz (shipped) | 0.017–0.042 |
| ±160 Hz | 0.659 |
| ±450 Hz (full acquisition range) | 0.696 at every frequency |

At ±160 Hz a **birdie outscores our best real on-air frame** (0.654). The alternation is what saves
the narrow grid — a steady carrier cancels between the +1 and −1 half-symbols — and that protection
disappears as soon as the search can rotate a spectral line onto plain carrier.

**Therefore the codec2 *ordering* — grid the full acquisition range as the detector, then seed the
frequency estimate from the winner — is rejected for our waveform**, measured, not assumed. It is
the design the comparison above most naturally implies, and it is the one that cannot distinguish a
birdie from a preamble. Our second OTA rig was blocked by PC/USB birdies, so this is not theoretical.

The correlation gate therefore closes the *broadband* noise case (which is what #1020/#1021/#1039/
#1040/#1045 actually were) and leaves the structured-interferer case to the existing condemnation
recovery. **The ceiling is the preamble, not the detector.** Matching the references properly would
mean adopting a PN or chirp sync word — a wire-format change on both ends, and the real follow-on
item.

---

### Outcome (#1053, 2026-08-02) — checked against Mercury + modem73 at source, and it changed the design

The QPSK extension of the #1049 veto was compared against both projects **read as source**, not from
this doc's earlier summaries. Three durable results:

1. **Per-template thresholds are the deployed norm, and our own doc was the false premise.** codec2's
   `timing_mx_thresh` is per-mode 0.08–0.5 *upstream* (spread by template tone-likeness at constant
   preamble duration); modem73 gates its known-sequence probes per geometry (0.78/0.60, 0.62/0.40).
   The `PreambleTemplate`-carries-its-constants API (PR #1057) matches practice. The contrary belief
   — "codec2 uses one global constant" — came from **this document quoting only the struct default**.
2. **No reference lets the sync word scale with symbol rate, and that is why our thresholds would not
   close.** codec2 keeps a ~110 ms PN preamble regardless of payload rate, paying **33 % of the burst
   on datac14** without complaint. Ours is a period-4 run of 32 symbols on BPSK; the 16-symbol QPSK preamble is a *designed* sequence, not an alternating run, so this comparison covers BPSK only. BPSK250 is 124 ms but QPSK1000 shrinks to 15 ms, and the gap
   between the noise ceiling and the decode cliff closes from both sides. The QPSK threshold table was
   withdrawn for exactly that reason (see the ledger entry for #1053). **The fix is #1052's
   wire-format change extended to decouple sync *duration* from symbol rate — not more threshold
   tuning, which cannot buy processing gain a template does not have.**
3. **No reference consults a wall clock for a detection decision.** All budgets are sample counts over
   O(1)-per-sample streaming detectors. Our wall-clock retry latch (#1058) is a defensible real-time
   viability test *given* an O(buffer) rescan primitive — and the reference answer is to not own that
   primitive. The open question #1058 leaves is therefore not "why do we consult a clock" but "why are
   we scanning a growing buffer for a frame at all".

Also transferable, and cheap: modem73's MFSK preamble is alternating tones **terminated by a
2-symbol ordered unique word**, which is precisely the structure whose absence made our onset-snap
ambiguous in #1052 — alternation for run detection, an aperiodic terminator for placement.

## Recurring lesson

The three **single-carrier** references above (gnuradio, qo100-modem,
SSB_HighSpeed_Modem) all use **RRC-shaped pulses** and a **dedicated frequency
acquisition stage** (FLL or coarse preamble-correlation CFO) ahead of phase
recovery. OpenPulseHF's rectangular-pulse PSK modes with a single Costas loop are
the outlier; the carrier-offset robustness gaps (8PSK) trace directly to that.
Mercury takes the other route entirely — **OFDM with cyclic prefix + pilots**
instead of RRC + FLL — which is the architecture of our OFDM higher-order ladder.
