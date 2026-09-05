use crate::PrivateRecord;
use crate::trace::{
    AggregateAttestation, PendingRecord, ReceiptTermination, SessionTerminal, StepCommitment,
    TRACE_FORMAT_VERSION, TraceEntry,
};
use crate::{
    Ensemble, OutcomeHash, PeerId, PrivateEffect, PrivateEvent, PublicEffect, PublicEvent,
    StateHash,
};

use super::{
    AbortKind, ActiveTimer, ExecutionBinding, ExecutionState, MAX_ACTIVE_TIMERS,
    MAX_EFFECT_PAYLOAD_BYTES, MAX_PRIVATE_EFFECTS, MAX_PRIVATE_RECORD_BYTES, MAX_RECEIPT_BYTES,
    MAX_SHARED_EFFECTS, MAX_TERMINAL_OUTCOME_BYTES, MAX_TERMINAL_REASON_BYTES,
    MAX_TIMER_PAYLOAD_BYTES, MAX_TRACE_ENTRY_BYTES, ParticipantTerminalSignature, ProofId,
    ProtocolError, PublicCursor, ReceiptBody, ReceiptId, SharedProposal, StopCause,
    TerminalCertificate, TerminalOutcome, TerminalProof, TimerMutation,
};

/// Validate an input's total encoded size without decoding it first.
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

