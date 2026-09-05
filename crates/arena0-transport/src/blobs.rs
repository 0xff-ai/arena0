//! Bounded content-addressed blob helpers for the local transport.
//!
//! [`LocalTransport`](crate::LocalTransport) owns the in-process blob store;
//! this module owns the shared size, hash-provider, and retention validation
//! rules. The helpers deliberately operate on raw bytes so the host never
//! interprets program-owned payloads.

use std::collections::HashSet;

use arena0_protocol::PeerId;

use crate::TransportError;

const MAX_PROVIDERS_PER_FETCH: usize = 2;

pub(crate) fn validate_declared_length(declared: u64, max: u64) -> Result<(), TransportError> {
    if declared > max {
        Err(TransportError::BlobInvalidLength { declared, max })
    } else {
        Ok(())
    }
}

pub(crate) fn validate_actual_size(size: u64, max: u64) -> Result<(), TransportError> {
    if size > max {
        Err(TransportError::BlobTooLarge { size, max })
    } else {
        Ok(())
    }
}

pub(crate) fn validate_stored_size(
    size: u64,
    declared: u64,
    max: u64,
) -> Result<(), TransportError> {
    validate_actual_size(size, max)?;
    if size != declared {
        Err(TransportError::BlobLengthMismatch {
            expected: declared,
            actual: size,
        })
    } else {
        Ok(())
    }
}

pub(crate) fn ordered_unique_providers(providers: &[PeerId]) -> Vec<PeerId> {
    let mut seen = HashSet::with_capacity(providers.len());
    providers
        .iter()
        .copied()
        .filter(|provider| seen.insert(*provider))
        .take(MAX_PROVIDERS_PER_FETCH)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_distinguish_declared_and_actual_size() {
        assert!(matches!(
            validate_declared_length(10, 9),
            Err(TransportError::BlobInvalidLength {
                declared: 10,
                max: 9,
            })
        ));
        assert!(validate_declared_length(9, 9).is_ok());
        assert!(matches!(
            validate_actual_size(10, 9),
            Err(TransportError::BlobTooLarge { size: 10, max: 9 })
        ));
        assert!(matches!(
            validate_stored_size(8, 9, 9),
            Err(TransportError::BlobLengthMismatch {
                expected: 9,
                actual: 8,
            })
        ));
    }

    #[test]
    fn providers_keep_order_and_remove_duplicates() {
        let providers = [
            PeerId([1; 32]),
            PeerId([2; 32]),
            PeerId([1; 32]),
            PeerId([3; 32]),
        ];
        assert_eq!(
            ordered_unique_providers(&providers),
            vec![PeerId([1; 32]), PeerId([2; 32])]
        );
    }
}
