---
project: openpulsehf
doc: docs/dev/design/js8-discovery-rendezvous-plan.md
status: approved-plan (decisions D1–D7 locked 2026-07-10; Phases A–G shipped — full RX + beacon TX + rendezvous → HPX handoff; only Phase H on-air validation remains)
last_updated: 2026-07-11
---

# JS8-Based Station Discovery and Rendezvous — Design Plan

This document is the engineering plan for a JS8-based **station discovery and rendezvous** subsystem: when an OpenPulse station is idle it QSYs to the band's conventional JS8 calling frequency, participates as a *real, well-behaved JS8 station* (heartbeats, directed queries, station info), marks itself with an in-band OpenPulse capability hint, caches other OpenPulse-marked stations, and — on user request — negotiates a working frequency over JS8, QSYs there, and hands off to a native OpenPulse HPX session.

This is a **plan only**. Nothing here is implemented. Every claim about the existing codebase cites the actual file; every claim about JS8 was verified against the JS8Call-improved source (`JS8_Mode/JS8Submode.cpp`, `JS8_Mode/JS8.cpp`, `JS8_Include/commons.h`, `JS8_Main/Varicode.h`, commit-current `master`) and the official user guide.

---

## 1. Summary, goals, non-goals

### Goals

1. **G1 — JS8 interop**: implement enough of the real JS8 protocol (NORMAL submode first) that OpenPulse decodes and is decoded by stock JS8Call/JS8Call-improved stations on the air. No "JS8-like" dialect: byte-identical frames, tone sequences, and message grammar.
2. **G2 — passive discovery**: while idle, listen on the JS8 calling frequency, decode all stations in the passband, and cache them (callsign, grid, SNR, last-heard).
3. **G3 — OpenPulse hint**: advertise OpenPulse capability inside otherwise-valid JS8 traffic; detect the hint in received traffic and mark those stations.
4. **G4 — active discovery**: send JS8 heartbeats and (rate-limited) `INFO?`/`GRID?` queries.
5. **G5 — rendezvous**: negotiate a working frequency with a discovered OpenPulse peer over JS8, QSY both stations, and hand off to the existing signed HPX handshake (`begin_secure_session` / CONREQ–CONACK).
6. **G6 — operator surface**: panel tab with enable/disable, live discovered-station list, per-station rendezvous action, and status; `[discovery]` config section, off by default.
7. **G7 — plugin packaging**: the JS8 *waveform* is a first-class `ModulationPlugin` (`plugins/js8`), registered like every other plugin; the protocol/scheduler layers are library crates driven by the daemon (see §4.1 for why, and why that is the only honest reading of "as a plugin" in this codebase).

### Non-goals

- Full JS8Call feature parity (inbox/MSG store-and-forward, relays `>`
  , APRS gateway, PSK Reporter spotting, groups management UI). We implement the *grammar* so we can parse it, but only generate the subset we need.
- SLOW/FAST/TURBO/ULTRA submodes in the MVP (NORMAL only; the submode table is designed in from day one, §2.2).
- Replacing the existing `openpulse-qsy` in-session QSY protocol. Rendezvous is a *pre-session* frequency agreement carried over JS8; the existing QSY protocol remains the in-session mechanism (§5.3).
- General keyboard-to-keyboard JS8 chat UI. (The decode log makes traffic visible; composing arbitrary JS8 text is a possible later add-on.)
- On-air automatic operation defaults. Everything TX-capable ships **off**; RX-only discovery is the default enabled mode when the feature is turned on (§8, §9).

---

## 2. JS8 protocol primer — what "JS8-compatible" actually requires

JS8 is FT8's physical layer (K1JT's 8-GFSK + LDPC design) with a different message layer on top. Being compatible means implementing an FT8-class weak-signal modem. That is the single largest work item in this plan and it is called out honestly below.

### 2.1 Physical layer (verified from source)

Every JS8 transmission, in every submode, is **79 symbols**: 3 × 7 Costas sync symbols (positions 0–6, 36–42, 72–78) + 58 data symbols. Each data symbol carries 3 bits (one of 8 tones), 58 × 3 = 174 bits = one LDPC codeword.

- **FEC**: LDPC(174,87) — from `JS8_Mode/JS8.cpp`: `constexpr int N = 174; constexpr int K = 87;` with the comment "Information bits (75 + CRC12)". So: 75 message bits + 12-bit CRC = 87 info bits, coded to 174 bits. This is the *old* FT8 v1 code (WSJT-X ≤ 1.9), not today's FT8 (174,91)/CRC-14 — JS8 froze on the v1 protocol. The parity/generator tables must be ported from JS8Call-improved (GPL-3.0, same license as this repo — compatible).
- **Tone mapping**: 3-bit groups → Gray-coded 8-FSK tones; **GFSK** pulse shaping (Gaussian frequency smoothing, FT8-style BT≈2.0) with continuous phase. A rectangular-FSK approximation (what `plugins/fsk4` does) will decode locally but splatters adjacent JS8 users on the air — GFSK is required for G1.
- **Costas arrays** (from `JS8_Mode/JS8.h`, verbatim):
  - `ORIGINAL` (NORMAL only, FT8-legacy): `{4,2,5,6,1,3,0}` repeated for all 3 blocks.
  - `MODIFIED` (all other submodes): block 1 `{0,6,2,3,5,4,1}`, block 2 `{1,5,0,2,3,6,4}`, block 3 `{2,5,0,6,4,1,3}`. The per-block-unique arrays prevent false sync at ±36-symbol offsets.

### 2.2 Submodes (verified from `commons.h` + `JS8Submode.cpp`)

JS8Call runs at 12 000 Hz audio. OpenPulse runs at 8 000 Hz (`AudioConfig::default()`, `crates/openpulse-core/src/audio.rs:36`). Tone spacing and baud are sample-rate-independent protocol facts; samples/symbol below are recomputed for 8 kHz — **all are exact integers**, so no resampling is needed anywhere:

| Submode | Proposed mode string | Baud = spacing (Hz) | BW (Hz) | samples/sym @12k → @8k | TX duration (s) | Start delay (ms) | Slot (s) | Costas | Decode floor (dB, source) |
|---|---|---|---|---|---|---|---|---|---|
| SLOW | `JS8-SLOW` | 3.125 | 25 | 3840 → 2560 | 25.28 | 500 | 30 | MODIFIED | −28 |
| NORMAL | `JS8-NORMAL` | 6.25 | 50 | 1920 → **1280** | 12.64 | 500 | 15 | ORIGINAL | −24 |
| FAST | `JS8-FAST` | 10 | 80 | 1200 → 800 | 7.90 | 200 | 10 | MODIFIED | −22 |
| TURBO ("JS8 40") | `JS8-TURBO` | 20 | 160 | 600 → 400 | 3.95 | 100 | 6 | MODIFIED | −20 |
| ULTRA ("JS8 60") | `JS8-ULTRA` | 31.25 | 250 | 384 → 256 | ~2.5 | 100 | 4 | MODIFIED | −18 |

BW = 8 × tone spacing. A NORMAL transmission is 79 × 1280 = 101 120 samples (12.64 s) starting 0.5 s into a 15 s wall-clock slot. Heartbeats are conventionally sent at a random audio offset in the **500–1000 Hz "HB sub-band"**; general traffic is anywhere in the ~300–2500 Hz passband. ULTRA exists only in the improved fork; heartbeats are disabled in TURBO/ULTRA by convention.

**MVP = NORMAL only.** It is the interop baseline (every JS8Call station decodes NORMAL), uses the historical FT8 Costas array, and its 15 s slot is the conventional calling-frequency rhythm.

### 2.3 T/R timing — the clock problem

Slots are aligned to wall-clock UTC (`:00/:15/:30/:45` for NORMAL). JS8Call requires the station clock within **±2 s** of UTC. Consequences for us:

- We need a UTC-disciplined system clock (NTP is sufficient; no PPS needed). The decoder's time-offset (`dt`) search of roughly ±2.5 s absorbs residual error.
- JS8Call's `DriftingDateTime` measures the median `dt` of decoded signals and can nudge its notion of slot phase. We should do the same: a `Js8Clock` that reports slot index/phase from `SystemTime` plus a drift bias estimated from decode `dt` medians, and a warning event when |bias| exceeds a threshold. Note the existing daemon patterns are `Instant`-based (`StationIdTimer`); this scheduler is necessarily `SystemTime`-based.
- TX must start at slot start + start-delay (500 ms) with ≲100 ms tolerance; PTT assert latency therefore has to be measured/absorbed (assert at T−200 ms; the existing PTT wrap idiom in `server.rs:822–855` is synchronous and fine).

