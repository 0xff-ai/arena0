use super::*;
use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy)]
#[repr(u16)]
pub(crate) enum EnvelopeKind {
    Activation = 1,
    PreparedActivation = 2,
    ExecutionState = 3,
    EventRecord = 4,
    AgreedStep = 5,
    Effects = 6,
    Receipt = 8,
    Timer = 10,
    ExecutionSalt = 11,
    Program = 12,
    ExecutionAdmission = 13,
    StepState = 14,
}

impl EnvelopeKind {
    const fn tag(self) -> u16 {
        self as u16
    }
}

#[derive(BorshSerialize, BorshDeserialize)]
pub(crate) struct DurableEnvelope {
    magic: [u8; 8],
    kind: u16,
    version: u16,
    payload: Vec<u8>,
    checksum: [u8; 32],
}

pub(crate) fn decode_borsh<T: BorshDeserialize>(
    bytes: &[u8],
    field: &str,
) -> Result<T, StoreError> {
    borsh::from_slice(bytes)
        .map_err(|error| StoreError::Corruption(format!("{field} decode: {error}")))
}

pub(crate) fn state_bytes(state: &ExecutionState) -> Result<Vec<u8>, StoreError> {
    state.encode().map_err(StoreError::Protocol)
}

pub(crate) fn decode_execution_salt(encoded: &[u8]) -> Result<ExecutionSalt, StoreError> {
    let payload = Zeroizing::new(DurableEnvelope::open(
        EnvelopeKind::ExecutionSalt,
        encoded,
        32,
    )?);
    let bytes: [u8; 32] = payload.as_slice().try_into().map_err(|_| {
        StoreError::Corruption("execution salt must contain exactly 32 bytes".into())
    })?;
    ExecutionSalt::try_from_bytes(bytes).map_err(|error| {
        StoreError::Corruption(format!("execution salt validation failed: {error}"))
    })
}

pub(crate) fn max_program_bytes() -> Result<usize, StoreError> {
    usize::try_from(arena0_program::PROGRAM_MAX_LEN)
        .map_err(|_| StoreError::InvalidConfiguration("program size bound does not fit usize"))
}

pub(crate) fn lifecycle_tag(lifecycle: ExecLifecycle) -> i64 {
    match lifecycle {
        ExecLifecycle::Negotiating => 0,
        ExecLifecycle::Activating => 1,
        ExecLifecycle::Waiting => 2,
        ExecLifecycle::Active => 3,
        ExecLifecycle::Completed => 4,
        ExecLifecycle::Aborted => 5,
        ExecLifecycle::Failed => 7,
    }
}

pub(crate) fn lifecycle_from_tag(tag: i64) -> Result<ExecLifecycle, StoreError> {
    match tag {
        0 => Ok(ExecLifecycle::Negotiating),
        1 => Ok(ExecLifecycle::Activating),
        2 => Ok(ExecLifecycle::Waiting),
        3 => Ok(ExecLifecycle::Active),
        4 => Ok(ExecLifecycle::Completed),
        5 => Ok(ExecLifecycle::Aborted),
        7 => Ok(ExecLifecycle::Failed),
        _ => Err(StoreError::Corruption(format!(
            "unknown lifecycle tag {tag}"
        ))),
    }
}

pub(crate) fn bounded_reason(reason: String) -> Result<String, StoreError> {
    if reason.len() > MAX_ERROR_BYTES {
        return Err(StoreError::PayloadTooLarge {
            required: reason.len(),
            capacity: MAX_ERROR_BYTES,
        });
    }
    Ok(reason)
}

pub(crate) fn account_response(current: &mut usize, additional: usize) -> Result<(), StoreError> {
    let next = current
        .checked_add(additional)
        .ok_or(StoreError::PayloadTooLarge {
            required: usize::MAX,
            capacity: MAX_RESPONSE_BYTES,
        })?;
    if next > MAX_RESPONSE_BYTES {
        return Err(StoreError::PayloadTooLarge {
            required: next,
            capacity: MAX_RESPONSE_BYTES,
        });
    }
    *current = next;
    Ok(())
}

pub(crate) fn sqlite_u64(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| {
        StoreError::Corruption("u64 value cannot be represented in SQLite INTEGER".into())
    })
}

pub(crate) fn sqlite_i64(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| {
        StoreError::Corruption("negative SQLite INTEGER where u64 was required".into())
    })
}

pub(crate) fn array32(bytes: &[u8], field: &str) -> Result<[u8; 32], StoreError> {
    bytes.try_into().map_err(|_| {
        StoreError::Corruption(format!("{field} is {} bytes, expected 32", bytes.len()))
    })
}

pub(crate) fn peer_id_from_blob(bytes: &[u8], field: &str) -> Result<PeerId, StoreError> {
    Ok(PeerId(array32(bytes, field)?))
}
/// Narrow end-phase projection used for routing without loading state images.
pub(crate) fn end_columns(end: &arena0_protocol::EndPhase) -> Result<(i64, Vec<u8>), StoreError> {
    use arena0_protocol::EndPhase;
    let empty = std::collections::BTreeSet::<PeerId>::new();
    let (tag, peers) = match end {
        EndPhase::Open => (0, &empty),
        EndPhase::Ending { unconfirmed } => (1, unconfirmed),
        EndPhase::Ended { unconfirmed } => (2, unconfirmed),
    };
    Ok((
        tag,
        borsh::to_vec(peers).map_err(|e| StoreError::Corruption(e.to_string()))?,
    ))
}

pub(crate) fn decode_end(tag: i64, bytes: &[u8]) -> Result<arena0_protocol::EndPhase, StoreError> {
    use arena0_protocol::EndPhase;
    if bytes.len() > 4 + arena0_protocol::MAX_PARTICIPANTS * 32 {
        return Err(StoreError::Corruption(
            "end peer set exceeds participant bound".into(),
        ));
    }
    let unconfirmed: std::collections::BTreeSet<PeerId> =
        borsh::from_slice(bytes).map_err(|e| StoreError::Corruption(e.to_string()))?;
    match tag {
        0 if unconfirmed.is_empty() => Ok(EndPhase::Open),
        1 if !unconfirmed.is_empty() => Ok(EndPhase::Ending { unconfirmed }),
        2 => Ok(EndPhase::Ended { unconfirmed }),
        _ => Err(StoreError::Corruption(
            "invalid end phase projection".into(),
        )),
    }
}

/// Retain the stored field name in corruption diagnostics; protocol-validated state encoding uses its own boundary.
pub(crate) fn encode_borsh<T: BorshSerialize + ?Sized>(
    value: &T,
    field: &str,
) -> Result<Vec<u8>, StoreError> {
    let _ = (value, field);
    todo!("STUB(store)")
}

impl DurableEnvelope {
    /// Encode the current store envelope without changing its domain, kind, version, or checksum preimage.
    pub(crate) fn seal(kind: EnvelopeKind, payload: &[u8]) -> Result<Vec<u8>, StoreError> {
        let _ = (kind, payload);
        todo!("STUB(store)")
    }

    /// Reject oversized envelopes before decoding, then authenticate the header, payload bound, and checksum.
    pub(crate) fn open(
        kind: EnvelopeKind,
        encoded: &[u8],
        max_payload: usize,
    ) -> Result<Vec<u8>, StoreError> {
        let _ = (kind, encoded, max_payload);
        todo!("STUB(store)")
    }

    /// Commit the envelope domain, kind, version, and exact payload bytes in their existing order.
    fn checksum(kind: EnvelopeKind, payload: &[u8]) -> [u8; 32] {
        let _ = (kind, payload);
        todo!("STUB(store)")
    }
}
