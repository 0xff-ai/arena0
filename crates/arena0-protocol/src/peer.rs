//! Peer identity.

use crate::id::id_type;

id_type!(
    /// A 32-byte Host identity: its durable Ed25519 public key. A fresh
    /// execution BLS key is bound to this identity in its signed negotiation
    /// ticket; the BLS key is not the identity.
    pub struct Id
);

impl Id {
    /// A peer's identity is its durable Ed25519 public identity key.
    #[must_use]
    pub const fn from_ed25519(identity: &arena0_crypto::AgentPubKey) -> Self {
        Self(identity.0)
    }
}

/// Conversion from Host key custody to the protocol's peer identity.
pub trait IdSource {
    fn peer_id(&self) -> Id;
}

impl IdSource for arena0_crypto::NodeKeys {
    fn peer_id(&self) -> Id {
        Id::from_ed25519(&self.ed25519_public_key())
    }
}
