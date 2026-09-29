//! Kernel-owned signing contracts for synchronous guest signing requests.

use arena0_crypto::SignScheme;
use arena0_program::ProgramHash;
use arena0_program::bounded;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::io;

use crate::{ExecId, ExecutionBinding, PeerId, SessionHash, VerifyError};

use super::{ProtocolError, ensure_payload};
use crate::MAX_EFFECT_PAYLOAD_BYTES;

/// Versioned, execution-bound preimage presented to a local signer for one
/// guest signing call. The guest payload is data inside this contract, never
/// the protocol message itself, so it cannot be used as a signing oracle for
/// step, activation, or receipt commitments.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, BorshSerialize)]
pub struct GuestSignData {
    domain: [u8; 24],
    version: u16,
    session_id: SessionHash,
    program_hash: ProgramHash,
    execution_id: ExecId,
    event_position: u64,
    call_index: u32,
    scheme: SignScheme,
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_EFFECT_PAYLOAD_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_EFFECT_PAYLOAD_BYTES>"
    )]
    payload: Vec<u8>,
}

#[derive(BorshDeserialize)]
struct GuestSignDataRaw {
    domain: [u8; 24],
    version: u16,
    session_id: SessionHash,
    program_hash: ProgramHash,
    execution_id: ExecId,
    event_position: u64,
    call_index: u32,
    scheme: SignScheme,
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_EFFECT_PAYLOAD_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_EFFECT_PAYLOAD_BYTES>"
    )]
    payload: Vec<u8>,
}

impl BorshDeserialize for GuestSignData {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        let raw = GuestSignDataRaw::deserialize_reader(reader)?;
        let data = Self {
            domain: raw.domain,
            version: raw.version,
            session_id: raw.session_id,
            program_hash: raw.program_hash,
            execution_id: raw.execution_id,
            event_position: raw.event_position,
            call_index: raw.call_index,
            scheme: raw.scheme,
            payload: raw.payload,
        };
        data.validate()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        Ok(data)
    }
}

impl GuestSignData {
    /// Check a guest `sign` result from `signer` against this session.
    ///
    /// Decode check: `signed` must decode (validating decoder, whole input) as a
    /// `GuestSignData` with DOMAIN and VERSION. Binding check: its session and
    /// program equal `binding`'s. `signer` must be a participant. The signature
    /// must verify over `signed` under `signer`'s key for the preimage's scheme:
    /// Ed25519 uses the PeerId bytes, BLS uses `binding.participant_key(signer)`.
    /// The preimage's execution id, event position and call index are the
    /// signer's provenance and are not checked. Returns the payload.
    pub fn verify(
        signed: &[u8],
        signature: &[u8],
        signer: &PeerId,
        binding: &ExecutionBinding,
    ) -> Result<Vec<u8>, VerifyError> {
        let data: Self = borsh::from_slice(signed).map_err(|_| VerifyError::Malformed)?;
        if data.session_id != binding.session_id() || data.program_hash != binding.program_hash() {
            return Err(VerifyError::OtherSession);
        }
        if !binding.is_participant(*signer) {
            return Err(VerifyError::NotParticipant);
        }
        let bls_key;
        let key: &[u8] = match data.scheme {
            SignScheme::Ed25519 => &signer.0,
            SignScheme::Bls => {
                bls_key = binding
                    .participant_key(signer)
                    .map_err(|_| VerifyError::NotParticipant)?;
                &bls_key.0
            }
        };
        match arena0_crypto::verify(data.scheme, key, signed, signature) {
            Ok(true) => Ok(data.payload),
            Ok(false) | Err(_) => Err(VerifyError::BadSignature),
        }
    }

    /// Domain separation tag for guest-owned signing requests.
    pub const DOMAIN: [u8; 24] = *b"arena0/guest-sign/v3\0\0\0\0";
    /// Version of the guest signing request contract.
    pub const VERSION: u16 = 3;

    pub fn new(
        session_id: SessionHash,
        program_hash: ProgramHash,
        execution_id: ExecId,
        event_position: u64,
        call_index: u32,
        scheme: SignScheme,
        payload: Vec<u8>,
    ) -> Result<Self, ProtocolError> {
        let data = Self {
            domain: Self::DOMAIN,
            version: Self::VERSION,
            session_id,
            program_hash,
            execution_id,
            event_position,
            call_index,
            scheme,
            payload,
        };
        data.validate()?;
        Ok(data)
    }

    /// Return the session bound into the signing request.
    #[must_use]
    pub const fn session_id(&self) -> SessionHash {
        self.session_id
    }

    /// Return the program bound into the signing request.
    #[must_use]
    pub const fn program_hash(&self) -> ProgramHash {
        self.program_hash
    }

    /// Return the execution bound into the signing request.
    #[must_use]
    pub const fn execution_id(&self) -> ExecId {
        self.execution_id
    }

    /// Return the event position bound into the signing request.
    #[must_use]
    pub const fn event_position(&self) -> u64 {
        self.event_position
    }

    /// Return the sign call ordinal within its dispatch.
    #[must_use]
    pub const fn call_index(&self) -> u32 {
        self.call_index
    }

    /// Return the requested signing scheme.
    #[must_use]
    pub const fn scheme(&self) -> SignScheme {
        self.scheme
    }

