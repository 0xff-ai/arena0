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
        if sender == receiver || length == 0 || length > MAX_BLOB_BYTES {
            return Err(Error::Configuration);
        }
        Ok(Self {
            id,
            sender,
            receiver,
            hash,
            length,
            status: None,
        })
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
        self.length.div_ceil(LEAF_BYTES) as u32
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
        let TransferTimer::Send { transfer_id } = timer;
        if transfer_id != self.id || self.status.is_some() {
            return None;
        }
        if ctx.me() == self.sender {
            self.send_step(ctx, local, 0);
        }
        None
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
        let transfer_id = match &msg {
            DirectMessage::Leaves { transfer_id, .. }
            | DirectMessage::Chunk { transfer_id }
            | DirectMessage::Next { transfer_id }
            | DirectMessage::Missing { transfer_id } => *transfer_id,
        };
        if transfer_id != self.id || self.status.is_some() {
            return None;
        }
        if ctx.me() == self.sender {
            if from == self.receiver && matches!(msg, DirectMessage::Next { .. }) {
                let sent = local(ctx.local_mut()).sent;
                if sent != self.batches() + self.leaf_count() {
                    self.send_step(ctx, local, sent);
                }
            }
            return None;
        }
        if ctx.me() != self.receiver || from != self.sender || local(ctx.local_mut()).offered {
            return None;
        }
        match msg {
            DirectMessage::Leaves { first, cvs, .. } => self.receive_leaves(ctx, local, first, cvs),
            DirectMessage::Chunk { .. } => self.receive_chunk(ctx, local, attachment),
            DirectMessage::Missing { .. } => Self::offer(ctx, local, TransferMessage::Failed),
            DirectMessage::Next { .. } => None,
        }
    }

    /// Record the receiver's agreed result. `InvalidMessage` (rejecting the
    /// dispatch) for a message from anyone else or after the result is set.
    pub fn handle<S, L>(
        ctx: &mut Context<S, L>,
        field: fn(&mut S) -> &mut Self,
        from: Participant,
        msg: TransferMessage,
    ) -> Result<(), Error> {
        let transfer = field(ctx.shared_mut());
        if from != transfer.receiver || transfer.status.is_some() {
            return Err(Error::InvalidMessage);
        }
        transfer.status = Some(match msg {
            TransferMessage::Complete => TransferStatus::Complete,
            TransferMessage::Failed => TransferStatus::Failed,
        });
        Ok(())
    }

    /// Messages the sender sends: `batches() + leaf_count()` steps.
    fn batches(&self) -> u32 {
        self.leaf_count().div_ceil(LEAVES_PER_MESSAGE as u32)
    }

    /// `[start, end)` of leaf `index`.
    fn leaf_range(&self, index: u32) -> (u64, u64) {
        let start = u64::from(index) * LEAF_BYTES;
        (start, (start + LEAF_BYTES).min(self.length))
    }

    /// Send step `step` (a Leaves batch or a Chunk), or Missing when a leaf's
    /// chaining value cannot be computed (`subtree_cv` → NotFound). Advances `sent`.
    fn send_step<S, L>(
        &self,
        ctx: &mut LocalContext<S, L>,
        local: fn(&mut L) -> &mut VerifiedTransferLocal,
        step: u32,
    ) {
        if step < self.batches() {
            let first = step * LEAVES_PER_MESSAGE as u32;
            let end = ((step + 1) * LEAVES_PER_MESSAGE as u32).min(self.leaf_count());
            let mut cvs = Vec::with_capacity((end - first) as usize);
            for index in first..end {
                let (start, end) = self.leaf_range(index);
                let cv = ctx.blobs().subtree_cv(
                    CvSource::Blob {
                        hash: self.hash,
                        start,
                        end,
                    },
                    start,
                );
                if matches!(cv, Err(BlobError::NotFound)) {
                    ctx.send_direct(
                        self.receiver,
                        &DirectMessage::Missing {
                            transfer_id: self.id,
                        },
                        None,
                    )
                    .expect("stop-and-wait fits the direct queue");
                    return;
                }
                cvs.push(cv.expect("leaf range is a valid BLAKE3 subtree"));
            }
            ctx.send_direct(
                self.receiver,
                &DirectMessage::Leaves {
                    transfer_id: self.id,
                    first,
                    cvs,
                },
                None,
            )
            .expect("stop-and-wait fits the direct queue");
        } else {
            let (start, end) = self.leaf_range(step - self.batches());
            ctx.send_direct(
                self.receiver,
                &DirectMessage::Chunk {
                    transfer_id: self.id,
                },
                Some(RangeAttachment {
                    hash: self.hash,
                    start,
                    end,
                }),
            )
            .expect("stop-and-wait fits the direct queue");
        }
        local(ctx.local_mut()).sent = step + 1;
    }

    /// Mark `offered` and return the terminal message.
    fn offer<S, L>(
        ctx: &mut LocalContext<S, L>,
        local: fn(&mut L) -> &mut VerifiedTransferLocal,
        message: TransferMessage,
    ) -> Option<TransferMessage> {
        local(ctx.local_mut()).offered = true;
        Some(message)
    }

    /// The receiver's handling of one Leaves; returns the terminal message or None after sending Next.
    fn receive_leaves<S, L>(
        &self,
        ctx: &mut LocalContext<S, L>,
        local: fn(&mut L) -> &mut VerifiedTransferLocal,
        first: u32,
        cvs: Vec<ChainingValue>,
    ) -> Option<TransferMessage> {
        let progress = local(ctx.local_mut());
        if first as usize != progress.leaves.len()
            || cvs.is_empty()
            || cvs.len() > LEAVES_PER_MESSAGE
            || progress.leaves.len() + cvs.len() > self.leaf_count() as usize
        {
            return Self::offer(ctx, local, TransferMessage::Failed);
        }
        progress.leaves.extend(cvs);
        if progress.leaves.len() == self.leaf_count() as usize
            && self.leaf_count() >= 2
            && root(&progress.leaves) != self.hash
        {
            return Self::offer(ctx, local, TransferMessage::Failed);
        }
        ctx.send_direct(
            self.sender,
            &DirectMessage::Next {
                transfer_id: self.id,
            },
            None,
        )
        .expect("stop-and-wait fits the direct queue");
        None
    }

    /// The receiver's handling of one Chunk; returns the terminal message or None after sending Next.
    fn receive_chunk<S, L>(
        &self,
        ctx: &mut LocalContext<S, L>,
        local: fn(&mut L) -> &mut VerifiedTransferLocal,
        attachment: Option<Attachment>,
    ) -> Option<TransferMessage> {
        let progress = local(ctx.local_mut());
        if progress.leaves.len() != self.leaf_count() as usize {
            return Self::offer(ctx, local, TransferMessage::Failed);
        }
        let Some(attachment) = attachment else {
            return Self::offer(ctx, local, TransferMessage::Failed);
        };
        let offset = u64::from(progress.appended) * LEAF_BYTES;
        let expected = progress.leaves[progress.appended as usize];
        if ctx
            .blobs()
            .subtree_cv(CvSource::Attachment(attachment), offset)
            != Ok(expected)
            || ctx
                .blobs()
                .append(self.hash, self.length, attachment)
                .is_err()
        {
            return Self::offer(ctx, local, TransferMessage::Failed);
        }
        let progress = local(ctx.local_mut());
        progress.appended += 1;
        if progress.appended == self.leaf_count() {
            // A single leaf's chaining value is not its root hash. Commit also
            // checks that case, and binds the final bytes to the agreed hash.
            let message = match ctx.blobs().commit(self.hash) {
                Ok(()) => TransferMessage::Complete,
                Err(_) => TransferMessage::Failed,
            };
            return Self::offer(ctx, local, message);
        }
        ctx.send_direct(
            self.sender,
            &DirectMessage::Next {
                transfer_id: self.id,
            },
            None,
        )
        .expect("stop-and-wait fits the direct queue");
        None
    }
}

