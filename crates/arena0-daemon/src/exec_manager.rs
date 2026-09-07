//! Live execution capabilities and their per-execution supervisors.
//!
//! The SQLite store is the only execution authority. The daemon retains an
//! [`ExecutionHandle`] only while an actor or negotiation is live; all public
//! observations are projected from the store on demand. This module does not
//! keep a daemon-side execution aggregate, callout, trace, terminal, receipt,
//! or execution key.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::Context as _;
use arena0_api::{ApiError, ApiErrorCode, AwaitState, ExecLifecycle, NextEvent, PendingId};
use arena0_node::{ExecCommand, LocalTicketWithdrawal, SessionMessage, SpawnedExec};
use arena0_program::{JsonSchemaDocument, ProgramHash, ProgramSchema};
use arena0_protocol::execution::ExecutionState;
use arena0_protocol::{
    AbortKind, EventSource, ExecId, ExecutionStatus, NegotiationId, SessionHash, StopCause, Ticket,
    View,
};
use arena0_sandbox::Program;
use arena0_store::{PendingRequest, StoreHandle};
use arena0_transport::Transport;
use tokio::sync::{Mutex as TokioMutex, mpsc, oneshot, watch};
use tokio::time::Instant;
use tracing::Instrument as _;

use crate::server::{Events, HostEvent};

/// How often to warn while an active session emits no messages.
const STALL_TIMEOUT: Duration = Duration::from_secs(300);
const EXECUTION_STOP_TIMEOUT: Duration = Duration::from_secs(6);
/// Time allowed to finish a selected activation or an exact targeted join.
pub(crate) const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
struct NegotiationHandle {
    offer: watch::Receiver<arena0_protocol::Offer>,
    ticket: watch::Receiver<Option<Ticket>>,
    withdrawals: mpsc::Sender<LocalTicketWithdrawal>,
}

/// One negotiating or running execution's process-local capabilities.
///
/// The store handle is read-only from this type's perspective. The sole
/// execution writer is claimed by the Host and is moved through negotiation
/// into the node actor.
#[derive(Debug)]
pub(crate) struct ExecutionHandle {
    exec_id: ExecId,
    store: StoreHandle,
    cmd_tx: StdMutex<Option<mpsc::Sender<ExecCommand>>>,
    changed: tokio::sync::Notify,
    withdrawal_requested: watch::Sender<bool>,
    negotiation: TokioMutex<Option<NegotiationHandle>>,
}

impl ExecutionHandle {
    pub(crate) fn new(exec_id: ExecId, store: StoreHandle) -> Self {
        let (withdrawal_requested, _) = watch::channel(false);
        Self {
            exec_id,
            store,
            cmd_tx: StdMutex::new(None),
            changed: tokio::sync::Notify::new(),
            withdrawal_requested,
            negotiation: TokioMutex::new(None),
        }
    }

    pub(crate) fn exec_id(&self) -> ExecId {
        self.exec_id
    }

    pub(crate) async fn execution(&self) -> anyhow::Result<Option<ExecutionState>> {
        Ok(self.store.load_execution(self.exec_id).await?)
    }

    pub(crate) async fn request(&self) -> anyhow::Result<Option<arena0_store::ExecutionRequest>> {
        Ok(self.store.load_execution_request(self.exec_id).await?)
    }

    pub(crate) async fn activation(
        &self,
    ) -> anyhow::Result<Option<arena0_store::ActivationRecord>> {
        Ok(self.store.load_activation(self.exec_id).await?)
    }

    pub(crate) async fn negotiation_id(&self) -> anyhow::Result<Option<NegotiationId>> {
        self.request()
            .await?
            .map(|request| request.negotiation_id())
            .context("execution request is missing")
    }

    pub(crate) async fn program_id(&self) -> anyhow::Result<ProgramHash> {
        self.request()
            .await?
            .map(|request| request.program_hash())
            .context("execution request is missing")
    }

