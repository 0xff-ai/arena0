//! Versioned, bounded wire values and canonical frame encoding.
//!
//! This crate owns the representations exchanged by protocol transports and
//! the canonical length-prefixed Borsh encoding for those values. It has no
//! transport, runtime, storage, or asynchronous I/O dependencies. A transport
//! supplies the bytes and owns delivery; this crate only validates and encodes
//! the wire representation. Execution and fetch frames carry protocol-domain
//! values, so `arena0-protocol` owns their encoding on top of this crate's
//! bounded field codec.

mod codec;
mod error;
mod fetch;
mod stream;

use borsh::{BorshDeserialize, BorshSerialize};
use std::io;

/// A frame value whose decoder enforces its field-level bounds before
/// allocating variable-length fields. Implement it only for frame values
/// whose every variable-length field is read through [`read_bounded_bytes`]
/// or an equivalent bounded reader.
pub trait WireDecode: BorshDeserialize {}

pub use codec::{
    Codec, DEFAULT_MAX_MESSAGE_SIZE, FRAME_HEADER_SIZE, FRAME_VERSION, FRAME_VERSION_SIZE,
};
pub use error::WireError;
pub use fetch::{MAX_FETCH_RESPONSE_BYTES, MAX_FETCH_TICKET_BYTES, MAX_FETCH_TICKETS};
pub use stream::{MAX_EXEC_FRAME_BYTES, PROTO_EXEC, PROTO_FETCH, StreamProtocol};

/// Write length-prefixed bytes, reporting an over-bound value as a typed
/// [`WireError::ValueTooLarge`] naming `field`.
pub fn serialize_bounded_bytes<W: io::Write>(
    writer: &mut W,
    bytes: &[u8],
    max: usize,
    field: &'static str,
) -> io::Result<()> {
    if bytes.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            WireError::ValueTooLarge {
                field,
                size: bytes.len(),
                max,
            },
        ));
    }
    let length = u32::try_from(bytes.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            WireError::ValueTooLarge {
                field,
                size: bytes.len(),
                max: u32::MAX as usize,
            },
        )
    })?;
    BorshSerialize::serialize(&length, writer)?;
    writer.write_all(bytes)
}

/// Reject a collection of `len` elements when it exceeds `max`, as a typed
/// [`WireError::CollectionTooLarge`] naming `field`.
pub fn check_collection_len(len: usize, max: usize, field: &'static str) -> io::Result<()> {
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            WireError::CollectionTooLarge {
                field,
                size: len,
                max,
            },
        ));
    }
    Ok(())
}

/// Write the `u32` element count of a bounded collection after checking it
/// with [`check_collection_len`].
pub fn write_collection_len<W: io::Write>(
    writer: &mut W,
    len: usize,
    max: usize,
    field: &'static str,
) -> io::Result<()> {
    check_collection_len(len, max, field)?;
    let length = u32::try_from(len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            WireError::CollectionTooLarge {
                field,
                size: len,
                max: u32::MAX as usize,
            },
        )
    })?;
    BorshSerialize::serialize(&length, writer)
}

/// Read length-prefixed bytes, rejecting an over-bound length with a typed
/// [`WireError::ValueTooLarge`] before allocating.
pub fn read_bounded_bytes<R: io::Read>(
    reader: &mut R,
    max: usize,
    field: &'static str,
) -> io::Result<Vec<u8>> {
    let length = u32::deserialize_reader(reader)?;
    let length = usize::try_from(length).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{field} length overflows usize"),
        )
    })?;
    if length > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            WireError::ValueTooLarge {
                field,
                size: length,
                max,
            },
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Read the `u32` element count of a bounded collection, rejecting an
/// over-bound count with a typed [`WireError::CollectionTooLarge`] before the
/// caller allocates.
pub fn read_collection_len<R: io::Read>(
    reader: &mut R,
    max: usize,
    field: &'static str,
) -> io::Result<usize> {
    let length = u32::deserialize_reader(reader)?;
    let length = usize::try_from(length).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{field} length overflows usize"),
        )
    })?;
    if length > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            WireError::CollectionTooLarge {
                field,
                size: length,
                max,
            },
        ));
    }
    Ok(length)
}
