//! Kernel-owned signing contracts for synchronous guest signing requests.

use arena0_crypto::SignScheme;
use arena0_program::ProgramHash;
use arena0_program::bounded;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::io;

use crate::{ExecId, SessionHash};

use super::{MAX_EFFECT_PAYLOAD_BYTES, ProtocolError, ensure_payload};

/// Versioned, execution-bound preimage presented to a local signer for one
/// guest signing call. The guest payload is data inside this contract, never
/// the protocol message itself, so it cannot be used as a signing oracle for
/// step, terminal, activation, or receipt commitments.
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
