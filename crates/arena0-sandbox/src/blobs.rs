//! The blob store as one dispatch sees it, supplied by the Host.

use arena0_protocol::BlobHash;
use arena0_protocol::execution::BlobPartial;
use std::ops::Range;

/// The committed blob store as one execution's dispatch sees it. The Host
/// implements it with short synchronous store reads and file reads; an `Err`
/// (a store or task failure, not a missing blob) traps the dispatch.
///
/// Every method answers for the execution the view was built for. Changes the
/// dispatch stages are not visible here; the sandbox overlays them.
pub trait BlobView: Send + Sync {
    /// The length of `hash` if it is granted to this execution.
    fn granted(&self, hash: BlobHash) -> Result<Option<u64>, String>;
    /// Bytes `range` of blob `hash`, after the caller has checked [`Self::granted`]
    /// and bounded the range by that length. This method does not recheck the
    /// grant. Grants remain valid for the duration of a dispatch.
    /// `Ok(None)` when the file cannot supply the whole range (missing,
    /// unreadable, or shorter than `range.end`).
    fn read_granted(&self, hash: BlobHash, range: Range<u64>) -> Result<Option<Vec<u8>>, String>;
    /// This execution's partial object for `hash`, if it received any of it.
    fn partial(&self, hash: BlobHash) -> Result<Option<BlobPartial>, String>;
    /// BLAKE3 of the partial's durable bytes `[0, written)` followed by `tail`.
    /// The caller has checked that the partial exists.
    fn hash_partial(&self, hash: BlobHash, tail: &[u8]) -> Result<BlobHash, String>;
}
