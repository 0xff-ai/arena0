//! BLS12-381 (MinSig) aggregate signing and verification for non-Wasm hosts.
//!
//! MinSig places signatures in G1 (48 bytes) and public keys in G2 (96 bytes),
//! the profile that suits arena0: few keys (stored once per session) but many
//! signatures (recorded per step), so the small object is the signature.
//! Proof-of-possession defends same-message aggregation against rogue keys.
//!
//! blst is C and host-only; Cargo excludes this module and dependency on Wasm
//! targets.

use blst::BLST_ERROR;
use blst::min_sig::{AggregateSignature, PublicKey, SecretKey, Signature};

use super::{BlsPublicKey, BlsSignature};
use crate::CryptoError;

/// Signing ciphersuite. MinSig hashes messages to G1; the `POP_` tag pairs with
/// proof-of-possession aggregation.
const DST_SIG: &[u8] = b"BLS_SIG_BLS12381G1_XMD:SHA-256_SSWU_RO_POP_";
/// Key-binding ciphersuite: the scope-bound possession proof that replaces the
/// generic copyable PoP. The `NUL_` tag marks a non-PoP, non-signing purpose.
const DST_BIND: &[u8] = b"BLS_BIND_BLS12381G1_XMD:SHA-256_SSWU_RO_NUL_";

/// A BLS secret key. Host-only and never serialized into a proof.
pub struct BlsSecretKey(SecretKey);

impl std::fmt::Debug for BlsSecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("BlsSecretKey").field(&"[redacted]").finish()
    }
}

impl BlsSecretKey {
    /// Derive a deterministic secret key from 32 bytes of execution-scoped key
    /// material (HKDF per the IRTF `KeyGen`).
    pub fn from_seed(seed: &[u8; 32]) -> Result<Self, CryptoError> {
        SecretKey::key_gen(seed, &[])
            .map(Self)
            .map_err(|e| CryptoError::SigningFailed(format!("bls key_gen: {e:?}")))
    }

    /// This key's public key.
    #[must_use]
    pub fn public_key(&self) -> BlsPublicKey {
        BlsPublicKey(self.0.sk_to_pk().to_bytes())
    }

    /// Sign `msg` with the signing ciphersuite.
    #[must_use]
    pub fn sign(&self, msg: &[u8]) -> BlsSignature {
        BlsSignature(self.0.sign(msg, DST_SIG, &[]).to_bytes())
    }

    /// Sign `msg` with the key-binding ciphersuite (the scope-bound possession
    /// proof used by `TicketAction::Active.key_binding`).
    #[must_use]
    pub fn sign_binding(&self, msg: &[u8]) -> BlsSignature {
        BlsSignature(self.0.sign(msg, DST_BIND, &[]).to_bytes())
    }
}

fn parse_pk(pk: &BlsPublicKey) -> Result<PublicKey, CryptoError> {
    PublicKey::from_bytes(&pk.0).map_err(blst_err)
}

fn parse_sig(sig: &BlsSignature) -> Result<Signature, CryptoError> {
    Signature::from_bytes(&sig.0).map_err(blst_err)
}

impl BlsPublicKey {
    /// Verify a single `sig` over `msg` under this key.
    pub fn verify(&self, msg: &[u8], sig: &BlsSignature) -> Result<bool, CryptoError> {
        let pk = parse_pk(self)?;
        let sig = parse_sig(sig)?;
        Ok(sig.verify(true, msg, DST_SIG, &[], &pk, true) == BLST_ERROR::BLST_SUCCESS)
    }

    /// Verify a key-binding signature over `msg` under this key (the
    /// `key_binding` ciphersuite).
    pub fn verify_binding(&self, msg: &[u8], sig: &BlsSignature) -> Result<bool, CryptoError> {
        let pk = parse_pk(self)?;
        let sig = parse_sig(sig)?;
        Ok(sig.verify(true, msg, DST_BIND, &[], &pk, true) == BLST_ERROR::BLST_SUCCESS)
    }
}

impl BlsSignature {
    /// Aggregate individual signatures into one.
    pub fn aggregate(sigs: &[Self]) -> Result<Self, CryptoError> {
        let parsed: Vec<Signature> = sigs.iter().map(parse_sig).collect::<Result<_, _>>()?;
        let refs: Vec<&Signature> = parsed.iter().collect();
        let agg = AggregateSignature::aggregate(&refs, true).map_err(blst_err)?;
        Ok(Self(agg.to_signature().to_bytes()))
    }

    /// Same-message aggregate verification: this signature must be the aggregate of
    /// signatures by exactly `pubkeys`, each over `msg`. Assumes every key's PoP was
    /// verified.
    pub fn fast_aggregate_verify(
        &self,
        msg: &[u8],
        pubkeys: &[BlsPublicKey],
    ) -> Result<bool, CryptoError> {
        if pubkeys.is_empty() {
            return Ok(false);
        }
        let parsed: Vec<PublicKey> = pubkeys.iter().map(parse_pk).collect::<Result<_, _>>()?;
        let refs: Vec<&PublicKey> = parsed.iter().collect();
        let sig = parse_sig(self)?;
        Ok(sig.fast_aggregate_verify(true, msg, DST_SIG, &refs) == BLST_ERROR::BLST_SUCCESS)
    }
}

fn blst_err(e: BLST_ERROR) -> CryptoError {
    CryptoError::VerificationFailed(format!("blst: {e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> BlsSecretKey {
        BlsSecretKey::from_seed(&[seed; 32]).expect("seed is valid ikm")
    }

    #[test]
    fn sign_verify_round_trip() {
        let sk = key(1);
        let pk = sk.public_key();
        let sig = sk.sign(b"hello");
        assert!(pk.verify(b"hello", &sig).unwrap());
        assert!(!pk.verify(b"tampered", &sig).unwrap());
    }

    #[test]
    fn key_binding_round_trip_and_domain_separation() {
        let sk = key(2);
        let pk = sk.public_key();
        let binding = sk.sign_binding(b"bind me");
        assert!(pk.verify_binding(b"bind me", &binding).unwrap());
        // A signing-domain signature is not a valid binding.
        let not_binding = sk.sign(b"bind me");
        assert!(!pk.verify_binding(b"bind me", &not_binding).unwrap());
        // A binding-domain signature is not a valid signing signature.
        assert!(!pk.verify(b"bind me", &binding).unwrap());
    }

    #[test]
    fn aggregate_same_message_verifies() {
        let msg = b"committed step";
        let sks = [key(3), key(4), key(5)];
        let pks: Vec<BlsPublicKey> = sks.iter().map(BlsSecretKey::public_key).collect();
        let sigs: Vec<BlsSignature> = sks.iter().map(|s| s.sign(msg)).collect();
        let agg = BlsSignature::aggregate(&sigs).unwrap();
        assert!(agg.fast_aggregate_verify(msg, &pks).unwrap());

        // Dropping a signer breaks the aggregate against the full key set.
        let partial = BlsSignature::aggregate(&sigs[..2]).unwrap();
        assert!(!partial.fast_aggregate_verify(msg, &pks).unwrap());
        // The aggregate of the two does verify against the two keys (the bitmap case).
        assert!(partial.fast_aggregate_verify(msg, &pks[..2]).unwrap());
    }
}
