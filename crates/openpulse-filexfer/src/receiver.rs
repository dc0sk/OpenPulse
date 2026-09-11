//! Receiver-side transfer state machine (see the diagram in `docs/dev/design/file-transfer-plan.md` §3.3).
//!
//! Byte-level fragment ingestion and reassembly live in the daemon's blocks layer (Phase B); this
//! machine is driven by `note_block_complete` (a block fully reassembled) and `set_verify_result`
//! (the reassembled payload was verified), which keeps the crate pure.

use crate::offer::{FileOffer, OfferDecision};
use crate::wire::{CompleteStatus, FxFrame, Reason};
use crate::{FxAction, Outcome, Timeouts, TransferResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    AwaitingDecision,
    Receiving,
    Verifying,
    Terminal,
}

/// Drives receiving one file from a peer.
pub struct ReceiverSession {
    transfer_id: u32,
    block_count: u16,
    state: State,
    blocks_done: Vec<bool>,
    done_count: u16,
    timeouts: Timeouts,
    deadline: u64,
    /// Blocks already held from a previous (interrupted) transfer of the same file — announced to the
    /// sender in `FileAccept.have_bitmap` so it skips them (resume). Empty for a fresh transfer.
    held: Vec<bool>,
}

impl ReceiverSession {
    /// Create a receiver for an inbound `offer` given the policy `decision` (from [`crate::decide`]).
    /// Emits the immediate action: transmit `FileReject`, or `FileAccept` (auto-accept), or a `Prompt`.
    pub fn new(
        offer: &FileOffer,
        decision: OfferDecision,
        timeouts: Timeouts,
        now_ms: u64,
    ) -> (Self, Vec<FxAction>) {
        Self::resume(offer, decision, &[], timeouts, now_ms)
    }

    /// Like [`new`](Self::new) but resuming an interrupted transfer: `held` marks blocks already on
    /// disk (indexed by block), which are counted done and announced in `FileAccept.have_bitmap` so the
    /// sender skips them.
    pub fn resume(
        offer: &FileOffer,
        decision: OfferDecision,
        held: &[bool],
        timeouts: Timeouts,
        now_ms: u64,
    ) -> (Self, Vec<FxAction>) {
        let bc = offer.block_count as usize;
        let held_v: Vec<bool> = (0..bc)
            .map(|i| held.get(i).copied().unwrap_or(false))
            .collect();
        let done_count = held_v.iter().filter(|&&h| h).count() as u16;
        let mut session = Self {
            transfer_id: offer.transfer_id,
            block_count: offer.block_count,
            state: State::AwaitingDecision,
            blocks_done: held_v.clone(),
            done_count,
            timeouts,
            deadline: 0,
            held: held_v,
        };
        let actions = match decision {
            OfferDecision::Reject(reason) => session.reject_now(reason),
            OfferDecision::AutoAccept => session.begin_receiving(now_ms),
            OfferDecision::Prompt => {
                session.deadline = now_ms.saturating_add(timeouts.offer_ms);
                vec![FxAction::Prompt {
                    transfer_id: session.transfer_id,
                }]
            }
        };
        (session, actions)
    }

    /// Operator accepts a prompted offer.
    pub fn accept(&mut self, now_ms: u64) -> Vec<FxAction> {
        if self.state != State::AwaitingDecision {
            return Vec::new();
        }
        self.begin_receiving(now_ms)
    }

    /// Operator rejects a prompted offer.
    pub fn reject(&mut self, reason: Reason) -> Vec<FxAction> {
        if self.state != State::AwaitingDecision {
            return Vec::new();
        }
        self.reject_now(reason)
    }

    /// The blocks layer reports that block `block_index` has fully reassembled. When every block is in,
    /// transitions to `Verifying` and emits a `Verify` action.
    pub fn note_block_complete(&mut self, block_index: u16, now_ms: u64) -> Vec<FxAction> {
        if self.state != State::Receiving {
            return Vec::new();
        }
        match self.blocks_done.get_mut(block_index as usize) {
            Some(slot) if !*slot => {
                *slot = true;
                self.done_count += 1;
            }
            Some(_) => return Vec::new(), // duplicate block completion
            None => return Vec::new(),    // out-of-range block index
        }
        self.deadline = now_ms.saturating_add(self.timeouts.block_stall_ms);
        let mut actions = vec![self.progress()];
        if self.done_count == self.block_count {
            self.state = State::Verifying;
            self.deadline = now_ms.saturating_add(self.timeouts.verify_ms);
            actions.push(FxAction::Verify {
                transfer_id: self.transfer_id,
            });
        }
        actions
    }