pub(crate) fn validate_proposal(
    binding: &ExecutionBinding,
    public: PublicCursor,
    proposal: &SharedProposal,
) -> Result<(), ProtocolError> {
    validate_shared_entry(binding, public.next_step, &proposal.entry)?;
    if proposal.entry.agreement != AggregateAttestation::empty() {
        return Err(ProtocolError::InvalidCertificate(
            "pending proposal carries a certificate".into(),
        ));
    }
    if proposal.commitment.domain != crate::STEP_COMMIT_DOMAIN
        || proposal.commitment.session_id != binding.session_id()
        || proposal.commitment.step != public.next_step
        || proposal.commitment.pre_state != public.state_hash
        || proposal.commitment.link != public.chain_hash
        || proposal.commitment.post_state != StateHash::of(proposal.shared_state.as_bytes())
        || proposal.commitment.entry_hash != proposal.entry.entry_hash()
    {
        return Err(ProtocolError::InvalidCertificate(
            "pending proposal commitment is inconsistent".into(),
        ));
    }

    let participants = binding.participant_keys()?;
    if proposal.signatures.len() > participants.len() {
        return Err(ProtocolError::CollectionTooLarge {
            kind: "step signatures",
            actual: proposal.signatures.len(),
            max: participants.len(),
        });
    }
    for signature in &proposal.signatures {
        let key = binding.participant_key(&signature.participant)?;
        if signature.signature.step != proposal.commitment.step {
            return Err(ProtocolError::InvalidStepSignature {
                participant: signature.participant,
                step: proposal.commitment.step,
            });
        }
        let valid = key
            .verify(
                &proposal.commitment.signing_bytes(),
                &signature.signature.sig,
            )
            .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
        if !valid {
            return Err(ProtocolError::InvalidStepSignature {
                participant: signature.participant,
                step: proposal.commitment.step,
            });
        }
    }
    for pair in proposal.signatures.windows(2) {
        if pair[0].participant >= pair[1].participant {
            if pair[0].participant == pair[1].participant {
                return Err(if pair[0].signature == pair[1].signature {
                    ProtocolError::DuplicateStepSignature {
                        participant: pair[0].participant,
                    }
                } else {
                    ProtocolError::ConflictingStepSignature {
                        participant: pair[0].participant,
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

pub(crate) fn validate_terminal_progress(
    binding: &ExecutionBinding,
    producer: PeerId,
    public: PublicCursor,
    terminal: &TerminalProof,
) -> Result<(), ProtocolError> {
    if let Some((commitment, outcome, signatures)) = terminal.pending_parts() {
        validate_terminal_outcome(commitment, outcome)?;
        if commitment.domain != crate::TERMINAL_DOMAIN
            || commitment.session_id != binding.session_id()
            || commitment.final_step >= public.next_step
            || commitment.final_state != public.state_hash
        {
            return Err(ProtocolError::InvalidCertificate(
                "pending terminal commitment is inconsistent".into(),
            ));
        }
        if signatures.len() > binding.activation.tickets().len() {
            return Err(ProtocolError::CollectionTooLarge {
                kind: "terminal signatures",
                actual: signatures.len(),
                max: binding.activation.tickets().len(),
            });
        }
        for signature in signatures {
            let key = binding.participant_key(&signature.participant)?;
            let valid = key
                .verify(&commitment.signing_bytes(), &signature.signature)
                .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
            if !valid {
                return Err(ProtocolError::InvalidTerminalSignature {
                    participant: signature.participant,
                });
            }
        }
        validate_terminal_signature_order(signatures)?;
        return Ok(());
    }

    if let Some((certificate, outcome)) = terminal.certified_parts() {
        validate_terminal_outcome(&certificate.commitment, outcome)?;
        return validate_terminal_certificate(binding, public, certificate);
    }
    if let Some((certificate, outcome, body, request)) = terminal.seal_requested_parts() {
        validate_terminal_outcome(&certificate.commitment, outcome)?;
        validate_terminal_certificate(binding, public, certificate)?;
        validate_receipt_body(binding, producer, body)?;
        let ReceiptTermination::Completed { terminal } = body.termination() else {
            return Err(ProtocolError::ReceiptBodyMismatch);
        };
        if terminal.final_step != certificate.commitment.final_step
            || terminal.final_state != certificate.commitment.final_state
            || terminal.outcome_hash != certificate.commitment.outcome_hash
            || terminal.agreement != certificate.agreement
            || body.outcome() != outcome.borsh()
        {
            return Err(ProtocolError::ReceiptBodyMismatch);
        }
        if ProofId::derive(body)? != request.proof_id() {
            return Err(ProtocolError::InvalidProducerSeal);
        }
        request.data().validate()?;
        if request.producer() != binding.activation.offer().data().creator
            && !binding
                .activation
                .tickets()
                .iter()
                .any(|ticket| ticket.data.signer == request.producer())
        {
            return Err(ProtocolError::UnknownParticipant {
                participant: request.producer(),
            });
        }
        if request.proof_id() == ProofId::from_bytes([0; 32]) {
            return Err(ProtocolError::InvalidCertificate(
                "seal request has an empty proof id".into(),
            ));
        }
        return Ok(());
    }
    if let Some((cause, body, request)) = terminal.stopped_receipt_assembled_parts() {
        cause.validate()?;
        super::status::validate_cause_binding(cause, binding, public)?;
        validate_receipt_body(binding, producer, body)?;
        let ReceiptTermination::Stopped { cause: body_cause } = body.termination() else {
            return Err(ProtocolError::ReceiptBodyMismatch);
        };
        if body_cause != cause {
            return Err(ProtocolError::ReceiptBodyMismatch);
        }
        if ProofId::derive(body)? != request.proof_id()
            || ReceiptId::derive_body(body)? != request.data().receipt_id()
        {
            return Err(ProtocolError::InvalidProducerSeal);
        }
        request.data().validate()?;
        if request.producer() != producer {
            return Err(ProtocolError::InvalidProducerSeal);
        }
        return Ok(());
    }
    Err(ProtocolError::InvalidCertificate(
        "unknown terminal proof state".into(),
    ))
}

fn validate_terminal_signature_order(
    signatures: &[ParticipantTerminalSignature],
) -> Result<(), ProtocolError> {
    for pair in signatures.windows(2) {
        if pair[0].participant >= pair[1].participant {
            if pair[0].participant == pair[1].participant {
                return Err(if pair[0].signature == pair[1].signature {
                    ProtocolError::DuplicateTerminalSignature {
                        participant: pair[0].participant,
                    }
                } else {
                    ProtocolError::ConflictingTerminalSignature {
                        participant: pair[0].participant,
                    }
                });
            }
            return Err(ProtocolError::InvalidCertificate(
                "pending terminal signatures are not canonical".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_terminal_certificate(
    binding: &ExecutionBinding,
    public: PublicCursor,
    certificate: &TerminalCertificate,
) -> Result<(), ProtocolError> {
    if certificate.commitment.domain != crate::TERMINAL_DOMAIN
        || certificate.commitment.session_id != binding.session_id()
        || certificate.commitment.final_step >= public.next_step
        || certificate.commitment.final_state != public.state_hash
    {
        return Err(ProtocolError::InvalidCertificate(
            "terminal certificate is inconsistent with the public cursor".into(),
        ));
    }
    let participants = binding.participant_keys()?;
    if !certificate.agreement.signers.is_full(participants.len()) {
        return Err(ProtocolError::IncompleteProof {
            actual: certificate.agreement.signers.count(),
            expected: participants.len(),
        });
    }
    certificate
        .agreement
        .verify_signatures(
            certificate.commitment.final_step,
            &certificate.commitment.signing_bytes(),
            &participants.iter().map(|(_, key)| *key).collect::<Vec<_>>(),
        )
        .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))
}

fn validate_terminal_outcome(
    commitment: &crate::trace::TerminalCommitment,
    outcome: &TerminalOutcome,
) -> Result<(), ProtocolError> {
    outcome.validate()?;
    if commitment.outcome_hash != OutcomeHash::of(outcome.borsh()) {
        return Err(ProtocolError::OutcomeProjectionMismatch);
    }
    Ok(())
}

/// Validate the complete receipt body independently of terminal state.
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
        ReceiptTermination::Completed { .. } => {}
        ReceiptTermination::Stopped { cause } => {
            cause.validate()?;
            if !body.outcome().is_empty() {
                return Err(ProtocolError::OutcomeProjectionMismatch);
            }
        }
    }
    Ok(())
}

/// Validate receipt identity, activation binding, full trace, and terminal proof.
///
/// The body is self-describing: completion carries its terminal certificate and
/// stopped receipts carry their authenticated/shared stop cause.  No separate
/// optional terminal argument is accepted, so callers cannot accidentally
/// validate an abort body against a successful certificate.
pub(crate) fn validate_receipt_body(
    binding: &ExecutionBinding,
    producer: PeerId,
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
        || header.producer != producer
        || body.params() != binding.activation.offer().data().params.as_bytes()
    {
        return Err(ProtocolError::ReceiptBodyMismatch);
    }
    let participants = binding.participant_keys()?;
    let participant_keys = participants.iter().map(|(_, key)| *key).collect::<Vec<_>>();
    match header.terminal {
        ReceiptTermination::Completed { ref terminal } => validate_completed_receipt(
            binding,
            body,
            terminal,
            &participant_keys,
            participants.len(),
        ),
        ReceiptTermination::Stopped { ref cause } => {
            validate_stopped_receipt(binding, body, cause, &participant_keys, participants.len())
        }
    }
}

fn validate_completed_receipt(
    binding: &ExecutionBinding,
    body: &ReceiptBody,
    terminal: &SessionTerminal,
    participant_keys: &[arena0_crypto::BlsPublicKey],
    participant_count: usize,
) -> Result<(), ProtocolError> {
    if body.trace().is_empty()
        || terminal.final_step
            != u64::try_from(body.trace().len() - 1)
                .map_err(|_| ProtocolError::ReceiptBodyMismatch)?
        || terminal.outcome_hash != OutcomeHash::of(body.outcome())
        || terminal.agreement.signers.count() != participant_count
        || !terminal.agreement.signers.is_full(participant_count)
    {
        return Err(ProtocolError::ReceiptBodyMismatch);
    }
    let commitment = crate::TerminalCommitment::new(
        binding.session_id(),
        terminal.final_step,
        terminal.final_state,
        terminal.outcome_hash,
    );
    if terminal.agreement != AggregateAttestation::empty() {
        terminal
            .agreement
            .verify_signatures(
                terminal.final_step,
                &commitment.signing_bytes(),
                participant_keys,
            )
            .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
    }
    validate_receipt_trace(
        binding,
        body.trace(),
        participant_keys,
        participant_count,
        ReceiptTraceTerminal::Completed {
            outcome: body.outcome(),
            final_state: terminal.final_state,
        },
    )
    .map(|_| ())
}

fn validate_stopped_receipt(
    binding: &ExecutionBinding,
    body: &ReceiptBody,
    cause: &StopCause,
    participant_keys: &[arena0_crypto::BlsPublicKey],
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
            if body.trace().is_empty()
                || body.trace().last().map(|entry| entry.step) != Some(commitment.step)
                || body.trace().last().map(TraceEntry::entry_hash) != Some(commitment.entry_hash)
            {
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
    Completed {
        outcome: &'a [u8],
        final_state: crate::StateHash,
    },
    Authenticated,
    Stopped {
        kind: AbortKind,
        reason: &'a str,
    },
}

fn validate_receipt_trace(
    binding: &ExecutionBinding,
    trace: &[TraceEntry],
    participant_keys: &[arena0_crypto::BlsPublicKey],
    participant_count: usize,
    terminal: ReceiptTraceTerminal<'_>,
) -> Result<PublicCursor, ProtocolError> {
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
        if commitment.post_state != entry.post_state || commitment.entry_hash != entry.entry_hash()
        {
            return Err(ProtocolError::ReceiptBodyMismatch);
        }
        entry
            .agreement
            .verify_signatures(step, &commitment.signing_bytes(), participant_keys)
            .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
        let terminal_count = terminal_effect_count(entry);
        if index + 1 < trace.len() && terminal_count != 0 {
            return Err(ProtocolError::TerminalTraceMismatch);
        }
        previous_state = entry.post_state;
        previous_link = commitment.link_hash();
    }

    let final_entry = trace.last();
    match terminal {
        ReceiptTraceTerminal::Completed {
            outcome,
            final_state,
        } => {
            let Some(entry) = final_entry else {
                return Err(ProtocolError::ReceiptBodyMismatch);
            };
            if !matches!(entry.effects.as_slice(), [PublicEffect::SessionEnd { .. }])
                || entry.completed_outcome() != Some(outcome)
                || entry.post_state != final_state
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
            let valid = match (kind, entry.effects.as_slice()) {
                (AbortKind::Abort, [PublicEffect::SessionAbort { reason: actual }])
                | (AbortKind::Fail, [PublicEffect::Fail { reason: actual }]) => actual == reason,
                _ => false,
            };
            if !valid {
                return Err(ProtocolError::TerminalTraceMismatch);
            }
        }
    }
    Ok(PublicCursor::new(
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
    validate_public_event_payload(&entry.event)?;
    if entry.effects.len() > MAX_SHARED_EFFECTS {
        return Err(ProtocolError::CollectionTooLarge {
            kind: "shared effects",
            actual: entry.effects.len(),
            max: MAX_SHARED_EFFECTS,
        });
    }
    for effect in &entry.effects {
        validate_public_effect(effect)?;
    }
    Ok(())
}

/// Validate the event/effect class allowed at a public consensus boundary.
/// Local answers, timers, reactions, and guest suspension effects never enter
/// ensemble evidence.
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
        PublicEvent::SessionStarted { ensemble } => {
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
        PublicEvent::MessageReceived { .. } => {
            if expected_step == 0 {
                return Err(ProtocolError::MissingSessionStart);
            }
        }
    }
    if terminal_effect_count(entry) > 1 {
        return Err(ProtocolError::MultipleTerminalEffects);
    }
    Ok(())
}

pub(crate) fn validate_private_record(record: &PrivateRecord) -> Result<(), ProtocolError> {
    let encoded =
        borsh::to_vec(record).map_err(|error| ProtocolError::Serialization(error.to_string()))?;
    ensure_encoded("private record", encoded.len(), MAX_PRIVATE_RECORD_BYTES).map_err(|error| {
        match error {
            ProtocolError::EncodedTooLarge { actual, max, .. } => {
                ProtocolError::PrivateRecordTooLarge { actual, max }
            }
            other => other,
        }
    })?;
    if record.effects.len() > MAX_PRIVATE_EFFECTS {
        return Err(ProtocolError::CollectionTooLarge {
            kind: "private effects",
            actual: record.effects.len(),
            max: MAX_PRIVATE_EFFECTS,
        });
    }
    for effect in &record.effects {
        validate_private_effect(effect)?;
    }
    for draw in &record.draws {
        ensure_payload("random draw", draw.len(), MAX_EFFECT_PAYLOAD_BYTES)?;
    }
    validate_private_event_payload(&record.event)?;
    Ok(())
}

pub(crate) fn validate_pending_record(pending: &PendingRecord) -> Result<(), ProtocolError> {
    if pending
        .label
        .as_ref()
        .is_some_and(|label| label.len() > MAX_TERMINAL_REASON_BYTES)
        || pending
            .expected_type
            .as_ref()
            .is_some_and(|value| value.len() > MAX_TERMINAL_REASON_BYTES)
    {
        return Err(ProtocolError::InvalidPendingContinuation);
    }
    Ok(())
}

fn validate_public_event_payload(event: &PublicEvent) -> Result<(), ProtocolError> {
    match event {
        PublicEvent::MessageReceived { msg, .. } => {
            ensure_payload("event payload", msg.len(), MAX_EFFECT_PAYLOAD_BYTES)
        }
        PublicEvent::SessionStarted { .. } => Ok(()),
    }
}

fn validate_private_event_payload(event: &PrivateEvent) -> Result<(), ProtocolError> {
    match event {
        PrivateEvent::InputReceived { data: msg, .. } => {
            ensure_payload("event payload", msg.len(), MAX_EFFECT_PAYLOAD_BYTES)
        }
        PrivateEvent::TypedTimerFired { timer } => {
            ensure_payload(
                "timer type name",
                timer.type_name.len(),
                MAX_TERMINAL_REASON_BYTES,
            )?;
            ensure_payload("timer data", timer.data.len(), MAX_TIMER_PAYLOAD_BYTES)
        }
        PrivateEvent::Signed { signature, .. } => ensure_payload(
            "signature payload",
            signature.len(),
            MAX_EFFECT_PAYLOAD_BYTES,
        ),
        PrivateEvent::TimerFired | PrivateEvent::React => Ok(()),
    }
}

fn validate_public_effect(effect: &PublicEffect) -> Result<(), ProtocolError> {
    match effect {
        PublicEffect::SessionEnd { outcome } => ensure_payload(
            "terminal outcome",
            outcome.len(),
            MAX_TERMINAL_OUTCOME_BYTES,
        ),
        PublicEffect::SessionAbort { reason } | PublicEffect::Fail { reason } => {
            ensure_payload("terminal reason", reason.len(), MAX_TERMINAL_REASON_BYTES)
        }
    }
}

fn validate_private_effect(effect: &PrivateEffect) -> Result<(), ProtocolError> {
    match effect {
        PrivateEffect::Broadcast { data } => {
            ensure_payload("guest effect payload", data.len(), MAX_EFFECT_PAYLOAD_BYTES)
        }
        PrivateEffect::Sign { data, .. } => {
            ensure_payload("guest effect payload", data.len(), MAX_EFFECT_PAYLOAD_BYTES)
        }
        PrivateEffect::Callout {
            context,
            pending_label,
            expected_type,
            ..
        } => {
            ensure_payload("callout context", context.len(), MAX_EFFECT_PAYLOAD_BYTES)?;
            if let Some(label) = pending_label {
                ensure_payload("pending label", label.len(), MAX_TERMINAL_REASON_BYTES)?;
            }
            if let Some(expected) = expected_type {
                ensure_payload("expected type", expected.len(), MAX_TERMINAL_REASON_BYTES)?;
            }
            Ok(())
        }
        PrivateEffect::SetTimer { timer, .. } => {
            if let Some(timer) = timer {
                ensure_payload(
                    "timer type name",
                    timer.type_name.len(),
                    MAX_TERMINAL_REASON_BYTES,
                )?;
                ensure_payload("timer data", timer.data.len(), MAX_TIMER_PAYLOAD_BYTES)?;
            }
            Ok(())
        }
        PrivateEffect::RetryInput { reason } => {
            ensure_payload("retry reason", reason.len(), MAX_TERMINAL_REASON_BYTES)
        }
    }
}

pub(crate) fn terminal_effect_count(entry: &TraceEntry) -> usize {
    entry
        .effects
        .iter()
        .filter(|effect| {
            matches!(
                effect,
                PublicEffect::SessionEnd { .. }
                    | PublicEffect::SessionAbort { .. }
                    | PublicEffect::Fail { .. }
            )
        })
        .count()
}

/// Apply the one-shot timer mutations owned by one plan.
pub(crate) fn apply_timer_mutations(
    active: &mut Vec<ActiveTimer>,
    mutations: &[TimerMutation],
) -> Result<(), ProtocolError> {
    let mut seen = std::collections::BTreeSet::new();
    for mutation in mutations {
        mutation.validate()?;
        if !seen.insert(mutation.timer_id()) {
            return Err(ProtocolError::DuplicateTimerMutation {
                timer_id: mutation.timer_id(),
            });
        }
        match mutation {
            TimerMutation::Arm { timer_id, .. } => {
                if active.iter().any(|timer| timer.id == *timer_id) {
                    return Err(ProtocolError::DuplicateTimerMutation {
                        timer_id: *timer_id,
                    });
                }
                active.push(ActiveTimer { id: *timer_id });
            }
            TimerMutation::Cancel { timer_id } => {
                let Some(index) = active.iter().position(|timer| timer.id == *timer_id) else {
                    return Err(ProtocolError::StaleTimerFiring);
                };
                active.remove(index);
            }
        }
        if active.len() > MAX_ACTIVE_TIMERS {
            return Err(ProtocolError::TooManyTimers {
                actual: active.len(),
                max: MAX_ACTIVE_TIMERS,
            });
        }
    }
    active.sort_by_key(|timer| timer.id);
    if active.windows(2).any(|pair| pair[0].id == pair[1].id) {
        return Err(ProtocolError::TimerSetNotCanonical);
    }
    Ok(())
}

pub(crate) fn require_active(
    state: &ExecutionState,
    input: &'static str,
) -> Result<(), ProtocolError> {
    if !state.status().is_runnable() {
        return Err(illegal(state, input));
    }
    Ok(())
}

pub(crate) fn illegal(state: &ExecutionState, input: &'static str) -> ProtocolError {
    if state.status().is_terminal() {
        ProtocolError::AlreadyTerminal
    } else {
        ProtocolError::IllegalLifecycle {
            current: state.lifecycle(),
            input,
        }
    }
}
