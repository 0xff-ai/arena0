//! Exact Bao slices for Host-owned object ranges.

use std::io::{Cursor, Read};

/// The Bao 0.13 combined-encoding slice of `content[start..start + len]`.
/// Panics if the range is empty or outside `content` (callers validate first).
pub fn encode_slice(content: &[u8], start: u64, len: u64) -> Vec<u8> {
    assert!(len > 0 && start <= content.len() as u64 && len <= content.len() as u64 - start);
    let (encoded, _) = ::bao::encode::encode(content);
    let mut extractor = ::bao::encode::SliceExtractor::new(Cursor::new(encoded), start, len);
    let mut slice = Vec::new();
    extractor
        .read_to_end(&mut slice)
        .expect("in-memory Bao slice extraction");
    slice
}

/// The bytes `start..start + len` of the object `(hash, length)` proven by
/// `slice`, or None. Exact: the slice's 8-byte header must equal `length`, the
/// decoder must yield exactly `len` bytes, and it must consume the whole slice
/// (Bao tolerates trailing bytes; this does not). Returns None for an empty or
/// out-of-bounds range.
pub fn decode_slice(
    slice: &[u8],
    hash: &[u8; 32],
    length: u64,
    start: u64,
    len: u64,
) -> Option<Vec<u8>> {
    if len == 0
        || start > length
        || len > length - start
        || slice.get(..8) != Some(length.to_le_bytes().as_slice())
    {
        return None;
    }
    let mut decoder =
        ::bao::decode::SliceDecoder::new(Cursor::new(slice), &(*hash).into(), start, len);
    let mut bytes = Vec::new();
    decoder
        .by_ref()
        .take(len.checked_add(1)?)
        .read_to_end(&mut bytes)
        .ok()?;
    // Bao accepts trailing bytes for streaming use; a direct attachment is one
    // complete proof, so accepting a suffix would make its framing ambiguous.
    (bytes.len() as u64 == len && decoder.into_inner().position() == slice.len() as u64)
        .then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_round_trip_and_reject_tampering() {
        let content: Vec<u8> = (0..8192).map(|n| (n % 251) as u8).collect();
        let hash = *blake3::hash(&content).as_bytes();
        let slice = encode_slice(&content, 1023, 2048);
        assert_eq!(
            decode_slice(&slice, &hash, 8192, 1023, 2048),
            Some(content[1023..3071].to_vec())
        );
        let mut tampered = slice.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert_eq!(decode_slice(&tampered, &hash, 8192, 1023, 2048), None);
        assert_eq!(decode_slice(&slice, &[0; 32], 8192, 1023, 2048), None);
        let mut wrong_header = slice.clone();
        wrong_header[..8].copy_from_slice(&8191u64.to_le_bytes());
        assert_eq!(decode_slice(&wrong_header, &hash, 8192, 1023, 2048), None);
        let mut trailing = slice.clone();
        trailing.push(0);
        assert_eq!(decode_slice(&trailing, &hash, 8192, 1023, 2048), None);
        assert_eq!(decode_slice(&slice, &hash, 8192, 4096, 2048), None);
        assert_eq!(decode_slice(&slice, &hash, 8192, 0, 0), None);
        assert_eq!(decode_slice(&slice, &hash, 8192, 8191, 2), None);
    }

    #[test]
    fn worst_case_slice_fits_max_direct_slice_bytes() {
        use arena0_protocol::{MAX_BLOB_BYTES, MAX_DIRECT_RANGE_BYTES, MAX_DIRECT_SLICE_BYTES};
        let content = vec![0x5a; MAX_BLOB_BYTES as usize];
        let (encoded, _) = ::bao::encode::encode(&content);
        let mut starts = vec![0, 1023, MAX_BLOB_BYTES - MAX_DIRECT_RANGE_BYTES];
        // Cross every power-of-two subtree boundary from a partially used leaf.
        for power in 0..=14 {
            let boundary = 1024u64 << power;
            for start in [
                boundary - 1,
                boundary.saturating_sub(MAX_DIRECT_RANGE_BYTES) + 1,
            ] {
                if start + MAX_DIRECT_RANGE_BYTES <= MAX_BLOB_BYTES {
                    starts.push(start);
                }
            }
        }
        let mut maximum = 0;
        for start in starts {
            let mut extractor = ::bao::encode::SliceExtractor::new(
                Cursor::new(&encoded),
                start,
                MAX_DIRECT_RANGE_BYTES,
            );
            let mut slice = Vec::new();
            extractor.read_to_end(&mut slice).unwrap();
            assert!(
                slice.len() <= MAX_DIRECT_SLICE_BYTES,
                "start {start}: {}",
                slice.len()
            );
            maximum = maximum.max(slice.len());
        }
        assert_eq!(maximum, MAX_DIRECT_SLICE_BYTES);
    }
}