    pub(crate) async fn session_id(&self) -> anyhow::Result<Option<SessionHash>> {
        if let Some(state) = self.execution().await? {
            return Ok(Some(state.binding().session_id()));
        }
        Ok(self.activation().await?.map(|record| record.session_id()))
    }

    /// Derive the public lifecycle from durable state, including phases before
    /// an execution aggregate exists.
    pub(crate) async fn lifecycle(&self) -> anyhow::Result<ExecLifecycle> {
        if let Some(state) = self.execution().await? {
            return Ok(state.lifecycle());
        }
        if self
            .request()
            .await?
            .is_some_and(|request| request.failure().is_some())
        {
            return Ok(ExecLifecycle::Failed);
        }
        if self.activation().await?.is_some() {
            return Ok(ExecLifecycle::Activating);
        }
        Ok(ExecLifecycle::Negotiating)
    }

    /// Build the source for a safe daemon event from durable request and
    /// activation facts.
    pub(crate) async fn event_source(&self) -> anyhow::Result<EventSource> {
        let peer_id = self.store.host_id();
        let program_id = self.program_id().await?;
        if let Some(state) = self.execution().await? {
            return Ok(EventSource::Session {
                peer_id,
                exec_id: self.exec_id,
                program_id,
                session_hash: state.binding().session_id(),
            });
        }
        match self.negotiation_id().await? {
            Some(negotiation_id) => Ok(EventSource::Negotiation {
                peer_id,
                exec_id: self.exec_id,
                program_id,
                negotiation_id,
            }),
            // An open Join can fail before it accepts an offer, so there is
            // no negotiation identity to attach to its terminal event.
            None => Ok(EventSource::Execution {
                peer_id,
                exec_id: self.exec_id,
                program_id,
            }),
        }
    }

    pub(crate) async fn await_state(
        &self,
        until: AwaitState,
        deadline: Instant,
    ) -> Result<ExecLifecycle, ApiError> {
        loop {
            let notified = self.changed.notified();
            let lifecycle = self
                .lifecycle()
                .await
                .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
            if satisfies(lifecycle, until) {
                return Ok(lifecycle);
            }
            tokio::select! {
                () = notified => {}
                () = tokio::time::sleep_until(deadline) => {
                    let lifecycle = self
                        .lifecycle()
                        .await
                        .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
                    if satisfies(lifecycle, until) {
                        return Ok(lifecycle);
                    }
                    return Err(ApiError::new(ApiErrorCode::Timeout, "exec.await deadline elapsed"));
                }
            }
        }
    }

    /// Wait for and project the next durable agent-facing event. Signing
    /// continuations remain internal to the actor and are never projected.
    pub(crate) async fn next(&self, schema: &ProgramSchema) -> Result<NextEvent, ApiError> {
        loop {
            let notified = self.changed.notified();
            if let Some(event) = self.next_durable(schema).await? {
                return Ok(event);
            }
            notified.await;
        }
    }

    pub(crate) async fn next_durable(
        &self,
        schema: &ProgramSchema,
    ) -> Result<Option<NextEvent>, ApiError> {
        let state = self
            .execution()
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
        let Some(state) = state else {
            return Ok(None);
        };
        let pending = self
            .store
            .list_pending_requests(self.exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
        project_durable_next(state, pending, schema)
    }

    pub(crate) async fn wait_for_change(&self) {
        self.changed.notified().await;
    }

    pub(crate) fn change_notified(&self) -> impl std::future::Future<Output = ()> + '_ {
        self.changed.notified()
    }

    pub(crate) fn request_withdrawal(&self) {
        self.withdrawal_requested
            .send_modify(|requested| *requested = true);
    }

    pub(crate) fn withdrawal_receiver(&self) -> watch::Receiver<bool> {
        self.withdrawal_requested.subscribe()
    }

