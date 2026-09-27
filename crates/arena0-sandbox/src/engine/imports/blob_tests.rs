//! Exercise the blob imports with real Wasm calls and an in-memory view.

use super::*;
use arena0_protocol::BlobHash;
use arena0_protocol::execution::BlobPartial;
use std::ops::Range;

/// An in-memory blob view: granted blobs with their bytes, and partials with
/// their length, durable bytes `[0, written)`, and committed flag.
#[derive(Default)]
pub(crate) struct View {
    pub(crate) granted: Vec<(BlobHash, Vec<u8>)>,
    pub(crate) partials: Vec<(BlobHash, u64, Vec<u8>, bool)>,
}

impl crate::BlobView for View {
    fn granted(&self, hash: BlobHash) -> Result<Option<u64>, String> {
        Ok(self
            .granted
            .iter()
            .find(|(h, _)| *h == hash)
            .map(|(_, bytes)| bytes.len() as u64))
    }
    fn read(&self, hash: BlobHash, range: Range<u64>) -> Result<Option<Vec<u8>>, String> {
        Ok(self
            .granted
            .iter()
            .find(|(h, _)| *h == hash)
            .and_then(|(_, bytes)| {
                bytes
                    .get(range.start as usize..range.end as usize)
                    .map(<[u8]>::to_vec)
            }))
    }
    fn partial(&self, hash: BlobHash) -> Result<Option<BlobPartial>, String> {
        Ok(self.partials.iter().find(|(h, _, _, _)| *h == hash).map(
            |(_, length, bytes, committed)| BlobPartial {
                length: *length,
                written: bytes.len() as u64,
                committed: *committed,
            },
        ))
    }
    fn hash_partial(&self, hash: BlobHash, tail: &[u8]) -> Result<BlobHash, String> {
        let (_, _, bytes, _) = self
            .partials
            .iter()
            .find(|(h, _, _, _)| *h == hash)
            .expect("caller checked the partial");
        let content = [bytes.as_slice(), tail].concat();
        Ok(BlobHash(arena0_crypto::hash(
            arena0_crypto::HashAlgorithm::Blake3,
            &content,
        )))
    }
}
