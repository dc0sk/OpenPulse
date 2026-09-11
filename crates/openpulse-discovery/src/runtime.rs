//! Discovery runtime orchestrator (RX-only MVP): ties the [`DiscoverySm`], [`Js8Clock`],
//! [`SlotTracker`], dwell audio buffer, JS8 [`decode_window`], and [`StationTable`] together.
//!
//! Pure and async-free — the daemon feeds it captured audio + the idle predicate and executes the
//! returned [`DiscoveryOutcome`]s (retune to the JS8 calling frequency, restore home). On each UTC
//! slot boundary while dwelling it decodes the buffered slot, upserts every heard station, and emits
//! `StationHeard`. This keeps the daemon glue (async loop, CAT retune, event plumbing) thin.

use std::collections::{BTreeMap, VecDeque};

use js8_plugin::beacon::{directed, frame_audio, heartbeat, opulse_hint, BeaconFrame};
use js8_plugin::decoder::{decode_window, DecodeCfg, Js8Decode};
use js8_plugin::grammar::{parse_heartbeat, unpack_compound_frame};
use js8_plugin::submode::{params, Submode};

use crate::discovery_sm::{DiscoveryAction, DiscoveryEvent, DiscoverySm, DiscoveryState};
use crate::hint::{encode_hint, HintPayload, HINT_VERSION};
use crate::hint_assembler::HintAssembler;
use crate::rendezvous::{
    respond, RejectReason, RendezvousInitiator, RendezvousMsg, RendezvousOutcome,
    DEFAULT_SWITCH_SLOTS,
};
use crate::rendezvous_assembler::RendezvousAssembler;
use crate::scheduler::{Js8Clock, SlotTracker};
use crate::station::{Observation, OphfHint, StationTable};

/// Beacon-TX policy (plan §8). `RxOnly` transmits nothing; `Beacon`/`Full` opt into heartbeat + hint TX.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TxMode {
    /// No transmission at all (the default).
    #[default]
    RxOnly,
    /// Periodic `@HB` heartbeats + `@OPULSE` capability hints.
    Beacon,
    /// Beacon plus (future) directed queries + rendezvous responder.
    Full,
}

/// The JS8 mode label used for the regulatory TX log.
fn js8_mode_label(submode: Submode) -> String {
    format!("JS8-{submode:?}").to_uppercase()
}

/// Audio-offset tolerance (Hz) for bucketing an over's frames — just under one NORMAL tone spacing.
const HINT_FREQ_TOL_HZ: f32 = 6.0;
/// Slots to keep an incomplete over before evicting it (a NORMAL beacon is ~4 frames/slots).
const HINT_MAX_OVER_SLOTS: u64 = 6;

/// Runtime parameters resolved from `[discovery]` config (plan §8) plus the resolved dwell frequency.
#[derive(Debug, Clone)]
pub struct DiscoveryParams {
    /// Discovery master switch.
    pub enabled: bool,
    /// Idle predicate hold time before activating (ms).
    pub idle_grace_ms: u64,
    /// Maximum dwell before returning home (ms; 0 = until preempted).
    pub dwell_ms: u64,
    /// Station-table TTL (ms).
    pub station_ttl_ms: u64,
    /// Calling submode (MVP: NORMAL).
    pub submode: Submode,
    /// JS8 calling frequency for the current band (Hz).
    pub calling_freq_hz: u64,
    /// Beacon-TX policy (default `RxOnly` — no transmission).
    pub tx_mode: TxMode,
    /// Station callsign for beacons (empty disables TX).
    pub callsign: String,
    /// Station grid for beacons.
    pub grid: String,
    /// Capability hint to advertise; `None` = heartbeat-only.
    pub hint: Option<HintPayload>,
    /// Transmit a beacon every N slots (`N × 15 s` for NORMAL; default 8 ≈ 2 min).
    pub heartbeat_interval_slots: u64,
    /// Send the `@OPULSE` hint on every Nth beacon (0 = never; else heartbeat between).
    pub hint_interval_beacons: u64,
    /// Audio offset (Hz) to transmit beacons at.
    pub tx_offset_hz: f32,
    /// Hard TX-refusal clock-skew bound (ms); JS8's published ±2 s.
    pub max_clock_skew_ms: u64,
}

/// Side effects the daemon executes / events it forwards.
#[derive(Debug, Clone, PartialEq)]
pub enum DiscoveryOutcome {
    /// Save the home frequency/mode and tune to this JS8 calling frequency.
    Retune { dial_freq_hz: u64 },
    /// Restore the saved home frequency/mode.
    RestoreHome,
    /// Lifecycle state changed.
    StateChanged(DiscoveryState),
    /// A station was heard (new or updated) this slot.
    StationHeard {
        /// Sender callsign.
        callsign: String,
        /// Grid if advertised.
        grid: Option<String>,
        /// Whether this was the first time we heard it.
        is_new: bool,
    },
    /// Transmit this pre-built beacon-frame audio (the daemon does the final DCD check + PTT wrap and
    /// calls `engine.transmit_raw_audio`). Only emitted when `tx_mode != RxOnly` and the clock is in
    /// skew tolerance.
    TransmitBeacon {
        /// Baseband GFSK audio for one 79-symbol JS8 frame.
        audio: Vec<f32>,
        /// Regulatory-log mode label (e.g. `"JS8-NORMAL"`).
        mode: String,
    },
    /// A rendezvous exchange with `peer` succeeded: both stations QSY to channel **index** `channel`
    /// (into the current band's `rendezvous_channels_hz` table) in `switch_in_slots` slots, after which
    /// the daemon runs the signed CONREQ handshake. The daemon resolves the index to Hz.
    RendezvousAgreed {
        /// The peer callsign.
        peer: String,
        /// Agreed channel index into the per-band table.
        channel: u8,
        /// Slots until both stations retune.
        switch_in_slots: u8,
    },
    /// The peer declined our rendezvous proposal.
    RendezvousRejected {
        /// The peer callsign.
        peer: String,
        /// Why it declined.
        reason: RejectReason,
    },
    /// Our rendezvous proposal expired with no reply.
    RendezvousTimedOut {
        /// The peer callsign.
        peer: String,
    },
}

