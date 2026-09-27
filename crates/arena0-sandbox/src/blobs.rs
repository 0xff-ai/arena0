//! The committed blob view supplied by the Host for one dispatch.

use arena0_protocol::execution::BlobResource;
use arena0_protocol::{BlobHandle, BlobHash};
use std::ops::Range;

/// The committed blob store as one dispatch sees it. The Host implements it with
/// short synchronous store reads; an Err traps the dispatch.
pub trait BlobView: Send + Sync {
    fn contains(&self, hash: BlobHash, length: u64) -> Result<bool, String>;
    fn resource(&self, handle: BlobHandle) -> Result<Option<BlobResource>, String>;
    /// Written ranges of an uncommitted output, sorted by start.
    fn written(&self, handle: BlobHandle) -> Result<Vec<Range<u64>>, String>;
}
