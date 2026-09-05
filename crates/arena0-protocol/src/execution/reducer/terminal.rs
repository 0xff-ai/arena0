#[cfg(feature = "performance-tracing")]
use std::time::Instant;

use super::super::{
    AbortOccurrence, DurableEffect, ExecutionState, ExecutionStatus, MAX_PROOF_SIGNATURES,
    MAX_TERMINAL_REASON_BYTES, ParticipantTerminalSignature, PlanDraft, ProtocolError,
    ReceiptArtifact, ReceiptBody, TerminalProof, TerminalPublication, TimerMutation,
    apply_timer_mutations, build_plan, ensure_payload, make_terminal_certificate, require_active,
    validate_receipt_body,
};

/// Add one participant signature to a pending terminal commitment.
pub(crate) fn reduce_signature(
    state: &ExecutionState,
    signature: ParticipantTerminalSignature,
) -> Result<PlanDraft, ProtocolError> {
    #[cfg(feature = "performance-tracing")]
    let performance_enabled = tracing::enabled!(
        target: "arena0::performance",
        tracing::Level::TRACE
    );
    #[cfg(feature = "performance-tracing")]
    let started = performance_enabled.then(Instant::now);
    #[cfg(feature = "performance-tracing")]
    let count = performance_enabled.then(|| {
        state
            .status()
            .terminal_proof()
            .and_then(TerminalProof::pending_parts)
            .map_or(0, |(_, _, signatures)| signatures.len())
    });
    let result = reduce_signature_inner(state, signature);

    #[cfg(feature = "performance-tracing")]
    if let (Some(started), Some(count)) = (started, count) {
        let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        tracing::trace!(
            target: "arena0::performance",
            operation = "terminal_signature_apply",
            exec_id = %state.execution_id(),
            session_id = %state.binding().session_id(),
            version = state.version().get(),
            public_step = state.public().next_step(),
            input_kind = "terminal_signature",
            count,
            success = result.is_ok(),
            result_class = if result.is_ok() { "accepted" } else { "rejected" },
            elapsed_us,
        );
    }

    result
}

fn reduce_signature_inner(
    state: &ExecutionState,
    signature: ParticipantTerminalSignature,
) -> Result<PlanDraft, ProtocolError> {
    require_active(state, "terminal signature")?;
    let Some((commitment, outcome, signatures)) = state
        .status()
        .terminal_proof()
        .and_then(TerminalProof::pending_parts)
    else {
        return Err(ProtocolError::TerminalProofMissing);
    };

    let participant = signature.participant;
    if let Some(existing) = signatures
        .iter()
        .find(|existing| existing.participant == participant)
    {
        if existing.signature == signature.signature {
            return Err(ProtocolError::DuplicateTerminalSignature { participant });
        }
        return Err(ProtocolError::ConflictingTerminalSignature { participant });
    }
    let key = state.binding.participant_key(&participant)?;
    let valid = key
        .verify(&commitment.signing_bytes(), &signature.signature)
        .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
    if !valid {
        return Err(ProtocolError::InvalidTerminalSignature { participant });
    }
    if signatures.len() >= MAX_PROOF_SIGNATURES {
        return Err(ProtocolError::CollectionTooLarge {
            kind: "terminal signatures",
            actual: signatures.len() + 1,
            max: MAX_PROOF_SIGNATURES,
        });
    }

    let mut next_signatures = signatures.to_vec();
    let local_signature = participant == state.producer();
    let publication_signature = local_signature.then(|| signature.clone());
    next_signatures.push(signature);
    next_signatures.sort_by_key(|item| item.participant);
    let mut next = state.clone();
    if next_signatures.len() == state.binding.activation.tickets().len() {
        let certificate = make_terminal_certificate(&state.binding, commitment, &next_signatures)?;
        next.status = ExecutionStatus::from_terminal_proof(TerminalProof::certified(
            certificate,
            outcome.clone(),
        ));
    } else {
        next.status = ExecutionStatus::from_terminal_proof(TerminalProof::pending(
            commitment.clone(),
            outcome.clone(),
            next_signatures,
        ));
    }
    let mut effects = Vec::new();
    if local_signature {
        let signature = publication_signature.expect("local terminal signature was just staged");
        for (destination, _) in state.binding.participant_keys()? {
            if destination != state.producer() {
                effects.push(DurableEffect::publish_terminal_signature(
                    destination,
                    commitment.clone(),
                    signature.signature(),
                ));
            }
        }
    }
    build_plan(state, next, None, None, None, Vec::new(), effects)
}

