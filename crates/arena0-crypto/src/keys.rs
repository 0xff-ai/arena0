use ed25519_dalek::Signer;
#[cfg(not(target_arch = "wasm32"))]
use zeroize::Zeroizing;

#[cfg(not(target_arch = "wasm32"))]
use super::ExecutionSalt;
use super::{AgentPubKey, SecretKey};
use crate::{CryptoError, Ed25519Signature};

/// This node's durable signing keys.
///
/// Ed25519 identifies the node to the protocol layer. Execution keys have a
/// separate owner and lifecycle. Hashing and signature verification are the free functions
/// [`hash`](super::hash) and [`verify`](super::verify).
pub struct NodeKeys {
    ed25519_keypair: ed25519_dalek::SigningKey,
}

impl std::fmt::Debug for NodeKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeKeys").finish_non_exhaustive()
    }
}

impl NodeKeys {
    /// Create keys from the persistent node secret.
    /// Execution keys are created separately from a persisted execution salt.
    #[must_use]
    pub fn from_secret(secret: SecretKey) -> Self {
        Self {
            ed25519_keypair: ed25519_dalek::SigningKey::from_bytes(secret.as_bytes()),
        }
    }

    /// Return the Ed25519 public identity key.
    #[must_use]
    pub fn ed25519_public_key(&self) -> AgentPubKey {
        AgentPubKey(self.ed25519_keypair.verifying_key().to_bytes())
    }

    /// Sign `data` with this node's Ed25519 identity key.
    #[must_use]
    pub fn sign(&self, data: &[u8]) -> Ed25519Signature {
        Ed25519Signature(self.ed25519_keypair.sign(data).to_bytes())
    }
}

/// Execution-scoped BLS signing capability derived from a durable local salt.
#[cfg(not(target_arch = "wasm32"))]
pub struct ExecutionKey(super::bls::BlsSecretKey);

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Debug for ExecutionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionKey").finish_non_exhaustive()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl ExecutionKey {
    /// Derive the execution key from random local entropy and its public scope.
    pub fn derive(
        salt: &ExecutionSalt,
        exec_id: &[u8; 32],
        negotiation_id: &[u8; 32],
    ) -> Result<Self, CryptoError> {
        let mut material = Zeroizing::new([0u8; 96]);
        material[..32].copy_from_slice(salt.as_bytes());
        material[32..64].copy_from_slice(exec_id);
        material[64..].copy_from_slice(negotiation_id);
        let seed = Zeroizing::new(blake3::derive_key(
            "arena0 execution BLS key v2",
            &material[..],
        ));
        super::bls::BlsSecretKey::from_seed(&seed).map(Self)
    }

    /// This execution's public BLS key.
    #[must_use]
    pub fn public_key(&self) -> crate::BlsPublicKey {
        self.0.public_key()
    }

    /// Sign one execution commitment.
    #[must_use]
    pub fn sign(&self, data: &[u8]) -> crate::BlsSignature {
        self.0.sign(data)
    }

    /// Bind this execution key to an offer and node identity.
    #[must_use]
    pub fn key_binding(&self, offer_hash: &[u8; 32], signer: &[u8; 32]) -> crate::BlsSignature {
        let public = self.public_key();
        self.0
            .sign_binding(&key_binding_message(offer_hash, signer, &public))
    }
}

/// Fixed domain prefix of the key-binding message (§2):
/// `BLS_BINDING_DOMAIN || offer_hash || signer || execution_bls`. Version 2
/// identifies the offer-scoped, BLS-signed binding generation.
pub const BLS_BINDING_DOMAIN: &[u8] = b"arena0/execution-key-binding/v2";

/// The canonical key-binding message (§2):
/// `BLS_BINDING_DOMAIN || offer_hash || signer || execution_bls`.
/// The signer is the ticket issuer's `PeerId`; the binding is BLS-signed with
/// the dedicated binding ciphersuite.
#[must_use]
pub fn key_binding_message(
    offer_hash: &[u8; 32],
    signer: &[u8; 32],
    execution_bls: &crate::BlsPublicKey,
) -> Vec<u8> {
    let mut m = Vec::with_capacity(BLS_BINDING_DOMAIN.len() + 32 + 32 + 96);
    m.extend_from_slice(BLS_BINDING_DOMAIN);
    m.extend_from_slice(offer_hash);
    m.extend_from_slice(signer);
    m.extend_from_slice(&execution_bls.0);
    m
}

