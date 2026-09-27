use arena0_protocol::execution::BlobResource;
use arena0_protocol::{BlobHandle, BlobHash, ExecId};

/// The committed blob store as one dispatch sees it, through short blocking
/// store reads on the actor's thread.
pub(super) struct StoreBlobView {
    pub(super) store: arena0_store::StoreHandle,
    pub(super) execution_id: ExecId,
}

impl arena0_sandbox::BlobView for StoreBlobView {
    fn contains(&self, hash: BlobHash, length: u64) -> Result<bool, String> {
        self.store
            .blob_contains_blocking(hash, length)
            .map_err(|error| error.to_string())
    }

    fn resource(&self, handle: BlobHandle) -> Result<Option<BlobResource>, String> {
        self.store
            .blob_resource_blocking(self.execution_id, handle)
            .map_err(|error| error.to_string())
    }

    fn written(&self, handle: BlobHandle) -> Result<Vec<std::ops::Range<u64>>, String> {
        self.store
            .blob_written_blocking(self.execution_id, handle)
            .map_err(|error| error.to_string())
    }
}
