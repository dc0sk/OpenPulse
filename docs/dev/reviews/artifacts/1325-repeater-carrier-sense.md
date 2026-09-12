---
project: openpulsehf
doc: docs/dev/reviews/artifacts/1325-repeater-carrier-sense.md
status: review
last_updated: 2026-09-09
---

# #1325 — the repeater keys rig_b with no carrier sense on rig_b's band

## The established facts

1. `relay_burst_at` keys rig_b whenever a burst decodes on the input side. Nothing checks rig_b's
   band. `grep -rniE 'dcd|csma|carrier.sense|channel_busy' crates/openpulse-repeater/src` returns one
   comment, about the *daemon's receive-side* DCD — a different band by definition. (**The `-E` is
   load-bearing and I originally omitted it.** Without it the alternation is literal and the command
   returns 0 — including against `engine.rs`, which contains 100 matches. I published a zero from a
   filter that finds nothing, as evidence, in a note citing this repo's own rule against exactly
   that. The conclusion happened to be right; the evidence for it was not.)

2. **A CSMA mechanism exists and would be an UNREACHABLE GUARD here.** `ModemEngine::enable_csma`
   makes `transmit` consult `self.dcd`, but `update_dcd_at_seam` has exactly one caller —
   `route_audio_stage(PipelineStage::InputCapture)` (engine.rs:7210). Since #1308 *neither* repeater
   engine captures: `engine_tx` only transmits, and `engine_rx` decodes bursts the daemon already
   passed through its own seam. So `dcd.is_busy()` on either engine is never updated.

   **Measured, not read** (throwaway probe, 2026-09-09): an engine that never captures reports
   `busy=false energy=0` permanently, and with CSMA enabled 20 transmit attempts gave **ok=4,
   refused=16**. Every refusal was the 0.3 p-persistence dice; not one was carrier sense. So
   "fail-open guard" understates it — but so did my first correction. I measured the ENGINE and
   inferred a 70 % drop rate; the repeater's error handling makes it far worse. `relay_burst_at`
   maps `ChannelBusy` into `RepeaterError::Modem` and `?`-returns it, `run_full_duplex` does
   `Err(e) => break Err(e)`, and the thread owns the repeater — so the **first** refused relay ends
   the session for good. Measured (probe, 8 decodable bursts queued, CSMA on):
   `outcome=Err(Modem("channel busy: CSMA deferred transmission"))` — seven bursts never relayed.
   Expected lifetime ≈ 1.4 bursts. So enabling CSMA would not throttle the repeater, it would
   **kill it on its first unlucky dice roll**, while providing zero interference protection.

3. **There is no audio path from rig_b's receiver into anything the repeater holds.** rig_b is a PTT
   controller (`build_ptt_controller`), not an audio device. The tx engine's audio device is not even
   configurable yet — that is #1308 PR 3 (`[repeater] tx_device`).

## The question I want tested first, because it may dissolve the issue

**Real repeaters do not carrier-sense their output, and for a coordinated pair doing so would be
wrong.** A coordinated repeater owns its output frequency; a station transmitting there is the
anomaly, and a repeater that fell silent because someone was on its output would be broken, not
polite. Standard hardware repeater behaviour is to key on input activity, full stop.

So the defect is real only for the *uncoordinated* deployment — a personal relay/digipeater on
ordinary shared frequencies, which is what this feature actually is today (no coordination story
anywhere in the config, the docs, or `docs/regulatory.md`). If that is right, the answer is not
"add carrier sense" but "**make it configurable, defaulting to sense**", because an operator without
a coordinated pair is the common case and is the one who can cause interference.

I may be wrong about which deployment this feature targets. That is the first thing to check, and it
decides everything below.

## Options, if sensing is wanted

- **A. CAT S-meter on rig_b.** A second rigctld connection to `rig_b.rigctld_addr`, reading
  `\get_level STRENGTH` before keying. Needs no audio device and works when rig_b's audio is
  TX-only. Only possible when `rig_b.backend == "rigctld"` — `rts`/`dtr`/`cm108`/`gpio` have no CAT
  channel, so those configurations cannot sense this way at all.
