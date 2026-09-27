//! Blob-store changes staged by one dispatch.
//!
//! The sandbox stages these while a local handler runs; the store applies them
//! in the dispatch's transaction, so a write is durable exactly when its
//! transition is, and a rejected dispatch leaves none.

use crate::{BlobHandle, BlobHash};

/// One blob-store mutation, in call order within its dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobChange {
    /// A new output bound to object `(hash, length)`.
    Create {
        handle: BlobHandle,
        hash: BlobHash,
        length: u64,
    },
    /// Verified bytes for `output` at `offset`.
    Write {
        handle: BlobHandle,
        offset: u64,
        bytes: Vec<u8>,
    },
    /// `handle`'s output is complete; its content is published under its hash.
    Commit { handle: BlobHandle },
    /// A handle naming already stored content `(hash, length)`.
    Resolve {
        handle: BlobHandle,
        hash: BlobHash,
        length: u64,
    },
}
