use arena0_protocol::BlobHash;
use serde::{Deserialize, Serialize};

/// `POST /uploads` reply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Uploaded {
    pub upload: BlobHash,
    pub length: u64,
}
