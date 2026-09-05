//! Stable identities for externally redeliverable execution inputs.
//!
//! An occurrence has two deliberately separate identities. [`OccurrenceKey`]
//! names the semantic slot (for example, one participant's signature for one
//! step), while [`OccurrenceDigest`] commits to the complete canonical input
//! occupying that slot.  A store keeps these pairs for the lifetime of an
//! execution.  The execution aggregate only keeps the evidence it can still
//! prove from its current state.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::ExecId;

/// Domain separator for occurrence-key derivation.
pub const OCCURRENCE_KEY_DOMAIN: &[u8] = b"arena0/occurrence-key/v1";
/// Domain separator for canonical input digests.
pub const OCCURRENCE_DIGEST_DOMAIN: &[u8] = b"arena0/occurrence-digest/v1";

/// The externally redeliverable input class occupying an occurrence slot.
///
/// Tags are part of the persisted protocol contract.  Do not reorder them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OccurrenceKind {
    /// Activation of an execution's already committed session.
    Activate,
    /// A proposed public shared step.
    SharedProposal,
    /// One local private guest record, including timer and continuation
    /// resumes.
    Private,
    /// One participant's signature over a shared-step commitment.
    StepSignature,
    /// One participant's signature over a terminal commitment.
    TerminalSignature,
    /// The complete assembled receipt body.
    ReceiptBody,
    /// A signed local or peer abort/fail occurrence.
    Abort,
    /// A local interruption while terminal proof is being assembled.
    InterruptTerminal,
}

impl OccurrenceKind {
    /// Stable version-1 Borsh tag.
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Self::Activate => 0x00,
            Self::SharedProposal => 0x01,
            Self::Private => 0x02,
            Self::StepSignature => 0x03,
            Self::TerminalSignature => 0x04,
            Self::ReceiptBody => 0x05,
            Self::Abort => 0x07,
            Self::InterruptTerminal => 0x08,
        }
    }

    /// Decode a stable version-1 Borsh tag.
    pub fn from_tag(tag: u8) -> Result<Self, borsh::io::Error> {
        match tag {
            0x00 => Ok(Self::Activate),
            0x01 => Ok(Self::SharedProposal),
            0x02 => Ok(Self::Private),
            0x03 => Ok(Self::StepSignature),
            0x04 => Ok(Self::TerminalSignature),
            0x05 => Ok(Self::ReceiptBody),
            0x07 => Ok(Self::Abort),
            0x08 => Ok(Self::InterruptTerminal),
            tag => Err(borsh::io::Error::new(
                borsh::io::ErrorKind::InvalidData,
                format!("unknown occurrence kind tag {tag}"),
            )),
        }
    }
}

impl BorshSerialize for OccurrenceKind {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        BorshSerialize::serialize(&self.tag(), writer)
    }
}

impl BorshDeserialize for OccurrenceKind {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<Self> {
        Self::from_tag(u8::deserialize_reader(reader)?)
    }
}

impl Serialize for OccurrenceKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(self.tag())
    }
}

impl<'de> Deserialize<'de> for OccurrenceKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let tag = <u8 as serde::Deserialize>::deserialize(deserializer)?;
        Self::from_tag(tag).map_err(serde::de::Error::custom)
    }
}

/// Stable semantic address for one input slot in one execution.
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
#[serde(rename_all = "snake_case")]
pub struct OccurrenceKey {
    execution_id: ExecId,
    kind: OccurrenceKind,
    coordinate: [u8; 32],
}

impl OccurrenceKey {
    /// Derive a semantic key from an execution, input class, and bounded
    /// coordinate preimage.  The coordinate itself is hashed so callers never
    /// need to expose variable-size or private history in the key.
    #[must_use]
    pub fn derive(execution_id: ExecId, kind: OccurrenceKind, coordinate: &[u8]) -> Self {
        let mut preimage = Vec::with_capacity(
            OCCURRENCE_KEY_DOMAIN.len() + std::mem::size_of::<ExecId>() + 1 + coordinate.len(),
        );
        preimage.extend_from_slice(OCCURRENCE_KEY_DOMAIN);
        preimage.extend_from_slice(&execution_id.0);
        preimage.push(kind.tag());
        preimage.extend_from_slice(coordinate);
        Self {
            execution_id,
            kind,
            coordinate: *blake3::hash(&preimage).as_bytes(),
        }
    }

    /// Return the execution owning this occurrence slot.
    #[must_use]
    pub const fn execution_id(self) -> ExecId {
        self.execution_id
    }

    /// Return the input class occupying this slot.
    #[must_use]
    pub const fn kind(self) -> OccurrenceKind {
        self.kind
    }

    /// Return the hashed semantic coordinate.
    #[must_use]
    pub const fn coordinate(self) -> [u8; 32] {
        self.coordinate
    }
}

/// Digest of the complete canonical, validated input content.
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
pub struct OccurrenceDigest([u8; 32]);

impl OccurrenceDigest {
    /// Hash canonical Borsh input bytes under the occurrence digest domain.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(OCCURRENCE_DIGEST_DOMAIN);
        hasher.update(bytes);
        Self(*hasher.finalize().as_bytes())
    }

    /// Construct a digest from persisted bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// The bounded evidence a reducer places on a commit plan and a store places
/// in its lifetime occurrence table.
#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
pub struct OccurrenceEvidence {
    key: OccurrenceKey,
    digest: OccurrenceDigest,
}

impl OccurrenceEvidence {
    /// Pair a semantic key with its canonical content digest.
    #[must_use]
    pub const fn new(key: OccurrenceKey, digest: OccurrenceDigest) -> Self {
        Self { key, digest }
    }

    /// Return the semantic occurrence slot.
    #[must_use]
    pub const fn key(&self) -> OccurrenceKey {
        self.key
    }

    /// Return the canonical input digest.
    #[must_use]
    pub const fn digest(&self) -> OccurrenceDigest {
        self.digest
    }
}

/// Typed evidence produced when one semantic slot receives a different
/// canonical digest.  A conflict is diagnostic data; it is never a terminal
/// authorization.
#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
pub struct OccurrenceConflict {
    key: OccurrenceKey,
    existing: OccurrenceDigest,
    incoming: OccurrenceDigest,
}

impl OccurrenceConflict {
    /// Construct conflict evidence for one semantic slot.
    #[must_use]
    pub const fn new(
        key: OccurrenceKey,
        existing: OccurrenceDigest,
        incoming: OccurrenceDigest,
    ) -> Self {
        Self {
            key,
            existing,
            incoming,
        }
    }

    /// Return the conflicted semantic slot.
    #[must_use]
    pub const fn key(&self) -> OccurrenceKey {
        self.key
    }

    /// Return the previously committed digest.
    #[must_use]
    pub const fn existing(&self) -> OccurrenceDigest {
        self.existing
    }

    /// Return the incoming conflicting digest.
    #[must_use]
    pub const fn incoming(&self) -> OccurrenceDigest {
        self.incoming
    }
}
