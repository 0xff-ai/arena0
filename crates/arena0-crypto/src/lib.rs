//! Cryptographic types and operations for arena0.
//!
//! Hashing and signature verification are free functions: they do not belong to
//! a node. [`NodeKeys`] holds only the node's durable Ed25519 identity;
//! [`ExecutionKey`] is a separate host-only BLS signer derived from a persisted
//! random [`ExecutionSalt`].

mod keys;

/// Host-only BLS12-381 (MinSig) aggregate signing and verification.
#[cfg(not(target_arch = "wasm32"))]
pub mod bls;

#[cfg(not(target_arch = "wasm32"))]
pub use keys::ExecutionKey;
pub use keys::NodeKeys;
pub use keys::{BLS_BINDING_DOMAIN, key_binding_message, verify_key_binding};

use core::fmt;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use thiserror::Error;
use tiny_keccak::Hasher as _;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// An Ed25519 public identity key.
#[derive(
    BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default,
)]
pub struct AgentPubKey(pub [u8; 32]);

/// An Ed25519 secret key seed.
///
/// The seed is intentionally opaque and is wiped when this value is dropped.
/// Callers can construct one only from an explicitly sized byte array and can
/// borrow its bytes for the duration of an operation; there is no owned-byte
/// extraction API.
///
/// ```compile_fail
/// use arena0_crypto::SecretKey;
///
/// fn cannot_clone(key: SecretKey) {
///     let _ = key.clone();
/// }
/// ```
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SecretKey([u8; 32]);

impl SecretKey {
    /// Construct a key from exactly 32 bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the key seed for an operation without transferring ownership.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl TryFrom<&[u8]> for SecretKey {
    type Error = CryptoError;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| CryptoError::InvalidKeyLength {
                expected: 32,
                actual: bytes.len(),
            })?;
        Ok(Self::from_bytes(bytes))
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SecretKey").field(&"[redacted]").finish()
    }
}

/// Random daemon-local entropy persisted for one execution.
///
/// The salt is intentionally opaque and is wiped when this value is dropped.
///
/// ```compile_fail
/// use arena0_crypto::ExecutionSalt;
///
/// fn cannot_clone(salt: ExecutionSalt) {
///     let _ = salt.clone();
/// }
/// ```
#[derive(Zeroize, ZeroizeOnDrop, PartialEq, Eq)]
pub struct ExecutionSalt([u8; 32]);

impl ExecutionSalt {
    /// Construct a non-zero salt from exactly 32 bytes.
    #[must_use = "the result must be checked for a zero salt"]
    pub fn try_from_bytes(bytes: [u8; 32]) -> Result<Self, CryptoError> {
        if bytes == [0; 32] {
            Err(CryptoError::ZeroExecutionSalt)
        } else {
            Ok(Self(bytes))
        }
    }

    /// Borrow the salt for a derivation without transferring ownership.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl TryFrom<[u8; 32]> for ExecutionSalt {
    type Error = CryptoError;

    fn try_from(bytes: [u8; 32]) -> Result<Self, Self::Error> {
        Self::try_from_bytes(bytes)
    }
}

impl TryFrom<&[u8]> for ExecutionSalt {
    type Error = CryptoError;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| CryptoError::InvalidKeyLength {
                expected: 32,
                actual: bytes.len(),
            })?;
        Self::try_from_bytes(bytes)
    }
}

impl fmt::Debug for ExecutionSalt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ExecutionSalt").field(&"[redacted]").finish()
    }
}

/// A BLS12-381 (MinSig) public key: a compressed G2 point (96 bytes). A fresh
/// per-execution key, minted per exec and bound to the node's durable Ed25519
/// identity; it is not itself the identity. Pure data so it is guest-safe; the
/// blst-backed operations are excluded on Wasm targets.
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlsPublicKey(pub [u8; 96]);

/// A BLS12-381 (MinSig) signature or proof-of-possession: a compressed G1 point
/// (48 bytes). Pure data; aggregation and verification are host-only.
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlsSignature(pub [u8; 48]);

/// An ed25519 signature (64 bytes), including identity signatures over tickets
/// and execution-key bindings.
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ed25519Signature(pub [u8; 64]);

#[cfg(target_arch = "wasm32")]
impl BlsPublicKey {
    /// BLS verification requires the host-only `blst` implementation.
    pub fn verify(&self, _msg: &[u8], _sig: &BlsSignature) -> Result<bool, CryptoError> {
        Err(bls_unavailable())
    }

