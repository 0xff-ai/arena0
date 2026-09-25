//! Guest dispatches and execution-side agreement validation.
//!
//! One actor owns one resident [`ProgramInstance`]. Every mutating program
//! event enters that instance through `arena0_dispatch`; this module validates
//! the resulting state/effect boundary and hands the complete result to the
//! store's transactional `persist` operation. The resident is
//! never used as durable state: a proposal or an uncertain store reply always
//! restores it from the committed images.

use crate::context::{ExecError, SessionMessage};
use arena0_crypto::{ExecutionKey, NodeKeys, SignScheme};
use arena0_program::{CallStatus, JsonBytes, ProgramHash};
use arena0_protocol::execution::GuestSignData;
use arena0_protocol::{
    Committed, Effect, Ensemble, Event, ExecFrame, ExecLifecycle, ExecutionState, ExecutionStatus,
    ParticipantStepSignature, PeerIdSource, PendingId, SessionHash, StepEvent, TerminalOutcome,
};
use arena0_sandbox::{DispatchCall, GuestSigner, OutcomeCall, QueryCall, ViewCall, WriterCall};
use arena0_store::Change;
use std::sync::Arc;

use super::{ExecutionActor, MAX_TIMER_BATCH, now_ms};

/// The validated durable source of one dispatched event.
#[derive(Debug, Clone)]
pub(super) enum DispatchSource {
    /// A local event with no durable identity beyond its event record.
    Local,
    /// A callout answer that must name the exact open callout.
    Answer(PendingId),
    /// A timer firing that consumes its durable timer identity.
    Timer(arena0_protocol::TimerId),
    /// An authenticated peer message with the author's complete commitment.
    PeerMessage {
        commitment: arena0_protocol::StepCommitment,
    },
    /// This participant's own queued message.
    OwnMessage,
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
/// event to run. `Rejected` discards candidate state and observations. The
/// caller reports an input rejection or ends the session for a writer-message
/// divergence, according to the event being dispatched.
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
        let state = &self.state;
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
                DispatchSource::Answer(pending_id),
            )
            .await
            .map_err(|error| match error {
                ExecError::CalloutNotPending => SubmitInputError::Expected(error),
                _ => SubmitInputError::Fatal(error),
            })?;
        match accepted {
            DispatchOutcome::Committed => self.progress().await?,
            DispatchOutcome::Frozen => {
                // A staged proposal froze the guest. The answer was not
                // consumed; surface that boundary to the caller so it can
                // retry with the same pending id.
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

    pub(super) fn query(&self, query_index: u32, query: JsonBytes) -> Result<JsonBytes, ExecError> {
        let state = &self.state;
        let projection = self.context.program.query(QueryCall::new(
            state.shared_state().clone(),
            self.ensemble(),
            query,
            query_index,
        ))?;
        Ok(projection.output)
    }

    pub(super) fn view(
        &self,
        viewport: JsonBytes,
    ) -> Result<(u64, arena0_protocol::View), ExecError> {
        let state = &self.state;
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
        let state = &self.state;
        if state.status().lifecycle() != ExecLifecycle::Active
            || state.agreed_step() != 0
            || state.pending_shared().is_some()
        {
            if self.session_start_is_durable(state) {
                self.emit_session_started().await?;
            }
            return Ok(());
        }
        let accepted = self
            .dispatch_event(
                Event::SessionStarted {
                    ensemble: self.ensemble(),
                },
                DispatchSource::Local,
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
                state
                    .proposal_commitment()
                    .is_some_and(|commitment| commitment.step == 0)
                    && matches!(proposal.entry().event, StepEvent::SessionStarted { .. })
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
    /// resulting proposal. The producer retains its own broadcast as staged
    /// state and sends it only to remote participants.
    pub(super) async fn apply_message(
        &mut self,
        source: arena0_protocol::PeerId,
        frame: ExecFrame,
    ) -> Result<bool, ExecError> {
        let ExecFrame::Message { commitment, data } = frame else {
            return Err(ExecError::InvalidState(
                "execution frame is not a message".into(),
            ));
        };
        let state = &self.state;
        if state.status().is_terminal() {
            return Ok(false);
        }
        if state.pending_shared().is_some() {
            return Err(ExecError::InvalidState(
                "cannot apply a local message while a shared proposal is pending".into(),
            ));
        }
        let seq = commitment.step;
        if seq > state.agreed_step() {
            return Ok(false);
        }
        if seq < state.agreed_step()
            || commitment.session_id != state.binding().session_id()
            || commitment.pre_state != state.agreed_state()
            || commitment.link != state.agreed_link()
        {
            return Ok(false);
        }
        if self.writer_for_shared(state.shared_state(), &self.ensemble())? != Some(source) {
            return Ok(false);
        }
        let outcome = self
            .dispatch_event(
                Event::MessageReceived {
                    from: source,
                    msg: data,
                },
                DispatchSource::PeerMessage { commitment },
            )
            .await?;
        match outcome {
            DispatchOutcome::Committed => Ok(true),
            DispatchOutcome::Frozen => Ok(false),
            DispatchOutcome::Rejected { reason } => {
                let mut message =
                    format!("diverged at step {seq}: program rejected the writer message");
                if let Some(reason) = reason {
                    message.push_str(": ");
                    message.push_str(&reason);
                }
                Err(ExecError::Diverged(super::truncate_reason(
                    message,
                    arena0_protocol::MAX_TERMINAL_REASON_BYTES,
                )))
            }
        }
    }

    pub(super) fn writer_for_shared(
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

    /// Whether this participant currently owns the next agreed message.
    ///
    /// True only while active, with no staged proposal, a non-empty outgoing
    /// queue, and the `writer` projection selecting this participant.
    pub(super) fn may_author(&self) -> Result<bool, ExecError> {
        let state = &self.state;
        if !matches!(state.status(), ExecutionStatus::Active)
            || state.pending_shared().is_some()
            || state.outgoing().is_empty()
        {
            return Ok(false);
        }
        Ok(
            self.writer_for_shared(state.shared_state(), &self.ensemble())?
                == Some(self.context.identity.peer_id()),
        )
    }

    /// Author the oldest queued message through the same dispatch every
    /// receiver runs, staging a proposal on acceptance.
    ///
    /// A message the program rejects is dropped from the durable queue and the
    /// loop continues with the next one; it never reaches a peer.
    pub(super) async fn author_next_message(&mut self) -> Result<(), ExecError> {
        while self.may_author()? {
            let event = Event::MessageReceived {
                from: self.context.identity.peer_id(),
                msg: self.state.outgoing()[0].clone(),
            };
            match self
                .dispatch_event(event, DispatchSource::OwnMessage)
                .await?
            {
                DispatchOutcome::Committed | DispatchOutcome::Frozen => return Ok(()),
                DispatchOutcome::Rejected { .. } => {
                    let mut next = self.state.clone();
                    next.drop_outgoing_head()?;
                    self.persist(next, Change::DropOutgoing).await?;
                    tracing::error!(
                        exec_id = %self.context.exec_id,
                        "own queued message rejected by the program"
                    );
                }
            }
        }
        Ok(())
    }

    /// Apply one event through the resident dispatch and persist its result.
    ///
    /// Pre-dispatch checks run before the resident is touched, so they
    /// return without discarding any candidate. Everything after the
    /// resident dispatch funnels through the single `discard_candidate`
    /// site below: any error or rejection restores the committed
    /// checkpoint, and a committed dispatch with a staged proposal does
    /// the same. A committed local dispatch instead commits the resident,
    /// so the checkpoint tracks the new state. Persistence failure reloads
    /// the last durable state before any further transition.
    pub(super) async fn dispatch_event(
        &mut self,
        event: Event<Vec<u8>>,
        source: DispatchSource,
    ) -> Result<DispatchOutcome, ExecError> {
        if let Event::InputReceived { callout_index, .. } = &event {
            let DispatchSource::Answer(pending_id) = source else {
                return Err(ExecError::CalloutNotPending);
            };
            if !self
                .state
                .callout()
                .is_some_and(|open| pending_id == open.id && *callout_index == open.callout_index)
            {
                return Err(ExecError::CalloutNotPending);
            }
        }
        if self.state.status().is_terminal() {
            return Ok(DispatchOutcome::Frozen);
        }
        if self.state.pending_shared().is_some() {
            return Ok(DispatchOutcome::Frozen);
        }
        if !matches!(self.state.status(), ExecutionStatus::Active) {
            return Ok(DispatchOutcome::Frozen);
        }

        // The sandbox bounds broadcasts against the committed queue. An
        // own message authors the head it is about to pop, so it is not
        // counted as already committed.
        let outgoing_len = match &source {
            DispatchSource::OwnMessage => self.state.outgoing().len().saturating_sub(1),
            DispatchSource::Local
            | DispatchSource::Answer(_)
            | DispatchSource::Timer(_)
            | DispatchSource::PeerMessage { .. } => self.state.outgoing().len(),
        };
        let outcome = self.dispatch_inner(event, source, outgoing_len).await;
        match outcome {
            Ok(DispatchOutcome::Committed) => {
                if self.state.pending_shared().is_some() {
                    self.discard_candidate()?;
                }
                Ok(DispatchOutcome::Committed)
            }
            Ok(DispatchOutcome::Rejected { reason }) => {
                self.discard_candidate()?;
                Ok(DispatchOutcome::Rejected { reason })
            }
            Ok(DispatchOutcome::Frozen) => Ok(DispatchOutcome::Frozen),
            Err(error) => {
                // Trap paths already restored or replaced the resident, so a
                // missing instance means there is nothing to discard. Every
                // other error leaves candidate state behind and must restore
                // it. A discard failure drops the resident and masks the
                // original error, exactly as the former per-site `?` did.
                if self.instance.is_some() {
                    self.discard_candidate()?;
                }
                Err(error)
            }
        }
    }

    /// Run one checked event through the resident and persist the result.
    ///
    /// The caller owns the single discard site; this function restores the
    /// resident itself only on sandbox trap paths, where the sandbox has
    /// already rolled back and the durable image is reloaded here.
    async fn dispatch_inner(
        &mut self,
        event: Event<Vec<u8>>,
        source: DispatchSource,
        outgoing_len: usize,
    ) -> Result<DispatchOutcome, ExecError> {
        let event_position = self.state.event_position();
        let call = {
            let mut call = DispatchCall::new(
                self.context.identity.peer_id(),
                self.ensemble(),
                event.clone(),
            )
            .with_outgoing_len(outgoing_len);
            // Only local handlers may sign. `SessionStarted` is a
            // pre-session dispatch and `MessageReceived` reproduces a
            // peer's agreed result, so neither is offered a signer.
            if matches!(
                &event,
                Event::InputReceived { .. } | Event::TimerFired { .. }
            ) {
                call = call.with_signer(Arc::new(DispatchSigner {
                    session_id: self.context.activation.session_hash(),
                    program_hash: self.context.program.program().hash(),
                    execution_id: self.context.exec_id,
                    event_position,
                    identity: Arc::clone(&self.context.identity),
                    execution_key: Arc::clone(&self.context.execution_key),
                }));
            }
            call
        };
        let result = match self.resident_mut()?.dispatch(call) {
            Ok(result) => result,
            Err(error) => {
                // ProgramInstance rolls back on guest traps, but dropping
                // the resident also covers a future sandbox error
                // path that cannot prove its own rollback; the next use
                // rebuilds it from the durable images.
                self.instance = None;
                let handler = match &event {
                    Event::InputReceived { .. } => Some("input"),
                    Event::MessageReceived { .. } => Some("message"),
                    _ => None,
                };
                if let Some(handler) = handler {
                    return Ok(DispatchOutcome::Rejected {
                        reason: Some(super::truncate_reason(
                            format!("{handler} handler trapped: {error}"),
                            arena0_program::MAX_REJECTION_REASON_BYTES,
                        )),
                    });
                }
                return Err(error.into());
            }
        };
        if result.status == CallStatus::Rejected {
            return Ok(DispatchOutcome::Rejected {
                reason: result.reason,
            });
        }
        // An accepted dispatch always carries its images; the protocol owns
        // their hash and computes it once in `apply_dispatch`.
        let (shared, local) = match (result.shared, result.local) {
            (Some(shared), Some(local)) => (shared, local),
            _ => {
                return Err(ExecError::InvalidState(
                    "accepted dispatch returned no state images".into(),
                ));
            }
        };
        let effects = result.observations.effects;
        let terminal_outcome = self.terminal_outcome(&shared, &effects)?;
        // Clone late: a rejected dispatch reaches no protocol transition
        // and clones nothing.
        let mut next = self.state.clone();
        if let Err(error) = next.apply_dispatch(
            &event,
            shared,
            local,
            &effects,
            terminal_outcome,
            match source {
                DispatchSource::Answer(pending_id) => Some(pending_id),
                DispatchSource::Local
                | DispatchSource::Timer(_)
                | DispatchSource::PeerMessage { .. }
                | DispatchSource::OwnMessage => None,
            },
            result.callout,
        ) {
            return match (&source, error) {
                // An agreed step whose broadcasts would overflow the local
                // outgoing queue fails the session; the Host does not sign
                // it. The failure boundary records the Host-signed `Fail`.
                (_, arena0_protocol::ProtocolError::OutgoingQueueFull { .. }) => {
                    Err(ExecError::OutgoingQueueOverflow)
                }
                // A local receipt-budget limit is a session-level stop, not
                // a program divergence.
                (_, arena0_protocol::ProtocolError::ReceiptBudgetExhausted { step }) => {
                    Err(ExecError::ReceiptBudgetExhausted { step })
                }
                // A peer message the local program cannot apply is a
                // divergence, not an agent rejection.
                (DispatchSource::PeerMessage { .. }, other) => {
                    Err(ExecError::Diverged(super::truncate_reason(
                        format!("diverged at step {}: {other}", self.state.agreed_step()),
                        arena0_protocol::MAX_TERMINAL_REASON_BYTES,
                    )))
                }
                // A local event or an authored own message is a plain
                // rejection; the caller keeps the session live.
                (
                    DispatchSource::Local
                    | DispatchSource::Answer(_)
                    | DispatchSource::Timer(_)
                    | DispatchSource::OwnMessage,
                    other,
                ) => Ok(DispatchOutcome::Rejected {
                    reason: Some(super::truncate_reason(
                        other.to_string(),
                        arena0_program::MAX_REJECTION_REASON_BYTES,
                    )),
                }),
            };
        }
        // The receiver rebuilds the entry itself; its commitment must equal
        // the author's in every field. Compare before anything is persisted
        // so recovery can never sign an incompatible proposal.
        if let DispatchSource::PeerMessage { commitment } = &source {
            let local = next
                .proposal_commitment()
                .expect("a peer dispatch stages a proposal");
            if local != *commitment {
                let cause = if local.post_state != commitment.post_state {
                    "post-state mismatch"
                } else if local.entry_hash != commitment.entry_hash {
                    "entry mismatch"
                } else if local.link != commitment.link {
                    "link mismatch"
                } else {
                    "commitment mismatch"
                };
                return Err(ExecError::Diverged(super::truncate_reason(
                    format!("diverged at step {}: {cause}", self.state.agreed_step()),
                    arena0_protocol::MAX_TERMINAL_REASON_BYTES,
                )));
            }
        }
        let proposal_staged = next.pending_shared().is_some();
        self.persist(
            next,
            Change::Dispatch {
                event,
                effects,
                timer_id: match &source {
                    DispatchSource::Timer(timer_id) => Some(*timer_id),
                    DispatchSource::Local
                    | DispatchSource::Answer(_)
                    | DispatchSource::PeerMessage { .. }
                    | DispatchSource::OwnMessage => None,
                },
            },
        )
        .await?;
        if !proposal_staged {
            if let Err(error) = self.resident_mut()?.commit() {
                self.instance = None;
                return Err(error.into());
            }
        }
        Ok(DispatchOutcome::Committed)
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
                .dispatch_event(event, DispatchSource::Timer(timer.timer_id))
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
        let Some(proposal) = self.state.pending_shared() else {
            return Ok(());
        };
        if proposal
            .signatures()
            .iter()
            .any(|signature| signature.participant() == self.context.identity.peer_id())
        {
            return Ok(());
        }
        let commitment = self
            .state
            .proposal_commitment()
            .expect("signature needs a staged proposal");
        let signature = ParticipantStepSignature::new(
            self.context.identity.peer_id(),
            commitment.step,
            self.context.execution_key.sign(&commitment.signing_bytes()),
        );
        let mut next = self.state.clone();
        let certified = next.add_step_signature(signature)?;
        let agreed_step = certified.as_ref().map(|proposal| proposal.entry().step);
        self.persist(next, Change::StepSignature { certified })
            .await?;
        if agreed_step.is_some() {
            // Certification replaced the committed images.
            self.reload_resident()?;
        }
        self.emit_trace_appended(agreed_step).await;
        Ok(())
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
}
