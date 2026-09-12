---
project: openpulsehf
doc: docs/dev/design/handshake-binary-encoding.md
status: resolved
last_updated: 2026-09-12
---

# Binary handshake encoding (#1147)

> **Outcome (added 2026-09-12).** Shipped in `114f8e5d` (#1189, closes #1147/#1166 and lands the
> #1178 `dst_station` addressing — that issue closed 2026-09-01), then
> corrected by its adversarial review in `6dbcafe6` (#1190) and by the #1191 budget decision in
> `eb662cdd` (PR #1204). The live specification is `protocol-wire-spec.md` §3; this doc is the design
> record and is **not** updated field-for-field. Where the two differ, the spec is right:
>
> - `WIRE_VERSION` shipped at `0x02` as specified below, and was **reset to `0x01` and frozen until
>   1.0** by maintainer decision in #1204 (overriding the review's bump; rationale and cost in
>   `handshake_wire.rs` and the ledger entry of 2026-08-26). The "0x01 is rejected" rule below is
>   void until 1.0.
> - `session_id` is a fixed `u64`, not a ≤24 B string (#1190 F1 — the old cap failed open on an
>   11-character callsign). `caps::STATION_ID` is 18, not 12 (#1204). `dst_station` is **not** on the
>   CONACK (#1204; the design's 244 B budget included it). Maximal frames are **236 / 237 B**, not
>   241 / 244 — asserted in `handshake_wire.rs`, not budgeted here.
> - Unknown *offered* signing modes are skipped, not fatal; an unknown *selected* mode still fails
>   (#1190 F2).
> - The PQ determinism caveat in *Gates* is resolved: ML-DSA-44 signing is deterministic in this
>   build (`pq_signing_is_deterministic_in_this_build`); the PQ CONREQ is pinned as a digest at
>   5 049 B, the PQ CONACK cannot be pinned (randomised KEM).
> - F-1147-06 (no PQ key column in the trust store) remains deferred.
> - The "#1148 precedent not yet on `main`" note was already false when **this design** merged
>   (`59e6ff5a`, 2026-08-22 17:17): `3c710d7c` had landed at 14:40 the same day.
>
> All `file:line` anchors below refer to the tree **before** `114f8e5d`; they locate v1 code that no
> longer exists and are left as the design's evidence, not as navigation.

Design for re-laying CONREQ/CONACK — classical **and** PQ — as a binary format, in the decided
pre-1.0 wire-format break window. Reviewed adversarially twice before implementation; the
corrections are marked as corrections rather than quietly folded in, including three claims of mine
that the tree contradicted.

## Why: fragments, not seconds

A shipped CONREQ is **710 B** of payload (`references.md:375-379`, measured 2026-08-10 and
independently reproduced at 711 B; PR #1127) = **752 B on the wire** = **3 SAR fragments** (4 B SAR
header + 251 B of data per fragment, 10 B frame envelope each). That costs three preambles, three
acquisitions, and decodes with probability ≈ p³ on a fading channel — against a recorded background
where uncoded slow rungs decode ~0 on `moderate_f1`.

A binary CONREQ is **≤ 251 B by construction** (see *Caps*) = **one fragment**: one acquisition, p.

Airtime is the smaller half: 752 B ≈ **24 s** at BPSK250 (31.25 B/s) against ~8 s. The bytes go into
`pubkey`, `kex_pubkey` and `signature`, which inflate **~3.4×** as JSON number arrays
(`references.md:379`; uniform bytes cost ~3.6 chars/byte in a compact array).

**Corrected on review, twice.** The first draft led with seconds. The second still carried 646 B /
20.7 s — a figure from a *compact-JSON model I wrote myself*, not from the encoder, contradicting
every recorded measurement and the issue title, while claiming in the same paragraph to have fixed
exactly that error. The numbers above are the recorded ones.

PQ Hybrid is 17 939 B ≈ 9.6 min → ~5 kB ≈ 2.7 min. **Still not deployable**: scoping PQ into this
change buys **format completeness** — so that wiring PQ later is not a second wire break — and not a
1.0 feature.

## What gets signed

**The transmitted prefix: `magic || version || length || body`.** The signed message is exactly the
bytes on the air, minus the trailing signature. There is no second representation, so verification is
"hash what you received" and cannot drift from the encoder.

Today `canonical_bytes` (`handshake.rs:307-322`, and identically `pq_handshake.rs:254-257`) is
`serde_json::to_vec` in **serde declaration order with no key sorting** — "canonical" is a label, not
a property, and `protocol-wire-spec.md:194-195` states the opposite ("keys sorted recursively").

**Domain-separation tags — SUPERSEDED by #1193.** This section originally rejected tags because the
proposed ones (`OPHF-CONREQ-v2` …) begin with `OPHF`, the magic of `WireEnvelope`, which the same
station key already signs; it argued those contexts would then be "separated only by which byte
differs first — separation by coincidence, the very property tags were meant to remove".

The conclusion (sign the transmitted prefix) was right and still stands; **the reasoning was
muddled**, and #1193 corrected it. *All* prefix separation is byte-differs-first, tags included.
What a tag actually provides is **guaranteed** distinctness, whereas a magic provides it only if the
complete set of signed contexts is inventoried and checked. Nothing did that, and the inventory when
finally taken found **13** contexts where the issue that prompted it listed 7 — three of which
(route response, route update, file offer) begin with a peer-influenced integer and had no fixed
leading bytes at all. What a tag provides is not distinctness by itself: an ad-hoc tag scheme rots
exactly as a magic does. The guarantee comes from the registry, the distinctness check, and the
lint wall together.

What shipped: a `SigningDomain` registry, a tag on every signed message, and a `clippy.toml` wall so
an unregistered signing site cannot compile. These frames keep their magic as their tag, so they are
unchanged on the wire — the known-answer vector still matches.

## Layout rules

* **Length field**: `u16` big-endian (v1 used `u32`), which covers the classical frames and the
  ~5 kB PQ ones.
* **Fixed-size objects carry no length prefix.** Classical `pubkey`/`kex_pubkey` are 32 B and
  `signature` 64 B; ML-DSA-44 pubkey 1312, signature 2420, ML-KEM-768 ek 1184, ct 1088 — all
  compile-time constants (`pq_handshake.rs:27-37`). A length prefix on a constant-size field is a
  parser decision handed to the sender. The one genuinely variable binary field is the PQ
  `classical_signature` (0 or 64 B) — a presence flag, not a length.
* **Wrong lengths are rejected at decode**, where v1 accepted an arbitrary `Vec<u8>` and let
  verification fail late (`handshake.rs:594-608`). **Reject means the existing action**: drop and
  log, **no reply** (`lib.rs:1497-1500`). An error frame would spend RF and hand an attacker an
  oracle.
* **Short strings** are `u8`-length-prefixed with decoder-enforced caps (below).
* **Integers** are fixed-width big-endian. `profile_fingerprint` is an FNV `u64` compared for
  equality (`profile.rs:66-84`, daemon `lib.rs:1666-1668`).
* **`timestamp_ms` is mandatory.** The `0`-sentinel and `skip_serializing_if` contortions
  (`handshake.rs:143-165`) exist only to keep v1 signatures byte-identical to pre-#615 frames; this
  break discards that, so the sentinel and the legacy constructors (`create`, `create_with_grid` —
  test-only callers) go with it.
* **`signing_modes` is signed in transmitted order** and needs no canonicalisation: it is consumed as
  a set (`select_signing_mode`, `trust.rs:189-211`).

## Caps — one fragment by construction, not by example

`u8`-prefixed strings would admit a legal 300 B CONREQ, so "it fits one fragment" must be a property
of the **maximal legal frame**:

| field | cap | worst case today |
|---|---|---|
| `station_id`, `dst_station` | 12 | `DC0SK/P` = 7 |
| `session_id` | 24 | `"{callsign}-{now_ms}"` ≈ 20 (`lib.rs:1961`) |
| `station_grid` | 8 | 6 |
| `profile_name` | 24 | `hpx_pilot_fast_rrc` = 18 |
| `signing_modes` | 4 entries | 3 |

Worst-case **CONREQ** = 4+1+2 + 170 + 64 = **241 B** ≤ 251. ✅

**A consequence found by doing this arithmetic rather than assuming it:** a CONACK that also echoes
`session_id` is **269 B** worst-case and does *not* fit. So **the CONACK does not carry
`session_id`** — `conreq_hash` subsumes the echo, and both endpoints already hold the id (the
responder from the CONREQ it verified, the initiator from the CONREQ it sent) for
`derive_session_keys` (`trust.rs:237-251`). Worst-case CONACK = **244 B** ≤ 251. ✅

## Field-set changes, and why each belongs in this window

| change | rationale |
|---|---|
| **Delete `supported_compression` / `supported_fec_modes`** (#1166) | Nothing consumes the selection: the daemon sends them empty and hardcodes `None`/`None` (`lib.rs:1548-1549, 1974-1975`). Wiring them "with real values" means also building the consumer that honours the selection on session traffic — feature work smuggled into a format window, and until then the field is a capability claim the station cannot back, which is why Type C was removed in PR #948. **Decided before the layout**, because `CompressionAlgorithm::Zstd(u32)` carries a payload (`compression.rs:20-28`) that a bare `u8` discriminant cannot encode: deleting dissolves the problem instead of specifying around it. **Deletion scope**: the CONREQ fields, the CONACK's `selected_compression`/`selected_fec_mode`, `verify_conack`'s two list parameters and its membership checks at `handshake.rs:703-713`, `HandshakeError::UnsupportedCompression`/`UnsupportedFecMode`, and spec §3.1/§3.2 rows |
| **Add `dst_station`** (#1178) | A CONREQ has no destination, so `handle_inbound_conreq` (`lib.rs:1484-1570`) has nothing to check and **every daemon in range answers**. The initiator filters afterwards (`lib.rs:1608`), but the RF is already spent by every listener. Refusing to key up *unidentified* is already policy (F6, §97.119); *unaddressed* is the sibling gap. Wildcard is the explicit token `"*"`; **empty is invalid**, so "unaddressed" cannot be spelled by omission |
| **Add `conreq_hash` (32 B) to the CONACK** | Transcript binding: **SHA-256 over the complete transmitted CONREQ frame, including its signature**. The daemon concedes the session id is "cleartext and time-based (guessable within the handshake window)" (`lib.rs:1601-1604`); binding to the exact frame — including the initiator's `kex_pubkey` — closes mix-and-match shapes generically |
| **PQ bodies gain `timestamp_ms`**, and `verify_pq_conreq`/`verify_pq_conack` gain a `Freshness` parameter (the daemon's ±120 s) | `PqConReqBody`/`PqConAckBody` (`pq_handshake.rs:76-93`) have **no timestamp at all** — no replay freshness, the hole the classical path closed. Freezing that field-for-field would bake a known defect into the format and void the stated reason for scoping PQ in |
| **PQ frames gain magics and a daemon dispatch arm** | `encode_pq_conreq` is bare JSON (`pq_handshake.rs:488-490`) and the daemon routes by magic sniff (`lib.rs:1470-1472`, `HSCQ`/`HSAK` only), so a PQ frame would be silently dropped even if something sent it |
| **Add the missing signing-mode membership check** | `verify_conack` evaluates `&[ack.selected_mode]` against local policy only (`handshake.rs:734`) and never checks it against the modes the CONREQ offered. The PQ path does (`pq_handshake.rs:444-445`, `UnauthorizedMode`) and `protocol-wire-spec.md:169` claims the classical path does. **Correction:** an earlier draft of this doc said the *compression/FEC* check was the missing one. It is not — that check exists at `handshake.rs:703-713` and the spec is right about it |

**Deferred, recorded rather than skipped:** the trust store has no PQ key column, so `bind_frame_key`
covers only the classical pubkey on the PQ path.

## Versioning

Version byte goes to **`0x02`**; `0x01` is rejected (drop, no reply). No dual decode — there is no
compatibility mode for the data plane, and inventing one for the handshake alone would be the
"legacy keystream" mistake in a different file.

## API, and the tests that must change shape

`verify_conreq`/`verify_conack` **take the frame bytes** (not a decoded struct). Feasible: the daemon
is the only production caller of the classical codec and holds the raw reassembled bytes at both
verify sites (`lib.rs:1491, 1583`); the PQ path has **zero** production callers; nothing persists a
handshake for later re-verification (`record_verified_peer` stores callsign, grid, pubkey and the
peer's profile name/fingerprint — `lib.rs:1654-1668` — never the frame); and `pki-tooling`'s sorted
canonical JSON is a disjoint system (it does not depend on `openpulse-core` at all).

**Every existing tamper test mutates struct fields and re-verifies** (`handshake_integration.rs`,
`compression_integration.rs`, the daemon's `poison_fragment_does_not_block_conreq_verification`).
Under a sign-the-bytes format those go vacuous. Each becomes a **byte**-tamper test and is watched
failing once before it is trusted.

## Gates

| objective | gate |
|---|---|
| The signed span is the transmitted prefix | one tamper test per mutable region — magic, version, length, each body field — mutating **bytes**, each watched failing |
| A frame cannot verify as another type or version | a CONACK's bytes offered to `verify_conreq` fails; a v2 frame with the version byte set to `0x03` fails |
| The format is frozen, not merely well-formed | a **known-answer vector** for one fully-specified CONREQ (fixed seed, fixed timestamp) in the wire spec. It pins **encoder output** — `magic‖version‖length‖body‖signature`, **pre-SAR and pre-whitening** — so #1148's keystream change cannot invalidate it. Ed25519 signing is deterministic (RFC 8032; `ed25519-dalek 2.0`, whose `rand_core` feature gates keygen only), so the signature pins too. **PQ caveat:** ML-KEM `encapsulate()` is randomised (`pq_handshake.rs:302`), so a PQ KAT pins the encoder given fixed field values, and a signing KAT only if the `ml-dsa` build is deterministic rather than hedged — verify before promising one |
| One fragment, by construction | encode the **maximal legal** frame (every string at its cap, 4 signing modes) and assert ≤ 251 B — not one example frame |
| An unaddressed CONREQ is not answered | through the **daemon's TX path** (a transmit counter or the twin harness's bridge sample count), so it cannot be satisfied by a unit check on a filter function (#1178) |
| A CONACK is bound to its CONREQ | a CONACK carrying another CONREQ's hash fails verification |
| A CONACK cannot select an unoffered mode | a CONACK whose `selected_mode` is absent from the CONREQ's list fails (new check, watched failing) |
| PQ replay freshness exists at all | a stale PQ frame is rejected, and one with no timestamp cannot be constructed |

## Doc sweep

`protocol-wire-spec.md`: §3.3's "keys sorted recursively" (false today), §3.1's table omitting
`profile_name`/`profile_fingerprint`, §3.2's `selected_mode` claim at `:169` (the check does not
exist — being added here), the "~530 B" figure at `:140`, and the §3/§4 JSON layouts. Also
`docs/features.md`, `docs/openpulse-book.md`, `docs/dev/design/architecture.md`, `traceability.md`,
the stale "~500 B" comment at `lib.rs:1422`, and `references.md:373-388` (mark superseded, do not
delete; reconcile against the 710/752 B figures used here).

**Note on the #1148 precedent**: the KAT pattern this doc copies lives on `fix/1148-whitener-period`
and is **not yet on `main`** — if this lands first, the reference is forward-looking.

## Findings ledger

| ID | Finding | State |
|---|---|---|
| F-1147-10 | Design overtaken in five particulars by #1190/#1204 without the doc moving, and edited by #1193 (`f5a43514`) without its date moving, so `last_updated` was already stale from 2026-08-24 while the status still said "not yet implemented" for shipped work. | fixed — outcome note above |
| F-1147-09 | A CONACK that echoes `session_id` is 269 B worst-case and does not fit one SAR fragment. | fixed — the echo is dropped; `conreq_hash` subsumes it |
| F-1147-08 | This doc claimed the CONACK compression/FEC membership check was missing. It exists (`handshake.rs:703-713`); the **signing-mode** check is the missing one. | fixed — corrected here |
| F-1147-07 | This doc carried 646 B / 20.7 s from a compact-JSON model of my own, contradicting the recorded 710 B while claiming to have corrected exactly that error. | fixed — recorded figures used |
| F-1147-06 | The trust store has no PQ key column; `bind_frame_key` covers only the classical pubkey on the PQ path. | deferred — recorded |
| F-1147-05 | `verify_conack` does not check `selected_mode` against the offered modes, though the PQ path does and the spec claims it does. | fixed in this change |
| F-1147-04 | PQ bodies carry no `timestamp_ms`, so the PQ path has no replay freshness. | fixed in this change |
| F-1147-03 | A CONREQ has no destination field, so every daemon in range answers one. | fixed in this change (#1178) |
| F-1147-02 | Proposed domain tags began with `OPHF`, colliding with `WireEnvelope`'s magic under the same signing key. | fixed — sign the transmitted prefix instead |
| F-1147-01 | The motivation was stated in seconds and computed on the payload; the real win is 3 SAR fragments → 1. | fixed — restated |