/// Minimum gap between rendezvous replies to the *same* peer.
///
/// Answering a `Propose` costs a full JS8 over on the air. Without a floor, a peer that repeats its
/// proposal — malfunctioning, or deliberately — drives an unattended transmitter continuously, and
/// the rendezvous path is reachable with no authentication (the post-QSY CONREQ is the auth gate, so
/// it comes *after* this). Two minutes still admits a legitimate retry when our Accept was lost,
/// while capping what one peer can extract (audit 2026-07-19, #5).
pub const RESPONDER_COOLDOWN_MS: u64 = 120_000;

/// Cap on tracked peers for the cooldown. The key is a peer-supplied callsign, so an unbounded map is
/// itself a memory-exhaustion vector — the guard has to bound the thing that actually grows.
const MAX_TRACKED_RESPONDERS: usize = 512;

/// The RX-only discovery runtime.
pub struct DiscoveryRuntime {
    params: DiscoveryParams,
    sm: DiscoverySm,
    clock: Js8Clock,
    slots: SlotTracker,
    table: StationTable,
    /// Cross-slot assembler recognising `@OPULSE` capability beacons.
    assembler: HintAssembler,
    /// Cross-slot assembler recognising rendezvous overs directed at us.
    rendezvous_assembler: RendezvousAssembler,
    /// Rendezvous channel indices usable here (responder's `available` set; the daemon supplies them
    /// from the current band's table). Empty ⇒ every proposal is rejected `NoCommonFreq`.
    rendezvous_channels: Vec<u8>,
    /// Active outgoing rendezvous proposal awaiting a reply, if any.
    initiator: Option<RendezvousInitiator>,
    /// Directed rendezvous frames pending transmission (priority over beacons, no cadence gate).
    rendezvous_tx: VecDeque<BeaconFrame>,
    /// When we last transmitted a rendezvous reply to each peer, for the responder cooldown.
    /// Bounded by `MAX_TRACKED_RESPONDERS`: the key is a peer-supplied callsign, so an unbounded map
    /// would itself be the memory-exhaustion vector this cooldown exists to close.
    last_response_ms: BTreeMap<String, u64>,
    /// A responder agreement `(peer, channel, switch_in_slots)` withheld until our Accept over has fully
    /// transmitted — so the daemon schedules the responder's QSY *after* the Accept is sent (aligned with
    /// the initiator's decode) instead of preempting and truncating the Accept's last frame.
    pending_responder_agreement: Option<(String, u8, u8)>,
    /// Audio accumulated since the last slot boundary (only while dwelling).
    dwell_buf: Vec<f32>,
    /// Frames of the beacon over currently being transmitted, one per slot.
    beacon_queue: VecDeque<BeaconFrame>,
    /// Slots elapsed since the last beacon started (heartbeat cadence).
    slots_since_beacon: u64,
    /// Beacons started so far (hint cadence).
    beacons_sent: u64,
}

impl DiscoveryRuntime {
    /// Build a runtime for `params`.
    pub fn new(params: DiscoveryParams) -> Self {
        let sm = DiscoverySm::new(params.enabled, params.idle_grace_ms, params.dwell_ms);
        let clock = Js8Clock::new(params.submode);
        let rendezvous_assembler =
            RendezvousAssembler::new(&params.callsign, HINT_FREQ_TOL_HZ, HINT_MAX_OVER_SLOTS);
        Self {
            params,
            sm,
            clock,
            slots: SlotTracker::new(),
            table: StationTable::new(),
            assembler: HintAssembler::new(HINT_FREQ_TOL_HZ, HINT_MAX_OVER_SLOTS),
            rendezvous_assembler,
            rendezvous_channels: Vec::new(),
            initiator: None,
            rendezvous_tx: VecDeque::new(),
            last_response_ms: BTreeMap::new(),
            pending_responder_agreement: None,
            dwell_buf: Vec::new(),
            beacon_queue: VecDeque::new(),
            slots_since_beacon: 0,
            beacons_sent: 0,
        }
    }

    /// Current lifecycle state.
    pub fn state(&self) -> DiscoveryState {
        self.sm.state()
    }

    /// Enable/disable discovery at runtime (the operator `EnableDiscovery`/`DisableDiscovery` commands).
    /// Disabling while dwelling stands the machine down.
    pub fn set_enabled(&mut self, on: bool) -> Vec<DiscoveryOutcome> {
        let actions = self.sm.set_enabled(on);
        self.run_actions(actions, 0)
    }

    /// JS8 calling frequency this runtime dwells on (Hz).
    pub fn dial_freq_hz(&self) -> u64 {
        self.params.calling_freq_hz
    }

    /// Set the JS8 calling frequency to dwell on (Hz). The daemon updates this from the operator's
    /// current home band before each activation, so discovery tunes to the band-appropriate calling
    /// channel; it takes effect on the next `Retune` outcome.
    pub fn set_dial_freq_hz(&mut self, hz: u64) {
        self.params.calling_freq_hz = hz;
    }

    /// Set the rendezvous channel indices usable here (the responder's `available` set). The daemon
    /// supplies `0..n` for the current band's `rendezvous_channels_hz` table; empty rejects proposals.
    pub fn set_rendezvous_channels(&mut self, channels: Vec<u8>) {
        self.rendezvous_channels = channels;
    }

    /// Begin a rendezvous with `peer`: propose `channels` (ranked indices) under session `token`,
    /// queueing the `Propose` directed over for transmission. `timeout_slots` bounds the wait for a
    /// reply. Requires a configured callsign (the frames carry it); a no-op otherwise. Replaces any
    /// in-flight proposal.
    pub fn start_rendezvous(
        &mut self,
        peer: &str,
        token: &str,
        channels: Vec<u8>,
        timeout_slots: u64,
    ) {
        // Rendezvous requires transmitting a Propose — refuse in RX-only (the documented zero-TX default),
        // matching the responder gate, so a stray command can't break the RxOnly invariant.
        if self.params.callsign.trim().is_empty() || self.params.tx_mode == TxMode::RxOnly {
            return;
        }
        let (init, propose) = RendezvousInitiator::start(peer, token, channels, timeout_slots);
        self.enqueue_directed(init.peer(), &propose);
        self.initiator = Some(init);
    }

