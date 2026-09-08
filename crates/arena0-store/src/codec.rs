use super::*;
use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy)]
#[repr(u16)]
pub(crate) enum EnvelopeKind {
    Activation = 1,
    PreparedActivation = 2,
    ExecutionState = 3,
    ExecutionInput = 4,
    SharedCommit = 5,
    PrivateCommit = 6,
    TerminalPublication = 7,
    Receipt = 8,
    InboundFrame = 9,
    Effect = 10,
    Timer = 11,
    ExecutionSalt = 12,
    Program = 13,
    ExecutionAdmission = 14,
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

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredFrame {
    tag: u8,
    payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InboxStatus {
    Accepted,
    Applied,
    Consumed,
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

pub(crate) fn checksum(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

pub(crate) fn decode_borsh<T: BorshDeserialize>(
    bytes: &[u8],
    field: &str,
) -> Result<T, StoreError> {
    borsh::from_slice(bytes)
        .map_err(|error| StoreError::Corruption(format!("{field} decode: {error}")))
}

pub(crate) fn encoded_len<T: BorshSerialize>(value: &T, max: usize) -> Result<usize, StoreError> {
    let bytes = borsh::to_vec(value)
        .map_err(|error| StoreError::Corruption(format!("value encode: {error}")))?;
    if bytes.len() > max {
        return Err(StoreError::CommandTooLarge {
            required: bytes.len(),
            capacity: max,
        });
    }
    Ok(bytes.len())
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

pub(crate) fn input_bytes(input: &ExecutionInput) -> Result<Vec<u8>, StoreError> {
    input.encode().map_err(StoreError::Protocol)
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

pub(crate) fn frame_bytes(frame: &AuthenticatedFrame) -> Result<Vec<u8>, StoreError> {
    let stored = canonical_frame_shape(&frame.frame)?;
    borsh::to_vec(&stored)
        .map_err(|error| StoreError::Corruption(format!("inbox frame encode: {error}")))
}

pub(crate) fn inbox_identity_bytes(
    source: PeerId,
    stored: &StoredFrame,
) -> Result<Vec<u8>, StoreError> {
    let canonical = borsh::to_vec(stored)
        .map_err(|error| StoreError::Corruption(format!("inbox frame encode: {error}")))?;
    let mut bytes = Vec::with_capacity(16 + 32 + canonical.len());
    bytes.extend_from_slice(b"arena0/inbox/v1");
    bytes.extend_from_slice(&source.0);
    bytes.extend_from_slice(&canonical);
    Ok(bytes)
}

pub(crate) fn inbox_identity_digest(
    source: PeerId,
    stored: &StoredFrame,
) -> Result<[u8; 32], StoreError> {
    let identity = inbox_identity_bytes(source, stored)?;
    Ok(*blake3::hash(&identity).as_bytes())
}

pub(crate) fn occurrence_key_bytes(key: OccurrenceKey) -> Result<Vec<u8>, StoreError> {
    borsh::to_vec(&key)
        .map_err(|error| StoreError::Corruption(format!("occurrence key encode: {error}")))
}

pub(crate) fn decode_effect(encoded: &[u8]) -> Result<DurableEffect, StoreError> {
    let payload = open_envelope(
        EnvelopeKind::Effect,
        encoded,
        arena0_protocol::MAX_RECEIPT_BYTES,
    )?;
    decode_borsh(&payload, "outbox effect")
}

pub(crate) fn canonical_frame_shape(frame: &ExecFrame) -> Result<StoredFrame, StoreError> {
    let (tag, payload) = match frame {
        ExecFrame::Message {
            message_id,
            seq,
            prestate,
            data,
            witness,
        } => (
            0,
            borsh::to_vec(&(*message_id, *seq, *prestate, data, *witness)),
        ),
        ExecFrame::StepSignature {
            commitment,
            signature,
        } => (1, borsh::to_vec(&(commitment, signature))),
        ExecFrame::End {
            commitment,
            signature,
        } => (2, borsh::to_vec(&(commitment, signature))),
        ExecFrame::Abort { occurrence } => (3, borsh::to_vec(occurrence)),
    };
    let payload =
        payload.map_err(|error| StoreError::Corruption(format!("inbox frame encode: {error}")))?;
    let required = payload
        .len()
        .checked_add(5)
        .ok_or_else(|| StoreError::CommandTooLarge {
            required: usize::MAX,
            capacity: MAX_FRAME_BYTES,
        })?;
    if required > MAX_FRAME_BYTES {
        return Err(StoreError::CommandTooLarge {
            required,
            capacity: MAX_FRAME_BYTES,
        });
    }
    Ok(StoredFrame { tag, payload })
}

pub(crate) fn decode_stored_frame(stored: &StoredFrame) -> Result<ExecFrame, StoreError> {
    let frame = match stored.tag {
        0 => {
            let (message_id, seq, prestate, data, witness): (
                MessageId,
                u64,
                StateHash,
                Vec<u8>,
                WitnessCommitment,
            ) = decode_borsh(&stored.payload, "inbox message frame")?;
            ExecFrame::Message {
                message_id,
                seq,
                prestate,
                data,
                witness,
            }
        }
        1 => {
            let (commitment, signature): (StepCommitment, BlsSignature) =
                decode_borsh(&stored.payload, "inbox step signature")?;
            ExecFrame::StepSignature {
                commitment,
                signature,
            }
        }
        2 => {
            let (commitment, signature): (TerminalCommitment, BlsSignature) =
                decode_borsh(&stored.payload, "inbox terminal signature")?;
            ExecFrame::End {
                commitment,
                signature,
            }
        }
        3 => ExecFrame::Abort {
            occurrence: decode_borsh(&stored.payload, "inbox abort frame")?,
        },
        tag => {
            return Err(StoreError::Corruption(format!(
                "unknown stored inbox frame tag {tag}"
            )));
        }
    };
    if canonical_frame_shape(&frame)? != *stored {
        return Err(StoreError::Corruption(
            "stored inbox frame is not canonical".into(),
        ));
    }
    Ok(frame)
}

pub(crate) fn canonical_frame(
    frame: &AuthenticatedFrame,
    state: &ExecutionState,
) -> Result<StoredFrame, StoreError> {
    if !is_participant(state, frame.source) {
        return Err(StoreError::UnauthenticatedSource(format!(
            "{} is not an activation participant",
            frame.source
        )));
    }
    match &frame.frame {
        ExecFrame::Message {
            message_id,
            seq,
            prestate,
            data,
            witness,
        } => {
            let expected = MessageId::derive(
                state.binding().session_id(),
                frame.source,
                *seq,
                *prestate,
                data,
                *witness,
            );
            if expected != *message_id {
                return Err(StoreError::UnauthenticatedSource(
                    "message id does not authenticate its source".into(),
                ));
            }
        }
        ExecFrame::StepSignature { commitment, .. } => {
            if commitment.session_id != state.binding().session_id() {
                return Err(StoreError::UnauthenticatedSource(
                    "step signature names another session".into(),
                ));
            }
        }
        ExecFrame::End { commitment, .. } => {
            if commitment.session_id != state.binding().session_id() {
                return Err(StoreError::UnauthenticatedSource(
                    "terminal signature names another session".into(),
                ));
            }
        }
        ExecFrame::Abort { occurrence } => {
            if occurrence.sender() != frame.source
                || occurrence.session_id() != state.binding().session_id()
                || !occurrence.verify_signature()?
            {
                return Err(StoreError::UnauthenticatedSource(
                    "abort occurrence is not authenticated by its source".into(),
                ));
            }
        }
    }
    canonical_frame_shape(&frame.frame)
}

pub(crate) fn is_participant(state: &ExecutionState, peer: PeerId) -> bool {
    state
        .binding()
        .activation()
        .tickets()
        .iter()
        .any(|ticket| ticket.data.signer == peer)
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

pub(crate) fn parse_inbox_status(value: &str) -> Result<InboxStatus, StoreError> {
    match value {
        "accepted" => Ok(InboxStatus::Accepted),
        "applied" => Ok(InboxStatus::Applied),
        "consumed" => Ok(InboxStatus::Consumed),
        other => Err(StoreError::Corruption(format!(
            "unknown inbox status {other}"
        ))),
    }
}

pub(crate) fn parse_outbox_status(value: &str) -> Result<OutboxStatus, StoreError> {
    match value {
        "pending" => Ok(OutboxStatus::Pending),
        "leased" => Ok(OutboxStatus::Leased),
        "acknowledged" => Ok(OutboxStatus::Acknowledged),
        other => Err(StoreError::Corruption(format!(
            "unknown outbox status {other}"
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
        ExecLifecycle::Incomplete => 6,
        ExecLifecycle::Failed => 7,
    }
}

pub(crate) fn bounded_reason(reason: String) -> Result<String, StoreError> {
    if reason.len() > MAX_ERROR_BYTES {
        return Err(StoreError::CommandTooLarge {
            required: reason.len(),
            capacity: MAX_ERROR_BYTES,
        });
    }
    Ok(reason)
}

pub(crate) fn account_response(current: &mut usize, additional: usize) -> Result<(), StoreError> {
    let next = current
        .checked_add(additional)
        .ok_or(StoreError::CommandTooLarge {
            required: usize::MAX,
            capacity: MAX_RESPONSE_BYTES,
        })?;
    if next > MAX_RESPONSE_BYTES {
        return Err(StoreError::CommandTooLarge {
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

pub(crate) fn derive_lease_id(outbox_id: OutboxId, attempts: u32, now_ms: u64) -> LeaseId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arena0/store/outbox-lease/v1");
    hasher.update(outbox_id.as_bytes());
    hasher.update(&attempts.to_le_bytes());
    hasher.update(&now_ms.to_le_bytes());
    LeaseId::from_bytes(*hasher.finalize().as_bytes())
}

pub(crate) fn unix_time_ms() -> Result<u64, StoreError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            StoreError::Io(std::io::Error::other(format!(
                "system clock before Unix epoch: {error}"
            )))
        })?;
    u64::try_from(duration.as_millis())
        .map_err(|_| StoreError::Corruption("system clock millisecond value exceeds u64".into()))
}
