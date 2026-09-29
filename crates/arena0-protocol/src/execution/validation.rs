use arena0_crypto::BlsPublicKey;

use crate::trace::{
    AggregateAttestation, ReceiptTermination, StepCommitment, StepEvent, StepTerminal,
    TRACE_FORMAT_VERSION, TraceEntry,
};
use crate::{Effect, StateHash};

use super::{
    AbortKind, ExecutionBinding, MAX_EFFECTS, MAX_RECEIPT_BYTES, MAX_TERMINAL_OUTCOME_BYTES,
    MAX_TERMINAL_REASON_BYTES, MAX_TIMER_PAYLOAD_BYTES, MAX_TRACE_ENTRY_BYTES,
    ParticipantStepSignature, ProtocolError, ReceiptBody, SharedProposal, StepCursor, StopCause,
};

/// Validate an encoded value's total size without decoding it first.
pub(crate) fn ensure_encoded(
    kind: &'static str,
    actual: usize,
    max: usize,
) -> Result<(), ProtocolError> {
    if actual > max {
        return Err(ProtocolError::EncodedTooLarge { kind, actual, max });
    }
    Ok(())
}

/// Validate one bounded opaque payload.
pub(crate) fn ensure_payload(
    kind: &'static str,
    actual: usize,
    max: usize,
) -> Result<(), ProtocolError> {
    if actual > max {
        return Err(ProtocolError::PayloadTooLarge { kind, actual, max });
    }
    Ok(())
}

/// Validate all evidence retained by a pending shared proposal.
pub(crate) fn validate_proposal(
    binding: &ExecutionBinding,
    agreed: StepCursor,
    event_position: u64,
    proposal: &SharedProposal,
) -> Result<(), ProtocolError> {
    validate_shared_entry(binding, agreed.next_step(), &proposal.entry)?;
    if proposal.event_position != event_position {
        return Err(ProtocolError::InvalidCertificate(
            "pending proposal event position is not the expected event".into(),
        ));
    }
    if proposal.entry.agreement != AggregateAttestation::empty() {
        return Err(ProtocolError::InvalidCertificate(
            "pending proposal carries an agreement".into(),
        ));
    }
    // The commitment derives from this same entry, so the staged pre-state
    // is pinned to the agreed cursor explicitly here; without the former
    // stored-commitment comparison this mutation would otherwise reach
    // `advance` as a different error variant.
    if proposal.entry.pre_state != agreed.state_hash() {
        return Err(ProtocolError::InvalidCertificate(
            "pending proposal pre-state is not the agreed state".into(),
        ));
    }
    let expected_commitment =
        StepCommitment::for_entry(binding.session_id(), &proposal.entry, agreed.chain_hash());
    // The commitment is derived, never stored: the only consistency check
    // is that the proposed bytes hash to the entry's post-state.
    if expected_commitment.post_state != StateHash::of_shared(&proposal.shared_state) {
        return Err(ProtocolError::InvalidCertificate(
            "pending proposal commitment is inconsistent".into(),
        ));
    }
    let advanced = agreed.advance(&expected_commitment)?;

    let lifecycle_terminal =
        single_lifecycle_effect(proposal.effects.iter().map(|(_, effect)| effect))?
            .and_then(StepTerminal::from_effect);
    if lifecycle_terminal != proposal.entry.terminal {
        return Err(ProtocolError::InvalidCertificate(
            "proposal effects and trace terminal do not match".into(),
        ));
    }
    proposal.status.validate_binding(binding, advanced)?;
    validate_proposal_status(&proposal.entry, &expected_commitment, &proposal.status)?;

    let participants = binding.participant_keys()?;
    if proposal.signatures.len() > participants.len() {
        return Err(ProtocolError::CollectionTooLarge {
            kind: "step signatures",
            actual: proposal.signatures.len(),
            max: participants.len(),
        });
    }
    for signature in &proposal.signatures {
        let key = participants
            .binary_search_by_key(&signature.participant(), |(participant, _)| *participant)
            .map(|index| participants[index].1)
            .map_err(|_| ProtocolError::UnknownParticipant {
                participant: signature.participant(),
            })?;
        verify_step_signature(&key, &expected_commitment, signature)?;
    }
    validate_step_signature_order(&proposal.signatures)
}

