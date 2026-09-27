use arena0_protocol::execution::BlobPartial;
use arena0_protocol::{BlobHash, ExecId};
use std::ops::Range;

/// The blob store as one execution's dispatch sees it, through short blocking
/// store reads and file reads on the actor's thread.
pub(super) struct StoreBlobView {
    pub(super) store: arena0_store::StoreHandle,
    pub(super) execution_id: ExecId,
}

impl arena0_sandbox::BlobView for StoreBlobView {
    fn granted(&self, hash: BlobHash) -> Result<Option<u64>, String> {
        self.store
            .blob_granted_blocking(self.execution_id, hash)
            .map_err(|error| error.to_string())
    }

    fn read(&self, hash: BlobHash, range: Range<u64>) -> Result<Option<Vec<u8>>, String> {
        if self.granted(hash)?.is_none() {
            return Ok(None);
        }
        self.store
            .read_blob_range_blocking(hash, range)
            .map_err(|error| error.to_string())
    }

    fn partial(&self, hash: BlobHash) -> Result<Option<BlobPartial>, String> {
        self.store
            .blob_partial_blocking(self.execution_id, hash)
            .map_err(|error| error.to_string())
    }

    fn hash_partial(&self, hash: BlobHash, tail: &[u8]) -> Result<BlobHash, String> {
        self.store
            .hash_blob_partial_blocking(self.execution_id, hash, tail)
            .map_err(|error| error.to_string())
    }
}
