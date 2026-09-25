//! The durable open callout and its identity.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use arena0_program::MAX_CALLOUT_CONTEXT_BYTES;
use arena0_program::bounded;

/// The stable identity of one open callout.
///
/// The protocol stores this value as an unsigned 64-bit integer so its
/// deterministic Borsh representation remains compact. JSON surfaces encode
/// it as a decimal string because JSON number consumers commonly use a
/// lossless range smaller than `u64` (for example, JavaScript's `Number`).
#[derive(
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
    valuable::Valuable,
)]
pub struct CalloutId(u64);

impl CalloutId {
    /// Construct a callout identity from its protocol representation.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the compact protocol representation.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The single open callout derived from program state.
///
/// After every accepted dispatch the program's read-only `callout` function
/// computes at most one open callout from the resulting state image. The host
/// stores it with that image, so the callout is never emitted as an effect and
/// never acts as a lock.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct OpenCallout {
    /// Stable identity of this open callout.
    pub id: CalloutId,
    /// Program-local callout variant index.
    pub callout_index: u32,
    /// Agent-facing JSON context for that callout.
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_CALLOUT_CONTEXT_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_CALLOUT_CONTEXT_BYTES>"
    )]
    pub context: Vec<u8>,
}

/// Derive a callout identity from the execution event position that
/// produced it. Event position is the only execution coordinate; there is no
/// private record sequence.
#[must_use]
pub fn callout_id(execution_id: crate::ExecId, event_position: u64) -> CalloutId {
    // The domain tag predates the callout naming; it is part of the ID bytes.
    let bytes = borsh::to_vec(&(b"arena0/pending/v3", execution_id, event_position))
        .expect("callout id preimage is serializable");
    let digest = blake3::hash(&bytes);
    CalloutId::new(u64::from_le_bytes(
        digest.as_bytes()[..8]
            .try_into()
            .expect("digest prefix has eight bytes"),
    ))
}

impl From<u64> for CalloutId {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

impl From<CalloutId> for u64 {
    fn from(value: CalloutId) -> Self {
        value.get()
    }
}

impl fmt::Display for CalloutId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Failure to parse a decimal callout identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalloutIdParseError {
    /// The input was empty or contained a non-decimal character.
    InvalidDecimal,
    /// The input did not fit in the protocol's 64-bit representation.
    Overflow,
}

impl fmt::Display for CalloutIdParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDecimal => formatter.write_str("callout id must be a decimal integer"),
            Self::Overflow => formatter.write_str("callout id does not fit in u64"),
        }
    }
}

impl std::error::Error for CalloutIdParseError {}

impl FromStr for CalloutId {
    type Err = CalloutIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(CalloutIdParseError::InvalidDecimal);
        }
        value
            .parse::<u64>()
            .map(Self::new)
            .map_err(|_| CalloutIdParseError::Overflow)
    }
}

impl Serialize for CalloutId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for CalloutId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = <String as Deserialize>::deserialize(deserializer)?;
        value.parse().map_err(D::Error::custom)
    }
}

impl schemars::JsonSchema for CalloutId {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CalloutId")
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[0-9]+$"
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callout_ids_use_decimal_json_strings_and_u64_borsh_across_boundaries() {
        for value in [0, 1, (1 << 53) - 1, 1 << 53, (1 << 53) + 1, u64::MAX] {
            let id = CalloutId::new(value);
            let decimal = value.to_string();
            let json = serde_json::to_value(id).unwrap();
            assert_eq!(json, serde_json::Value::String(decimal.clone()));
            assert_eq!(serde_json::from_value::<CalloutId>(json).unwrap(), id);
            assert!(serde_json::from_str::<CalloutId>(&decimal).is_err());
            assert_eq!(borsh::to_vec(&id).unwrap(), value.to_le_bytes());
            assert_eq!(CalloutId::try_from_slice(&value.to_le_bytes()).unwrap(), id);
        }
        for invalid in ["", "-1", "+1", "1.0", " 1", "18446744073709551616"] {
            assert!(serde_json::from_value::<CalloutId>(serde_json::json!(invalid)).is_err());
        }
    }
}