    /// BLS key-binding verification requires the host-only `blst` implementation.
    pub fn verify_binding(&self, _msg: &[u8], _sig: &BlsSignature) -> Result<bool, CryptoError> {
        Err(bls_unavailable())
    }
}

#[cfg(target_arch = "wasm32")]
impl BlsSignature {
    /// BLS aggregation requires the host-only `blst` implementation.
    pub fn aggregate(_sigs: &[Self]) -> Result<Self, CryptoError> {
        Err(bls_unavailable())
    }

    /// BLS aggregate verification requires the host-only `blst` implementation.
    pub fn fast_aggregate_verify(
        &self,
        _msg: &[u8],
        _pubkeys: &[BlsPublicKey],
    ) -> Result<bool, CryptoError> {
        Err(bls_unavailable())
    }
}

/// serde and display for fixed-size byte arrays as hex (serde derives only support
/// arrays up to 32 bytes, so these are hand-rolled).
macro_rules! hex_bytes {
    ($name:ident, $n:literal) => {
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({}…)", stringify!($name), hex::encode(&self.0[..4]))
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", hex::encode(self.0))
            }
        }
        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&hex::encode(self.0))
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = <String as Deserialize>::deserialize(d)?;
                let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
                let arr: [u8; $n] = bytes.as_slice().try_into().map_err(|_| {
                    serde::de::Error::custom(format!("expected {} bytes, got {}", $n, bytes.len()))
                })?;
                Ok($name(arr))
            }
        }
    };
}

hex_bytes!(BlsPublicKey, 96);
hex_bytes!(BlsSignature, 48);
hex_bytes!(Ed25519Signature, 64);
hex_bytes!(AgentPubKey, 32);

impl schemars::JsonSchema for AgentPubKey {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("AgentPubKey")
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[0-9a-f]{64}$"
        })
    }
}

/// Hash algorithms available to guest programs and Hosts.
#[derive(
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
pub enum HashAlgorithm {
    Blake3,
    Sha256,
    Keccak256,
}

/// Digital signature schemes available at the arena0 ABI boundary.
#[derive(
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
pub enum SignScheme {
    Ed25519,
    Bls,
}

impl SignScheme {
    /// Return the stable key used for canonical ordering of signing schemes.
    #[must_use]
    pub const fn canonical_order(self) -> u8 {
        match self {
            Self::Ed25519 => 0,
            Self::Bls => 1,
        }
    }
}

/// Cryptographic operation errors.
#[derive(Error, Debug, Clone)]
#[non_exhaustive]
pub enum CryptoError {
    #[error("unsupported algorithm: {0}")]
    UnsupportedAlgorithm(String),
    #[error("invalid key length: expected {expected}, got {actual}")]
    InvalidKeyLength { expected: usize, actual: usize },
    #[error("signing failed: {0}")]
    SigningFailed(String),
    #[error("verification failed: {0}")]
    VerificationFailed(String),
    #[error("invalid signature")]
    InvalidSignature,
    #[error("execution salt must not be zero")]
    ZeroExecutionSalt,
}

impl CryptoError {
    #[track_caller]
    pub fn verification_failed(err: impl core::fmt::Display) -> Self {
        Self::VerificationFailed(err.to_string())
    }
}

#[cfg(target_arch = "wasm32")]
fn bls_unavailable() -> CryptoError {
    CryptoError::UnsupportedAlgorithm("Bls is unavailable on Wasm".into())
}

/// Hash `data` with the requested algorithm. Wasm-safe (blake3, SHA-256, and
/// Keccak-256 only involve pure-Rust deps already linked by the guest SDK), and
/// infallible: every [`HashAlgorithm`] variant always produces a digest. Shared
/// by the SDK's guest-side `Crypto::hash`.
#[must_use]
pub fn hash(algo: HashAlgorithm, data: &[u8]) -> [u8; 32] {
    match algo {
        HashAlgorithm::Blake3 => *blake3::hash(data).as_bytes(),
        HashAlgorithm::Sha256 => {
            let result = sha2::Sha256::digest(data);
            let mut out = [0u8; 32];
            out.copy_from_slice(&result);
            out
        }
        HashAlgorithm::Keccak256 => {
            let mut hasher = tiny_keccak::Keccak::v256();
            let mut out = [0u8; 32];
            hasher.update(data);
            hasher.finalize(&mut out);
            out
        }
    }
}

