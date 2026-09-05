//! Content-addressed program topic.

use crate::id::id_type;

id_type!(
    /// A 32-byte content-addressed topic for negotiation facts, typically the
    /// program's content hash.
    pub struct Hash,
    Default
);
