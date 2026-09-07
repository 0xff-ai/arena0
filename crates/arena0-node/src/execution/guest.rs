//! Guest-facing calls and private/shared progress transitions.
//!
//! Every method here loads a fresh durable snapshot, invokes the admitted
//! guest, and commits the resulting protocol delta through the store.

use crate::context::{ExecError, SessionMessage};
use arena0_crypto::SignScheme;
use arena0_program::{CallStatus, JsonBytes};
use arena0_protocol::PendingId;
use arena0_protocol::execution::GuestSignData;
use arena0_protocol::{
    AggregateAttestation, BroadcastFrame, Committed, Ensemble, Event, ExecFrame, ExecLifecycle,
    ExecutionInput, ExecutionState, MessageId, ParticipantStepSignature,
    ParticipantTerminalSignature, PeerIdSource, PendingKind, PendingRecord, PrivateCause,
    PrivateDelta, PrivateEffect, PrivateEvent, PrivateRecord, PublicEffect, PublicEvent,
    SharedDelta, StateHash, TRACE_FORMAT_VERSION, TerminalOutcome, TimerFiring, TimerPayload,
    TraceEntry, WitnessCommitment,
};
use arena0_sandbox::{
    LocalCall, LocalEvent, OutcomeCall, QueryCall, RandomReplay, SharedCall, SharedEvent, ViewCall,
    WriterCall,
};
use arena0_store::ApplyOutcome;

use super::{ExecutionActor, MAX_CAS_RETRIES, MAX_TIMER_BATCH, now_ms};

/// A shared call may be replayed only when the durable head changed after the
/// guest call. Keeping that case typed prevents ordinary runtime failures from
/// accidentally entering the CAS retry loop.
#[derive(Debug)]
enum SharedCommitError {
    VersionMismatch,
    Runtime(ExecError),
}

pub(super) struct ExecutionMessage {
    message_id: MessageId,
    sequence: u64,
    pre_state: StateHash,
    data: Vec<u8>,
    witness: WitnessCommitment,
}

pub(super) trait IntoExecutionMessage {
    fn into_execution_message(self) -> Result<ExecutionMessage, ExecError>;
}

impl IntoExecutionMessage for BroadcastFrame {
    fn into_execution_message(self) -> Result<ExecutionMessage, ExecError> {
        Ok(ExecutionMessage {
            message_id: self.message_id(),
            sequence: self.sequence(),
            pre_state: self.pre_state(),
            data: self.data().to_vec(),
            witness: self.witness(),
        })
    }
}

impl IntoExecutionMessage for ExecFrame {
    fn into_execution_message(self) -> Result<ExecutionMessage, ExecError> {
        let Self::Message {
            message_id,
            seq,
            prestate,
            data,
            witness,
        } = self
        else {
            return Err(ExecError::InvalidState(
                "execution frame is not a message".into(),
            ));
        };
        Ok(ExecutionMessage {
            message_id,
            sequence: seq,
            pre_state: prestate,
            data,
            witness,
        })
    }
}

impl From<ExecError> for SharedCommitError {
    fn from(error: ExecError) -> Self {
        Self::Runtime(error)
    }
}

impl ExecutionActor {
    pub(super) async fn submit_input(
        &mut self,
        pending_id: PendingId,
        callout_index: u32,
        data: JsonBytes,
    ) -> Result<(), ExecError> {
        let state = self.load_state().await?;
        let Some((pending, _)) = state.status().pending() else {
            return Err(ExecError::CalloutNotPending);
        };
        if pending.id != pending_id
            || pending.operation != (arena0_protocol::PendingOperation::Callout { callout_index })
        {
            return Err(ExecError::CalloutNotPending);
        }
        self.run_local(
            LocalEvent::InputReceived {
                callout_index,
                data: data.clone(),
                continuation_tag: pending.continuation_tag,
            },
            PrivateEvent::InputReceived {
                callout_index,
                data: data.into_bytes(),
                continuation_tag: pending.continuation_tag,
            },
            PrivateCause::resume(pending_id, PendingKind::Callout),
        )
        .await?;
        self.progress().await
    }