/// Authenticate the store-assembled evidence and publish it atomically.
pub(crate) fn reduce_receipt_body(
    state: &ExecutionState,
    body: ReceiptBody,
) -> Result<PlanDraft, ProtocolError> {
    validate_receipt_body(&state.binding, &body)?;
    let artifact = ReceiptArtifact::new(body)?;
    let body = artifact.body();
    let mut next = state.clone();
    next.status = match (state.status(), body.termination()) {
        (
            ExecutionStatus::TerminalProof { proof },
            crate::ReceiptTermination::Completed { terminal },
        ) => {
            let (certificate, outcome) = proof
                .certified_parts()
                .ok_or(ProtocolError::TerminalProofMissing)?;
            if terminal.final_step != certificate.commitment().final_step
                || terminal.final_state != certificate.commitment().final_state
                || terminal.outcome_hash != certificate.commitment().outcome_hash
                || terminal.agreement != *certificate.agreement()
                || body.outcome() != outcome.borsh()
            {
                return Err(ProtocolError::ReceiptBodyMismatch);
            }
            ExecutionStatus::completed(certificate.clone(), outcome.clone(), artifact.receipt_id())
        }
        (
            ExecutionStatus::Stopped { cause },
            crate::ReceiptTermination::Stopped { cause: body_cause },
        ) if cause == body_cause => {
            ExecutionStatus::stopped_published(cause.clone(), artifact.receipt_id())
        }
        _ => return Err(ProtocolError::ReceiptBodyMismatch),
    };
    let timer_mutations = state
        .timers
        .iter()
        .map(|timer| TimerMutation::cancel(timer.id))
        .collect::<Vec<_>>();
    apply_timer_mutations(&mut next.timers, &timer_mutations)?;
    let effect = DurableEffect::publish_receipt(artifact.clone())?;
    build_plan(
        state,
        next,
        None,
        None,
        Some(TerminalPublication { receipt: artifact }),
        timer_mutations,
        vec![effect],
    )
}

/// Accept a signed local or peer abort/fail occurrence. Neither public nor
/// private execution cursors change; the durable version still advances.
pub(crate) fn reduce_abort(
    state: &ExecutionState,
    occurrence: AbortOccurrence,
) -> Result<PlanDraft, ProtocolError> {
    occurrence.validate_for_session(state.binding().session_id())?;
    if !state
        .binding()
        .activation
        .tickets()
        .iter()
        .any(|ticket| ticket.data.signer == occurrence.sender())
    {
        return Err(ProtocolError::UnauthenticatedAbort);
    }
    if !occurrence.verify_signature()? {
        return Err(ProtocolError::UnauthenticatedAbort);
    }
    if *occurrence.coordinate() != state.public() {
        return Err(ProtocolError::InvalidAbortCoordinate);
    }
    let local = occurrence.sender() == state.producer();
    if state.status().is_terminal() {
        return Err(ProtocolError::AlreadyTerminal);
    }
    if state.terminal_proof().is_some() {
        return Err(ProtocolError::TerminalProofPending);
    }
    let mut next = state.clone();
    let timer_mutations = state
        .timers
        .iter()
        .map(|timer| TimerMutation::cancel(timer.id))
        .collect::<Vec<_>>();
    apply_timer_mutations(&mut next.timers, &timer_mutations)?;
    let effects = if local {
        state
            .binding()
            .participant_keys()?
            .into_iter()
            .filter(|(peer, _)| *peer != state.producer())
            .map(|(peer, _)| DurableEffect::send_abort(peer, occurrence.clone()))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    next.proposal = None;
    next.status = ExecutionStatus::stopped(occurrence)?;
    build_plan(state, next, None, None, None, timer_mutations, effects)
}

/// Preserve in-flight terminal evidence when an operator or transport event
/// interrupts receipt publication. This is a distinct terminal projection,
/// never an aborted session and never a completed receipt.
pub(crate) fn reduce_interrupt_terminal(
    state: &ExecutionState,
    reason: String,
) -> Result<PlanDraft, ProtocolError> {
    if state.status().is_terminal() {
        return Err(ProtocolError::AlreadyTerminal);
    }
    let Some(proof) = state.terminal_proof() else {
        return Err(ProtocolError::TerminalProofMissing);
    };
    ensure_payload("terminal reason", reason.len(), MAX_TERMINAL_REASON_BYTES)?;
    let mut next = state.clone();
    next.status = ExecutionStatus::incomplete_from_local(proof.clone(), reason)?;
    let timer_mutations = state
        .timers
        .iter()
        .map(|timer| TimerMutation::cancel(timer.id))
        .collect::<Vec<_>>();
    apply_timer_mutations(&mut next.timers, &timer_mutations)?;
    build_plan(state, next, None, None, None, timer_mutations, Vec::new())
}
