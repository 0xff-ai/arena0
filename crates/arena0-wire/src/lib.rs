//! Versioned, bounded wire values and canonical frame encoding.
//!
//! This crate owns the representations exchanged by protocol transports and
//! the canonical length-prefixed Borsh encoding for those values. It has no
//! transport, runtime, storage, or asynchronous I/O dependencies. A transport
//! supplies the bytes and owns delivery; this crate only validates and encodes
//! the wire representation. Protocol-domain conversion belongs to
//! `arena0-protocol`.

mod codec;
mod error;
mod exec;
mod fetch;
mod stream;

use borsh::{BorshDeserialize, BorshSerialize};
use std::io;

mod sealed {
    pub trait WireDecode {}
}

/// A wire value with a decoder that enforces its field-level bounds before
/// allocating variable-length fields. This trait is sealed and is implemented
/// only for the raw frame values defined by this crate.
pub trait WireDecode: sealed::WireDecode + BorshDeserialize {}

impl<T> WireDecode for T where T: sealed::WireDecode + BorshDeserialize {}

pub use codec::{
    Codec, DEFAULT_MAX_MESSAGE_SIZE, FRAME_HEADER_SIZE, FRAME_VERSION, FRAME_VERSION_SIZE,
};
pub use error::WireError;
pub use exec::{
    ABORT_KIND_ABORT, ABORT_KIND_FAIL, EXEC_KIND_ABORT, EXEC_KIND_END, EXEC_KIND_MESSAGE,
    EXEC_KIND_STEP_SIGNATURE, ExecFrame, MAX_EXEC_ABORT_OCCURRENCE_BYTES, MAX_EXEC_MESSAGE_BYTES,
    MAX_EXEC_REASON_BYTES, MessageIdBytes, PROTO_EXEC, PROTO_FETCH, PeerIdBytes, SessionHashBytes,
    StateHashBytes, WireAbortCoordinate, WireAbortOccurrence, WireStepCommitment,
    WireTerminalCommitment, WitnessCommitmentBytes,
};
pub use fetch::{
    FETCH_KIND_REQUEST, FETCH_KIND_RESPONSE, FetchFrame, MAX_FETCH_RESPONSE_BYTES,
    MAX_FETCH_TICKET_BYTES, MAX_FETCH_TICKETS,
};
pub use stream::StreamProtocol;

pub(crate) fn serialize_bounded_bytes<W: io::Write>(
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

pub(crate) fn read_bounded_bytes<R: io::Read>(
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
