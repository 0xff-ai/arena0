use crate::STEP_COMMIT_DOMAIN;
use crate::trace::{AggregateAttestation, StepCommitment, TerminalCommitment};
use crate::{OutcomeHash, StateHash};
#[cfg(feature = "performance-tracing")]
use std::time::Instant;

use super::super::{
    DurableEffect, ExecutionState, ExecutionStatus, MAX_PROOF_SIGNATURES,
    MAX_TERMINAL_OUTCOME_BYTES, ProtocolError, SharedCommit, SharedDelta, SharedProposal,
    TerminalProof, build_plan, ensure_payload, make_step_certificate, require_active,
    terminal_effect_count, validate_shared_entry,
};
use super::super::{ParticipantStepSignature, PlanDraft};

pub(crate) fn reduce_propose(
    state: &ExecutionState,
    delta: SharedDelta,
) -> Result<PlanDraft, ProtocolError> {
    require_active(state, "propose shared")?;
    if state.proposal.is_some() {
        return Err(ProtocolError::SharedProposalExists);
    }
    if state.terminal_proof().is_some() {
        return Err(ProtocolError::TerminalProofPending);
    }
    delta.validate()?;
    validate_shared_entry(&state.binding, state.public.next_step, &delta.entry)?;
    let entry = delta.entry;
    if entry.agreement != AggregateAttestation::empty() {
        return Err(ProtocolError::InvalidCertificate(
            "a pending proposal cannot carry an aggregate agreement".into(),
        ));
    }
    let post_state = StateHash::of(delta.shared_state.as_bytes());
    if entry.step != state.public.next_step {
        return Err(ProtocolError::StepCoordinateMismatch {
            expected: state.public.next_step,
            actual: entry.step,
        });
    }
    if entry.pre_state != state.public.state_hash {
        return Err(ProtocolError::PublicPreStateMismatch {
            expected: state.public.state_hash,
            actual: entry.pre_state,
        });
    }
    if entry.post_state != post_state {
        return Err(ProtocolError::PublicStateHashMismatch);
    }
    if entry.is_terminal() && terminal_effect_count(&entry) > 1 {
        return Err(ProtocolError::MultipleTerminalEffects);
    }
    let commitment =
        StepCommitment::for_entry(state.binding.session_id(), &entry, state.public.chain_hash);
    if commitment.domain != STEP_COMMIT_DOMAIN
        || commitment.session_id != state.binding.session_id()
        || commitment.step != entry.step
        || commitment.pre_state != state.public.state_hash
        || commitment.post_state != post_state
        || commitment.link != state.public.chain_hash
    {
        return Err(ProtocolError::InvalidCertificate(
            "shared proposal commitment does not bind the public cursor".into(),
        ));
    }
    let mut next = state.clone();
    next.proposal = Some(SharedProposal {
        commitment,
        entry,
        shared_state: delta.shared_state,
        terminal_outcome: delta.terminal_outcome,
        signatures: Vec::new(),
    });
    let effects = vec![DurableEffect::request_step_signature(
        next.proposal
            .as_ref()
            .expect("proposal was just staged")
            .commitment
            .clone(),
    )];
    build_plan(state, next, None, None, None, Vec::new(), effects)
}

