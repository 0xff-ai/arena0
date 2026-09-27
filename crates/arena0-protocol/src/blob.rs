//! Blob and direct-message values that programs hold.
//!
//! Guest-visible: a program names stored objects and received slices only by
//! these small values. File bytes and Bao proofs never enter Wasm.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// Content address of one immutable object: the BLAKE3 (Bao root) hash of its
/// bytes.
#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
pub struct BlobHash(pub [u8; 32]);

/// A program's durable name for one stored or in-progress object.
///
/// The Host derives it from the dispatch that minted it (its event position
/// and the blob call's index in that dispatch), so rerunning a dispatch after a
/// crash mints the same handles. Handles are scoped to one execution.
#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
pub struct BlobHandle {
    pub event_position: u64,
    pub call_index: u32,
}

/// A received slice's token, valid only during the dispatch that delivered
/// it. The slice bytes stay in the Host; `accept_range` consumes the token.
#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
pub struct Attachment(pub u32);

/// The object range a direct message carries. The Host Bao-encodes it from the
/// immutable `source` when it sends the frame.
#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
pub struct RangeAttachment {
    pub source: BlobHandle,
    pub start: u64,
    pub end: u64,
}

/// Why a blob operation did nothing.
///
/// The Borsh tags are part of the ABI: blob imports return `0` for success and
/// `tag + 1` for an error.
#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    thiserror::Error,
)]
pub enum BlobError {
    /// The store lacks the object, or the handle names nothing.
    #[error("no such blob")]
    NotFound,
    /// The object exceeds `MAX_BLOB_BYTES` or the store's capacity.
    #[error("blob quota exceeded")]
    Quota,
    /// The range is empty, out of bounds, not what the operation expects, or
    /// over `MAX_DIRECT_RANGE_BYTES`.
    #[error("bad blob range")]
    BadRange,
    /// The slice does not prove the range against the object's hash, or the
    /// attachment is missing or already used.
    #[error("bad blob slice")]
    BadSlice,
    /// `commit` found bytes the output does not hold yet.
    #[error("blob incomplete")]
    Incomplete,
}
