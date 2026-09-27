//! Blob-store changes staged by one dispatch.
//!
//! The sandbox stages these while a local handler runs; the store applies them
//! in the dispatch's transaction, so received bytes are durable exactly when
//! their transition is, and a rejected dispatch leaves none.

use crate::BlobHash;

/// One blob-store mutation, in call order within its dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobChange {
    /// Received bytes of object `(hash, length)` at `offset`. The first append
    /// of an object in an execution creates its partial; later appends
    /// continue at the partial's `written` offset.
    Append {
        hash: BlobHash,
        length: u64,
        offset: u64,
        bytes: Vec<u8>,
    },
    /// The execution's partial for `hash` is complete and the dispatch checked
    /// that its bytes hash to `hash`. Publishes it, grants it to the execution,
    /// and marks the partial committed.
    Commit { hash: BlobHash },
}

/// An object an execution is receiving or received: its declared length and
/// the bytes durably written so far, `[0, written)`. Once `committed`, the
/// execution can never append to or commit that hash again, so a published
/// file is never rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobPartial {
    pub length: u64,
    pub written: u64,
    pub committed: bool,
}