    fn running(&self) -> Result<mpsc::Sender<ExecCommand>, ApiError> {
        self.cmd_tx.lock().unwrap().clone().ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::BadRequest,
                "execution is still negotiating (negotiation has not confirmed)",
            )
        })
    }

    pub(crate) async fn submit(
        &self,
        pending_id: PendingId,
        data: arena0_program::JsonBytes,
    ) -> Result<(), ApiError> {
        let callout_index = self
            .store
            .list_pending_requests(self.exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .into_iter()
            .find_map(|request| match request {
                PendingRequest::Callout {
                    pending_id: id,
                    callout_index,
                    ..
                } if id == pending_id => Some(callout_index),
                _ => None,
            })
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::CalloutNotPending,
                    format!("no pending {pending_id}"),
                )
            })?;
        let (reply, rx) = oneshot::channel();
        self.running()?
            .send(ExecCommand::SubmitInput {
                pending_id,
                callout_index,
                data,
                reply,
            })
            .await
            .map_err(|_| ApiError::new(ApiErrorCode::Execution, "execution task gone"))?;
        rx.await
            .map_err(|_| ApiError::new(ApiErrorCode::Execution, "input reply dropped"))?
            .map_err(|error| match error {
                arena0_node::ExecError::CalloutNotPending => ApiError::new(
                    ApiErrorCode::CalloutNotPending,
                    format!("no pending {pending_id}"),
                ),
                error => ApiError::new(ApiErrorCode::Execution, format!("input rejected: {error}")),
            })?;
        self.changed.notify_waiters();
        Ok(())
    }

    pub(crate) async fn install_negotiation(
        &self,
        offer: arena0_protocol::Offer,
        ticket: watch::Sender<Option<Ticket>>,
        withdrawals: mpsc::Sender<LocalTicketWithdrawal>,
    ) -> watch::Sender<arena0_protocol::Offer> {
        let (offer_tx, offer_rx) = watch::channel(offer);
        *self.negotiation.lock().await = Some(NegotiationHandle {
            offer: offer_rx,
            ticket: ticket.subscribe(),
            withdrawals,
        });
        offer_tx
    }

    pub(crate) async fn negotiation_ticket(&self) -> Option<Ticket> {
        self.negotiation
            .lock()
            .await
            .as_ref()
            .and_then(|handle| handle.ticket.borrow().clone())
    }

    pub(crate) async fn negotiation_offer(&self) -> Option<arena0_protocol::Offer> {
        self.negotiation
            .lock()
            .await
            .as_ref()
            .map(|handle| handle.offer.borrow().clone())
    }

    pub(crate) async fn withdraw_negotiation_ticket(
        &self,
        offer: arena0_protocol::Offer,
        ticket: Ticket,
    ) -> Result<(), String> {
        let mut negotiation = self.negotiation.lock().await;
        let handle = negotiation
            .as_mut()
            .ok_or_else(|| "execution does not have an active ticket".to_string())?;
        let previous = handle
            .ticket
            .borrow()
            .clone()
            .ok_or_else(|| "ticket is not ready for withdrawal".to_string())?;
        if ticket.data.revision != previous.data.revision.saturating_add(1)
            || ticket.data.signer != previous.data.signer
            || ticket.data.negotiation_id != previous.data.negotiation_id
            || ticket.data.offer_seq != previous.data.offer_seq
            || offer.data().negotiation_id != ticket.data.negotiation_id
            || offer.data().offer_seq != ticket.data.offer_seq
        {
            return Err("ticket was revised concurrently; retry the withdrawal".to_string());
        }
        let (accepted, result) = oneshot::channel();
        handle
            .withdrawals
            .send(LocalTicketWithdrawal { ticket, accepted })
            .await
            .map_err(|_| "negotiation is no longer accepting withdrawals".to_string())?;
        if !result
            .await
            .map_err(|_| "negotiation ended before accepting the update".to_string())?
        {
            return Err("activation preparation already made the ticket irrevocable".to_string());
        }
        Ok(())
    }

    pub(crate) fn attach_cmd(&self, cmd: mpsc::Sender<ExecCommand>) {
        *self.cmd_tx.lock().unwrap() = Some(cmd);
    }

    pub(crate) async fn query(
        &self,
        query: arena0_program::JsonBytes,
    ) -> Result<arena0_program::JsonBytes, ApiError> {
        let (reply, rx) = oneshot::channel();
        self.running()?
            .send(ExecCommand::Query {
                query_index: 0,
                query,
                reply,
            })
            .await
            .map_err(|_| ApiError::new(ApiErrorCode::Execution, "execution task gone"))?;
        rx.await
            .map_err(|_| ApiError::new(ApiErrorCode::Execution, "query reply dropped"))?
            .map_err(|error| {
                ApiError::new(ApiErrorCode::Execution, format!("query failed: {error}"))
            })
    }

    pub(crate) async fn view(
        &self,
        viewport: arena0_program::JsonBytes,
    ) -> Result<(u64, View), ApiError> {
        let (reply, rx) = oneshot::channel();
        self.running()?
            .send(ExecCommand::View { viewport, reply })
            .await
            .map_err(|_| ApiError::new(ApiErrorCode::Execution, "execution task gone"))?;
        rx.await
            .map_err(|_| ApiError::new(ApiErrorCode::Execution, "view reply dropped"))?
            .map_err(|error| {
                ApiError::new(ApiErrorCode::Execution, format!("view failed: {error}"))
            })
    }

    pub(crate) async fn terminate(&self, reason: String) -> Result<(), ApiError> {
        let (reply, rx) = oneshot::channel();
        self.running()?
            .send(ExecCommand::Terminate { reason, reply })
            .await
            .map_err(|_| ApiError::new(ApiErrorCode::Execution, "execution task gone"))?;
        rx.await
            .map_err(|_| ApiError::new(ApiErrorCode::Execution, "terminate reply dropped"))?
            .map_err(|error| {
                ApiError::new(
                    ApiErrorCode::Execution,
                    format!("terminate failed: {error}"),
                )
            })
    }

    pub(crate) fn notify(&self) {
        self.changed.notify_waiters();
    }
}

