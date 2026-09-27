//! Handle-only transfer policy. The Host proves, verifies, and stores file bytes.
//!
//! A receiver signs only after staging its verified write (and final publication)
//! in the same dispatch. The Host commits that dispatch before delivering its
//! outgoing ack. Direct progress stays local; only sender-authored checkpoints
//! advance the agreed state. Envelopes remain opaque: only `ctx.verify` extracts
//! an acknowledgement payload.

use arena0::prelude::*;
use arena0::types::{MAX_BLOB_BYTES, MAX_DIRECT_RANGE_BYTES};
use std::{ops::Range, time::Duration};

/// Domain and version bound into every acknowledgement.
pub const ACK_DOMAIN: [u8; 28] = *b"arena0/verified-transfer/ack";
pub const ACK_VERSION: u16 = 1;
/// Delay before the sender resends an unacknowledged chunk.
pub const RESEND_AFTER: Duration = Duration::from_secs(2);

/// The receiver's signed statement: "I durably hold bytes 0..accepted of object hash".
#[arena0::data]
pub struct TransferAck {
    pub domain: [u8; 28],
    pub version: u16,
    pub transfer_id: u64,
    pub sender: PeerId,
    pub hash: BlobHash,
    pub length: u64,
    pub chunk_size: u32,
    pub sequence: u64,
    pub accepted: u64,
    pub complete: bool,
}

/// A verified checkpoint: the receiver's envelope and the body `ctx.verify` returned.
#[arena0::data]
pub struct Checkpoint {
    pub ack: TransferAck,
    pub signed: Signed,
}

/// Agreed messages; only a transfer's sender broadcasts them.
#[arena0::message]
pub enum TransferMessage {
    Checkpoint(Signed),
    Failed,
}

/// Direct messages; each names its transfer.
#[arena0::message]
pub enum DirectMessage {
    Chunk { transfer_id: u64, index: u64 },
    Ack { transfer_id: u64, ack: Signed },
    Failed { transfer_id: u64 },
}

/// Local timers.
#[arena0::data]
pub enum TransferTimer {
    Send { transfer_id: u64 },
    Resend { transfer_id: u64, index: u64 },
}

/// Agreed terms and receiver-certified progress for one object.
#[arena0::primitive(capabilities(Messaging, Timers, Blobs, Sign { schemes: [Ed25519] }))]
pub struct VerifiedTransfer {
    id: u64,
    sender: Participant,
    receiver: Participant,
    hash: BlobHash,
    length: u64,
    chunk_size: u32,
    checkpoint_every: u32,
    checkpoint: Option<Checkpoint>,
    failed: bool,
}

