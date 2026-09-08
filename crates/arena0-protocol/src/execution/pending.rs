//! Typed identities for durable execution continuations.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The stable identity of one pending execution continuation.
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
pub struct PendingId(u64);

impl PendingId {
    /// Construct a pending identity from its protocol representation.
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

impl From<u64> for PendingId {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

impl From<PendingId> for u64 {
    fn from(value: PendingId) -> Self {
        value.get()
    }
}

impl fmt::Display for PendingId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Failure to parse a decimal pending identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingIdParseError {
    /// The input was empty or contained a non-decimal character.
    InvalidDecimal,
    /// The input did not fit in the protocol's 64-bit representation.
    Overflow,
}

impl fmt::Display for PendingIdParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDecimal => formatter.write_str("pending id must be a decimal integer"),
            Self::Overflow => formatter.write_str("pending id does not fit in u64"),
        }
    }
}

impl std::error::Error for PendingIdParseError {}

impl FromStr for PendingId {
    type Err = PendingIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(PendingIdParseError::InvalidDecimal);
        }
        value
            .parse::<u64>()
            .map(Self::new)
            .map_err(|_| PendingIdParseError::Overflow)
    }
}

impl Serialize for PendingId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PendingId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = <String as Deserialize>::deserialize(deserializer)?;
        value.parse().map_err(D::Error::custom)
    }
}

impl schemars::JsonSchema for PendingId {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("PendingId")
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
    fn pending_ids_use_decimal_json_strings_and_u64_borsh_across_boundaries() {
        for value in [0, 1, (1 << 53) - 1, 1 << 53, (1 << 53) + 1, u64::MAX] {
            let id = PendingId::new(value);
            let decimal = value.to_string();
            let json = serde_json::to_value(id).unwrap();
            assert_eq!(json, serde_json::Value::String(decimal.clone()));
            assert_eq!(serde_json::from_value::<PendingId>(json).unwrap(), id);
            assert!(serde_json::from_str::<PendingId>(&decimal).is_err());
            assert_eq!(borsh::to_vec(&id).unwrap(), value.to_le_bytes());
            assert_eq!(PendingId::try_from_slice(&value.to_le_bytes()).unwrap(), id);
        }
        for invalid in ["", "-1", "+1", "1.0", " 1", "18446744073709551616"] {
            assert!(serde_json::from_value::<PendingId>(serde_json::json!(invalid)).is_err());
        }
    }
}
