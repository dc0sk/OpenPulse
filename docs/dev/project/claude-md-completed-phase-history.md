---
project: openpulsehf
doc: docs/dev/project/claude-md-completed-phase-history.md
status: archive
last_updated: 2026-08-29
---

# CLAUDE.md — archived completed-phase history

Moved out of `CLAUDE.md` by #1169. Every block below describes work that is **finished**; it was
costing a third of the agent contract's tokens on every session's first read while deciding nothing
about work still to be done.

Nothing here is live guidance. The rules an agent must follow stay in `CLAUDE.md`: *Build and test
commands*, the crate map, *Coding conventions*, *Known sharp edges*, the *DSP acquisition &
carrier-recovery playbook*, *Verification mechanics*, *Adversarial review*, and the
acceptance-criteria table. The narrative history of the project lives in
`docs/dev/project/roadmap.md`; the requirement→test chain lives in
`docs/dev/project/traceability.md`. This file is the per-PR implementation chronology that used to
sit between them.

The two "Resolved" design questions at the end are kept for the same reason as the rest: they record
a decision that has already been implemented, so they answer "why is it like this" and never "what
should I do next".

---

**Recently shipped (PRs #316–#321)**:
- `crates/openpulse-daemon/src/lib.rs`: QSY RF wiring — `QsySession` state machine wired into `AcceptQsy`; QSY_REQ + QSY_LIST frames transmitted via modem engine; `process_received_bytes` drives responder role from incoming RF (PR #321)
- `crates/openpulse-daemon/src/lib.rs`: CrossBandRepeater wiring — pre-built in `main.rs`; `EnableRepeater` spawns thread via `run_full_duplex`; `DisableRepeater` stops and joins it (PR #321)
- `apps/openpulse-panel/src/app.rs`: mode list updated to include RRC modes added in #319 and correct SCFDMA names (PR #321)
- `plugins/scfdma`: DFT-CE pilot-aided channel estimation; SCFDMA52-16QAM, SCFDMA52-32QAM (cross-32QAM), SCFDMA52-64QAM, SCFDMA52-64QAM-P4 modes; MMSE equalization (PR #316)
- `crates/openpulse-modem/src/arq_session.rs` (since refactored — the ARQ/HARQ logic now lives in `harq.rs` + `rate_policy.rs`): `ArqSession` — ARQ retry loop with soft LLR accumulation across retransmissions; runtime mode switching between registered plugins (PR #318)
- `crates/openpulse-core/src/profile.rs`: `hpx_narrowband_hd()` profile — SL8=QPSK9600-RRC, SL9=8PSK9600-RRC; `hpx_narrowband()` gains QPSK2000-RRC (SL10) and 8PSK2000-RRC (SL11) (PR #319)
- `plugins/qpsk`: `QPSK2000-RRC` and `QPSK9600-RRC` modes; `plugins/psk8`: `8PSK2000-RRC` and `8PSK9600-RRC` modes (PR #319)
- `crates/openpulse-daemon/src/main.rs`: PTT controller wired from config; `apply_command_to_engine` skips dispatch on PTT hardware assertion failure (PR #319)
- `apps/openpulse-testmatrix`: LDPC FEC entries added (PR #319)
- `crates/openpulse-core/src/profile.rs`: `hpx_wideband_hd()` updated to SL12–SL15 (SCFDMA52-16QAM → SCFDMA52-64QAM → 64QAM2000-RRC); ACK-UP gate at SL14 protecting SL15 admission (PR #320)
- `crates/openpulse-b2f/src/session.rs`: `queue_message_type_c()` — ISS Type C proposals (PR #320). **REMOVED (PR #948):** its external-Winlink compatibility was never verified and could not have held (LHA `LH5` vs FBB's Okumura LZHUF are different bitstreams); the path had no production caller. Inbound Type C is now answered `Reject`.

**Previously shipped (PRs #193–#195)**:
- `crates/openpulse-b2f`: `compress_lzhuf_winlink` / `decompress_lzhuf_winlink` — 4-byte LE prefix (PR #193). **REMOVED (PR #948)** — "matching Winlink Type C convention" was an unverified claim; see the PR #320 entry above.
- `crates/openpulse-dsp`: `LmsEqualizer` — complex symbol-rate LMS/DFE, supervised preamble training then decision-directed; wired into BPSK-RRC demodulation path after Gardner TED (PR #194)
- `plugins/64qam`: full 64QAM plugin — Gray-coded 8×8 PAM-8 constellation, rectangular-windowed and RRC modulator/demodulator, max-log-MAP soft demodulator; modes `64QAM500`, `64QAM1000`, `64QAM2000-RRC` (PR #195)
- `crates/openpulse-core/src/rate.rs`: `SpeedLevel` extended to SL20 (PR #195)
- `crates/openpulse-core/src/profile.rs`: initial `hpx_wideband_hd()` profile (SL12–SL14); profile slot arrays widened to 21 (PR #195)

**Previously shipped (PRs #187–#192)**:
- `plugins/psk8`: max-log-MAP `demodulate_soft()` replacing ±1.0 fallback
- `openpulse-cli`: `manifest verify` fully wired to `verify_manifest()`
- `openpulse-core::ldpc`: real rate-1/2 min-sum BP replacing passthrough stub
- `openpulse-modem`: `transmit_with_ldpc` / `receive_with_ldpc` and `transmit_with_fec_mode` / `receive_with_fec_mode` dispatch (multi-block since PR #691; a `Frame`'s `u8` payload length caps it at three 128-byte blocks)
- `openpulse-core::trust_store_file`: `load_trust_store_from_file()` — parses CLI JSON trust store format into `InMemoryTrustStore`
- ARDOP + KISS bridges: trust store loaded at startup; `RelayForwarder` wired into worker receive loop when `relay.enabled`

Execute Phase 1 tasks in this order. Tasks within the same group are independent and may be parallelised.

### Group 1 — ✅ Complete

**1.1a — Re-enable multi-platform CI** ✅ Done (PR #67)

**1.1b — Add block interleaver to `openpulse-core`** ✅ Done (PR #68)

**1.1c — Create `openpulse-channel` crate scaffold** ✅ Done (PR #69)

**1.1d — Refactor `openpulse-cli/src/main.rs`** ✅ Done
- `src/commands/` module with one file per subcommand
- `main.rs` is under 150 lines; all CLI behavior preserved

### Group 2 — ✅ Complete

**1.4 — Implement channel models** ✅ Done (PR #71)
Full spec in `docs/dev/design/testbench-design.md` and `docs/dev/benchmark-harness.md`.

**1.3 — Wire interleaver into `FecCodec` and `ModemEngine`** ✅ Done (PR #70)

**1.5 — Radio interface layer (`openpulse-radio` crate)** ✅ Done
- `PttController` trait, `PttError`, `NoOpPtt`, `SerialRtsDtrPtt`, `VoxPtt`, `RigctldPtt` all implemented and tested

### Group 3 — ✅ Complete

**1.2 — SAR (Segmentation and Reassembly)** ✅ Done (Option B)
- `crates/openpulse-core/src/sar.rs`: `sar_encode()` and `SarReassembler` with `ingest()` / `expire()`
- SAR header: `segment_id (u16) | fragment_index (u8) | fragment_total (u8)` — 4 bytes, leaving 251 bytes data per fragment
- Max transportable segment: 255 × 251 = 64 005 bytes; encoder rejects larger inputs with `SarError::DataTooLarge`
- Reassembly keyed on `(session_id, segment_id)`; configurable timeout; duplicate fragments are idempotent
- Integration tests: `tests/sar_roundtrip.rs` — 256-byte, 64 KB, max-size round-trips; missing-fragment; timeout; session isolation

### Remaining 1.5 integration items

**1.5a — AFC loop in BPSK demodulator** ✅ Done
- `estimate_frequency_offset()` in `plugins/bpsk/src/demodulate.rs` — IQ-squaring estimator
- `afc_estimate_hz()` — pure function, called via `ModulationPlugin::estimate_afc_hz()` default override in BPSK plugin
- `ModemEngine::last_afc_offset_hz()` — getter updated after every receive call
- `SessionDiagnostics::afc_offset_hz` — `Option<f32>` field, ready for CLI to populate
- Tracking range: ±baud_rate/4 (BPSK250: ±62.5 Hz; BPSK31: ±7.8 Hz)

**1.5b — PTT CLI wiring** ✅ Done
- `--ptt <none|rts|dtr|vox|rigctld>` and `--rig <address:port>` global options added to `openpulse-cli`
- `crates/openpulse-cli/src/radio.rs`: `build_ptt_controller()` factory selects backend at startup
- `commands/transmit.rs`: wraps transmit with PTT assert/release (release guaranteed on TX error)
- `serial` feature added to `openpulse-cli` propagates to `openpulse-radio/serial`
- Integration tests: `ptt_wiring_integration.rs` — none/default/unknown-backend cases

### Phase 2 — ✅ Partial (2.1, 2.2, 2.3, 2.4 complete)

**2.1 — ACK taxonomy and rate adaptation** ✅ Done
- `crates/openpulse-core/src/ack.rs`: `AckType` (8 variants, `#[repr(u8)]`), `AckFrame` (5-byte codec with CRC-8/SMBUS and FNV-1a session hash), `AckError`
- `crates/openpulse-core/src/rate.rs`: `SpeedLevel` (SL1–SL11), `RateEvent`, `RateAdapter::apply_ack()` state machine
  - ACK-DOWN floors at SL2; SL1 only reachable via 3 consecutive NACKs at SL2 (ChirpFallback)
  - 3 consecutive NACKs at SL3+ → NackDecrement; nack_threshold configurable (default 3)
- `plugins/fsk4/`: `Fsk4Plugin` implementing `ModulationPlugin` for mode `"FSK4-ACK"`
  - 4 tones at fc±50 Hz and fc±150 Hz (default fc=1050 Hz), 100 baud, Hann-windowed, Goertzel demodulator
  - 5 bytes = 20 symbols = 200 ms at 8 kHz
- Integration tests: `crates/openpulse-core/tests/rate_adaptation.rs` (11 tests); FSK4 loopback in `plugins/fsk4/src/lib.rs` (3 tests)

**2.2 — HPX500 and HPX2300 adaptive profiles** ✅ Done
- `plugins/qpsk/src/lib.rs`: added `QPSK1000` mode (1000 baud, 8 samples/symbol @ 8 kHz)
- `plugins/psk8/`: `Psk8Plugin` implementing `ModulationPlugin` for `"8PSK500"` and `"8PSK1000"`
  - Gray-coded 8PSK constellation (8 phases, 3 bits/symbol); Hann-windowed modulator; nearest-point IQ demodulator
  - HPX2300 waveform decision: single-carrier chosen over OFDM (lower PAPR, no cyclic prefix, simpler AFC — see `docs/dev/design/architecture.md`)
- `crates/openpulse-core/src/profile.rs`: `SessionProfile` struct mapping `SpeedLevel` → mode string
  - `SessionProfile::hpx500()`: SL2=BPSK31, SL3=BPSK63, SL4=BPSK250, SL5=QPSK250, SL6=QPSK500; initial=SL2
  - `SessionProfile::hpx2300()`: SL8=QPSK500, SL9=QPSK1000, SL11=8PSK1000; initial=SL8
- `crates/openpulse-modem/src/engine.rs`: `start_adaptive_session()`, `apply_ack()`, `current_adaptive_mode()` wired to `RateAdapter`
- Integration tests: `crates/openpulse-core/tests/session_profile.rs` (4 tests); `crates/openpulse-modem/tests/adaptive_profile_integration.rs` (8 tests); psk8 loopback (5 tests)

**2.3 — Signed handshake and manifest verification** ✅ Done
- `crates/openpulse-core/Cargo.toml`: added `ed25519-dalek 2.0` (signing/verification); `rand 0.8` in dev-deps
- `crates/openpulse-core/src/handshake.rs`: `ConReq`, `ConAck` wire frames with Ed25519 sign/verify; `TrustStore` trait; `InMemoryTrustStore`; `verify_conreq()`, `verify_conack()`
  - CONREQ: MAGIC "HSCQ" + VERSION + LENGTH + JSON; signature covers canonical JSON of body fields
  - CONACK: MAGIC "HSAK" + VERSION + LENGTH + JSON; echoes CONREQ session_id; signature covers canonical JSON
  - **SUPERSEDED by #1147 (v2, 2026-08-23):** both frames are now BINARY, version `0x02`, `u16`
    length, and the signature covers the **transmitted prefix** rather than any "canonical" form —
    the keys were never sorted, so canonical was a label, not a property. The CONACK no longer
    echoes `session_id` (it binds by SHA-256 over the whole transmitted CONREQ), a `dst_station`
    was added so only the addressed station answers (#1178), and `supported_compression` /
    `supported_fec_modes` were deleted (#1166 — nothing consumed the selection). Both frames now
    fit ONE SAR fragment (236 B / 237 B against 251 B) where v1 took three
  - Trust evaluation wired to existing `evaluate_handshake()` / `classify_connection_trust()` in `trust.rs`
  - Revoked key → `TrustError::RejectedTrustLevel`; no mutual mode → `TrustError::NoMutualSigningMode`
- `crates/openpulse-core/src/manifest.rs`: `TransferManifest` with SHA-256 payload hash, sender ID, Ed25519 signature; `verify_manifest()` and `TransferManifest::sign()`
- Integration tests: `crates/openpulse-core/tests/handshake_integration.rs` (12 tests); `crates/openpulse-core/tests/manifest_integration.rs` (6 tests)
  - Tests cover: happy path, tampered signature, revoked key, session ID mismatch, no mutual mode, full round-trip

**2.4 — DCD and CSMA channel access** ✅ Done
- `crates/openpulse-core/src/dcd.rs`: `DcdState` struct — RMS energy threshold, configurable hold window (default 100 ms at 8 kHz), `update(samples)`, `is_busy()`, `energy()`, `force_busy()`
- `crates/openpulse-core/src/error.rs`: added `ModemError::ChannelBusy` variant
- `crates/openpulse-modem/src/engine.rs`: DCD update wired into `receive()` after sample capture; 0.3-persistence `csma_check()` called at the head of each transmit path (pre-encode, to avoid burning a sequence number on deferral — including `broadcast()` as of the G-1 fix); `enable_csma()`, `disable_csma()`, `is_channel_busy()`, `dcd_energy()` public API; `rand 0.8` added to `[dependencies]`
- Integration tests: `crates/openpulse-modem/tests/csma_loopback.rs` (4 tests)
  - DCD detects energy from received signal; CSMA blocks on busy channel; disabled CSMA ignores DCD; two-station deferral scenario

**2.5 — Peer cache and query subsystem** ✅ Done
- `crates/openpulse-core/src/peer_descriptor.rs`: `PeerDescriptor` self-authenticating signed identity descriptor
  - `peer_id` IS the Ed25519 verifying-key bytes; `verify_peer_descriptor()` needs no external key store
  - Signed fields: `peer_id`, `callsign`, `capability_mask`, `timestamp_ms` via canonical JSON (same pattern as CONREQ/CONACK)
  - `callsign_hash()` returns SHA-256 of callsign bytes for use in query response entries
- `crates/openpulse-core/src/peer_cache.rs`: extended `PeerRecord` with `capability_mask: u32`
  - Added `TrustFilter` enum (TrustedOnly, TrustedOrUnknown, Any) — wire codes per peer-query-relay-wire.md
  - Added `PeerCache::query(capability_mask, min_quality, trust_filter, max_results, now_ms)` — sorted by quality descending
- `crates/openpulse-core/src/wire_query.rs`: OPHF binary envelope + peer query payloads
  - `WireEnvelope`: encode/decode per docs/dev/peer-query-relay-wire.md; header 104 B + payload + an **optional** 64 B Ed25519 origin signature (wire v2 replaced the old unauthenticated 16 B `auth_tag`; E3)
  - `PeerQueryRequest` (msg_type 0x01): 17-byte fixed payload
  - `PeerQueryResponse` (msg_type 0x02): variable-length results with descriptor_signature
- Integration tests: `tests/peer_descriptor_integration.rs` (9 tests); `tests/wire_query_integration.rs` (9 tests)

**2.6 — Multi-hop relay forwarding** ✅ Done
- `crates/openpulse-core/src/relay.rs`: extended with `RelayForwarder`, `RelayEvent`, `RelayForwardError`, `score_route`, `select_best_scored_route`
  - `RelayForwarder`: stateful relay node — hop-limit enforcement, (session_id, nonce) duplicate suppression with TTL eviction, src_peer_id trust-policy check
  - On success: clones envelope with `hop_index += 1`; emits `RelayEvent::Forwarded`
  - On failure: returns typed `RelayForwardError`; emits corresponding `RelayEvent` for observability
  - `drain_events()` returns buffered events (Forwarded, HopLimitExceeded, DuplicateSuppressed, PolicyRejected)
- Trust-weighted path scoring: `score_route` uses bottleneck (min) hop score = `trust_weight × route_quality`
  - `trust_weight`: Verified=4, PskVerified=3, Unknown=2, Reduced=1
  - Direct routes (no intermediate hops) score `u32::MAX` — never penalized
  - `select_best_scored_route`: highest-score wins; ties broken by shorter path
- `crates/openpulse-core/src/peer_cache.rs`: added `peek(&self)` — read-only lookup for route scoring (no LRU update)
- `crates/openpulse-core/src/wire_query.rs`: extended `WireMsgType` (added 0x03–0x08); added `RelayDataChunk` (msg_type 0x05, 82-byte header + variable sig/data), `AckStatus` enum, `RelayHopAck` (msg_type 0x06, 49 bytes fixed)
- Integration tests: `tests/relay_integration.rs` (12 tests)
  - Path scoring, bottleneck scoring, best-route selection, policy denial
  - 3-hop hop_index increment across separate `RelayForwarder` nodes
  - Hop-limit drop, duplicate suppression, trust-policy rejection at hop
  - Event drain verification, `RelayDataChunk` round-trip, `RelayHopAck` round-trip

**3.2 — Network query propagation** ✅ Done
- `crates/openpulse-core/src/wire_query.rs`: added remaining wire payload codecs
  - `WireTrustState` enum (0x00–0x03); `RouteChangeReason` enum (u16, 5 variants); `RouteHop` (37 bytes per hop)
  - `RouteDiscoveryRequest` (msg_type 0x03, 47 bytes fixed): route_query_id, destination_peer_id, max_hops, required_capability_mask, policy_flags
  - `RouteDiscoveryResponse` (msg_type 0x04, variable): route_query_id, route_id, hops(Vec<RouteHop>), route_signature
  - `RelayRouteUpdate` (msg_type 0x07, variable): route_id, previous_hop_count, route_change_reason, replacement_hops, route_update_signature
  - `RelayRouteReject` (msg_type 0x08, 45 bytes fixed): route_id, reject_hop_peer_id, reason_code, trust_decision, policy_reference
  - Added `WireQueryError::HopCountExceeded` and `WireQueryError::SignatureTooLarge`
- `crates/openpulse-core/src/query_propagation.rs`: added `QueryForwarder` and events
  - `QueryForwarder`: stateful propagation node — msg-type check, hop-limit enforcement, trust-policy check, (src_peer_id, query_id) duplicate suppression via `QueryPropagationTracker`
  - On success: clones envelope with `hop_index += 1`; emits `QueryEvent::Propagated`
  - `QueryForwardError` (4 variants) and `QueryEvent` (4 variants) for observability
- Integration tests: `tests/query_propagation_integration.rs` (12 tests)
  - Wire codec round-trips: `RouteDiscoveryRequest`, `RouteDiscoveryResponse` (no-hop and multi-hop), `RelayRouteUpdate`, `RelayRouteReject`
  - `QueryForwarder`: basic propagation, hop-limit, duplicate suppression, trust-policy rejection, event drain
  - 3-node query chain verifying hop_index increment across separate `QueryForwarder` nodes

**2.7 — Compression (session layer)** ✅ Done
- `crates/openpulse-core/Cargo.toml`: added `lz4_flex 0.11` (pure-Rust LZ4 implementation, no C bindings)
- `crates/openpulse-core/src/compression.rs`: new module — `CompressionAlgorithm` enum (`None`, `Lz4`), `compress()`, `decompress()`, `compress_if_smaller()` (compress-then-compare; returns original bytes with `None` if LZ4 is not smaller)
- `crates/openpulse-core/src/handshake.rs`: added `supported_compression: Vec<CompressionAlgorithm>` to `ConReq` and `selected_compression: CompressionAlgorithm` to `ConAck`; both fields are included in the canonical JSON covered by the Ed25519 signature, so post-signing injection is detectable. **REMOVED by #1166 in the #1147 wire break** — nothing consumed the selection, so the field was a capability claim the station could not back (the same reasoning that removed Type C in PR #948)
- Integration tests: `tests/compression_integration.rs` (9 tests)
  - Codec round-trips (`None`, `Lz4`), decompression of garbage returns error
  - `compress_if_smaller` picks Lz4 for compressible data, keeps original for incompressible
  - `ConReq`/`ConAck` carry and verify compression fields; full negotiation round-trip; tampered compression field invalidates signature

**3.1 — Post-quantum in-band handshake** ✅ Done
- `crates/openpulse-core/Cargo.toml`: added `ml-dsa 0.1.0-rc.9` (ML-DSA-44, `rand_core` feature), `ml-kem 0.3` (`getrandom` feature), `rand 0.8` promoted from dev-deps to deps
- `crates/openpulse-core/src/trust.rs`: added `SigningMode::Pq` (ML-DSA-44 only, strength 4) and `SigningMode::Hybrid` (Ed25519 + ML-DSA-44, strength 5); updated `mode_strength()` and `allowed_signing_modes()` for all three policy profiles
- `crates/openpulse-core/src/pq_handshake.rs`: new module — ML-DSA-44/ML-KEM-768 post-quantum handshake
  - Size constants: `ML_DSA_44_PUBKEY_SIZE=1312`, `ML_DSA_44_SIG_SIZE=2420`, `ML_KEM_768_EK_SIZE=1184`, `ML_KEM_768_DK_SIZE=64` (d||z seed), `ML_KEM_768_CT_SIZE=1088`, `ML_KEM_768_SS_SIZE=32`
  - `PqConReq` / `PqConAck` wire frames with classical + PQ pubkeys, KEM pubkey/ciphertext, dual signatures
  - `generate_ml_dsa_44_keypair()`, `generate_ml_kem_768_keypair()` — OsRng-based key generation
  - `create_pq_conreq()`, `create_pq_conack()` — build and sign frames; `Hybrid` signs with both Ed25519 and ML-DSA-44; `Pq`-only mode leaves `classical_signature` empty
  - `kem_decapsulate(dk, ct)` — recovers 32-byte shared secret from `PqConAck.kem_ciphertext`
  - `verify_pq_conreq()`, `verify_pq_conack()` — verify both signatures then call `evaluate_handshake()`
  - `encode_pq_conreq/ack`, `decode_pq_conreq/ack` — JSON serde helpers for SAR transport
- `crates/openpulse-cli/src/output.rs`: added `Pq` and `Hybrid` arms to `signing_mode_to_str()`
- Integration tests: `tests/pq_handshake_integration.rs` (12 tests)
  - Key sizes, KEM shared-secret match, Hybrid and Pq-only round-trips, mode negotiation, tamper rejection (PQ sig, classical sig, session ID mismatch), SAR size gate, SAR encode→fragment→reassemble→decode round-trip

**PKI service-side trust-bundle signing** ✅ Done (PR #88)
- `pki-tooling/Cargo.toml`: added `rand = "0.8"` for explicit dev-only ephemeral key generation
- `pki-tooling/src/lib.rs`: `AppState` gains `signing_key: ed25519_dalek::SigningKey`; new route `GET /api/v1/signing-key`
- `pki-tooling/src/main.rs`: requires `PKI_SIGNING_KEY` (base64 32-byte seed) by default; explicit dev fallback requires `PKI_ALLOW_EPHEMERAL_KEY=true`; bind address is configurable with `PKI_BIND_ADDR`
- `pki-tooling/src/verification.rs`: added `bundle_canonical_body()` (with recursive key-sort for JSONB stability) and `verify_bundle_signature()`; 8 unit tests
- `pki-tooling/src/api/handlers.rs`: service computes Ed25519 signature at publish time; `bundle_signature` removed from request; `service_pubkey` persisted per-row and returned in `TrustBundleResponse`; `get_signing_key` handler
- `pki-tooling/migrations/0010_trust_bundle_service_pubkey.sql`: adds `service_pubkey TEXT NOT NULL DEFAULT ''` to `trust_bundles`

**Phase 3.5-substitute — sound-loopback channel simulation** ✅ Done (PR #89)
- `crates/openpulse-audio/src/loopback.rs`: added `drain_samples()` and `fill_samples()` test-utility methods to `LoopbackBackend`
- `crates/openpulse-modem/src/channel_sim.rs`: new `ChannelSimHarness` wiring two `ModemEngine` instances through `openpulse_channel::ChannelModel`; `route()` and `route_clean()` methods
- `crates/openpulse-modem/tests/channel_loopback.rs`: 6 integration tests (clean passthrough, AWGN 20 dB, Watterson F1, Watterson F2 negative, G-E light+FEC positive, G-E burst negative)
- `crates/openpulse-channel/src/watterson.rs`: fix `fading_coeff` bug — was passing loop-local index instead of absolute sample index, causing O(n) envelope FFT refills per `apply()` call; fixed to O(n/1024)

**Phase 3.2 — Convolutional FEC evaluation** ✅ Done
- `crates/openpulse-core/src/conv.rs`: `ConvCodec` — rate-1/2, K=3 (4-state), generators G={7,5} octal, hard-decision Viterbi decoder; same `encode/decode` interface as `FecCodec`
- Benchmark: at channel BER 1%, RS post-decode BER = 0.497 vs ConvCodec = 0.0004 (AWGN regime; RS fails because random errors exceed 16-byte/block capacity); CPU overhead 3.8×
- Decision: **ACCEPTED** — ConvCodec is an optional alternative FEC for AWGN-dominant paths; RS+interleaver remains default for HF burst-error profiles
- 6 integration tests in `crates/openpulse-core/tests/fec_comparison.rs`; decision documented in `docs/dev/research/vara-research.md`

**Phase 3.3 — GPU compute acceleration for BPSK DSP** ✅ Done (PR #90)
- `crates/openpulse-gpu/`: new crate — `GpuContext` (wgpu device + pre-compiled pipelines), WGSL kernels for BPSK modulation, IQ demodulation, and timing offset search
- `plugins/bpsk/`: `gpu` feature flag; `BpskPlugin::with_gpu(Arc<GpuContext>)`; GPU dispatch in modulate and demodulate paths; CPU fallback when GPU readback returns `None`
- All GPU functions return `Option<T>` so callers can detect failures rather than silently getting empty/zero data
- GPU-vs-CPU equivalence tests under `#[cfg(feature = "gpu")]`

**Phase 3.4 — ARDOP-compatible TCP interface** ✅ Done
- `crates/openpulse-ardop/`: new crate with library + `openpulse-tnc` binary
  - `state.rs`: `TncState` enum (Disc, Listen, Connecting, Connected, Disconnecting) with ARDOP state labels
  - `bridge.rs`: `ModemBridge` — shared state, `broadcast` event/data channels, sync TX queue, background worker thread
  - `command.rs`: ASCII line protocol — VERSION, MYID, LISTEN, CONNECT, DISCONNECT, ABORT, STATE, BUFFER, PTT, CLOSE
  - `data.rs`: `u16 BE` length-prefixed binary framing in both directions
  - `main.rs`: binary reads `ARDOP_CMD_PORT`, `ARDOP_DATA_PORT`, `ARDOP_MODE`, `ARDOP_BIND` env vars
  - Loopback mode (`ArdopConfig::loopback`) echoes TX data as RX data for protocol-level integration tests
- `crates/openpulse-ardop/tests/ardop_integration.rs`: 8 tests — VERSION, MYID, STATE, CONNECT/DISCONNECT, ABORT, BUFFER, data port single-frame and multi-frame loopback

**Phase 4.2 — Structured JSON event stream** ✅ Done
- `crates/openpulse-core/src/hpx.rs`: added `#[derive(Serialize, Deserialize)]` to `HpxState`, `HpxEvent`
- `crates/openpulse-core/src/rate.rs`: added `#[derive(Serialize, Deserialize)]` to `RateEvent`
- `crates/openpulse-modem/src/event.rs`: new `EngineEvent` enum (8 variants, NDJSON-ready); `SessionStarted.session_id` is `Option<String>`; `SessionStarted.peer_modes` (not `peer`)
- `crates/openpulse-modem/src/engine.rs`: added `broadcast::Sender<EngineEvent>` field; `subscribe()` method; events emitted at transmit/receive/apply_ack/hpx_apply_event/begin_secure_session/end_secure_session with DCD change detection; `RateChange` only emitted when adaptive session is active
- `crates/openpulse-cli/src/commands/monitor.rs`: `openpulse monitor --mode <MODE>` subcommand streaming NDJSON to stdout; fatal errors propagated; stdout flushed per event
- Integration tests: `crates/openpulse-modem/tests/engine_events.rs` (7 tests including DcdChange and AfcUpdate)

**Phase 4.1 — TUI frontend** ✅ Done
- `crates/openpulse-tui/`: new binary crate using ratatui 0.27 + crossterm
  - `src/app.rs`: `App` state struct updated by `EngineEvent`s; transitions ring buffer (last 50); pause/scroll support
  - `src/ui.rs`: three-panel layout — HPX state (colour-coded), AFC/rate meters + DCD energy bar, scrollable transitions log
  - `src/events.rs`: `spawn_worker()` runs engine receive loop in a background thread; `drain_worker()` applies events to `App`
  - `src/main.rs`: 100 ms tick loop; keyboard: `q`/Ctrl+C quit, `p` pause, ↑↓ scroll

**Phase 4.3 — KISS and AX.25 interface** ✅ Done
- `crates/openpulse-kiss/`: new binary crate with `openpulse-kisstnc` binary
  - `src/kiss.rs`: KISS frame encode/decode with full byte stuffing (FEND/FESC/TFEND/TFESC); `KISS_DATA=0x00` type constant
  - `src/ax25.rs`: AX.25 UI frame encode/decode — `Ax25Addr` (callsign + SSID), `Ax25UiFrame`; Control=0x03, PID=0xF0; callsign wire encoding via 1-bit left-shift
  - `src/bridge.rs`: `KissBridge` with `broadcast::Sender<Vec<u8>>` RX channel and `std::sync::mpsc::SyncSender<Vec<u8>>` TX queue; OS-thread worker loop mirrors `openpulse-ardop`
  - `src/server.rs`: single TCP listener; per-client task reads FEND-delimited KISS frames, KISS-encodes RX payloads back to clients
  - `src/main.rs`: reads `KISS_PORT` (default 8100), `KISS_BIND`, `KISS_MODE` env vars
- Integration tests: `tests/kiss_integration.rs` (8 tests)
  - KISS codec round-trip, FEND/FESC byte stuffing, AX.25 callsign parse and UI frame round-trip
  - TCP single-frame loopback, multi-frame loopback, byte-stuffed payload loopback

**Phase 4.4 — B2F protocol and Winlink gateway integration** ✅ Done
- `crates/openpulse-b2f/`: new pure-protocol library crate (no tokio, no modem engine dependency)
  - `src/banner.rs`: WL2K connection banner encode/decode — `[WL2K-3.0-B2FWINMOR-4.0-XXXXXXXX]`; FNV-1a session key
  - `src/frame.rs`: B2F control frame codec — `Fc`, `Fs`, `Ff`, `Fq`; `ProposalType` (C/D); `FsAnswer` (Accept/Reject/Defer); CR-terminated ASCII
  - `src/header.rs`: WL2K message header encode/decode; RFC-5322-like CRLF-terminated; `WlHeader`, `AttachmentInfo`
  - `src/compress.rs`: Gzip (type D) via `flate2`; LZHUF (type C) pass-through stubs
  - `src/session.rs`: `B2fSession` state machine; `SessionRole::Iss`/`Irs`; Handshake→ProposalExchange→Transfer→Done; handles ISS-immediate-proposal pattern
- `crates/openpulse-ardop/src/bridge.rs` + `command.rs`: Pat-compatible ARDOP commands — GRIDSQUARE, ARQBW, ARQTIMEOUT, CWID, SENDID, PING; `gridsquare/arq_bw/arq_timeout` fields with `Arc<RwLock<>>` sharing
- Integration tests: `crates/openpulse-b2f/tests/b2f_integration.rs` (9 tests); `ardop_integration.rs` extended to 11 tests

**Phase 4.5 — Signal-path testbench GUI** ✅ Done
- `apps/openpulse-testbench/`: new egui/eframe 0.29 binary crate
  - 4-column live view: TX (clean), Noise channel, Mixed (TX+noise), RX (decoded)
  - Per-tap: spectrum line plot (FFT dBFS) + plasma-colourmap waterfall texture
  - Toolbar: mode (BPSK31–QPSK500), noise model (7 models), SNR slider, FEC toggle, seed, dB range sliders
  - Stats bar: runs / OK / fail / BER / last event from rolling log
  - Signal thread uses `bpsk-plugin`/`qpsk-plugin` directly; `Arc<RwLock<TapData>>` shared with UI; `crossbeam_channel` stop signal
  - All 7 channel models wired through `build_channel()` factory from `openpulse-channel`

**Phase 5.1 — B2F session driver** ✅ Done (PR #98)
- `crates/openpulse-b2f-driver/`: new pure-std crate (no tokio); `B2fDriver`, `DecodedMessage`, `DriverError`
  - `src/cmd.rs`: `CmdPort` — BufReader<TcpStream> + write half; `TimedOut`/`WouldBlock` mapped to `DriverError::Timeout`
  - `src/data.rs`: `DataPort` — u16 BE length-prefixed frames; send validated against u16::MAX
  - `run_iss()`: MYID→CONNECT→recv banner→send FC+FF→recv FS→send blobs→DISCONNECT
  - `run_irs()`: MYID→LISTEN→wait CONNECTED (with timeout)→send banner→recv FC/FF→send FS→recv N blobs→DISCONNECT
- Integration tests: `tests/driver_integration.rs` (4 tests): `iss_sends_one_message`, `irs_receives_one_message`, `iss_irs_roundtrip`, `multi_message_roundtrip`

**Phase 5.2 — LZHUF codec** ✅ Done (PR #98)
- `crates/openpulse-b2f/src/compress.rs`: real LZHUF LH5 via `oxiarc-lzhuf = "0.2.7"`
  - 4-byte BE original-length prefix makes stream self-contained (known incompatibility with external Winlink Type C — deferred). **REMOVED in PR #948**: the incompatibility was never closed, the code had no production caller, and keeping it implied a capability the crate did not have.
- `B2fSession::accepted_count()` added to `session.rs` — IRS driver uses this to know how many data frames to read
- (The `compress_lzhuf`/`decompress_lzhuf` helpers and the `lzhuf_round_trip`/`lzhuf_bad_input_error` tests were removed with the codec in PR #948.)

**Phase 5.3 — TOML configuration management** ✅ Done (PR #102)
- `crates/openpulse-config/`: new crate with typed TOML schema covering station, modem, ARDOP, KISS, logging, relay, and trust-store settings
  - `load()` reads `~/.config/openpulse/config.toml`; propagates errors so misconfiguration is visible at startup
  - `init_template()` returns a fully-commented TOML template
  - Precedence: CLI flag > config file > built-in defaults
- `openpulse-tnc` and `openpulse-kisstnc` accept clap CLI flags (`--cmd-port`, `--data-port`, `--mode`, `--bind` / `--port`) that override config file values, replacing the previous env-var-only approach
- `openpulse config init` writes the commented template to stdout; short-circuits before any hardware/network setup
- Three tests: `load_defaults_when_no_file`, `cli_override_pattern`, `missing_fields_get_defaults`

**Phase 5.4 — End-to-end loopback integration test** ✅ Done (PR #100)
- `crates/openpulse-b2f-driver/tests/e2e_loopback.rs`: full-stack gate test (no hardware required)
  - Bidirectional modem relay chains two `B2fDriver` instances through `ChannelSimHarness` (BPSK250 encode → channel → BPSK250 decode)
  - `e2e_single_message_awgn_20db`: one message through AWGN 20 dB (seed 42, deterministic)
  - `e2e_multi_message_clean`: three messages through clean channel, all bodies verified in order
  - Shared test helpers extracted to `tests/common/mod.rs` (reused by `driver_integration.rs`)
- Phase 3.5 on-air validation is now unblocked

**Phase 5.5 — Direct TCP Winlink CMS gateway** ✅ Done
- `crates/openpulse-gateway/`: new binary crate (`openpulse-gateway`)
  - Phase 1 (ISS): connects to `cms.winlink.org:8772`, reads CMS banner, sends FC+FF proposals, reads FS, sends compressed blobs
  - Phase 2 (IRS): same TCP connection, fresh `B2fSession(Irs)`, reads CMS FC+FF proposals, sends FS, reads and decompresses reply blobs
  - `DataPort` wraps `TcpStream` directly — Winlink CMS TCP uses identical u16-BE framing as B2F driver
  - CLI: `openpulse-gateway [--host] [--port] [--callsign] send --to <CALL> [--subject] [--message | stdin]`
  - Callsign read from `~/.config/openpulse/config.toml`; `--callsign` overrides; bails on default `N0CALL`
  - `gateway_round_trip` unit test: mock CMS TCP server validates full ISS+IRS exchange without network access

**Phase 5.6 — CpalBackend wiring + TOML audio config + on-air test plan** ✅ Done (PR #105)
- `crates/openpulse-config/src/lib.rs`: `AudioConfig { backend: String }` (default `"default"`); no device-name fields
- `crates/openpulse-ardop/src/main.rs` + `crates/openpulse-kiss/src/main.rs`: `--backend` CLI flag; `#[cfg(feature = "cpal")]`/`#[cfg(not(feature = "cpal"))]` match arms; `"default"` silently falls back to loopback; `"cpal"` warns when feature absent
- Build with real audio: `cargo build --release -p openpulse-kiss --features cpal` / `--features cpal` for ardop
- `docs/on-air_testplan.md`: hardware prereqs, station config template, audio path verification (Python KISS frame sender), test matrix (BPSK250 exchange, rate adaptation, Winlink CMS via RF, multi-mode ladder, ID compliance), regulatory checklist, diagnostics table

**Phase 5.7 — Testbench live audio capture** ✅ Done (PR #108)
- `apps/openpulse-testbench/Cargo.toml`: `cpal` feature gates `openpulse-audio/cpal-backend`
- `AudioSource` enum (`Synthetic` / `LiveCapture`); `AppConfig::audio_source` field
- `run_live()`: opens default system input at 8 kHz mono, captures audio into tap[2], demodulates into tap[3], synthesized TX reference in tap[0]; failure propagated to stats event log
- Source combo (cpal only) disabled while simulation is running; panel labels update to match live mode
- `JoinHandle::is_finished()` check in `update()` auto-clears `running` when thread exits early
- Build: `cargo build --release -p openpulse-testbench --features cpal`

These must be confirmed by the user before the relevant implementation starts. Do not implement speculatively.

### SAR wire format — Resolved (Option B implemented)

Option B (full SAR sub-layer) was selected and implemented in `crates/openpulse-core/src/sar.rs`.  See Group 3 task entry for details.  Phase 3.1 (PQ handshake) is unblocked once any other remaining Phase 1 gates are closed.

### `PttController` trait location (blocks Phase 1.5)

Resolved and implemented: `crates/openpulse-radio` with `NoOpPtt`, `SerialRtsDtrPtt`, `VoxPtt`, `RigctldPtt`.