/// Participant-local progress for one transfer.
#[arena0::local]
#[derive(Clone, Default, serde::Serialize)]
pub struct VerifiedTransferLocal {
    /// Sender: the resolved source. Receiver: the output bound to (hash, length).
    pub blob: Option<BlobHandle>,
    /// Sender: next chunk to send. Receiver: next chunk expected.
    pub next_index: u64,
    /// Receiver: its latest signed ack (resent for duplicates).
    /// Sender: the latest verified ack received (the next checkpoint candidate).
    pub last_ack: Option<Signed>,
    /// The sequence of `last_ack`; 0 before the first.
    pub last_sequence: u64,
    /// Sender: accepted prefix of the last checkpoint offered for broadcast.
    /// It advances even if the program's bounded broadcast queue is full.
    pub last_checkpointed: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid transfer configuration")]
    Configuration,
    #[error("transfer arithmetic overflow")]
    Overflow,
    #[error("chunk is outside the object")]
    ChunkOutOfRange,
    #[error("invalid transfer acknowledgement")]
    InvalidAck,
    #[error("conflicting transfer acknowledgements")]
    ConflictingAck,
    #[error("acknowledgement progress decreased")]
    DecreasingAck,
    #[error("blob operation failed: {0}")]
    Blob(#[from] BlobError),
    #[error("acknowledgement verification failed: {0}")]
    Verify(#[from] VerifyError),
    #[error("message queue is full: {0}")]
    Send(#[from] SendError),
}

// The zero hash is a placeholder replaced by initialization with `new` before use.
impl Default for VerifiedTransfer {
    fn default() -> Self {
        Self {
            id: 0,
            sender: Participant::new(0),
            receiver: Participant::new(0),
            hash: BlobHash([0; 32]),
            length: 0,
            chunk_size: 0,
            checkpoint_every: 0,
            checkpoint: None,
            failed: false,
        }
    }
}

impl VerifiedTransfer {
    /// Fix the terms: distinct participants, a nonzero chunk no larger than
    /// `MAX_DIRECT_RANGE_BYTES`, bounded object length, and nonzero cadence.
    pub fn new(
        id: u64,
        sender: Participant,
        receiver: Participant,
        hash: BlobHash,
        length: u64,
        chunk_size: u32,
        checkpoint_every: u32,
    ) -> Result<Self, Error> {
        if sender == receiver
            || chunk_size == 0
            || u64::from(chunk_size) > MAX_DIRECT_RANGE_BYTES
            || length > MAX_BLOB_BYTES
            || checkpoint_every == 0
        {
            return Err(Error::Configuration);
        }
        Ok(Self {
            id,
            sender,
            receiver,
            hash,
            length,
            chunk_size,
            checkpoint_every,
            checkpoint: None,
            failed: false,
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
    pub fn is_complete(&self) -> bool {
        self.checkpoint.as_ref().is_some_and(|c| c.ack.complete)
    }
    pub fn is_failed(&self) -> bool {
        self.failed
    }
    pub fn acknowledged(&self) -> u64 {
        self.checkpoint.as_ref().map_or(0, |c| c.ack.accepted)
    }
    pub fn chunk_count(&self) -> u64 {
        self.length.div_ceil(u64::from(self.chunk_size))
    }

    /// Compute the range from agreed terms; a sender never supplies its geometry.
    pub fn chunk_range(&self, index: u64) -> Result<Range<u64>, Error> {
        let start = index
            .checked_mul(u64::from(self.chunk_size))
            .ok_or(Error::Overflow)?;
        if start >= self.length {
            return Err(Error::ChunkOutOfRange);
        }
        let end = start
            .checked_add(u64::from(self.chunk_size))
            .ok_or(Error::Overflow)?
            .min(self.length);
        Ok(start..end)
    }

    /// Arm the initial local work on every participant; roles are checked when it fires.
    pub fn start<S, L>(&self, ctx: &mut Context<S, L>) {
        ctx.effects().set_timer(
            TransferTimer::Send {
                transfer_id: self.id,
            },
            Duration::ZERO,
        );
    }

    /// Resolve and send the current chunk, or retry it while its index is current.
    /// Empty outputs are committed and acknowledged by the receiver's Send timer.
    pub fn on_timer<S, L>(
        &self,
        ctx: &mut LocalContext<S, L>,
        local: fn(&mut L) -> &mut VerifiedTransferLocal,
        timer: TransferTimer,
    ) -> Result<Option<TransferMessage>, Error> {
        if self.is_complete() || self.failed {
            return Ok(None);
        }
        let progress = ctx.mutate_local(|state| local(state).clone());
        match timer {
            TransferTimer::Send { transfer_id } if transfer_id == self.id => {}
            TransferTimer::Resend { transfer_id, index }
                if transfer_id == self.id
                    && index == progress.next_index
                    && ctx.me() == self.sender => {}
            _ => return Ok(None),
        }
        if ctx.me() == self.receiver && self.length == 0 {
            let signed = if let Some(signed) = progress.last_ack {
                signed
            } else {
                let result = (|| {
                    let mut blobs = ctx.blobs();
                    let output = blobs.create(self.hash, 0)?;
                    blobs.commit(output)?;
                    Ok::<_, BlobError>(output)
                })();
                let output = match result {
                    Ok(output) => output,
                    Err(_) => {
                        self.report_failure(ctx);
                        return Ok(None);
                    }
                };
                ctx.mutate_local(|state| local(state).blob = Some(output));
                self.sign_ack(ctx, local, 0, true)?
            };
            // An empty object has no duplicate chunk to trigger another ack.
            // Retry its Send timer if the direct queue could not retain it.
            if ctx
                .send_direct(
                    self.sender,
                    &DirectMessage::Ack {
                        transfer_id: self.id,
                        ack: signed,
                    },
                    None,
                )
                .is_err()
            {
                ctx.effects().set_timer(
                    TransferTimer::Send {
                        transfer_id: self.id,
                    },
                    RESEND_AFTER,
                );
            }
            return Ok(None);
        }
        if ctx.me() != self.sender {
            return Ok(None);
        }
        let source = match progress.blob {
            Some(source) => source,
            None => match ctx.blobs().resolve(self.hash, self.length) {
                Ok(source) => {
                    ctx.mutate_local(|state| local(state).blob = Some(source));
                    source
                }
                Err(_) => return Ok(Some(TransferMessage::Failed)),
            },
        };
        if progress.next_index >= self.chunk_count() {
            return Ok(None);
        }
        let range = self.chunk_range(progress.next_index)?;
        // Backpressure is transient. Retain the retry even when this attempt
        // queued nothing; propagating QueueFull would reject that timer too.
        let _ = ctx.send_direct(
            self.receiver,
            &DirectMessage::Chunk {
                transfer_id: self.id,
                index: progress.next_index,
            },
            Some(RangeAttachment {
                source,
                start: range.start,
                end: range.end,
            }),
        );
        ctx.effects().set_timer(
            TransferTimer::Resend {
                transfer_id: self.id,
                index: progress.next_index,
            },
            RESEND_AFTER,
        );
        Ok(None)
    }

    /// Apply direct progress locally. Only the sender returns agreed messages;
    /// receiver errors travel directly to that sender for agreement.
    pub fn on_direct<S, L>(
        &self,
        ctx: &mut LocalContext<S, L>,
        local: fn(&mut L) -> &mut VerifiedTransferLocal,
        from: Participant,
        msg: DirectMessage,
        slice: Option<Attachment>,
    ) -> Result<Option<TransferMessage>, Error> {
        if self.is_complete() || self.failed {
            return Ok(None);
        }
        match msg {
            DirectMessage::Chunk { transfer_id, index }
                if transfer_id == self.id && ctx.me() == self.receiver && from == self.sender =>
            {
                let progress = ctx.mutate_local(|state| local(state).clone());
                if index < progress.next_index {
                    let ack = progress
                        .last_ack
                        .expect("accepted chunk has a retained ack");
                    let _ = ctx.send_direct(
                        self.sender,
                        &DirectMessage::Ack {
                            transfer_id: self.id,
                            ack,
                        },
                        None,
                    );
                    return Ok(None);
                }
                if index > progress.next_index {
                    return Ok(None);
                }
                let range = self.chunk_range(index)?;
                let result = (|| {
                    let output = match progress.blob {
                        Some(output) => output,
                        None => {
                            let output = ctx.blobs().create(self.hash, self.length)?;
                            ctx.mutate_local(|state| local(state).blob = Some(output));
                            output
                        }
                    };
                    ctx.blobs().accept_range(
                        output,
                        slice.ok_or(BlobError::BadSlice)?,
                        range.clone(),
                    )?;
                    if range.end == self.length {
                        ctx.blobs().commit(output)?;
                    }
                    Ok::<_, BlobError>(())
                })();
                if result.is_err() {
                    self.report_failure(ctx);
                    return Ok(None);
                }
                ctx.mutate_local(|state| local(state).next_index = index + 1);
                let ack = self.sign_ack(ctx, local, range.end, range.end == self.length)?;
                // The sender's resend timer solicits this same retained ack if
                // the direct queue is full; successful blob work stays durable.
                let _ = ctx.send_direct(
                    self.sender,
                    &DirectMessage::Ack {
                        transfer_id: self.id,
                        ack,
                    },
                    None,
                );
                Ok(None)
            }
            DirectMessage::Ack {
                transfer_id,
                ack: signed,
            } if transfer_id == self.id && ctx.me() == self.sender && from == self.receiver => {
                let ack = self.verified_ack(ctx, &signed)?;
                let progress = ctx.mutate_local(|state| local(state).clone());
                if ack.sequence <= progress.last_sequence {
                    return Ok(None);
                }
                if let Some(previous) = &progress.last_ack {
                    let previous = self.verified_ack(ctx, previous)?;
                    monotonic(Some(&previous), &ack)?;
                }
                let due = checkpoint_due(
                    ack.accepted,
                    ack.complete,
                    self.acknowledged(),
                    progress.last_checkpointed,
                    self.chunk_size,
                    self.checkpoint_every,
                );
                ctx.mutate_local(|state| {
                    let progress = local(state);
                    progress.last_sequence = ack.sequence;
                    progress.last_ack = Some(signed.clone());
                    progress.next_index = if ack.complete {
                        self.chunk_count()
                    } else {
                        ack.accepted / u64::from(self.chunk_size)
                    };
                });
                if !ack.complete
                    && let Some(failed) = self.on_timer(
                        ctx,
                        local,
                        TransferTimer::Send {
                            transfer_id: self.id,
                        },
                    )?
                {
                    return Ok(Some(failed));
                }
                if due {
                    ctx.mutate_local(|state| local(state).last_checkpointed = ack.accepted);
                }
                Ok(due.then_some(TransferMessage::Checkpoint(signed)))
            }
            DirectMessage::Failed { transfer_id }
                if transfer_id == self.id && ctx.me() == self.sender && from == self.receiver =>
            {
                Ok(Some(TransferMessage::Failed))
            }
            _ => Ok(None),
        }
    }

    /// Verify the receiver's evidence under this session before changing agreed state.
    /// Only this transfer's sender may author its checkpoints or failure.
    pub fn handle<S, L>(
        ctx: &mut Context<S, L>,
        field: fn(&mut S) -> &mut Self,
        from: Participant,
        msg: TransferMessage,
    ) -> Result<(), Error> {
        let transfer = ctx.mutate_shared(|state| field(state).clone());
        if from != transfer.sender {
            return Err(Error::InvalidAck);
        }
        match msg {
            TransferMessage::Failed => ctx.mutate_shared(|state| field(state).failed = true),
            TransferMessage::Checkpoint(signed) => {
                let ack = transfer.verified_ack(ctx, &signed)?;
                if monotonic(transfer.checkpoint.as_ref().map(|c| &c.ack), &ack)? {
                    ctx.mutate_shared(|state| {
                        field(state).checkpoint = Some(Checkpoint { ack, signed })
                    });
                }
            }
        }
        Ok(())
    }

    fn verified_ack<S, L, M: arena0::Mode>(
        &self,
        ctx: &arena0::Ctx<S, L, M>,
        signed: &Signed,
    ) -> Result<TransferAck, Error> {
        let receiver = ctx
            .ensemble()
            .peer_at(self.receiver)
            .ok_or(Error::Configuration)?;
        let sender = ctx
            .ensemble()
            .peer_at(self.sender)
            .ok_or(Error::Configuration)?;
        let payload = ctx.verify(signed, receiver)?;
        let ack: TransferAck = borsh::from_slice(&payload).map_err(|_| Error::InvalidAck)?;
        self.validate_ack(&ack, sender)?;
        Ok(ack)
    }

    fn sign_ack<S, L>(
        &self,
        ctx: &mut LocalContext<S, L>,
        local: fn(&mut L) -> &mut VerifiedTransferLocal,
        accepted: u64,
        complete: bool,
    ) -> Result<Signed, Error> {
        let sequence = ctx
            .mutate_local(|state| local(state).last_sequence)
            .checked_add(1)
            .ok_or(Error::Overflow)?;
        let ack = TransferAck {
            domain: ACK_DOMAIN,
            version: ACK_VERSION,
            transfer_id: self.id,
            sender: ctx
                .ensemble()
                .peer_at(self.sender)
                .ok_or(Error::Configuration)?,
            hash: self.hash,
            length: self.length,
            chunk_size: self.chunk_size,
            sequence,
            accepted,
            complete,
        };
        let signed = ctx.sign(
            SignScheme::Ed25519,
            &borsh::to_vec(&ack).expect("ack serialization"),
        );
        ctx.mutate_local(|state| {
            let progress = local(state);
            progress.last_sequence = sequence;
            progress.last_ack = Some(signed.clone());
        });
        Ok(signed)
    }

    fn report_failure<S, L>(&self, ctx: &mut LocalContext<S, L>) {
        if ctx
            .send_direct(
                self.sender,
                &DirectMessage::Failed {
                    transfer_id: self.id,
                },
                None,
            )
            .is_err()
            && self.length == 0
        {
            ctx.effects().set_timer(
                TransferTimer::Send {
                    transfer_id: self.id,
                },
                RESEND_AFTER,
            );
        }
    }

    fn validate_ack(&self, ack: &TransferAck, sender: PeerId) -> Result<(), Error> {
        if ack.domain != ACK_DOMAIN
            || ack.version != ACK_VERSION
            || ack.transfer_id != self.id
            || ack.sender != sender
            || ack.hash != self.hash
            || ack.length != self.length
            || ack.chunk_size != self.chunk_size
            || ack.sequence == 0
            || ack.accepted > self.length
            || (ack.complete && ack.accepted != self.length)
            || (ack.accepted != self.length
                && !ack.accepted.is_multiple_of(u64::from(self.chunk_size)))
            || (ack.accepted == 0 && !(self.length == 0 && ack.complete))
        {
            return Err(Error::InvalidAck);
        }
        Ok(())
    }
}

fn monotonic(previous: Option<&TransferAck>, next: &TransferAck) -> Result<bool, Error> {
    let Some(previous) = previous else {
        return Ok(true);
    };
    if next.sequence == previous.sequence {
        return if previous == next {
            Ok(false)
        } else {
            Err(Error::ConflictingAck)
        };
    }
    if next.sequence < previous.sequence {
        return if next.accepted <= previous.accepted {
            Ok(false)
        } else {
            Err(Error::DecreasingAck)
        };
    }
    if next.accepted < previous.accepted || (previous.complete && !next.complete) {
        return Err(Error::DecreasingAck);
    }
    Ok(true)
}

fn checkpoint_due(
    accepted: u64,
    complete: bool,
    acknowledged: u64,
    last_checkpointed: u64,
    chunk_size: u32,
    checkpoint_every: u32,
) -> bool {
    complete
        || accepted.saturating_sub(acknowledged.max(last_checkpointed))
            >= u64::from(chunk_size) * u64::from(checkpoint_every)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transfer(length: u64, chunk_size: u32) -> VerifiedTransfer {
        VerifiedTransfer::new(
            7,
            Participant::new(0),
            Participant::new(1),
            BlobHash([9; 32]),
            length,
            chunk_size,
            4,
        )
        .unwrap()
    }

    fn ack(terms: &VerifiedTransfer, sequence: u64, accepted: u64, complete: bool) -> TransferAck {
        TransferAck {
            domain: ACK_DOMAIN,
            version: ACK_VERSION,
            transfer_id: terms.id,
            sender: PeerId([1; 32]),
            hash: terms.hash,
            length: terms.length,
            chunk_size: terms.chunk_size,
            sequence,
            accepted,
            complete,
        }
    }

    #[test]
    fn terms_are_checked() {
        for (sender, receiver, length, chunk, cadence) in [
            (0, 0, 1, 1, 1),
            (0, 1, MAX_BLOB_BYTES + 1, 1, 1),
            (0, 1, 1, 0, 1),
            (0, 1, 1, MAX_DIRECT_RANGE_BYTES as u32 + 1, 1),
            (0, 1, 1, 1, 0),
        ] {
            assert!(matches!(
                VerifiedTransfer::new(
                    7,
                    Participant::new(sender),
                    Participant::new(receiver),
                    BlobHash([9; 32]),
                    length,
                    chunk,
                    cadence
                ),
                Err(Error::Configuration)
            ));
        }
        let terms = transfer(MAX_BLOB_BYTES, MAX_DIRECT_RANGE_BYTES as u32);
        assert_eq!(terms.id(), 7);
        assert_eq!(terms.sender(), Participant::new(0));
        assert_eq!(terms.receiver(), Participant::new(1));
        assert_eq!(terms.acknowledged(), 0);
        assert!(!terms.is_complete());
        assert!(!terms.is_failed());
    }

    #[test]
    fn chunk_geometry_covers_the_object_with_a_short_last_chunk() {
        for length in [0, 1, 1023, 1024, 1025, 50_000, 50_001, 1_000_000] {
            let terms = transfer(length, 50_000);
            let mut prefix = 0;
            for index in 0..terms.chunk_count() {
                let range = terms.chunk_range(index).unwrap();
                assert_eq!(range.start, prefix);
                assert!(range.end > range.start && range.end <= length);
                assert!(range.end - range.start <= 50_000);
                prefix = range.end;
            }
            assert_eq!(prefix, length);
            assert_eq!(terms.chunk_count(), length.div_ceil(50_000));
            assert!(matches!(
                terms.chunk_range(terms.chunk_count()),
                Err(Error::ChunkOutOfRange)
            ));
        }
        assert_eq!(
            transfer(50_001, 50_000).chunk_range(1).unwrap(),
            50_000..50_001
        );
        let mut oversized = transfer(1, 2);
        oversized.length = u64::MAX;
        assert!(matches!(
            oversized.chunk_range(u64::MAX),
            Err(Error::Overflow)
        ));
        assert!(matches!(
            oversized.chunk_range(u64::MAX / 2),
            Err(Error::Overflow)
        ));
    }

    #[test]
    fn acknowledgements_bind_one_transfer() {
        let terms = transfer(100_001, 50_000);
        let valid = ack(&terms, 1, 50_000, false);
        terms.validate_ack(&valid, PeerId([1; 32])).unwrap();
        let mutations: &[fn(&mut TransferAck)] = &[
            |a| a.domain[0] ^= 1,
            |a| a.version += 1,
            |a| a.transfer_id += 1,
            |a| a.sender = PeerId([2; 32]),
            |a| a.hash = BlobHash([8; 32]),
            |a| a.length += 1,
            |a| a.chunk_size += 1,
            |a| a.sequence = 0,
            |a| a.accepted = 0,
            |a| a.accepted = 999,
            |a| a.accepted = 100_002,
            |a| a.complete = true,
        ];
        for mutate in mutations {
            let mut invalid = valid.clone();
            mutate(&mut invalid);
            assert!(matches!(
                terms.validate_ack(&invalid, PeerId([1; 32])),
                Err(Error::InvalidAck)
            ));
        }
        assert!(terms.validate_ack(&valid, PeerId([2; 32])).is_err());
        let mut other = terms.clone();
        other.id += 1;
        assert!(other.validate_ack(&valid, PeerId([1; 32])).is_err());
        // Holding the full prefix does not itself claim final publication.
        for complete in [false, true] {
            terms
                .validate_ack(&ack(&terms, 3, terms.length, complete), PeerId([1; 32]))
                .unwrap();
        }
        let empty = transfer(0, 1);
        empty
            .validate_ack(&ack(&empty, 1, 0, true), PeerId([1; 32]))
            .unwrap();
        assert!(
            empty
                .validate_ack(&ack(&empty, 1, 0, false), PeerId([1; 32]))
                .is_err()
        );
    }

    #[test]
    fn monotonic_orders_and_rejects_conflicts() {
        let terms = transfer(100_001, 50_000);
        let first = ack(&terms, 1, 50_000, false);
        let later = ack(&terms, 2, 100_000, false);
        assert!(monotonic(None, &first).unwrap());
        assert!(monotonic(Some(&first), &later).unwrap());
        assert!(!monotonic(Some(&later), &first).unwrap());
        assert!(!monotonic(Some(&first), &first).unwrap());
        assert!(matches!(
            monotonic(Some(&first), &ack(&terms, 1, 100_000, false)),
            Err(Error::ConflictingAck)
        ));
        assert!(matches!(
            monotonic(Some(&later), &ack(&terms, 3, 50_000, false)),
            Err(Error::DecreasingAck)
        ));
        assert!(matches!(
            monotonic(Some(&first), &ack(&terms, 0, 100_000, false)),
            Err(Error::DecreasingAck)
        ));
        let full = ack(&terms, 3, terms.length, false);
        let complete = ack(&terms, 4, terms.length, true);
        assert!(monotonic(Some(&full), &complete).unwrap());
        assert!(matches!(
            monotonic(Some(&complete), &ack(&terms, 5, terms.length, false)),
            Err(Error::DecreasingAck)
        ));
        let mut conflict = complete.clone();
        conflict.hash = BlobHash([8; 32]);
        assert!(matches!(
            monotonic(Some(&complete), &conflict),
            Err(Error::ConflictingAck)
        ));
    }

    #[test]
    fn checkpoint_cadence() {
        assert!(!checkpoint_due(150_000, false, 0, 0, 50_000, 4));
        assert!(checkpoint_due(200_000, false, 0, 0, 50_000, 4));
        // The other transfer may hold the writer: queued checkpoints still
        // count toward cadence even while acknowledged() remains at zero.
        assert!(!checkpoint_due(250_000, false, 0, 200_000, 50_000, 4));
        assert!(!checkpoint_due(350_000, false, 0, 200_000, 50_000, 4));
        assert!(checkpoint_due(400_000, false, 0, 200_000, 50_000, 4));
        assert!(!checkpoint_due(450_000, false, 400_000, 200_000, 50_000, 4));
        assert!(checkpoint_due(600_000, false, 400_000, 200_000, 50_000, 4));
        assert!(checkpoint_due(450_001, true, 400_000, 400_000, 50_000, 4));
        assert!(checkpoint_due(0, true, 0, 0, 50_000, 4));
        assert!(!checkpoint_due(1, false, 2, 3, 1, 1));
        assert!(!checkpoint_due(
            u64::from(u32::MAX),
            false,
            0,
            0,
            u32::MAX,
            u32::MAX
        ));
    }
}
