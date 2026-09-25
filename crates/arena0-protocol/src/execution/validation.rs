use arena0_crypto::BlsPublicKey;

use crate::trace::{
    AggregateAttestation, ReceiptTermination, StepCommitment, StepEvent, StepTerminal,
    TRACE_FORMAT_VERSION, TraceEntry,
};
use crate::{Effect, Ensemble, StateHash};

use super::{
    AbortKind, ExecutionBinding, MAX_EFFECTS, MAX_RECEIPT_BYTES, MAX_TERMINAL_OUTCOME_BYTES,
    MAX_TERMINAL_REASON_BYTES, MAX_TIMER_PAYLOAD_BYTES, MAX_TRACE_ENTRY_BYTES, ProtocolError,
    ReceiptBody, SharedProposal, StepCursor, StopCause,
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
    let expected_commitment =
        StepCommitment::for_entry(binding.session_id(), &proposal.entry, agreed.chain_hash());
    if proposal.commitment != expected_commitment
        || proposal.commitment.post_state != StateHash::of_shared(&proposal.shared_state)
    {
        return Err(ProtocolError::InvalidCertificate(
            "pending proposal commitment is inconsistent".into(),
        ));
    }
    let advanced = agreed.advance(&proposal.commitment)?;

    let lifecycle = proposal
        .effects
        .iter()
        .filter_map(|(_, effect)| is_lifecycle_effect(effect).then_some(effect))
        .collect::<Vec<_>>();
    if lifecycle.len() > 1 {
        return Err(ProtocolError::MultipleTerminalEffects);
    }
    let lifecycle_terminal = lifecycle
        .first()
        .and_then(|effect| StepTerminal::from_effect(effect));
    if lifecycle_terminal != proposal.entry.terminal {
        return Err(ProtocolError::InvalidCertificate(
            "proposal effects and trace terminal do not match".into(),
        ));
    }
    proposal.status.validate_binding(binding, advanced)?;
    validate_proposal_status(&proposal.entry, &proposal.commitment, &proposal.status)?;

    let participants = binding.participant_keys()?;
    if proposal.signatures.len() > participants.len() {
        return Err(ProtocolError::CollectionTooLarge {
            kind: "step signatures",
            actual: proposal.signatures.len(),
            max: participants.len(),
        });
    }
    for signature in &proposal.signatures {
        let key = binding.participant_key(&signature.participant())?;
        if signature.signature().step != proposal.commitment.step {
            return Err(ProtocolError::InvalidStepSignature {
                participant: signature.participant(),
                step: proposal.commitment.step,
            });
        }
        let valid = key
            .verify(
                &proposal.commitment.signing_bytes(),
                &signature.signature().sig,
            )
            .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
        if !valid {
            return Err(ProtocolError::InvalidStepSignature {
                participant: signature.participant(),
                step: proposal.commitment.step,
            });
        }
    }
    validate_step_signature_order(&proposal.signatures)
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
    signatures: &[super::ParticipantStepSignature],
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

/// Validate receipt identity, activation binding, ordered trace, and terminal proof.
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
    let participants = binding.participant_keys()?;
    let participant_keys = participants.iter().map(|(_, key)| *key).collect::<Vec<_>>();
    match &header.terminal {
        ReceiptTermination::Completed => {
            validate_completed_receipt(binding, body, &participant_keys, participants.len())
        }
        ReceiptTermination::Stopped { cause } => {
            validate_stopped_receipt(binding, body, cause, &participant_keys, participants.len())
        }
    }
}

fn validate_completed_receipt(
    binding: &ExecutionBinding,
    body: &ReceiptBody,
    participant_keys: &[BlsPublicKey],
    participant_count: usize,
) -> Result<(), ProtocolError> {
    validate_receipt_trace(
        binding,
        body.trace(),
        participant_keys,
        participant_count,
        ReceiptTraceTerminal::Completed {
            outcome: body.outcome(),
        },
    )
    .map(|_| ())
}

