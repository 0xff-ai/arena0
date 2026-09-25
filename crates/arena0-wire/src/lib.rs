//! Versioned, bounded wire values and canonical frame encoding.
//!
//! This crate owns the representations exchanged by protocol transports and
//! the canonical length-prefixed Borsh encoding for those values. It has no
//! transport, runtime, storage, or asynchronous I/O dependencies. A transport
//! supplies the bytes and owns delivery; this crate only validates and encodes
//! the wire representation. Execution frames carry protocol-domain values, so
//! `arena0-protocol` owns their encoding on top of this crate's bounded codec.

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
pub use fetch::{
    FETCH_KIND_REQUEST, FETCH_KIND_RESPONSE, FetchFrame, MAX_FETCH_RESPONSE_BYTES,
    MAX_FETCH_TICKET_BYTES, MAX_FETCH_TICKETS, SessionHashBytes,
};
pub use stream::{PROTO_EXEC, PROTO_FETCH, StreamProtocol};

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

pub(crate) fn serialize_bounded_vec<W: io::Write, T: BorshSerialize>(
    writer: &mut W,
    values: &[T],
    max: usize,
    field: &'static str,
) -> io::Result<()> {
    if values.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            WireError::CollectionTooLarge {
                field,
                size: values.len(),
                max,
            },
        ));
    }
    let length = u32::try_from(values.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            WireError::CollectionTooLarge {
                field,
                size: values.len(),
                max: u32::MAX as usize,
            },
        )
    })?;
    BorshSerialize::serialize(&length, writer)?;
    for value in values {
        BorshSerialize::serialize(value, writer)?;
    }
    Ok(())
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

pub(crate) fn read_bounded_bytes_vec<R: io::Read>(
    reader: &mut R,
    max_items: usize,
    max_item_bytes: usize,
    items_field: &'static str,
    item_field: &'static str,
) -> io::Result<Vec<Vec<u8>>> {
    let length = u32::deserialize_reader(reader)?;
    let length = usize::try_from(length).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{items_field} length overflows usize"),
        )
    })?;
    if length > max_items {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            WireError::CollectionTooLarge {
                field: items_field,
                size: length,
                max: max_items,
            },
        ));
    }
    let mut values = Vec::with_capacity(length);
    for _ in 0..length {
        values.push(read_bounded_bytes(reader, max_item_bytes, item_field)?);
    }
    Ok(values)
}
