---
project: openpulsehf
doc: docs/dev/design/architecture.md
status: living
last_updated: 2026-06-19
---

# Architecture

## System goals

- Provide a Rust-native, plugin-based software modem for amateur radio data links.
- Keep a reusable workspace split into core, audio, modem engine, and frontend crates.
- Maintain reliable loopback testing that works without external audio hardware.
- Keep frontend behavior consistent by making CLI the reference execution path.
- Support incremental protocol growth through plugin-based modulation modes.

## Core architecture

1. Input payload is framed into OpenPulseHF packets with sequence and CRC.
2. A modulation plugin transforms frames into baseband symbols and samples.
3. An audio backend transports samples to and from loopback or hardware I/O.
4. A receive pipeline demodulates, validates frames, and reassembles payload data.
5. Frontend surfaces status and decoded payloads to users and automation.

For HPX, the pipeline also includes signed session handshake validation and signed transfer manifest verification before delivery completion is acknowledged.
For relayed operation, the control plane includes peer discovery cache, query handling, and route selection across one or more relay hops.

## Workspace architecture

| Crate/Path | Role |
|-----------|------|
| crates/openpulse-core | Core traits (ModulationPlugin and AudioBackend), frame format, CRC-16, and plugin registry |
| crates/openpulse-audio | Audio backend implementations: in-process loopback (testing) and CPAL-based backends |
| crates/openpulse-modem | Modem engine wiring plugins and audio together |
| crates/openpulse-cli | openpulse binary and user-facing CLI options |
| plugins/bpsk | BPSK modulation plugin with NRZI and raised-cosine pulse shaping |

Future plugins (for example QPSK or ARDOP-compatible modes) should implement ModulationPlugin and register at startup.

## Modulation design decisions

### Single-carrier versus OFDM

OpenPulseHF's original/default waveforms are single-carrier phase-shift keying; VARA HF and PACTOR-3/4 use OFDM. The single-carrier choice (rationale below) is still the robust default, but **OFDM and SC-FDMA multicarrier families have since shipped** (`plugins/ofdm`, `plugins/scfdma`; the `hpx_ofdm_hf`/`hpx_wideband_hd` ladders) for HF high-throughput on frequency-selective fades — see the OFDM-over-SC-FDMA decision in `mode-fec-ladder.md` §7. Rationale for the single-carrier default:

**Advantages of single-carrier for this project:**
- Peak-to-Average Power Ratio (PAPR) is near 0 dB for BPSK and approximately 3–4 dB for QPSK. OFDM with many subcarriers produces PAPR of 9–12 dB. Low PAPR means OpenPulseHF can transmit at rated power from any linear amplifier without back-off, which is critical for portable and QRP operation.
- No IQ balance or phase noise requirements. Any SSB-capable transceiver works without special calibration.
- No cyclic prefix overhead. OFDM requires a guard interval (VARA uses 5.33 ms per symbol) that consumes channel time with no data.
- Simpler receiver. A single correlator or matched filter per symbol versus an N-point FFT plus per-subcarrier equalization.
- Automatic frequency control (AFC) is simpler: a single carrier frequency estimate versus per-subcarrier frequency drift tracking.

**Accepted trade-offs:**
- Single-carrier modes are more susceptible to inter-symbol interference (ISI) from multipath delay spread. At the symbol periods used by OpenPulseHF (32 ms for BPSK31, 4 ms for BPSK250), ISI from HF multipath delays of 0.5–2 ms is tolerable for lower baud rates without equalization. Higher-rate single-carrier modes will require adaptive equalization as they are developed.
- Peak throughput for a given occupied bandwidth is lower than OFDM with higher-order QAM, because OFDM can overlap subcarriers spectrally. `hpx_hf` and `hpx_wideband` address this through adaptive modulation across the rate ladder (BPSK → QPSK → 8PSK) within a single occupied bandwidth class.

### Differential BPSK encoding