- **B. Audio capture on rig_b's device.** Blocked on #1308 PR 3 (`tx_device` does not exist), needs
  rig_b's receive audio physically wired, and must respect #1007 (one capture stream per device) —
  an operator pointing `tx_device` at the main rig's card would be a second stream on one device.
- **C. Policy on top of either:** refuse to autostart when no sense path is available, with an
  explicit opt-out for the coordinated-pair case.
- **D. Deployment constraint only** — document it, no code.

**My recommendation, held loosely:** A for the mechanism (it is the only one that works on a
TX-audio-only rig, which is the normal cross-band wiring), with a config field that defaults to
sensing and can be turned off for a coordinated pair. Fail-**open** on a CAT read error, logged
loudly — fail-closed would make the repeater die silently on any rig whose hamlib backend does not
implement `STRENGTH`, which is a large fraction of them, and a repeater that stops relaying is also
a service failure.

I am least sure about that last sentence. Fail-open on an unattended transmitter is exactly the
shape this repo keeps getting burned by.

## Consumer

`crates/openpulse-repeater/src/lib.rs:112` `relay_burst_at` — the only site that keys rig_b, called
from `relay_burst` ← `run_full_duplex` ← the `spawn_repeater` thread
(`crates/openpulse-daemon/src/lib.rs`). Confirmed by `grep -rn 'assert_ptt\|PttKeyGuard'
crates/openpulse-repeater/src`: keying happens in that one function.

## Prior art

- **`QsyScanner::scan` (`crates/openpulse-qsy/src/scanner.rs:44`) already does CAT-based carrier
  sense**: tune, dwell, `get_signal_strength()`, restore. So the S-meter path is live production
  code in this workspace, not a new dependency — and it returns dB, so a threshold is expressible.
- **The daemon's meter poll (`server.rs:534`) already opens a SECOND rigctld connection** to the same
  rig, with the stated reason "the separate connection means it never contends with the PTT/frequency
  command path". Option A is that pattern applied to rig_b.
- `ModemEngine::enable_csma` / `csma_check` — exists, and per fact 2 is not usable here. Worth
  recording as a rejected option so the next person does not reach for it.
- `openpulse-mesh` was **removed** rather than guarded, and "no carrier sense" was one of four stated
  reasons (`crates/openpulse-mesh/tests/no_real_audio.rs`). That is the precedent for treating this
  as blocking rather than cosmetic — though mesh lacked the other three too.

## Twins

- **The daemon's own transmit paths.** `server.rs:1653` and `2747` already call
  `engine.is_channel_busy()`, and that engine DOES capture, so those are live. The repeater is the
  odd one out — the twin that looks the same and is not.
- **ARDOP and KISS front-ends**: both transmit on operator/host command rather than automatically,
  so the unattended-doubling argument does not reach them. Neither builds a repeater
  (`grep -rn 'CrossBandRepeater'` → daemon + repeater crate only).