fn validate_stopped_receipt(
    binding: &ExecutionBinding,
    body: &ReceiptBody,
    cause: &StopCause,
    participant_keys: &[BlsPublicKey],
    participant_count: usize,
) -> Result<(), ProtocolError> {
    cause.validate()?;
    match cause {
        StopCause::Authenticated(occurrence) => {
            occurrence.validate_for_session(binding.session_id())?;
            if !binding
                .participant_keys()?
                .into_iter()
                .any(|(participant, _)| participant == occurrence.sender())
                || !occurrence.verify_signature()?
            {
                return Err(ProtocolError::UnauthenticatedAbort);
            }
            let cursor = validate_receipt_trace(
                binding,
                body.trace(),
                participant_keys,
                participant_count,
                ReceiptTraceTerminal::Authenticated,
            )?;
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
                participant_keys,
                participant_count,
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
    participant_keys: &[BlsPublicKey],
    participant_count: usize,
    terminal: ReceiptTraceTerminal<'_>,
) -> Result<StepCursor, ProtocolError> {
    if matches!(
        &terminal,
        ReceiptTraceTerminal::Completed { .. } | ReceiptTraceTerminal::Stopped { .. }
    ) && trace.is_empty()
    {
        return Err(ProtocolError::ReceiptBodyMismatch);
    }
    let mut previous_state = binding.activation.offer().data().initial_state;
    let mut previous_link = crate::CHAIN_START;
    for (index, entry) in trace.iter().enumerate() {
        let step = u64::try_from(index).map_err(|_| ProtocolError::ReceiptBodyMismatch)?;
        validate_shared_entry(binding, step, entry)?;
        if entry.step != step || entry.pre_state != previous_state {
            return Err(ProtocolError::ReceiptBodyMismatch);
        }
        if entry.agreement.signers.count() != participant_count
            || !entry.agreement.signers.is_full(participant_count)
        {
            return Err(ProtocolError::IncompleteProof {
                actual: entry.agreement.signers.count(),
                expected: participant_count,
            });
        }
        let commitment = StepCommitment::for_entry(binding.session_id(), entry, previous_link);
        entry
            .agreement
            .verify_signatures(step, &commitment.signing_bytes(), participant_keys)
            .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
        if index + 1 < trace.len() && terminal_effect_count(entry) != 0 {
            return Err(ProtocolError::TerminalTraceMismatch);
        }
        previous_state = entry.post_state;
        previous_link = commitment.link_hash();
    }

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
    Ok(StepCursor::new(
        u64::try_from(trace.len()).map_err(|_| ProtocolError::ReceiptBodyMismatch)?,
        previous_state,
        previous_link,
    ))
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
            let expected = Ensemble::from_peers(
                binding
                    .participant_keys()?
                    .into_iter()
                    .map(|(peer, _)| peer)
                    .collect(),
            )
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
/// The sandbox calls this at emission and recovery validation calls it on
/// stored proposals, so the two boundaries cannot drift.
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
        let len = borsh::object_length(effect)
            .map_err(|error| ProtocolError::Serialization(error.to_string()))?;
        total = total
            .checked_add(len)
            .ok_or(ProtocolError::EncodedTooLarge {
                kind: "effects",
                actual: usize::MAX,
                max: arena0_program::MAX_EFFECT_BYTES as usize,
            })?;
    }
    ensure_encoded("effects", total, arena0_program::MAX_EFFECT_BYTES as usize)
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
pub fn validate_effect_payload(effect: &Effect) -> Result<(), ProtocolError> {
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

fn is_lifecycle_effect(effect: &Effect) -> bool {
    matches!(
        effect,
        Effect::SessionEnd { .. } | Effect::SessionAbort { .. } | Effect::Fail { .. }
    )
}

pub(crate) fn terminal_effect_count(entry: &TraceEntry) -> usize {
    usize::from(entry.terminal.is_some())
}