/// Verify one participant's BLS signature over an exact step commitment.
pub(crate) fn verify_step_signature(
    key: &BlsPublicKey,
    commitment: &StepCommitment,
    signature: &ParticipantStepSignature,
) -> Result<(), ProtocolError> {
    let invalid = || ProtocolError::InvalidStepSignature {
        participant: signature.participant(),
        step: commitment.step,
    };
    if signature.signature().step != commitment.step {
        return Err(invalid());
    }
    let valid = key
        .verify(&commitment.signing_bytes(), &signature.signature().sig)
        .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
    if !valid {
        return Err(invalid());
    }
    Ok(())
}

/// Verify an N-of-N aggregate agreement over an exact step commitment.
///
/// `keys` are the committed participants' execution keys in participant
/// order, the order the signer bitmap indexes.
pub(crate) fn verify_full_agreement(
    agreement: &AggregateAttestation,
    commitment: &StepCommitment,
    keys: &[BlsPublicKey],
) -> Result<(), ProtocolError> {
    if agreement.signers.count() != keys.len() || !agreement.signers.is_full(keys.len()) {
        return Err(ProtocolError::IncompleteProof {
            actual: agreement.signers.count(),
            expected: keys.len(),
        });
    }
    agreement
        .verify_signatures(commitment.step, &commitment.signing_bytes(), keys)
        .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))
}

fn validate_proposal_status(
    entry: &TraceEntry,
    commitment: &StepCommitment,
    status: &super::ExecutionStatus,
) -> Result<(), ProtocolError> {
    match (&entry.terminal, status) {
        (None, super::ExecutionStatus::Active) => Ok(()),
        (None, _) => Err(ProtocolError::InvalidTerminalStatus),
        (
            Some(StepTerminal::Abort { reason }),
            super::ExecutionStatus::Stopped {
                cause:
                    super::StopCause::Shared {
                        kind: AbortKind::Abort,
                        commitment: actual_commitment,
                        reason: actual_reason,
                    },
            },
        ) if actual_commitment == commitment && actual_reason == reason => Ok(()),
        (
            Some(StepTerminal::Fail { reason }),
            super::ExecutionStatus::Stopped {
                cause:
                    super::StopCause::Shared {
                        kind: AbortKind::Fail,
                        commitment: actual_commitment,
                        reason: actual_reason,
                    },
            },
        ) if actual_commitment == commitment && actual_reason == reason => Ok(()),
        (
            Some(StepTerminal::End { outcome }),
            super::ExecutionStatus::Certified { outcome: actual },
        ) => {
            actual.validate()?;
            if actual.borsh() == outcome.as_slice() {
                Ok(())
            } else {
                Err(ProtocolError::TerminalOutcomeMismatch)
            }
        }
        (Some(StepTerminal::End { .. }), _) => Err(ProtocolError::TerminalOutcomeRequired),
        (Some(StepTerminal::Abort { .. } | StepTerminal::Fail { .. }), _) => {
            Err(ProtocolError::InvalidTerminalStatus)
        }
    }
}

fn validate_step_signature_order(
    signatures: &[ParticipantStepSignature],
) -> Result<(), ProtocolError> {
    for pair in signatures.windows(2) {
        if pair[0].participant() >= pair[1].participant() {
            if pair[0].participant() == pair[1].participant() {
                return Err(if pair[0].signature() == pair[1].signature() {
                    ProtocolError::DuplicateStepSignature {
                        participant: pair[0].participant(),
                    }
                } else {
                    ProtocolError::ConflictingStepSignature {
                        participant: pair[0].participant(),
                    }
                });
            }
            return Err(ProtocolError::InvalidCertificate(
                "pending step signatures are not canonical".into(),
            ));
        }
    }
    Ok(())
}

/// Validate receipt structure independently of its binding.
pub(crate) fn validate_receipt_body_shape(body: &ReceiptBody) -> Result<(), ProtocolError> {
    let encoded =
        borsh::to_vec(body).map_err(|error| ProtocolError::Serialization(error.to_string()))?;
    ensure_encoded("receipt body", encoded.len(), MAX_RECEIPT_BYTES)?;
    if body.trace().len() > super::MAX_RECEIPT_TRACE_ENTRIES {
        return Err(ProtocolError::CollectionTooLarge {
            kind: "receipt trace entries",
            actual: body.trace().len(),
            max: super::MAX_RECEIPT_TRACE_ENTRIES,
        });
    }
    for entry in body.trace() {
        validate_trace_entry(entry)?;
    }
    match body.termination() {
        ReceiptTermination::Completed => {}
        ReceiptTermination::Stopped { cause } => {
            cause.validate()?;
            if !body.outcome().is_empty() {
                return Err(ProtocolError::OutcomeProjectionMismatch);
            }
        }
    }
    Ok(())
}

