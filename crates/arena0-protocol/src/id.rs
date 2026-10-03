//! Internal helpers for the 32-byte newtype id pattern.
//!
//! Each domain module defines its id as `Id` (or `Hash`) via [`id_type!`], and
//! the crate root re-exports a flat alias (`PeerId`, `SessionHash`, `StateHash`,
//! ...). Ids are 32-byte values shown as hex; they implement `Display`,
//! `FromStr`, `Ord`, and `Hash` so they work as map keys, log labels, and
//! CLI/API parameters.

pub use arena0_crypto::id::IdParseError;
pub(crate) use arena0_crypto::id::{HexShort, parse_id_hex};

/// Define a 32-byte newtype with the standard derives, hex `Display`/`FromStr`,
/// and `fmt_short()`. Pass a trailing `, Default` to also derive `Default` (for
/// the zeroable hash/nonce/key types).
macro_rules! id_type {
    ($(#[$m:meta])* $vis:vis struct $name:ident $(, $default:ident)?) => {
        $(#[$m])*
        #[derive(
            ::borsh::BorshSerialize,
            ::borsh::BorshDeserialize,
            ::borsh::BorshSchema,
            ::valuable::Valuable,
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash
            $(, $default)?
        )]
        $vis struct $name(pub [u8; 32]);

        // JSON carries hex strings. Treating ids as a TS primitive also avoids
        // collisions between domain-local names such as `Id` and `Hash`.
        #[cfg(feature = "ts")]
        impl ::ts_rs::TS for $name {
            type WithoutGenerics = Self;
            type OptionInnerType = Self;

            fn name(_: &::ts_rs::Config) -> String {
                "string".into()
            }

            fn inline(_: &::ts_rs::Config) -> String {
                "string".into()
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> ::core::result::Result<S::Ok, S::Error>
            where
                S: ::serde::Serializer,
            {
                ::serde::Serializer::serialize_str(serializer, &::hex::encode(self.0))
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> ::core::result::Result<Self, D::Error>
            where
                D: ::serde::Deserializer<'de>,
            {
                let value = <String as ::serde::Deserialize>::deserialize(deserializer)?;
                $crate::id::parse_id_hex(&value)
                    .map(Self)
                    .map_err(|error| <D::Error as ::serde::de::Error>::custom(error.to_string()))
            }
        }

        impl ::schemars::JsonSchema for $name {
            fn inline_schema() -> bool {
                true
            }

            fn schema_name() -> ::std::borrow::Cow<'static, str> {
                ::std::borrow::Cow::Borrowed(stringify!($name))
            }

            fn json_schema(
                _generator: &mut ::schemars::SchemaGenerator,
            ) -> ::schemars::Schema {
                ::schemars::json_schema!({
                    "type": "string",
                    "pattern": "^[0-9a-f]{64}$"
                })
            }
        }

        impl $name {
            /// First 4 bytes (8 hex chars) for human-readable logs.
            #[must_use]
            pub fn fmt_short(&self) -> impl ::core::fmt::Display + '_ {
                $crate::id::HexShort(&self.0)
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                write!(f, "{}", ::hex::encode(self.0))
            }
        }

        impl ::core::str::FromStr for $name {
            type Err = $crate::id::IdParseError;
            fn from_str(s: &str) -> ::core::result::Result<Self, Self::Err> {
                $crate::id::parse_id_hex(s).map(Self)
            }
        }
    };
}

pub(crate) use id_type;

#[cfg(test)]
mod tests {
    use super::parse_id_hex;
    use crate::PeerId;

    #[test]
    fn ids_use_lowercase_hex_json() {
        let id = PeerId([0xab; 32]);
        let json = serde_json::to_string(&id).expect("id serializes");
        assert_eq!(json, format!("\"{}\"", "ab".repeat(32)));
        let decoded: PeerId = serde_json::from_str(&json).expect("id deserializes");
        assert_eq!(decoded, id);
        assert!(parse_id_hex(&"AB".repeat(32)).is_err());
    }
}
