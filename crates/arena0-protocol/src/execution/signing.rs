//! Kernel-owned signing contracts for guest `Sign` effects.

use arena0_crypto::SignScheme;
use arena0_program::ProgramHash;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::io;

use crate::bounded::read_bytes;
use crate::{ExecId, SessionHash};

use super::{MAX_EFFECT_PAYLOAD_BYTES, ProtocolError, ensure_payload};

/// Versioned, execution-bound preimage presented to a local signer for a guest
/// `Effect::Sign`. The guest payload is data inside this contract, never the
/// protocol message itself, so it cannot be used as a signing oracle for step,
/// terminal, activation, or receipt commitments.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GuestSignData {
    domain: [u8; 24],
    version: u16,
    session_id: SessionHash,
    program_hash: ProgramHash,
    execution_id: ExecId,
    private_sequence: u64,
    effect_index: u32,
    scheme: SignScheme,
    payload: Vec<u8>,
}

impl BorshSerialize for GuestSignData {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        BorshSerialize::serialize(&self.domain, writer)?;
        BorshSerialize::serialize(&self.version, writer)?;
        BorshSerialize::serialize(&self.session_id, writer)?;
        BorshSerialize::serialize(&self.program_hash, writer)?;
        BorshSerialize::serialize(&self.execution_id, writer)?;
        BorshSerialize::serialize(&self.private_sequence, writer)?;
        BorshSerialize::serialize(&self.effect_index, writer)?;
        BorshSerialize::serialize(&self.scheme, writer)?;
        let length = u32::try_from(self.payload.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "signing payload too long"))?;
        BorshSerialize::serialize(&length, writer)?;
        writer.write_all(&self.payload)
    }
}

impl BorshDeserialize for GuestSignData {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        let data = Self {
            domain: <[u8; 24]>::deserialize_reader(reader)?,
            version: u16::deserialize_reader(reader)?,
            session_id: crate::SessionHash::deserialize_reader(reader)?,
            program_hash: ProgramHash::deserialize_reader(reader)?,
            execution_id: crate::ExecId::deserialize_reader(reader)?,
            private_sequence: u64::deserialize_reader(reader)?,
            effect_index: u32::deserialize_reader(reader)?,
            scheme: SignScheme::deserialize_reader(reader)?,
            payload: read_bytes(reader, MAX_EFFECT_PAYLOAD_BYTES, "signing payload")?,
        };
        data.validate()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        Ok(data)
    }
}

impl GuestSignData {
    /// Domain separation tag for guest-owned signing requests.
    pub const DOMAIN: [u8; 24] = *b"arena0/guest-sign/v1\0\0\0\0";
    /// Version of the guest signing request contract.
    pub const VERSION: u16 = 1;

    pub(crate) fn new(
        session_id: SessionHash,
        program_hash: ProgramHash,
        execution_id: ExecId,
        private_sequence: u64,
        effect_index: u32,
        scheme: SignScheme,
        payload: Vec<u8>,
    ) -> Result<Self, ProtocolError> {
        let data = Self {
            domain: Self::DOMAIN,
            version: Self::VERSION,
            session_id,
            program_hash,
            execution_id,
            private_sequence,
            effect_index,
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

    /// Return the private coordinate bound into the signing request.
    #[must_use]
    pub const fn private_sequence(&self) -> u64 {
        self.private_sequence
    }

    /// Return the guest effect ordinal bound into the signing request.
    #[must_use]
    pub const fn effect_index(&self) -> u32 {
        self.effect_index
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