/// Verify `sig` over `data` against the given public `key`.
///
/// Returns `Ok(false)` for a valid key with a non-matching signature, and
/// `Err` for malformed keys or signatures that cannot be parsed. Local BLS
/// verification is unavailable on Wasm targets.
pub fn verify(
    scheme: SignScheme,
    key: &[u8],
    data: &[u8],
    sig: &[u8],
) -> Result<bool, CryptoError> {
    match scheme {
        SignScheme::Ed25519 => {
            let verifying_key =
                ed25519_dalek::VerifyingKey::from_bytes(key.try_into().map_err(|_| {
                    CryptoError::InvalidKeyLength {
                        expected: 32,
                        actual: key.len(),
                    }
                })?)
                .map_err(CryptoError::verification_failed)?;

            let signature = ed25519_dalek::Signature::from_bytes(
                sig.try_into().map_err(|_| CryptoError::InvalidSignature)?,
            );

            use ed25519_dalek::Verifier as _;
            Ok(verifying_key.verify(data, &signature).is_ok())
        }
        SignScheme::Bls => {
            #[cfg(not(target_arch = "wasm32"))]
            {
                let pk = crate::BlsPublicKey(key.try_into().map_err(|_| {
                    CryptoError::InvalidKeyLength {
                        expected: 96,
                        actual: key.len(),
                    }
                })?);
                let signature =
                    crate::BlsSignature(sig.try_into().map_err(|_| CryptoError::InvalidSignature)?);
                pk.verify(data, &signature)
            }
            #[cfg(target_arch = "wasm32")]
            {
                let _ = (key, data, sig);
                Err(CryptoError::UnsupportedAlgorithm(
                    "Bls is unavailable on Wasm".into(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_values_have_checked_construction_and_borrowing_access() {
        let secret = SecretKey::from_bytes([0xA5; 32]);
        assert_eq!(secret.as_bytes(), &[0xA5; 32]);
        let salt = ExecutionSalt::try_from_bytes([0x5A; 32]).expect("non-zero salt");
        assert_eq!(salt.as_bytes(), &[0x5A; 32]);

        let invalid_secret = SecretKey::try_from(&[0u8; 31][..]).unwrap_err();
        assert!(matches!(
            invalid_secret,
            CryptoError::InvalidKeyLength {
                expected: 32,
                actual: 31
            }
        ));
        let invalid_salt = ExecutionSalt::try_from(&[0u8; 33][..]).unwrap_err();
        assert!(matches!(
            invalid_salt,
            CryptoError::InvalidKeyLength {
                expected: 32,
                actual: 33
            }
        ));
        assert!(matches!(
            ExecutionSalt::try_from_bytes([0; 32]),
            Err(CryptoError::ZeroExecutionSalt)
        ));
        assert!(matches!(
            ExecutionSalt::try_from([0; 32]),
            Err(CryptoError::ZeroExecutionSalt)
        ));
    }

    #[test]
    fn secret_debug_is_redacted() {
        let secret = SecretKey::from_bytes([0xA5; 32]);
        let salt = ExecutionSalt::try_from_bytes([0x5A; 32]).expect("non-zero salt");
        let secret_debug = format!("{secret:?}");
        let salt_debug = format!("{salt:?}");
        assert_eq!(secret_debug, "SecretKey(\"[redacted]\")");
        assert_eq!(salt_debug, "ExecutionSalt(\"[redacted]\")");
        assert!(!secret_debug.contains("a5"));
        assert!(!salt_debug.contains("5a"));
    }

    #[test]
    fn hash_blake3_matches() {
        let data = b"arena0";
        assert_eq!(
            hash(HashAlgorithm::Blake3, data),
            *blake3::hash(data).as_bytes()
        );
    }

    #[test]
    fn sign_scheme_canonical_order_is_explicit_and_keeps_borsh_tags() {
        assert_eq!(SignScheme::Ed25519.canonical_order(), 0);
        assert_eq!(SignScheme::Bls.canonical_order(), 1);
        assert_eq!(borsh::to_vec(&SignScheme::Ed25519).unwrap(), vec![0]);
        assert_eq!(borsh::to_vec(&SignScheme::Bls).unwrap(), vec![1]);
    }
}
