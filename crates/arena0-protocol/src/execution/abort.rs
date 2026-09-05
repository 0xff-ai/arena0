//! Signed, portable abort and failure occurrences.

use arena0_crypto::{Ed25519Signature, SignScheme};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::{PeerId, SessionHash};

use super::{
    MAX_TERMINAL_REASON_BYTES, OccurrenceDigest, ProtocolError, PublicCursor, ensure_payload,
};

/// Domain separator for the signed abort occurrence preimage.
pub const ABORT_OCCURRENCE_DOMAIN: [u8; 24] = *b"arena0/abort-occurrence\0";
const _: () = assert!(ABORT_OCCURRENCE_DOMAIN.len() == 24);
/// Version of the portable abort occurrence contract.
pub const ABORT_OCCURRENCE_VERSION: u16 = 1;

/// The terminal meaning authenticated by an [`AbortOccurrence`].
///
/// The explicit tags are part of the version-1 wire and proof contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

impl BorshSerialize for AbortKind {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        BorshSerialize::serialize(&self.tag(), writer)
    }
}

impl BorshDeserialize for AbortKind {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<Self> {
        Self::from_tag(u8::deserialize_reader(reader)?)
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
/// sender, terminal kind/code/reason, and exact public chain coordinate.  A
/// stream generation is intentionally absent: Phase 1 has no independent
/// portable stream-generation authority, so inventing one would weaken the
/// contract rather than bind it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AbortOccurrence {
    domain: [u8; 24],
    version: u16,
    session_id: SessionHash,
    sender: PeerId,
    kind: AbortKind,
    code: u32,
    reason: String,
    coordinate: PublicCursor,
    signature: Ed25519Signature,
}

impl BorshSerialize for AbortOccurrence {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        if self.reason.len() > MAX_TERMINAL_REASON_BYTES {
            return Err(borsh::io::Error::new(
                borsh::io::ErrorKind::InvalidInput,
                "abort reason exceeds bound",
            ));
        }
        BorshSerialize::serialize(&self.domain, writer)?;
        BorshSerialize::serialize(&self.version, writer)?;
        BorshSerialize::serialize(&self.session_id, writer)?;
        BorshSerialize::serialize(&self.sender, writer)?;
        BorshSerialize::serialize(&self.kind, writer)?;
        BorshSerialize::serialize(&self.code, writer)?;
        let length = u32::try_from(self.reason.len()).map_err(|_| {
            borsh::io::Error::new(borsh::io::ErrorKind::InvalidInput, "abort reason too long")
        })?;
        BorshSerialize::serialize(&length, writer)?;
        writer.write_all(self.reason.as_bytes())?;
        BorshSerialize::serialize(&self.coordinate, writer)?;
        BorshSerialize::serialize(&self.signature, writer)
    }
}

impl BorshDeserialize for AbortOccurrence {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let domain = <[u8; 24]>::deserialize_reader(reader)?;
        let version = u16::deserialize_reader(reader)?;
        let session_id = SessionHash::deserialize_reader(reader)?;
        let sender = PeerId::deserialize_reader(reader)?;
        let kind = AbortKind::deserialize_reader(reader)?;
        let code = u32::deserialize_reader(reader)?;
        let length = u32::deserialize_reader(reader)? as usize;
        if length > MAX_TERMINAL_REASON_BYTES {
            return Err(borsh::io::Error::new(
                borsh::io::ErrorKind::InvalidData,
                "abort reason exceeds bound",
            ));
        }
        let mut reason_bytes = vec![0; length];
        reader.read_exact(&mut reason_bytes)?;
        let reason = String::from_utf8(reason_bytes).map_err(|error| {
            borsh::io::Error::new(borsh::io::ErrorKind::InvalidData, error.to_string())
        })?;
        let coordinate = PublicCursor::deserialize_reader(reader)?;
        let signature = Ed25519Signature::deserialize_reader(reader)?;
        let occurrence = Self {
            domain,
            version,
            session_id,
            sender,
            kind,
            code,
            reason,
            coordinate,
            signature,
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
    coordinate: &'a PublicCursor,
}

impl AbortOccurrence {
    /// Construct one signed occurrence.  The reducer additionally checks that
    /// `sender` is an activation participant before accepting it.
    pub fn new(
        session_id: SessionHash,
        sender: PeerId,
        kind: AbortKind,
        code: u32,
        reason: impl Into<String>,
        coordinate: PublicCursor,
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
        coordinate: PublicCursor,
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

    /// Borrow the exact public chain coordinate.
    #[must_use]
    pub const fn coordinate(&self) -> &PublicCursor {
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

    /// Return a stable digest of the signed semantic content (excluding the
    /// signature bytes themselves).
    pub fn digest(&self) -> Result<OccurrenceDigest, ProtocolError> {
        Ok(OccurrenceDigest::of(&self.signing_bytes()?))
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
    /// checked by the execution reducer because it belongs to activation.
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
