//! Bounded semantic state values.
//!
//! State bytes are opaque to the host, but their role is not interchangeable:
//! shared bytes are consensus-visible and hashed, while local bytes belong to
//! one participant and are never included in shared commitments. These
//! newtypes make that distinction explicit at every caller.
//!
//! The sandbox stores each value in a fixed state memory as
//! `u32::to_le_bytes(payload_len) || payload || zero_tail`; this crate keeps
//! only the payload bytes so persistence and transport never retain padded
//! memory images.

use std::fmt;
use std::io;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::de::{self, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::bounded;

/// Maximum accepted serialized shared-state length.
pub const MAX_SHARED_STATE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum accepted serialized participant-local-state length.
pub const MAX_LOCAL_STATE_BYTES: usize = 4 * 1024 * 1024;

/// Number of bytes in the canonical state-memory length prefix.
pub const CANONICAL_STATE_PREFIX_BYTES: usize = std::mem::size_of::<u32>();
/// Fixed Wasm pages allocated to each canonical state memory.
pub const CANONICAL_STATE_MEMORY_PAGES: u64 = 65;
/// Fixed byte capacity of each canonical state memory.
pub const CANONICAL_STATE_MEMORY_BYTES: usize = CANONICAL_STATE_MEMORY_PAGES as usize * 65_536;

/// Failure to construct a bounded semantic state value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StateBytesError {
    /// Shared state exceeded its explicit bound.
    #[error("shared state is {actual} bytes; maximum is {max}")]
    SharedTooLarge { actual: usize, max: usize },
    /// Local state exceeded its explicit bound.
    #[error("local state is {actual} bytes; maximum is {max}")]
    LocalTooLarge { actual: usize, max: usize },
    /// A payload cannot fit in the fixed canonical memory frame.
    #[error("state payload is {actual} bytes; canonical frame allows {max}")]
    CanonicalTooLarge { actual: usize, max: usize },
}

/// Reconstruct one fixed canonical state-memory image from its payload.
///
/// The result is exactly [`CANONICAL_STATE_MEMORY_BYTES`] bytes containing a
/// little-endian `u32` payload length, the payload, and an all-zero tail. The
/// generic input keeps this helper usable for both bounded shared and local
/// state newtypes without coupling the program crate to protocol hashing.
pub fn canonical_state_image(payload: impl AsRef<[u8]>) -> Result<Vec<u8>, StateBytesError> {
    let payload = payload.as_ref();
    let max_payload = CANONICAL_STATE_MEMORY_BYTES - CANONICAL_STATE_PREFIX_BYTES;
    if payload.len() > max_payload {
        return Err(StateBytesError::CanonicalTooLarge {
            actual: payload.len(),
            max: max_payload,
        });
    }

    let mut image = vec![0; CANONICAL_STATE_MEMORY_BYTES];
    let payload_len = u32::try_from(payload.len()).expect("canonical state length fits in u32");
    image[..CANONICAL_STATE_PREFIX_BYTES].copy_from_slice(&payload_len.to_le_bytes());
    let end = CANONICAL_STATE_PREFIX_BYTES + payload.len();
    image[CANONICAL_STATE_PREFIX_BYTES..end].copy_from_slice(payload);
    Ok(image)
}