    /// Whether an outgoing rendezvous proposal is awaiting a reply.
    pub fn rendezvous_active(&self) -> bool {
        self.initiator.is_some()
    }

    /// Record that we answered `peer` at `now_ms`, evicting the oldest entry when full.
    ///
    /// Eviction is by oldest response, so a flood of fresh spoofed callsigns cannot push out the
    /// entry for a peer we are actively rate-limiting without first ageing past it.
    fn note_response(&mut self, peer: String, now_ms: u64) {
        if self.last_response_ms.len() >= MAX_TRACKED_RESPONDERS
            && !self.last_response_ms.contains_key(&peer)
        {
            if let Some(oldest) = self
                .last_response_ms
                .iter()
                .min_by_key(|(_, &t)| t)
                .map(|(k, _)| k.clone())
            {
                self.last_response_ms.remove(&oldest);
            }
        }
        self.last_response_ms.insert(peer, now_ms);
    }

    /// Queue the frames of `sender: to <msg>` for transmission (priority over beacons).
    fn enqueue_directed(&mut self, to: &str, msg: &RendezvousMsg) {
        let frames = directed(&self.params.callsign, &self.params.grid, to, &msg.encode());
        self.rendezvous_tx.extend(frames);
    }

    /// Current UTC clock drift-bias estimate (ms).
    pub fn drift_bias_ms(&self) -> i64 {
        self.clock.drift_bias_ms()
    }

    /// The discovered-station table (the panel's source of truth).
    pub fn stations(&self) -> &StationTable {
        &self.table
    }

    /// Clock, for drift-bias updates from decode `dt`s.
    /// No caller (#1092): test seam for advancing the slot clock without waiting on wall time.
    /// Verification objective, not a product requirement.
    pub fn clock_mut(&mut self) -> &mut Js8Clock {
        &mut self.clock
    }

    /// Append captured audio (buffered only while dwelling).
    pub fn push_audio(&mut self, samples: &[f32]) {
        if self.sm.state() == DiscoveryState::Dwelling {
            self.dwell_buf.extend_from_slice(samples);
        }
    }

    /// Report the result of a requested retune.
    pub fn qsy_complete(&mut self, ok: bool) -> Vec<DiscoveryOutcome> {
        let actions = self.sm.step(DiscoveryEvent::QsyComplete { ok });
        self.run_actions(actions, 0)
    }

    /// An operator command needs the modem — stand down.
    pub fn preempt(&mut self) -> Vec<DiscoveryOutcome> {
        let actions = self.sm.step(DiscoveryEvent::Preempt);
        self.run_actions(actions, 0)
    }

    /// One scheduler tick: feed the idle predicate + clock state, advance slots, and (while dwelling)
    /// decode a completed slot. `now_ms` is UTC epoch millis.
    pub fn tick(&mut self, now_ms: u64, idle: bool) -> Vec<DiscoveryOutcome> {
        let clock_ok = self.clock.tx_allowed(u64::MAX); // RX gate is always open; TX skew is Phase E
        let actions = self.sm.step(DiscoveryEvent::Tick {
            idle,
            clock_ok,
            now_ms,
        });
        let mut out = self.run_actions(actions, now_ms);

        // On a UTC slot boundary while dwelling: transmit a beacon frame if it's our slot, else decode
        // the slot we just buffered (half-duplex — a TX slot skips RX).
        if self.sm.state() == DiscoveryState::Dwelling {
            if let Some(_completed) = self.slots.advance(self.clock.slot_index(now_ms)) {
                // Our own directed over (Propose or Accept) has fully transmitted once the queue empties.
                if self.rendezvous_tx.is_empty() {
                    // Responder: surface the withheld agreement now that the Accept is on the air, so the
                    // daemon's QSY schedule starts here (aligned with the initiator's Accept decode)
                    // instead of preempting the Accept's final frame.
                    if let Some((peer, channel, switch_in_slots)) =
                        self.pending_responder_agreement.take()
                    {
                        out.push(DiscoveryOutcome::RendezvousAgreed {
                            peer,
                            channel,
                            switch_in_slots,
                        });
                    }
                    // Initiator: only age the proposal while genuinely waiting for a reply (the Propose is
                    // fully sent), never during our own Propose transmission — else it times out before a
                    // well-behaved reply can possibly arrive.
                    if let Some(init) = self.initiator.as_mut() {
                        if let Some(RendezvousOutcome::TimedOut) = init.on_slot() {
                            let peer = init.peer().to_string();
                            self.initiator = None;
                            out.push(DiscoveryOutcome::RendezvousTimedOut { peer });
                        }
                    }
                }
                if let Some(tx) = self.maybe_transmit(now_ms) {
                    self.dwell_buf.clear(); // don't decode our own transmission
                    out.push(tx);
                } else {
                    let actions = self.sm.step(DiscoveryEvent::SlotElapsed { now_ms });
                    out.extend(self.run_actions(actions, now_ms));
                }
            }
        }
        out
    }

