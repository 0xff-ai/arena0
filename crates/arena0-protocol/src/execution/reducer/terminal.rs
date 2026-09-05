use arena0_crypto::SignScheme;
#[cfg(feature = "performance-tracing")]
use std::time::Instant;

use super::super::{
    AbortOccurrence, DurableEffect, ExecutionState, ExecutionStatus, MAX_PROOF_SIGNATURES,
    MAX_TERMINAL_REASON_BYTES, ParticipantTerminalSignature, PlanDraft, ProducerSeal,
    ProducerSealRequest, ProtocolError, Receipt, ReceiptBody, TerminalProof, TerminalPublication,
    TimerMutation, apply_timer_mutations, build_plan, ensure_payload, make_terminal_certificate,
    require_active, validate_receipt_body,
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

/// Stage and validate the complete store-assembled receipt body, then request
/// the producer's typed Ed25519 seal over its stable identity.
pub(crate) fn reduce_receipt_body(
    state: &ExecutionState,
    body: ReceiptBody,
) -> Result<PlanDraft, ProtocolError> {
    let completion = state
        .status()
        .terminal_proof()
        .and_then(TerminalProof::certified_parts);
    let stopped = match state.status() {
        ExecutionStatus::Stopped { cause } => Some(cause.clone()),
        _ => None,
    };
    if completion.is_none() && stopped.is_none() {
        return Err(ProtocolError::TerminalProofMissing);
    }
    validate_receipt_body(&state.binding, state.producer, &body)?;
    let proof_id = super::super::ProofId::derive(&body)?;
    let receipt_id = super::super::ReceiptId::derive_body(&body)?;
    let request = ProducerSealRequest::new(proof_id, receipt_id, state.producer);
    let proof = match (completion, stopped) {
        (Some((certificate, outcome)), None) => {
            let crate::ReceiptTermination::Completed { terminal } = body.termination() else {
                return Err(ProtocolError::ReceiptBodyMismatch);
            };
            if terminal.final_step != certificate.commitment().final_step
                || terminal.final_state != certificate.commitment().final_state
                || terminal.outcome_hash != certificate.commitment().outcome_hash
                || terminal.agreement != *certificate.agreement()
                || body.outcome() != outcome.borsh()
            {
                return Err(ProtocolError::ReceiptBodyMismatch);
            }
            TerminalProof::receipt_assembled(certificate.clone(), outcome.clone(), body, request)
        }
        (None, Some(cause)) => {
            let crate::ReceiptTermination::Stopped { cause: body_cause } = body.termination()
            else {
                return Err(ProtocolError::ReceiptBodyMismatch);
            };
            if body_cause != &cause {
                return Err(ProtocolError::ReceiptBodyMismatch);
            }
            TerminalProof::stopped_receipt_assembled(cause, body, request)
        }
        _ => return Err(ProtocolError::InvalidTerminalStatus),
    };
    let mut next = state.clone();
    let data = match &proof {
        TerminalProof::ReceiptAssembled { request, .. }
        | TerminalProof::StoppedReceiptAssembled { request, .. } => *request.data(),
        _ => return Err(ProtocolError::InvalidTerminalStatus),
    };
    next.status = ExecutionStatus::from_terminal_proof(proof);
    build_plan(
        state,
        next,
        None,
        None,
        None,
        Vec::new(),
        vec![DurableEffect::RequestProducerSeal { data }],
    )
}

/// Verify the producer seal against the durable staged body and publish the
/// resulting sealed receipt artifact.
pub(crate) fn reduce_producer_seal(
    state: &ExecutionState,
    seal: ProducerSeal,
) -> Result<PlanDraft, ProtocolError> {
    require_active(state, "producer seal")?;
    enum Staged<'a> {
        Completed {
            certificate: &'a super::super::TerminalCertificate,
            outcome: &'a super::super::TerminalOutcome,
            body: &'a ReceiptBody,
            request: &'a ProducerSealRequest,
        },
        Stopped {
            cause: &'a super::super::StopCause,
            body: &'a ReceiptBody,
            request: &'a ProducerSealRequest,
        },
    }
    let staged = match state.status().terminal_proof() {
        Some(proof) => {
            if let Some((certificate, outcome, body, request)) = proof.receipt_assembled_parts() {
                Staged::Completed {
                    certificate,
                    outcome,
                    body,
                    request,
                }
            } else if let Some((cause, body, request)) = proof.stopped_receipt_assembled_parts() {
                Staged::Stopped {
                    cause,
                    body,
                    request,
                }
            } else {
                return Err(ProtocolError::TerminalProofMissing);
            }
        }
        None => return Err(ProtocolError::TerminalProofMissing),
    };
    let request = match &staged {
        Staged::Completed { request, .. } | Staged::Stopped { request, .. } => request,
    };
    if seal.data() != request.data() {
        return Err(ProtocolError::InvalidProducerSeal);
    }
    let body = match &staged {
        Staged::Completed {
            certificate,
            outcome,
            body,
            ..
        } => {
            validate_receipt_body(&state.binding, state.producer, body)?;
            let crate::ReceiptTermination::Completed { terminal } = body.termination() else {
                return Err(ProtocolError::ReceiptBodyMismatch);
            };
            if terminal.final_step != certificate.commitment().final_step
                || terminal.final_state != certificate.commitment().final_state
                || terminal.outcome_hash != certificate.commitment().outcome_hash
                || terminal.agreement != *certificate.agreement()
                || body.outcome() != outcome.borsh()
            {
                return Err(ProtocolError::ReceiptBodyMismatch);
            }
            body
        }
        Staged::Stopped { cause, body, .. } => {
            validate_receipt_body(&state.binding, state.producer, body)?;
            let crate::ReceiptTermination::Stopped { cause: body_cause } = body.termination()
            else {
                return Err(ProtocolError::ReceiptBodyMismatch);
            };
            if body_cause != *cause {
                return Err(ProtocolError::ReceiptBodyMismatch);
            }
            body
        }
    };
    let signing_bytes = request.signing_bytes()?;
    let verified = arena0_crypto::verify(
        SignScheme::Ed25519,
        &state.producer.0,
        &signing_bytes,
        &seal.signature().0,
    )
    .map_err(|error| ProtocolError::InvalidProducerSealCrypto(error.to_string()))?;
    if !verified {
        return Err(ProtocolError::InvalidProducerSeal);
    }
    let receipt = Receipt::new(
        (*body).clone(),
        ProducerSeal::new(*request.data(), seal.signature()),
    )?;
    let timer_mutations = state
        .timers
        .iter()
        .map(|timer| TimerMutation::cancel(timer.id))
        .collect::<Vec<_>>();
    let mut next = state.clone();
    apply_timer_mutations(&mut next.timers, &timer_mutations)?;
    next.status = match &staged {
        Staged::Completed {
            certificate,
            outcome,
            ..
        } => ExecutionStatus::completed(
            (*certificate).clone(),
            (*outcome).clone(),
            receipt.proof_id(),
            receipt.receipt_id(),
            state.producer,
        ),
        Staged::Stopped { cause, .. } => ExecutionStatus::stopped_published(
            (*cause).clone(),
            receipt.proof_id(),
            receipt.receipt_id(),
            state.producer,
        ),
    };
    let effect = DurableEffect::publish_receipt(receipt.clone())?;
    build_plan(
        state,
        next,
        None,
        None,
        Some(TerminalPublication { receipt }),
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