### 2.4 Message layer (verified from `Varicode.h` + user guide)

The 75 message bits are: **3-bit transmission flags** + **72-bit payload** (`pack72bits(quint64 value, quint8 rem)`).

- `TransmissionType` (3 flag bits): `000` continuation frame, `001` first frame of a message, `010` last frame, `1XX` data frame (flags double as 2 payload bits — see `Varicode.h:33–46`).
- `FrameType` (leading 3 bits of the payload for non-data frames): `000` **Heartbeat**, `001` Compound-callsign partial, `010` Compound directed, `011` **Directed**, `10X` Data (Huffman), `11X` Data (dictionary/compressed).
- **Packing primitives** to reimplement bit-exactly: `packCallsign` (28-bit standard callsign, EME-2000 scheme), `packGrid` (15-bit Maidenhead), `packAlphaNumeric50` (compound callsigns), `packAlphaNumeric22` (prefix/suffix), `packCmd` (5/6-bit directed-command code + optional packed number, e.g. SNR).
- **Heartbeat frame** = single frame: callsign + grid + status bits. Rendered as `KN4CRD: @HB HEARTBEAT EM73`. **There is no free-text room in an HB frame** — this constrains the hint design (§3).
- **Directed frame** = single frame: from-callsign + to-callsign-or-group + command code (+ optional number). The command table includes: `SNR?`, `GRID?`, `INFO?`, `STATUS?`, `HEARING?`, `QUERY CALL?`, `QUERY MSGS`, `AGN?`, `MSG`, `ACK`, `SNR <n>`, short codes (`QSL?`, `RR`, `73`, …).
- **Group addressing**: `@ALLCALL`, `@HB`, and custom groups (`@` + up to 8 chars from `[A-Z0-9/]`, packed with the callsign packer). Groups are how we get a clean OpenPulse-only address (`@OPULSE`, §3).
- **Free text** rides in Data frames: **varicode** — a modified Huffman code over the JS8 character set (space/`E` ≈ 2.5 bits … rare chars ≈ 14 bits; ~13 chars per frame on average) plus **JSC** (`JS8_JSC/`), an (s,c)-dense word-dictionary compressor (~260 k entries). Multi-frame messages are chained with the first/last flag bits.

### 2.5 The hard parts, honestly

1. **The decoder is an FT8-class weak-signal receiver.** Sync = 2-D search (frequency bins × time lags) correlating against the Costas pattern over the whole passband, producing many candidates; per candidate: downmix/downsample, fine time-and-frequency sync, per-symbol 8-point spectral estimation, soft LLRs from tone energies, LDPC belief-propagation (with retries/depth), CRC-12 check, then dedup. JS8Call decodes *dozens of overlapping stations per window at −24 dB*. This is several weeks of focused DSP work even with the algorithms in front of us, and it is the schedule-critical path — hence Phase A/B first and an explicit fallback decision (§12, D1). Our engine's acquisition machinery (`EnergyGate`, `refine_onset`, `afc_mini_settle` in `crates/openpulse-modem/src/engine.rs`) does **not** apply: those assume one burst, one signal, energy above the noise floor. The JS8 decoder is self-contained inside the plugin and receives whole 15 s windows (§6.2).
2. **LLR quality gates everything.** The repo has hard-won LLR-calibration discipline (CLAUDE.md "Test what an LLR *means*"; `plugins/scfdma/tests/llr_reliability.rs`). The JS8 soft demod must follow the same contract from day one or the LDPC decoder will silently underperform.
3. **Multi-decode changes the RX model.** One window → N decodes at N audio offsets. The `ModulationPlugin::demodulate` contract (one `Vec<u8>` out) cannot express this; §4.2 defines the crate-level API that can, while keeping the trait impl honest for loopback/engine use.
4. **Wall-clock scheduling is new.** Nothing in the daemon is UTC-slot-aligned today; the closest pattern (`StationIdTimer`, `crates/openpulse-core/src/station_id.rs`) is interval-based. New but small (§6.3).
5. **CPU on Raspberry Pi.** A full-passband candidate search every 15 s must fit the Pi budget alongside the modem. JS8Call-improved dropped Fortran and decodes fixed-depth-2 on desktop CPUs; we should budget ~1–2 s of one core per NORMAL window and verify early (Phase B acceptance includes a Pi-class timing gate via `cross`).

---

## 3. The OpenPulse hint

### 3.1 Constraints

- A heartbeat frame has **no free-text field** (§2.4) — the hint cannot ride inside the HB itself.
- Free text costs ~13 varicode chars per 15 s frame — the hint must be ≲ 1–2 frames.
- It must read as unremarkable, valid JS8 to human operators and stock JS8Call software (displayed as an ordinary group/directed message), and must never false-positive on organic JS8 text.

### 3.2 Design: `@OPULSE` group beacon + INFO token (both, layered)

**Primary (broadcast) — the group beacon.** OpenPulse stations periodically send a directed free-text message to the custom group `@OPULSE` (a legal JS8 custom group; stock JS8Call users who are not members see it as normal third-party group traffic):

```
DC0SK: @OPULSE OPHF1 <PAYLOAD>
```

- `OPHF1` = magic + hint version 1. Detection rule: a directed message **to group `@OPULSE`** whose text begins with the standalone token `OPHF<digit>`. Both conditions must hold — group address alone is not enough (anyone can send to a group), token alone is not enough (someone could type it); together with the payload checksum below, organic collision is effectively impossible.
- `<PAYLOAD>` = 8 chars of base-36 (`[A-Z0-9]`, chosen because they are cheap in varicode and survive JS8's uppercase normalization) encoding 40 bits: `caps:u16 | pref_channel:u6 | listen_submode:u3 | reserved:u7 | check:u8` where `check` = CRC-8 over the preceding bits salted with the sender callsign (kills both random-text collisions and copy-paste replay of someone else's payload verbatim).
- `caps` mirrors the low 16 bits of the peer `capability_mask` (§5.2) — enough for "speaks HPX", "QSY/rendezvous capable", "PQ handshake", "relay".
- Total text: `OPHF1 XXXXXXXX` = 14 chars ≈ 1–2 NORMAL frames. Sent every `hint_interval_beacons`-th beacon slot (default: every 3rd heartbeat), so the steady-state pattern on-air is: mostly standard `@HB HEARTBEAT <grid>`, occasionally one small group message.

**Secondary (queried) — the INFO token.** The station's JS8 INFO text (the standard free-text station info that `INFO?` returns) is configured to end with the same token: `... OPHF1 XXXXXXXX`. Any JS8Call operator who queries us sees a normal INFO string with a trailing code; any OpenPulse station that queries a candidate peer detects capability without waiting for a beacon. This also gives us hint detection for stations we discover by their reply to *someone else's* `INFO?`.

**Rejected alternatives** (documented for the record):

- *Unused directed-command codes*: bit-efficient but not forward-safe — stock JS8Call renders unknown command codes oddly or drops them, and we would be squatting on protocol space the upstream project may assign. Violates "remain a valid JS8 station".
- *Steganographic tricks* (grid low-bits, timing, tone-offset signatures): fragile, undetectable by half the fleet, and arguably a coded transmission obscuring meaning — a regulatory red flag (§9).
- *Hint inside HEARTBEAT*: impossible; no text field (§2.4).

**Implementation note — on-air frame reality (measured, corrects the "1–2 frames" estimate above).**
Running the *verbatim upstream* `Varicode::buildMessageFrames("@OPULSE OPHF1 XXXXXXXX")` (compiled
against real Qt5) shows the beacon is **four** NORMAL frames, not 1–2: `[0]` a `FrameCompound`
carrying the sender + grid, `[1]` a `FrameCompoundDirected` carrying the group `@OPULSE` (a *custom*
group is **not** in the upstream `basecalls` map, so it cannot ride the 28-bit directed `to` field —
it is sent as the `<....>` placeholder in the directed frame and for real in this compound frame),
and `[2]`/`[3]` `FrameData` frames carrying the free text. The free-text frames **mix coders**:
`packDataMessage` emits whichever of Huffman or the JSC word-dictionary coder packs more chars per
frame, so a general beacon needs both decoders. Since the hint alphabet (`0-9 A-Z space OPHF`) is
entirely in the Huffman table and OpenPulse controls its own hint TX, **the OpenPulse beacon is
standardized on Huffman-framed data** (still a valid JS8 frame that stock JS8Call decodes), so
Huffman-only RX fully covers the hint; JSC decode (the 262k-entry codebook) is a follow-on that only
buys us general third-party free-text (the secondary INFO-token path and reading arbitrary traffic).

