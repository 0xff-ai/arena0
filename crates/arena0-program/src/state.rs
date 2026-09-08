//! Bounded semantic state values.
//!
//! State bytes are opaque to the host, but their role is not interchangeable:
//! shared bytes are consensus-visible and hashed, while local bytes belong to
//! one participant and are never included in shared commitments. These
//! newtypes make that distinction explicit at every caller.

use std::fmt;
use std::io;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::de::{self, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

/// Maximum accepted serialized shared-state length.
pub const MAX_SHARED_STATE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum accepted serialized participant-local-state length.
pub const MAX_LOCAL_STATE_BYTES: usize = 4 * 1024 * 1024;

/// Failure to construct a bounded semantic state value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StateBytesError {
    /// Shared state exceeded its explicit bound.
    #[error("shared state is {actual} bytes; maximum is {max}")]
    SharedTooLarge { actual: usize, max: usize },
    /// Local state exceeded its explicit bound.
    #[error("local state is {actual} bytes; maximum is {max}")]
    LocalTooLarge { actual: usize, max: usize },
}

/// Opaque, bounded bytes for replicated program state.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, BorshSerialize)]
#[serde(transparent)]
pub struct SharedStateBytes(Vec<u8>);

impl SharedStateBytes {
    /// Maximum number of bytes accepted by this type.
    pub const MAX_LEN: usize = MAX_SHARED_STATE_BYTES;

    /// Construct shared state after enforcing [`Self::MAX_LEN`].
    pub fn try_new(bytes: impl Into<Vec<u8>>) -> Result<Self, StateBytesError> {
        let bytes = bytes.into();
        if bytes.len() > Self::MAX_LEN {
            return Err(StateBytesError::SharedTooLarge {
                actual: bytes.len(),
                max: Self::MAX_LEN,
            });
        }
        Ok(Self(bytes))
    }

    /// Construct shared state from borrowed bytes after enforcing the bound.
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

impl AsRef<[u8]> for SharedStateBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl<'de> Deserialize<'de> for SharedStateBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_bounded(deserializer, Self::MAX_LEN, "shared", Self)
    }
}

impl BorshDeserialize for SharedStateBytes {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        read_borsh_bounded(reader, Self::MAX_LEN, "shared").map(Self)
    }
}

impl TryFrom<Vec<u8>> for SharedStateBytes {
    type Error = StateBytesError;

    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        Self::try_new(bytes)
    }
}

impl TryFrom<&[u8]> for SharedStateBytes {
    type Error = StateBytesError;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        Self::try_from_slice(bytes)
    }
}

impl fmt::Debug for SharedStateBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedStateBytes")
            .field("len", &self.len())
            .finish()
    }
}

/// Opaque, bounded bytes for participant-local program state.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, BorshSerialize)]
#[serde(transparent)]
pub struct LocalStateBytes(Vec<u8>);

impl LocalStateBytes {
    /// Maximum number of bytes accepted by this type.
    pub const MAX_LEN: usize = MAX_LOCAL_STATE_BYTES;

    /// Construct local state after enforcing [`Self::MAX_LEN`].
    pub fn try_new(bytes: impl Into<Vec<u8>>) -> Result<Self, StateBytesError> {
        let bytes = bytes.into();
        if bytes.len() > Self::MAX_LEN {
            return Err(StateBytesError::LocalTooLarge {
                actual: bytes.len(),
                max: Self::MAX_LEN,
            });
        }
        Ok(Self(bytes))
    }

    /// Construct local state from borrowed bytes after enforcing the bound.
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

impl AsRef<[u8]> for LocalStateBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl<'de> Deserialize<'de> for LocalStateBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_bounded(deserializer, Self::MAX_LEN, "local", Self)
    }
}

impl BorshDeserialize for LocalStateBytes {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        read_borsh_bounded(reader, Self::MAX_LEN, "local").map(Self)
    }
}

impl TryFrom<Vec<u8>> for LocalStateBytes {
    type Error = StateBytesError;

    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        Self::try_new(bytes)
    }
}

impl TryFrom<&[u8]> for LocalStateBytes {
    type Error = StateBytesError;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        Self::try_from_slice(bytes)
    }
}

impl fmt::Debug for LocalStateBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalStateBytes")
            .field("len", &self.len())
            .finish()
    }
}

/// Read a Borsh `Vec<u8>` after checking its length prefix, so a hostile
/// length cannot trigger an unbounded allocation.
fn read_borsh_bounded<R: io::Read>(reader: &mut R, max: usize, label: &str) -> io::Result<Vec<u8>> {
    let length = u32::deserialize_reader(reader)?;
    let length = usize::try_from(length).expect("u32 length fits usize");
    if length > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} state is {length} bytes; maximum is {max}"),
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
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
        assert!(shared_error.to_string().contains("maximum"));
        assert!(local_error.to_string().contains("maximum"));
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
    fn serde_round_trips_bounded_state() {
        let shared = SharedStateBytes::try_from_slice(b"shared").unwrap();
        let local = LocalStateBytes::try_from_slice(b"local").unwrap();
        let shared_json = serde_json::to_string(&shared).unwrap();
        let local_json = serde_json::to_string(&local).unwrap();
        assert_eq!(
            serde_json::from_str::<SharedStateBytes>(&shared_json).unwrap(),
            shared
        );
        assert_eq!(
            serde_json::from_str::<LocalStateBytes>(&local_json).unwrap(),
            local
        );
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