macro_rules! bounded_state_bytes {
    ($(#[$meta:meta])* $name:ident, $max:expr, $label:literal, $too_large:ident) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash, Serialize, BorshSerialize)]
        #[serde(transparent)]
        pub struct $name(Vec<u8>);

        impl $name {
            /// Maximum number of bytes accepted by this type.
            pub const MAX_LEN: usize = $max;

            /// Construct state after enforcing [`Self::MAX_LEN`].
            pub fn try_new(bytes: impl Into<Vec<u8>>) -> Result<Self, StateBytesError> {
                let bytes = bytes.into();
                if bytes.len() > Self::MAX_LEN {
                    return Err(StateBytesError::$too_large {
                        actual: bytes.len(),
                        max: Self::MAX_LEN,
                    });
                }
                Ok(Self(bytes))
            }

            /// Construct state from borrowed bytes after enforcing the bound.
            pub fn try_from_slice(bytes: &[u8]) -> Result<Self, StateBytesError> {
                Self::try_new(bytes.to_vec())
            }

            /// Borrow the opaque state bytes.
            #[must_use]
            pub fn as_bytes(&self) -> &[u8] {
                &self.0
            }

            /// Consume the value and return its bytes.
            #[must_use]
            pub fn into_bytes(self) -> Vec<u8> {
                self.0
            }

            /// Number of bytes in this state value.
            #[must_use]
            pub fn len(&self) -> usize {
                self.0.len()
            }

            /// Whether this state value contains no bytes.
            #[must_use]
            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl AsRef<[u8]> for $name {
            fn as_ref(&self) -> &[u8] {
                self.as_bytes()
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                deserialize_bounded(deserializer, Self::MAX_LEN, $label, Self)
            }
        }

        impl BorshDeserialize for $name {
            fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
                bounded::read_bytes::<{ Self::MAX_LEN }>(reader).map(Self)
            }
        }

        impl TryFrom<Vec<u8>> for $name {
            type Error = StateBytesError;

            fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
                Self::try_new(bytes)
            }
        }

        impl TryFrom<&[u8]> for $name {
            type Error = StateBytesError;

            fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
                Self::try_from_slice(bytes)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("len", &self.len())
                    .finish()
            }
        }
    };
}

bounded_state_bytes! {
    /// Opaque, bounded bytes for replicated program state.
    SharedStateBytes, MAX_SHARED_STATE_BYTES, "shared", SharedTooLarge
}

bounded_state_bytes! {
    /// Opaque, bounded bytes for participant-local program state.
    LocalStateBytes, MAX_LOCAL_STATE_BYTES, "local", LocalTooLarge
}

/// Deserialize a byte sequence without allowing its backing allocation to
/// grow past the semantic maximum.
fn deserialize_bounded<'de, D, T>(
    deserializer: D,
    max: usize,
    label: &'static str,
    build: fn(Vec<u8>) -> T,
) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_byte_buf(BoundedBytesVisitor { max, label, build })
}

struct BoundedBytesVisitor<T> {
    max: usize,
    label: &'static str,
    build: fn(Vec<u8>) -> T,
}

impl<'de, T> Visitor<'de> for BoundedBytesVisitor<T> {
    type Value = T;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "at most {} bytes of {} state",
            self.max, self.label
        )
    }

    fn visit_bytes<E>(self, bytes: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if bytes.len() > self.max {
            return Err(self.oversized(bytes.len()));
        }
        Ok((self.build)(bytes.to_vec()))
    }

    fn visit_byte_buf<E>(self, bytes: Vec<u8>) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if bytes.len() > self.max {
            return Err(self.oversized(bytes.len()));
        }
        Ok((self.build)(bytes))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        if sequence.size_hint().is_some_and(|hint| hint > self.max) {
            return Err(self.oversized(self.max.saturating_add(1)));
        }

        // Allocate at most the semantic bound up front. This avoids Vec's
        // geometric growth allocating a capacity larger than `self.max` near
        // the boundary when a sequence has no useful size hint.
        let mut bytes = Vec::with_capacity(self.max);
        while bytes.len() < self.max {
            let Some(byte) = sequence.next_element::<u8>()? else {
                return Ok((self.build)(bytes));
            };
            bytes.push(byte);
        }

        if sequence.next_element::<IgnoredAny>()?.is_some() {
            return Err(self.oversized(self.max.saturating_add(1)));
        }
        Ok((self.build)(bytes))
    }
}