**Shipped (Phase C message-layer + peer recognition):**
- `plugins/js8/src/varicode.rs` (`HUFF_TABLE`, `huff_decode`, `unpack_data_message` →
  `DataText::{Huffman, JscUnsupported}`) and `grammar::unpack_directed_message` (`FrameDirected` →
  from/to/cmd/num) — both validated against Qt5-compiled ground-truth vectors. (Also fixed
  `unpack_compound_frame` to reject all data flags 4–7, not just 4.)
- `openpulse-discovery::hint_assembler::HintAssembler` — cross-slot, per-frequency-offset buffering
  of an over's frames (`First`/`Last` bracketed), assembling `Compound`(sender) +
  `CompoundDirected`(`@OPULSE`) + Huffman `Data` → `decode_hint` → `RecognizedHint`. Wired into
  `runtime.rs::decode_slot`: a recognised beacon upserts the sender into the `StationTable` with its
  `OphfHint`, so the daemon's existing `ListStations` reports `is_opulse: true`. End-to-end tested
  through the real `decode_window` on a 4-slot beacon.

- Shared `PeerCache` map (§5.2): `openpulse-daemon` `RuntimeControlState.peer_cache` +
  `sync_discovered_peers` — `discovery_tick` folds every newly-heard hinted station into the shared
  cache via `station_to_peer_record` (bringing `peer_map` live), and a `ListPeers`/`PeerList` control
  command exposes the recognized peers (capabilities + quality + trust) to clients. The cache is now
  the queryable substrate rendezvous (Phase F), relay routing, and peer queries read.

- Operator surface (Phase G): CLI — `openpulse daemon {enable-discovery, disable-discovery, stations,
  peers}` prints the daemon's `StationList`/`PeerList` as JSON; panel — a **Discovery tab** in
  `openpulse-panel` (Enable/Disable/Refresh + live station and OpenPulse-peer tables), driven by the
  `DiscoveryStatus`/`StationList`/`PeerList` events (reducer unit-tested; view build-verified).

- JSC decode (`plugins/js8/src/jsc.rs`): the full 262 144-entry JSC codebook (zlib-compressed ~1.1 MB
  blob, ported from GPL-3.0 `jsc_map.cpp`) + `jsc_decompress`, wired into `unpack_data_message`'s
  `compressed = 1` branch. Completes the JS8 free-text decoder (Huffman **and** JSC), so recognition
  no longer depends on Huffman-forced beacons and the passive INFO-token hint path (overheard INFO
  replies, which stock stations JSC-pack) is decodable. Validated against Qt5 ground-truth vectors
  across the codebook range (single chars → index-220k words).