    /// Sign and durably resume one guest signing continuation.
    ///
    /// The request is an outbox effect, so the actor remains the only caller
    /// with access to either signing capability. A missing continuation after
    /// its private coordinate has advanced is the idempotent crash-replay
    /// case; no second guest invocation or signature round trip is needed.
    pub(super) async fn sign_and_resume(
        &mut self,
        pending_id: PendingId,
        data: &GuestSignData,
    ) -> Result<(), ExecError> {
        self.validate_guest_sign_data(pending_id, data)?;

        let expected_next_record = data.private_sequence().checked_add(1).ok_or_else(|| {
            ExecError::InvalidState("guest signing request private coordinate overflows".into())
        })?;
        let state = self.load_state().await?;
        let already_resumed = state.private().next_record() > expected_next_record;
        let Some((pending, coordinate)) = state.status().pending() else {
            if already_resumed {
                // The signature was durably applied before a crash interrupted
                // outbox acknowledgement. Replaying the leased request is
                // therefore an idempotent acknowledgement, not a new resume.
                return Ok(());
            }
            return Err(ExecError::InvalidState(
                "guest signing request has no durable continuation".into(),
            ));
        };
        if pending.operation.kind() != PendingKind::Sign || pending.id != pending_id {
            if already_resumed {
                // A later private continuation may already be pending when an
                // older signature lease is recovered. The private cursor proves
                // that this request crossed its durable reducer boundary.
                return Ok(());
            }
            return Err(ExecError::InvalidState(
                "guest signing request does not match the durable continuation".into(),
            ));
        }
        if coordinate.record() != data.private_sequence()
            || coordinate.effect_index() != data.effect_index()
        {
            return Err(ExecError::InvalidState(
                "guest signing request coordinate does not match durable continuation".into(),
            ));
        }
        if state.private().next_record() != expected_next_record {
            return Err(ExecError::InvalidState(
                "guest signing request private coordinate does not match durable state".into(),
            ));
        }

        let signing_bytes = data.signing_bytes()?;
        let signature = match data.scheme() {
            SignScheme::Ed25519 => self.context.identity.sign(&signing_bytes).0.to_vec(),
            SignScheme::Bls => self.context.execution_key.sign(&signing_bytes).0.to_vec(),
        };
        self.resume_signature(pending_id, signature).await
    }

    fn validate_guest_sign_data(
        &self,
        pending_id: PendingId,
        data: &GuestSignData,
    ) -> Result<(), ExecError> {
        if data.execution_id() != self.context.exec_id {
            return Err(ExecError::InvalidState(
                "guest signing request execution id does not match actor execution".into(),
            ));
        }
        if data.session_id() != self.context.activation.session_hash() {
            return Err(ExecError::InvalidState(
                "guest signing request session does not match actor session".into(),
            ));
        }
        if data.program_hash() != self.context.program.program().hash() {
            return Err(ExecError::InvalidState(
                "guest signing request program does not match actor program".into(),
            ));
        }
        let effect_index = usize::try_from(data.effect_index()).map_err(|_| {
            ExecError::InvalidState(
                "guest signing request effect coordinate does not fit host index".into(),
            )
        })?;
        let expected_pending_id = arena0_protocol::pending_id(
            self.context.exec_id,
            data.private_sequence(),
            effect_index,
        );
        if pending_id != expected_pending_id {
            return Err(ExecError::InvalidState(
                "guest signing request coordinate does not match its continuation".into(),
            ));
        }
        Ok(())
    }

    async fn resume_signature(
        &mut self,
        pending_id: PendingId,
        signature: Vec<u8>,
    ) -> Result<(), ExecError> {
        if signature.len() > arena0_protocol::MAX_EFFECT_PAYLOAD_BYTES {
            return Err(ExecError::InvalidState(
                "submitted signature exceeds the protocol payload bound".into(),
            ));
        }
        let state = self.load_state().await?;
        let Some((pending, _)) = state.status().pending() else {
            return Err(ExecError::InvalidState(
                "execution is not waiting for a signature".into(),
            ));
        };
        if pending.operation.kind() != PendingKind::Sign || pending.id != pending_id {
            return Err(ExecError::InvalidState(
                "signature does not match the durable continuation".into(),
            ));
        }
        let continuation_tag = pending.continuation_tag;
        self.run_local(
            LocalEvent::Signed {
                signature: signature.clone(),
                continuation_tag,
            },
            PrivateEvent::Signed {
                signature,
                continuation_tag,
            },
            PrivateCause::resume(pending_id, PendingKind::Sign),
        )
        .await?;
        Ok(())
    }

    pub(super) async fn query(
        &self,
        query_index: u32,
        query: JsonBytes,
    ) -> Result<JsonBytes, ExecError> {
        let state = self.load_state().await?;
        let projection = self.context.program.query(QueryCall::new(
            state.shared_state().clone(),
            self.ensemble(),
            query,
            query_index,
        ))?;
        Ok(projection.output)
    }

