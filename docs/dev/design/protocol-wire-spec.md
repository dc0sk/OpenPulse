---
project: openpulsehf
doc: docs/dev/design/protocol-wire-spec.md
status: living
last_updated: 2026-08-23
---

# Protocol & Handshake Wire Specification

Normative byte-level specification of the OpenPulseHF **data-plane** frames and the **signed
session handshake**. It complements the two companion specs:

- [HPX Session State Machine Specification](../hpx-session-state-machine.md) — the states,
  events, transitions, timing, and security gates that *drive* these frames.
- [Peer Query and Relay Wire Schema](../peer-query-relay-wire.md) — the `OPHF` control-plane
  envelope (peer query, route discovery, relay control). Not repeated here.

The authoritative source is always the code; this document pins the layouts and links each to its
module. Capability IDs (CAP-NN) refer to [traceability-matrix.md](../project/traceability-matrix.md).

## Conventions

- **Endianness**: multi-byte integers are **big-endian** on the wire unless stated otherwise.
- **Audio context**: 8 kHz mono, centre frequency 1500 Hz (engine defaults); not part of the byte
  layout but assumed by the modulators.
- **Single-frame payload cap**: the base [frame](#1-base-frame-opls) payload is **≤ 255 bytes**.
  Anything larger (PQ handshake, signed classical handshake, multi-block transfers) is carried by
  the [SAR layer](#2-segmentation-and-reassembly-sar).
- A "frame" below is the application/protocol PDU handed to the modem; the plugin then adds its own
  preamble + pulse shaping (out of scope here — see the plugin demods).

---

## 1. Base frame (`OPLS`)

Source: `crates/openpulse-core/src/frame.rs` · CAP-65, CAP-10.

```
┌────────┬─────────┬───────────────────┬─────────────┬──────────┬───────────┐
│ magic  │ version │ sequence (u16 BE) │ length (u8) │ payload  │ CRC-16    │
│ "OPLS" │  0x01   │     2 bytes       │   1 byte    │ 0–255 B  │  2 bytes  │
│  4 B   │  1 B    │                   │             │          │  (BE)     │
└────────┴─────────┴───────────────────┴─────────────┴──────────┴───────────┘
```

| Field | Bytes | Notes |
|---|---|---|
| `magic` | 4 | ASCII `OPLS`; decode rejects any other value |
| `version` | 1 | `0x01`; decode rejects unknown versions |
| `sequence` | 2 | monotonic, wraps at 65535 |
| `length` | 1 | payload length, 0–255 |
| `payload` | 0–255 | application bytes |
| `CRC-16` | 2 | **CRC-16/CCITT**, big-endian, over `magic … payload` (everything before the CRC) |

Header is 8 bytes; min frame (empty payload) is 10 bytes; max is 265 bytes. A payload > 255 returns
`FrameError::PayloadTooLarge` — callers must SAR-fragment first.

---

## 1a. Wire whitening (scrambler)

Source: `crates/openpulse-core/src/scramble.rs` · frozen 2026-08-21 (#1148).

Everything modulated is whitened first, and this layer was **undocumented until #1148** — a
third-party implementer building from this spec would have produced a non-interoperable modem, since
nothing above hints that the bytes on the air are not the bytes in the frame.

| Property | Value |
|---|---|
| Polynomial | `x⁹ + x⁵ + 1` (ITU-T O.150 PRBS9), primitive |
| Recurrence | `s[n+9] = s[n+5] ⊕ s[n]` |
| Seed | all ones (`0x1FF`); the all-zero state is the degenerate fixed point and is unreachable |
| Period | **511 bits**, and **511 bytes** — equal because `gcd(8, 511) = 1` |
| Packing | LSB-first: bit *i* of keystream byte *k* is `s[8k + i]` |
| Application | XOR, **additive** (not self-synchronising) — so it needs frame alignment, which the preamble supplies, and it does **not** multiply errors in front of the FEC |
| Coverage | the post-FEC wire bytes; the **preamble is not whitened** |
| Inverse | self-inverse — apply the same function to undo it |

**Known-answer vector.** First 16 keystream bytes, i.e. the result of whitening 16 zero bytes:

```
FF E1 1D 9A ED 85 33 24 EA 7A D2 39 70 97 57 0A
```

An implementation that reproduces the period but not this vector is **not** interoperable: the
reciprocal trinomial `x⁹ + x⁴ + 1` is also primitive with period 511 and produces
`FF C1 FB E8 …` instead. Gated by
`openpulse_core::scramble::tests::the_keystream_is_the_frozen_prbs9_sequence`.

**History, because it bears on any capture predating this spec.** Until #1148 the implementation used
taps `(state >> 8) ^ (state >> 4)`, i.e. `s[n+9] = s[n+8] ⊕ s[n+4]` — characteristic
`x⁹+x⁸+x⁴ = x⁴(x⁵+x⁴+1)`, reducible, so the true period was **21 bits**. Recordings made before that
date carry the 21-bit keystream and do not decode against this spec.

---

## 2. Segmentation and reassembly (SAR)

Source: `crates/openpulse-core/src/sar.rs` · CAP-07. Used to carry any PDU larger than one base
frame (the signed handshakes below, multi-block objects).

```
┌───────────────────┬─────────────────────┬─────────────────────┬────────┐
│ segment_id (u16)  │ fragment_index (u8) │ fragment_total (u8) │ data   │
│      2 B (BE)     │       1 B           │       1 B           │ ≤251 B │
└───────────────────┴─────────────────────┴─────────────────────┴────────┘
```

- `SAR_HEADER_SIZE = 4`; `SAR_MAX_FRAGMENT_DATA = 251` (255 − header); `SAR_MAX_SEGMENT_DATA =
  255 × 251 = 64 005` bytes.
- `fragment_index` is 0-based; `fragment_total` is 1–255.
- Reassembly is keyed on `(session_id, segment_id)` with a timeout; duplicate fragments are
  idempotent. Each SAR fragment is itself carried in one base frame's payload.
- **Poison-resilience.** A key holds up to `MAX_CANDIDATES_PER_KEY = 8` concurrent *candidate*
  reassemblies. A fragment joins only candidates it is **consistent** with (same `fragment_total`, and
  its index empty or already holding identical bytes); an inconsistent fragment starts a new candidate
  rather than corrupting an in-flight one. This matters where the caller reuses a constant key for every
  message — the handshake path keys all frames `("handshake", 0)` — so a crafted or stray fragment (or
  two interleaved handshakes) cannot poison the reassembly: the bad candidate reassembles to a frame
  that fails signature verification and is dropped while the good one completes. `ingest` therefore
  returns **all** frames a fragment completed (usually one; more only under such a collision), and the
  candidate set is capped (oldest evicted) so a flood can't exhaust memory.

---

## 3. Signed classical handshake

Source: `crates/openpulse-core/src/handshake.rs` · CAP-01 (+ CAP-05 signing, CAP-04 trust). Driven
by the `discovery` state of the [HPX state machine](../hpx-session-state-machine.md).

Both frames share one binary container. **v2 (#1147)** — v1's JSON body is gone.

```
┌────────┬─────────┬──────────────────┬───────────────┬────────────────┐
│ magic  │ version │ length (u16 BE)  │ body          │ signature      │
│ 4 B    │  0x01   │    2 bytes       │ `length` B    │ 64 B (Ed25519) │
└────────┴─────────┴──────────────────┴───────────────┴────────────────┘
   CONREQ magic = "HSCQ"      CONACK magic = "HSAK"
```

**The signature covers the transmitted prefix** — `magic || version || length || body` — i.e. exactly
the bytes on the air minus the trailing signature. There is no second representation, so verification
is "check what you received" and cannot drift from the encoder. It also binds the **version**, which
v1 did not.

**Both frames fit ONE SAR fragment by construction.** The maximal legal CONREQ is **236 B** and CONACK
**237 B**, against the 251 B a fragment holds — a property of the worst case at every cap, not of a
typical frame. v1 was ~752 B on the wire = 3 fragments = three preambles and three acquisitions,
decoding at roughly p³ on a fading channel.

**A frame whose version byte is not `WIRE_VERSION` is rejected outright.** There is no dual decode:
there is no compatibility mode for the data plane, and inventing one for the handshake alone would
carry a second wire format in production source.

**Corrected 2026-09-12** — this said "version 0x01 is rejected outright", and the container diagram
above said `0x02`, both contradicting the freeze in §3.2a. The design specified a bump to `0x02`,
#1189 shipped it, and #1204 reset the byte to `0x01` and froze it until 1.0; the normative rule is
and always was "not `WIRE_VERSION` → reject".

### 3.0a Decoder caps

Caps are what make "one fragment" a property of every legal frame rather than of an example, and they
are enforced at **both** ends — a station cannot emit a frame its peer is obliged to reject, so the
failure surfaces at the sender where it is actionable.

| field | cap (bytes) |
|---|---|
| `station_id`, `dst_station` | 18 |
| `station_grid` | 8 |
| `profile_name` | 24 |
| `signing_modes` | 4 entries |

Fixed-size fields (`pubkey` 32, `kex_pubkey` 32, `conreq_hash` 32) carry **no length prefix** — a
length prefix on a constant-size field hands a parser decision to the sender — and a wrong length is a
decode error, not a late verification failure.

### 3.1 CONREQ body (`HSCQ`)

Fields in wire order. Strings are `u8`-length-prefixed and capped; integers are big-endian.

| Field | Type | Meaning |
|---|---|---|
| `station_id` | string (≤18) | initiator callsign |
| `dst_station` | string (≤18) | addressee; `"*"` = broadcast. **Empty is invalid** |
| `pubkey` | bytes (32) | Ed25519 verifying key |
| `kex_pubkey` | bytes (32) | ephemeral X25519 key for OTA-ACK key agreement (E7) |
| `signing_modes` | u8 count + u8 each | modes offered, in preference order |
| `session_id` | u64 | session identifier — fixed-width, not a string (F1) |
| `station_grid` | string (≤8) | Maidenhead grid; empty = not advertised |
| `profile_name` | string (≤24) | active OTA ladder name; empty = none |
| `profile_fingerprint` | u64 | fingerprint of the ladder mapping; 0 = none |
| `timestamp_ms` | u64 | signed creation time. **Mandatory** — no sentinel |

`dst_station` exists because a v1 CONREQ had no destination, so **every daemon in range answered one**
and spent RF before the initiator filtered. "Unaddressed" must not be spellable by omission, which is
why empty is refused at both ends rather than treated as a wildcard.

### 3.2 CONACK body (`HSAK`)

| Field | Type | Meaning |
|---|---|---|
| `station_id` | string (≤18) | responder callsign |
| `pubkey` | bytes (32) | Ed25519 verifying key |
| `kex_pubkey` | bytes (32) | ephemeral X25519 key |
| `selected_mode` | u8 | chosen from the modes the CONREQ offered |
| `conreq_hash` | bytes (32) | SHA-256 over the **complete transmitted CONREQ**, signature included |
| `station_grid` | string (≤8) | responder grid; empty = not advertised |
| `profile_name` | string (≤24) | responder's ladder name; empty = none |
| `profile_fingerprint` | u64 | fingerprint; 0 = none |
| `timestamp_ms` | u64 | signed creation time. **Mandatory** |

**The CONACK carries no `session_id`.** Echoing it costs 25 B and pushes the maximal frame to 269 B,
past what one fragment holds — and `conreq_hash` subsumes the echo while binding harder: the session
id is cleartext and time-based, hence guessable inside the handshake window, whereas the hash covers
the whole frame including the initiator's `kex_pubkey`. Both endpoints already hold the id for
`derive_session_keys`.

### 3.2a OTA-ACK key agreement (E7)

Both frames carry an ephemeral **X25519** `kex_pubkey` inside the signed body. When both peers
advertise one, each derives a shared 32-byte key via ECDH → HKDF-SHA256
(`session_key::derive_ack_key`). Because the ephemeral keys are covered by the identity signature, a
MITM cannot substitute them. The key authenticates the tiny FSK4 **rate ACK**: the 5-byte ACK's
`session_hash` (2 B) + CRC (1 B) fields are replaced by a **24-bit keyed HMAC-SHA256 tag** over the ACK
content (`AckFrame::encode_authenticated`) — so the frame stays exactly 5 bytes (no waveform/airtime
change) but a listener who read the cleartext `session_id` can no longer forge rate-control ACKs. The
tag also serves anti-collision (a co-channel session has a different key). This is **authentication, not
encryption** — the ACK content stays in the clear — so it is compatible with amateur-radio rules that
forbid obscuring meaning (see `docs/regulatory.md`). A residual: a *replayed* valid ACK carries stale
but valid content within the session; the rate ladder is receiver-led and absolute, bounding the effect.

#### `station_id` cap, and why the CONACK has no `dst_station`

`caps::STATION_ID` is **18 bytes**, a POLICY number over an unbounded generator — ITU RR Article 19
bounds an ordinary amateur callsign at roughly seven characters but lets administrations authorise
longer special-event calls, and compound decoration stacks on top. 18 covers `SV5/DL1ABCD/P` (13),
`3DA0/DL1ABCD/QRP` (16) and `3DA0/VI110ACT/QRP` (17). Longer forms are refused **loudly**, at config
load and at the command boundary (#1199), never by silently downgrading the session.

**The CONACK carries no `dst_station`** (#1191). A CONACK has exactly one consumer and it
self-selects by `conreq_hash` — a SHA-256 over the whole transmitted CONREQ, which is a strictly
stronger filter than a callsign echo: it rejects a replayed CONACK between the *same two stations*,
which a callsign cannot. #1178's spent-RF argument for addressing the CONREQ does not transfer,
because nobody transmits in response to a CONACK. The same field is absent from the PQ CONACK for
the same reason.

**`WIRE_VERSION` is `0x01` and FROZEN until 1.0.** The format changes freely in the pre-1.0 window
and nothing is deployed outside the project's own test rigs, so bumping the byte per change would be
ceremony. The cost, stated so it is not a surprise: two builds from different points in this window
fail with a garbled decode rather than a clean version rejection. The mitigation is procedural —
rebuild both ends in lockstep before any on-air session. At 1.0 the byte starts moving.

### 3.3 Signing and verification

- The signature covers the **transmitted prefix** (`magic || version || length || body`). There is no
  canonicalisation step, because there is nothing to canonicalise: the signed bytes are the sent bytes.
  **Corrected in v2** — this section previously said the signature covered "canonical JSON … with keys
  sorted recursively". That was never true of the code, which used `serde_json::to_vec` in serde
  declaration order with no sorting. "Canonical" was a label, not a property.
- **Domain separation is by registered tag, and these frames carry theirs in-band.** Every context
  the station key signs is registered in `openpulse_core::signing_domain::SigningDomain`, and every
  signed message begins with that context's unique four-byte tag. For the handshake frames the tag
  *is* the transmitted magic (`HSCQ`/`HSAK`/`HPCQ`/`HPAK`), which is already inside the signed span —
  so **nothing is prepended and these frames are byte-identical to v2 as first shipped**, which the
  known-answer vector in §3.3a confirms. Contexts whose signed bytes have no fixed start (manifests,
  peer descriptors, route responses/updates, file offers) get `tag || version` prefixed to the
  *signed message only*; it is never transmitted, so it costs no airtime.
  **Corrected (#1193)** — this section previously said "no domain-separation tags", arguing that a
  tag would separate contexts "only by whichever byte differs first, which is what tags exist to
  prevent". That reasoning is muddled: *all* prefix separation is byte-differs-first. What a tag
  actually provides is **guaranteed** distinctness, where a magic provides it only if the full set is
  inventoried and checked — which nothing did. The registry is that inventory, and
  `every_tag_and_reserved_magic_is_pairwise_distinct` is that check.
- **The registry is enforced, not documented.** A `clippy.toml` `disallowed-methods` wall refuses raw
  `ed25519_dalek` sign/verify outside `openpulse_core::signing`, because one unregistered signing site
  voids separation for every context rather than only its own.
- Verification takes **bytes**: split → check the signature over the prefix → replay-freshness →
  trust evaluation. A CONACK additionally requires `conreq_hash` to match the CONREQ that was sent,
  and `selected_mode` to be one the CONREQ **offered** — the latter is new in v2 (v1 evaluated it
  against local policy only, though this document previously claimed otherwise).
- **Replay-freshness.** `timestamp_ms` is inside the signed prefix and mandatory. A verifier passing
  `Freshness { now_ms, max_skew_ms }` rejects a frame outside `±max_skew_ms` (the daemon uses ±120 s),
  bounding the capture-replay window. Zero is refused as `MissingTimestamp`. The check runs *after*
  signature verification, so an attacker cannot refresh a captured frame.

### 3.3a Known-answer vector

An independent implementer can check their encoder against this without building the crate. It is the
frame **pre-SAR and pre-whitening** — deliberately, so a change to the wire scrambler (#1148) cannot
invalidate a vector that has nothing to do with it.

Inputs, all fixed:

| field | value |
|---|---|
| signing seed | 32 × `0x01` |
| `station_id` / `dst_station` | `W1AW` / `K2XYZ` |
| `signing_modes` | `[Normal, Psk]` = `0x01, 0x02` |
| `session_id` | `W1AW-1700000000000` |
| `station_grid` / `profile_name` | `FN31pr` / `hpx_hf` |
| `profile_fingerprint` | `0x0123456789ABCDEF` |
| `timestamp_ms` | `1700000000000` |
| `kex_pubkey` | 32 × `0x42` |

Output — **187 bytes** (7 header + 116 body + 64 signature).
Taken verbatim from `CONREQ_KAT` in `crates/openpulse-core/tests/handshake_kat.rs`; that
test fails if the encoder stops producing it, so this copy cannot drift silently — it had
drifted before #1191 and disagreed with the code in both length and content.

```
485343510100740457314157054b3258595a8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94
121bf3748801b40f6f5c424242424242424242424242424242424242424242424242424242424242
42420201020000018bcfe5680006464e33317072066870785f68660123456789abcdef0000018bcf
e568003a2570b187d50bfd206bd9560e8cd85cae60496ad4765ea2577242a4f6b9e0d68c29b95f61
566d07849a35738e810c85050d877b1977bbb6002a8b7f552fb20c
```

(Line breaks are for reading only; the canonical form is the unbroken hex in
`crates/openpulse-core/tests/handshake_kat.rs`, which is what the gate compares against.)

**The signature pins too**, because Ed25519 signing is deterministic (RFC 8032) — no per-signature
randomness, so the same key over the same message yields the same 64 bytes everywhere. The test
asserts that property rather than only citing it.

**Why this exists alongside the round-trip tests.** A round-trip proves the encoder and decoder agree
with *each other*, so it passes just as happily when both drift together — which is exactly what a
format change does. Verified by sabotage: swapping two body fields in **both** the encoder and the
decoder leaves all 16 round-trip and tamper tests passing, and fails only here.

**PQ**: the PQ CONREQ is pinned by digest (SHA-256 `015a1aa8…`, 5060 B) rather than inline hex.
ML-DSA-44 signing in this build was **measured** deterministic, so the whole frame pins. The PQ
**CONACK** has no vector and cannot have one: its `kem_ciphertext` comes from `encapsulate()`, which
is randomised by design.

### 3.4 Daemon RF exchange

The daemon (CAP-63) drives this over RF on `ConnectPeer`: initiator sends CONREQ (SAR), responder
verifies and replies CONACK (SAR), initiator verifies. The verified peer (callsign + grid) is stored
and emitted as `ControlEvent::PeerVerified`; an unanswered handshake times out after 30 s. The
station key is the Ed25519 seed at `[station] identity_key_path`.

---

## 4. Post-quantum handshake

Source: `crates/openpulse-core/src/pq_handshake.rs` · CAP-02. `PqConReq` / `PqConAck` carry classical
+ PQ public keys, the KEM material, and dual signatures.

**v2 (#1147)**: the same binary container and the same signing rule as §3 — magics `HPCQ` / `HPAK`,
signature over the transmitted prefix. Previously these were bare JSON with **no magic at all**, and
because the daemon routes by magic sniff (`HSCQ`/`HSAK`), a PQ frame would have been silently dropped
even if something had sent one.

They also gained what the classical path already had and these did not:

- **`timestamp_ms`, mandatory.** The PQ bodies carried no timestamp, so the PQ path had **no replay
  freshness at all**. `verify_pq_conreq`/`verify_pq_conack` now take a `Freshness` parameter.
- **`dst_station`** (#1178) and **`conreq_hash`** transcript binding, replacing the session-id echo.

**The classical-signature presence flag lives inside the signed body.** These frames carry two
signatures and the Ed25519 one is optional (0 or 64 B), so the trailing region is not a constant.
Outside the signed span, flipping one bit would change how many trailing bytes a verifier reads as
the classical signature — and that bit would itself be unsigned. The decoder cross-checks the actual
trailer length against the signed flag, so a truncated or padded frame cannot be re-split.

**These frames do NOT fit one fragment, and are not expected to.** A PQ CONREQ is ~5 kB ≈ 2.7 min at
BPSK250 — not deployable on this link. PQ is in the v2 format so that the format is **finished**, and
wiring PQ later is not a second wire break; it is not a shipping feature, and the test suite asserts
the multi-fragment property rather than letting a green run imply otherwise.

Component sizes:

| Component | Bytes | Used in |
|---|---|---|
| ML-DSA-44 public key | 1312 | both frames |
| ML-DSA-44 signature | 2420 | both frames (PQ signature) |
| ML-KEM-768 encapsulation key | 1184 | `PqConReq` |
| ML-KEM-768 decapsulation seed (`d‖z`) | 64 | local only (not on wire) |
| ML-KEM-768 ciphertext | 1088 | `PqConAck` |
| ML-KEM-768 shared secret | 32 | derived (`kem_decapsulate`) |

- `SigningMode::Hybrid` signs with **both** Ed25519 and ML-DSA-44 (defense in depth); `SigningMode::Pq`
  leaves the classical signature empty.
- `PqConAck.kem_ciphertext` lets the initiator recover the 32-byte shared secret.

---

## 5. ACK frame (FSK4)

Source: `crates/openpulse-core/src/ack.rs` · driven by the rate/ACK taxonomy (CAP-32). A fixed
**5-byte** frame, sent on the `FSK4-ACK` waveform (20 symbols @ 100 baud ≈ 200 ms on air).

```
byte 0:  bits[2:0] ACK type │ bit[3] has_reverse_ack │ bit[4] has_recommended_level │ bits[7:5] reserved (MUST be 0; rejected)
bytes 1–2: session_hash (u16 BE)  — 16-bit FNV-1a of session_id (anti-collision)
byte 3:  bits[2:0] reverse_ack (iff bit[3] of byte 0) │ bits[7:3] recommended_level (iff bit[4])
byte 4:  CRC-8/SMBUS over bytes 0–3
```

ACK types (byte 0 bits[2:0]):

| Code | Type | Meaning |
|---|---|---|
| `0b000` | AckOk | decoded OK — hold speed |
| `0b001` | AckUp | decoded OK, high margin — step up |
| `0b010` | AckDown | marginal — step down |
| `0b011` | Nack | uncorrectable — retransmit |
| `0b100` | Break | request direction changeover |
| `0b101` | Req | repeat last frame |
| `0b110` | Qrt | graceful end |
| `0b111` | Abort | abnormal teardown |

`session_hash` lets a receiver filter ACKs not addressed to its session. Byte 3 carries the optional
`reverse_ack` (peer's RX-direction quality, for bidirectional sessions) and `recommended_level`
(receiver-led OTA rate control, CAP-33) — both gated by the byte-0 flag bits and ignored by older
receivers while the CRC still validates.

### 5.1 The two unused-bit regions are governed differently — deliberately

**Byte 0 bits[7:5] are reserved and enforced.** Since #1165 both decoders **reject** a frame with any
of them set, in `decode` and `decode_authenticated` alike (in the authenticated path the check runs
*after* the MAC: authenticity first, then format). Both encoders have always written zero there, so
the tightening was not a wire break. This is the frame's version/capability headroom, and **any
future extension announces itself here** — enforcement is what lets an old receiver fail closed on a
format it does not understand instead of silently misreading it.

**Byte 3's bits are NOT reserved, and are deliberately NOT enforced** (#1211, ruled 2026-08-28).
When a presence flag is clear its field is unused, encoders write zero, and decoders **ignore
whatever arrives**. Contract: *must be zero on transmit, ignored on receive.* Three reasons the
symmetric tightening was declined:

1. **Byte 3 is payload, not a reserved region.** It is *fully allocated* when both flags are set
   (3 + 5 = 8 bits), so it is not spare capacity — a future field that must coexist with both
   existing options gets nothing from it. This is a statement about the format's capacity, not
   about current traffic: production only ever emits `new(...).with_recommended_level(...)`, so
   bits[2:0] are free in every ACK shipped today, but a peer may legally set both and the codec
   must accept it.
2. **Every channel on which an extension could announce itself is already fail-closed — byte 3
   with its flags clear is the only one that is not.** That, not "byte 0 is the only place", is the
   real argument, and it is broader:
   - byte 0 bits[7:5] — rejected since #1165;
   - the ACK type, bits[2:0] — **all eight codes are assigned** (`AckOk`…`Abort`), so there is no
     spare code point to grow into;
   - `recommended_level`, when its flag is set — the 5-bit field spans 0–31 but `SpeedLevel` is
     1–20, so codes 0 and 21–31 are **rejected** by both decoders (`InvalidSpeedLevel`);
   - the frame length — an extended ACK is not 5 bytes and is rejected before parsing.

   So an extension can only hide in byte 3 *with the flags clear*, and enforcing byte 3 would add
   detection for exactly that one design — which nobody should choose, precisely because it is the
   only undetectable one.

**Not a reason, recorded so it is not re-argued:** the tightening was initially declined partly on
CRC robustness — that corruption landing in byte 3's ignored bits decodes correctly today and would
become a lost ACK. That argument is backwards and was cut. **No corruption confined to byte 3 alone
can pass the CRC**: an error polynomial of degree ≤ 7 cannot be a multiple of the degree-8
generator, verified by enumerating all 256 byte-3 values over 200 random prefixes with a
deliberately-weakened 4-bit CRC as the control. A CRC-passing corruption that reaches byte 3 must
also corrupt the CRC byte in a matching pair, and almost all such frames are corrupt in bytes 0–2
as well — i.e. they decode to the *wrong* ACK today. Enforcement would mostly reject frames that
are already semantically wrong, which is a benefit, not a cost. The ruling stands on 1–2 alone.

**Falsifier, and the standing price.** An extension genuinely mutually exclusive with *both*
`reverse_ack` and `recommended_level` — an extended reason code on a `Nack` is the plausible
candidate — would want those bits and reopens this. Note what "ignored on receive" costs until
then: because a third-party transmitter writing junk there is undetectable forever, **these bits can
never be reclaimed without spending one of byte 0's seven remaining non-zero patterns**. Byte-0
headroom is scarce, and that scarcity is exactly what would tempt a future designer toward byte 3.

---

## 6. Transfer manifest

Source: `crates/openpulse-core/src/manifest.rs` · CAP-03. A `TransferManifest` carries a SHA-256
payload hash, sender id, and an Ed25519 signature, verified before final acceptance of an object
transfer (the `active_transfer` → completion gate).

---

## 7. Negotiated parameters

### 7.1 Compression (`CompressionAlgorithm`, CAP-08)

| Variant | On-wire framing |
|---|---|
| `None` | payload as-is |
| `Lz4` | LZ4 block + 4-byte **little-endian** decompressed-size prefix |
| `Zstd(dict_id: u32)` | Zstd with the shared HPX dictionary; `dict_id` catches version skew |

Configured locally, NOT negotiated in the handshake (**Removed in #1166** (the #1147 wire-format break): nothing consumed the selection — the daemon sent the lists empty and hardcoded `None`/`None` — so the field was a capability claim the station could not back. Session compression itself is unchanged; only the *handshake negotiation of it* is gone.). A compressed frame
larger than the original is sent uncompressed (`compress_if_smaller`).

### 7.2 FEC modes (`FecMode`, CAP-26 and the soft-FEC caps)

`None`, `Rs`, `RsInterleaved`, `Concatenated`, `ShortRs` (ACK-sized), `RsStrong`, `SoftConcatenated`,
`Ldpc`, `LdpcHighRate`, `Turbo`. Configured locally, NOT negotiated in the handshake (see §3 and #1166). RS modes
bundle a block interleaver into the codec path so every FEC-protected frame is de-bursted by
construction. (Padded OFDM/SC-FDMA modes don't round-trip the hard 255-byte-block RS framing — see
the testmatrix note in the traceability matrix.)

### 7.3 Trust & signing modes (`trust.rs`, CAP-04)

Signing modes `Normal` / `Psk` / `Pq` / `Hybrid` (increasing strength; PQ=4, Hybrid=5). Trust levels
`Verified` / `PskVerified` / `Unknown` / `Reduced` / `Revoked`; policy profiles `Strict` / `Balanced`
/ `Permissive` set the minimum acceptable trust. See the HPX spec's *Security Gates* section.

---

## 8. Direct file transfer (`OPFX`)

Source: `crates/openpulse-filexfer` · design `docs/dev/design/file-transfer-plan.md` (FF-16). A
self-describing binary protocol for offering, transferring, and cryptographically verifying a file
over an RF session. Registered here to satisfy the "determinable emissions" openness requirement.

Every `FxFrame` is **SAR-encoded** before transmission (like handshake frames), so after reassembly a
frame is:

```
OPFX (4) │ ver (1) = 0x01 │ type (1) │ body…
```

`compression::unpack()` passes an `OPFX` frame through untouched (its magic check fails), so the magic
is safe alongside `OPLS`/`OPHF`/`OPZ1`/`HSCQ`/`HSAK`/`QSY`. Frame types:

| type | name | body |
|---|---|---|
| 0x01 | `FileOffer` | `transfer_id u32 \| flags u8 \| file_size u64 \| sha256 [32] \| block_size u32 \| block_count u16 \| sender_id str≤16 \| name str≤48 \| mime str≤24 \| signature [64]` |
| 0x02 | `FileAccept` | `transfer_id u32 \| have_len u16 \| have_bitmap [have_len]` (resume bitmap; empty in v1) |
| 0x03 | `FileReject` | `transfer_id u32 \| reason u8` |
| 0x04 | `FileData` | `transfer_id u32 \| block_index u16 \| packed block bytes…` (one SAR segment, `segment_id = block_index + 1`) |
| 0x05 | `BlockAck` | `transfer_id u32 \| block_index u16 \| complete u8 \| missing_len u8 \| missing_frag_bitmap [missing_len]` |
| 0x06 | `FileComplete` | `transfer_id u32 \| status u8 \| countersignature [64]` |
| 0x07 | `FileCancel` | `transfer_id u32 \| reason u8` |

Strings are `len(u8) \| UTF-8`; integers big-endian. `block_size` is bounded `1024..=49 152` so a
per-block `pack()` (§7.1) + the 12-byte `FileData` header never exceeds the 64 005-byte SAR-segment /
`MAX_DECOMPRESSED_SIZE` cap — this is how a file larger than one SAR object is carried (the **block**
is the multi-object unit; segment-id 0 stays reserved for handshake frames). `reason` codes: `0`
operator-declined, `1` feature-disabled, `2` too-large, `3` quota-exceeded, `4` busy, `5`
untrusted-peer, `6` timeout, `7` unsupported-version, `8` operator-cancel, `9` stall.
`FileComplete.status`: `0` verified-ok, `1` hash-mismatch, `2` signature-invalid, `3` size-mismatch.

**Integrity** reuses §6: `FileOffer` embeds the four `TransferManifest` fields inline; the receiver
reconstructs the manifest and calls `verify_manifest` at offer time (against the handshake-proven peer
key) and `verify_manifest_with_payload` after reassembly, then countersigns `FileComplete` on success.

---

## Cross-references

| Layer | Spec / source |
|---|---|
| Session lifecycle (states, transitions, timing) | [hpx-session-state-machine.md](../hpx-session-state-machine.md) |
| Peer query / route discovery / relay control (`OPHF`) | [peer-query-relay-wire.md](../peer-query-relay-wire.md) |
| Base frame / SAR / handshake / ACK / manifest byte layouts | this document + the cited `crates/openpulse-core/src/*.rs` |
| Capability → implementation → tests | [traceability-matrix.md](../project/traceability-matrix.md) |
