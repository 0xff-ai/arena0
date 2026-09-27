//! Handles-only blob operations for local handlers.

use crate::effects;
use arena0_protocol::{Attachment, BlobError, BlobHandle, BlobHash};
use std::{marker::PhantomData, ops::Range};

/// Handles-only access to the Host's blob store. File bytes never enter the program.
///
/// Local handlers can receive, commit, and forward objects through handles:
/// ```
/// use arena0::prelude::*;
/// fn receive(ctx: &mut LocalContext<(), ()>, from: Participant, hash: BlobHash,
///            length: u64, slice: Attachment) -> Result<(), ProgramFault> {
///     let output = {
///         let mut blobs = ctx.blobs();
///         let output = blobs.create(hash, length)?;
///         blobs.accept_range(output, slice, 0..length)?;
///         blobs.commit(output)?;
///         output
///     };
///     ctx.send_direct(from, &(), Some(RangeAttachment { source: output, start: 0, end: length }))?;
///     Ok(())
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

    /// Name stored content `(hash, length)`; fails if the Host lacks it.
    pub fn resolve(&mut self, hash: BlobHash, length: u64) -> Result<BlobHandle, BlobError> {
        effects::host_blob_resolve(hash, length)
    }

    /// Create an output permanently bound to `(hash, length)`.
    pub fn create(&mut self, hash: BlobHash, length: u64) -> Result<BlobHandle, BlobError> {
        effects::host_blob_create(hash, length)
    }

    /// Verify and stage a range under the output's bound hash and length.
    /// Once the Host checks the attachment, it consumes it even if later range
    /// or proof checks fail. Successful writes become durable with the dispatch.
    pub fn accept_range(
        &mut self,
        output: BlobHandle,
        slice: Attachment,
        range: Range<u64>,
    ) -> Result<(), BlobError> {
        effects::host_blob_accept_range(output, slice, range)
    }

    /// Require full coverage and stage publication under the bound hash.
    /// A rejected dispatch discards the publication along with its writes.
    pub fn commit(&mut self, output: BlobHandle) -> Result<(), BlobError> {
        effects::host_blob_commit(output)
    }
}
