---
project: openpulsehf
doc: docs/openpulse-manual.md
status: living
last_updated: 2026-07-18
---

# OpenPulseHF Complete Manual

This manual is the operator, integrator, and engineering reference for OpenPulseHF.
It is written as a practical guide first and a technical reference second.

It describes **v0.15.0**. See [docs/releasenotes.md](releasenotes.md) for the per-release history.

> **Interop notice — v0.15.0 requires both ends to be v0.15.0.**
> From v0.15.0 the transmitter opportunistically upgrades `Rs` to the stronger `RsStrong`
> (RS(255,191), t=32) on any frame where the stronger code fits the same number of 255-byte RS
> blocks — free on the wire, and it roughly doubles how often the weak rungs decode on a fade
> (BPSK31 at 3 dB: 0.25 → 1.00). A v0.15.0 receiver tries both codes and lets the CRC decide; a
> **pre-v0.15.0 receiver does not**, so a v0.15.0 station transmitting to an older one can lose
> small frames on the weak rungs. Update both ends of a link.
> Implementation: `openpulse_core::fec::free_rs_strengthening`, applied in
> `ModemEngine`'s transmit path and dual-decoded on receive.

Safety and compliance note:
- OpenPulseHF can run modes and features that are not legal in all jurisdictions or all bands.
- You are responsible for selecting legal frequencies, occupied bandwidth, power, and identification behavior for your country and license class.
- Review [docs/regulatory.md](regulatory.md), [docs/regulatory-compliance-checklist.md](regulatory-compliance-checklist.md), and [Chapter 11](#chapter-11-compliance-notes) before on-air operation.

---

## Table of Contents

1. [Chapter 1: Specification](#chapter-1-specification)
2. [Chapter 2: Quick Start by Use Case](#chapter-2-quick-start-by-use-case)
3. [Chapter 3: Detailed Standard Manual (Core Modem)](#chapter-3-detailed-standard-manual-core-modem)
4. [Chapter 4: Advanced Manual](#chapter-4-advanced-manual)
5. [Chapter 5: Technical Manual (Architecture and Implementation)](#chapter-5-technical-manual-architecture-and-implementation)
6. [Chapter 6: Comparison and Advantages](#chapter-6-comparison-and-advantages)
7. [Chapter 7: Collaboration and Plugins](#chapter-7-collaboration-and-plugins)
8. [Chapter 8: Non-GPLv3 Interfaces](#chapter-8-non-gplv3-interfaces)
9. [Chapter 9: Support and Funding](#chapter-9-support-and-funding)
10. [Chapter 10: Glossary and References](#chapter-10-glossary-and-references)
11. [Chapter 11: Compliance Notes](#chapter-11-compliance-notes)
12. [Chapter 12: Configuration File Overview](#chapter-12-configuration-file-overview)
13. [Chapter 13: Binary and Test-Rig Reference](#chapter-13-binary-and-test-rig-reference)

---

## Chapter 1: Specification

Cross-reference:
- Deployment basics are in [Chapter 2](#chapter-2-quick-start-by-use-case) and [Chapter 3](#chapter-3-detailed-standard-manual-core-modem).
- Advanced operational features are in [Chapter 4](#chapter-4-advanced-manual).
- Deep implementation details are in [Chapter 5](#chapter-5-technical-manual-architecture-and-implementation).
- Regulatory constraints and references are in [Chapter 11](#chapter-11-compliance-notes).

### 1.1 Product Scope

OpenPulseHF is a Rust workspace implementing:
- A plugin-based modem stack (multiple modulation families)
- Adaptive ARQ/session behavior (HPX)
- Multiple FEC and compression strategies
- Signed transfer and trust workflows
- Interop surfaces (ARDOP TCP, KISS TCP, B2F/Winlink gateway)
- Advanced features (QSY, relay, mesh, daemon/panel/TUI)

### 1.2 Mode and Waveform Specification

The modulation catalog spans 10 plugin families:

- **BPSK** (`bpsk`) — BPSK31, BPSK63, BPSK100, BPSK250, BPSK250-RRC; weak-signal to narrowband HF. Differentially (NRZI) decoded, which is why it survives HF fading where coherent PSK does not.
- **QPSK** (`qpsk`) — QPSK125/250/500/1000/2000/9600 with `-RRC` and `-HF` variants, plus the **differential** `QPSK250-D` / `QPSK500-D`.
- **8PSK** (`psk8`) — 8PSK500/1000/2000/9600, with `-RRC` variants and the `8PSK1000-HF`/`-HF-RRC` pair; Gray-coded.
- **64QAM** (`64qam`) — 64QAM500/1000/2000-RRC; max-log-MAP soft demodulator.
- **FSK4** (`fsk4`) — FSK4-ACK; the ACK control channel only.
- **MFSK16** (`mfsk16`) — `MFSK16` and `MFSK16-ACK`: constant-envelope non-coherent 16-GFSK weak-signal sub-floor waveform (REQ-WSIG-01). Self-acquiring, needs no carrier phase; it is `hpx_hf`'s deep-fade rung SL1.
- **OFDM** (`ofdm`) — OFDM16/52 (QPSK) plus the OFDM52 higher-order ladder (8PSK/16QAM/32QAM/64QAM — the HF high-throughput path).
- **SC-FDMA** (`scfdma`) — SCFDMA16/52 (QPSK), the SCFDMA26/52 higher-order ladders, SCFDMA52-64QAM(-P4).
- **Pilot** (`pilot`) — `PILOT-{QPSK,8PSK,16QAM,32APSK}{500,1000}` plus their `-RRC` variants and `2000-RRC`; pilot-framed single-carrier with pilot-aided carrier recovery (cycle-slip-immune, sample-rate-offset-robust); soft-capable. Four ladders: `hpx_pilot` (500 rect), `hpx_pilot_rrc` (narrowband), `hpx_pilot_fast` (1000 baud), `hpx_pilot_fast_rrc` (fast + narrowband).
- **JS8** (`js8`) — JS8-compatible 8-GFSK weak-signal waveform used by the station-discovery and rendezvous subsystem ([§4.9](#49-js8-station-discovery-and-rendezvous)), not by the data ladder. Registered in the daemon, not in the CLI's data-mode registry.

The plain rectangular `QPSK2000`/`8PSK2000` are registered but **RRC-superseded** (use `-RRC`). For the authoritative per-mode table (baud, bits/symbol, gross bps, occupied bandwidth) see the [README modulation-modes table](../README.md#modulation-types); for the HF mode/FEC selection ladder see [mode-fec-ladder.md](mode-fec-ladder.md). `openpulse modes` prints the live registry.

> **`PILOT-*` immunity is to carrier/sample-rate offset, not to fade.** Measured on Watterson
> `moderate_f1`, `PILOT-QPSK500+Rs` decodes 0 % at 40 dB while being perfect on AWGN down to 10 dB.
> Use the pilot family for offset-heavy paths, not for a fading HF path.

#### 1.2.1 The `hpx_hf` ladder (primary HF profile)

`hpx_hf` is the profile for a real ≤2700 Hz HF SSB channel; it is `SL1–SL14` and **every rung is
FEC-coded** (on a fade there is no useful uncoded rung — an uncoded BPSK31 entry rung decoded 0 % of
fading frames at every SNR tested). Above SL6 the ladder is OFDM: the coherent single-carrier mid
rungs it replaced decoded ~0 % on `moderate_f1` at any SNR up to 40 dB, and neither FEC nor
differential encoding rescues them (differential does not scale to 8PSK). Source of truth:
`SessionProfile::hpx_hf` in `crates/openpulse-core/src/profile.rs`.

| SL | Mode | FEC | SNR floor (dB) | Note |
|---|---|---|---|---|
| 1 | `MFSK16` | `Rs` | — | non-coherent sub-floor deep-fade rung |
| 2 | `BPSK31` | `Rs` | 3 | `initial_level` |
| 3 | `BPSK63` | `Rs` | 4 | |
| 4 | `BPSK100` | `Rs` | 4.5 | |
| 5 | `BPSK250` | `Rs` | 5 | |
| 6 | `QPSK250-D` | `Rs` | 7 | differential — coherent QPSK250 is 0 % on a fade |
| 7 | `OFDM52` | `SoftConcatenated` | 9 | first multicarrier rung |
| 8 | `OFDM52-8PSK` | `SoftConcatenated` | 10 | |
| 9 | `OFDM52-16QAM` | `SoftConcatenated` | 12 | |
| 10 | `OFDM52-32QAM` | `SoftConcatenated` | 14 | |
| 11 | `OFDM52-64QAM` | `SoftConcatenated` | 16 | |
| 12 | `OFDM52-16QAM` | `LdpcHighRate` | 18 | MODCOD: same modulation, r≈8/9 |
| 13 | `OFDM52-32QAM` | `LdpcHighRate` | 19 | |
| 14 | `OFDM52-64QAM` | `LdpcHighRate` | 20 | ladder top |

Two things an operator should know about these numbers:

- **The SNR floors are on two different scales, by physical necessity.** SL2–SL6 are single-carrier
  and read approximately true channel SNR; SL7–SL14 are OFDM, whose zero-forcing equalizer saturates
  the estimate near ~16 dB and physically cannot report the 20–30 dB the dense rungs run at. Do not
  compare a floor on one scale against a reading from the other, and do not try to "unify" the
  estimators — the gate `cargo test -p openpulse-modem --test snr_scale_boundary` pins this.
- **The controller climbs on decode evidence, not only on SNR.** At low baud a 1 Hz fade decorrelates
  in a few symbols, so no measurement window both tracks the fade and averages the noise, and the SNR
  estimate flattens into a constant. The receiver-led rate controller
  (`crates/openpulse-core/src/ota_rate.rs`) therefore also climbs after `ACK_CLIMB_THRESHOLD` (3)
  consecutive clean decodes, and never demotes below a level that just decoded.

> **Validation status.** The whole HF-fade arc (v0.13.0–v0.15.0) is validated against the
> **Watterson channel simulator only** (`openpulse-channel`), not on the air. Real-RF validation is
> still an open gate — see [§1.7](#17-current-non-code-gate-status).

### 1.3 FEC Specification

Available FEC/session protection modes include:
- None
- RS (Reed-Solomon)
- RS-Interleaved
- Concatenated
- ShortRS
- RsStrong
- SoftConcatenated
- LDPC (rate-1/2 min-sum belief propagation; soft input)
- LDPC high-rate (rate-8/9 PEG; soft; auto-selected on dense high-SNR rungs)
- Turbo (rate-1/3 PCCC; soft)

Operational guidance:
- HF bursty channels usually favor RS/concatenated profiles; the dense modes require a **soft** code.
- High-quality links can use lighter overhead profiles.
- **Opportunistic `RsStrong` (v0.15.0).** Where the profile selects `Rs`, the transmitter upgrades to
  `RsStrong` per frame **only when the stronger code occupies the same number of 255-byte RS blocks**,
  so airtime can never regress; the receiver tries both. See the interop notice at the top of this
  manual — both ends must be v0.15.0.
- **`RsInterleaved` is inert on a single-block payload.** A payload of ≤223 bytes is one RS codeword,
  and a single codeword is position-agnostic, so there is nothing for the interleaver to spread
  (measured: identical decode rates to bare `Rs`). Code *strength* is the lever at that size;
  interleaving earns its keep only on multi-block payloads.

### 1.4 Compression Specification

Session-layer compression in `openpulse-core` (`compression.rs`):
- None
- LZ4 (block format, 4-byte LE size prefix)
- Zstd with an embedded HPX dictionary (dictionary ID carried to catch version skew)

Behavior:
- Compression can be negotiated in signed handshake context.
- `compress_if_smaller()` picks the smaller of LZ4/Zstd and falls back to the original bytes.

This is distinct from the B2F/Winlink message compression in [§2.7](#27-winlinkb2f-path): B2F Type D
is Gzip, and **B2F Type C (LZHUF) is not supported** — the implementation was deleted in v0.15.0 and
an inbound Type C proposal is answered `Reject`.

### 1.5 Session, Security, and Trust Specification

Security and trust layers include:
- Ed25519-signed handshake and signed transfer manifest verification
- Post-quantum handshake support (ML-DSA-44 signatures, ML-KEM-768 key encapsulation)
- Hybrid signing mode (classical + PQ)
- Trust classification and policy profiles

### 1.6 Advanced Feature Specification

- QSY frequency agility (signed, freshness-stamped frame exchange authenticated to the handshake
  peer; candidate scanning and switch coordination)
- Multi-hop relay and query propagation
- Mesh daemon with peer discovery and cache
- Cross-band repeater support with dual-rig model (`rig_a` receive side, `rig_b` transmit side)
- IQ output mode for SDR workflows
- FreeDV authenticated voice shim components (signed beacon, UDP data-port transport, verdict socket)
- Direct P2P file transfer over RF (`OPFX`) — see [§4.8](#48-direct-p2p-file-transfer-opfx)
- JS8-based station discovery and rendezvous — see [§4.9](#49-js8-station-discovery-and-rendezvous)
- Simultaneous multi-mode receive monitor (`[monitor]`), receiver AGC and auto-notch
- Optional GPU acceleration paths in supported plugin flows

### 1.7 Current Non-Code Gate Status

Code implementation is broad and test-backed. Two gates remain open, and both need real radios:

- **On-air regulatory validation/reporting workflow** (station-ID audit, compliance report).
- **On-air validation of the HF-fade work.** Everything in [§1.2.1](#121-the-hpx_hf-ladder-primary-hf-profile)
  is measured against the Watterson channel simulator only.

Release readiness is tracked in [docs/dev/project/release-1.0-criteria.md](dev/project/release-1.0-criteria.md).
A reproducible SPDX software bill of materials is committed as `SBOM.spdx.json` and regenerated with
`scripts/generate-sbom.sh`.

---

## Chapter 2: Quick Start, by Use Case

Cross-reference:
- For complete CLI and architecture details, continue to [Chapter 3](#chapter-3-detailed-standard-manual-core-modem) and [Chapter 5](#chapter-5-technical-manual-architecture-and-implementation).

### 2.1 Basic Local Validation (No RF Hardware)

```bash
cargo build --release
openpulse modes
openpulse devices
openpulse --backend loopback transmit "CQ CQ TEST" --mode BPSK100
openpulse --backend loopback receive --mode BPSK100
```

### 2.2 Mode Recommendation by SNR

```bash
openpulse mode-advisor --snr 12
openpulse mode-advisor --snr 20
```

### 2.3 Session Metrics Export

```bash
openpulse session-metrics --format json
openpulse session-metrics --format text
```

### 2.4 Monitor Engine Events (NDJSON)

```bash
openpulse monitor --mode BPSK100
openpulse monitor --mode QPSK500
```

### 2.5 ARDOP TNC Drop-In Path

```bash
openpulse --backend loopback receive --mode BPSK250
openpulse --backend loopback transmit "ARDOP-path smoke test" --mode BPSK250
```

Use this smoke test to validate equivalent modem behavior before connecting external ARDOP-compatible clients through dedicated bridge services.

**Adaptive ARQ (opt-in).** By default the ARDOP TNC runs fixed-mode, and the host hints `ARQBW` /
`ARQTIMEOUT` are accepted-and-echoed but inert. Enable the adaptive ARQ session to make them
effective — the rate ladder then adapts, `ARQBW` caps it to a max occupied bandwidth, and
`ARQTIMEOUT` drops an idle connection:

```toml
[ardop]
enable_adaptive_arq = true       # default false (fixed-mode)
adaptive_profile = "hpx500"      # ladder profile; empty falls back to hpx500
```

### 2.6 KISS TNC Drop-In Path

```bash
openpulse --backend loopback receive --mode BPSK250
openpulse broadcast --payload "KISS-path smoke test" --mode BPSK250 --ttl 1 --callsign N0CALL
```

Use this baseline modem check before attaching APRS/AX.25 software through dedicated KISS bridge services.

### 2.7 Winlink/B2F Path

Winlink traffic does not go through the `openpulse` CLI. Two binaries carry it:

```bash
# Direct TCP to a Winlink CMS (no radio) — ISS proposals + IRS reply in one connection
openpulse-gateway --host cms.winlink.org --port 8772 --callsign N0CALL \
  send --to K5ABC --subject "Test" --message "Hello over OpenPulse"

# ARDOP-compatible TNC for Pat and other Winlink clients (over RF)
openpulse-tnc --bind 127.0.0.1 --cmd-port 8515 --data-port 8516 --mode QPSK500
```

Protocol notes an operator should know:

- **Only B2F Type D (Gzip) is supported.** Type C (LZHUF) was deleted in v0.15.0 — the shipped
  implementation used LHA `LH5` where FBB/Winlink use classic Okumura LZHUF, a different bitstream,
  so it could never have interoperated. An inbound Type C proposal is answered `Reject`, leaving the
  peer free to re-propose as Type D. The CMS gateway path is unaffected; it has always used Type D.
- **A fully-rejected transfer is now an error, not a success.** `B2fDriver::run_iss` returns
  `DriverError::AllProposalsRejected { count }` where it previously returned `Ok(())` for a session
  in which the peer accepted nothing; a refused connection surfaces as `DriverError::Aborted`.
  If you drive the library yourself and treated `Ok` as "sent", that check was wrong before.
- **Session limits** (v0.15.0 hardening, `crates/openpulse-b2f/src/session.rs`): at most 32 retained
  proposals (the overflow is counted and answered `Reject`, not stored), a session-wide ceiling of
  8192 frames that terminates a proposal flood, 16 MiB decompressed per message **and** 32 MiB
  aggregate per session, and caps on repeated `To:` (64) and `File:` (128) header fields. The driver's
  reads run against a single deadline per operation rather than a per-syscall timeout, and the command
  port caps a line at 4096 bytes.

### 2.8 Mesh Daemon Start

```bash
openpulse beacon --mode BPSK250 --interval 60 --callsign N0CALL --ttl 1
openpulse broadcast --payload "mesh relay probe" --mode BPSK250 --ttl 3 --callsign N0CALL
```

Set TTL and callsign values to match your mesh policy and local operating plan.

### 2.9 Operator UI Path

```bash
openpulse monitor --mode BPSK100
openpulse session state --diagnostics --format text
openpulse session log --follow --follow-timeout-ms 5000
```

### 2.10 FreeDV Authenticated Voice Use Case

Use this path when voice traffic is carried by FreeDV and you want cryptographic station-identity beacons alongside voice frames.

Current implementation model:
- FreeDV authentication support is provided by the `openpulse-freedv-auth` crate.
- It exposes reusable components (`AuthBeacon`, `FreeDvDataPort`, `BeaconScheduler`, `VerdictServer`) for companion integrations.
- The data path targets the FreeDV UDP data-port workflow, with verification verdict exported over a local Unix socket.

Operational integration checklist:
- Keep station key material protected and trust-store entries maintained.
- Inject signed beacons at a conservative repeat interval suitable for your station policy.
- Display or log local verdict state (`verified` / `unverified` / `invalid`) in your operator UI.
- Treat missing/invalid beacons as identity failures for policy-sensitive operation.

Reference material:
- Research and design notes: [docs/dev/research/freedv-auth-research.md](dev/research/freedv-auth-research.md)
- Implementation crate: `crates/openpulse-freedv-auth`
- Integration tests: `crates/openpulse-freedv-auth/tests/freedv_auth_integration.rs`

### 2.11 CI-Compatible Validation Path

```bash
cargo build --workspace --no-default-features
cargo test --workspace --no-default-features
```

### 2.12 PKI Tooling Service Quick Start

Use this subchapter when you need a local trust-bundle service for identity, moderation,
and trust publication workflows.

Startup requirements:
- `DATABASE_URL` must point to a reachable PostgreSQL instance.
- `PKI_API_KEY` must be non-empty for mutating endpoints.
- `PKI_SIGNING_KEY` should be a base64-encoded 32-byte Ed25519 seed for persistent deployments.
- `PKI_ALLOW_EPHEMERAL_KEY=true` is development-only and should not be used for persistent trust history.
- `PKI_BIND_ADDR` is optional and defaults to `127.0.0.1:8080`.

Example startup:

```bash
export DATABASE_URL='postgres://openpulse:openpulse@127.0.0.1:5432/openpulse_pki'
export PKI_API_KEY='replace-with-strong-token'
export PKI_SIGNING_KEY='replace-with-base64-32-byte-seed'
export PKI_BIND_ADDR='127.0.0.1:8080'

pki-tooling
```

Example read-only checks (no bearer token required):

```bash
curl -s http://127.0.0.1:8080/healthz | jq .
curl -s http://127.0.0.1:8080/api/v1/signing-key | jq .
curl -s http://127.0.0.1:8080/api/v1/trust-bundles/current | jq .
```

Example public submission intake (intentionally unauthenticated endpoint):

```bash
curl -sS -X POST http://127.0.0.1:8080/api/v1/submissions \
	-H 'Content-Type: application/json' \
	-d '{
		"payload_type": "identity-assertion",
		"payload": {
			"callsign": "N0CALL",
			"pubkey": "BASE64_ED25519_PUBKEY"
		}
	}' | jq .
```

Example trust-bundle publication and promotion (requires bearer token):

```bash
bundle_id=$(curl -sS -X POST http://127.0.0.1:8080/api/v1/trust-bundles \
	-H "Authorization: Bearer ${PKI_API_KEY}" \
	-H 'Content-Type: application/json' \
	-d '{
		"schema_version": "1.0",
		"generated_at": "2026-05-28T00:00:00Z",
		"issuer_instance_id": "manual-example",
		"signing_algorithms": ["ed25519"],
		"records": []
	}' | jq -r '.bundle_id')

curl -sS -X PATCH "http://127.0.0.1:8080/api/v1/trust-bundles/${bundle_id}/promote" \
	-H "Authorization: Bearer ${PKI_API_KEY}" \
	-H 'Content-Type: application/json' \
	-d '{"reason":"manual quickstart"}' | jq .
```

For full endpoint contracts and operational guidance, see:
- [docs/non-gpl-interfacing.md](non-gpl-interfacing.md#pki-tooling-rest-api)
- [docs/dev/pki/pki-tooling-api.md](dev/pki/pki-tooling-api.md)
- [docs/dev/pki/pki-tooling-operations-runbook.md](dev/pki/pki-tooling-operations-runbook.md)

---

## Chapter 3: Detailed Standard Manual (Core Modem)

This chapter focuses on standard modem operation and common deployment patterns.

Cross-reference:
- Advanced flows (QSY/relay/mesh/repeater/signed workflows) are in [Chapter 4](#chapter-4-advanced-manual).
- Plugin authoring and validation strategy are in [Chapter 7](#chapter-7-collaboration-and-plugins).

### 3.1 Component Model

Core execution components:
- Core protocol/types: `openpulse-core`
- Audio backends: `openpulse-audio`
- Modem execution pipeline: `openpulse-modem`
- User CLI: `openpulse-cli`
- Modulation plugins: `plugins/*`

Standard execution chain:
1. Payload enters CLI/session path
2. Frame encoding and optional FEC/compression
3. Plugin modulates samples
4. Audio backend emits/captures samples
5. Plugin demodulates and frame validation occurs
6. Decoded payload returned to user/output surface

### 3.2 Standard Use Cases

#### A. Interactive transmit and receive

```bash
openpulse --backend default transmit "status report" --mode BPSK250
openpulse --backend default receive --mode BPSK250
```

#### B. Session lifecycle operations

```bash
openpulse session start --peer W1AW
openpulse session state --diagnostics --format json
openpulse session log --follow --follow-timeout-ms 5000
openpulse session end
```

#### C. Benchmark and regression evaluation

```bash
openpulse benchmark run
openpulse benchmark run --min-pass-rate 1.0 --max-mean-transitions 20.0
```

#### D. Broadcast and beacon patterns

```bash
openpulse broadcast --payload "network status" --mode BPSK250 --ttl 2 --callsign N0CALL
openpulse beacon --mode BPSK250 --interval 600 --callsign N0CALL --ttl 1
```

### 3.3 Identity and Trust Workflows

#### Identity diagnostics

```bash
openpulse identity show N0CALL
openpulse identity verify N0CALL
openpulse identity cache
```

#### Trust operations

```bash
openpulse trust show N0CALL
openpulse trust explain N0CALL
openpulse trust list
openpulse trust policy show
openpulse trust policy set balanced
```

#### Trust store update example

```bash
openpulse trust import --station-id N0CALL --key-id ed25519:abcd1234 --trust full --source out_of_band
openpulse trust revoke --station-or-key N0CALL --reason operator_revoked
```

### 3.4 Diagnostics and Verification

```bash
openpulse diagnose handshake --peer W1AW --format json
openpulse diagnose manifest --session demo-session --format json
openpulse diagnose session --peer W1AW --format text
```

### 3.5 Standard Configuration Baseline

Use template:

```bash
openpulse config init > ~/.config/openpulse/config.toml
```

Recommended baseline:
- Conservative fixed HF mode to start (`[modem] mode = "BPSK250"`, the built-in default)
- Adaptive profile `[modem] profile = "hpx_hf"` (also the default) for HF; `hpx500` for a narrow
  ≤600 Hz channel. The `hpx_wideband*`/`hpx_narrowband*` profiles exceed the 2700 Hz HF channel and
  are for FM/VHF/UHF links only.
- Explicit backend selection
- Trust policy set to balanced/strict according to operation
- Logging level set to `info` for field work, `debug` for issue triage

### 3.6 Interoperability Cookbook

This section focuses on interoperability checks for existing TNC/modem endpoints while keeping upstream client software workflows unchanged where practical.

#### A. ARDOP-compatible interoperability path

```bash
openpulse --backend loopback receive --mode BPSK250
openpulse --backend loopback transmit "ARDOP replacement validation" --mode BPSK250
```

Typical client expectations to validate:
- Command port responds to VERSION/STATE/LISTEN/CONNECT sequence.
- Data port framing remains u16 big-endian length-prefixed binary payloads.
- Disconnect and reconnect handling preserves client workflow.

#### B. KISS/AX.25 TCP interoperability path

```bash
openpulse --backend loopback receive --mode BPSK250
openpulse broadcast --payload "AX25 replacement validation" --mode BPSK250 --ttl 1 --callsign N0CALL
```

Validation checklist:
- KISS FEND/FESC byte-stuffing behavior is preserved.
- AX.25 UI frame payloads round-trip correctly.
- Multi-frame client sessions do not deadlock under reconnect.

Operational note:
- Treat these as protocol-compatibility checks rather than a formal certification of third-party client behavior in every deployment.

#### C. Standard operational script examples

```bash
# station startup
openpulse --backend default receive --mode BPSK250

# second terminal: transmit smoke test
openpulse --backend default transmit "OPHF link check" --mode BPSK250

# diagnostics snapshot
openpulse session state --diagnostics --format json
openpulse trust policy show
openpulse session-metrics --format json
```

Cross-reference:
- For advanced transport behaviors (relay/mesh/QSY/signed workflows), continue to [Chapter 4](#chapter-4-advanced-manual).
- For implementation-level protocol and engine details, continue to [Chapter 5](#chapter-5-technical-manual-architecture-and-implementation).

---

## Chapter 4: Advanced Manual

This chapter covers advanced behaviors and high-leverage workflows.

### 4.1 QSY Mode (Frequency Agility)

Purpose:
- Stations negotiate migration to better frequencies based on candidate scans.

Protocol frames:
- `QSY_REQ`
- `QSY_LIST`
- `QSY_VOTE`
- `QSY_ACK`
- `QSY_REJECT`

CLI examples:

```bash
openpulse qsy status
openpulse qsy init --rig 127.0.0.1:4532
```

Config example:

```toml
[qsy]
enabled = true
allow_trustlevels = ["verified", "psk_verified"]
bandplan_mode = "ham-iaru-r1"
bandplan_awareness_enabled = true
enforce_max_channel_width = true
enforce_segment_conventions = true
candidate_freqs_hz = [7074000, 7076000, 7104000]
scan_dwell_ms = 500
switchover_offset_s = 5
```

### 4.2 Relay Mode

Purpose:
- Forward traffic with hop controls and trust-aware policies.

Key behaviors:
- Hop-limit enforcement
- Duplicate suppression
- Route scoring and best-route selection
- Envelope-origin authentication (a forged or unsigned `src_peer_id` is rejected at the relay)

There is **no `openpulse relay` CLI subcommand**. Relay is configured in `config.toml` and runs
inside the daemon, the mesh daemon, and the ARDOP/KISS bridges when enabled:

```toml
[relay]
enabled = true
max_hops = 3
store_forward_ttl_s = 300
deny_list = []
allow_list = []
```

Observe it through the event stream (`openpulse monitor`, or the daemon's NDJSON control port),
which is where forwarding, hop-limit, duplicate-suppression, and policy-rejection events surface.

Use with:
- Mesh daemon for networked propagation
- Policy and trust tuning in config

### 4.3 Mesh Mode

Start:

```bash
openpulse beacon --mode BPSK250 --interval 60 --callsign N0CALL --ttl 1
openpulse broadcast --payload "mesh hello" --mode BPSK250 --ttl 3 --callsign N0CALL
```

Config example:

```toml
[mesh]
enabled = true
max_hops = 3
relay_policy = "balanced"   # "strict" relays NOTHING in openpulse-mesh — see below
store_forward_ttl_s = 300
beacon_interval_s = 60
peer_cache_capacity = 256
peer_cache_ttl_s = 3600
```

**`relay_policy` in `openpulse-mesh` (#1253).** `openpulse-mesh` has no local trust store, and
since #1253 it imports a peer's self-asserted trust byte as `Unknown` rather than believing it — a
station used to be able to claim `trusted` about itself and thereby satisfy your `strict` policy.
The consequence is that **`strict` now relays nothing at all** in this binary; it warns loudly at
startup instead of failing quietly. Use `"balanced"` to relay for peers of unknown trust, which is
the project's default posture for synchronized operation, or run the full daemon, which resolves
trust against the signed handshake. Note also that since #1251 `openpulse-mesh` has no route to a
sound card at all.

Operational pattern:
- Each node performs periodic step cycles and emits mesh events
- Peer cache updates via beacons and query responses
- Multi-hop relay paths are selected by score and policy constraints

### 4.4 Dual-Transceiver and Cross-Band Repeater

Model:
- `rig_a`: receive/primary side
- `rig_b`: transmit/secondary side

Config example:

```toml
[radio.rig_a]
rigctld_addr = "127.0.0.1:4532"
backend = "rigctld"

[radio.rig_b]
rigctld_addr = "127.0.0.1:4533"
backend = "rigctld"

[repeater]
enabled = true          # starts at daemon startup
mode = "BPSK250"
tx_hang_ms = 500
full_duplex = false
tx_device = "plughw:2,0"  # rig_b's card; must differ from [audio] device
```

Behavior highlights:
- Half duplex: assert PTT around each forwarded frame
- Full duplex mode available (`full_duplex = true`) with session-level PTT handling

> **Note — CAT backend selection lives at the top-level `[radio]`.** The daemon configures the
> primary rig's CAT from the top-level `[radio]` block, not from `[radio.rig_a]` (which is reserved
> for the planned multi-rig refactor). To use a rig Hamlib/rigctld doesn't support, select the
> generic serial CAT backend there (see [§4.4.1](#441-generic-serial-cat-backend) and [§12.4](#124-radio-cat-rig-definition-files)).

#### 4.4.1 Generic serial CAT backend

For a transceiver not supported by Hamlib/rigctld, drive CAT directly over a serial port with a
TOML-scripted command set. Requires a build with the `generic-serial` feature (Unix only).

```toml
[radio]
cat_backend = "generic"               # "rigctld" (default) | "generic" | "none"
serial_port = "/dev/ttyUSB0"          # the rig's CAT serial device
rig_file    = "docs/config/rig-icom-ic7300.toml"   # command templates + serial params
```

Build and run:

```bash
cargo build --release -p openpulse-daemon --features generic-serial
```

Notes:
- Rig-definition TOML format and examples: [§12.4](#124-radio-cat-rig-definition-files).
- Rig meters (ALC/power/SWR) are rigctld-only; with the generic backend the panel shows no live meters.
- `cat_backend = "none"` disables CAT entirely (manual tuning); PTT still works via `[modem] ptt_backend`.

Usage examples:

```bash
openpulse broadcast --payload "cross-band relay test" --mode BPSK250 --ttl 2 --callsign N0CALL
openpulse monitor --mode BPSK250
```

### 4.5 Custom Zstd Dictionary Training

Dictionary trainer is available at `tools/openpulse-dict-trainer`.

Operational examples:

```bash
openpulse diagnose manifest --session dict-training-smoke --format json
openpulse session-metrics --format json
```

Trainer details:
- Uses built-in synthetic HPX/Winlink corpus if no external corpus directory is provided
- Produces a zstd dictionary artifact and prints dictionary ID

Integration note:
- Runtime compression uses embedded dictionary in `openpulse-core`; replacing dictionary in production requires controlled code and test updates.

### 4.6 Signed Transfers and Handshake Modes

Capabilities:
- Classical signed handshake + manifest
- PQ-capable and hybrid handshake variants
- **Over-the-air signed handshake on connect (daemon).** A `ConnectPeer` initiates a signed
  `ConReq`/`ConAck` exchange over RF: the initiator transmits a signed `ConReq` (SAR-fragmented,
  since the frame exceeds one modem frame), the responder verifies it and replies with a signed
  `ConAck`, and the initiator verifies that. The verified peer identity (callsign + Maidenhead grid)
  is stored and emitted as a `PeerVerified` event; the verified grid is written to the ADIF logbook
  QSO (ahead of the `[logbook.peer_grids]` fallback). An unanswered handshake times out after 30 s.

Station signing key:

```toml
[station]
callsign = "N0CALL"
grid_square = "AA00"
# 32-byte Ed25519 identity seed used to sign CONREQ/CONACK frames. Empty = the platform default
# (~/.config/openpulse/identity.key), generated on first run. Set an explicit path to give
# co-located stations (e.g. the twin-station rig) distinct identities.
identity_key_path = ""
```

Examples:

```bash
openpulse diagnose handshake --peer W1AW --format json
openpulse diagnose manifest --session session-123 --format json
openpulse trust explain W1AW --format json
```

Operational best practices:
- Keep trust store curated and backed up
- Prefer strict or balanced policy for operational paths
- Capture diagnostics logs during field issues for trust and signature events

Cross-reference:
- Trust and signature implementation detail is expanded in [Chapter 5.5](#55-security-and-trust-topics).
- Regulatory and jurisdictional constraints are summarized in [Chapter 11](#chapter-11-compliance-notes).

### 4.7 FreeDV Authenticated Voice Integration

Purpose:
- Add authenticated identity signaling to FreeDV voice operation without changing FreeDV codec internals.

Integration pattern:
- Build a companion process around `openpulse-freedv-auth` to emit signed beacons on the FreeDV UDP data port.
- Verify incoming beacons against your trust-store policy.
- Publish current identity verdict to a local socket for UI consumption.

Practical guidance:
- Keep beacon interval and identity policy aligned with your operating procedure.
- Store trust decisions and failures in logs so operator review is possible after the QSO.
- Use this feature as an identity/authenticity layer; it does not replace required legal station ID behavior.

Cross-reference:
- FreeDV quick-start use case is in [Chapter 2.10](#210-freedv-authenticated-voice-use-case).
- Security/trust internals are in [Chapter 5.5](#55-security-and-trust-topics).

### 4.8 Direct P2P File Transfer (`OPFX`)

Purpose:
- Send a file directly to another OpenPulse station over RF, without a Winlink CMS in the path.

Off by default. Enable and bound it in `config.toml`:

```toml
[file_transfer]
enabled = true
download_dir = "~/.local/share/openpulse/downloads"
auto_accept_max_bytes = 0        # 0 = always ask the operator
max_file_bytes = 1048576         # hard per-file cap, both directions
per_peer_quota_bytes = 0         # retained bytes per peer (0 = no limit)
require_verified_peer = true     # demand a signature-verified CONREQ/CONACK peer
allowed_peers = []               # empty = any peer passing the trust policy
offer_timeout_secs = 120
partial_ttl_hours = 72           # resume window for `.partial` blocks
burst_max_secs = 20.0            # max keyed-TX seconds per burst (keep under the PTT watchdog)
```

Drive it through a running daemon:

```bash
openpulse daemon send-file --help      # send a local file to a peer callsign
openpulse daemon files                 # files received this session, as JSON
openpulse daemon accept-file --help    # accept a pending inbound offer by transfer id
openpulse daemon reject-file --help
openpulse daemon cancel-file --help
```

Operational notes:
- Transfers are split into bursts bounded by `burst_max_secs` so a large file never holds PTT past the
  radio's watchdog and yields the channel between bursts.
- A partially-received transfer resumes from its `.partial` blocks within `partial_ttl_hours`.
- Payloads are hashed and verified; a tampered transfer fails verification rather than being delivered.
- **On-air validation is deferred.** The protocol is exercised over loopback, the modem, and the
  twin-daemon rig, not over RF.

Design note: [docs/dev/design/file-transfer-plan.md](dev/design/file-transfer-plan.md).

### 4.9 JS8 Station Discovery and Rendezvous

Purpose:
- Find other OpenPulse stations on a shared JS8 calling frequency, then negotiate a move to a working
  channel and hand off to a signed HPX connection.

Off by default, and TX is gated: beaconing requires an explicit mode, a configured callsign, the host
clock within ±2 s of UTC, and a clear channel. **The operator remains responsible for §97.221
automatic-control compliance** — see [docs/regulatory.md](regulatory.md).

```toml
[discovery]
enabled = true
mode = "rx_only"                 # "rx_only" (no TX) | "beacon" | "full"
submode = "normal"
dwell_secs = 300                 # 0 = dwell until preempted
heartbeat_interval_slots = 4     # TX modes only
hint_interval_beacons = 4        # send the @OPULSE hint every Nth beacon
max_clock_skew_ms = 2000         # refuse TX beyond this; degrade to RX-only
station_ttl_secs = 3600
# calling_freqs_hz and rendezvous_channels_hz are per-band tables with sane defaults;
# a rendezvous Propose carries an *index* into the current band's list, not a frequency.
```

```bash
openpulse daemon enable-discovery
openpulse daemon stations    # heard JS8 stations as JSON (`is_opulse` flags OpenPulse peers)
openpulse daemon peers       # recognized OpenPulse peers from the shared cache
openpulse daemon disable-discovery
```

**On-air validation is deferred** (Phase H); the waveform, message layer, and rendezvous handoff are
validated in simulation and against JS8Call ground-truth vectors.

Design note: [docs/dev/design/js8-discovery-rendezvous-plan.md](dev/design/js8-discovery-rendezvous-plan.md).

---

## Chapter 5: Technical Manual (Architecture and Implementation)

Primary technical references now live under [docs/dev/README.md](dev/README.md).

### 5.1 Workspace and Layering

Representative layer map:
- Core and protocol primitives: `openpulse-core`
- DSP and signal utilities: `openpulse-dsp`
- Channel simulation: `openpulse-channel`
- Audio backends: `openpulse-audio`
- Runtime modem engine: `openpulse-modem`
- User and service frontends: CLI/TUI/panel/daemon
- Interop surfaces: ARDOP, KISS, B2F, gateway

### 5.2 Data Flow and Runtime Contracts

Pipeline contracts:
- Plugin boundary takes/returns normalized sample payloads
- Audio backend boundary abstracts loopback vs hardware
- Session/state machinery emits structured events for observability

Event stream:
- Engine events can be consumed via monitor/daemon for NDJSON workflows.

### 5.3 Signal Processing Topics

Covered in implementation and docs:
- Pulse shaping and demod paths
- AFC and correction loops
- Timing/coherence handling
- Equalization and profile-specific tuning
- OFDM and SC-FDMA profile behavior
- Receiver front-end transforms (AGC, auto-notch) wired at the single
  `route_audio_stage(PipelineStage::InputCapture)` seam, so every capture path gets them by
  construction
- **CE-SSB transmit envelope conditioning applies to the QPSK-subcarrier OFDM waveforms only**
  (`OFDM16`, `OFDM52`). Every denser OFDM constellation and **all** SC-FDMA are excluded — measured
  end-to-end, CE-SSB's in-band EVM collapses their decode. The predicate is
  `ModemEngine::cessb_benefits`; the master switch is a no-op on every other mode.

### 5.4 Reliability and Access Control Topics

- DCD/CSMA shared channel access
- ACK taxonomy and adaptive rate changes; the OTA rate ACK is authenticated with an ECDH-derived
  keyed MAC, so a forged ACK is rejected
- FEC mode dispatch and comparative behavior
- HARQ/soft-combining and profile-based adaptation. The ARQ/HARQ logic lives in
  `crates/openpulse-modem/src/harq.rs` and `rate_policy.rs`; the receiver-led rate controller is
  `crates/openpulse-core/src/ota_rate.rs`. (The older `arq_session` module no longer exists.)
- Soft combining is composed with plain retry, not substituted for it: each attempt is decoded
  standalone before the MAP sum is tried, so success is a strict superset of both.

### 5.5 Security and Trust Topics

- Handshake mode selection under policy
- Trust-level classification and transition semantics
- Signed manifest validation
- PQ and hybrid compatibility

### 5.6 Test Strategy and CI Discipline

Primary CI-compatible gate:

```bash
cargo build --workspace --no-default-features
cargo test --workspace --no-default-features
```

Additional practice:
- Targeted crate tests during iteration
- Channel-model and integration-suite replay after feature changes

### 5.7 Performance and Deployment Topics

- Loopback-first deterministic validation
- Optional hardware backend activation and troubleshooting
- Edge/portable operational profile support

---

## Chapter 6: Comparison and Advantages

This chapter positions OpenPulseHF against commonly used digital-radio software categories.

### 6.1 Positioning

OpenPulseHF differentiators include:
- Fully open Rust workspace with plugin-centric architecture
- Strong in-tree integration testing across protocol and signal layers
- Broad feature surface in one stack (modem + trust + relay + mesh + UI + interop)
- Built-in modern trust/signing path including PQ-capable handshake support

### 6.2 Practical Advantages

Compared with closed or single-focus stacks, OpenPulseHF offers:
- Auditability: protocol, modem, and trust logic are inspectable
- Extensibility: new waveforms and behaviors can be added as plugins/crates
- Repeatability: loopback and channel simulation workflows reduce hardware friction
- Integration range: ARDOP, KISS, B2F, daemon control API, and panel/TUI surfaces

### 6.3 Operator Advantages

- Flexible deployment from local loopback to daemon/panel operations
- Rich diagnostics and event streaming for troubleshooting
- Multiple operating profiles from conservative to high-throughput paths

### 6.4 Engineering Advantages

- Workspace structure supports focused incremental delivery
- Test-backed behavior changes are straightforward to stage and verify
- Non-GPL integration strategies are documented for proprietary interop needs

---

## Chapter 7: Collaboration and Plugins

### 7.1 Contribution Workflow

Recommended path for new plugin work:
1. Read [docs/dev/contributing-plugins.md](dev/contributing-plugins.md)
2. Implement `ModulationPlugin` trait in dedicated plugin crate
3. Add unit tests plus at least one loopback/integration path
4. Register plugin in target runtime(s)
5. Run CI-compatible checks before PR

### 7.2 Plugin Design Checklist

- Clear mode naming and supported mode list
- Deterministic behavior under loopback tests
- Explicit demod error handling and bounds checks
- Soft demod path when applicable for advanced FEC
- Documentation updates in features/roadmap/manual where relevant

### 7.3 Collaboration Norms

- Keep PRs narrow and test-backed
- Avoid unrelated refactors in feature PRs
- Document behavior changes and operator-facing implications

### 7.4 Example New Plugin Bring-Up

```bash
cp -r plugins/bpsk plugins/mymode-plugin
# rename crate metadata in plugins/mymode-plugin/Cargo.toml, then implement trait + tests
cargo test -p mymode-plugin
```

Then wire into runtime registration in the target binary or engine wrapper.

---

## Chapter 8: Non-GPLv3 Interfaces

OpenPulseHF includes documented paths for proprietary or third-party integration without forcing in-tree GPL plugin coupling.

Canonical reference:
- [docs/dev/plugin-commercial-interface.md](dev/plugin-commercial-interface.md)

### 8.1 Approach A: Dynamic C ABI Shim

- Runtime-loaded shared library approach via C ABI boundary
- Suitable where low-latency in-process plugin handling is required

### 8.2 Approach B: Out-of-Process IPC

- Separate process model via local socket/pipe protocol
- Suitable for language-neutral plugins and stronger isolation

### 8.3 Approach C: Proprietary Data Layer Above Open Transport

- Treat OpenPulseHF as transparent bearer
- Keep proprietary logic entirely above ARDOP/KISS/B2F transport boundary

### 8.4 Integration Checklist

- Validate legal and regulatory context for your jurisdiction/band/service
- Define and document ABI/wire compatibility
- Add health metrics and timeout behavior from day one
- Build conformance tests around error and version skew handling

Legal note:
- This is technical guidance, not legal advice.

---

## Chapter 9: Support and Funding

OpenPulseHF advances fastest when operators, testers, and contributors close the loop with real evidence.

### 9.1 How You Can Help (Non-Financial)

High-value contributions:
- Reproducible bug reports with command lines, logs, and environment details
- Benchmark result sets and comparisons to previous baselines
- RF field notes tied to concrete mode/profile/channel context
- Documentation clarity fixes where operators get blocked

Suggested report bundle:
- Config excerpt (sanitized)
- Commands used
- Relevant logs and NDJSON monitor snippets
- Expected vs observed behavior

### 9.2 Testing and Validation Support

You can contribute by running:

```bash
cargo test --workspace --no-default-features
openpulse benchmark run
```

Attach artifacts and observations to issues/PRs.

### 9.3 Financial Support

If OpenPulseHF helps your operations, consider funding development and maintenance.

Current funding link:
- https://www.paypal.com/donate/?hosted_button_id=WY9U4MQ3ZAQWC

Funding is used to sustain:
- Engineering time for feature and reliability work
- Documentation and validation maintenance
- Infrastructure and release operations

### 9.4 Collaboration Mindset

The most effective support is specific, reproducible, and respectful.
Whether you contribute logs, tests, docs, code, or funding, you are helping build a stronger open stack.

---

## Chapter 10: Glossary and References

### 10.1 Glossary

- ACK: Acknowledgment frame indicating reception outcome.
- AFC: Automatic Frequency Control, tracks/corrects residual frequency offset.
- ARDOP: Amateur Radio Digital Open Protocol, often used via TCP TNC interface.
- ARQ: Automatic Repeat reQuest reliability strategy.
- B2F: Winlink message framing/session protocol family.
- BER: Bit Error Rate.
- CE-SSB: Controlled-Envelope SSB transmit conditioning; in OpenPulseHF it applies to `OFDM16`/`OFDM52` only.
- CPAL: Cross-platform audio I/O crate used by OpenPulse audio backend.
- CSMA: Carrier Sense Multiple Access channel access strategy.
- DCD: Data Carrier Detect channel busy estimator.
- Differential (`-D`): a mode encoding each symbol as a phase *increment*, so a fade rotation cancels symbol-to-symbol. Requires FEC.
- DFE: Decision Feedback Equalizer.
- FEC: Forward Error Correction.
- FER: Frame Error Rate.
- FSK4: Four-tone FSK mode used for ACK/control path.
- HARQ: Hybrid ARQ — retransmission combined with soft-LLR accumulation across attempts.
- HPX: OpenPulse adaptive session/profile and state-machine framework.
- JS8: Weak-signal 8-GFSK waveform, used here for station discovery and rendezvous.
- I/Q: In-phase and quadrature signal representation.
- KISS: Keep It Simple Stupid framing for AX.25/TNC transport.
- LDPC: Low-Density Parity-Check code family.
- LLR: Log-Likelihood Ratio soft decision value.
- LMS: Least Mean Squares adaptive filter/equalizer method.
- MFSK16: Non-coherent 16-GFSK sub-floor waveform; the deep-fade rung of `hpx_hf`.
- ML-DSA-44: Post-quantum signature scheme used in PQ handshake path.
- ML-KEM-768: Post-quantum key encapsulation mechanism used in PQ handshake path.
- MODCOD: A modulation + code-rate pair; two ladder rungs may share a modulation and differ only in FEC rate.
- NDJSON: Newline-delimited JSON stream format.
- OFDM: Orthogonal Frequency Division Multiplexing.
- PAPR: Peak-to-Average Power Ratio.
- PKI: Public Key Infrastructure.
- PQ: Post-quantum.
- QSY: Frequency shift procedure/negotiation between stations.
- RS: Reed-Solomon code.
- SAR: Segmentation and Reassembly.
- SC-FDMA: Single-Carrier Frequency Division Multiple Access (DFT-spread OFDM style waveform).
- SNR: Signal-to-Noise Ratio.
- TNC: Terminal Node Controller interface/service model.
- TTL: Time-to-live or hop limit field.
- Watterson: The ITU-R HF ionospheric channel model (Doppler spread + delay spread) used by `openpulse-channel` for all fading validation in this project.
- Winlink: Message transport ecosystem commonly accessed through B2F/ARDOP paths.

### 10.2 Public References

General protocol and DSP references:
- RFC 8032 (EdDSA / Ed25519): https://www.rfc-editor.org/rfc/rfc8032
- NIST FIPS 203 (ML-KEM): https://csrc.nist.gov/pubs/fips/203/final
- NIST FIPS 204 (ML-DSA): https://csrc.nist.gov/pubs/fips/204/final
- Zstandard format and dictionary docs: https://facebook.github.io/zstd/
- Reed-Solomon overview: https://en.wikipedia.org/wiki/Reed%E2%80%93Solomon_error_correction
- LDPC overview: https://en.wikipedia.org/wiki/Low-density_parity-check_code
- OFDM overview: https://en.wikipedia.org/wiki/Orthogonal_frequency-division_multiplexing
- IARU HF band plan portal: https://www.iaru.org/reference/band-plans/
- FCC Part 97 eCFR: https://www.ecfr.gov/current/title-47/chapter-I/subchapter-D/part-97

Project-local references:
- Architecture: [docs/dev/design/architecture.md](dev/design/architecture.md)
- Features: [docs/features.md](features.md)
- Mode and FEC ladder: [docs/mode-fec-ladder.md](mode-fec-ladder.md)
- Release notes: [docs/releasenotes.md](releasenotes.md)
- 1.0 release criteria: [docs/dev/project/release-1.0-criteria.md](dev/project/release-1.0-criteria.md)
- CLI guide: [docs/cli-guide.md](cli-guide.md)
- Roadmap: [docs/dev/project/roadmap.md](dev/project/roadmap.md)
- Plugin collaboration: [docs/dev/contributing-plugins.md](dev/contributing-plugins.md)
- Commercial interface: [docs/dev/plugin-commercial-interface.md](dev/plugin-commercial-interface.md)
- Regulatory analysis: [docs/regulatory.md](regulatory.md)
- On-air test plan: [docs/on-air_testplan.md](on-air_testplan.md)

---

## Chapter 11: Compliance Notes

This chapter summarizes regulatory and operational compliance touchpoints for OpenPulseHF operation.

### 11.1 Scope and legal posture

- OpenPulseHF is software; legal operation depends on operator actions, station licensing, regional rules, and band plans.
- Always validate local legal requirements before transmitting.
- Treat this chapter as an engineering aid, not legal advice.

### 11.2 FCC (United States)

- Primary amateur service rule set: 47 CFR Part 97.
- Reference: https://www.ecfr.gov/current/title-47/chapter-I/subchapter-D/part-97
- Practical checks:
	- Emission within authorized band segments.
	- Station identification timing and format.
	- Power and occupied bandwidth constraints for the selected band and mode.

### 11.3 CEPT and IARU region alignment

- CEPT recommendations and national implementations influence HF operating practice across Europe.
- IARU band plans should be followed for practical coexistence and interoperability.
- IARU reference portal: https://www.iaru.org/reference/band-plans/

### 11.4 EU regulatory context

- Verify applicable national administration rules (licensing, permitted emissions, duty cycle, and station obligations).
- For radio equipment/system integration, consult relevant RED compliance obligations where applicable.
- European Commission RED overview: https://single-market-economy.ec.europa.eu/sectors/radio-equipment_en

### 11.5 Operational compliance checklist

- Start with [docs/regulatory.md](regulatory.md) and [docs/regulatory-compliance-checklist.md](regulatory-compliance-checklist.md).
- Use [docs/on-air_testplan.md](on-air_testplan.md) to structure pre-deployment and field validation.
- Keep logs and test artifacts for traceability, especially when introducing new modes, plugins, or policy settings.

### 11.6 Engineering compliance-by-design guidance

- Prefer conservative default modes and narrow bandwidth profiles for first deployment.
- Use loopback and simulated channel paths before any on-air tests.
- Gate field operation behind explicit trust policy configuration and reproducible diagnostics capture.
- Document station-specific SOPs for identification cadence, fallback behavior, and emergency stop procedures.

---

## Chapter 12: Configuration File Overview

This chapter summarizes operator-facing configuration files used across OpenPulseHF.
All examples in this chapter are stored in `docs/config/` for copy/edit workflows.

### 12.1 OpenPulse Runtime Config (`config.toml`)

Purpose:
- Defines station, modem, relay, mesh, QSY, and daemon settings consumed by OpenPulse binaries.

Sections in the typed schema (`crates/openpulse-config/src/lib.rs`), all optional with defaults:
`[station]`, `[audio]`, `[modem]`, `[radio]` (+ `[radio.rig_a]`/`[radio.rig_b]`), `[repeater]`,
`[ardop]`, `[kiss]`, `[logging]`, `[relay]`, `[trust]`, `[mesh]`, `[qsy]`, `[daemon]`, `[logbook]`,
`[observability]`, `[control_security]`, `[compression]`, `[file_transfer]`, `[discovery]`,
`[monitor]`.

Precedence: CLI flag > config file > built-in default. `openpulse config init` writes a fully
commented template.

Default location:
- `~/.config/openpulse/config.toml`

Example file:
- [docs/config/openpulse.config.toml](config/openpulse.config.toml)

### 12.2 PKI Tooling Environment File

Purpose:
- Provides startup environment for the `pki-tooling` service (database, API key, signing key, bind address).

Typical usage:
- Source in shell before launching `pki-tooling`.

Example file:
- [docs/config/pki-tooling.env.example](config/pki-tooling.env.example)

### 12.3 On-Air Station Pair Script Config

Purpose:
- Configures station A/B connection, callsigns, PTT backend, and optional audio/rigctld parameters for on-air scripts.

Example file:
- [docs/config/onair-stations.example.sh](config/onair-stations.example.sh)

### 12.4 Radio CAT Rig Definition Files

Purpose:
- Defines model-specific serial/CAT command templates for the `generic` rig backend.

Selecting the generic backend (the daemon **does** consume these — see [§4.4.1](#441-generic-serial-cat-backend)):
- Set `[radio] cat_backend = "generic"`, `serial_port`, and `rig_file` to a definition file below.
- Build the daemon with `--features generic-serial` (Unix only).

Example files:
- [docs/config/rig-icom-ic7300.toml](config/rig-icom-ic7300.toml)
- [docs/config/rig-yaesu-ft817.toml](config/rig-yaesu-ft817.toml)

### 12.5 OpenPulse Panel

- `openpulse-panel` is a native desktop app (iced). It takes no config file and no CLI
  arguments: launch it, enter the daemon control-port address in the connection field
  (default `127.0.0.1:9000`, TCP; `ws://…:9001` for WebSocket), and click Connect.
- Themes (Dark / Light / Contrast / System) are selectable at runtime from the controls band;
  every control has a hover tooltip. The former egui/eframe panel — including its `Trunk`
  web build — was retired in 2026-07 (REQ-UX-04).

---

## Chapter 13: Binary and Test-Rig Reference

A consolidated, example-first reference for every shipped binary and every test
rig. Examples use real flags, subcommands, and environment variables only.

### 13.0 Build conventions

- **Audio feature flags differ per crate.** The CLI defaults to hardware audio;
  every other audio binary is loopback-only unless its feature is enabled:
  - `openpulse` (CLI): `cpal-backend` is **on by default** → plain
    `cargo build -p openpulse-cli` has hardware audio; add `--no-default-features`
    for loopback/CI. Optional `serial` (RTS/DTR PTT), `generic-serial` (scripted CAT), `gpio`.
  - `openpulse-tui`: feature `cpal-backend` (off by default).
  - `openpulse-tnc`, `openpulse-kisstnc`, `openpulse-mesh`, `openpulse-server`,
    `openpulse-testbench`: feature `cpal` (off by default; `openpulse-server` also has `gpu` on by default).
  - `openpulse-gateway`, `openpulse-dict-trainer`, `pki-tooling`: no audio feature.
  - `openpulse-linksim`: features `gui` and `serve` (both opt-in; `gpu` on by default).
- **CI / hardware-free** builds and tests always pass `--no-default-features`.
- Released binaries land in `target/release/`. The examples below assume the
  binary is on `PATH` (or prefix with `./target/release/`).

### 13.1 Binary reference

| Binary | Crate | Role | Hardware-audio build |
|---|---|---|---|
| `openpulse` | openpulse-cli | CLI: transmit/receive, ARQ, adaptive, diagnostics, daemon control | `cargo build --release -p openpulse-cli` (default) |
| `openpulse-tui` | openpulse-tui | Live terminal dashboard (HPX/AFC/rate/DCD) | `--features cpal-backend` |
| `openpulse-server` | openpulse-daemon | Background daemon; NDJSON control on TCP 9000 / WS 9001 | `--features cpal` |
| `openpulse-tnc` | openpulse-ardop | ARDOP-compatible TCP TNC (Pat/Winlink) | `--features cpal` |
| `openpulse-kisstnc` | openpulse-kiss | KISS/AX.25 TCP TNC | `--features cpal` |
| `openpulse-gateway` | openpulse-gateway | Direct TCP Winlink CMS gateway (no radio) | n/a |
| `openpulse-mesh` | openpulse-mesh | Mesh broadcast/relay daemon | *(loopback only — see below)* |
| `openpulse-panel` | openpulse-panel | iced operator panel (connects to the daemon) | n/a (control client) |
| `openpulse-twinview` | openpulse-twinview | egui both-directions viewer over two daemons | n/a (control client) |
| `openpulse-testbench` | openpulse-testbench | egui 4-column signal-path scope | `--features cpal` (live capture) |
| `openpulse-testmatrix` | openpulse-testmatrix | Mode × channel test-matrix runner (no audio) | n/a |
| `openpulse-linksim` | openpulse-linksim | Two-station ARQ link simulator (CLI) | n/a |
| `openpulse-linksim-gui` | openpulse-linksim | Live link visualiser | `--features gui` |
| `openpulse-dict-trainer` | openpulse-dict-trainer | Offline zstd dictionary trainer | n/a |
| `pki-tooling` | pki-tooling | PKI trust-bundle signing HTTP service | n/a |

#### `openpulse` (CLI)

Global flags: `--backend <default|loopback|cpal>`, `--log <level>`,
`--ptt <none|rts|dtr|vox|rigctld|cm108|gpio>`,
`--rig <serial path | rigctld addr | /dev/hidrawN | chip:line>`, `--rig-file <toml>`,
`--max-power <watts>` (default 100), `--pki-url <url>` (default `http://127.0.0.1:8787`).

```bash
# Local loopback transmit / receive (no hardware)
openpulse --backend loopback transmit "Hello World" --mode BPSK250 --fec rs
openpulse --backend loopback receive --mode BPSK250 --listen-ms 5000 --fec rs

# Inventory
openpulse modes        # list modulation modes
openpulse devices      # list audio devices

# Mode recommendation for a measured SNR (no hardware)
openpulse mode-advisor --snr 12.0 --profile hpx_hf

# Adaptive rate-control over a simulated channel (no hardware)
openpulse adaptive --profile hpx_hf --channel awgn --snr 6.0 --frames 8 --json

# Reliable two-way ARQ (FSK4 ACK + retransmit)
openpulse arq listen --mode BPSK250 --frames 5 --profile hpx_hf   # station 2
openpulse arq send --payload "Test message" --mode BPSK250 --retries 3   # station 1

# Stream engine events as NDJSON; benchmark regression gate
openpulse monitor --mode BPSK250 | jq .
openpulse --backend loopback --log error benchmark run

# On-device calibration (audio level, PTT latency, AFC)
openpulse --ptt rts --rig /dev/ttyUSB0 calibrate ptt --output ptt.json
# Guided ALC drive tuning (keys the TX — needs --features cpal-backend + rigctld; use a dummy load).
# Steps TX attenuation until the rig's ALC sits in a moderate band, so a high-PAPR multicarrier
# waveform doesn't over-drive the PA into splatter. Live ALC also shows in the panel
# (daemon polls the rig at [radio] meter_poll_ms).
openpulse --rig 127.0.0.1:4532 calibrate drive --mode OFDM52 --target-alc-lo 0.3 --target-alc-hi 0.5

# Identity / trust / config
openpulse trust list
openpulse config init > ~/.config/openpulse/config.toml

# Package audit-mode artifacts (events.ndjson, snapshot.json, logs) into a .tar.gz
openpulse audit-bundle --help

# Control a running openpulse-server daemon (OTA adaptive rate-stepping)
openpulse daemon --addr 127.0.0.1:9000 ota-start --profile hpx_modcod
openpulse daemon ota-bounds --min SL3 --max SL10
openpulse daemon ota-hysteresis --min-backlog 128 --upgrade-hold-frames 3
openpulse daemon ota-aggressiveness balanced   # conservative|balanced|aggressive (sets A2/A3 together)
openpulse daemon set-dcd-squelch 0.05          # raise the squelch above a noisy band's floor
openpulse daemon set-tx-attenuation 6          # TX attenuation dB for the current band (0 = none)
openpulse daemon set-tx-attenuation 6 --band 20m   # ...or a named band
openpulse daemon ota-lock --level SL6
openpulse daemon ota-status   # JSON snapshot

# Other runtime toggles reachable from the daemon CLI (all also available in the panel):
openpulse daemon set-agc true        # receiver streaming AGC (normalise level before demod)
openpulse daemon set-notch true      # receiver auto-notch (out-of-band CW interference)
openpulse daemon set-cessb true      # CE-SSB TX conditioning; acts on OFDM16/OFDM52 only
                                     # (no-op on dense OFDM-HOM, all SC-FDMA, and single-carrier)
openpulse daemon set-logbook true    # automatic ADIF logbook (one record per QSO)
```

#### `openpulse-tui`

```bash
# Loopback dashboard (default backend); requires a real callsign in config.toml
openpulse-tui --mode BPSK250
# Hardware audio
openpulse-tui --backend cpal --mode QPSK500 --log info
```
Keys: `q`/Ctrl+C quit · `p` pause · ↑/↓ scroll · `Q` toggle QSY · `b` cycle bandplan · `t` toggle tuner-on-high-SWR.

#### `openpulse-server` (daemon)

Config-file driven (`~/.config/openpulse/config.toml`); no CLI flags. Reads
`[daemon]` (TCP 9000, WS 9001, `receive_tick_ms` 50), `[modem]` (incl. `ota_*`),
`[audio]`, `[radio]` (incl. `cat_backend`), `[repeater]`, `[qsy]`, `[relay]`, `[trust]`.

```bash
# Production (real audio + GPU auto-detect)
cargo build --release -p openpulse-daemon --features cpal
openpulse-server          # serves control on 127.0.0.1:9000 (TCP) and :9001 (WS)
# Pair with the panel or the CLI daemon subcommands above.
```

#### `openpulse-tnc` (ARDOP) and `openpulse-kisstnc` (KISS)

CLI flags override `config.toml`; `RUST_LOG` sets verbosity.

```bash
# ARDOP TNC for Pat (cmd 8515 / data 8516) — loopback: Pat runs on this host
openpulse-tnc --bind 127.0.0.1 --cmd-port 8515 --data-port 8516 --mode QPSK500 --backend cpal

# KISS/AX.25 TNC (TCP 8100)
openpulse-kisstnc --bind 127.0.0.1 --port 8100 --mode BPSK500 --backend cpal
```

> **Binding a TNC off-loopback grants transmit control to the network.** Both interfaces implement
> third-party protocols (ARDOP, KISS/AX.25) that have **no authentication in their specifications**,
> so OpenPulseHF cannot add one without breaking the Pat/Winlink/APRS clients they exist to serve
> (see "Third-party protocol surfaces are exempt" in `docs/dev/requirements.md`). Anything that can reach the port can key your transmitter — the ARDOP command
> port accepts `PTT TRUE` and `MYID` from any connection.
>
> Both default to `127.0.0.1`. If a client genuinely runs on another machine, put the port on a
> trusted segment or an SSH tunnel / VPN — do **not** expose it to an untrusted LAN or the internet:
>
> ```bash
> # From the client host: forward the local ports over SSH instead of binding 0.0.0.0
> ssh -N -L 8515:127.0.0.1:8515 -L 8516:127.0.0.1:8516 operator@radio-host
> ```
>
> The transmit-safety guarantees are independent of the caller: every emission the TNC makes is keyed
> under an RAII guard that releases even on an early return or a panic, a manual `PTT TRUE` from a
> host is dropped if that client disconnects, and the shared watchdog force-releases past the max
> keyed duration. Those bound the damage; they are not a substitute for controlling who can reach the
> port.
>
> **PTT ownership.** The TNC keys from `[modem] ptt_backend` and emits `PTT TRUE` / `PTT FALSE` as
> asynchronous events for hosts that drive their own rig. If you let the TNC key, set Pat's
> `ptt_ctrl` to `false` — otherwise both key the same device. `ptt_backend = "none"` with a
> host-keyed rig is **not supported**: the TNC starts audio immediately after the edge, with no
> leader delay, so the first ~50 ms of preamble would be clipped and the frame would not demodulate.

#### `openpulse-gateway` (Winlink CMS, no radio)

```bash
openpulse-gateway --host cms.winlink.org --port 8772 --callsign N0CALL \
  send --to K5ABC --subject "Test" --message "Hello over OpenPulse"
# Omit --message to read the body from stdin.
```

#### `openpulse-mesh`

```bash
# Requires [mesh] enabled = true in config.toml
openpulse-mesh --mode BPSK500 --max-hops 5 --backend loopback
```

> **`openpulse-mesh` cannot use a real audio backend** (#1251). It has no PTT controller (so `[modem] ptt_backend` is unread), no carrier sense, and no station-ID timer, and its beacon carries no callsign field — so it is not an on-air station and `--backend cpal` is refused with that reason rather than degrading silently to loopback. To relay on air, run the relay inside `openpulse-daemon`, which keys, senses and identifies.


#### `openpulse-panel`

```bash
cargo build --release -p openpulse-panel
openpulse-panel    # then click Connect (default 127.0.0.1:9000); no CLI args
```

#### `openpulse-twinview`

```bash
cargo build --release -p openpulse-twinview
openpulse-twinview                              # 127.0.0.1:9000 + 127.0.0.1:9002
openpulse-twinview 127.0.0.1:9000 127.0.0.1:9002
```
One window, two columns — each column is a station's live spectrum/waterfall +
rate/OTA/HPX readouts, so **both directions** of a bridged twin-station link are
visible at once (the left station's TX level is the A→B rate, the right is B→A).

#### `openpulse-testbench`

```bash
cargo build --release -p openpulse-testbench            # synthetic source
cargo build --release -p openpulse-testbench --features cpal   # + live audio capture
openpulse-testbench
```

#### `openpulse-testmatrix`

```bash
cargo build --release -p openpulse-testmatrix
openpulse-testmatrix                              # quick tier → docs/test-reports/
openpulse-testmatrix --full --output ./reports    # all channels × payloads
openpulse-testmatrix --bench-only --bench-frames 100 --bench-payload 200
```

#### `openpulse-linksim` and `openpulse-linksim-gui`

```bash
# Single run: QPSK ladder on AWGN 15 dB, RS FEC, 128-byte payloads
openpulse-linksim --profile hpx_hf --channel awgn --fec rs --payload 128 --frames 40 --snr 15.0
# SNR sweep with a table, then JSON
openpulse-linksim --channel watterson-moderate --sweep "10.0:20.0:1.0"
openpulse-linksim --snr 20.0 --json
# Feed an unmodified panel from the simulated link (build --features serve)
openpulse-linksim --serve 127.0.0.1:9000 --serve-fps 20 --snr 12.0
# Live GUI (build --features gui)
openpulse-linksim-gui
```

#### `openpulse-dict-trainer`

```bash
openpulse-dict-trainer                                   # built-in corpus → zstd-hpx-dict.bin
openpulse-dict-trainer --corpus-dir ./messages --output ./my-dict.bin --dict-size 8192
```

#### `pki-tooling` (HTTP service)

```bash
export DATABASE_URL="postgresql://pki:secret@localhost/pki_db"
export PKI_API_KEY="my-api-key"
export PKI_SIGNING_KEY="<base64 32-byte ed25519 seed>"   # or PKI_ALLOW_EPHEMERAL_KEY=true (dev only)
export PKI_BIND_ADDR="127.0.0.1:8080"
pki-tooling
```

### 13.2 Test rigs

Listed loosest-to-tightest coupling to hardware. The first two need no radio; all
write JSON reports under `docs/dev/test-reports/`. See also
[docs/dev/virtual-loopback.md](dev/virtual-loopback.md) and
[docs/dev/ota-hardware-validation.md](dev/ota-hardware-validation.md).

#### A. In-process channel-sim harness (no audio hardware)

The hardware-free rig: two `ModemEngine`s bridged through an `openpulse_channel`
model. `ChannelSimHarness::route()` is one-way; `channel_sim::bridge_through(src,
dst, channel)` is the plugin-agnostic primitive for bidirectional harnesses (a
forward and a reverse channel). Used by the integration tests
`ota_channel_adaptation.rs` (real OTA rate-stepping under AWGN/Watterson),
`channel_loopback*.rs`, and `sro_confirmation.rs`.

```bash
cargo test -p openpulse-modem --no-default-features --test ota_channel_adaptation
cargo test -p openpulse-modem --no-default-features --test channel_loopback_multimode
```

#### A2. Twin-station rig — two REAL daemons bridged (no audio hardware)

The full-stack counterpart to the engine-level harness: two real `openpulse-server`
daemons run in one process and are bridged through a channel at the `LoopbackBackend`
sample tap (forward = A→B, reverse = B→A, each its own model). Both run the **real**
stack — `RateAdapter`, `HpxReactor`, OTA, QSY — so on-air bugs in those paths
surface here, unlike `openpulse-linksim` (which reimplements the policy layers).
`openpulse_daemon::twin::spawn_bridged_pair` wires it; each daemon binds its own
control port so two real `openpulse-panel`s can attach (one per direction). The
loopbacks use `LoopbackBackend::new_split()` so a daemon never receives its own TX.

```bash
# Headless deterministic round-trip across the bridge (CI):
cargo test -p openpulse-daemon --no-default-features --test twin_daemon_bridge

# Live rig for two-panel visualization (Ctrl+C to stop):
TWIN_SNR_DB=12 cargo run -p openpulse-daemon --example twin_station
#   then: openpulse-panel → Connect 127.0.0.1:9000 (station A)
#         openpulse-panel → Connect 127.0.0.1:9002 (station B)
#   or one combined window over both:  openpulse-twinview
```

#### A3. Twin-station rig over real audio (snd-aloop)

Same two real daemons, but routed through the real cpal+ALSA+resampler path
instead of an in-process channel, so it also covers the audio stack where
on-air-specific bugs live (resampler, sample format, and — on the dual-card rig —
true dual-clock). Each daemon runs as its own `openpulse-server` process with the
cpal backend pinned to a full-duplex `snd-aloop` PCM via `[audio] device`; the
kernel cross-links station A's PCM to B's and back. Two real panels attach as
above (control ports 9000 / 9002).

```bash
scripts/setup-twin-loopback.sh          # snd-aloop + aloop_a/aloop_b PCMs (sudo)
scripts/run-twin-station-audio.sh        # builds --features cpal, starts both daemons
OTA=1 MODE=QPSK500 scripts/run-twin-station-audio.sh   # with OTA rate-stepping
```

Generate continuous traffic to watch in the panels (sends random-data messages to
a daemon's control port in a loop; works for the in-process rig too):

```bash
scripts/twin-traffic.sh                          # 127.0.0.1:9000 → TWIN-B, every 2 s
ADDR2=127.0.0.1:9002 scripts/twin-traffic.sh     # both directions (A→B and B→A)
INTERVAL=1 SIZE=128 COUNT=20 scripts/twin-traffic.sh   # 20 rounds, 128-byte bodies
```

With `OTA=1` the traffic also **animates the rate ladder**: a `send_message` on an
OTA-active daemon transmits via the receiver-led OTA path (transmit at the OTA
mode → wait for the peer's ACK → adopt its absolute recommended level), so the TX
level climbs as the peer's recommendation rises. Drive **one direction** for a
clean ladder demo (no `ADDR2`); the receiving station's panel shows its
recommendation, the sender's shows its TX level stepping up.

Single shared clock here (snd-aloop). For a true two-clock test, point
`A_DEVICE`/`B_DEVICE` at two USB cards (see the dual-card rig in F).

#### B. Test matrix (no audio hardware)

```bash
scripts/run-test-matrix.sh           # quick tier (~30 s)
scripts/run-test-matrix-full.sh      # full tier (all channels × payloads)
# Reports archived under docs/dev/test-reports/archive/<timestamp>-<git-sha>/
```

#### C. Virtual single-clock loopback (snd-aloop, one host, no radio)

Real cpal+ALSA+resampler path with one shared clock — isolates DSP/code from
analog and dual-clock effects. The default transport before any hardware run.

```bash
scripts/setup-virtual-loopback.sh        # loads snd-aloop; writes aloop_tx/aloop_rx to ~/.asoundrc (sudo)
cargo build --release -p openpulse-cli   # cpal default
MODES="BPSK250 QPSK500" scripts/run-loopback-virtual.sh
```
Key vars: `MODES`, `LISTEN_MS` (120000), `PAYLOAD_BYTES` (32), `RETRIES` (3),
`PRE_WAIT`/`POST_WAIT` (AFC settle margins), `OUTPUT_DIR`.

#### D. Dual-card hardware loopback (two USB cards, one host)

Two USB soundcards + an analog cable — exercises the real analog path without a
second machine. It does **not** reproduce sample-rate offset: both adapters slave
to the host's USB frame clock, measured at +0.10 ppm (`--sro-check`).

```bash
scripts/setup-dualcard-loopback.sh       # resolves both USB cards; disables AGC; sets capture gain
cargo build --release -p openpulse-cli
scripts/run-loopback-dualcard.sh --quick
FEC=soft-concatenated scripts/run-loopback-dualcard.sh --single-case "SCFDMA26-16QAM|64"
```
**Gotcha 1 — re-run the setup script after every replug.** USB mixer state is not
persistent, and the capture AGC comes back on. An AGC moves the level *during* a
frame, which is near-harmless to a phase-only mode and destructive to anything
carrying bits in amplitude (64QAM, the dense SC-FDMA QAMs) — so a sweep run on a
non-normalised rig fails exactly those modes and looks like a waveform limit. That
misclassified eight modes; six of them passed unchanged once the rig was
re-normalised. Both scripts now verify rather than assume: the setup script reads
the AGC back and fails if it is still on, and `run-loopback-dualcard.sh` refuses to
sweep while any card's AGC is live (`AGC_PREFLIGHT=0` overrides).

**Gotcha 2:** `CAPTURE_GAIN=16` (not max). At 16 the modem TX peaks ~0.79 FS
unclipped; higher clips line-level input. Other vars: `TX_BYPATH`/`RX_BYPATH`,
`TIER` (quick/full), `FEC`, `IRS_LISTEN_MS`.

#### E. RPi station-pair loopback (two Pis over SSH + cable)

Two-machine, two-clock, analog-cable scenario — the pre-on-air regression gate.

```bash
scripts/deploy-rpi-pair.sh               # cross-compiles aarch64, rsyncs binaries to both stations
scripts/run-loopback-rpi51-rpi52.sh --quick
scripts/run-loopback-rpi51-rpi52.sh --full
```
Key vars: `ISS_SSH`/`IRS_SSH` (TX/RX stations), `ISS_DEVICE`/`IRS_DEVICE`
(`plughw:CARD=Device,DEV=0`), `IRS_STARTUP_WAIT`, `TX_TIMEOUT`, `FEC`, `TIER`.

#### F. On-air rigs (real RF)

Profiles live in `docs/config/onair-*.example.sh`; callsigns must not be `N0CALL`.
The FT-991A must use **CAT PTT** (`B_PTT_TYPE="CAT"`), not RTS.

```bash
# IC-9700 (TX) ↔ FT-991A (RX), 2 m, dual-SSH
source docs/config/onair-ic9700-ft991a.example.sh
scripts/run-onair-ic9700-ft991a.sh supervise --quick --label 2m-test

# Lab599 TX500 (Pi) ↔ KX3 (local), HF
source docs/config/onair-tx500-kx3-local.example.sh
scripts/run-onair-tx500-kx3.sh supervise --full

# Twin-OTA: two real daemons + OTA adaptive rate-stepping over the air, watched in
# openpulse-twinview (the real-radio counterpart of the in-process twin rig).
source docs/config/onair-twin-ota.example.sh
scripts/run-onair-twin-ota.sh supervise        # then attach openpulse-twinview
```
The twin-OTA scenario is daemon-based (two `openpulse-server` over rigctld CAT+PTT
with cpal audio) — see [docs/dev/onair-twin-ota.md](dev/onair-twin-ota.md).
Supporting scripts: `onair-preflight.sh` (env/binary/callsign checks),
`run-onair-validation-flow.sh` (preflight → matrix → bundle → report),
`onair-generate-report.sh` (Phase 5.5-reg markdown), `onair-bundle-evidence.sh`
(timestamped evidence dir with `metadata.json`, SHA-256, git SHA). Key on-air
vars: `A_SSH`/`B_SSH`, `CALLSIGN_A`/`CALLSIGN_B`, `*_HAMLIB_MODEL`, `*_CAT_PORT`,
`*_PTT_TYPE`, `TEST_FREQ_HZ` (band-enforced), `A_RFPOWER`/`B_RFPOWER` (default 0.05).

---

End of manual.