    pub(super) async fn view(
        &self,
        viewport: JsonBytes,
    ) -> Result<(u64, arena0_protocol::View), ExecError> {
        let state = self.load_state().await?;
        let projection = self.context.program.view(ViewCall::new(
            state.shared_state().clone(),
            self.ensemble(),
            viewport,
        ))?;
        let view = serde_json::from_slice(projection.output.as_bytes()).map_err(|error| {
            ExecError::Unavailable(format!("view projection is not a View: {error}"))
        })?;
        Ok((state.public().next_step(), view))
    }

    pub(super) async fn ensure_session_started(&mut self) -> Result<(), ExecError> {
        for _ in 0..MAX_CAS_RETRIES {
            let state = self.load_state().await?;
            if state.status().lifecycle() != ExecLifecycle::Active
                || state.public().next_step() != 0
                || state.pending_shared().is_some()
            {
                if self.session_start_is_durable(&state) {
                    self.emit_session_started().await?;
                }
                return Ok(());
            }
            let ensemble = self.ensemble();
            let raw = Event::SessionStarted {
                ensemble: ensemble.clone(),
            };
            let result = self
                .context
                .program
                .apply_shared(SharedCall::session_started(
                    state.shared_state().clone(),
                    ensemble,
                ))?;
            match self
                .commit_shared_result(state, raw, result, None, None)
                .await
            {
                Ok(true) => {
                    // The observer boundary follows the store CAS. A
                    // restarted actor may re-emit this handoff, but it can
                    // never report SessionStarted before its durable proposal
                    // exists.
                    self.emit_session_started().await?;
                    return Ok(());
                }
                Ok(false) => {
                    return Err(ExecError::InvalidState(
                        "session start was rejected by the shared guest handler".into(),
                    ));
                }
                Err(SharedCommitError::VersionMismatch) => continue,
                Err(SharedCommitError::Runtime(error)) => return Err(error),
            }
        }
        Err(ExecError::Unavailable(
            "session-start CAS retry limit exceeded".into(),
        ))
    }

    fn session_start_is_durable(&self, state: &ExecutionState) -> bool {
        state.public().next_step() > 0
            || state.pending_shared().is_some_and(|proposal| {
                proposal.commitment().step == 0
                    && matches!(proposal.entry().event, PublicEvent::SessionStarted { .. })
            })
    }

    async fn emit_session_started(&mut self) -> Result<(), ExecError> {
        if self.session_started_emitted {
            return Ok(());
        }
        self.messages
            .send(SessionMessage::SessionStarted {
                session_id: self.context.activation.session_hash(),
                ensemble: self.ensemble(),
            })
            .await
            .map_err(|_| ExecError::Unavailable("message receiver closed".into()))?;
        self.session_started_emitted = true;
        Ok(())
    }

    /// Apply one public message. The caller supplies an accepted inbox id for
    /// inbound frames; local messages use `None` and the regular proposal path.
    pub(super) async fn apply_message<F: IntoExecutionMessage>(
        &mut self,
        source: arena0_protocol::PeerId,
        frame: F,
        inbox_id: Option<arena0_store::InboxId>,
    ) -> Result<bool, ExecError> {
        let frame = frame.into_execution_message()?;
        for _ in 0..MAX_CAS_RETRIES {
            let state = self.load_state().await?;
            if state.status().is_terminal() {
                return Ok(false);
            }
            if state.pending_shared().is_some() {
                if inbox_id.is_some() {
                    // Public proposals are serialized before inbound frames
                    // are resolved. Keep the accepted frame durable while
                    // the current proposal gathers its signatures; a benign
                    // network reorder must not turn into a session abort.
                    return Ok(false);
                }
                return Err(ExecError::InvalidState(
                    "cannot apply a local message while another public proposal is pending".into(),
                ));
            }
            if frame.sequence > state.public().next_step() {
                return Ok(false);
            }
            if frame.sequence < state.public().next_step()
                || frame.pre_state != state.public().state_hash()
            {
                if let Some(inbox_id) = inbox_id {
                    let _ = self
                        .context
                        .store
                        .reject_inbound(inbox_id, now_ms())
                        .await?;
                }
                return Ok(false);
            }
            let ensemble = self.ensemble();
            if !self.writer_is(source, &state, &ensemble)? {
                if let Some(inbox_id) = inbox_id {
                    let _ = self
                        .context
                        .store
                        .reject_inbound(inbox_id, now_ms())
                        .await?;
                    return Ok(false);
                }
                return Err(ExecError::InvalidState(
                    "local producer is not the guest-selected writer".into(),
                ));
            }
            // The writer projection and the semantic shared call both consume
            // this exact loaded shared snapshot. A retry starts over with a
            // fresh state and a fresh guest instance.
            let raw = Event::MessageReceived {
                message_id: frame.message_id,
                from: source,
                position: frame.sequence,
                pre_state: frame.pre_state,
                msg: frame.data.clone(),
            };
            let shared_event = SharedEvent::MessageReceived {
                message_id: frame.message_id,
                from: source,
                position: frame.sequence,
                pre_state: frame.pre_state,
                msg: frame.data.clone(),
            };
            let result = self.context.program.apply_shared(SharedCall::new(
                state.shared_state().clone(),
                ensemble,
                shared_event,
            ))?;
            match self
                .commit_shared_result(state, raw, result, Some(frame.witness), inbox_id)
                .await
            {
                Ok(committed) => return Ok(committed),
                Err(SharedCommitError::VersionMismatch) => continue,
                Err(SharedCommitError::Runtime(error)) => return Err(error),
            }
        }
        Err(ExecError::Unavailable(
            "shared proposal CAS retry limit exceeded".into(),
        ))
    }