impl<T> BoundedBytesVisitor<T> {
    fn oversized<E: de::Error>(&self, actual: usize) -> E {
        E::custom(format!(
            "{} state is {actual} bytes; maximum is {}",
            self.label, self.max
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_state_image_accepts_bounded_payloads_at_their_limit() {
        let shared = SharedStateBytes::try_from(vec![0xa5; MAX_SHARED_STATE_BYTES]).unwrap();
        let shared_image = canonical_state_image(&shared).unwrap();
        assert_eq!(shared_image.len(), CANONICAL_STATE_MEMORY_BYTES);
        assert_eq!(
            u32::from_le_bytes(
                shared_image[..CANONICAL_STATE_PREFIX_BYTES]
                    .try_into()
                    .unwrap()
            ),
            MAX_SHARED_STATE_BYTES as u32
        );
        assert_eq!(
            &shared_image
                [CANONICAL_STATE_PREFIX_BYTES..CANONICAL_STATE_PREFIX_BYTES + shared.len()],
            shared.as_bytes()
        );
        assert!(
            shared_image[CANONICAL_STATE_PREFIX_BYTES + shared.len()..]
                .iter()
                .all(|byte| *byte == 0)
        );

        let local = LocalStateBytes::try_from(vec![0x5a; MAX_LOCAL_STATE_BYTES]).unwrap();
        let local_image = canonical_state_image(&local).unwrap();
        assert_eq!(
            &local_image[CANONICAL_STATE_PREFIX_BYTES..CANONICAL_STATE_PREFIX_BYTES + local.len()],
            local.as_bytes()
        );
        assert!(
            local_image[CANONICAL_STATE_PREFIX_BYTES + local.len()..]
                .iter()
                .all(|byte| *byte == 0)
        );
    }

    #[test]
    fn shared_state_accepts_at_bound_and_rejects_above_it() {
        assert_eq!(
            SharedStateBytes::try_from(vec![0; MAX_SHARED_STATE_BYTES])
                .unwrap()
                .len(),
            MAX_SHARED_STATE_BYTES
        );
        assert!(matches!(
            SharedStateBytes::try_from(vec![0; MAX_SHARED_STATE_BYTES + 1]),
            Err(StateBytesError::SharedTooLarge { .. })
        ));
    }

    #[test]
    fn borsh_round_trips_without_changing_vec_encoding() {
        let shared = SharedStateBytes::try_from_slice(b"shared").unwrap();
        let local = LocalStateBytes::try_from_slice(b"local").unwrap();

        let shared_bytes = borsh::to_vec(&shared).unwrap();
        let local_bytes = borsh::to_vec(&local).unwrap();
        assert_eq!(shared_bytes, borsh::to_vec(&b"shared".to_vec()).unwrap());
        assert_eq!(local_bytes, borsh::to_vec(&b"local".to_vec()).unwrap());
        assert_eq!(
            <SharedStateBytes as BorshDeserialize>::try_from_slice(&shared_bytes).unwrap(),
            shared
        );
        assert_eq!(
            <LocalStateBytes as BorshDeserialize>::try_from_slice(&local_bytes).unwrap(),
            local
        );
    }

    #[test]
    fn borsh_oversized_prefix_is_rejected_before_body_allocation() {
        let prefix = u32::MAX.to_le_bytes();
        let shared_error =
            <SharedStateBytes as BorshDeserialize>::try_from_slice(&prefix).unwrap_err();
        let local_error =
            <LocalStateBytes as BorshDeserialize>::try_from_slice(&prefix).unwrap_err();
        // The shared bounded codec names the length and bound, not the field.
        assert!(shared_error.to_string().contains("exceeds bound"));
        assert!(local_error.to_string().contains("exceeds bound"));
    }

    fn oversized_json(len: usize) -> String {
        let mut json = String::with_capacity(len * 2 + 2);
        json.push('[');
        for index in 0..len {
            if index > 0 {
                json.push(',');
            }
            json.push('0');
        }
        json.push(']');
        json
    }

    #[test]
    fn serde_oversized_sequences_are_rejected() {
        let json = oversized_json(MAX_SHARED_STATE_BYTES + 1);
        let shared_error = serde_json::from_str::<SharedStateBytes>(&json).unwrap_err();
        let local_error = serde_json::from_str::<LocalStateBytes>(&json).unwrap_err();
        assert!(shared_error.to_string().contains("maximum"));
        assert!(local_error.to_string().contains("maximum"));
    }

    #[test]
    fn local_state_accepts_at_bound_and_rejects_above_it() {
        assert_eq!(
            LocalStateBytes::try_from(vec![0; MAX_LOCAL_STATE_BYTES])
                .unwrap()
                .len(),
            MAX_LOCAL_STATE_BYTES
        );
        assert!(matches!(
            LocalStateBytes::try_from(vec![0; MAX_LOCAL_STATE_BYTES + 1]),
            Err(StateBytesError::LocalTooLarge { .. })
        ));
    }
}