/// Verify a scope-bound key binding: `binding` must be a BLS signature over
/// `BLS_BINDING_DOMAIN || offer_hash || signer || execution_bls` under
/// `execution_bls` with the dedicated binding ciphersuite. `Ok(false)` for a
/// valid key with a non-matching binding, `Err` for a malformed key.
pub fn verify_key_binding(
    offer_hash: &[u8; 32],
    signer: &[u8; 32],
    execution_bls: &crate::BlsPublicKey,
    binding: &crate::BlsSignature,
) -> Result<bool, CryptoError> {
    let msg = key_binding_message(offer_hash, signer, execution_bls);
    execution_bls.verify_binding(&msg, binding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify;

    fn keys() -> NodeKeys {
        NodeKeys::from_secret(SecretKey::from_bytes([7u8; 32]))
    }

    #[test]
    fn ed25519_sign_and_verify_round_trip() {
        let keys = keys();
        let data = b"sign me";
        let public_key = keys.ed25519_public_key();
        let sig = keys.sign(data);
        assert!(verify(crate::SignScheme::Ed25519, &public_key.0, data, &sig.0).expect("verify"));
        assert!(
            !verify(
                crate::SignScheme::Ed25519,
                &public_key.0,
                b"tampered",
                &sig.0
            )
            .expect("verify")
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn execution_key_binding_binds_offer_scope() {
        let offer_hash = [3u8; 32];
        let other_offer = [4u8; 32];
        let keys = keys();
        let execution = ExecutionKey::derive(
            &ExecutionSalt::try_from_bytes([9; 32]).expect("non-zero test salt"),
            &[1; 32],
            &[2; 32],
        )
        .expect("bls key");
        let execution_bls = execution.public_key();
        let binding = execution.key_binding(&offer_hash, &keys.ed25519_public_key().0);
        assert!(
            verify_key_binding(
                &offer_hash,
                &keys.ed25519_public_key().0,
                &execution_bls,
                &binding
            )
            .unwrap()
        );
        // A different offer hash breaks the binding.
        assert!(
            !verify_key_binding(
                &other_offer,
                &keys.ed25519_public_key().0,
                &execution_bls,
                &binding
            )
            .unwrap()
        );
        // A different signer breaks the binding.
        assert!(!verify_key_binding(&offer_hash, &[7; 32], &execution_bls, &binding).unwrap());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn execution_key_binding_golden_vector() {
        let offer_hash = [3u8; 32];
        let signer = keys().ed25519_public_key().0;
        let execution = ExecutionKey::derive(
            &ExecutionSalt::try_from_bytes([9; 32]).expect("non-zero test salt"),
            &[1; 32],
            &[2; 32],
        )
        .expect("bls key");
        let public = execution.public_key();
        let message = key_binding_message(&offer_hash, &signer, &public);
        let binding = execution.key_binding(&offer_hash, &signer);

        assert_eq!(
            hex::encode(public.0),
            concat!(
                "b4435255bfadd618f179ad3f0456db32b6fbce36a3cab8c17524fd9549e28746",
                "599543b71ab3c5bf433dfcdad047dda70d4a10c868cad3a59ffebaaf44d524ccb",
                "04750cfffc8e030dd746790010e3439f228c1d8c111541a144d56b421496996",
            )
        );
        assert_eq!(
            hex::encode(&message),
            concat!(
                "6172656e61302f657865637574696f6e2d6b65792d62696e64696e672f7632",
                "0303030303030303030303030303030303030303030303030303030303030303",
                "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c",
                "b4435255bfadd618f179ad3f0456db32b6fbce36a3cab8c17524fd9549e28746",
                "599543b71ab3c5bf433dfcdad047dda70d4a10c868cad3a59ffebaaf44d524ccb",
                "04750cfffc8e030dd746790010e3439f228c1d8c111541a144d56b421496996",
            )
        );
        assert_eq!(
            hex::encode(binding.0),
            concat!(
                "b46de8ae1626f54bcddc111affae8316e828f357f637513a73d0d1675f87ad87",
                "58065d91fc2ca85abbf40a500752e6fb",
            )
        );

        let mut wrong_domain = message;
        wrong_domain[BLS_BINDING_DOMAIN.len() - 1] = b'1';
        assert!(!public.verify_binding(&wrong_domain, &binding).unwrap());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn execution_key_is_stable_for_scope_and_changes_with_salt() {
        let first = ExecutionKey::derive(
            &ExecutionSalt::try_from_bytes([1; 32]).expect("non-zero test salt"),
            &[2; 32],
            &[3; 32],
        )
        .unwrap();
        let repeated = ExecutionKey::derive(
            &ExecutionSalt::try_from_bytes([1; 32]).expect("non-zero test salt"),
            &[2; 32],
            &[3; 32],
        )
        .unwrap();
        let different = ExecutionKey::derive(
            &ExecutionSalt::try_from_bytes([4; 32]).expect("non-zero test salt"),
            &[2; 32],
            &[3; 32],
        )
        .unwrap();

        assert_eq!(first.public_key(), repeated.public_key());
        assert_ne!(first.public_key(), different.public_key());
    }
}
