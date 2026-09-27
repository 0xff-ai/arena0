//! Verified transfer of one blob from its sender to its receiver.
//!
//! The runtime moves opaque bytes; this primitive does the verification it
//! wants through Host imports. The sender sends the object's leaf chaining
//! values (BLAKE3 subtrees of `LEAF_BYTES`), then the leaves themselves. The
//! receiver checks that the leaf values merge to the agreed hash before any
//! leaf arrives, checks each leaf against its value before appending it, and
//! commits the object (the Host hashes it once more). Only the receiver
//! authors the agreed outcome, once.
//!
//! Both phases are stop-and-wait: every `Leaves` and `Chunk` waits for the
//! receiver's `Next`. The stack delivers direct messages once, in order, across
//! crashes, so there is no resend, ack, or reordering logic. A `Next` can
//! arrive before its predecessor settles, so each direction holds at most two
//! entries per transfer.

use arena0::prelude::*;
use arena0::types::{MAX_BLOB_BYTES, MAX_DIRECT_RANGE_BYTES};
use std::time::Duration;

/// Bytes per leaf: one direct attachment and one whole BLAKE3 subtree.
pub const LEAF_BYTES: u64 = MAX_DIRECT_RANGE_BYTES;
/// Leaf chaining values per `Leaves` message. 63 values and the message's
/// framing fit `MAX_DIRECT_CONTROL_BYTES`.
pub const LEAVES_PER_MESSAGE: usize = 63;

/// Agreed messages; only a transfer's receiver broadcasts them, at most once.
#[arena0::message]
pub enum TransferMessage {
    /// The receiver committed the object: it holds the bytes of the hash.
    Complete,
    /// The transfer ended without the object.
    Failed,
}

/// Direct messages; each names its transfer.
#[arena0::message]
pub enum DirectMessage {
    /// Sender -> receiver: leaf chaining values `first..first + cvs.len()`.
    Leaves {
        transfer_id: u64,
        first: u32,
        cvs: Vec<ChainingValue>,
    },
    /// Sender -> receiver: the next leaf's bytes, as the attachment.
    Chunk { transfer_id: u64 },
    /// Receiver -> sender: send the next `Leaves` or `Chunk`.
    Next { transfer_id: u64 },
    /// Sender -> receiver: the sender's Host cannot read the object.
    Missing { transfer_id: u64 },
}

/// Local timers.
#[arena0::data]
pub enum TransferTimer {
    /// Starts the sender; armed on every participant at session start because
    /// agreed handlers cannot send direct messages.
    Send { transfer_id: u64 },
}

/// A transfer's agreed result.
#[arena0::data]
pub enum TransferStatus {
    Complete,
    Failed,
}

/// Agreed terms and result of one transfer.
#[arena0::primitive(capabilities(Messaging, Timers, Blobs))]
pub struct VerifiedTransfer {
    id: u64,
    sender: Participant,
    receiver: Participant,
    hash: BlobHash,
    length: u64,
    status: Option<TransferStatus>,
}

/// Participant-local progress for one transfer.
#[arena0::local]
#[derive(Clone, Default, serde::Serialize)]
pub struct VerifiedTransferLocal {
    /// Sender: messages sent so far. Steps `0..batches` are `Leaves` batches,
    /// then steps `batches..batches + leaf_count` are chunks.
    pub sent: u32,
    /// Receiver: the leaf chaining values received so far, in order.
    pub leaves: Vec<ChainingValue>,
    /// Receiver: leaves appended to the partial object so far.
    pub appended: u32,
    /// Receiver: whether it offered this transfer's `Complete` or `Failed`.
    /// Afterwards it ignores the transfer's direct messages.
    pub offered: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid transfer configuration")]
    Configuration,
    #[error("transfer message from a participant other than the receiver, or after the result")]
    InvalidMessage,
}

// The zero hash is a placeholder replaced by `new` before the session starts.
impl Default for VerifiedTransfer {
    fn default() -> Self {
        Self {
            id: 0,
            sender: Participant::new(0),
            receiver: Participant::new(0),
            hash: BlobHash([0; 32]),
            length: 0,
            status: None,
        }
    }
}

impl VerifiedTransfer {
    /// Fix the terms: distinct participants and `0 < length <= MAX_BLOB_BYTES`.
    pub fn new(
        id: u64,
        sender: Participant,
        receiver: Participant,
        hash: BlobHash,
        length: u64,
    ) -> Result<Self, Error> {
        let _ = (id, sender, receiver, hash, length, MAX_BLOB_BYTES);
        todo!("U5")
    }

    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn sender(&self) -> Participant {
        self.sender
    }
    pub fn receiver(&self) -> Participant {
        self.receiver
    }
    pub fn status(&self) -> Option<TransferStatus> {
        self.status.clone()
    }
    /// `length / LEAF_BYTES`, rounded up.
    pub fn leaf_count(&self) -> u32 {
        todo!("U5")
    }

    /// Arm the Send timer on every participant; roles are checked when it fires.
    pub fn start<S, L>(&self, ctx: &mut Context<S, L>) {
        ctx.effects().set_timer(
            TransferTimer::Send {
                transfer_id: self.id,
            },
            Duration::ZERO,
        );
    }

    /// The sender's Send timer sends step 0 (the first `Leaves` batch), or
    /// `Missing` when its Host cannot read the object. Returns the receiver's
    /// agreed message to broadcast, which is always `None` here.
    pub fn on_timer<S, L>(
        &self,
        ctx: &mut LocalContext<S, L>,
        local: fn(&mut L) -> &mut VerifiedTransferLocal,
        timer: TransferTimer,
    ) -> Option<TransferMessage> {
        let _ = (ctx, local, timer);
        todo!("U5")
    }

    /// Apply one direct message. Returns the receiver's agreed message to
    /// broadcast; the receiver returns `Some` at most once per transfer.
    pub fn on_direct<S, L>(
        &self,
        ctx: &mut LocalContext<S, L>,
        local: fn(&mut L) -> &mut VerifiedTransferLocal,
        from: Participant,
        msg: DirectMessage,
        attachment: Option<Attachment>,
    ) -> Option<TransferMessage> {
        let _ = (ctx, local, from, msg, attachment);
        todo!("U5")
    }

    /// Record the receiver's agreed result. `InvalidMessage` (rejecting the
    /// dispatch) for a message from anyone else or after the result is set.
    pub fn handle<S, L>(
        ctx: &mut Context<S, L>,
        field: fn(&mut S) -> &mut Self,
        from: Participant,
        msg: TransferMessage,
    ) -> Result<(), Error> {
        let _ = (ctx, field, from, msg);
        todo!("U5")
    }
}

/// The BLAKE3 hash of an object from its leaf chaining values (at least two),
/// merged in BLAKE3's left-balanced tree: the left subtree holds the largest
/// power of two of leaves smaller than the count; only the top merge is a root.
pub fn root(leaves: &[ChainingValue]) -> BlobHash {
    let _ = leaves;
    todo!("U5")
}
