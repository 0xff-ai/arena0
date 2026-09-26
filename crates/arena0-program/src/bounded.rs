//! Bounded Borsh field codecs, used through
//! `#[borsh(serialize_with = "...", deserialize_with = "...")]` on fields whose
//! encoding carries a length bound. Readers reject an over-limit length prefix
//! before they allocate. Each encoding is the stock Borsh encoding of the
//! unbounded type.

use std::io;

use borsh::{BorshDeserialize, BorshSerialize};

/// Write stock-Borsh `Vec<u8>` bytes, rejecting lengths over `MAX`.
pub fn write_bytes<const MAX: usize>(bytes: &[u8], writer: &mut impl io::Write) -> io::Result<()> {
    let len = bytes.len();
    if len > MAX {
        return Err(over_bound(len, MAX));
    }
    let length = u32::try_from(len).map_err(|_| over_bound(len, MAX))?;
    length.serialize(writer)?;
    writer.write_all(bytes)
}

/// Read stock-Borsh `Vec<u8>` bytes, checking the length prefix before allocating.
pub fn read_bytes<const MAX: usize>(reader: &mut impl io::Read) -> io::Result<Vec<u8>> {
    let len = read_len::<MAX>(reader)?;
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Write a stock-Borsh `String`, rejecting UTF-8 lengths over `MAX`.
pub fn write_string<const MAX: usize>(value: &str, writer: &mut impl io::Write) -> io::Result<()> {
    write_bytes::<MAX>(value.as_bytes(), writer)
}

/// Read a stock-Borsh `String`, checking the length prefix before allocating.
pub fn read_string<const MAX: usize>(reader: &mut impl io::Read) -> io::Result<String> {
    String::from_utf8(read_bytes::<MAX>(reader)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Write a stock-Borsh `Option<String>` with its `0/1` tag, bounding the inner string.
#[allow(
    clippy::ref_option,
    reason = "borsh serialize_with passes the field by reference"
)]
pub fn write_option_string<const MAX: usize>(
    value: &Option<String>,
    writer: &mut impl io::Write,
) -> io::Result<()> {
    match value {
        None => 0u8.serialize(writer),
        Some(value) => {
            1u8.serialize(writer)?;
            write_string::<MAX>(value, writer)
        }
    }
}

/// Read a stock-Borsh `Option<String>`, rejecting tags other than `0/1`.
pub fn read_option_string<const MAX: usize>(
    reader: &mut impl io::Read,
) -> io::Result<Option<String>> {
    match u8::deserialize_reader(reader)? {
        0 => Ok(None),
        1 => Ok(Some(read_string::<MAX>(reader)?)),
        tag => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown option tag {tag}"),
        )),
    }
}

/// Write a stock-Borsh `Vec<T>`, rejecting element counts over `MAX`.
pub fn write_vec<const MAX: usize, T: BorshSerialize>(
    items: &[T],
    writer: &mut impl io::Write,
) -> io::Result<()> {
    let len = items.len();
    if len > MAX {
        return Err(over_bound(len, MAX));
    }
    let length = u32::try_from(len).map_err(|_| over_bound(len, MAX))?;
    length.serialize(writer)?;
    for item in items {
        item.serialize(writer)?;
    }
    Ok(())
}

/// Read a stock-Borsh `Vec<T>`, checking the element count before allocating.
pub fn read_vec<const MAX: usize, T: BorshDeserialize>(
    reader: &mut impl io::Read,
) -> io::Result<Vec<T>> {
    let len = read_len::<MAX>(reader)?;
    let mut items = Vec::with_capacity(len);
    for _ in 0..len {
        items.push(T::deserialize_reader(reader)?);
    }
    Ok(items)
}

/// Read one `u32` length prefix and reject it before the caller allocates.
fn read_len<const MAX: usize>(reader: &mut impl io::Read) -> io::Result<usize> {
    let len = u32::deserialize_reader(reader)? as usize;
    if len > MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("length {len} exceeds bound {MAX}"),
        ));
    }
    Ok(len)
}

/// Reject an over-bound write with the shared length message.
fn over_bound(len: usize, max: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("length {len} exceeds bound {max}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_bound_writes_are_rejected_before_encoding() {
        let oversized = vec![0u8; 3];
        let error = write_bytes::<2>(&oversized, &mut Vec::new()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn over_bound_byte_prefixes_are_rejected_before_allocation() {
        let prefix = 3u32.to_le_bytes();
        let error = read_bytes::<2>(&mut prefix.as_slice()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn over_bound_string_prefixes_are_rejected_before_allocation() {
        let prefix = 3u32.to_le_bytes();
        let error = read_string::<2>(&mut prefix.as_slice()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn over_bound_option_prefixes_are_rejected_before_allocation() {
        // Tag 1 followed by an over-bound string length, with no payload.
        let encoded = [1u8, 3, 0, 0, 0];
        let error = read_option_string::<2>(&mut encoded.as_slice()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn over_bound_vec_counts_are_rejected_before_allocation() {
        let prefix = 3u32.to_le_bytes();
        let error = read_vec::<2, u8>(&mut prefix.as_slice()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
