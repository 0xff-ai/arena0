//! Signed, portable abort and failure occurrences.

use arena0_crypto::{Ed25519Signature, SignScheme};
use arena0_program::bounded;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::{PeerId, SessionHash};

use super::{MAX_TERMINAL_REASON_BYTES, ProtocolError, StepCursor, ensure_payload};

/// Domain separator for the signed abort occurrence preimage.
pub const ABORT_OCCURRENCE_DOMAIN: [u8; 24] = *b"arena0/abort-occurrence\0";
const _: () = assert!(ABORT_OCCURRENCE_DOMAIN.len() == 24);
/// Version of the portable abort occurrence contract.
pub const ABORT_OCCURRENCE_VERSION: u16 = 1;

/// The terminal meaning authenticated by an [`AbortOccurrence`].
///
/// The explicit tags are part of the version-1 wire and proof contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize)]
pub enum AbortKind {
    /// Explicitly stop the execution without classifying it as a failure.
    Abort,
    /// Classify the execution as failed.
    Fail,
}

impl AbortKind {
    /// Stable version-1 tag.
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Self::Abort => 0x00,
            Self::Fail => 0x01,
        }
    }

    /// Decode a stable version-1 tag.
    pub fn from_tag(tag: u8) -> Result<Self, borsh::io::Error> {
        match tag {
            0x00 => Ok(Self::Abort),
            0x01 => Ok(Self::Fail),
            tag => Err(borsh::io::Error::new(
                borsh::io::ErrorKind::InvalidData,
                format!("unknown abort kind tag {tag}"),
            )),
        }
    }
}

impl Serialize for AbortKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(self.tag())
    }
}

impl<'de> Deserialize<'de> for AbortKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::from_tag(<u8 as serde::Deserialize>::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

/// A portable, signed abort/failure occurrence.
///
/// The signature covers every field except itself, including the session,
/// sender, terminal kind/code/reason, and exact agreed chain coordinate.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, BorshSerialize)]
pub struct AbortOccurrence {
    domain: [u8; 24],
    version: u16,
    session_id: SessionHash,
    sender: PeerId,
    kind: AbortKind,
    code: u32,
    #[borsh(
        serialize_with = "bounded::write_string::<MAX_TERMINAL_REASON_BYTES>",
        deserialize_with = "bounded::read_string::<MAX_TERMINAL_REASON_BYTES>"
    )]
    reason: String,
    coordinate: StepCursor,
    signature: Ed25519Signature,
}

#[derive(BorshDeserialize)]
struct AbortOccurrenceRaw {
    domain: [u8; 24],
    version: u16,
    session_id: SessionHash,
    sender: PeerId,
    kind: AbortKind,
    code: u32,
    #[borsh(
        serialize_with = "bounded::write_string::<MAX_TERMINAL_REASON_BYTES>",
        deserialize_with = "bounded::read_string::<MAX_TERMINAL_REASON_BYTES>"
    )]
    reason: String,
    coordinate: StepCursor,
    signature: Ed25519Signature,
}

impl BorshDeserialize for AbortOccurrence {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let raw = AbortOccurrenceRaw::deserialize_reader(reader)?;
        let occurrence = Self {
            domain: raw.domain,
            version: raw.version,
            session_id: raw.session_id,
            sender: raw.sender,
            kind: raw.kind,
            code: raw.code,
            reason: raw.reason,
            coordinate: raw.coordinate,
            signature: raw.signature,
        };
        occurrence.validate_shape().map_err(|error| {
            borsh::io::Error::new(borsh::io::ErrorKind::InvalidData, error.to_string())
        })?;
        Ok(occurrence)
    }
}

#[derive(BorshSerialize)]
struct AbortSigningPayload<'a> {
    domain: [u8; 24],
    version: u16,
    session_id: SessionHash,
    sender: PeerId,
    kind: AbortKind,
    code: u32,
    reason: &'a str,
    coordinate: &'a StepCursor,
}

impl AbortOccurrence {
    /// Construct one signed occurrence. The actor additionally checks that
    /// `sender` is an activation participant before accepting it.
    pub fn new(
        session_id: SessionHash,
        sender: PeerId,
        kind: AbortKind,
        code: u32,
        reason: impl Into<String>,
        coordinate: StepCursor,
        signature: Ed25519Signature,
    ) -> Result<Self, ProtocolError> {
        let occurrence = Self {
            domain: ABORT_OCCURRENCE_DOMAIN,
            version: ABORT_OCCURRENCE_VERSION,
            session_id,
            sender,
            kind,
            code,
            reason: reason.into(),
            coordinate,
            signature,
        };
        occurrence.validate_shape()?;
        Ok(occurrence)
    }