fn satisfies(lifecycle: ExecLifecycle, until: AwaitState) -> bool {
    match until {
        AwaitState::Active => {
            matches!(lifecycle, ExecLifecycle::Waiting | ExecLifecycle::Active)
                || lifecycle.is_terminal()
        }
        AwaitState::Terminal => lifecycle.is_terminal(),
    }
}

fn stop_reason(cause: &StopCause) -> String {
    match cause {
        StopCause::Authenticated(occurrence) => occurrence.reason().to_owned(),
        StopCause::Shared { reason, .. } => reason.clone(),
    }
}

fn project_callout(request: PendingRequest, schema: &ProgramSchema) -> anyhow::Result<NextEvent> {
    let PendingRequest::Callout {
        pending_id,
        callout_index,
        context,
        ..
    } = request
    else {
        anyhow::bail!("signing request is daemon-internal")
    };
    let callout = schema
        .callouts
        .get(usize::try_from(callout_index).context("callout index overflow")?)
        .context("callout index out of range")?;
    Ok(NextEvent::Callout {
        pending_id,
        callout_index,
        name: callout.name.clone(),
        prompt: callout.prompt.clone(),
        schema: callout.output.clone(),
        context: serde_json::from_slice(&context).context("decode callout context")?,
    })
}

/// Project one durable execution event without requiring a live actor. This
/// keeps terminal rows and acknowledged callouts observable across a restart.
pub(crate) fn project_durable_next(
    state: ExecutionState,
    pending: Vec<PendingRequest>,
    schema: &ProgramSchema,
) -> Result<Option<NextEvent>, ApiError> {
    if let Some(request) = pending
        .into_iter()
        .find(|request| matches!(request, PendingRequest::Callout { .. }))
    {
        return project_callout(request, schema).map(Some).map_err(|error| {
            ApiError::new(ApiErrorCode::Storage, format!("project callout: {error}"))
        });
    }
    match state.status() {
        ExecutionStatus::Completed { .. } => Ok(Some(NextEvent::Completed {
            session_id: state.binding().session_id(),
            outcome: state
                .terminal_outcome_json()
                .map(serde_json::from_slice)
                .transpose()
                .map_err(|error| {
                    ApiError::new(ApiErrorCode::Storage, format!("decode outcome: {error}"))
                })?,
        })),
        ExecutionStatus::Stopped { cause } | ExecutionStatus::StoppedPublished { cause, .. } => {
            Ok(Some(NextEvent::Failed {
                reason: stop_reason(cause),
            }))
        }
        ExecutionStatus::Incomplete { reason, .. } => Ok(Some(NextEvent::Failed {
            reason: reason.clone(),
        })),
        _ => Ok(None),
    }
}