/// The BLAKE3 hash of an object from its leaf chaining values (at least two),
/// merged in BLAKE3's left-balanced tree: the left subtree holds the largest
/// power of two of leaves smaller than the count; only the top merge is a root.
pub fn root(leaves: &[ChainingValue]) -> BlobHash {
    BlobHash(merge(leaves, true))
}

/// The parent value of `leaves` (at least two): `root = true` only at the top.
fn merge(leaves: &[ChainingValue], root: bool) -> ChainingValue {
    assert!(leaves.len() >= 2, "a parent requires at least two leaves");
    let left = 1 << (leaves.len() - 1).ilog2();
    arena0::merge_cv(&subtree(&leaves[..left]), &subtree(&leaves[left..]), root)
}

/// A subtree's value: the leaf itself for one leaf, else `merge(leaves, false)`.
fn subtree(leaves: &[ChainingValue]) -> ChainingValue {
    if leaves.len() == 1 {
        leaves[0]
    } else {
        merge(leaves, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_crypto::{HashAlgorithm, blake3_tree::subtree_cv, hash};

    #[test]
    fn root_matches_blake3_for_left_balanced_trees() {
        for count in [2, 3, 5, 6, 7, 8, 9, 64, 512] {
            let bytes: Vec<u8> = (0..(count - 1) * LEAF_BYTES as usize + 137)
                .map(|i| (i % 251) as u8)
                .collect();
            let leaves: Vec<_> = bytes
                .chunks(LEAF_BYTES as usize)
                .enumerate()
                .map(|(i, bytes)| subtree_cv(bytes, i as u64 * LEAF_BYTES))
                .collect();
            assert_eq!(leaves.len(), count);
            assert_eq!(
                root(&leaves),
                BlobHash(hash(HashAlgorithm::Blake3, &bytes)),
                "{count} leaves"
            );
        }
    }

    #[test]
    fn new_rejects_bad_terms() {
        let sender = Participant::new(0);
        for (receiver, length) in [
            (sender, 1),
            (Participant::new(1), 0),
            (Participant::new(1), MAX_BLOB_BYTES + 1),
        ] {
            assert!(matches!(
                VerifiedTransfer::new(7, sender, receiver, BlobHash([0; 32]), length),
                Err(Error::Configuration)
            ));
        }
    }

    #[test]
    fn leaf_and_step_counts() {
        for (length, leaves, batches) in [
            (1, 1, 1),
            (LEAF_BYTES, 1, 1),
            (LEAF_BYTES + 1, 2, 1),
            (63 * LEAF_BYTES, 63, 1),
            (64 * LEAF_BYTES, 64, 2),
            (MAX_BLOB_BYTES, 512, 9),
        ] {
            let transfer = VerifiedTransfer::new(
                7,
                Participant::new(0),
                Participant::new(1),
                BlobHash([0; 32]),
                length,
            )
            .unwrap();
            assert_eq!(transfer.leaf_count(), leaves, "length {length}");
            assert_eq!(transfer.batches(), batches, "length {length}");
            assert!(transfer.status().is_none());
        }
    }

    #[test]
    fn handle_accepts_only_the_receiver_once() {
        for message in [TransferMessage::Complete, TransferMessage::Failed] {
            let transfer = VerifiedTransfer::new(
                7,
                Participant::new(0),
                Participant::new(1),
                BlobHash([0; 32]),
                1,
            )
            .unwrap();
            // SAFETY: this test models an agreed dispatch and invokes no host effects.
            let mut ctx = unsafe { Context::__new(transfer, (), PeerId([0; 32])) };
            assert!(matches!(
                VerifiedTransfer::handle(&mut ctx, |s| s, Participant::new(0), message.clone()),
                Err(Error::InvalidMessage)
            ));
            assert!(ctx.shared().status().is_none());
            VerifiedTransfer::handle(&mut ctx, |s| s, Participant::new(1), message.clone())
                .unwrap();
            let expected = match message {
                TransferMessage::Complete => TransferStatus::Complete,
                TransferMessage::Failed => TransferStatus::Failed,
            };
            assert_eq!(ctx.shared().status(), Some(expected.clone()));
            for from in [Participant::new(0), Participant::new(1)] {
                for message in [TransferMessage::Complete, TransferMessage::Failed] {
                    assert!(matches!(
                        VerifiedTransfer::handle(&mut ctx, |s| s, from, message),
                        Err(Error::InvalidMessage)
                    ));
                    assert_eq!(ctx.shared().status(), Some(expected.clone()));
                }
            }
        }
    }
}
