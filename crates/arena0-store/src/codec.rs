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

pub(crate) fn envelope(kind: EnvelopeKind, payload: &[u8]) -> Result<Vec<u8>, StoreError> {
    let checksum = envelope_checksum(kind, payload);
    let envelope = DurableEnvelope {
        magic: ENVELOPE_MAGIC,
        kind: kind.tag(),
        version: ENVELOPE_VERSION,
        payload: payload.to_vec(),
        checksum,
    };
    borsh::to_vec(&envelope)
        .map_err(|error| StoreError::Corruption(format!("durable envelope encode: {error}")))
}

pub(crate) fn open_envelope(
    kind: EnvelopeKind,
    encoded: &[u8],
    max_payload: usize,
) -> Result<Vec<u8>, StoreError> {
    let max_encoded = max_payload
        .checked_add(MAX_ENVELOPE_OVERHEAD)
        .ok_or_else(|| StoreError::Corruption("durable envelope size bound overflow".into()))?;
    if encoded.len() > max_encoded {
        return Err(StoreError::Corruption(
            "durable envelope exceeds bound".into(),
        ));
    }
    let envelope: DurableEnvelope = borsh::from_slice(encoded)
        .map_err(|error| StoreError::Corruption(format!("durable envelope decode: {error}")))?;
    if envelope.magic != ENVELOPE_MAGIC
        || envelope.kind != kind.tag()
        || envelope.version != ENVELOPE_VERSION
    {
        return Err(StoreError::Corruption(
            "durable envelope header is invalid".into(),
        ));
    }
    if envelope.payload.len() > max_payload {
        return Err(StoreError::Corruption(
            "durable envelope payload exceeds bound".into(),
        ));
    }
    if envelope.checksum != envelope_checksum(kind, &envelope.payload) {
        return Err(StoreError::Corruption(
            "durable envelope checksum mismatch".into(),
        ));
    }
    Ok(envelope.payload)
}

pub(crate) fn envelope_checksum(kind: EnvelopeKind, payload: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(ENVELOPE_DOMAIN);
    hasher.update(&kind.tag().to_le_bytes());
    hasher.update(&ENVELOPE_VERSION.to_le_bytes());
    hasher.update(payload);
    *hasher.finalize().as_bytes()
}

pub(crate) fn decode_borsh<T: BorshDeserialize>(
    bytes: &[u8],
    field: &str,
) -> Result<T, StoreError> {
    borsh::from_slice(bytes)
        .map_err(|error| StoreError::Corruption(format!("{field} decode: {error}")))
}

pub(crate) fn activation_bytes(activation: &Activation) -> Result<Vec<u8>, StoreError> {
    borsh::to_vec(activation)
        .map_err(|error| StoreError::Corruption(format!("activation encode: {error}")))
}

pub(crate) fn prepared_activation_bytes(
    activation: &PreparedActivation,
) -> Result<Vec<u8>, StoreError> {
    borsh::to_vec(activation)
        .map_err(|error| StoreError::Corruption(format!("prepared activation encode: {error}")))
}

pub(crate) fn state_bytes(state: &ExecutionState) -> Result<Vec<u8>, StoreError> {
    state.encode().map_err(StoreError::Protocol)
}

pub(crate) fn event_bytes(event: &Event<Vec<u8>>) -> Result<Vec<u8>, StoreError> {
    borsh::to_vec(event).map_err(|error| StoreError::Corruption(format!("event encode: {error}")))
}

pub(crate) fn effects_bytes(effects: &[Effect]) -> Result<Vec<u8>, StoreError> {
    borsh::to_vec(effects)
        .map_err(|error| StoreError::Corruption(format!("effects encode: {error}")))
}

pub(crate) fn decode_execution_salt(encoded: &[u8]) -> Result<ExecutionSalt, StoreError> {
    let payload = Zeroizing::new(open_envelope(EnvelopeKind::ExecutionSalt, encoded, 32)?);
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

pub(crate) fn prepared_activation_record(
    execution_id: ExecId,
    prepared: PreparedActivation,
) -> ActivationRecord {
    ActivationRecord {
        execution_id,
        state: ActivationRecordState::Prepared { evidence: prepared },
    }
}

pub(crate) fn committed_activation_record(
    execution_id: ExecId,
    activation: Activation,
) -> Result<ActivationRecord, StoreError> {
    let prepared = activation.prepared().clone();
    Ok(ActivationRecord {
        execution_id,
        state: ActivationRecordState::Committed {
            evidence: prepared,
            activation: Box::new(activation),
        },
    })
}

pub(crate) fn parse_activation_status(value: &str) -> Result<ActivationRecordStatus, StoreError> {
    match value {
        "prepared" => Ok(ActivationRecordStatus::Prepared),
        "committed" => Ok(ActivationRecordStatus::Committed),
        other => Err(StoreError::Corruption(format!(
            "unknown activation status {other}"
        ))),
    }
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