/// Tracks live execution capabilities and their owned supervisor tasks.
#[derive(Debug)]
pub(crate) struct ExecutionHandles {
    store: StoreHandle,
    execs: StdMutex<HashMap<ExecId, Arc<ExecutionHandle>>>,
    supervisors: StdMutex<HashMap<ExecId, tokio::task::JoinHandle<()>>>,
    stop_signals: StdMutex<HashMap<ExecId, oneshot::Sender<()>>>,
}

/// Owns the live actor and its bounded supervisor loop. Durable execution
/// state is always loaded through `entry.store`; no protocol state is copied.
#[allow(missing_debug_implementations)]
pub(crate) struct Supervisor {
    pub entry: Arc<ExecutionHandle>,
    pub spawned: SpawnedExec,
    pub events: Events,
    pub transport: Arc<dyn Transport + Sync>,
}

impl ExecutionHandles {
    pub(crate) fn new(store: StoreHandle) -> Self {
        Self {
            store,
            execs: StdMutex::new(HashMap::new()),
            supervisors: StdMutex::new(HashMap::new()),
            stop_signals: StdMutex::new(HashMap::new()),
        }
    }

    /// Register one live handle. Registration is idempotent for concurrent
    /// recovery scans; the first owner remains the only process-local actor.
    pub(crate) fn register_live(&self, exec_id: ExecId) -> anyhow::Result<Arc<ExecutionHandle>> {
        if let Some(existing) = self.execs.lock().unwrap().get(&exec_id).cloned() {
            return Ok(existing);
        }
        let entry = Arc::new(ExecutionHandle::new(exec_id, self.store.clone()));
        self.execs
            .lock()
            .unwrap()
            .entry(exec_id)
            .or_insert_with(|| Arc::clone(&entry));
        Ok(self
            .execs
            .lock()
            .unwrap()
            .get(&exec_id)
            .cloned()
            .unwrap_or(entry))
    }

    /// Attach a supervisor after activation has committed and the actor has
    /// been spawned.
    pub(crate) fn attach(self: &Arc<Self>, supervisor: Supervisor) {
        let entry = Arc::clone(&supervisor.entry);
        entry.attach_cmd(supervisor.spawned.cmd_tx.clone());
        let exec_id = entry.exec_id;
        let manager = Arc::clone(self);
        let span = tracing::info_span!("execution_supervisor", %exec_id);
        let (stop_tx, stop_rx) = oneshot::channel();
        self.stop_signals.lock().unwrap().insert(exec_id, stop_tx);
        let task = tokio::spawn(
            async move {
                supervisor.run(STALL_TIMEOUT, stop_rx).await;
                manager.execs.lock().unwrap().remove(&exec_id);
                manager.supervisors.lock().unwrap().remove(&exec_id);
                manager.stop_signals.lock().unwrap().remove(&exec_id);
            }
            .instrument(span),
        );
        self.supervisors.lock().unwrap().insert(exec_id, task);
    }

    /// Signal all actors first, then await every supervisor concurrently under
    /// one global deadline. Handles remain owned until timeout so a remainder
    /// can be explicitly aborted rather than detached.
    pub(crate) async fn stop(&self) {
        let signals = self
            .stop_signals
            .lock()
            .unwrap()
            .drain()
            .map(|(_, signal)| signal)
            .collect::<Vec<_>>();
        for signal in signals {
            let _ = signal.send(());
        }
        let mut tasks = self
            .supervisors
            .lock()
            .unwrap()
            .drain()
            .map(|(_, task)| task)
            .collect::<Vec<_>>();
        let all = futures::future::join_all(tasks.iter_mut());
        if tokio::time::timeout(EXECUTION_STOP_TIMEOUT, all)
            .await
            .is_err()
        {
            for task in &tasks {
                task.abort();
            }
            for task in tasks {
                let _ = task.await;
            }
        }
    }