**Discovery RX, operator surface, beacon TX, and rendezvous are all complete** (Phases A–G, PRs
#744–#805). Phase E beacon TX (heartbeat + `@OPULSE` hint via the `transmit_raw_audio` seam) and Phase F
rendezvous (2-message Propose/Accept/Reject over JS8 directed free text → scheduled QSY → `ConnectPeer`
CONREQ handoff) both ship **off by default**, gated on the §97.221 automatic-control documentation
(`docs/regulatory.md`) plus the callsign / ±2 s clock-skew / DCD / self-ID gates. **Only Phase H (on-air
validation vs stock JS8Call) remains**, in the deferred field-test batch.

### 3.3 What the hint can and cannot carry

40 payload bits is enough for capability flags + a preferred rendezvous channel index + preferred submode — i.e. enough to *filter and initiate*. It is **not** enough for keys: peer authentication happens after QSY via the existing Ed25519 CONREQ/CONACK handshake (`crates/openpulse-core/src/handshake.rs`), which is the design anchor that keeps rendezvous itself lightweight (§5.3, §9).

---

## 4. Architecture

### 4.1 "As a plugin" — resolving the tension

Investigated reality: `ModulationPlugin` (`crates/openpulse-core/src/plugin.rs:117`) is a **stateless waveform codec** — `&self` everywhere, no lifecycle hooks, no scheduler, no engine access; every protocol- or time-shaped behavior in the repo lives in the engine or in service crates (`openpulse-mesh`'s `BeaconScheduler`, `openpulse-repeater`, the daemon select loop). There is no service-plugin concept to attach a discovery scheduler to, and inventing one for this feature would fork the plugin model.

**Resolution** (the cleanest workable reading of "implemented as a plugin"):

1. **`plugins/js8`** — the JS8 waveform as a genuine `ModulationPlugin` (modes `JS8-NORMAL` first), registered in `crates/openpulse-daemon/src/server.rs` (~line 103–120, next to the other eight) and `crates/openpulse-cli/src/plugins.rs::register_all`, declaring `trait_version_required: "1.0"`. This satisfies the letter of the requirement where it is truthful: the *modem* is a plugin.
2. **`crates/openpulse-discovery`** — a pure-`std` protocol library (no tokio, no audio; the `openpulse-b2f` precedent): discovery + rendezvous state machines, station cache, hint codec, JS8 message grammar consumers. Fully unit-testable.
3. **Daemon wiring** — a thin module `crates/openpulse-daemon/src/discovery.rs` that owns the scheduler inside `server::run`'s select loop (the only place with `&mut engine` + `&mut ptt`), exactly like the station-ID block (`server.rs:806–862`) and QSY (`lib.rs::execute_qsy_actions`).
4. The whole feature is **compile-time optional** via a cargo feature `discovery` on `openpulse-daemon` (default **on** for the workspace build but trivially excludable), and **runtime opt-in** via `[discovery] enabled = false`.

This split is stated plainly in this plan and should be re-stated in the PR that introduces it, so nobody later "fixes" the discovery service into the plugin trait.

### 4.2 Crate/module layout

```
plugins/js8/                          # ModulationPlugin: the JS8 waveform
  src/lib.rs                          #   Js8Plugin, PluginInfo, trait impl, frame_geometry
  src/submode.rs                      #   table from §2.2 (params_for_mode() à la plugins/ofdm/src/params.rs)
  src/costas.rs                       #   the two 3×7 arrays (§2.1), sync-pattern helpers
  src/ldpc174.rs                      #   LDPC(174,87) tables (ported from JS8Call-improved) +
                                      #   min-sum BP decode (pattern: openpulse-core::ldpc), CRC-12
  src/modulate.rs                     #   75-bit frame -> Gray 8-FSK tones -> GFSK audio (continuous phase)
  src/demodulate.rs                   #   window decoder: candidate search, fine sync, soft demod, BP, CRC
  src/frame.rs                        #   Js8Frame: pack/unpack 75-bit frames; packCallsign/packGrid/packCmd ports
  src/varicode.rs                     #   Huffman table + (s,c)-dense JSC dictionary (ported)
  src/message.rs                      #   grammar: HeartbeatMsg, DirectedMsg, DataMsg <-> frames; text render/parse
  tests/reference_vectors.rs          #   bit/tone-exact vs committed JS8Call-improved vectors
  tests/js8_loopback.rs               #   encode -> openpulse-channel -> decode
  tests/llr_reliability.rs            #   LLR calibration gate (scfdma pattern)

crates/openpulse-discovery/           # protocol library, no I/O
  src/lib.rs
  src/hint.rs                         #   OPHF payload codec (§3.2) + detector + CRC-8
  src/station.rs                      #   Js8Station record, StationTable (TTL), PeerCache mapping (§5.2)
  src/scheduler.rs                    #   Js8Clock (SystemTime slots, drift bias), SlotPlan (HB/hint/query cadence)
  src/discovery_sm.rs                 #   DiscoveryState machine (§4.3) -> Vec<DiscoveryAction>
  src/rendezvous.rs                   #   RendezvousSession (§5.3) -> Vec<RendezvousAction>
  tests/{hint_collision,discovery_sm,rendezvous,scheduler}.rs

crates/openpulse-daemon/src/discovery.rs   # glue: drives the SMs from the select loop; owns dwell audio ring
```

**Plugin trait fit.** `Js8Plugin::modulate(data, config)` takes one packed 75-bit frame (10 bytes, 5 pad bits) and emits the 101 120-sample NORMAL burst; `demodulate` returns the *best* decode's 10 bytes (satisfying the trait's hard-slice conformance and enabling `ChannelSimHarness` loopback tests). The real receiver entry is an inherent, richer API used by the discovery service directly:

```rust
pub struct Js8Decode { pub frame: [u8; 10], pub freq_hz: f32, pub dt_s: f32, pub snr_db: f32 }
pub fn decode_window(samples: &[f32], submode: Submode, cfg: &DecodeCfg) -> Vec<Js8Decode>;
```

`frame_geometry` declares `min_frame_samples = max_frame_samples = samples_per_period` so the engine treats a slot as one frame. `occupied_bandwidth_hz` returns 50.0 for NORMAL (feeds the existing bandplan width checks). One deliberate deviation from every other plugin: **JS8 frames must go on the wire without the OpenPulse `Frame` envelope** (interop requires byte-exact JS8), so the discovery service does *not* use `engine.transmit()` (which wraps payloads in the envelope). See §6.4 for the TX seam.

### 4.3 Discovery state machine

Lives in `openpulse-discovery::discovery_sm`, pure (`step(event, now) -> Vec<DiscoveryAction>`), driven by the daemon.

```
                 +-------------------------------------------------------------+
                 |                                                             v
+----------+  idle predicate held      +-----------+   slot boundary    +-------------+
| INACTIVE |  for idle_grace_secs      | QSY_TO_HB |  QSY ok, squelch   |   DWELL     |
| (normal  |-------------------------->| save home |------------------->| slot-aligned|
|  ops)    |  && enabled && clock ok   | freq, tune|   re-applied       | RX + sched  |
+----------+                           +-----------+                    | TX (HB/hint/|
     ^                                       | QSY fail                 |  query)     |
     |                                       v                          +-------------+
     |                                  INACTIVE (event)                 |  |   |
     |   restore home freq                                               |  |   | user picks peer
     |<------------------------------------------------------------------+  |   v
     |     dwell_secs elapsed, or operator activity / ControlCommand        |  +--------------+
     |     needing the modem, or discovery disabled                         |  | RENDEZVOUS   |
     |                                                                      |  | (§5.3 session|
     |<---------------------------------------------------------------------+  |  or responder|
     |     decode results -> StationTable upserts -> events (every slot)       |  role)       |
     |                                                                         +--------------+
     |        rendezvous agreed: QSY to agreed freq, hand off to HPX handshake     |
     +------------------------------------------------------------------------ ---+
                        (on handshake success: normal session; on timeout/fail:
                         restore home or return to DWELL per config)
```

**Idle predicate** (assembled from daemon investigation; there is no single flag today):

```
engine.hpx_state() == HpxState::Idle                       // engine.rs:1735, hpx.rs:8
&& engine.hpx_session_id().is_none()                       // no secure session
&& runtime_state.pending_handshake.is_none()               // lib.rs:199
&& qsy_session_quiescent(&runtime_state.qsy_session)       // NB: never cleared today — check state, not is_some()
&& runtime_state.ptt_asserted_at.is_none()
&& frames_transmitted() delta == 0 over idle_grace_secs    // the StationIdTimer idiom, server.rs:578
&& !engine.is_channel_busy()                               // home-frequency DCD clear
&& !runtime_state.repeater_enabled
```

**Dwell behavior per NORMAL slot (15 s):**

- RX: accumulate all tick audio into the dwell ring; at slot end + guard, decode the window off-thread (§6.2); upsert stations; emit events.
- TX (only in `beacon`/`full` modes, §8): per `SlotPlan` — heartbeat every `heartbeat_interval_slots` (default 8 ≈ 2 min), hint beacon every `hint_interval_beacons`-th beacon, at a random 500–1000 Hz offset; queries per §4.4. **Never TX if the previous window decoded a transmission still in progress at our offset, and skip the slot if `engine.dcd_energy()` shows the channel occupied at T−200 ms** (direct DCD check; do *not* enable the engine's 0.3-persistence CSMA — random deferral breaks slot alignment).
- Auto-ID: suppress the OpenPulse-mode `StationIdTimer` TX while on the JS8 frequency (transmitting a BPSK ID burst on the JS8 calling channel is QRM); JS8 heartbeats carry our callsign *in the same code* and satisfy §97.119 — but keep the timer accounting so a dwell that somehow only sent non-callsign frames still forces an ID (§9).

### 4.4 Query policy (politeness by construction)

- Only query stations not already resolved (no grid, or hint-unknown but `query_new_stations = true`).
- Global budget: `max_queries_per_10min` (default 2), one query per station per dwell, exponential per-station backoff persisted in the `StationTable`.
- Never query during a slot in which we heard the target transmitting (they're busy).

---

## 5. Wire formats and data model

### 5.1 Discovered-station record

```rust
/// openpulse-discovery::station
pub struct Js8Station {
    pub callsign: String,            // normalized uppercase
    pub grid: Option<String>,        // from HB/GRID replies (4–6 chars kept)
    pub snr_db: f32,                 // EWMA of decode SNRs
    pub freq_offset_hz: f32,         // last audio offset
    pub dial_freq_hz: u64,           // calling freq we heard them on
    pub last_heard_ms: u64,
    pub heard_count: u32,
    pub hint: Option<OphfHint>,      // parsed §3.2 payload; None = plain JS8 station
    pub query_backoff: QueryBackoff,
}
pub struct OphfHint { pub version: u8, pub caps: u16, pub pref_channel: Option<u8>, pub listen_submode: Submode }
```

`StationTable`: `BTreeMap<String, Js8Station>` + TTL sweep (`station_ttl_secs`, default 3600). This richer table is the panel's source of truth; `PeerCache` is the *shared* substrate other subsystems already query.

### 5.2 Mapping into `PeerCache`

`PeerRecord` (`crates/openpulse-core/src/peer_cache.rs:25`) mapping on every upsert of an OpenPulse-marked station (plain JS8 stations stay only in `StationTable` — they are not OpenPulse peers):

| `PeerRecord` field | Source |
|---|---|
| `peer_id: String` | `format!("js8:{callsign}")` until an Ed25519 `PeerDescriptor` is learned post-handshake; then re-keyed to the descriptor's key-bytes ID (the handshake path already does this) |
| `capability_mask: u32` | low 16 bits from the hint `caps`; **new capability-bit registry** below |
| `route_quality: u8` | `((snr_db + 30.0).clamp(0.0, 42.0) * 6.0) as u8` — maps the JS8 dynamic range −30…+12 dB onto 0–252, monotone, documented next to the constant |
| `trust_level` | `peer_cache::TrustLevel::Unknown` — a key-less RF-heard peer; passes `TrustFilter::Any`/`TrustedOrUnknown`, excluded from `TrustedOnly`; loses upsert conflicts to any authenticated record (exactly the semantics we want) |
| `revision` | 0 (JS8-heard records never outrank descriptor-signed ones) |
| `updated_at_ms` | `last_heard_ms` |
| `callsign_hash` | `sha256(callsign)` (the `PeerDescriptor::callsign_hash` convention) |

**Capability-bit registry** (none exists today — `wire_query.rs:202` says "application-defined"; this plan claims the low bits and documents them in `docs/dev/peer-query-relay-wire.md`):

```
bit 0  CAP_HPX          speaks OpenPulse HPX sessions
bit 1  CAP_RENDEZVOUS   accepts JS8 OPHF rendezvous
bit 2  CAP_QSY          in-session openpulse-qsy protocol
bit 3  CAP_PQ           post-quantum handshake
bit 4  CAP_RELAY        relay forwarding
bits 5–15 reserved (carried in the 16-bit hint caps field)
```

Existing `PeerCache::query(capability_mask, min_quality, trust_filter, max_results, now_ms)` then answers "rendezvous-capable peers by quality" for free.

### 5.3 Rendezvous wire protocol (over JS8)

**Relationship to `openpulse-qsy`** — examined and decided:

- The existing `QsySession` (`crates/openpulse-qsy/src/session.rs`) is a 4-message REQ→LIST→VOTE→ACK negotiation with per-candidate live scanning, ASCII frames (`QSY_REQ <token> <n>` …), no timeouts, designed for an *established* OpenPulse link. Over JS8, four exchanges × (1–4 frames each) × 15 s/frame ≈ **3–6 minutes** of channel time on a shared calling frequency — rude and fragile.
- **Decision: a new 2-message `RendezvousSession` in `openpulse-discovery`, reusing `openpulse-qsy`'s bandplan machinery** (`BandplanPolicy::validate_frequency`, `band_label_for_hz`, `occupied_bandwidth_hz` — `crates/openpulse-qsy/src/bandplan.rs`) and the daemon's existing scan/QSY executors, but not its wire frames or state machine. The initiator pre-scans *before* transmitting (it has idle time), so LIST/VOTE collapse.

Carried as JS8 **directed free-text messages** (Data frames addressed to the peer, standard first/last chaining), compact grammar:

```
DC0SK: KN4CRD OPHF1 QSY? R7 C3 C9 K2        # propose: token R7 (2 base-36 chars), ranked channels C3,C9,K2
KN4CRD: DC0SK OPHF1 QSY R7 C9 S4            # accept: channel C9, switch at slot +4 (~60 s)
KN4CRD: DC0SK OPHF1 NO R7 B                 # or reject: reason code (B=busy, T=trust, F=no common freq, ...)
```

- **Channel table**: candidate frequencies are indexed against a published per-band channel table (config `rendezvous_channels_hz`, validated through `BandplanPolicy` at load). 2 chars per channel instead of 7+ digits of Hz keeps the proposal in ~2 frames (~30 s round trip ≈ 45–60 s total).
- **Timeouts** live in `RendezvousSession` (the QSY session has none — a documented gap): proposal expires after `rendezvous_timeout_slots` (default 8); on expiry → `NO` internally, return to DWELL.
- **No Ed25519 signature on rendezvous frames** — a 64-byte signature is ~7 extra frames (~2 min). Instead: rendezvous is *unauthenticated but harmless* — the parties merely agree where to meet; immediately after QSY the initiator sends the existing **signed CONREQ** and the responder verifies per trust policy (`crates/openpulse-core/src/handshake.rs`). A spoofed rendezvous wastes ≤ one timeout and fails the handshake. This mirrors (and is safer than) the current daemon QSY reality, which already transmits unsigned frames (`encode_unsigned` in `crates/openpulse-daemon/src/lib.rs:54`) with responder trust hard-coded `Unverified` (lib.rs:1162).
- **Handoff**: at the agreed slot both stations `CatController::set_frequency(chan_hz)` (`crates/openpulse-radio/src/cat_controller.rs:12`), re-apply band squelch (`apply_band_squelch`, lib.rs:2026), switch to the configured OpenPulse mode/profile, and the initiator drives the normal `ConnectPeer` path (`begin_secure_session`); responder listens. Handshake timeout (`rendezvous_handshake_timeout_secs`, default 30) → both return (initiator to home freq, responder per config).

### 5.4 Hint payload layout (normative)

```
OPHF payload, 40 bits, big-endian bit order, rendered as 8 base-36 chars:
  [0:16)   caps        u16   capability bits (§5.2 registry, bits 0–15)
  [16:22)  pref_chan   u6    index into rendezvous channel table; 63 = none
  [22:25)  submode     u3    preferred listen submode (0=NORMAL,1=SLOW,2=FAST,3=TURBO,4=ULTRA)
  [25:32)  reserved    u7    zero; receivers ignore
  [32:40)  check       u8    CRC-8/SMBUS over bits [0:32) XOR-folded with sha256(callsign)[0]
```

Version bumps change the `OPHF<d>` digit; parsers must ignore unknown versions (forward compat).

---

## 6. Daemon integration

### 6.1 Ownership and control flow

All new daemon logic hangs off the existing single-owner select loop in `crates/openpulse-daemon/src/server.rs::run` (line 44; engine is a loop-local `mut`, *not* `Arc<Mutex>` — any TX-capable scheduler must live here, confirmed by investigation). Additions:

- `RuntimeControlState` (lib.rs:155) gains `discovery: DiscoveryRuntime` — the SM instances, dwell ring buffer handle, saved home frequency/mode, decode-task join handle.
- The **rx_ticker arm** (50 ms default) gains a `discovery::tick(...)` call placed with the station-ID block (server.rs:806–862): feed samples, check slot boundaries, run due actions.
- Command arm: new `ControlCommand`s dispatch through `apply_command_to_engine` (lib.rs:1542) as usual. Any operator command that needs the modem (`ConnectPeer`, `SendMessage`, `StartOtaSession`, `SetMode`, `SetFreq`) while dwelling first triggers `DiscoveryEvent::OperatorPreempt` → restore home freq, then executes.
- `maybe_qsy_on_interference` (lib.rs:1445) is **suppressed during dwell** (we are intentionally parked on a busy shared channel).

### 6.2 RX path — dwell ring, not the burst pipeline

The production RX pipeline is DCD-burst-driven (`accumulate_capture` → `accumulate_routed`, engine.rs:1287/1302: accumulate while energy ≥ squelch, flush on carrier drop). Two reasons it cannot carry JS8: (a) −24 dB signals never trip the squelch; (b) JS8 needs fixed wall-clock windows, not carrier-drop framing.

Design: the select loop already owns each tick's raw samples *before* handing them to `accumulate_capture` (server.rs rx arm). While dwelling, it **also** appends them to a `DwellRing` (16 s @ 8 kHz = 128 000 f32 ≈ 512 KB). At slot end + 0.5 s guard, the ring's window slice is cloned into a `tokio::task::spawn_blocking` running `js8::decode_window` (decode is plugin-internal, needs no `&mut engine` — this keeps a ~1 s decode off the 50 ms tick loop; results return via a channel polled next tick). The normal `accumulate_capture` keeps running untouched — an OpenPulse station calling us directly on the JS8 frequency (misconfiguration) still behaves as today.

Per the seam-gap lesson (CLAUDE.md "RX capture has two entry families"), the dwell tap is placed **after** `route_audio_stage(InputCapture)` processing order in the tick (so notch/DC removal apply — but AGC noted: verify the notch does not sit inside the 500–1000 Hz HB sub-band when parked; if enabled, disable auto-notch during dwell), with a `dwell_samples_accumulated` tripwire counter and a test through the production entry (`twin` harness), not only a convenience seam.

### 6.3 The T/R scheduler

`Js8Clock` (openpulse-discovery, pure): `slot_index(now: SystemTime, submode) -> u64`, `phase_ms(now)`, `apply_drift_bias(ms)`; drift estimated as the running median of decode `dt` values; `ControlEvent::DiscoveryStatus` carries the current bias, and TX is refused (RX-only degrade + `CommandError`-style event) when `|bias| > max_clock_skew_ms` (default 2000). `SlotPlan` decides, per slot: `Listen`, `TxHeartbeat`, `TxHint`, `TxQuery(callsign)`, `TxRendezvous(frames)` — all subject to the DCD check at T−200 ms.

### 6.4 TX path — one small engine addition

JS8 frames must not carry the OpenPulse `Frame` envelope, so `engine.transmit(payload, mode, None)` (which frames + FECs payloads) is unusable. Rather than bypass the engine (which would skip the CE-SSB/attenuation/metrics `OutputEmit` seam — the exact seam-gap bug class this repo has already been burned by), add:

```rust
/// ModemEngine: play pre-modulated audio through the standard output seam
/// (route_audio_stage(PipelineStage::OutputEmit): attenuation, CE-SSB per-mode policy,
///  metrics, loopback tap). No Frame envelope, no FEC, no CSMA — caller owns channel access.
pub fn transmit_raw_audio(&mut self, samples: &[f32], device: Option<&str>) -> Result<(), ModemError>;
```

with a `raw_audio_blocks_transmitted()` tripwire counter (the `notch_blocks_processed()` pattern). CE-SSB policy: JS8 GFSK is constant-envelope; CE-SSB must be a no-op/off for this path (same per-mode conditioner mechanism as the multicarrier-only rule from PR #521). The discovery glue then transmits with the canonical PTT idiom (server.rs:822–855): assert PTT (skip TX entirely on hardware failure) → `PttChanged{true}` → `block_in_place(|| engine.transmit_raw_audio(&samples, None))` → release → `PttChanged{false}`; the PTT watchdog (lib.rs:1108) is already in force.

### 6.5 New control-protocol surface

`crates/openpulse-daemon/src/protocol.rs`, following the existing serde-tagged NDJSON shapes (`#[serde(tag="cmd"/"type", rename_all="snake_case")]`); note the panel-side constraint that events must not carry a field literally named `ok` (connection.rs drops such lines):

```rust
// ControlCommand additions
EnableDiscovery, DisableDiscovery,
ListStations,                                   // request/response, like ListMessages
QueryStation   { callsign: String },            // manual INFO? (counts against budget)
StartRendezvous{ callsign: String },
AbortRendezvous,

// ControlEvent additions
DiscoveryStatus { state: String,                // "inactive"|"dwell"|"rendezvous"|...
                  dial_freq_hz: Option<u64>, clock_bias_ms: i64,
                  slots_listened: u64, last_decode_count: u32 },
StationHeard    { station: StationSummary },    // per new/updated station
StationList     { stations: Vec<StationSummary> },  // reply to ListStations
Js8Traffic      { from: String, text: String, snr_db: f32, freq_hz: f32 }, // decode log line
RendezvousProgress { peer: String, phase: String,   // "proposed"|"agreed"|"qsy"|"handshake"|"failed"
                     agreed_freq_hz: Option<u64>, detail: String },
```

`StationSummary { callsign, grid, snr_db, last_heard_secs, openpulse: bool, caps: u16, dial_freq_hz }` lives in protocol.rs next to `MessageSummary`. Request/response commands (`ListStations`) are handled in the per-client `handle_command` — remember the documented lib.rs/ws.rs KEEP-IN-SYNC pair (lib.rs:756–760).

---

## 7. Panel UX (`apps/openpulse-panel`, iced)

Follows the exact existing patterns found in investigation: new `Tab` variant + `tab_btn` + match arm in `tabbed_lower` (ui.rs:818–895); daemon-echoed toggle (the Repeater pattern, app.rs:270–281); `stats_widget`-style columnar table (ui.rs:1178–1254) with `link_btn` per-row actions (inbox pattern, ui.rs:913–942).

### 7.1 State and wiring

- `PanelState` (state.rs) additions: `discovery_enabled: bool`, `discovery_state: String`, `discovery_freq: Option<u64>`, `clock_bias_ms: i64`, `stations: Vec<StationSummary>`, `rendezvous: Option<(String, String)>` (peer, phase). Updated exclusively in `connection.rs::apply_event` match arms (new variants → fields + `push_log`).
- `Message` additions (app.rs): `ToggleDiscovery`, `RefreshStations`, `QueryStation(String)`, `Rendezvous(String)`, `AbortRendezvous`. Each maps to `self.send(ControlCommand::…)` in `update()` — the single funnel (app.rs:173–177).
- `Tab::Discovery` added to the enum (app.rs:37–45) and dispatch.

### 7.2 Mockup

```
[ INFO ] [ STATS ] [ DISCOVERY ] [ CONFIG ] [ MESSAGES ] [ LOG ]
+---------------------------------------------------------------------------+
| DISCOVERY                                        Discovery: ON            |
| state: DWELL @ 14.078 MHz (20m)   clock: -0.4 s   slots: 124   heard: 17  |
|---------------------------------------------------------------------------|
| CALLSIGN   GRID    SNR    AGE     OP?   CAPS         ACTIONS              |
| KN4CRD     EM73   -07     0:12    [OP]  HPX RDV QSY  [Rendezvous] [Query] |
| OH8STN     KP25   -18     1:03    [OP]  HPX RDV      [Rendezvous] [Query] |
| DL1ABC     JO62   -21     3:40     -    -            [Query]              |
| M0XYZ      IO91   -24     7:15     -    -            [Query]              |
|                                                            (scrollable)   |
|---------------------------------------------------------------------------|
| RENDEZVOUS: KN4CRD — phase: agreed @ 14.109 MHz, QSY in 2 slots   [Abort] |
+---------------------------------------------------------------------------+
```

- "Discovery: ON/off" = `toggle_btn` bound to `snap.discovery_enabled` (daemon-echoed, not optimistic — the daemon may refuse, e.g. clock skew).
- Table = `stats_widget` cell/row closures; `[OP]` badge rendered in `ColorRole::Locked`, plain stations `Inactive`; rows sorted OP-first then SNR.
- `[Rendezvous]`/`[Query]`/`[Abort]` = `link_btn`s with `tip(...)` tooltips (house convention).
- The controls band (top of the panel) additionally gets a small status chip (`state_chip`) showing `DISC` when dwelling, so the operator always sees the rig is parked off the home frequency.
- Rendezvous flow: click `[Rendezvous]` → row pins to a status strip fed by `RendezvousProgress` events → on `handshake` phase success the existing `RfConnectionChanged` takes over (already rendered in Line 1 of the controls band).

---

## 8. Configuration (`crates/openpulse-config`)

New section following the `LogbookConfig` pattern (struct-level `#[serde(default)]`, manual `Default`, one-line doc comments, opt-in false; template block appended to `init_template()`'s raw string — which must keep parsing under the `modem_profile_loads_and_template_parses` guard test):

```toml
[discovery]
# Master switch for JS8 discovery. Default false.
enabled = false
# "rx_only" (default) | "beacon" (HB + hint TX) | "full" (adds queries + rendezvous responder)
mode = "rx_only"
# JS8 submode for the calling channel. NORMAL only in MVP.
submode = "normal"
# Seconds the idle predicate must hold before auto-QSY to the JS8 frequency.
idle_grace_secs = 120
# Maximum dwell before returning to the home frequency (0 = until preempted).
dwell_secs = 900
# Heartbeat every N slots (N * 15 s for NORMAL). 8 = every 2 minutes.
heartbeat_interval_slots = 8
# Send the @OPULSE hint every Nth beacon.
hint_interval_beacons = 3
# Actively query newly-heard stations with INFO? (mode = "full" only).
query_new_stations = false
max_queries_per_10min = 2
# JS8 calling frequencies per band (defaults = the published JS8 conventions).
[discovery.calling_freqs_hz]
"160m" = 1842000
"80m"  = 3578000
"40m"  = 7078000
"30m"  = 10130000
"20m"  = 14078000
"17m"  = 18104000
"15m"  = 21078000
"12m"  = 24922000
"10m"  = 28078000
# Rendezvous channel table (indices used on-air; validated against the bandplan at load).
rendezvous_channels_hz = []
rendezvous_timeout_slots = 8
rendezvous_handshake_timeout_secs = 30
station_ttl_secs = 3600
# Refuse TX when |UTC offset estimate| exceeds this (RX-only degrade).
max_clock_skew_ms = 2000
group = "OPULSE"
```

Band selection: dwell uses the calling frequency for the band of the current home frequency (`band_label_for_hz`, `crates/openpulse-qsy/src/bandplan.rs:404`); no automatic band-hopping in this plan (a later extension). `SetConfig` persistence mirrors the existing `save_qsy_config` mechanism.

---

## 9. Regulatory and interop etiquette

- **Station ID (REQ-REG-05/REQ-REG-10, §97.119)**: every heartbeat/directed frame carries our callsign *in the transmitted code* — JS8 traffic self-identifies. The daemon `StationIdTimer` (CAP-66) stays armed; its OpenPulse-waveform ID burst is **suppressed while parked on the JS8 channel** (it would be QRM in a 50 Hz-channelized band segment) and the JS8 TX counts as identification. On return to the home frequency, normal ID behavior resumes; the `frames_transmitted` baseline is re-synced so the dwell doesn't trigger a spurious immediate ID.
- **Automatic control (REQ-REG-04/06, §97.221)**: beaconing while unattended is automatic control. Mitigations already present: PTT watchdog (`ptt_max_duration`), control-port kill (`DisableDiscovery`), and this plan's TX duty ceiling (one 12.64 s TX per 8 slots ≈ 10% worst case, typically ~5%). The plan's default `mode = "rx_only"` means **no transmission at all** until the operator opts in; docs must state the operator remains the control operator and cite the recognized automatic sub-bands (regulatory.md:181 — e.g. 30 m 10.147–10.150 MHz) where applicable. REQ-REG-04 documentation gap gets a section in `docs/regulatory.md` in Phase E.
- **The hint is not an encoded message obscuring meaning**: `OPHF1 <base36>` is a published, openly documented capability code (this document + `docs/dev/design/protocol-wire-spec.md` addition), analogous to ADIF fields or JT-alike telemetry — the same stance JS8Call itself takes for its packed frames. Publishing the spec is the compliance mechanism; keep it in the repo docs and release notes.
- **Etiquette on the calling frequency**: HB in the 500–1000 Hz sub-band convention; DCD check before every slot TX; heartbeat cadence ≥ 2 min (JS8Call community norms frown on faster); hint beacons ride the existing cadence rather than adding TX; queries rate-limited (§4.4); rendezvous *moves off* the calling channel for the actual session — which is precisely the polite pattern (meet on calling, work elsewhere). Long OpenPulse sessions never happen on the JS8 channel.
- **Bandplan**: every frequency we tune (calling + rendezvous channels) passes `BandplanPolicy::validate_frequency` with the OpenPulse session mode's `occupied_bandwidth_hz` for the *rendezvous* channel (an HPX2300 session needs a wideband-appropriate segment — the existing REQ-REG-13/CAP-45 machinery).

---

## 10. Testing strategy

Hard rule: everything passes `cargo test --workspace --no-default-features`; no audio hardware; deterministic seeds.

### 10.1 Reference vectors (the interop anchor)

- `tools/js8-vectors/` (dev-only, not in the workspace test path): instructions + a small harness to build JS8Call-improved and dump, for a fixed corpus of messages (HB, directed `INFO?`, multi-frame data text, compound callsigns): (a) packed 75-bit frames, (b) 174-bit codewords, (c) 79-tone sequences, (d) 12 kHz reference WAVs. Outputs are **committed** under `plugins/js8/tests/data/` (small: tones + frames as JSON, a few WAVs resampled offline to 8 kHz) so CI never needs the C++ build.
- `plugins/js8/tests/reference_vectors.rs`: pack/unpack, CRC-12, LDPC encode, Gray mapping, and tone sequences must match bit-for-bit; decoder must decode the reference WAVs.

### 10.2 Modem tests

- `js8_loopback.rs`: encode → `openpulse-channel` (AWGN sweep, Watterson good/moderate F1) → `decode_window`; gates: 100% at −10 dB AWGN, ≥90% at −18 dB AWGN (MVP), stretch −22 dB tracked as an `--ignored` sweep like `scfdma_ce_sweep`; multi-signal test: 3 stations at distinct offsets in one window, all decoded.
- `llr_reliability.rs`: binned |LLR| vs empirical error rate (the scfdma gate, per CLAUDE.md LLR contract).
- Decode CPU budget test (`--ignored`, informational): NORMAL window decode wall-time printed; `cross check` target covers Pi builds.
- Trait conformance: hard/soft slice equivalence via the existing `soft_demod_conformance` harness (js8 is not engine-registered, so the sweep does not reach it — see that file's header); `ChannelSimHarness` single-frame round trip (engine-level registration works).

### 10.3 Protocol/service tests (all pure, no I/O)

- `hint_collision.rs`: (a) round-trip all field combinations; (b) a committed corpus of ~500 real-world JS8 message texts (scraped/transcribed, plain text) must produce **zero** hint detections; (c) mutated payloads fail the CRC-8; (d) unknown `OPHF9` versions are ignored not errored.
- `discovery_sm.rs`: idle→dwell→return transitions under scripted events (operator preempt, dwell timeout, clock-skew degrade, DCD-busy slot skip); TX never scheduled in `rx_only`; query budget enforced.
- `rendezvous.rs`: two `RendezvousSession` endpoints exchanging text frames in-memory — agree path, reject path, timeout path, bandplan-invalid channel path, simultaneous-initiate tiebreak (lower callsign wins, deterministic).
- `scheduler.rs`: slot arithmetic across day boundaries, drift-bias application, mock `SystemTime`.
- Cache mapping: SNR→quality monotonicity; `PeerCache::query(CAP_RENDEZVOUS, …)` returns hint-carrying stations only; JS8 record loses upsert to a descriptor-signed record.

### 10.4 Integration

- `crates/openpulse-daemon/tests/js8_discovery_twin.rs`: two real daemons on the twin harness (the `server::run` in-process bridge from PRs #507–#510), discovery enabled, virtual channel: A hears B's heartbeat + hint → `StationHeard` event asserted through the **production entry path** (dwell ring fed by the real rx tick), tripwire counters (`dwell_samples_accumulated`, `raw_audio_blocks_transmitted`) asserted non-zero; then `StartRendezvous` → both engines end on the agreed frequency (mock `CatController` recording `set_frequency` calls) → CONREQ observed.
- Panel: `theme.rs`-style iced-free unit tests for the new state reducers (`apply_event` arms), matching existing panel test structure.

### 10.5 Acceptance table (CLAUDE.md style)

| Requirement | Acceptance test |
|---|---|
> **Note (2026-07-18):** the target names below were written when this was a plan; the shipped tests
> landed under different names. Equivalent coverage exists — the mapping is given per row.

| JS8 NORMAL decode reaches the −18 dB weak-signal gate | `cargo test -p js8-plugin --test snr_sweep gate_at_minus_18_db` (planned as `js8_loopback`/`reference_vectors`) |
| JS8 SNR estimation | `cargo test -p js8-plugin --test snr_estimate` (planned as `llr_reliability`) |
| Beacon assembly + `@OPULSE` hint over GFSK audio | `cargo test -p openpulse-discovery --test beacon_loopback` (planned as `hint_collision`/`discovery_sm`) |
| Rendezvous 2-message agreement, end to end over two runtimes | `cargo test -p openpulse-discovery --test rendezvous_end_to_end` (planned as `rendezvous`/`js8_discovery_twin`) |
| Daemon discovery tick: dwell, decode, cache, emit `StationHeard` | `cargo test -p openpulse-daemon --no-default-features discovery_tick` |
| PeerCache mapping + capability query | `cargo test -p openpulse-discovery station` |

---

## 11. Phased implementation plan

Ordered to de-risk the modem and the clock first; each phase is 1–3 PR-sized units with its own acceptance test; traceability chain (requirement → design → implementation → tests → results) per commit as usual.

| Phase | Deliverable | Acceptance | Risk |
|---|---|---|---|
| **A — JS8 TX core** (`plugins/js8`: submode/costas/ldpc174/frame/modulate + vectors tool) | Bit/tone-exact NORMAL encode; reference WAVs committed | `reference_vectors` (encode half) | Medium — table porting is mechanical; GFSK shaping needs care |
| **B — JS8 RX core** (demodulate: candidate search, sync, soft demod, BP decode; multi-decode) | Decode reference WAVs; loopback gate −18 dB; 3-signal window; LLR gate; CPU budget measured | `js8_loopback`, `llr_reliability` | **Highest** — FT8-class DSP; go/no-go checkpoint for D1 fallback |
| **C — message layer + hint** (varicode/JSC, grammar, `openpulse-discovery::hint`) | Grammar round-trips incl. compound callsigns; hint corpus test | `reference_vectors` (text), `hint_collision` | Medium — varicode/JSC tables are fiddly but fully vector-testable |
| **D — RX-only discovery MVP** (discovery SM, `Js8Clock`, `StationTable`+`PeerCache` map, daemon dwell ring + auto-QSY + events, config section) | Twin-daemon test: hears + caches + events; **zero TX paths compiled in this phase's runtime modes** | `discovery_sm`, `scheduler`, `station`, twin test (RX half) | Medium — daemon plumbing is pattern-following; clock code is new |
| **E — beacon/query TX** (`transmit_raw_audio` engine seam, SlotPlan TX, PTT wrap, ID suppression, DCD gating, regulatory doc section) | Twin test observes HB + hint on the wire; duty/budget/skew gates unit-tested | twin test (TX half), `discovery_sm` TX cases | Medium — the engine seam + seam-gap tests |
| **F — rendezvous + handoff** (`RendezvousSession`, channel table, QSY executor reuse, HPX handshake handoff) | Full twin flow: discover → rendezvous → both retune → CONREQ verified | `rendezvous`, twin test (full) | Medium-high — cross-daemon timing |
| **G — panel** (Tab::Discovery, state, connection arms, table + actions) | Manual + reducer unit tests | panel tests | Low — pure pattern-following |
| **H — on-air validation** (real JS8Call across the bench rigs/SDR monitor; etiquette review; regulatory checklist) | Decoded by stock JS8Call-improved; we decode live band traffic; report in `docs/test-reports/` | on-air (deferred-class, like Phase 5.5-reg) | External |

**MVP ship line = end of Phase D**: RX-only discovery with panel-less operation via `ListStations`/events (or reorder G's table ahead of E if operator visibility is wanted sooner). It exercises the two hardest subsystems (modem RX, wall-clock scheduling) with zero on-air TX and zero regulatory exposure.

**Go/no-go checkpoint after Phase B**: if native decode can't reach −18 dB or blows the Pi CPU budget, fall back to decision D1's external-process option for RX while keeping native TX (TX is comparatively easy and already proven by then).

---

## 12. Decisions — RESOLVED 2026-07-10

All seven decisions are locked; the rest of this doc is normative against them.

1. **D1 — Native JS8 modem.** ✅ **Native Rust** (Phases A–C). The **Phase-B go/no-go fallback is retained** as a contingency: if native decode can't reach −18 dB or blows the Pi CPU budget, fall back to a headless external JS8Call process for **RX only** while keeping native TX. Does not change Phase A/B work.
2. **D2 — Hint transport.** ✅ **`@OPULSE` group beacon *plus* INFO token** (§3.2). Group name is `OPULSE`. The 40-bit hint **keeps the preferred-channel field** (`caps | pref-channel | submode | callsign-salted CRC-8`, §5.4), so a caller can pre-seed the rendezvous.
3. **D3 — Rendezvous shape.** ✅ **New 2-message `RendezvousSession` over JS8 text, unauthenticated**; authentication is deferred to the post-QSY signed CONREQ/CONACK (§5.3). No signature is carried over JS8. Reuses `openpulse-qsy` bandplan/scan/QSY-executor but not its 4-message wire session.
4. **D4 — TX policy + regulatory gate.** ✅ Defaults accepted: feature **off**; when on, **`rx_only`**; beacon/query each opt-in; **HB every 8 slots**; **ID-suppressed during dwell** (§9). The **REQ-REG-04 automatic-control documentation is a hard gate on Phase E** — no on-air TX ships until it lands. Phases A–D (native modem + RX-only discovery MVP) carry zero TX and are unblocked now.
5. **D5 — Time sync.** ✅ **NTP-disciplined system clock required**; residual bias estimated from decode `dt`; **hard TX refusal beyond `max_clock_skew_ms` = ±2 s** (JS8's published clock tolerance) with a visible panel warning; degrade to RX-only otherwise. **No PPS/GPS/manual-offset** support in scope.
6. **D6 — capability-bit registry.** ✅ Bits 0–4 of `capability_mask` claimed per §5.2, registered normatively in `docs/dev/peer-query-relay-wire.md`.
7. **D7 — dwell/home-band.** ✅ **Single-band dwell** (current band's JS8 frequency) for the MVP. The per-band config table (§8) is in place so multi-band scan rotation is a later, config-compatible extension.

---

## 13. Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| JS8 decoder misses the −24 dB class (lands at, say, −12 dB) | Discovery only sees strong stations; interop claim weakened | Phase-B gate at −18 dB with reference WAVs as ground truth; sweep harness tracks the gap; D1 fallback path; the *swept-applied-correction* and noiseless-first diagnostics from the DSP playbook apply verbatim |
| Decode CPU blows the Pi budget | Daemon tick starvation, missed slots | `spawn_blocking` isolation (never on the tick loop); fixed-depth decode like the improved fork; candidate-count cap; measured budget test in Phase B |
| Clock skew in the field (no NTP) | TX lands outside peers' sync search; we look broken on-air | Skew estimate from decode `dt`s; hard TX refusal beyond threshold with a visible panel warning; RX still works (dt search absorbs) |
| Hint reads as abuse to the JS8 community | Social/interop blowback worse than any bug | Hint is rare (every 3rd beacon), tiny, on a legal group address, openly documented; RX-only default; heartbeat cadence at community norms; publish the spec |
| Rendezvous hijack/spoof (unauthenticated JS8 text) | Station lured to a frequency; wasted timeout | Harmless-by-design: signed CONREQ immediately after QSY authenticates or aborts; bounded by `rendezvous_handshake_timeout_secs`; return-home is unconditional |
| Dwell hides the station from OpenPulse callers on the home frequency | Missed inbound sessions | That is inherent to single-receiver QSY — surfaced honestly in the panel status chip; `idle_grace_secs` + operator preempt keep the window small; future: announce dwell schedule via mesh beacon |
| Seam-gap regressions (dwell tap or raw-TX path bypasses a front-end/back-end transform) | Silent feature non-function in production paths | Both new seams get tripwire counters + twin-harness tests through production entries, per the CLAUDE.md checklist |
| `qsy_session` never-cleared quirk poisons the idle predicate | Discovery never activates after one QSY | Idle predicate checks session *state*, not `is_some()` (§4.3); fix-the-quirk is a candidate hygiene PR in Phase D |
| Varicode/JSC table porting errors | Subtle text corruption invisible to loopback | Reference-vector tests cover text framing end-to-end, not just tones |
| Scope creep toward a JS8 chat client | Schedule | Non-goals list (§1); decode log is read-only `Js8Traffic` events |

---

## Appendix A — key existing code anchors (for implementers)

| Concern | Anchor |
|---|---|
| Plugin trait + registry | `crates/openpulse-core/src/plugin.rs:117` (`ModulationPlugin`), `:266` (`PluginRegistry`), `PLUGIN_TRAIT_VERSION` `:7` |
| Plugin registration sites | `crates/openpulse-daemon/src/server.rs:82–120`, `crates/openpulse-cli/src/plugins.rs::register_all` |
| Multi-mode plugin template | `plugins/pilot/src/lib.rs` (mode-string params), `plugins/ofdm/src/params.rs::params_for_mode` |
| FSK precedent (Goertzel demod) | `plugins/fsk4/src/demodulate.rs` |
| LDPC min-sum pattern | `crates/openpulse-core/src/ldpc.rs` |
| Daemon select loop / rx tick / TX idiom | `crates/openpulse-daemon/src/server.rs:44` (`run`), `:546` (rx_ticker), `:806–862` (station-ID block = scheduler insertion point), `:822–855` (PTT wrap) |
| Periodic-timer pattern | `crates/openpulse-core/src/station_id.rs` (`StationIdTimer`), `crates/openpulse-mesh/src/beacon.rs` (`BeaconScheduler`) |
| RX streaming path | `ModemEngine::accumulate_capture` engine.rs:1287, `decode_burst` :1383, `BURST_MAX_SAMPLES` :395 |
| QSY session/frames/scan/executor | `crates/openpulse-qsy/src/{session,frame,scanner,bandplan}.rs`, `crates/openpulse-daemon/src/lib.rs:996` (`execute_qsy_actions`), `:1445` (`maybe_qsy_on_interference`) |
| CAT retune | `crates/openpulse-radio/src/cat_controller.rs:12`, `rig_controller.rs:37` (`RigctldController::set_frequency`); `apply_band_squelch` daemon lib.rs:2026 |
| Peer cache / descriptor / capability mask | `crates/openpulse-core/src/peer_cache.rs:25` (`PeerRecord`), `:135` (`query`), `peer_descriptor.rs`, `wire_query.rs:202` |
| Control protocol shapes | `crates/openpulse-daemon/src/protocol.rs:114` (events), `:223` (commands); KEEP-IN-SYNC lib.rs:756–760 |
| Panel patterns | `apps/openpulse-panel/src/app.rs:37` (`Tab`), `:173` (`send`), `ui.rs:818` (`tabbed_lower`), `:1178` (`stats_widget` table), `connection.rs:168` (`apply_event`) |
| Config section pattern | `crates/openpulse-config/src/lib.rs:104` (`LogbookConfig`), `init_template()` `:739`, template-parse guard test `:1128` |
| Regulatory | `docs/regulatory.md` (§97.119 :48–55, §97.221 :59–76, EU :110–155, segments :181), REQ IDs in `docs/dev/project/traceability-matrix.md:123–137` |

## Appendix B — JS8 constants to port (verified against JS8Call-improved `master`)

- `JS8_NUM_SYMBOLS = 79`; LDPC `N=174, K=87` (75 msg + CRC-12); submode table §2.2 (`commons.h:29–47`).
- Costas ORIGINAL `{4,2,5,6,1,3,0}×3`; MODIFIED `{0,6,2,3,5,4,1},{1,5,0,2,3,6,4},{2,5,0,6,4,1,3}` (`JS8.h:24–36`).
- Frame types / flags: `Varicode.h:24–60` (SubmodeType, TransmissionType, FrameType).
- Packers to reimplement: `packCallsign` (28b), `packGrid` (15b), `packAlphaNumeric50`, `packAlphaNumeric22`, `packCmd`, `pack72bits` (`Varicode.h:120–155`).
- Varicode Huffman table: `Varicode.cpp`; JSC dictionary: `JS8_JSC/{JSC.cpp,JSC_list.cpp,JSC_map.cpp}`.
- Calling frequencies + HB sub-band (500–1000 Hz) + ±2 s clock rule: JS8Call user guide (mirrored in §2.2/§8).
- License: JS8Call-improved is GPL-3.0; this workspace is `GPL-3.0-or-later` — port-compatible.