pub(crate) fn reduce_signature(
    state: &ExecutionState,
    signature: ParticipantStepSignature,
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
            .pending_shared()
            .map_or(0, SharedProposal::signature_count)
    });
    let result = reduce_signature_inner(state, signature);

    #[cfg(feature = "performance-tracing")]
    if let (Some(started), Some(count)) = (started, count) {
        let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        tracing::trace!(
            target: "arena0::performance",
            operation = "step_signature_apply",
            exec_id = %state.execution_id(),
            session_id = %state.binding().session_id(),
            version = state.version().get(),
            public_step = state.public().next_step(),
            input_kind = "step_signature",
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
    signature: ParticipantStepSignature,
) -> Result<PlanDraft, ProtocolError> {
    require_active(state, "step signature")?;
    if state.terminal_proof().is_some() {
        return Err(ProtocolError::TerminalProofPending);
    }
    let proposal = state
        .proposal
        .as_ref()
        .ok_or(ProtocolError::SharedProposalMissing)?;
    let participant = signature.participant;
    if let Some(existing) = proposal
        .signatures
        .iter()
        .find(|existing| existing.participant == participant)
    {
        if existing.signature == signature.signature {
            return Err(ProtocolError::DuplicateStepSignature { participant });
        }
        return Err(ProtocolError::ConflictingStepSignature { participant });
    }
    let key = state.binding.participant_key(&participant)?;
    if signature.signature.step != proposal.commitment.step {
        return Err(ProtocolError::InvalidStepSignature {
            participant,
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
            participant,
            step: proposal.commitment.step,
        });
    }
    let mut next = state.clone();
    let next_proposal = next
        .proposal
        .as_mut()
        .ok_or(ProtocolError::SharedProposalMissing)?;
    if next_proposal.signatures.len() >= MAX_PROOF_SIGNATURES {
        return Err(ProtocolError::CollectionTooLarge {
            kind: "step signatures",
            actual: next_proposal.signatures.len() + 1,
            max: MAX_PROOF_SIGNATURES,
        });
    }
    let local_signature = participant == state.producer();
    next_proposal.signatures.push(signature.clone());
    next_proposal
        .signatures
        .sort_by_key(|item| item.participant);
    let mut effects = Vec::new();
    if local_signature {
        for (destination, _) in state.binding.participant_keys()? {
            if destination != state.producer() {
                effects.push(DurableEffect::publish_step_signature(
                    destination,
                    next_proposal.commitment.clone(),
                    signature.signature().sig,
                ));
            }
        }
    }
    if next_proposal.signatures.len() < state.binding.activation.tickets().len() {
        return build_plan(state, next, None, None, None, Vec::new(), effects);
    }

    let proposal = next
        .proposal
        .take()
        .ok_or(ProtocolError::SharedProposalMissing)?;
    let certificate = make_step_certificate(&state.binding, &proposal)?;
    let mut committed_entry = proposal.entry.clone();
    committed_entry.agreement = certificate.agreement.clone();
    let terminal_outcome = proposal.terminal_outcome.clone();
    let shared_commit = SharedCommit {
        entry: committed_entry,
        shared_state: proposal.shared_state.clone(),
        terminal_outcome: terminal_outcome.clone(),
        certificate,
    };
    next.public = state
        .public
        .advance(&shared_commit.certificate.commitment)?;
    next.shared_state = proposal.shared_state;
    if let Some(outcome) = shared_commit.entry.completed_outcome() {
        ensure_payload(
            "terminal outcome",
            outcome.len(),
            MAX_TERMINAL_OUTCOME_BYTES,
        )?;
        let commitment = TerminalCommitment::new(
            state.binding.session_id(),
            shared_commit.certificate.commitment.step,
            shared_commit.certificate.commitment.post_state,
            OutcomeHash::of(outcome),
        );
        let outcome = terminal_outcome.ok_or(ProtocolError::TerminalOutcomeRequired)?;
        next.status = ExecutionStatus::from_terminal_proof(TerminalProof::pending(
            commitment,
            outcome,
            Vec::new(),
        ));
        effects.push(DurableEffect::request_terminal_signature(
            next.status()
                .terminal_proof()
                .and_then(TerminalProof::pending_parts)
                .map(|(commitment, _, _)| commitment.clone())
                .expect("terminal proof was just staged"),
        ));
    } else if shared_commit.entry.effects.iter().any(|effect| {
        matches!(
            effect,
            crate::PublicEffect::SessionAbort { .. } | crate::PublicEffect::Fail { .. }
        )
    }) && let Some(status) = ExecutionStatus::from_shared_entry(
        &shared_commit.entry,
        shared_commit.certificate.commitment.clone(),
    )? {
        next.status = status;
    }
    build_plan(
        state,
        next,
        Some(shared_commit),
        None,
        None,
        Vec::new(),
        effects,
    )
}
