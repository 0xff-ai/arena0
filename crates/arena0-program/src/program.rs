//! Program identity, metadata, and capability declarations.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use arena0_crypto::SignScheme;

use crate::schema::ProgramSchema;

/// Magic bytes at the start of every encoded program definition.
pub const PROGRAM_DEFINITION_MAGIC: [u8; 4] = *b"A0MD";
/// Version of the program-definition envelope and value-contract format.
pub const PROGRAM_DEFINITION_VERSION: u32 = 1;
/// Maximum Wasm program size accepted by a Host registry.
pub const PROGRAM_MAX_LEN: u64 = 64 * 1024 * 1024;

/// A 32-byte program content hash: the blake3 hash of the Wasm binary.
#[derive(
    BorshSerialize,
    BorshDeserialize,
    valuable::Valuable,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    /// Content address of a Wasm binary: blake3 of the bytes.
    #[must_use]
    pub fn of(wasm: &[u8]) -> Self {
        Self(*blake3::hash(wasm).as_bytes())
    }

    /// Borrow the raw hash bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl serde::Serialize for Hash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(self.0))
    }
}

impl<'de> serde::Deserialize<'de> for Hash {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        crate::id::parse_id_hex(&value)
            .map(Self)
            .map_err(|error| serde::de::Error::custom(error.to_string()))
    }
}

impl schemars::JsonSchema for Hash {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("ProgramHash")
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[0-9a-f]{64}$"
        })
    }
}

impl std::fmt::Display for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl std::str::FromStr for Hash {
    type Err = crate::id::IdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        crate::id::parse_id_hex(s).map(Self)
    }
}

impl Hash {
    /// First 4 bytes (8 hex chars) for human-readable logs.
    #[must_use]
    pub fn fmt_short(&self) -> impl std::fmt::Display + '_ {
        crate::id::HexShort(&self.0)
    }
}

/// Participant counts supported by a program.
#[derive(
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
    schemars::JsonSchema,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParticipantCount {
    /// The program requires one exact participant count.
    Exact { count: u8 },
    /// The program accepts every count in this inclusive range.
    Range { min: u8, max: u8 },
}

impl ParticipantCount {
    /// Return the inclusive supported bounds.
    #[must_use]
    pub const fn bounds(self) -> (u8, u8) {
        match self {
            Self::Exact { count } => (count, count),
            Self::Range { min, max } => (min, max),
        }
    }

    /// Return whether the program supports an admission target.
    #[must_use]
    pub fn accepts(self, participants: u16) -> bool {
        let Ok(participants) = u8::try_from(participants) else {
            return false;
        };
        let (min, max) = self.bounds();
        (min..=max).contains(&participants)
    }

    /// Reject reversed ranges.
    pub fn validate(self) -> Result<(), ParticipantCountError> {
        let (min, max) = self.bounds();
        if min > max {
            return Err(ParticipantCountError::Reversed { min, max });
        }
        Ok(())
    }
}

impl std::fmt::Display for ParticipantCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exact { count } => count.fmt(f),
            Self::Range { min, max } => write!(f, "{min}..={max}"),
        }
    }
}

/// Invalid [`ParticipantCount`] range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParticipantCountError {
    /// A range's lower bound exceeds its upper bound.
    #[error("participant count range is reversed: {min}..={max}")]
    Reversed { min: u8, max: u8 },
}

/// Metadata embedded in a Wasm program's custom section.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq)]
pub struct ProgramMetadata {
    /// Machine-readable program name.
    pub name: String,
    /// Semver version string.
    pub version: String,
    /// One-line human-readable description.
    pub description: String,
    /// Optional author or organization.
    pub author: Option<String>,
    /// Host capabilities this program requires.
    pub capabilities: Vec<Capability>,
    /// Human-facing display name.
    pub display_name: String,
    /// Supported participant counts for the session ensemble.
    pub participants: ParticipantCount,
}

/// A program bundled with its schema, ready for registration.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq)]
pub struct ProgramDefinition {
    /// Program metadata from the `arena0.metadata` custom section.
    pub metadata: ProgramMetadata,
    /// Introspection schema for program values.
    pub schema: ProgramSchema,
}

impl ProgramDefinition {
    /// Encode this definition with the `A0MD|v1|borsh` envelope.
    pub fn encode(&self) -> Result<Vec<u8>, ProgramDefinitionError> {
        let body = borsh::to_vec(self).map_err(ProgramDefinitionError::Encode)?;
        let mut bytes = Vec::with_capacity(8 + body.len());
        bytes.extend_from_slice(&PROGRAM_DEFINITION_MAGIC);
        bytes.extend_from_slice(&PROGRAM_DEFINITION_VERSION.to_le_bytes());
        bytes.extend_from_slice(&body);
        Ok(bytes)
    }

    /// Decode one versioned `A0MD|v1|borsh` envelope.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProgramDefinitionError> {
        let Some((header, body)) = bytes.split_at_checked(8) else {
            return Err(ProgramDefinitionError::MissingHeader);
        };
        if header[..4] != PROGRAM_DEFINITION_MAGIC {
            return Err(ProgramDefinitionError::WrongMagic);
        }
        let version = u32::from_le_bytes(header[4..8].try_into().expect("fixed header width"));
        if version != PROGRAM_DEFINITION_VERSION {
            return Err(ProgramDefinitionError::UnsupportedVersion(version));
        }
        Self::try_from_slice(body).map_err(ProgramDefinitionError::Decode)
    }
}