    pub(crate) fn get(&self, exec_id: &ExecId) -> Option<Arc<ExecutionHandle>> {
        self.execs.lock().unwrap().get(exec_id).cloned()
    }

    pub(crate) fn all_live(&self) -> Vec<Arc<ExecutionHandle>> {
        self.execs.lock().unwrap().values().cloned().collect()
    }

    pub(crate) fn remove(&self, exec_id: &ExecId) {
        self.execs.lock().unwrap().remove(exec_id);
    }
}

impl Supervisor {
    async fn run(mut self, stall: Duration, mut stop: oneshot::Receiver<()>) {
        let initial_session = self.entry.session_id().await.ok().flatten();
        let exec_id = self.entry.exec_id;
        let mut resume = tokio::time::Instant::now() + stall;
        let mut terminal = false;
        loop {
            tokio::select! {
                _ = &mut stop => break,
                event = self.spawned.message_rx.recv() => {
                    let Some(event) = event else { break };
                    resume = tokio::time::Instant::now() + stall;
                    terminal = self.handle_message(event).await || terminal;
                    self.entry.notify();
                    if terminal { break; }
                }
                _ = tokio::time::sleep_until(resume) => {
                    if self.entry.lifecycle().await.is_ok_and(|state| state == ExecLifecycle::Active) {
                        tracing::warn!(%exec_id, secs = stall.as_secs(), "session is stalled; waiting for durable progress");
                    }
                    resume = tokio::time::Instant::now() + stall;
                }
            }
        }
        self.spawned.shutdown().await;
        if let Some(session_id) = initial_session.or(self.entry.session_id().await.ok().flatten())
            && let Err(error) = self.transport.release_session_blobs(session_id).await
        {
            tracing::warn!(%session_id, %error, "failed to release ended session blobs");
        }
    }