    /// Construct an unsigned occurrence shell whose signing bytes can be
    /// passed to an Ed25519 identity key.  Call [`Self::with_signature`] with
    /// the resulting signature before placing it on an input.
    pub fn unsigned(
        session_id: SessionHash,
        sender: PeerId,
        kind: AbortKind,
        code: u32,
        reason: impl Into<String>,
        coordinate: StepCursor,
    ) -> Result<Self, ProtocolError> {
        Self::new(
            session_id,
            sender,
            kind,
            code,
            reason,
            coordinate,
            Ed25519Signature([0; 64]),
        )
    }

    /// Return a copy carrying the supplied Ed25519 signature.
    pub fn with_signature(mut self, signature: Ed25519Signature) -> Result<Self, ProtocolError> {
        self.signature = signature;
        self.validate_shape()?;
        Ok(self)
    }

    /// Return the session bound by this occurrence.
    #[must_use]
    pub const fn session_id(&self) -> SessionHash {
        self.session_id
    }

    /// Return the Ed25519 identity that authored this occurrence.
    #[must_use]
    pub const fn sender(&self) -> PeerId {
        self.sender
    }

    /// Return the authenticated terminal kind.
    #[must_use]
    pub const fn kind(&self) -> AbortKind {
        self.kind
    }

    /// Return the stable terminal code.
    #[must_use]
    pub const fn code(&self) -> u32 {
        self.code
    }

    /// Borrow the bounded reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Borrow the exact agreed chain coordinate.
    #[must_use]
    pub const fn coordinate(&self) -> &StepCursor {
        &self.coordinate
    }

    /// Return the signature bytes.
    #[must_use]
    pub const fn signature(&self) -> Ed25519Signature {
        self.signature
    }

    /// Return canonical bytes covered by the identity signature.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, ProtocolError> {
        borsh::to_vec(&AbortSigningPayload {
            domain: self.domain,
            version: self.version,
            session_id: self.session_id,
            sender: self.sender,
            kind: self.kind,
            code: self.code,
            reason: &self.reason,
            coordinate: &self.coordinate,
        })
        .map_err(|error| ProtocolError::Serialization(error.to_string()))
    }

    /// Verify the Ed25519 signature against the sender's public identity key.
    pub fn verify_signature(&self) -> Result<bool, ProtocolError> {
        let bytes = self.signing_bytes()?;
        arena0_crypto::verify(
            SignScheme::Ed25519,
            &self.sender.0,
            &bytes,
            &self.signature.0,
        )
        .map_err(|error| ProtocolError::InvalidAbortSignature(error.to_string()))
    }

    /// Validate shape and the session/coordinate binding.  Membership is
    /// checked by the execution actor because it belongs to activation.
    pub fn validate_for_session(&self, session: SessionHash) -> Result<(), ProtocolError> {
        self.validate_shape()?;
        if self.session_id != session {
            return Err(ProtocolError::InvalidAbortCoordinate);
        }
        if self.coordinate.next_step() == 0 && self.coordinate.chain_hash() != crate::CHAIN_START {
            return Err(ProtocolError::InvalidAbortCoordinate);
        }
        Ok(())
    }

    pub fn validate_shape(&self) -> Result<(), ProtocolError> {
        if self.domain != ABORT_OCCURRENCE_DOMAIN
            || self.version != ABORT_OCCURRENCE_VERSION
            || self.session_id == SessionHash([0; 32])
            || self.sender == PeerId([0; 32])
        {
            return Err(ProtocolError::InvalidAbortOccurrence);
        }
        if self.coordinate.next_step() == 0 && self.coordinate.chain_hash() != crate::CHAIN_START {
            return Err(ProtocolError::InvalidAbortCoordinate);
        }
        ensure_payload(
            "terminal reason",
            self.reason.len(),
            MAX_TERMINAL_REASON_BYTES,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_crypto::Ed25519Signature;

    fn occurrence() -> AbortOccurrence {
        AbortOccurrence::new(
            SessionHash([1; 32]),
            PeerId([2; 32]),
            AbortKind::Abort,
            0,
            "stop",
            StepCursor::new(0, crate::StateHash([3; 32]), crate::CHAIN_START),
            Ed25519Signature([0; 64]),
        )
        .expect("valid abort occurrence")
    }

    #[test]
    fn abort_occurrence_round_trips() {
        let value = occurrence();
        assert_eq!(
            borsh::from_slice::<AbortOccurrence>(&borsh::to_vec(&value).unwrap()).unwrap(),
            value
        );
    }
}