    /// The blocks layer reports the reassembled payload's verification result (from
    /// `verify_manifest_with_payload`). Emits the `FileComplete` frame + terminal outcome.
    pub fn set_verify_result(
        &mut self,
        status: CompleteStatus,
        countersignature: [u8; 64],
    ) -> Vec<FxAction> {
        if self.state != State::Verifying {
            return Vec::new();
        }
        self.state = State::Terminal;
        let frame = FxFrame::FileComplete {
            transfer_id: self.transfer_id,
            status,
            countersignature,
        };
        vec![
            FxAction::Transmit(frame.encode()),
            FxAction::Finished(Outcome {
                transfer_id: self.transfer_id,
                result: TransferResult::Received {
                    verified: status.is_ok(),
                },
            }),
        ]
    }

    /// Apply an inbound frame. Data-frame ingestion is the blocks layer's job (Phase B); here only
    /// `FileCancel` is meaningful.
    pub fn apply(&mut self, frame: &FxFrame, _now_ms: u64) -> Vec<FxAction> {
        if frame.transfer_id() != self.transfer_id {
            return Vec::new();
        }
        match frame {
            FxFrame::FileCancel { reason, .. } if self.state != State::Terminal => {
                self.state = State::Terminal;
                vec![FxAction::Finished(Outcome {
                    transfer_id: self.transfer_id,
                    result: TransferResult::Cancelled { reason: *reason },
                })]
            }
            _ => Vec::new(),
        }
    }

    /// Fire time-based transitions: an unanswered prompt rejects `timeout`; a stalled transfer fails.
    pub fn poll_timeout(&mut self, now_ms: u64) -> Vec<FxAction> {
        if self.state == State::Terminal || now_ms < self.deadline {
            return Vec::new();
        }
        match self.state {
            State::AwaitingDecision => self.reject_now(Reason::Timeout),
            State::Receiving | State::Verifying => {
                self.state = State::Terminal;
                vec![FxAction::Finished(Outcome {
                    transfer_id: self.transfer_id,
                    result: TransferResult::Failed {
                        reason: Reason::Stall,
                    },
                })]
            }
            State::Terminal => Vec::new(),
        }
    }

    /// Whether the transfer has been accepted and is actively receiving blocks.
    ///
    /// A caller must consult this **before** persisting peer bytes or acknowledging a block on air:
    /// in `AwaitingDecision` the operator has not accepted the transfer yet, so neither writing the
    /// data nor keying the transmitter to ack it is authorised (audit 2026-07-19, #11).
    pub fn is_receiving(&self) -> bool {
        self.state == State::Receiving
    }

    /// Whether the session has reached a terminal state.
    pub fn is_terminal(&self) -> bool {
        self.state == State::Terminal
    }

    fn begin_receiving(&mut self, now_ms: u64) -> Vec<FxAction> {
        self.state = State::Receiving;
        self.deadline = now_ms.saturating_add(self.timeouts.block_stall_ms);
        let accept = FxFrame::FileAccept {
            transfer_id: self.transfer_id,
            have_bitmap: bitmap_from_bools(&self.held),
        };
        let mut actions = vec![FxAction::Transmit(accept.encode()), self.progress()];
        // Resume edge: every block was already on disk — nothing to receive, go straight to verify.
        if self.done_count == self.block_count {
            self.state = State::Verifying;
            self.deadline = now_ms.saturating_add(self.timeouts.verify_ms);
            actions.push(FxAction::Verify {
                transfer_id: self.transfer_id,
            });
        }
        actions
    }

    fn reject_now(&mut self, reason: Reason) -> Vec<FxAction> {
        self.state = State::Terminal;
        let frame = FxFrame::FileReject {
            transfer_id: self.transfer_id,
            reason,
        };
        vec![
            FxAction::Transmit(frame.encode()),
            FxAction::Finished(Outcome {
                transfer_id: self.transfer_id,
                result: TransferResult::Rejected { reason },
            }),
        ]
    }

    fn progress(&self) -> FxAction {
        FxAction::Progress {
            transfer_id: self.transfer_id,
            blocks_done: self.done_count,
            blocks_total: self.block_count,
        }
    }
}

/// Pack a per-block boolean mask into a little-endian-by-byte bitmap (bit `i` = block `i` held).
fn bitmap_from_bools(bools: &[bool]) -> Vec<u8> {
    if bools.iter().all(|&b| !b) {
        return Vec::new(); // fresh transfer — empty bitmap keeps the wire minimal
    }
    let mut out = vec![0u8; bools.len().div_ceil(8)];
    for (i, &b) in bools.iter().enumerate() {
        if b {
            out[i / 8] |= 1 << (i % 8);
        }
    }
    out
}
