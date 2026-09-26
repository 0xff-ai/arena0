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

/// A state-memory frame that violates `len || payload || zero_tail`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StateFrameError {
    /// The memory cannot hold the length prefix.
    #[error("state memory is smaller than its length prefix")]
    MissingPrefix,
    /// The memory is not exactly one canonical state memory.
    #[error("state memory has {actual} bytes; expected {expected}")]
    WrongSize { actual: usize, expected: usize },
    /// The payload exceeds the caller's bound.
    #[error("payload length {actual} exceeds maximum {max}")]
    TooLarge { actual: usize, max: usize },
    /// The payload does not fit after the prefix.
    #[error("payload length {actual} exceeds the {capacity}-byte state memory")]
    ExceedsMemory { actual: usize, capacity: usize },
    /// A byte after the payload is not zero.
    #[error("state memory has non-zero bytes after its payload")]
    NonZeroTail,
}

/// The payload length recorded in a state-memory frame, after checking that
/// it is within `max_payload` and fits in `frame`. The tail is not scanned.
pub fn state_frame_len(frame: &[u8], max_payload: usize) -> Result<usize, StateFrameError> {
    let prefix: [u8; CANONICAL_STATE_PREFIX_BYTES] = frame
        .get(..CANONICAL_STATE_PREFIX_BYTES)
        .and_then(|prefix| prefix.try_into().ok())
        .ok_or(StateFrameError::MissingPrefix)?;
    let len = u32::from_le_bytes(prefix) as usize;
    if len > max_payload {
        return Err(StateFrameError::TooLarge {
            actual: len,
            max: max_payload,
        });
    }
    if len > frame.len() - CANONICAL_STATE_PREFIX_BYTES {
        return Err(StateFrameError::ExceedsMemory {
            actual: len,
            capacity: frame.len(),
        });
    }
    Ok(len)
}

/// The payload of one complete canonical state memory: exactly
/// [`CANONICAL_STATE_MEMORY_BYTES`] bytes, a payload within `max_payload`, and
/// an all-zero tail.
pub fn canonical_state_payload(frame: &[u8], max_payload: usize) -> Result<&[u8], StateFrameError> {
    if frame.len() != CANONICAL_STATE_MEMORY_BYTES {
        return Err(StateFrameError::WrongSize {
            actual: frame.len(),
            expected: CANONICAL_STATE_MEMORY_BYTES,
        });
    }
    let end = CANONICAL_STATE_PREFIX_BYTES + state_frame_len(frame, max_payload)?;
    if frame[end..].iter().any(|byte| *byte != 0) {
        return Err(StateFrameError::NonZeroTail);
    }
    Ok(&frame[CANONICAL_STATE_PREFIX_BYTES..end])
}

/// Replace the payload of a state-memory frame whose current payload is
/// `old_len` bytes, zeroing the bytes the shorter payload no longer covers.
/// With a zero tail before the write, the frame keeps a zero tail after it.
pub fn write_state_frame(
    frame: &mut [u8],
    old_len: usize,
    payload: &[u8],
) -> Result<(), StateFrameError> {
    let capacity = frame
        .len()
        .checked_sub(CANONICAL_STATE_PREFIX_BYTES)
        .ok_or(StateFrameError::MissingPrefix)?;
    if payload.len().max(old_len) > capacity {
        return Err(StateFrameError::ExceedsMemory {
            actual: payload.len().max(old_len),
            capacity: frame.len(),
        });
    }
    let payload_len = u32::try_from(payload.len()).map_err(|_| StateFrameError::ExceedsMemory {
        actual: payload.len(),
        capacity: frame.len(),
    })?;
    let end = CANONICAL_STATE_PREFIX_BYTES + payload.len();
    if old_len > payload.len() {
        frame[end..CANONICAL_STATE_PREFIX_BYTES + old_len].fill(0);
    }
    frame[..CANONICAL_STATE_PREFIX_BYTES].copy_from_slice(&payload_len.to_le_bytes());
    frame[CANONICAL_STATE_PREFIX_BYTES..end].copy_from_slice(payload);
    Ok(())
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
    write_state_frame(&mut image, 0, payload).expect("payload fits the canonical frame");
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
    fn state_frame_rewrite_zeroes_the_stale_tail() {
        let mut frame = canonical_state_image(b"longer").unwrap();
        write_state_frame(&mut frame, 6, b"ab").unwrap();
        assert_eq!(canonical_state_payload(&frame, 8), Ok(&b"ab"[..]));
        assert_eq!(frame, canonical_state_image(b"ab").unwrap());
        assert_eq!(
            state_frame_len(&frame, 1),
            Err(StateFrameError::TooLarge { actual: 2, max: 1 })
        );
        assert_eq!(
            state_frame_len(&[0; 3], 8),
            Err(StateFrameError::MissingPrefix)
        );
        assert_eq!(
            state_frame_len(&[9, 0, 0, 0, 0], 16),
            Err(StateFrameError::ExceedsMemory {
                actual: 9,
                capacity: 5
            })
        );

        frame[CANONICAL_STATE_PREFIX_BYTES + 2] = 1;
        assert_eq!(
            canonical_state_payload(&frame, 8),
            Err(StateFrameError::NonZeroTail)
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