/// Validate receipt identity, activation binding, ordered trace, and terminal evidence.
pub(crate) fn validate_receipt_body(
    binding: &ExecutionBinding,
    body: &ReceiptBody,
) -> Result<(), ProtocolError> {
    validate_receipt_body_shape(body)?;
    let header = body.header();
    header
        .activation
        .validate()
        .map_err(ProtocolError::InvalidActivation)?;
    if header.activation != *binding.activation()
        || header.session_hash() != binding.session_id()
        || header.program_hash() != binding.program_hash()
        || body.params() != binding.activation.offer().data().params.as_bytes()
    {
        return Err(ProtocolError::ReceiptBodyMismatch);
    }
    match &header.terminal {
        ReceiptTermination::Completed => validate_receipt_trace(
            binding,
            body.trace(),
            ReceiptTraceTerminal::Completed {
                outcome: body.outcome(),
            },
        )
        .map(|_| ()),
        ReceiptTermination::Stopped { cause } => validate_stopped_receipt(binding, body, cause),
    }
}

fn validate_stopped_receipt(
    binding: &ExecutionBinding,
    body: &ReceiptBody,
    cause: &StopCause,
) -> Result<(), ProtocolError> {
    cause.validate()?;
    match cause {
        StopCause::Authenticated(occurrence) => {
            occurrence.validate_for_session(binding.session_id())?;
            if !binding.is_participant(occurrence.sender()) || !occurrence.verify_signature()? {
                return Err(ProtocolError::UnauthenticatedAbort);
            }
            let cursor =
                validate_receipt_trace(binding, body.trace(), ReceiptTraceTerminal::Authenticated)?;
            if *occurrence.coordinate() != cursor {
                return Err(ProtocolError::InvalidAbortCoordinate);
            }
            Ok(())
        }
        StopCause::Shared {
            kind,
            commitment,
            reason,
        } => {
            let Some(entry) = body.trace().last() else {
                return Err(ProtocolError::ReceiptBodyMismatch);
            };
            if entry.step != commitment.step || entry.entry_hash() != commitment.entry_hash {
                return Err(ProtocolError::ReceiptBodyMismatch);
            }
            let cursor = validate_receipt_trace(
                binding,
                body.trace(),
                ReceiptTraceTerminal::Stopped {
                    kind: *kind,
                    reason,
                },
            )?;
            super::status::validate_cause_binding(cause, binding, cursor)
        }
    }
}