    /// Decide whether this slot transmits a beacon frame (plan §8): only when opted into TX and the
    /// clock is within skew tolerance. Continues an in-progress over, else starts one on cadence —
    /// every `hint_interval_beacons`-th beacon is an `@OPULSE` hint, the rest are heartbeats.
    fn maybe_transmit(&mut self, now_ms: u64) -> Option<DiscoveryOutcome> {
        if self.params.callsign.trim().is_empty() {
            return None;
        }
        // Hard TX refusal beyond the clock-skew bound (§D5) — degrade to RX-only for this slot.
        if !self.clock.tx_allowed(self.params.max_clock_skew_ms) {
            return None;
        }
        // RX-only transmits nothing at all — check before the rendezvous queue so the documented zero-TX
        // default can never emit a Propose/Accept frame.
        if self.params.tx_mode == TxMode::RxOnly {
            return None;
        }
        // Rendezvous frames have priority and no cadence gate: a directed exchange (operator-initiated,
        // or a responder Accept) transmits every slot until its over is complete.
        if let Some(frame) = self.rendezvous_tx.pop_front() {
            let audio = frame_audio(&frame, self.params.tx_offset_hz, self.params.submode);
            return Some(DiscoveryOutcome::TransmitBeacon {
                audio,
                mode: js8_mode_label(self.params.submode),
            });
        }

        if self.beacon_queue.is_empty() {
            self.slots_since_beacon += 1;
            if self.slots_since_beacon < self.params.heartbeat_interval_slots {
                return None;
            }
            self.slots_since_beacon = 0;
            let use_hint = self.params.hint.is_some()
                && self.params.hint_interval_beacons > 0
                && self
                    .beacons_sent
                    .is_multiple_of(self.params.hint_interval_beacons);
            let frames = if use_hint {
                let text = encode_hint(self.params.hint.as_ref().unwrap(), &self.params.callsign);
                opulse_hint(&self.params.callsign, &self.params.grid, &text)
            } else {
                heartbeat(&self.params.callsign, &self.params.grid)
            };
            self.beacons_sent = self.beacons_sent.wrapping_add(1);
            self.beacon_queue = frames.into();
            let _ = now_ms;
        }

        let frame = self.beacon_queue.pop_front()?;
        let audio = frame_audio(&frame, self.params.tx_offset_hz, self.params.submode);
        Some(DiscoveryOutcome::TransmitBeacon {
            audio,
            mode: js8_mode_label(self.params.submode),
        })
    }

    /// Translate SM actions into outcomes, performing the decode for `DecodeSlot`.
    fn run_actions(&mut self, actions: Vec<DiscoveryAction>, now_ms: u64) -> Vec<DiscoveryOutcome> {
        let mut out = Vec::new();
        for action in actions {
            match action {
                DiscoveryAction::SaveHomeAndTune => out.push(DiscoveryOutcome::Retune {
                    dial_freq_hz: self.params.calling_freq_hz,
                }),
                DiscoveryAction::RestoreHome => {
                    self.dwell_buf.clear();
                    out.push(DiscoveryOutcome::RestoreHome);
                }
                DiscoveryAction::StateChanged(s) => out.push(DiscoveryOutcome::StateChanged(s)),
                DiscoveryAction::DecodeSlot => out.extend(self.decode_slot(now_ms)),
            }
        }
        out
    }

    /// Decode the buffered slot, upsert every heard station, and emit `StationHeard`.
    fn decode_slot(&mut self, now_ms: u64) -> Vec<DiscoveryOutcome> {
        let buf = std::mem::take(&mut self.dwell_buf);
        let sm = params(self.params.submode);
        // Only decode once a full slot's audio is present.
        let sig_len = sm.samples_per_period();
        if buf.len() < sig_len {
            return Vec::new();
        }
        let slot = self.clock.slot_index(now_ms);
        // A conforming JS8 over does NOT start at buffer sample 0: it begins `start_delay_ms` into the
        // UTC slot (500 ms ≈ 4000 samples for NORMAL) and clock jitter shifts it further. Search a window
        // around the expected start rather than offset 0 (which only ever matched a test signal injected
        // at the buffer start, so real off-air transmissions never decoded at any SNR). NTP is required
        // (D5), so a ±0.75 s window covers realistic tick/clock jitter; a coarse-then-fine freq grid
        // keeps the added time search affordable on a Pi.
        let start_delay = sm.start_delay_ms as usize * 8; // 8 samples/ms at 8 kHz
        let jitter = 6000; // ±0.75 s
        let slack = buf.len() - sig_len;
        let cfg = DecodeCfg {
            min_offset: start_delay.saturating_sub(jitter),
            max_offset: (start_delay + jitter).min(slack),
            offset_step: (sm.samples_per_symbol / 2).max(1),
            base_step_coarse: sm.tone_spacing_hz * 2.0,
            ..DecodeCfg::default()
        };
        let mut out = Vec::new();
        for d in decode_window(&buf, &sm, &cfg) {
            // Fold this frame's timing error into the clock drift estimate: a conforming station starts
            // `start_delay` samples into the slot, so `dt = start_delay − observed` (ms) is our clock's
            // offset (fast ⇒ observed later ⇒ negative). This is what makes the ±2 s TX-skew gate and the
            // operator drift readout live (previously `set_drift_bias_ms` had no caller → both stuck at 0).
            let dt_ms = (start_delay as i64 - d.sample_offset as i64) / 8;
            self.clock.observe_dt_ms(dt_ms);
            // Feed every frame to the cross-slot hint assembler; a completed `@OPULSE` beacon upserts
            // the sender as an OpenPulse peer (its hint) and is reported as heard.
            if let Some(r) = self
                .assembler
                .ingest(&d.payload, d.i3bit, d.base_freq_hz, slot)
            {
                let is_new = self.table.upsert(
                    Observation {
                        callsign: r.callsign.clone(),
                        grid: r.grid.clone(),
                        snr_db: d.snr_db,
                        freq_offset_hz: r.base_freq_hz,
                        dial_freq_hz: self.params.calling_freq_hz,
                        hint: Some(OphfHint::from_payload(HINT_VERSION, &r.hint)),
                    },
                    now_ms,
                );
                out.push(DiscoveryOutcome::StationHeard {
                    callsign: r.callsign,
                    grid: r.grid,
                    is_new,
                });
            }
            // Feed every frame to the rendezvous assembler; a completed over directed at us drives the
            // initiator reply-handling or the responder decision.
            if let Some(r) =
                self.rendezvous_assembler
                    .ingest(&d.payload, d.i3bit, d.base_freq_hz, slot)
            {
                out.extend(self.handle_rendezvous(r.from, r.msg, now_ms));
            }
            if let Some(o) = self.ingest_decode(&d, now_ms) {
                out.push(o);
            }
        }
        self.table.sweep(now_ms, self.params.station_ttl_ms);
        out
    }

