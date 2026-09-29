//! Hash-named blob operations for local handlers.

use crate::effects;
use arena0_protocol::{Attachment, BlobError, BlobHash, ChainingValue, CvSource};
use std::marker::PhantomData;

/// Access to the Host's blob store. Blob bytes never enter the program: it
/// names blobs by hash and received bytes by their [`Attachment`] token.
///
/// A program reads only blobs granted to its execution: those its participant
/// granted when creating or joining the execution, and those it committed.
/// The Host verifies nothing on its own; a program that wants received bytes
/// checked hashes them with [`Blobs::subtree_cv`] and [`crate::merge_cv`], and
/// [`Blobs::commit`] checks the whole object against its hash.
///
/// ```
/// use arena0::prelude::*;
/// fn receive(ctx: &mut LocalContext<(), ()>, hash: BlobHash, length: u64,
///            attachment: Attachment) -> Result<(), BlobError> {
///     let mut blobs = ctx.blobs();
///     blobs.append(hash, length, attachment)?;
///     blobs.commit(hash)
/// }
/// let declared = arena0::__arena0_capability_vec!(Blobs, Messaging, Timers);
/// assert!(declared.contains(&Capability::Blobs));
/// ```
#[derive(Debug)]
pub struct Blobs<'a> {
    _ctx: PhantomData<&'a mut ()>,
}

impl Blobs<'_> {
    pub(crate) fn new() -> Self {
        Self { _ctx: PhantomData }
    }

    /// Append the attachment's bytes to this execution's partial object
    /// `(hash, length)`, creating the partial on its first append. The bytes
    /// land at the partial's written offset and become durable with the
    /// dispatch. Fails with `Quota` over `MAX_BLOB_BYTES`, `BadAttachment`
    /// for a token this dispatch did not deliver, and `BadRange` when `length`
    /// differs from the partial's, the bytes would pass `length`, or this
    /// execution already committed `hash`.
    pub fn append(
        &mut self,
        hash: BlobHash,
        length: u64,
        attachment: Attachment,
    ) -> Result<(), BlobError> {
        effects::host_blob_append(hash, length, attachment)
    }

    /// Check that the partial object for `hash` is complete and hashes to
    /// `hash`, then publish it and grant it to this execution with the
    /// dispatch. Fails with `NotFound` without a partial, `BadRange` when this
    /// execution already committed `hash`, `Incomplete` before all bytes
    /// arrived, and `Mismatch` when the bytes hash differently.
    pub fn commit(&mut self, hash: BlobHash) -> Result<(), BlobError> {
        effects::host_blob_commit(hash)
    }

    /// The BLAKE3 chaining value of `source` as the subtree starting at byte
    /// `offset` of a larger input. `offset` must be a multiple of 1 KiB and the
    /// source no longer than the subtree that can start there and no longer
    /// than `MAX_DIRECT_RANGE_BYTES` (`BadRange`). A blob source must be
    /// granted and readable (`NotFound`).
    pub fn subtree_cv(
        &mut self,
        source: CvSource,
        offset: u64,
    ) -> Result<ChainingValue, BlobError> {
        effects::host_subtree_cv(source, offset)
    }
}