enum ReceiptTraceTerminal<'a> {
    Completed { outcome: &'a [u8] },
    Authenticated,
    Stopped { kind: AbortKind, reason: &'a str },
}

fn validate_receipt_trace(
    binding: &ExecutionBinding,
    trace: &[TraceEntry],
    terminal: ReceiptTraceTerminal<'_>,
) -> Result<StepCursor, ProtocolError> {
    if matches!(
        &terminal,
        ReceiptTraceTerminal::Completed { .. } | ReceiptTraceTerminal::Stopped { .. }
    ) && trace.is_empty()
    {
        return Err(ProtocolError::ReceiptBodyMismatch);
    }
    let cursor = validate_agreed_trace(binding, trace)?;

    let final_entry = trace.last();
    match terminal {
        ReceiptTraceTerminal::Completed { outcome } => {
            let Some(entry) = final_entry else {
                return Err(ProtocolError::ReceiptBodyMismatch);
            };
            let completed = entry
                .terminal
                .as_ref()
                .and_then(StepTerminal::completed_outcome);
            if !matches!(entry.terminal, Some(StepTerminal::End { .. }))
                || completed != Some(outcome)
            {
                return Err(ProtocolError::TerminalTraceMismatch);
            }
        }
        ReceiptTraceTerminal::Authenticated => {
            if final_entry.is_some_and(|entry| terminal_effect_count(entry) != 0) {
                return Err(ProtocolError::TerminalTraceMismatch);
            }
        }
        ReceiptTraceTerminal::Stopped { kind, reason } => {
            let Some(entry) = final_entry else {
                return Err(ProtocolError::ReceiptBodyMismatch);
            };
            let valid = match (kind, entry.terminal.as_ref()) {
                (AbortKind::Abort, Some(StepTerminal::Abort { reason: actual }))
                | (AbortKind::Fail, Some(StepTerminal::Fail { reason: actual })) => {
                    actual == reason
                }
                _ => false,
            };
            if !valid {
                return Err(ProtocolError::TerminalTraceMismatch);
            }
        }
    }
    Ok(cursor)
}

/// Validate a certified trace prefix from the activation's genesis state and
/// return the agreed cursor after its last entry.
///
/// Every entry must be well formed at its position, extend the hash chain from
/// the previous entry's post-state, and carry an N-of-N aggregate agreement
/// over its derived [`StepCommitment`]. Only the final entry may carry a
/// terminal. The receipt verifier and the store's reopen check share this path.
pub fn validate_agreed_trace(
    binding: &ExecutionBinding,
    trace: &[TraceEntry],
) -> Result<StepCursor, ProtocolError> {
    let keys = binding.participant_bls_keys()?;
    let mut cursor = StepCursor::new(
        0,
        binding.activation.offer().data().initial_state,
        crate::CHAIN_START,
    );
    for (index, entry) in trace.iter().enumerate() {
        validate_shared_entry(binding, cursor.next_step(), entry)?;
        let commitment =
            StepCommitment::for_entry(binding.session_id(), entry, cursor.chain_hash());
        let next = cursor
            .advance(&commitment)
            .map_err(|_| ProtocolError::ReceiptBodyMismatch)?;
        verify_full_agreement(&entry.agreement, &commitment, &keys)?;
        if index + 1 < trace.len() && terminal_effect_count(entry) != 0 {
            return Err(ProtocolError::TerminalTraceMismatch);
        }
        cursor = next;
    }
    Ok(cursor)
}

pub(crate) fn validate_trace_entry(entry: &TraceEntry) -> Result<(), ProtocolError> {
    if entry.trace_version != TRACE_FORMAT_VERSION {
        return Err(ProtocolError::TraceVersionMismatch {
            actual: entry.trace_version,
            expected: TRACE_FORMAT_VERSION,
        });
    }
    let encoded =
        borsh::to_vec(entry).map_err(|error| ProtocolError::Serialization(error.to_string()))?;
    ensure_encoded("trace entry", encoded.len(), MAX_TRACE_ENTRY_BYTES).map_err(
        |error| match error {
            ProtocolError::EncodedTooLarge { actual, max, .. } => {
                ProtocolError::TraceEntryTooLarge { actual, max }
            }
            other => other,
        },
    )?;
    match &entry.event {
        StepEvent::SessionStarted { .. } => {}
        StepEvent::Message { data, .. } => {
            ensure_payload(
                "message payload",
                data.len(),
                super::MAX_EFFECT_PAYLOAD_BYTES,
            )?;
        }
    }
    if let Some(terminal) = &entry.terminal {
        validate_terminal(terminal)?;
    }
    Ok(())
}

fn validate_terminal(terminal: &StepTerminal) -> Result<(), ProtocolError> {
    match terminal {
        StepTerminal::End { outcome } => ensure_payload(
            "terminal outcome",
            outcome.len(),
            MAX_TERMINAL_OUTCOME_BYTES,
        ),
        StepTerminal::Abort { reason } | StepTerminal::Fail { reason } => {
            ensure_payload("terminal reason", reason.len(), MAX_TERMINAL_REASON_BYTES)
        }
    }
}

pub(crate) fn validate_shared_entry(
    binding: &ExecutionBinding,
    expected_step: u64,
    entry: &TraceEntry,
) -> Result<(), ProtocolError> {
    validate_trace_entry(entry)?;
    if entry.step != expected_step {
        return Err(ProtocolError::StepCoordinateMismatch {
            expected: expected_step,
            actual: entry.step,
        });
    }
    match &entry.event {
        StepEvent::SessionStarted { ensemble } => {
            if expected_step != 0 {
                return Err(ProtocolError::SessionStartPosition);
            }
            let expected = binding
                .ensemble()
                .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
            if ensemble != &expected {
                return Err(ProtocolError::SessionStartMismatch);
            }
        }
        StepEvent::Message { from, .. } => {
            if expected_step == 0 {
                return Err(ProtocolError::MissingSessionStart);
            }
            binding.participant_key(from)?;
        }
    }
    Ok(())
}

/// Check every protocol limit one dispatch's effects must satisfy: the count
/// bound, each effect's payload bounds, and the exact canonical `Vec<Effect>`
/// encoding bound, including the vector's length prefix.
///
/// Three callers share this one owner: the sandbox at emission,
/// `ExecutionState::apply_dispatch` before staging or installing, and the
/// store when it reopens event records. Recovery reaches it through
/// `validate_effects`.
pub fn check_effect_budget<'a>(
    effects: impl IntoIterator<Item = &'a Effect>,
) -> Result<(), ProtocolError> {
    let mut total = std::mem::size_of::<u32>();
    let mut count = 0usize;
    for effect in effects {
        count += 1;
        if count > MAX_EFFECTS {
            return Err(ProtocolError::CollectionTooLarge {
                kind: "effects",
                actual: count,
                max: MAX_EFFECTS,
            });
        }
        validate_effect_payload(effect)?;
        total = add_effect_len(total, effect)?;
    }
    ensure_encoded("effects", total, arena0_program::MAX_EFFECT_BYTES as usize)
}