    pub(super) fn writer_is(
        &self,
        source: arena0_protocol::PeerId,
        state: &ExecutionState,
        ensemble: &Ensemble<Committed>,
    ) -> Result<bool, ExecError> {
        let writer = self
            .context
            .program
            .writer(WriterCall::new(
                state.shared_state().clone(),
                ensemble.clone(),
            ))?
            .writer;
        Ok(writer.and_then(|participant| ensemble.peer_at(participant)) == Some(source))
    }

    async fn commit_shared_result(
        &mut self,
        state: ExecutionState,
        raw_event: Event<Vec<u8>>,
        result: arena0_sandbox::SharedCallResult,
        witness: Option<arena0_protocol::WitnessCommitment>,
        inbox_id: Option<arena0_store::InboxId>,
    ) -> Result<bool, SharedCommitError> {
        if result.status == CallStatus::Rejected {
            if let Some(inbox_id) = inbox_id {
                let _ = self
                    .context
                    .store
                    .reject_inbound(inbox_id, now_ms())
                    .await
                    .map_err(ExecError::from)?;
            }
            return Ok(false);
        }
        let public_event = PublicEvent::try_from(raw_event).map_err(|error| {
            ExecError::InvalidState(format!("shared event classification failed: {error}"))
        })?;
        let effects = result
            .observations
            .effects
            .into_iter()
            .map(PublicEffect::try_from)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                ExecError::InvalidState(format!("shared handler emitted local effect: {error}"))
            })?;
        let terminal_outcome = self
            .terminal_outcome(&state, &result.shared, &effects)
            .map_err(ExecError::from)?;
        let entry = TraceEntry {
            trace_version: TRACE_FORMAT_VERSION,
            step: state.public().next_step(),
            event: public_event,
            effects,
            pre_state: state.public().state_hash(),
            post_state: StateHash::of(result.shared.as_bytes()),
            fuel_used: result.observations.fuel_used,
            witness,
            agreement: AggregateAttestation::empty(),
        };
        let delta =
            SharedDelta::new(entry, result.shared, terminal_outcome).map_err(ExecError::from)?;
        let outcome = match inbox_id {
            Some(inbox_id) => self
                .context
                .store
                .apply_inbound_message(inbox_id, delta, now_ms())
                .await
                .map_err(ExecError::from)?,
            None => {
                self.apply_input(ExecutionInput::ProposeShared(delta))
                    .await?
            }
        };
        match outcome {
            ApplyOutcome::Committed(_) => Ok(true),
            ApplyOutcome::AlreadyApplied
            | ApplyOutcome::InboxAlreadyApplied { .. }
            | ApplyOutcome::InboxAlreadyConsumed { .. } => Ok(true),
            ApplyOutcome::VersionMismatch { .. } => Err(SharedCommitError::VersionMismatch),
            ApplyOutcome::Conflict(conflict) => {
                Err(SharedCommitError::Runtime(ExecError::InvalidState(
                    format!("shared proposal conflicts with durable occurrence: {conflict:?}"),
                )))
            }
        }
    }

    fn terminal_outcome(
        &self,
        _state: &ExecutionState,
        shared: &arena0_program::SharedStateBytes,
        effects: &[PublicEffect],
    ) -> Result<Option<TerminalOutcome>, arena0_protocol::ProtocolError> {
        let Some(outcome_bytes) = effects.iter().find_map(|effect| match effect {
            PublicEffect::SessionEnd { outcome } => Some(outcome.as_slice()),
            _ => None,
        }) else {
            return Ok(None);
        };
        let projection = self
            .context
            .program
            .outcome(OutcomeCall::new(shared.clone(), self.ensemble()))
            .map_err(|error| arena0_protocol::ProtocolError::Serialization(error.to_string()))?;
        if projection.borsh.as_bytes() != outcome_bytes {
            return Err(arena0_protocol::ProtocolError::OutcomeProjectionMismatch);
        }
        TerminalOutcome::new(projection.borsh.into_bytes(), projection.json.into_bytes()).map(Some)
    }

    pub(super) async fn run_local(
        &mut self,
        guest_event: LocalEvent,
        trace_event: PrivateEvent,
        cause: PrivateCause,
    ) -> Result<bool, ExecError> {
        let mut replay = None;
        for _ in 0..MAX_CAS_RETRIES {
            let state = self.load_state().await?;
            if state.status().is_terminal() {
                return Ok(false);
            }
            if state.status().pending().is_some() && !matches!(cause, PrivateCause::Resume { .. }) {
                return Ok(false);
            }
            if matches!(cause, PrivateCause::React)
                && state.private().last_reaction_position() == Some(state.public().next_step())
            {
                // The durable cursor is the idempotency boundary for the
                // automatic local reaction. This also protects a caller that
                // races a restart or recovery pass against an already
                // committed reaction.
                return Ok(false);
            }
            let mut call = LocalCall::new(
                self.context.identity.peer_id(),
                state.shared_state().clone(),
                state.local_state().clone(),
                self.ensemble(),
                guest_event.clone(),
            );
            if let Some(random_replay) = replay.clone() {
                call = call.with_random_replay(random_replay);
            }
            let result = self.context.program.apply_local(call)?;
            if result.status == CallStatus::Rejected {
                return Err(ExecError::InvalidState(
                    "local event was rejected without a durable continuation".into(),
                ));
            }
            let private_effects = result
                .observations
                .effects
                .clone()
                .into_iter()
                .map(PrivateEffect::try_from)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| {
                    ExecError::InvalidState(format!("local handler emitted public effect: {error}"))
                })?;
            if private_effects
                .iter()
                .any(|effect| matches!(effect, PrivateEffect::Broadcast { .. }))
                && !self.writer_is(self.context.identity.peer_id(), &state, &self.ensemble())?
            {
                // The private delta has not reached the store yet. Rejecting
                // here therefore leaves both state and outbox unchanged.
                return Err(ExecError::InvalidState(
                    "local producer is not the guest-selected writer".into(),
                ));
            }
            let sequence = state.private().next_record();
            let pending = private_effects
                .iter()
                .position(|effect| {
                    matches!(
                        effect,
                        PrivateEffect::Callout { .. } | PrivateEffect::Sign { .. }
                    )
                })
                .and_then(|index| {
                    PendingRecord::from_effects(
                        arena0_protocol::pending_id(self.context.exec_id, sequence, index),
                        &private_effects,
                    )
                });
            let draws = result.observations.random_draws.clone();
            let replay_draws = draws.clone();
            let record = PrivateRecord {
                seq: sequence,
                after_position: state.public().next_step(),
                event: trace_event.clone(),
                effects: private_effects,
                draws,
                fuel_used: result.observations.fuel_used,
                pending,
            };
            let delta = PrivateDelta::from_record(
                self.context.exec_id,
                record,
                result.local,
                arena0_protocol::execution::PrivateContext::new(now_ms()),
                cause,
            )?;
            let outcome = self
                .context
                .store
                .apply_input(ExecutionInput::Private(delta), now_ms())
                .await?;
            match outcome {
                ApplyOutcome::Committed(_) | ApplyOutcome::AlreadyApplied => {
                    return Ok(true);
                }
                ApplyOutcome::VersionMismatch { .. } => {
                    replay = Some(
                        RandomReplay::new(replay_draws)
                            .map_err(|error| ExecError::InvalidState(error.to_string()))?,
                    );
                    continue;
                }
                ApplyOutcome::Conflict(conflict) => {
                    return Err(ExecError::InvalidState(format!(
                        "private occurrence conflict: {conflict:?}"
                    )));
                }
                ApplyOutcome::InboxAlreadyApplied { .. }
                | ApplyOutcome::InboxAlreadyConsumed { .. } => return Ok(true),
            }
        }
        Err(ExecError::Unavailable(
            "private delta CAS retry limit exceeded".into(),
        ))
    }

    pub(super) async fn fire_due_timers(&mut self) -> Result<(), ExecError> {
        let timers = self
            .context
            .store
            .due_timers(now_ms(), MAX_TIMER_BATCH)
            .await?;
        for timer in timers {
            let firing = TimerFiring::new(timer.timer_id);
            let (guest_event, trace_event) = if timer.payload.is_empty() {
                (LocalEvent::TimerFired, PrivateEvent::TimerFired)
            } else {
                let payload = TimerPayload {
                    // The current store deliberately retains the opaque timer
                    // data. An empty type name preserves that opacity until
                    // the store's typed timer projection is available.
                    type_name: String::new(),
                    data: timer.payload,
                };
                (
                    LocalEvent::TypedTimerFired {
                        timer: payload.clone(),
                    },
                    PrivateEvent::TypedTimerFired { timer: payload },
                )
            };
            let ran = self
                .run_local(guest_event, trace_event, PrivateCause::timer(firing))
                .await?;
            if !ran {
                // A pending callout/signature continuation owns the local
                // guest until it is answered. Leave the due timer durable so
                // the continuation can resume first; the next progress pass
                // will revisit the timer.
                break;
            }
        }
        Ok(())
    }

    pub(super) async fn ensure_step_signature(&mut self) -> Result<(), ExecError> {
        for _ in 0..MAX_CAS_RETRIES {
            let state = self.load_state().await?;
            let Some(proposal) = state.pending_shared() else {
                return Ok(());
            };
            let commitment = proposal.commitment().clone();
            let signature = self.context.execution_key.sign(&commitment.signing_bytes());
            let input = ExecutionInput::StepSignature(ParticipantStepSignature::new(
                self.context.identity.peer_id(),
                commitment.step,
                signature,
            ));
            let outcome = self.apply_input(input).await?;
            self.emit_trace_appended(&outcome).await;
            match outcome {
                ApplyOutcome::Committed(_) | ApplyOutcome::AlreadyApplied => return Ok(()),
                ApplyOutcome::VersionMismatch { .. } => continue,
                ApplyOutcome::Conflict(conflict) => {
                    return Err(ExecError::InvalidState(format!(
                        "step signature conflict: {conflict:?}"
                    )));
                }
                ApplyOutcome::InboxAlreadyApplied { .. }
                | ApplyOutcome::InboxAlreadyConsumed { .. } => return Ok(()),
            }
        }
        Err(ExecError::Unavailable(
            "step signature CAS retry limit exceeded".into(),
        ))
    }

    /// Emit the reliable trace observation only for a commit that advanced the
    /// durable public cursor. Proposal and partial-signature commits carry no
    /// public step in their typed store summary.
    pub(super) async fn emit_trace_appended(&mut self, outcome: &ApplyOutcome) {
        let step = match outcome {
            ApplyOutcome::Committed(summary) => summary.public_step(),
            _ => None,
        };
        let Some(step) = step else {
            return;
        };
        let _ = self
            .messages
            .send(SessionMessage::TraceAppended { step })
            .await;
    }

    pub(super) async fn ensure_terminal_signature(&mut self) -> Result<(), ExecError> {
        for _ in 0..MAX_CAS_RETRIES {
            let state = self.load_state().await?;
            let Some(commitment) = state.pending_terminal().cloned() else {
                return Ok(());
            };
            let signature = self.context.execution_key.sign(&commitment.signing_bytes());
            let input = ExecutionInput::TerminalSignature(ParticipantTerminalSignature::new(
                self.context.identity.peer_id(),
                signature,
            ));
            match self.apply_input(input).await? {
                ApplyOutcome::Committed(_) | ApplyOutcome::AlreadyApplied => return Ok(()),
                ApplyOutcome::VersionMismatch { .. } => continue,
                ApplyOutcome::Conflict(conflict) => {
                    return Err(ExecError::InvalidState(format!(
                        "terminal signature conflict: {conflict:?}"
                    )));
                }
                ApplyOutcome::InboxAlreadyApplied { .. }
                | ApplyOutcome::InboxAlreadyConsumed { .. } => return Ok(()),
            }
        }
        Err(ExecError::Unavailable(
            "terminal signature CAS retry limit exceeded".into(),
        ))
    }
}
