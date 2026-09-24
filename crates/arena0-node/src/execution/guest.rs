//! Guest dispatches and execution-side agreement validation.
//!
//! One actor owns one resident [`ProgramInstance`]. Every mutating program
//! event enters that instance through `arena0_dispatch`; this module validates
//! the resulting state/effect boundary and hands the complete result to the
//! store's one transactional `commit_dispatch` operation. The resident is
//! never used as durable state: a proposal or an uncertain store reply always
//! restores it from the committed images.

use crate::context::{ExecError, SessionMessage};
use arena0_crypto::{ExecutionKey, NodeKeys, SignScheme};
use arena0_program::{CallStatus, JsonBytes, ProgramHash};
use arena0_protocol::execution::GuestSignData;
use arena0_protocol::{
    Committed, Effect, Ensemble, Event, ExecFrame, ExecLifecycle, ExecutionState, ExecutionStatus,
    MessageId, ParticipantStepSignature, ParticipantTerminalSignature, PeerIdSource, PendingId,
    SessionHash, StateHash, TerminalOutcome,
};
use arena0_sandbox::{
    DispatchCall, GuestSigner, OutcomeCall, QueryCall, RandomReplay, ViewCall, WriterCall,
};
use arena0_store::{ApplyOutcome, InboxId};
use std::sync::Arc;

use super::{ExecutionActor, MAX_CAS_RETRIES, MAX_TIMER_BATCH, now_ms};

/// Durable identities owned by the source of an event. The store validates
/// that only the applicable identity is present and that it matches its
/// authoritative inbox, timer, or committed open callout.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct DispatchSource {
    pub(super) inbox_id: Option<InboxId>,
    pub(super) timer_id: Option<arena0_protocol::TimerId>,
    pub(super) pending_id: Option<PendingId>,
    pub(super) advertised_post_state: Option<StateHash>,
}

/// Classification for the agent-facing input command.
///
/// A callout can become unavailable for an expected protocol reason (for
/// example, a proposal froze the execution or another answer consumed the
/// continuation), in which case the command reports the error and the actor
/// remains live. An input-handler trap is also an expected input rejection;
/// traps from other guest events, invalid durable state, and store failures
/// remain fatal execution errors and return to `run` so its normal failure
/// boundary can authenticate and persist the stop.
#[derive(Debug)]
pub(super) enum SubmitInputError {
    Expected(ExecError),
    Fatal(ExecError),
}

/// Result of attempting to dispatch one event.
///
/// `Frozen` means that the durable execution boundary did not allow the
/// event to run. `Rejected` is a guest-level rejection: its candidate state
/// and observations are discarded, while the actor and the durable pending
/// continuation remain live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DispatchOutcome {
    Committed,
    Frozen,
    Rejected { reason: Option<String> },
}

impl From<ExecError> for SubmitInputError {
    fn from(error: ExecError) -> Self {
        Self::Fatal(error)
    }
}

/// Per-dispatch signer handed to the guest's synchronous `sign` import.
///
/// It owns the execution-bound preimage construction so the guest receives the
/// exact bytes that were signed, and it can only be reached through the import
/// a local handler dispatch installed it for.
struct DispatchSigner {
    session_id: SessionHash,
    program_hash: ProgramHash,
    execution_id: arena0_protocol::ExecId,
    event_position: u64,
    identity: Arc<NodeKeys>,
    execution_key: Arc<ExecutionKey>,
}

impl GuestSigner for DispatchSigner {
    fn sign(
        &self,
        call_index: u32,
        scheme: SignScheme,
        payload: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>), String> {
        let data = GuestSignData::new(
            self.session_id,
            self.program_hash,
            self.execution_id,
            self.event_position,
            call_index,
            scheme,
            payload.to_vec(),
        )
        .map_err(|error| error.to_string())?;
        let signing_bytes = data.signing_bytes().map_err(|error| error.to_string())?;
        let signature = match scheme {
            SignScheme::Ed25519 => self.identity.sign(&signing_bytes).0.to_vec(),
            SignScheme::Bls => self.execution_key.sign(&signing_bytes).0.to_vec(),
        };
        Ok((signing_bytes, signature))
    }
}

