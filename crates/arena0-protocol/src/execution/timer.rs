use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// Domain separator for timer identities.
pub(crate) const TIMER_DOMAIN: &[u8] = b"arena0/timer/v2";

/// Identity of a timer, distinct from a protocol frame or pending operation.
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
#[serde(transparent)]
pub struct TimerId([u8; 32]);

impl TimerId {
    /// Derive a timer identity from a stable event/effect coordinate.
    #[must_use]
    pub fn derive(label: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(TIMER_DOMAIN);
        hasher.update(label);
        Self(*hasher.finalize().as_bytes())
    }

    /// Construct an id from persisted bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the id bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}