/// Accumulate one effect's canonical encoding length into an aggregate total.
fn add_effect_len(total: usize, effect: &Effect) -> Result<usize, ProtocolError> {
    let len = borsh::object_length(effect)
        .map_err(|error| ProtocolError::Serialization(error.to_string()))?;
    total
        .checked_add(len)
        .ok_or(ProtocolError::EncodedTooLarge {
            kind: "effects",
            actual: usize::MAX,
            max: arena0_program::MAX_EFFECT_BYTES as usize,
        })
}

pub(crate) fn validate_effects(effects: &[(u32, Effect)]) -> Result<(), ProtocolError> {
    for (ordinal, _) in effects {
        if *ordinal as usize >= MAX_EFFECTS {
            return Err(ProtocolError::InvalidCertificate(
                "effect ordinal is outside the dispatch range".into(),
            ));
        }
    }
    if effects.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return Err(ProtocolError::InvalidCertificate(
            "effect ordinals are not strictly increasing".into(),
        ));
    }
    check_effect_budget(effects.iter().map(|(_, effect)| effect))
}

/// Check one effect's payload against its protocol bound.
fn validate_effect_payload(effect: &Effect) -> Result<(), ProtocolError> {
    match effect {
        Effect::SessionEnd { outcome } => ensure_payload(
            "terminal outcome",
            outcome.len(),
            MAX_TERMINAL_OUTCOME_BYTES,
        ),
        Effect::SessionAbort { reason } | Effect::Fail { reason } => {
            ensure_payload("terminal reason", reason.len(), MAX_TERMINAL_REASON_BYTES)
        }
        Effect::Broadcast { data } => ensure_payload(
            "broadcast payload",
            data.len(),
            super::MAX_EFFECT_PAYLOAD_BYTES,
        ),
        Effect::SetTimer { timer, .. } => {
            ensure_payload(
                "timer type name",
                timer.type_name.len(),
                MAX_TERMINAL_REASON_BYTES,
            )?;
            ensure_payload("timer data", timer.data.len(), MAX_TIMER_PAYLOAD_BYTES)
        }
    }
}

/// Return the dispatch's lifecycle effect, rejecting more than one.
pub(crate) fn single_lifecycle_effect<'a>(
    effects: impl IntoIterator<Item = &'a Effect>,
) -> Result<Option<&'a Effect>, ProtocolError> {
    let mut lifecycle = effects.into_iter().filter(|effect| effect.is_lifecycle());
    let first = lifecycle.next();
    if lifecycle.next().is_some() {
        return Err(ProtocolError::MultipleTerminalEffects);
    }
    Ok(first)
}

pub(crate) fn terminal_effect_count(entry: &TraceEntry) -> usize {
    usize::from(entry.terminal.is_some())
}