    /// Drive a recognised rendezvous message: if it answers our active proposal, resolve the initiator
    /// (QSY / reject); otherwise, in `Full` mode, respond to an inbound `Propose` and (on accept) agree.
    fn handle_rendezvous(
        &mut self,
        from: String,
        msg: RendezvousMsg,
        now_ms: u64,
    ) -> Vec<DiscoveryOutcome> {
        // A reply to our own in-flight proposal (same peer + token)?
        if let Some(init) = self.initiator.as_mut() {
            if init.peer().eq_ignore_ascii_case(from.trim()) {
                let outcome = init.on_message(&msg);
                return match outcome {
                    Some(RendezvousOutcome::Qsy {
                        channel,
                        switch_in_slots,
                    }) => {
                        self.initiator = None;
                        vec![DiscoveryOutcome::RendezvousAgreed {
                            peer: from,
                            channel,
                            switch_in_slots,
                        }]
                    }
                    Some(RendezvousOutcome::Rejected(reason)) => {
                        self.initiator = None;
                        vec![DiscoveryOutcome::RendezvousRejected { peer: from, reason }]
                    }
                    _ => Vec::new(),
                };
            }
        }
        // Otherwise a proposal directed at us — respond only when opted into the responder role (Full).
        if self.params.tx_mode != TxMode::Full {
            return Vec::new();
        }
        if !matches!(msg, RendezvousMsg::Propose { .. }) {
            return Vec::new();
        }
        // Rate-limit per peer BEFORE deciding to transmit: replying costs a full JS8 over, and this
        // path is reachable pre-authentication.
        let key = from.trim().to_ascii_uppercase();
        if let Some(&last) = self.last_response_ms.get(&key) {
            if now_ms.saturating_sub(last) < RESPONDER_COOLDOWN_MS {
                return Vec::new();
            }
        }

        match respond(&msg, &self.rendezvous_channels, DEFAULT_SWITCH_SLOTS) {
            RendezvousOutcome::Send(reply) => {
                self.note_response(key, now_ms);
                self.enqueue_directed(&from, &reply);
                if let RendezvousMsg::Accept {
                    channel,
                    switch_in_slots,
                    ..
                } = reply
                {
                    // Withhold the agreement until the Accept over has fully transmitted (see the slot
                    // handler in `tick`), so the daemon schedules our QSY after the Accept is sent.
                    self.pending_responder_agreement = Some((from, channel, switch_in_slots));
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Turn one JS8 decode into a station upsert (heartbeats carry callsign + grid).
    fn ingest_decode(&mut self, d: &Js8Decode, now_ms: u64) -> Option<DiscoveryOutcome> {
        let hb = unpack_compound_frame(&d.payload)
            .as_ref()
            .and_then(parse_heartbeat)?;
        let obs = Observation {
            callsign: hb.callsign.clone(),
            grid: (!hb.grid.is_empty()).then_some(hb.grid.clone()),
            // Decoder's matched-filter SNR estimate (dB, 2500 Hz ref BW).
            snr_db: d.snr_db,
            freq_offset_hz: d.base_freq_hz,
            dial_freq_hz: self.params.calling_freq_hz,
            hint: None, // @OPULSE hint marking needs varicode free-text decode (a later unit)
        };
        let is_new = self.table.upsert(obs, now_ms);
        Some(DiscoveryOutcome::StationHeard {
            callsign: hb.callsign,
            grid: (!hb.grid.is_empty()).then_some(hb.grid),
            is_new,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use js8_plugin::costas::CostasKind;
    use js8_plugin::message::js8_info_bits;
    use js8_plugin::modulate::{modulate_tones, GfskParams};
    use js8_plugin::tones::message_to_tones;

    fn params() -> DiscoveryParams {
        DiscoveryParams {
            enabled: true,
            idle_grace_ms: 0,
            dwell_ms: 0,
            station_ttl_ms: 3_600_000,
            submode: Submode::Normal,
            calling_freq_hz: 14_078_000,
            tx_mode: TxMode::RxOnly,
            callsign: String::new(),
            grid: String::new(),
            hint: None,
            heartbeat_interval_slots: 8,
            hint_interval_beacons: 3,
            tx_offset_hz: 1500.0,
            max_clock_skew_ms: 2000,
        }
    }

    /// A NORMAL slot of audio with one heartbeat (KN4CRD EM73) at 1500 Hz.
    fn heartbeat_slot() -> Vec<f32> {
        // Payload from the C-2 upstream ground-truth vector.
        let payload: [u8; 9] = [0x0a, 0x2f, 0xb3, 0xa3, 0xee, 0x2e, 0xe2, 0xea, 0x58];
        let info = js8_info_bits(&payload, 0);
        let sm = js8_plugin::submode::params(Submode::Normal);
        modulate_tones(
            &message_to_tones(&info, CostasKind::Original),
            1500.0,
            &GfskParams::from_submode(&sm),
        )
    }

    /// A full NORMAL slot (120 000 samples) with the heartbeat starting `offset` samples in — as a real
    /// off-air over does (`start_delay_ms` + clock skew), not at buffer sample 0.
    fn heartbeat_slot_at(offset: usize) -> Vec<f32> {
        let sm = js8_plugin::submode::params(Submode::Normal);
        let mut buf = vec![0.0f32; offset];
        buf.extend_from_slice(&heartbeat_slot());
        buf.resize((sm.slot_secs as usize) * 8000, 0.0); // pad to a full 15 s slot
        buf
    }

    #[test]
    fn hears_a_station_that_starts_partway_into_the_slot() {
        // Regression: a conforming JS8 over begins ~500 ms into the slot, so the decoder must search the
        // timing slack, not only offset 0. Before the fix this failed at every SNR.
        let mut rt = DiscoveryRuntime::new(params());
        rt.tick(1000, true);
        rt.qsy_complete(true);
        assert_eq!(rt.state(), DiscoveryState::Dwelling);
        rt.push_audio(&heartbeat_slot_at(4000)); // 500 ms
        rt.tick(1000, true);
        let out = rt.tick(16_000, true);
        assert!(
            out.iter().any(|o| matches!(o, DiscoveryOutcome::StationHeard { callsign, .. } if callsign == "KN4CRD")),
            "heard KN4CRD despite a 500 ms slot-start offset: {out:?}"
        );
        // A station heard at exactly the expected start delay (500 ms = 4000 samples) contributes a ~0
        // timing error, so the drift estimate stays near 0 — but the observation path is now live
        // (audit #9: previously drift was permanently 0 because nothing ever called the clock).
        assert!(
            rt.drift_bias_ms().abs() < 100,
            "drift estimate is live and near zero for an on-time station, got {}",
            rt.drift_bias_ms()
        );
    }

    #[test]
    fn activates_dwells_hears_and_caches_a_station() {
        let mut rt = DiscoveryRuntime::new(params());
        // Idle → activate (idle_grace 0).
        let a = rt.tick(1000, true);
        assert!(a.contains(&DiscoveryOutcome::Retune {
            dial_freq_hz: 14_078_000
        }));
        assert_eq!(rt.state(), DiscoveryState::Activating);
        // Retune ok → dwell.
        rt.qsy_complete(true);
        assert_eq!(rt.state(), DiscoveryState::Dwelling);

        // Buffer a slot of audio, then cross a slot boundary → decode + cache.
        rt.push_audio(&heartbeat_slot());
        // First tick establishes the slot; the next slot advances the tracker.
        rt.tick(1000, true);
        let out = rt.tick(16_000, true); // next UTC slot
        assert!(
            out.iter().any(|o| matches!(o, DiscoveryOutcome::StationHeard { callsign, .. } if callsign == "KN4CRD")),
            "heard KN4CRD: {out:?}"
        );
        let s = rt.stations().get("KN4CRD").expect("station cached");
        assert_eq!(s.grid.as_deref(), Some("EM73"));
    }

    fn hex9(s: &str) -> [u8; 9] {
        let mut p = [0u8; 9];
        for (i, b) in p.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap();
        }
        p
    }

    /// One NORMAL frame of a beacon over, modulated at 1500 Hz (payload9 + its i3bit flag).
    fn beacon_frame(hex: &str, i3bit: u8) -> Vec<f32> {
        let sm = js8_plugin::submode::params(Submode::Normal);
        let info = js8_info_bits(&hex9(hex), i3bit);
        modulate_tones(
            &message_to_tones(&info, CostasKind::Original),
            1500.0,
            &GfskParams::from_submode(&sm),
        )
    }

    #[test]
    fn recognizes_an_opulse_peer_from_a_four_slot_beacon() {
        // The four Huffman-forced frames of `DC0SK: @OPULSE OPHF1 1FAX3AIT` (Qt5 ground truth), each
        // with its transmission flag. A NORMAL over sends one frame per 15 s slot.
        let frames = [
            ("2694fa766ea662ea58", 1u8), // Compound: DC0SK EM73 (First)
            ("531a90d5639ea3f5c8", 0u8), // CompoundDirected: @OPULSE
            ("bfec6491489275029b", 0u8), // Data: "OPHF1 1FAX3A"
            ("b9afffffffffffffff", 2u8), // Data (Last): "IT"
        ];

        let mut rt = DiscoveryRuntime::new(params());
        rt.tick(1000, true);
        rt.qsy_complete(true);
        assert_eq!(rt.state(), DiscoveryState::Dwelling);

        let mut heard = Vec::new();
        let mut t = 1000u64;
        for (hex, i3) in frames {
            rt.push_audio(&beacon_frame(hex, i3));
            rt.tick(t, true); // establish this slot
            t += 15_000;
            heard.extend(rt.tick(t, true)); // cross the slot boundary → decode this frame
        }

        // The sender is cached as an OpenPulse peer with its decoded capabilities.
        let s = rt.stations().get("DC0SK").expect("peer cached");
        let hint = s.hint.expect("carries an @OPULSE hint");
        assert_eq!(hint.caps, 0xB105);
        assert_eq!(hint.pref_channel, Some(42));
        assert_eq!(s.grid.as_deref(), Some("EM73"));
        assert!(
            heard
                .iter()
                .any(|o| matches!(o, DiscoveryOutcome::StationHeard { callsign, .. } if callsign == "DC0SK")),
            "emitted StationHeard for the recognized peer: {heard:?}"
        );
    }

    #[test]
    fn disabled_runtime_never_retunes() {
        let mut rt = DiscoveryRuntime::new(DiscoveryParams {
            enabled: false,
            ..params()
        });
        assert!(rt.tick(1000, true).is_empty());
        assert!(rt.tick(60_000, true).is_empty());
        assert_eq!(rt.state(), DiscoveryState::Inactive);
    }

    #[test]
    fn preempt_restores_home_and_clears_the_buffer() {
        let mut rt = DiscoveryRuntime::new(params());
        rt.tick(1000, true);
        rt.qsy_complete(true);
        rt.push_audio(&heartbeat_slot());
        let out = rt.preempt();
        assert!(out.contains(&DiscoveryOutcome::RestoreHome));
        assert_eq!(rt.state(), DiscoveryState::Inactive);
    }

    fn dwelling_beacon_runtime(tx_mode: TxMode, hb_slots: u64) -> DiscoveryRuntime {
        let mut p = params();
        p.tx_mode = tx_mode;
        p.callsign = "DC0SK".into();
        p.grid = "JN58".into();
        p.hint = None; // heartbeat-only
        p.heartbeat_interval_slots = hb_slots;
        let mut rt = DiscoveryRuntime::new(p);
        rt.tick(1000, true); // activate
        rt.qsy_complete(true); // dwell
        rt
    }

    #[test]
    fn beacon_mode_transmits_a_heartbeat_on_cadence() {
        let mut rt = dwelling_beacon_runtime(TxMode::Beacon, 2);
        let mut txs = Vec::new();
        let mut t = 1000u64;
        for _ in 0..4 {
            t += 15_000;
            txs.extend(rt.tick(t, true));
        }
        let beacon = txs
            .iter()
            .find(|o| matches!(o, DiscoveryOutcome::TransmitBeacon { .. }))
            .expect("beacon mode transmits on cadence");
        if let DiscoveryOutcome::TransmitBeacon { audio, mode } = beacon {
            assert_eq!(mode, "JS8-NORMAL");
            assert!(!audio.is_empty());
        }
    }

    #[test]
    fn rx_only_never_transmits() {
        let mut rt = dwelling_beacon_runtime(TxMode::RxOnly, 1);
        let mut t = 1000u64;
        for _ in 0..6 {
            t += 15_000;
            let out = rt.tick(t, true);
            assert!(
                !out.iter()
                    .any(|o| matches!(o, DiscoveryOutcome::TransmitBeacon { .. })),
                "rx_only must never transmit"
            );
        }
    }

    /// A dwelling runtime with a callsign + grid; beacon cadence pushed far out so it never fires during
    /// a rendezvous test.
    fn dwelling_rendezvous_runtime(tx_mode: TxMode) -> DiscoveryRuntime {
        let mut p = params();
        p.tx_mode = tx_mode;
        p.callsign = "DC0SK".into();
        p.grid = "JN58".into();
        p.heartbeat_interval_slots = 10_000;
        let mut rt = DiscoveryRuntime::new(p);
        rt.tick(1000, true);
        rt.qsy_complete(true);
        rt
    }

    /// A Propose message as it goes on the air, built through the CODEC rather than typed.
    ///
    /// These fixtures used to hold the wire text literally, so #1163's version token had to be
    /// hand-edited into thirteen of them — the same staleness #1162 hit one issue earlier. Built
    /// through `encode()`, the next format change updates every caller at once.
    fn propose_line(token: &str, channels: &[u8]) -> String {
        crate::rendezvous::RendezvousMsg::Propose {
            token: token.to_string(),
            channels: channels.to_vec(),
        }
        .encode()
    }

    /// Modulate `text` as a directed over `sender: to <text>` and clock its frames through the dwelling
    /// runtime, one per slot from `start_ms`, collecting every outcome. `t` advances by one slot/frame.
    fn feed_over(
        rt: &mut DiscoveryRuntime,
        sender: &str,
        to: &str,
        text: &str,
        t: &mut u64,
    ) -> Vec<DiscoveryOutcome> {
        let frames = directed(sender, "JN58", to, text);
        let mut out = Vec::new();
        for f in &frames {
            rt.push_audio(&frame_audio(f, 1500.0, Submode::Normal));
            rt.tick(*t, true); // establish this slot
            *t += 15_000;
            out.extend(rt.tick(*t, true)); // cross the boundary → decode
        }
        out
    }

    #[test]
    fn responder_accepts_a_proposal_and_agrees() {
        let mut rt = dwelling_rendezvous_runtime(TxMode::Full);
        rt.set_rendezvous_channels(vec![3, 9, 2]);
        let mut t = 1000u64;
        // KN4CRD proposes channels 9 then 3; we have [3,9,2] so the highest-ranked common is 9.
        let during = feed_over(
            &mut rt,
            "KN4CRD",
            "DC0SK",
            &propose_line("R7", &[9, 3]),
            &mut t,
        );
        // The agreement is withheld while the Accept is still being sent — emitting it during receive is
        // what let the daemon preempt and truncate the Accept's final frame (audit #4b).
        assert!(
            !during
                .iter()
                .any(|o| matches!(o, DiscoveryOutcome::RendezvousAgreed { .. })),
            "agreement must be deferred until the Accept over is on the air"
        );
        // Drain the Accept over; the agreement surfaces only once it has fully transmitted.
        let mut agreed = None;
        let mut txd = false;
        for _ in 0..8 {
            t += 15_000;
            for o in rt.tick(t, true) {
                match o {
                    DiscoveryOutcome::TransmitBeacon { .. } => txd = true,
                    DiscoveryOutcome::RendezvousAgreed {
                        peer,
                        channel,
                        switch_in_slots,
                    } => agreed = Some((peer, channel, switch_in_slots)),
                    _ => {}
                }
            }
        }
        assert!(txd, "the Accept over is transmitted");
        assert_eq!(
            agreed,
            Some(("KN4CRD".to_string(), 9, DEFAULT_SWITCH_SLOTS))
        );
    }

    /// Audit 2026-07-19 #5: answering a `Propose` costs a full JS8 over, and the path is reachable
    /// with no authentication. Without a per-peer floor, a peer repeating its proposal drives an
    /// unattended transmitter continuously.
    #[test]
    fn a_repeated_proposal_from_one_peer_does_not_transmit_every_time() {
        let mut rt = dwelling_rendezvous_runtime(TxMode::Full);
        rt.set_rendezvous_channels(vec![3, 9, 2]);
        let mut t = 1000u64;

        // First proposal: answered, as it should be.
        feed_over(
            &mut rt,
            "KN4CRD",
            "DC0SK",
            &propose_line("R7", &[9, 3]),
            &mut t,
        );
        let queued_after_first = rt.rendezvous_tx.len();
        assert!(
            queued_after_first > 0,
            "precondition: the first proposal must be answered, else this test proves nothing"
        );

        // Drain the queue so the next count is unambiguous.
        rt.rendezvous_tx.clear();

        // The same peer proposes again immediately — the flood case.
        feed_over(
            &mut rt,
            "KN4CRD",
            "DC0SK",
            &propose_line("R7", &[9, 3]),
            &mut t,
        );
        assert_eq!(
            rt.rendezvous_tx.len(),
            0,
            "a repeat proposal inside the cooldown must not queue another over — one peer could \
             otherwise hold the transmitter indefinitely, pre-authentication"
        );
    }

    /// Control: the cooldown is per peer, so a different station is still answered. A global lock
    /// would let one flooder silence rendezvous for everyone.
    #[test]
    fn the_responder_cooldown_is_per_peer() {
        let mut rt = dwelling_rendezvous_runtime(TxMode::Full);
        rt.set_rendezvous_channels(vec![3, 9, 2]);
        let mut t = 1000u64;

        feed_over(
            &mut rt,
            "KN4CRD",
            "DC0SK",
            &propose_line("R7", &[9, 3]),
            &mut t,
        );
        rt.rendezvous_tx.clear();

        feed_over(
            &mut rt,
            "W1AW",
            "DC0SK",
            &propose_line("R7", &[9, 3]),
            &mut t,
        );
        assert!(
            !rt.rendezvous_tx.is_empty(),
            "a different peer must still get an answer — the cooldown is per peer, not global"
        );
    }

    /// Control: once the cooldown has elapsed, a legitimate retry (our Accept was lost) is answered.
    #[test]
    fn a_proposal_after_the_cooldown_is_answered_again() {
        let mut rt = dwelling_rendezvous_runtime(TxMode::Full);
        rt.set_rendezvous_channels(vec![3, 9, 2]);
        let mut t = 1000u64;

        feed_over(
            &mut rt,
            "KN4CRD",
            "DC0SK",
            &propose_line("R7", &[9, 3]),
            &mut t,
        );
        rt.rendezvous_tx.clear();

        t += RESPONDER_COOLDOWN_MS + 1;
        feed_over(
            &mut rt,
            "KN4CRD",
            "DC0SK",
            &propose_line("R7", &[9, 3]),
            &mut t,
        );
        assert!(
            !rt.rendezvous_tx.is_empty(),
            "past the cooldown the same peer must be answered again, or a lost Accept can never be \
             retried"
        );
    }

    #[test]
    fn rx_only_start_rendezvous_transmits_nothing() {
        // Audit #4c: the documented zero-TX default must never emit a Propose, even if a rendezvous is
        // started against it (the daemon gates this too, but the runtime invariant must hold on its own).
        let mut rt = dwelling_rendezvous_runtime(TxMode::RxOnly);
        rt.set_rendezvous_channels(vec![0, 1, 2]);
        rt.start_rendezvous("KN4CRD", "R7", vec![1, 0], 16);
        assert!(
            !rt.rendezvous_active(),
            "RX-only refuses to start a rendezvous"
        );
        let mut t = 1000u64;
        for _ in 0..8 {
            t += 15_000;
            assert!(
                !rt.tick(t, true)
                    .iter()
                    .any(|o| matches!(o, DiscoveryOutcome::TransmitBeacon { .. })),
                "RX-only must never transmit a rendezvous frame"
            );
        }
    }

    #[test]
    fn responder_rejects_when_no_common_channel_and_does_not_agree() {
        let mut rt = dwelling_rendezvous_runtime(TxMode::Full);
        rt.set_rendezvous_channels(vec![0, 1]); // we don't have 9 or 3
        let mut t = 1000u64;
        let out = feed_over(
            &mut rt,
            "KN4CRD",
            "DC0SK",
            &propose_line("R7", &[9, 3]),
            &mut t,
        );
        assert!(
            !out.iter()
                .any(|o| matches!(o, DiscoveryOutcome::RendezvousAgreed { .. })),
            "no agreement without a common channel"
        );
    }

    #[test]
    fn rx_only_does_not_answer_a_proposal() {
        // Beacon (not Full) → the responder role is off; a proposal directed at us is not answered.
        let mut rt = dwelling_rendezvous_runtime(TxMode::Beacon);
        rt.set_rendezvous_channels(vec![3, 9]);
        let mut t = 1000u64;
        let out = feed_over(
            &mut rt,
            "KN4CRD",
            "DC0SK",
            &propose_line("R7", &[9, 3]),
            &mut t,
        );
        assert!(
            !out.iter()
                .any(|o| matches!(o, DiscoveryOutcome::RendezvousAgreed { .. })),
            "only Full mode responds to proposals"
        );
    }

    #[test]
    fn initiator_proposes_then_agrees_on_the_accept() {
        let mut rt = dwelling_rendezvous_runtime(TxMode::Full);
        rt.start_rendezvous("KN4CRD", "R7", vec![3, 9], 20);
        assert!(rt.rendezvous_active());
        // Drain the Propose over (transmits with priority, one frame per slot).
        let mut t = 1000u64;
        let mut proposed = false;
        for _ in 0..8 {
            t += 15_000;
            proposed |= rt
                .tick(t, true)
                .iter()
                .any(|o| matches!(o, DiscoveryOutcome::TransmitBeacon { .. }));
        }
        assert!(proposed, "the Propose over is transmitted");
        // KN4CRD accepts channel 9, switch in 4 slots.
        let out = feed_over(
            &mut rt,
            "KN4CRD",
            "DC0SK",
            &crate::rendezvous::RendezvousMsg::Accept {
                token: "R7".into(),
                channel: 9,
                switch_in_slots: 4,
            }
            .encode(),
            &mut t,
        );
        let agreed = out.iter().find_map(|o| match o {
            DiscoveryOutcome::RendezvousAgreed {
                peer,
                channel,
                switch_in_slots,
            } => Some((peer.clone(), *channel, *switch_in_slots)),
            _ => None,
        });
        assert_eq!(agreed, Some(("KN4CRD".to_string(), 9, 4)));
        assert!(!rt.rendezvous_active(), "the session concluded");
    }

    #[test]
    fn initiator_reports_a_reject() {
        let mut rt = dwelling_rendezvous_runtime(TxMode::Full);
        rt.start_rendezvous("KN4CRD", "R7", vec![3, 9], 20);
        let mut t = 1000u64;
        for _ in 0..8 {
            t += 15_000;
            rt.tick(t, true); // drain the Propose
        }
        let out = feed_over(
            &mut rt,
            "KN4CRD",
            "DC0SK",
            &crate::rendezvous::RendezvousMsg::Reject {
                token: "R7".into(),
                reason: crate::rendezvous::RejectReason::NoCommonFreq,
            }
            .encode(),
            &mut t,
        );
        assert!(
            out.iter().any(|o| matches!(
                o,
                DiscoveryOutcome::RendezvousRejected {
                    reason: RejectReason::NoCommonFreq,
                    ..
                }
            )),
            "peer reject surfaced: {out:?}"
        );
        assert!(!rt.rendezvous_active());
    }

    #[test]
    fn initiator_times_out_without_a_reply() {
        let mut rt = dwelling_rendezvous_runtime(TxMode::Full);
        rt.start_rendezvous("KN4CRD", "R7", vec![3, 9], 3);
        let mut t = 1000u64;
        // The Propose (~4 frames) must transmit first; aging only begins once it has drained, so the
        // timeout counts genuine reply-wait slots — run enough slots to cover Propose TX + 3 wait slots.
        let mut timed_out = false;
        for _ in 0..12 {
            t += 15_000;
            timed_out |= rt
                .tick(t, true)
                .iter()
                .any(|o| matches!(o, DiscoveryOutcome::RendezvousTimedOut { .. }));
        }
        assert!(timed_out, "the proposal timed out");
        assert!(!rt.rendezvous_active());
    }
}
