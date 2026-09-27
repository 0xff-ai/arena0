//! Blob and direct-message values that programs hold.
//!
//! Guest-visible: a program names stored objects by content hash and a received
//! attachment by a dispatch-scoped token. Blob bytes never enter Wasm.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::id::id_type;

id_type!(
    /// Content address of one immutable object: the BLAKE3 hash of its bytes.
    /// JSON and display use 64 lowercase hex characters.
    pub struct BlobHash
);

/// The token for a received direct message's attachment bytes, valid for the
/// whole dispatch that delivered it. The bytes stay in the Host; `append` and
/// `subtree_cv` read them through the token.
#[derive(
    schemars::JsonSchema,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
)]
pub struct Attachment(pub u32);

/// The blob range a direct message carries. The blob must be granted to the
/// sending execution; the Host reads the raw bytes `[start, end)` when it sends
/// the frame and attaches them unmodified.
#[derive(
    schemars::JsonSchema,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
)]
pub struct RangeAttachment {
    pub hash: BlobHash,
    pub start: u64,
    pub end: u64,
}

/// Why a blob operation did nothing.
///
/// The Borsh tags are part of the ABI: blob imports return `0` for success and
/// `tag + 1` for an error.
#[derive(
    schemars::JsonSchema,
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
    /// The blob is not granted to this execution, its file cannot supply the
    /// requested bytes, or `commit` found no bytes received for the hash.
    #[error("no such blob")]
    NotFound,
    /// The object exceeds `MAX_BLOB_BYTES`.
    #[error("blob quota exceeded")]
    Quota,
    /// The range is empty, out of bounds, misaligned for a subtree, over
    /// `MAX_DIRECT_RANGE_BYTES`, or the operation does not fit the object's
    /// receive state (length differs, past the end, already committed by
    /// this execution).
    #[error("bad blob range")]
    BadRange,
    /// The token names no attachment of this dispatch, or the attachment is empty.
    #[error("bad blob attachment")]
    BadAttachment,
    /// `commit` found fewer received bytes than the object's length.
    #[error("blob incomplete")]
    Incomplete,
    /// `commit` found received bytes whose BLAKE3 hash differs from the hash.
    #[error("blob hash mismatch")]
    Mismatch,
}

/// A BLAKE3 chaining value: the non-root hash of one subtree.
pub type ChainingValue = [u8; 32];

/// The bytes `subtree_cv` hashes: a range of a blob granted to this execution,
/// or the current dispatch's attachment. Crosses the ABI Borsh-encoded.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CvSource {
    Blob {
        hash: BlobHash,
        start: u64,
        end: u64,
    },
    Attachment(Attachment),
}