    async fn handle_message(&mut self, message: SessionMessage) -> bool {
        match message {
            SessionMessage::SessionStarted { .. } => {
                let Ok(Some(state)) = self.entry.execution().await else {
                    return false;
                };
                let source = EventSource::Session {
                    peer_id: self.entry.store.host_id(),
                    exec_id: self.entry.exec_id,
                    program_id: state.binding().program_hash(),
                    session_hash: state.binding().session_id(),
                };
                self.events.emit(HostEvent::SessionStarted {
                    source,
                    ensemble: state
                        .binding()
                        .activation()
                        .tickets()
                        .iter()
                        .map(|ticket| ticket.data.signer)
                        .collect(),
                });
            }
            SessionMessage::TraceAppended { step } => {
                let Ok(entries) = self
                    .entry
                    .store
                    .read_trace(self.entry.exec_id, step, step.saturating_add(1))
                    .await
                else {
                    return false;
                };
                let Some(entry) = entries.into_iter().next() else {
                    return false;
                };
                let Ok(Some(state)) = self.entry.execution().await else {
                    return false;
                };
                let source = EventSource::Session {
                    peer_id: self.entry.store.host_id(),
                    exec_id: self.entry.exec_id,
                    program_id: state.binding().program_hash(),
                    session_hash: state.binding().session_id(),
                };
                let signers = u16::try_from(entry.agreement.signers.count()).unwrap_or(u16::MAX);
                let participants =
                    u16::try_from(state.binding().activation().tickets().len()).unwrap_or(u16::MAX);
                self.events.emit(HostEvent::SessionStep {
                    source,
                    step: entry.step,
                    pre_state: entry.pre_state,
                    post_state: entry.post_state,
                    fuel_used: entry.fuel_used,
                    signers,
                    participants,
                });
            }
            SessionMessage::CalloutRequested { pending_id, .. } => {
                let request = self
                    .entry
                    .store
                    .list_pending_requests(self.entry.exec_id)
                    .await
                    .ok()
                    .and_then(|requests| {
                        requests.into_iter().find(|request| {
                            request.pending_id() == pending_id
                                && matches!(request, PendingRequest::Callout { .. })
                        })
                    });
                if let Some(request) = request
                    && let Ok(Some(callout)) = self.project_callout(request).await
                {
                    self.events.emit(HostEvent::SessionCallout {
                        source: callout.0,
                        pending_id: callout.1,
                        callout_index: callout.2,
                        name: callout.3,
                        prompt: callout.4,
                        schema: callout.5,
                        context: callout.6,
                    });
                }
            }
            SessionMessage::Completed { .. } => {
                if let Ok(Some(state)) = self.entry.execution().await
                    && matches!(state.status(), ExecutionStatus::Completed { .. })
                {
                    let source = EventSource::Session {
                        peer_id: self.entry.store.host_id(),
                        exec_id: self.entry.exec_id,
                        program_id: state.binding().program_hash(),
                        session_hash: state.binding().session_id(),
                    };
                    let outcome = state
                        .terminal_outcome_json()
                        .and_then(|json| serde_json::from_slice(json).ok());
                    self.events
                        .emit(HostEvent::SessionCompleted { source, outcome });
                    return true;
                }
            }
            SessionMessage::Aborted { .. } => {
                if let Ok(Some(state)) = self.entry.execution().await {
                    let source = EventSource::Session {
                        peer_id: self.entry.store.host_id(),
                        exec_id: self.entry.exec_id,
                        program_id: state.binding().program_hash(),
                        session_hash: state.binding().session_id(),
                    };
                    if let Some(cause) = state.status().terminal_cause() {
                        let step = match cause {
                            StopCause::Authenticated(occurrence) => {
                                occurrence.coordinate().next_step()
                            }
                            StopCause::Shared { commitment, .. } => commitment.step,
                        };
                        self.events.emit(HostEvent::SessionAborted {
                            source,
                            step,
                            reason: stop_reason(cause),
                            failure: if cause.kind() == AbortKind::Abort {
                                arena0_protocol::ExecutionFailureCode::ProgramAborted
                            } else {
                                arena0_protocol::ExecutionFailureCode::Runtime
                            },
                        });
                        return true;
                    }
                }
            }
            SessionMessage::Failed { reason } => {
                if let Ok(Some(state)) = self.entry.execution().await {
                    let source = self.entry.event_source().await.ok();
                    if state.lifecycle() == ExecLifecycle::Aborted {
                        if let Some(source) = source {
                            self.events.emit(HostEvent::SessionAborted {
                                source,
                                step: state.public().next_step().saturating_sub(1),
                                reason,
                                failure: arena0_protocol::ExecutionFailureCode::ProgramAborted,
                            });
                        }
                    } else if let Some(source) = source {
                        self.events.emit(HostEvent::Failed {
                            source,
                            reason,
                            failure: arena0_protocol::ExecutionFailureCode::Runtime,
                        });
                    }
                    return state.lifecycle().is_terminal();
                }
            }
            SessionMessage::ReceiptPublished { .. } | SessionMessage::Notification { .. } => {}
        }
        false
    }

    async fn project_callout(
        &mut self,
        request: PendingRequest,
    ) -> anyhow::Result<
        Option<(
            EventSource,
            PendingId,
            u32,
            String,
            String,
            JsonSchemaDocument,
            serde_json::Value,
        )>,
    > {
        let program_id = self.entry.program_id().await?;
        let Some(stored) = self.entry.store.load_program(program_id).await? else {
            return Ok(None);
        };
        let program =
            Program::try_from(stored.wasm().to_vec()).context("parse execution program")?;
        let event = project_callout(request, &program.definition().schema)?;
        let NextEvent::Callout {
            pending_id,
            callout_index,
            name,
            prompt,
            schema,
            context,
        } = event
        else {
            return Ok(None);
        };
        let source = self.entry.event_source().await?;
        Ok(Some((
            source,
            pending_id,
            callout_index,
            name,
            prompt,
            schema,
            context,
        )))
    }
}