    /// Borrow the guest-selected payload inside the kernel-owned preimage.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Return the exact canonical bytes the signer must sign.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, ProtocolError> {
        borsh::to_vec(self).map_err(|error| ProtocolError::Serialization(error.to_string()))
    }

    pub(crate) fn validate(&self) -> Result<(), ProtocolError> {
        if self.domain != Self::DOMAIN || self.version != Self::VERSION {
            return Err(ProtocolError::InvalidGuestSignData);
        }
        if self.session_id == SessionHash([0; 32])
            || self.program_hash == ProgramHash([0; 32])
            || self.execution_id == ExecId([0; 32])
        {
            return Err(ProtocolError::InvalidGuestSignData);
        }
        ensure_payload(
            "guest signing payload",
            self.payload.len(),
            MAX_EFFECT_PAYLOAD_BYTES,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_fixtures::fixture;
    use super::*;

    #[test]
    fn verify_returns_the_payload_for_ed25519_and_bls() {
        let fixture = fixture();
        let binding = fixture.binding();
        for (peer, bls) in &fixture.participants {
            for scheme in [SignScheme::Ed25519, SignScheme::Bls] {
                let signed = GuestSignData::new(
                    binding.session_id(),
                    binding.program_hash(),
                    ExecId([7; 32]),
                    4,
                    2,
                    scheme,
                    b"receipt".to_vec(),
                )
                .unwrap()
                .signing_bytes()
                .unwrap();
                let signature = match scheme {
                    SignScheme::Ed25519 => fixture.identity(*peer).sign(&signed).0.to_vec(),
                    SignScheme::Bls => bls.sign(&signed).0.to_vec(),
                };
                assert_eq!(
                    GuestSignData::verify(&signed, &signature, peer, &binding),
                    Ok(b"receipt".to_vec())
                );
            }
        }
    }

    #[test]
    fn verify_rejects_each_error_case() {
        let fixture = fixture();
        let binding = fixture.binding();
        let peer = fixture.producer();
        for scheme in [SignScheme::Ed25519, SignScheme::Bls] {
            let data = GuestSignData::new(
                binding.session_id(),
                binding.program_hash(),
                ExecId([7; 32]),
                4,
                2,
                scheme,
                b"receipt".to_vec(),
            )
            .unwrap();
            let signed = data.signing_bytes().unwrap();
            let signature = match scheme {
                SignScheme::Ed25519 => fixture.identity(peer).sign(&signed).0.to_vec(),
                SignScheme::Bls => fixture.participants[0].1.sign(&signed).0.to_vec(),
            };
            assert_eq!(
                GuestSignData::verify(b"garbage", &signature, &peer, &binding),
                Err(VerifyError::Malformed)
            );
            let mut wrong_domain = signed.clone();
            wrong_domain[0] ^= 1;
            assert_eq!(
                GuestSignData::verify(&wrong_domain, &signature, &peer, &binding),
                Err(VerifyError::Malformed)
            );
            let mut wrong_version = signed.clone();
            wrong_version[24] ^= 1;
            assert_eq!(
                GuestSignData::verify(&wrong_version, &signature, &peer, &binding),
                Err(VerifyError::Malformed)
            );
            let mut trailing = signed.clone();
            trailing.push(0);
            assert_eq!(
                GuestSignData::verify(&trailing, &signature, &peer, &binding),
                Err(VerifyError::Malformed)
            );
            let mut other = data.clone();
            other.session_id = SessionHash([8; 32]);
            assert_eq!(
                GuestSignData::verify(&other.signing_bytes().unwrap(), &signature, &peer, &binding),
                Err(VerifyError::OtherSession)
            );
            other = data;
            other.program_hash = ProgramHash([8; 32]);
            assert_eq!(
                GuestSignData::verify(&other.signing_bytes().unwrap(), &signature, &peer, &binding),
                Err(VerifyError::OtherSession)
            );
            assert_eq!(
                GuestSignData::verify(&signed, &signature, &PeerId([9; 32]), &binding),
                Err(VerifyError::NotParticipant)
            );
            let mut flipped = signature.clone();
            flipped[0] ^= 1;
            assert_eq!(
                GuestSignData::verify(&signed, &flipped, &peer, &binding),
                Err(VerifyError::BadSignature)
            );
            assert_eq!(
                GuestSignData::verify(&signed, &[], &peer, &binding),
                Err(VerifyError::BadSignature)
            );
            let other_signature = match scheme {
                SignScheme::Ed25519 => fixture
                    .identity(fixture.participants[1].0)
                    .sign(&signed)
                    .0
                    .to_vec(),
                SignScheme::Bls => fixture.participants[1].1.sign(&signed).0.to_vec(),
            };
            assert_eq!(
                GuestSignData::verify(&signed, &other_signature, &peer, &binding),
                Err(VerifyError::BadSignature)
            );
        }
    }

    #[test]
    fn verify_ignores_the_signers_execution_and_coordinates() {
        let fixture = fixture();
        let binding = fixture.binding();
        let peer = fixture.producer();
        for (execution, position, call) in [
            (ExecId([7; 32]), 0, 0),
            (ExecId([8; 32]), u64::MAX, u32::MAX),
        ] {
            let signed = GuestSignData::new(
                binding.session_id(),
                binding.program_hash(),
                execution,
                position,
                call,
                SignScheme::Ed25519,
                b"receipt".to_vec(),
            )
            .unwrap()
            .signing_bytes()
            .unwrap();
            let signature = fixture.identity(peer).sign(&signed);
            assert_eq!(
                GuestSignData::verify(&signed, &signature.0, &peer, &binding),
                Ok(b"receipt".to_vec())
            );
        }
    }
}