- **The JS8 discovery beacon** is the true twin — the other §97.221 automatic transmitter. It is
  DCD-gated (`CLAUDE.md`: "off-by-default behind `[discovery] mode` + a callsign + ±2 s
  clock-skew/DCD/self-ID gates"), and it transmits on the *same* rig the daemon is listening on, so
  its DCD is fed. That asymmetry is exactly why the repeater's case is hard: it is the only
  automatic transmitter here that keys a rig the daemon does not hear.

## Review outcome (Fable, 2026-09-09) — my mechanism was wrong; the issue stands

**The issue does NOT dissolve, and my repeater-practice argument was right about the wrong reference
class.** My claim holds for §97.205 FM repeaters (duplex, coordinated pair, key on input COS). This
feature is not one, by the project's own documents: `docs/regulatory.md:83` files it as a §97.221
automatically controlled digital station / relay node, its recommended segment is the *shared* 30 m
automatic window (`:231` — "coordination with other automatic stations on the frequency is
expected", i.e. coexistence, not ownership), `README.md:178` calls it a digipeater, and the mechanism
is decode-then-retransmit, not the simultaneous retransmission §97.3 defines a repeater by. In the US
a §97.205 repeater cannot exist below 29.5 MHz at all, so the coordinated-pair category is not even
available here. Decisively: **`RigConfig` has no frequency field** — nothing in the config names
rig_b's output frequency, let alone a pair. The right analogue is the cross-port packet digipeater,
which *does* carrier-sense its transmit port. The practice argument points the other way once the
class is right.

**Option A (CAT S-meter) is disqualified as the primary mechanism.** With `full_duplex = true` the
key is held across frames, and an S-meter read while the rig is keyed returns TX-meter garbage — so
after the first frame A would read as sensing and be incapable of firing, which is precisely the
archetype fact 2 condemns. Beyond that: `STRENGTH` is dB **relative to S9**, not dBm (the
`rig_controller.rs` docstring is wrong), band noise alone reads S5–S7 on 30 m, so a fixed threshold
is not expressible and a floor-relative one is a new DSP-path constant needing its own review; and
`l STRENGTH` reads `VFO_CURR`, while nothing sets or checks rig_b's frequency or split — "rig_b is
on the output frequency" is an operator assumption, not a controlled fact.

**Adopted mechanism — E, which I missed entirely.** `engine_tx` is not PTT-only: it is a full
`ModemEngine` over a real audio backend with both `open_input` and `open_output`, and
`default_device` names the device for *both*. So #1308 PR 3's `[repeater] tx_device` +
`set_default_device` also names a **capture** device on rig_b's card, for free. A short capture
routed through the existing `route_audio_stage(InputCapture)` seam yields the calibrated
`NoiseFloorTracker` + DCD verdict the rest of the receiver already trusts — no second sensor, no new
threshold constant, no full-duplex hole. #1007 is satisfied because rig_b's card is a distinct
device, and `tx_device == audio.device` must be refused the way rig_b's rigctld address collision
already is.

**Failure policy — fail-open was wrong, two tiers instead.** (a) *Capability absent* is a
**config-time** decision: probe once at enable; if there is no sense path the repeater refuses to
start unless the operator writes `carrier_sense = "off"`, which is then logged as their statement.
The "it would die silently" fear is false — #1298 already made a stopped repeater emit
`RepeaterChanged{false}` + `CommandError`. (b) *A transient runtime failure counts as BUSY* for that
burst: drop it (the channel is lossy by design), count it, emit an event; N consecutive failures exit
with a reason. Fail-open at runtime would recreate fact 2's shape on exactly the rigs where sensing
is most likely missing. Precedent both ways: mesh was **removed** for lacking carrier sense, and the
maintainer's #1285 ruling makes an unusable PTT backend refuse to start rather than degrade.

**Sequencing:** ship after #1308 PR 3, not before. A is mechanically independent of PR 3, but
choosing it first forks the mechanism — after PR 3, E costs almost nothing and reuses the sense the
receiver already trusts. Keep A only as a fallback for a station whose rig_b RX audio is genuinely
not wired, and **measure that such a station exists first.**

### Corrections to my own framing

- **Twins was wrong.** `server.rs:2747` is inside `#[cfg(test)]`; `:1653` is the discovery beacon —
  the path I separately named as the true twin. `grep csma crates/openpulse-daemon/src` returns
  nothing: the daemon never enables CSMA, so its OTA send, handshake, QSY lines, station ID and
  **`RelayForwarder`** (also §97.221 at `regulatory.md:83`) all transmit unsensed. The repeater is
  not "the odd one out". The relay-forward is the sharper twin, because there the DCD *is* fed and
  `is_channel_busy()` is one line.
- **Fact 3 conflated PTT with audio.** What `engine_tx` lacks is a device name and an `open_input`
  call, not an audio path. That error is what hid option E from me.
- **"TX-audio-only is the normal cross-band wiring" was UNCHECKED and is probably false** — the
  USB-codec rigs in this project's own records (IC-9700, FT-991A, IC-7300) carry both directions on
  one card. That premise is what made A look like "the only mechanism that works".
- **Prior art overstated.** `QsyScanner` is consumed only by the CLI `qsy` command, not the daemon;
  and "returns dB so a threshold is expressible" hid the dB-rel-S9 unit and the floor-relative
  problem.
- **"A large fraction of rigs lack STRENGTH" is UNCHECKED** by either of us — hamlib is not installed
  on this host.
