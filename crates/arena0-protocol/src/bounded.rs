//! Shared bounded Borsh field decoding for protocol-owned representations.

use std::io;

use borsh::{BorshDeserialize, BorshSerialize};

pub(crate) fn read_bytes<R: io::Read>(
    reader: &mut R,
    max: usize,
    field: &'static str,
) -> io::Result<Vec<u8>> {
    let len = u32::deserialize_reader(reader)? as usize;
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{field} exceeds bound"),
        ));
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub(crate) fn read_string<R: io::Read>(
    reader: &mut R,
    max: usize,
    field: &'static str,
) -> io::Result<String> {
    let bytes = read_bytes(reader, max, field)?;
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(crate) fn write_bytes<W: io::Write>(
    writer: &mut W,
    bytes: &[u8],
    max: usize,
    field: &'static str,
) -> io::Result<()> {
    if bytes.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{field} exceeds bound"),
        ));
    }
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{field} is too long")))?;
    BorshSerialize::serialize(&len, writer)?;
    writer.write_all(bytes)
}

pub(crate) fn write_string<W: io::Write>(
    writer: &mut W,
    value: &str,
    max: usize,
    field: &'static str,
) -> io::Result<()> {
    write_bytes(writer, value.as_bytes(), max, field)
}