impl ExecutionActor {
    /// Submit the answer to the current callout continuation. A rejected
    /// guest event leaves the continuation and both durable memories intact;
    /// the command reports that rejection without taking the actor down.
    pub(super) async fn submit_input(
        &mut self,
        pending_id: PendingId,
        data: JsonBytes,
    ) -> Result<(), SubmitInputError> {
        let state = self.load_state().await?;
        let Some(open) = state.callout() else {
            return Err(SubmitInputError::Expected(ExecError::CalloutNotPending));
        };
        if open.id != pending_id {
            return Err(SubmitInputError::Expected(ExecError::CalloutNotPending));
        }
        let callout_index = open.callout_index;
        let accepted = self
            .dispatch_event(
                Event::InputReceived {
                    callout_index,
                    data: data.into_bytes(),
                },
                DispatchSource {
                    pending_id: Some(pending_id),
                    ..DispatchSource::default()
                },
            )
            .await
            .map_err(|error| match error {
                ExecError::CalloutNotPending => SubmitInputError::Expected(error),
                _ => SubmitInputError::Fatal(error),
            })?;
        match accepted {
            DispatchOutcome::Committed => self.progress().await?,
            DispatchOutcome::Frozen => {
                // A deferred broadcast may have left a successor proposal in
                // flight while this continuation is still pending. The
                // answer was not consumed; surface that boundary to the
                // caller so it can retry with the same pending id.
                return Err(SubmitInputError::Expected(ExecError::AgreementPending));
            }
            DispatchOutcome::Rejected { reason } => {
                return Err(SubmitInputError::Expected(ExecError::InputRejected(
                    reason.unwrap_or_else(|| "input handler rejected the answer".into()),
                )));
            }
        }
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
        Ok((state.agreed_step(), view))
    }

    pub(super) async fn ensure_session_started(&mut self) -> Result<(), ExecError> {
        let state = self.load_state().await?;
        if state.status().lifecycle() != ExecLifecycle::Active
            || state.agreed_step() != 0
            || state.pending_shared().is_some()
        {
            if self.session_start_is_durable(&state) {
                self.emit_session_started().await?;
            }
            return Ok(());
        }
        let accepted = self
            .dispatch_event(
                Event::SessionStarted {
                    ensemble: self.ensemble(),
                },
                DispatchSource::default(),
            )
            .await?;
        if !matches!(accepted, DispatchOutcome::Committed) {
            return Err(ExecError::InvalidState(
                "session start was rejected by the guest handler".into(),
            ));
        }
        self.emit_session_started().await
    }