Current BPSK modes use differential encoding (DBPSK): the data bit is encoded as a phase *change* relative to the previous symbol, not as an absolute phase. This eliminates the need for carrier phase recovery at the receiver. The cost is approximately 3 dB SNR relative to coherent BPSK. For BPSK31 on HF where SNR often limits throughput, differential encoding is the pragmatic choice. Coherent detection may be added for higher-rate modes where the 3 dB gain is worth the receiver complexity.

### Raised-cosine pulse shaping

The BPSK plugin applies raised-cosine amplitude shaping (roll-off factor α = 1.0) to each symbol. This is the same shaping used by PSK31 (G3PLX, 1998). The result is near-zero out-of-band emissions despite the narrow 31–250 Hz symbol bandwidth: sidelobes fall at 1/f³ versus 1/f for rectangular pulse shaping. The trade-off is sensitivity to ISI, which is acceptable at HF symbol rates.

Note: this is an amplitude envelope shape applied per symbol, not a root-raised-cosine matched-filter pair. This is architecturally simpler but not optimal in the information-theoretic sense. Matched-filter design may be revisited for higher-rate modes.

## Supported modulation modes

The plugin registry spans BPSK, QPSK, 8PSK, 64QAM (single-carrier), FSK4 (ACK control
channel), the OFDM / SC-FDMA multicarrier families, and the pilot-framed single-carrier
family (`PILOT-*`). For the authoritative per-mode
table (baud, bits/symbol, gross bps, occupied bandwidth) see the
[README modulation-modes table](../../README.md#modulation-types); `openpulse modes`
prints the live registry.

**Pulse shaping & carrier-offset robustness.** Single-carrier modes come in a plain
form (per-symbol half-cosine crossfade envelope) and a root-raised-cosine `-RRC` form
(α = 0.35). The `-RRC` forms are the operational choice at higher baud — a proper
Nyquist matched filter, robust to a real carrier-frequency offset. The carrier-recovery
work (mid-2026) made the RRC modes plus 8PSK500/8PSK1000 acquire and decode through a
realistic ±50 Hz offset: the AFC estimate for RRC modes runs on the matched-filtered
baseband (not the passband demod), and 8PSK uses a two-pass *acquire-then-track*
decision-directed carrier loop. The plain rectangular `QPSK2000`/`8PSK2000` are
**RRC-superseded** — at 4 samples/symbol their crossfade pulse is ISI-limited; use the
`-RRC` variants.

The pilot-framed `PILOT-*` family takes a different route to carrier-offset robustness:
known in-band pilots drive a data-aided carrier loop instead of a decision-directed one,
which is cycle-slip-immune on dense constellations and sample-rate-offset robust without a
Gardner timing loop — see [hpx-waveform-design.md](hpx-waveform-design.md#pilot-framed-waveform).

## HPX adaptive profiles

HPX sessions select a modulation mode at runtime based on the current `SpeedLevel` reported by the `RateAdapter`. Several profiles are defined in `openpulse-core/src/profile.rs`; the [README profiles table](../../README.md#adaptive-rate-profiles) and [mode-fec-ladder.md §4](../../mode-fec-ladder.md) are the authoritative list. A few are shown below:

### `hpx500` — 500 Hz class

| SL  | Mode     | Notes |
|-----|----------|-------|
| SL1 | — | Chirp fallback (session teardown) |
| SL2 | BPSK31   | Initial level; most robust |
| SL3 | BPSK63   | |
| SL4 | BPSK250  | |
| SL5 | QPSK250  | |
| SL6 | QPSK500  | Highest hpx500 rate |
| SL7 | — | Reserved |

### `hpx_hf` — HF-compliant adaptive profile (≤ 2700 Hz)

Peaks at SL7 = 8PSK500 (1500 bps gross, ~2000 Hz BW with Hann windowing). Legal on all HF amateur allocations. Use this profile for HF operation.

| SL  | Mode      | Notes |
|-----|-----------|-------|
| SL1 | — | Chirp fallback |
| SL2 | BPSK31    | Initial level |
| SL3 | BPSK63    | |
| SL4 | BPSK250   | |
| SL5 | QPSK250   | |
| SL6 | QPSK500   | |
| SL7 | 8PSK500   | Ceiling — ~2000 Hz BW |

### `hpx_wideband` — Wideband profile (≤ 4000 Hz)

Exceeds the 2700 Hz HF channel-width limit at SL9–SL11. Legal on FM voice channels, satellite, and UHF/VHF links. **Do not use on HF amateur allocations.**

Single-carrier modulation was chosen as the default for this profile. Rationale: lower PAPR (near 0 dB for 8PSK vs 9–12 dB for OFDM), no cyclic prefix overhead, simpler AFC, and architectural consistency with `hpx500`. (OFDM was subsequently shipped under FF-4 — `plugins/ofdm` + the `hpx_ofdm_hf` ladder — as the preferred dense-multicarrier HF path; see `mode-fec-ladder.md` §7.)

| SL  | Mode      | Notes |
|-----|-----------|-------|
| SL1–SL7 | — | Chirp fallback / hpx500/hpx_hf territory |
| SL8 | QPSK500   | Initial level |
| SL9 | QPSK1000  | |
| SL10 | — | Reserved |
| SL11 | 8PSK1000 | Ceiling — ~4000 Hz BW |

### `hpx_pilot` — pilot-framed single-carrier profile (~550 Hz)

A cycle-slip-immune, sample-rate-offset-robust ladder built on the pilot-framed
`PILOT-*` waveform (pilot-aided carrier recovery; see
[hpx-waveform-design.md](hpx-waveform-design.md#pilot-framed-waveform)).

| SL  | Mode | Notes |
|-----|------|-------|
| SL2 | PILOT-QPSK500 | Initial level |
| SL3 | PILOT-8PSK500 | |
| SL4 | PILOT-16QAM500 | |
| SL5 | PILOT-32APSK500 | Ceiling — DVB-S2 32APSK |

## Signed session handshake (Phase 2.3)

HPX sessions begin with a two-message signed handshake that authenticates both peers and negotiates the signing mode for the session.

### CONREQ (connection request)

Sent by the initiating station at the start of the Discovery phase.

```
MAGIC("HSCQ") | VERSION(0x02) | LENGTH(u16, BE) | body | SIGNATURE(64)
```

Binary body, fields in wire order (v2, #1147 — v1's JSON body is gone):

| Field | Type | Description |
|-------|------|-------------|
| `station_id` | string ≤12 | Callsign of the initiating station |
| `dst_station` | string ≤12 | Station being addressed; `"*"` = broadcast. **Empty is invalid** |
| `pubkey` | `[u8; 32]` | Ed25519 verifying key |
| `kex_pubkey` | `[u8; 32]` | Ephemeral X25519 key for OTA-ACK key agreement |
| `signing_modes` | u8 count + u8 each | Modes offered, ≤4 |
| `session_id` | u64 | Session identifier (fixed-width since #1147's follow-up) |
| `station_grid` | string ≤8 | Maidenhead grid; empty = not advertised |
| `profile_name` | string ≤24 | Active OTA ladder; empty = none |
| `profile_fingerprint` | u64 BE | Ladder mapping fingerprint; 0 = none |
| `timestamp_ms` | u64 BE | Signed creation time; **mandatory** |

### CONACK (connection acknowledgment)

Sent by the responder in reply to a valid CONREQ **addressed to it**.

```
MAGIC("HSAK") | VERSION(0x02) | LENGTH(u16, BE) | body | SIGNATURE(64)
```

| Field | Type | Description |
|-------|------|-------------|
| `station_id` | string ≤12 | Callsign of the responding station |
| `dst_station` | string ≤12 | The initiator; never a wildcard |
| `pubkey` | `[u8; 32]` | Ed25519 verifying key |
| `kex_pubkey` | `[u8; 32]` | Ephemeral X25519 key |
| `selected_mode` | u8 | Chosen from the CONREQ's offer list — and **checked against it** |
| `conreq_hash` | `[u8; 32]` | SHA-256 over the complete transmitted CONREQ |
| `station_grid` | string ≤8 | Responder grid; empty = not advertised |
| `profile_name` | string ≤24 | Responder's ladder; empty = none |
| `profile_fingerprint` | u64 BE | Fingerprint; 0 = none |
| `timestamp_ms` | u64 BE | Signed creation time; **mandatory** |

**The signature covers `magic ‖ version ‖ length ‖ body`** — the transmitted bytes minus the
signature — so there is no second representation to drift from the wire, and the version is bound.
Both frames fit **one SAR fragment** by construction (236 B / 237 B against 251 B); v1's JSON frames
were ~752 B and took three.

The CONACK carries no `session_id`: echoing it would push the maximal frame past one fragment, and
`conreq_hash` binds harder — the session id is cleartext and time-based, hence guessable within the
handshake window.

### Handshake trust evaluation

The receiver of each handshake frame:

1. Verifies the Ed25519 signature against the included `pubkey`.
2. Looks up the peer's trust level in the local trust store.
3. Calls `evaluate_handshake(policy, local_min_mode, peer_modes, key_trust, cert_source)` from `openpulse-core::trust`.
4. On `Rejected` trust level or no mutual signing mode → fires `HpxEvent::SignatureVerificationFailed` → session enters `Failed`.
5. On success → fires `HpxEvent::DiscoveryOk` → session advances to `Training`.

### Transfer manifest

At session teardown, the sender emits a `TransferManifest` with:

| Field | Type | Description |
|-------|------|-------------|
| `payload_hash` | `[u8]` | SHA-256 of the complete reassembled payload |
| `payload_size` | `u64` | Total payload bytes |
| `sender_id` | string | Callsign of the sender |
| `signature` | `[u8]` | Ed25519 signature (64 bytes) over canonical JSON of the above fields (excluding `signature`) |

The receiver verifies the signature with `verify_manifest()` and also checks that `payload_hash` matches the locally-computed hash of the received data. A mismatch fires `HpxEvent::TransferError` → `HpxReasonCode::ManifestVerificationFailed`.

## DCD and CSMA channel access (Phase 2.4)

### Data Carrier Detect (DCD)

`DcdState` (`openpulse-core::dcd`) computes the RMS energy of each incoming sample batch and marks the channel busy when energy exceeds a configurable threshold.  A hold window (default 100 ms at 8 kHz = 800 samples) keeps the busy flag set for a short period after the last detection to debounce inter-symbol gaps.

`ModemEngine::receive()` calls `DcdState::update()` after every capture, so the engine always has a current energy estimate.  Callers can poll `engine.is_channel_busy()` or inspect `engine.dcd_energy()`.

### 0.3-persistence CSMA

When enabled via `engine.enable_csma()`, the engine applies 0.3-persistence CSMA before every audio emission (`stage_emit_output`):

1. If DCD reports channel busy → return `ModemError::ChannelBusy` immediately.
2. If channel is clear → draw a uniform random value in [0, 1).  If value ≥ 0.3 → defer (`ModemError::ChannelBusy`).  Otherwise → transmit.

The caller is responsible for backing off and retrying on `ChannelBusy`.  CSMA is off by default; it is appropriate for broadcast and relay channel access modes where multiple stations share a frequency.



OpenPulseHF frames follow this logical layout:

```text
magic("OPLS") | version(0x01) | sequence(u16, big-endian) | length(u8) | payload | crc16(ccitt)
```

The payload length range is 0–255 bytes.

### Frame size constraint and SAR dependency

The one-byte `length` field caps the payload at 255 bytes. This is a hard architectural constraint with the following implications:

- Any data object larger than 255 bytes requires multiple frames.
- Post-quantum signature sizes (ML-DSA-44: 2420 bytes; ML-KEM-768 public key: 1184 bytes) do not fit in a single frame. In-band PQ handshake transport requires a segmentation and reassembly (SAR) sub-layer.
- A future SAR sub-layer must define: segment ID (session-scoped), fragment number, total fragment count, and reassembly timeout.
- Until SAR is implemented, transfers above 255 bytes of application payload rely on application-layer framing (HPX session protocol) rather than the wire-layer frame format.
- Extending `length` to a `u16` field would require a frame format version bump and a migration strategy; this is a design option for evaluation during SAR planning.

## ACK frame taxonomy and turnaround timing

HPX sessions require a defined set of ACK frame types to drive ARQ and adaptive rate control. The following taxonomy is normative for HPX:

| ACK type | Meaning |
|----------|---------|
| ACK-OK | Data frame received correctly; maintain current rate |
| ACK-UP | Data frame received correctly; request rate increase |
| ACK-DOWN | Data frame received correctly; request rate decrease |
| NACK | Data frame received with uncorrectable errors; request retransmission |
| BREAK | Changeover: IRS requests to become ISS |
| REQ | ACK frame lost; request last data frame again |
| QRT | Session end; graceful teardown |
| ABORT | Session end; abnormal teardown |

ACK frames must be short enough to be demodulated reliably at lower SNR than data frames. The recommended implementation uses FSK modulation for ACK bursts independent of the data modulation in use, consistent with VARA's approach. This gives ACK frames approximately 6 dB PAPR headroom over data frames.

Turnaround timing contract:
- Transmitter must release PTT within 50 ms of last transmitted sample.
- Receiver must detect carrier and begin ACK within 150 ms of remote PTT drop.
- Total half-duplex cycle budget must be documented per mode profile.

## Frontend architecture

- CLI is production-first and defines expected behavior.
- Additional frontends may be added, but must call stable core APIs.
- Frontends must not duplicate modem logic that belongs in shared crates.

## Platform support

| Platform | Audio backend |
|----------|---------------|
| Linux | ALSA, including PipeWire through ALSA compatibility |
| macOS | CoreAudio |
| Windows | WASAPI |
| Any | In-process loopback for hardware-free testing |

## Performance architecture

- Real-time behavior depends on bounded buffering and deterministic frame timing.
- Loopback and no-default-features test paths remain fast and stable in CI.
- Optional optimization work should preserve functional parity with baseline paths.
- Modem execution should separate I/O, framing, and DSP stages so they can run on dedicated worker threads.
- Thread scheduling strategy should avoid unbounded queues and preserve deterministic latency under load.
- GPU offload should target compute-intensive DSP components only when benchmarks show net gain.
- GPU path should use open acceleration stacks (preferred: Vulkan via wgpu; optional: OpenCL) with an always-available CPU path.

## Edge platform support

- Raspberry Pi 4 and Raspberry Pi 5 are supported edge targets for HPX operation.
- ARM64 builds must preserve feature parity for signed-transfer and trust workflows.
- Resource-aware execution profiles should be available for Pi-class CPU and memory budgets.

## Extensibility architecture

- New modulation families are introduced as plugins implementing shared traits.
- Plugin APIs must remain stable enough for out-of-tree experimentation.
- Core crates should remain embeddable for future automation and integrations.

For HPX, keep signal path adaptation logic and trust/signature logic as separate internal components so they can be tested independently.

## Security architecture

- Identity management and trust evaluation are control-plane concerns.
- Transfer signing and verification are data-plane admission checks.
- Verification failures must surface clear failure reasons to frontends and logs.
- Session-state behavior for security and recovery is defined in docs/hpx-session-state-machine.md.
- Relay path trust and end-to-end signer trust are evaluated independently.

## Peer cache and query subsystem (Phase 2.5)

### PeerDescriptor

`PeerDescriptor` (`openpulse-core::peer_descriptor`) is a signed identity advertisement that a peer broadcasts during discovery.  The `peer_id` field IS the Ed25519 verifying-key bytes, so the descriptor is self-authenticating: `verify_peer_descriptor()` takes only the descriptor itself and performs no external key lookup.

Signed fields: `peer_id`, `callsign`, `capability_mask`, `timestamp_ms`.  Signature is Ed25519 over canonical JSON of these fields (same pattern as the file-transfer `TransferManifest`; the handshake frames moved to signing their transmitted bytes in #1147).

### PeerCache query API

`PeerCache::query(capability_mask, min_quality, trust_filter, max_results, now_ms)` returns live peers that match all supplied filters, sorted by `route_quality` descending and truncated to `max_results`.

`TrustFilter` values (wire code per peer-query-relay-wire.md):
- `TrustedOnly` (0x00) — `PskVerified` or `Verified` only
- `TrustedOrUnknown` (0x01) — any trust level except `Reduced`
- `Any` (0x02) — no trust constraint

**The filter is only as good as where the cached level came from (#1253).** A `trust_state` byte in
a `PeerQueryResponse` is the responder's claim *about itself*, in an envelope the receiver does not
authenticate before importing. `openpulse-mesh` used to map `0x00` to `Verified` and write it into
the cache this filter reads, so a station could vouch for itself into an operator's `TrustedOnly`
policy. It now imports every peer-asserted state as `Unknown`. Since that crate has no local trust
store, **`TrustedOnly` there relays nothing at all**, and the binary warns loudly at startup rather
than failing silently. A consumer that *does* hold local trust — the full daemon, via the signed
handshake — is the place to resolve this filter meaningfully.

`PeerRecord` now carries `capability_mask: u32`.

### Wire envelope and peer query payloads

`WireEnvelope` (`openpulse-core::wire_query`) implements the OPHF binary envelope from `docs/peer-query-relay-wire.md`:

```text
magic("OPHF") | version(u8) | msg_type(u8) | flags(u16) | session_id(u64) |
src_peer_id(32B) | dst_peer_id(32B) | nonce(12B) | timestamp_ms(u64) |
hop_limit(u8) | hop_index(u8) | payload_len(u16) | payload | auth_tag(16B)
```

Fixed header: 104 bytes.  Fixed auth_tag: 16 bytes.  Total minimum: 120 bytes.

`PeerQueryRequest` payload (msg_type 0x01): 17 bytes fixed — query_id(8), capability_mask(4), min_link_quality(2), trust_filter(1), max_results(2).

`PeerQueryResponse` payload (msg_type 0x02): query_id(8) + result_count(2) + variable results.  Each result entry: peer_id(32), callsign_hash(32), capability_mask(4), last_seen_ms(8), trust_state(1), sig_len(2), descriptor_signature(variable).

## Routing and relay architecture

- Peer cache stores signed identity and capability descriptors with aging policy.
- Query engine supports local filter queries and bounded network query propagation.
- Route planner selects direct or multi-hop path using trust and link-quality scoring.
- Relay layer enforces loop prevention, replay protection, and hop-limited forwarding.
- Wire-level relay and query envelopes are defined in docs/peer-query-relay-wire.md.

## Multi-hop relay forwarding (Phase 2.6)

### Trust-weighted path scoring

`score_route(route, cache)` computes a bottleneck (minimum) composite score across intermediate relay hops.  Each hop score is `trust_weight × route_quality` where `trust_weight` maps `TrustLevel` to: Verified=4, PskVerified=3, Unknown=2, Reduced=1.  Direct routes (no intermediate hops) receive `u32::MAX` so they are never penalized.  `select_best_scored_route` picks the highest-scoring valid candidate; ties broken by shorter path.

### RelayForwarder

`RelayForwarder` (`openpulse-core::relay`) is a stateful relay node that processes one `WireEnvelope` per call:

1. **Hop-limit enforcement** — if `hop_index >= hop_limit` the envelope is dropped and `RelayForwardError::HopLimitExceeded` returned.
2. **Duplicate suppression** — `(session_id, nonce)` is checked against a TTL-keyed map; replayed envelopes return `RelayForwardError::DuplicateDetected`.
3. **Trust policy** — `src_peer_id` (hex-encoded) is checked against `RelayTrustPolicy::denied_relays`; denied originators return `RelayForwardError::PolicyRejected`.

On success the returned envelope has `hop_index` incremented by one, ready for forwarding to the next hop.  Expired replay-suppression entries are evicted at each `forward()` call.

### Relay observability events

`RelayForwarder::drain_events()` returns buffered `RelayEvent` values since the last drain:

| Variant | Emitted when |
|---------|-------------|
| `Forwarded { session_id, hop_index_out, hop_limit }` | Envelope successfully forwarded |
| `HopLimitExceeded { session_id, hop_index }` | Hop budget exhausted |
| `DuplicateSuppressed { session_id, nonce }` | Replay detected |
| `PolicyRejected { session_id, src_peer_id }` | Originator denied by trust policy |

### Relay wire payloads

`RelayDataChunk` (msg_type 0x05, `WireEnvelope` payload): 82-byte fixed header carrying `transfer_id`, `chunk_seq`, `total_chunks`, `chunk_hash` (SHA-256), `e2e_manifest_hash` (SHA-256), `sig_len`; followed by variable-length `chunk_signature` and `chunk_data`.

`RelayHopAck` (msg_type 0x06, 49 bytes fixed): `transfer_id` (u64), `chunk_seq` (u32), `hop_peer_id` (32 bytes), `ack_status` (`Ok`/`Retry`/`Reject`), `retry_after_ms` (u16), `reason_code` (u16).

## Network query propagation (Phase 3.2)

### QueryForwarder

`QueryForwarder` (`openpulse-core::query_propagation`) propagates `PeerQueryRequest` envelopes (msg_type 0x01) across the network with the same check sequence as `RelayForwarder`:

1. **Message type check** — only `PeerQueryRequest` envelopes are accepted.
2. **Hop-limit enforcement** — `hop_index >= hop_limit` drops the envelope.
3. **Trust policy** — `src_peer_id` (hex) checked against `RelayTrustPolicy::denied_relays`.
4. **Duplicate suppression** — `(src_peer_id, query_id)` checked via `QueryPropagationTracker`; same query from same originator within TTL is suppressed.

On success: clones envelope with `hop_index += 1`; caller broadcasts to neighbouring peers.  `drain_events()` returns `QueryEvent` values (Propagated, HopLimitReached, DuplicateSuppressed, PolicyRejected).

### QueryPropagationTracker

`QueryPropagationTracker` maintains a bounded, TTL-keyed map of `(src_peer_id, query_id)` tuples seen since last eviction.  A separate `response_seen` map deduplicates responses per responder per query.  Both structures are LRU-evicted when capacity is exceeded.

### Route discovery wire payloads

`RouteDiscoveryRequest` (msg_type 0x03, 47 bytes fixed): `route_query_id` (u64), `destination_peer_id` (32 bytes), `max_hops` (u8), `required_capability_mask` (u32), `policy_flags` (u16).

`RouteDiscoveryResponse` (msg_type 0x04, variable): `route_query_id` (u64), `route_id` (u64), `hop_count` (u8), hops (`RouteHop` × hop_count, 37 bytes each), `sig_len` (u16), `route_signature`.  Each `RouteHop`: `hop_peer_id` (32 bytes), `hop_trust_state` (1 byte, `WireTrustState` code), `estimated_latency_ms` (u16), `estimated_reliability_permille` (u16).

`RelayRouteUpdate` (msg_type 0x07, variable): `route_id` (u64), `previous_hop_count` (u8), `new_hop_count` (u8), `route_change_reason` (u16, `RouteChangeReason` code), replacement hops, `route_update_signature`.

`RelayRouteReject` (msg_type 0x08, 45 bytes fixed): `route_id` (u64), `reject_hop_peer_id` (32 bytes), `reason_code` (u16), `trust_decision` (1 byte, `WireTrustState` code), `policy_reference` (u16).

## Session-layer compression (Phase 2.7)

Payload compression is applied above the FEC/modulation layer. It is **not** negotiated in the handshake — those fields were removed in #1166 because nothing consumed them (see *Negotiation* below).

- **Algorithm**: LZ4 block format with a 4-byte little-endian decompressed-size prefix (`lz4_flex::compress_prepend_size` / `decompress_size_prepended`; pure Rust, no C bindings). This is **not** the standard LZ4 frame container format and is not interoperable with LZ4 frame readers. Chosen for speed and determinism on embedded targets.
- **Negotiation**: none. **Removed in #1166** (the #1147 wire-format break): nothing consumed the selection — the daemon sent the lists empty and hardcoded `None`/`None` — so the field was a capability claim the station could not back. Session compression itself is unchanged; only the *handshake negotiation of it* is gone. The signing-mode analogue of that check does exist and was *added* in the same change (`HandshakeError::UnofferedSigningMode`).
- **Compress-then-compare**: `compress_if_smaller(payload)` tries LZ4; if the compressed form is not smaller the original bytes and `CompressionAlgorithm::None` are returned. This prevents length inflation on already-compressed or short payloads.
- **Decompression size guard**: before allocating, `decompress` reads the size prefix and rejects inputs whose claimed decompressed size exceeds `MAX_DECOMPRESSED_SIZE` (64 005 bytes, matching the SAR max-segment limit) to prevent OOM on malicious frames.
- **Decompression error**: treated as a frame error; the frame is discarded at the session layer.

## Post-quantum in-band handshake (Phase 3.1)

### Algorithms

- **ML-DSA-44 (FIPS 204)**: post-quantum digital signature. Verifying key: 1312 bytes. Signature: 2420 bytes. Signing key stored as 32-byte seed.
- **ML-KEM-768 (FIPS 203)**: post-quantum key encapsulation mechanism. Encapsulation key (EK): 1184 bytes. Decapsulation key stored as 64-byte `d||z` seed. Ciphertext: 1088 bytes. Shared secret: 32 bytes.

### Signing modes

`PqConReq` / `PqConAck` frames extend the classical handshake with two new `SigningMode` variants:

| Mode | Signatures | Strength rank |
|------|-----------|---------------|
| `Pq` | ML-DSA-44 only | 4 |
| `Hybrid` | Ed25519 + ML-DSA-44 | 5 (highest) |

The signed body is the **transmitted binary prefix** — `magic || version || length || body` — with the signatures trailing and unsigned, matching the classical `ConReq`/`ConAck` frames since #1147. There is no second representation to sign, so verification hashes what was received. (Before #1147 this was canonical JSON, which was a label rather than a property: the encoder emitted serde declaration order and sorted nothing.)

### SAR transport

Post-quantum key material exceeds a single 255-byte frame.  `encode_pq_conreq` / `encode_pq_conack` emit the binary format (#1147); the caller passes the bytes to `sar_encode` which fragments across up to 255 frames (max 64 005 bytes).  `SarReassembler` collects fragments and delivers the reassembled blob to `decode_pq_conreq` / `decode_pq_conack`.

Encoded `PqConReq` size: **5 049 bytes** (21 SAR fragments at 251 bytes/fragment), pinned as a known-answer vector in `crates/openpulse-core/tests/handshake_kat.rs`. The ≈ 6 700 B / 27-fragment figure this replaces was an estimate that matched neither the JSON encoding as measured (≈ 18 000 B) nor the binary one.

### Key exchange flow

1. Initiator calls `create_pq_conreq`: advertises EK + signing modes; signs with ML-DSA-44 (and Ed25519 for Hybrid).
2. Responder calls `create_pq_conack`: selects mode, encapsulates EK → ciphertext + shared secret; signs reply.
3. Initiator calls `kem_decapsulate(dk, ack.kem_ciphertext)` to recover the same 32-byte shared secret.

Both `verify_pq_conreq` and `verify_pq_conack` run ML-DSA-44 (and Ed25519 when applicable) before calling `evaluate_handshake` for trust policy evaluation.

## Radio interface architecture

The radio interface layer sits between the audio backend and the physical transceiver.

- PTT control must be abstracted behind a `PttController` trait with implementations for: no-op (loopback), serial RTS/DTR, VOX (audio-triggered), and CAT/rigctld.
- The CAT/rigctld implementation communicates with a running `rigctld` daemon from the Hamlib project, which supports over 300 amateur transceivers.
- Audio level monitoring must be available as a diagnostic output so operators can set appropriate drive levels without external tools.
- AFC state (estimated frequency offset in Hz) must be exposed in session diagnostics.

## Documentation process constraints

- Documentation updates flow through pull requests only.
- Frontmatter validation and stamping automation are required quality gates.