/// Failure to encode or decode the versioned program-definition envelope.
#[derive(Debug, thiserror::Error)]
pub enum ProgramDefinitionError {
    /// The byte string cannot contain the fixed envelope header.
    #[error("program definition is missing its versioned header")]
    MissingHeader,
    /// The envelope does not identify Arena0 program metadata.
    #[error("program definition has the wrong magic bytes")]
    WrongMagic,
    /// The host does not support the encoded contract version.
    #[error("unsupported program definition version {0}")]
    UnsupportedVersion(u32),
    /// Borsh could not encode the definition body.
    #[error("program definition encode failed: {0}")]
    Encode(std::io::Error),
    /// Borsh could not decode the definition body.
    #[error("program definition decode failed: {0}")]
    Decode(std::io::Error),
}

/// A host capability that a program declares it needs.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub enum Capability {
    /// Send and receive binary messages to/from peers.
    Messaging,
    /// Request input from the controlling agent.
    Input,
    /// Set one-shot timers.
    Timers,
    /// Sign data with the given schemes.
    Sign { schemes: Vec<SignScheme> },
}

/// An owned, deduplicated set of declared and inferred capabilities.
#[derive(Debug, Default)]
pub struct CapabilitySet(Vec<Capability>);

impl CapabilitySet {
    /// An empty capability set.
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Add one capability, merging duplicates where needed.
    pub fn insert(&mut self, capability: Capability) {
        match capability {
            Capability::Sign { schemes } => {
                if let Some(Capability::Sign { schemes: existing }) = self
                    .0
                    .iter_mut()
                    .find(|capability| matches!(capability, Capability::Sign { .. }))
                {
                    existing.extend(schemes);
                    existing.sort_by_key(|scheme| scheme.canonical_order());
                    existing.dedup();
                } else {
                    let mut schemes = schemes;
                    schemes.sort_by_key(|scheme| scheme.canonical_order());
                    schemes.dedup();
                    self.0.push(Capability::Sign { schemes });
                }
            }
            capability => {
                if !self.0.contains(&capability) {
                    self.0.push(capability);
                }
            }
        }
    }

    /// Merge capabilities from another declaration site.
    pub fn extend(&mut self, capabilities: impl IntoIterator<Item = Capability>) {
        for capability in capabilities {
            self.insert(capability);
        }
    }

    /// The contained capabilities, in declaration order.
    #[must_use]
    pub fn into_vec(self) -> Vec<Capability> {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use crate::{JsonSchemaDocument, StateSchema};

    use super::*;

    #[test]
    fn definition_envelope_round_trips() {
        let unit = JsonSchemaDocument::unit();
        let def = ProgramDefinition {
            metadata: ProgramMetadata {
                name: "t".into(),
                version: "0.1.0".into(),
                description: "x".into(),
                author: None,
                capabilities: Vec::new(),
                display_name: "T".into(),
                participants: ParticipantCount::Exact { count: 2 },
            },
            schema: ProgramSchema {
                state: StateSchema {
                    schema: unit.clone(),
                    max_bytes: 0,
                },
                callouts: Vec::new(),
                messages: Vec::new(),
                params: unit.clone(),
                queries: Vec::new(),
                outcome: unit,
            },
        };
        let bytes = def.encode().unwrap();
        assert_eq!(&bytes[..4], PROGRAM_DEFINITION_MAGIC);
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            PROGRAM_DEFINITION_VERSION
        );
        assert_eq!(ProgramDefinition::decode(&bytes).unwrap(), def);
    }

    #[test]
    fn participant_count_accepts_exact_and_range() {
        let exact = ParticipantCount::Exact { count: 2 };
        assert!(exact.accepts(2));
        assert!(!exact.accepts(3));

        let range = ParticipantCount::Range { min: 3, max: 32 };
        assert!(!range.accepts(2));
        assert!(range.accepts(3));
        assert!(range.accepts(32));
        assert!(!range.accepts(33));
        assert!(!range.accepts(256));
    }

    #[test]
    fn participant_count_rejects_reversed_range() {
        assert!(ParticipantCount::Exact { count: 1 }.validate().is_ok());
        assert!(matches!(
            ParticipantCount::Range { min: 4, max: 3 }.validate(),
            Err(ParticipantCountError::Reversed { min: 4, max: 3 })
        ));
    }

    #[test]
    fn merges_capability_declarations() {
        let mut capabilities = CapabilitySet::new();
        capabilities.extend([Capability::Messaging, Capability::Messaging]);
        assert_eq!(capabilities.into_vec(), vec![Capability::Messaging]);
    }

    #[test]
    fn merges_sign_capabilities_in_canonical_order() {
        let mut capabilities = CapabilitySet::new();
        capabilities.extend([
            Capability::Sign {
                schemes: vec![SignScheme::Bls],
            },
            Capability::Sign {
                schemes: vec![SignScheme::Ed25519, SignScheme::Bls],
            },
        ]);
        assert_eq!(
            capabilities.into_vec(),
            vec![Capability::Sign {
                schemes: vec![SignScheme::Ed25519, SignScheme::Bls],
            }]
        );
    }
}