    fn session_start_is_durable(&self, state: &ExecutionState) -> bool {
        state.agreed_step() > 0
            || state.pending_shared().is_some_and(|proposal| {
                proposal.commitment().step == 0
                    && matches!(proposal.entry().event, Event::SessionStarted { .. })
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

    /// Apply one authenticated or locally generated message envelope. The
    /// receiver checks both advertised frame hashes before it can sign the
    /// resulting proposal; the producer never calls this method for its own
    /// broadcast outbox row.
    pub(super) async fn apply_message(
        &mut self,
        source: arena0_protocol::PeerId,
        frame: ExecFrame,
        inbox_id: Option<InboxId>,
    ) -> Result<bool, ExecError> {
        let ExecFrame::Message {
            message_id,
            seq,
            prestate,
            data,
            poststate,
        } = frame
        else {
            return Err(ExecError::InvalidState(
                "execution frame is not a message".into(),
            ));
        };
        let state = self.load_state().await?;
        if state.status().is_terminal() {
            return Ok(false);
        }
        if state.pending_shared().is_some() {
            if inbox_id.is_some() {
                // Keep the accepted inbox row pending while the current
                // proposal gathers N-of-N signatures.
                return Ok(false);
            }
            return Err(ExecError::InvalidState(
                "cannot apply a local message while a shared proposal is pending".into(),
            ));
        }
        if seq > state.agreed_step() {
            return Ok(false);
        }
        if seq < state.agreed_step()
            || prestate != state.agreed_state()
            || MessageId::derive(
                state.binding().session_id(),
                source,
                seq,
                prestate,
                poststate,
                &data,
            ) != message_id
        {
            if let Some(inbox_id) = inbox_id {
                self.reject_inbound(inbox_id).await?;
            }
            return Ok(false);
        }
        if !self.writer_is(source, &state, &self.ensemble())? {
            if let Some(inbox_id) = inbox_id {
                self.reject_inbound(inbox_id).await?;
                return Ok(false);
            }
            return Err(ExecError::InvalidState(
                "message source is not the guest-selected writer".into(),
            ));
        }
        let accepted = match self
            .dispatch_event(
                Event::MessageReceived {
                    message_id,
                    from: source,
                    position: seq,
                    pre_state: prestate,
                    msg: data,
                },
                DispatchSource {
                    inbox_id,
                    advertised_post_state: Some(poststate),
                    ..DispatchSource::default()
                },
            )
            .await
        {
            Ok(accepted) => accepted,
            Err(ExecError::InvalidState(message))
                if message == "message dispatch did not reproduce the advertised post-state" =>
            {
                if let Some(inbox_id) = inbox_id {
                    self.reject_inbound(inbox_id).await?;
                    return Ok(false);
                }
                return Err(ExecError::InvalidState(message));
            }
            Err(error) => return Err(error),
        };
        if matches!(accepted, DispatchOutcome::Rejected { .. })
            && let Some(inbox_id) = inbox_id
        {
            self.reject_inbound(inbox_id).await?;
        }
        Ok(matches!(accepted, DispatchOutcome::Committed))
    }

    pub(super) fn writer_is(
        &self,
        source: arena0_protocol::PeerId,
        state: &ExecutionState,
        ensemble: &Ensemble<Committed>,
    ) -> Result<bool, ExecError> {
        Ok(self.writer_for_shared(state.shared_state(), ensemble)? == Some(source))
    }

    fn writer_for_shared(
        &self,
        shared: &arena0_program::SharedStateBytes,
        ensemble: &Ensemble<Committed>,
    ) -> Result<Option<arena0_protocol::PeerId>, ExecError> {
        let writer = self
            .context
            .program
            .writer(WriterCall::new(shared.clone(), ensemble.clone()))?
            .writer;
        Ok(writer.and_then(|participant| ensemble.peer_at(participant)))
    }

    /// Dispatch one flat event and persist the complete result. A frozen
    /// boundary leaves the event unconsumed, while a guest rejection discards
    /// the candidate and preserves the durable continuation. A compare-and-
    /// set mismatch reloads the resident and retries with the recorded random
    /// draws from the first invocation.
    pub(super) async fn dispatch_event(
        &mut self,
        event: Event<Vec<u8>>,
        source: DispatchSource,
    ) -> Result<DispatchOutcome, ExecError> {
        let mut replay = None;
        for _ in 0..MAX_CAS_RETRIES {
            let state = self.load_state().await?;
            if let Event::InputReceived { callout_index, .. } = &event
                && !state.callout().is_some_and(|open| {
                    source.pending_id == Some(open.id) && *callout_index == open.callout_index
                })
            {
                self.discard_candidate()?;
                return Err(ExecError::CalloutNotPending);
            }
            if state.status().is_terminal() {
                return Ok(DispatchOutcome::Frozen);
            }
            if state.pending_shared().is_some() {
                self.discard_candidate()?;
                return Ok(DispatchOutcome::Frozen);
            }
            if !matches!(state.status(), ExecutionStatus::Active) {
                self.discard_candidate()?;
                return Ok(DispatchOutcome::Frozen);
            }
            self.reconcile_resident(&state)?;

            let call = {
                let mut call = DispatchCall::new(
                    self.context.identity.peer_id(),
                    self.ensemble(),
                    event.clone(),
                );
                if let Some(replay) = replay.clone() {
                    call = call.with_random_replay(replay);
                }
                // Only local handlers may sign. `SessionStarted` is a
                // pre-session dispatch and `MessageReceived` reproduces a
                // peer's agreed result, so neither is offered a signer.
                if matches!(
                    &event,
                    Event::InputReceived { .. } | Event::TimerFired { .. } | Event::React
                ) {
                    call = call.with_signer(Arc::new(DispatchSigner {
                        session_id: self.context.activation.session_hash(),
                        program_hash: self.context.program.program().hash(),
                        execution_id: self.context.exec_id,
                        event_position: state.event_position(),
                        identity: Arc::clone(&self.context.identity),
                        execution_key: Arc::clone(&self.context.execution_key),
                    }));
                }
                call
            };
            let result = match self.resident_mut()?.dispatch(call) {
                Ok(result) => result,
                Err(error) => {
                    // ProgramInstance rolls back on guest traps, but loading
                    // the durable image also covers a future sandbox error
                    // path that cannot prove its own rollback.
                    self.instance = None;
                    self.restore_resident(&state)?;
                    if matches!(&event, Event::InputReceived { .. }) {
                        return Ok(DispatchOutcome::Rejected {
                            reason: Some(super::truncate_reason(
                                format!("input handler trapped: {error}"),
                                arena0_program::MAX_REJECTION_REASON_BYTES,
                            )),
                        });
                    }
                    return Err(error.into());
                }
            };
            if result.status == CallStatus::Rejected {
                self.discard_candidate()?;
                return Ok(DispatchOutcome::Rejected {
                    reason: result.reason,
                });
            }

            let candidate_hash = StateHash::of_shared(&result.shared);
            if candidate_hash != StateHash(result.shared_hash) {
                self.discard_candidate()?;
                return Err(ExecError::InvalidState(
                    "sandbox shared-state hash does not match its payload".into(),
                ));
            }
            if source
                .advertised_post_state
                .is_some_and(|post_state| post_state != candidate_hash)
            {
                self.discard_candidate()?;
                return Err(ExecError::InvalidState(
                    "message dispatch did not reproduce the advertised post-state".into(),
                ));
            }

            let agreed_event = matches!(
                &event,
                Event::SessionStarted { .. } | Event::MessageReceived { .. }
            );
            let candidate_shared = result.shared.clone();
            let effects = result.observations.effects;
            let broadcast_count = effects
                .iter()
                .filter(|effect| matches!(effect, Effect::Broadcast { .. }))
                .count();
            let lifecycle_count = effects
                .iter()
                .filter(|effect| {
                    matches!(
                        effect,
                        Effect::SessionEnd { .. }
                            | Effect::SessionAbort { .. }
                            | Effect::Fail { .. }
                    )
                })
                .count();
            let shared_changed = candidate_hash != state.agreed_state();
            if broadcast_count > 1 || lifecycle_count > 1 {
                self.discard_candidate()?;
                return Err(ExecError::InvalidState(
                    "dispatch emitted too many agreement or lifecycle effects".into(),
                ));
            }
            if agreed_event && broadcast_count != 0 && lifecycle_count != 0 {
                // The broadcast would begin a second position after this
                // terminal step, which has no successor to host it.
                self.discard_candidate()?;
                return Err(ExecError::InvalidState(
                    "terminal agreed event cannot defer a broadcast".into(),
                ));
            }
            if (shared_changed || lifecycle_count != 0) && !agreed_event && broadcast_count == 0 {
                self.discard_candidate()?;
                return Err(ExecError::InvalidState(
                    "local shared or lifecycle mutation requires a broadcast".into(),
                ));
            }
            if broadcast_count != 0 {
                // A broadcast is an agreement boundary, not an arbitrary
                // local effect. For a local event, the current committed
                // state selects its author. A broadcast observed while
                // applying an already-agreed event is deferred to the next
                // position, so the post-dispatch state selects that successor
                // author. This check runs before the store can stage anything.
                let writer_state = if agreed_event {
                    &candidate_shared
                } else {
                    state.shared_state()
                };
                let writer = match self.writer_for_shared(writer_state, &self.ensemble()) {
                    Ok(writer) => writer,
                    Err(error) => {
                        self.discard_candidate()?;
                        return Err(error);
                    }
                };
                if writer != Some(self.context.identity.peer_id()) {
                    self.discard_candidate()?;
                    return Err(ExecError::InvalidState(
                        "broadcast was emitted by a participant that is not the selected writer"
                            .into(),
                    ));
                }
            }
            let terminal_outcome = match self.terminal_outcome(&result.shared, &effects) {
                Ok(outcome) => outcome,
                Err(error) => {
                    self.discard_candidate()?;
                    return Err(error);
                }
            };
            let random_draws = result.observations.random_draws;
            let candidate_local = result.local.clone();
            let outcome = self
                .context
                .store
                .commit_dispatch(
                    state.version(),
                    event.clone(),
                    result.shared,
                    result.local,
                    effects,
                    terminal_outcome,
                    source.inbox_id,
                    source.timer_id,
                    source.pending_id,
                    result.callout,
                    now_ms(),
                )
                .await;
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => {
                    // Store reply loss is an unknown outcome. Never continue
                    // with candidate Wasm memories after that boundary.
                    self.restore_after_store_error().await;
                    return Err(error.into());
                }
            };
            match outcome {
                ApplyOutcome::Committed {
                    agreed_step,
                    proposal_staged,
                } => {
                    if proposal_staged {
                        self.discard_candidate()?;
                    } else {
                        let (shared, local) = match self.resident_mut()?.commit_payloads() {
                            Ok(payloads) => payloads,
                            Err(error) => {
                                self.instance = None;
                                self.reload_resident().await?;
                                return Err(error.into());
                            }
                        };
                        if shared != candidate_shared || local != candidate_local {
                            self.reload_resident().await?;
                            return Err(ExecError::InvalidState(
                                "store committed payloads differ from the resident dispatch result"
                                    .into(),
                            ));
                        }
                    }
                    self.emit_trace_appended(agreed_step).await;
                    return Ok(DispatchOutcome::Committed);
                }
                ApplyOutcome::AlreadyApplied
                | ApplyOutcome::InboxAlreadyApplied { .. }
                | ApplyOutcome::InboxAlreadyConsumed { .. } => {
                    self.discard_candidate()?;
                    self.reload_resident().await?;
                    return Ok(DispatchOutcome::Committed);
                }
                ApplyOutcome::VersionMismatch { .. } => {
                    replay = Some(match RandomReplay::new(random_draws) {
                        Ok(replay) => replay,
                        Err(error) => {
                            self.discard_candidate()?;
                            return Err(ExecError::InvalidState(error.to_string()));
                        }
                    });
                    self.discard_candidate()?;
                    self.reload_resident().await?;
                }
            }
        }
        Err(ExecError::Unavailable(
            "dispatch CAS retry limit exceeded".into(),
        ))
    }

    /// Discard an uncommitted candidate. If the resident cannot restore its
    /// checkpoint, drop it so the next actor operation must instantiate from
    /// the durable state rather than reusing a poisoned Wasm instance.
    fn discard_candidate(&mut self) -> Result<(), ExecError> {
        let restored = self
            .resident_mut()?
            .restore_committed()
            .map_err(ExecError::from);
        if restored.is_err() {
            self.instance = None;
        }
        restored
    }

    pub(super) async fn reload_resident(&mut self) -> Result<(), ExecError> {
        let state = self.load_state().await?;
        self.reconcile_resident(&state)
    }

    fn reconcile_resident(&mut self, state: &ExecutionState) -> Result<(), ExecError> {
        if self.instance.as_ref().is_some_and(|instance| {
            instance.committed_payloads().0 == state.shared_state()
                && instance.committed_payloads().1 == state.local_state()
        }) {
            return Ok(());
        }
        if let Some(instance) = self.instance.as_mut() {
            let restored = instance
                .restore_payloads(state.shared_state().clone(), state.local_state().clone())
                .map_err(ExecError::from);
            if restored.is_err() {
                self.instance = None;
            }
            restored
        } else {
            self.restore_resident(state)
        }
    }

    pub(super) async fn restore_after_store_error(&mut self) {
        self.instance = None;
        if let Ok(Some(state)) = self.context.store.load_execution().await
            && let Err(error) = self.reconcile_resident(&state)
        {
            tracing::error!(
                exec_id = %self.context.exec_id,
                %error,
                "unable to rebuild resident after unknown store outcome"
            );
            self.instance = None;
        }
    }

    fn terminal_outcome(
        &self,
        shared: &arena0_program::SharedStateBytes,
        effects: &[Effect],
    ) -> Result<Option<TerminalOutcome>, ExecError> {
        let Some(outcome_bytes) = effects.iter().find_map(|effect| match effect {
            Effect::SessionEnd { outcome } => Some(outcome.as_slice()),
            _ => None,
        }) else {
            return Ok(None);
        };
        let projection = self
            .context
            .program
            .outcome(OutcomeCall::new(shared.clone(), self.ensemble()))?;
        if projection.borsh.as_bytes() != outcome_bytes {
            return Err(ExecError::InvalidState(
                "SessionEnd outcome differs from the guest outcome projection".into(),
            ));
        }
        Ok(Some(TerminalOutcome::new(
            projection.borsh.into_bytes(),
            projection.json.into_bytes(),
        )?))
    }

    pub(super) async fn fire_due_timers(&mut self) -> Result<(), ExecError> {
        let timers = self
            .context
            .store
            .due_timers(now_ms(), MAX_TIMER_BATCH)
            .await?;
        for timer in timers {
            let event = Event::TimerFired { timer: timer.timer };
            let accepted = self
                .dispatch_event(
                    event,
                    DispatchSource {
                        timer_id: Some(timer.timer_id),
                        ..DispatchSource::default()
                    },
                )
                .await?;
            if matches!(accepted, DispatchOutcome::Frozen) {
                // A staged proposal or terminal boundary freezes the guest; leave
                // the timer durable for the next progress pass.
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
            let signature = ParticipantStepSignature::new(
                self.context.identity.peer_id(),
                proposal.commitment().step,
                self.context
                    .execution_key
                    .sign(&proposal.commitment().signing_bytes()),
            );
            let outcome = self
                .context
                .store
                .commit_step_signature(state.version(), signature, None, now_ms())
                .await;
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => {
                    self.restore_after_store_error().await;
                    return Err(error.into());
                }
            };
            match outcome {
                ApplyOutcome::Committed { agreed_step, .. } => {
                    self.reload_resident().await?;
                    self.emit_trace_appended(agreed_step).await;
                    return Ok(());
                }
                ApplyOutcome::AlreadyApplied
                | ApplyOutcome::InboxAlreadyApplied { .. }
                | ApplyOutcome::InboxAlreadyConsumed { .. } => {
                    self.reload_resident().await?;
                    return Ok(());
                }
                ApplyOutcome::VersionMismatch { .. } => {
                    self.reload_resident().await?;
                }
            }
        }
        Err(ExecError::Unavailable(
            "step signature CAS retry limit exceeded".into(),
        ))
    }

    pub(super) async fn emit_trace_appended(&mut self, step: Option<u64>) {
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
            let signature = ParticipantTerminalSignature::new(
                self.context.identity.peer_id(),
                self.context.execution_key.sign(&commitment.signing_bytes()),
            );
            let outcome = self
                .context
                .store
                .commit_terminal_signature(state.version(), signature, None, now_ms())
                .await;
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => {
                    self.restore_after_store_error().await;
                    return Err(error.into());
                }
            };
            match outcome {
                ApplyOutcome::Committed { .. } | ApplyOutcome::AlreadyApplied => return Ok(()),
                ApplyOutcome::InboxAlreadyApplied { .. }
                | ApplyOutcome::InboxAlreadyConsumed { .. } => return Ok(()),
                ApplyOutcome::VersionMismatch { .. } => {}
            }
        }
        Err(ExecError::Unavailable(
            "terminal signature CAS retry limit exceeded".into(),
        ))
    }
}
