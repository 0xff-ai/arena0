//! Actor lifecycle, recovery, and serialized command dispatch.
//!
//! This module owns task startup and the actor loop. Guest transitions,
//! frame classification and delivery, and terminal boundaries live in their
//! respective modules and extend the same private actor state owner.

use std::collections::HashMap;
use std::sync::Arc;

use arena0_protocol::{
    Committed, Ensemble, ExecLifecycle, ExecutionState, PeerIdSource, ReceiptWork, TicketAction,
};
use arena0_sandbox::ProgramInstance;
use arena0_store::{Change, TransitionRecord};
use arena0_transport::RecvHandle;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Interval, MissedTickBehavior};

use crate::Host;
use crate::context::{ActorContext, ExecCommand, ExecError, InboundStreamPayload, SpawnedExec};

use super::guest::SubmitInputError;
use super::{COMMAND_CAPACITY, ExecutionActor, PROGRESS_INTERVAL, STREAM_CAPACITY, now_ms};

/// Spawn one actor and one concurrent reader supervisor.
#[must_use]
pub(crate) fn spawn_execution(context: ActorContext, host: Arc<Host>) -> SpawnedExec {
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (message_tx, message_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (stream_tx, stream_rx) = mpsc::channel(STREAM_CAPACITY);

    let actor_task = tokio::spawn(async move {
        let mut context = context;
        let (state, startup_error) = match ExecutionActor::ensure_execution(&mut context).await {
            Ok(state) => (state, None),
            Err(error) => {
                // An existing execution still owns durable frame obligations.
                // Keep the actor alive to publish and drain them on failure.
                match context.store.load_execution().await {
                    Ok(Some(state)) => (state, Some(error)),
                    result => {
                        let reason = super::truncate_reason(
                            error.to_string(),
                            arena0_protocol::MAX_TERMINAL_REASON_BYTES,
                        );
                        let recorded = match result {
                            Ok(None) => context
                                .store
                                .record_execution_request_failure(reason.clone())
                                .await
                                .map(|_| ()),
                            Err(error) => Err(error),
                            Ok(Some(_)) => unreachable!(),
                        };
                        if let Err(error) = recorded {
                            tracing::error!(exec_id = %context.exec_id, %error, "unable to record startup failure");
                        }
                        let _ = message_tx
                            .send(crate::context::SessionMessage::Failed { reason })
                            .await;
                        return;
                    }
                }
            }
        };
        let end_deadline = tokio::time::Instant::now() + context.end_confirmation_window;
        let actor = ExecutionActor {
            context,
            state,
            instance: None,
            messages: message_tx,
            send_lanes: HashMap::new(),
            send_tasks: JoinSet::new(),
            end_deadline,
            session_started_emitted: false,
            terminal_emitted: false,
            announced_callout: None,
        };
        actor.run(command_rx, startup_error).await;
    });
    let stream_task = tokio::spawn(forward_streams(stream_rx, command_tx.clone()));

    SpawnedExec::new(
        host,
        command_tx,
        message_rx,
        stream_tx,
        crate::context::ExecutionTask::new(actor_task, stream_task),
    )
}

/// Read every accepted stream concurrently and forward authenticated
/// deliveries into the one actor queue. No protocol or durable work happens in
/// this task.
async fn forward_streams(
    mut streams: mpsc::Receiver<InboundStreamPayload>,
    commands: mpsc::Sender<ExecCommand>,
) {
    let mut readers = JoinSet::new();
    loop {
        tokio::select! {
            _ = commands.closed() => return,
            Some((_peer, recv)) = streams.recv() => {
                let commands = commands.clone();
                readers.spawn(read_stream(recv, commands));
            }
            joined = readers.join_next(), if !readers.is_empty() => {
                let _ = joined;
            }
            else => break,
        }
    }
}

async fn read_stream(recv: RecvHandle, commands: mpsc::Sender<ExecCommand>) {
    loop {
        match recv.recv_exec().await {
            Ok(delivery) => {
                if commands
                    .send(ExecCommand::Inbound { delivery })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

impl std::fmt::Debug for ExecutionActor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExecutionActor")
            .field("exec_id", &self.context.exec_id)
            .field("session", &self.context.activation.session_hash())
            .field("producer", &self.context.identity.peer_id())
            .finish_non_exhaustive()
    }
}

impl ExecutionActor {
    async fn run(
        mut self,
        mut commands: mpsc::Receiver<ExecCommand>,
        startup_error: Option<ExecError>,
    ) {
        let recovered = match startup_error {
            Some(error) => Err(error),
            None => self.recover().await,
        };
        if !self.continue_after(recovered).await {
            return;
        }

        let mut ticker = progress_ticker();
        loop {
            if self.end_run_finished() {
                return;
            }
            tokio::select! {
                Some(joined) = self.send_tasks.join_next(), if !self.send_tasks.is_empty() => {
                    let result = match joined {
                        Ok(result) => self.settle_send(result).await,
                        Err(_) => Err(ExecError::DeliveryInvariant("send task failed")),
                    };
                    if !self.continue_after(result).await { return; }
                    let progressed = self.progress().await;
                    if !self.continue_after(progressed).await { return; }
                }
                command = commands.recv() => {
                    match command {
                        Some(command) => {
                            let handled = self.handle(command).await;
                            if !self.continue_after(handled).await {
                                return;
                            }
                        }
                        None => {
                            self.fail_terminal(ExecError::Unavailable(
                                "execution command channel closed".into(),
                            )).await;
                            return;
                        }
                    }
                }
                _ = ticker.tick() => {
                    let progressed = self.progress().await;
                    if !self.continue_after(progressed).await {
                        return;
                    }
                }
            }
        }
    }

    /// Keep one error boundary for recovery, commands, progress, and transport
    /// settlement. A durably recorded terminal continues on the same actor
    /// loop so inbound acknowledgements and delivery retries cannot deadlock.
    async fn continue_after(&mut self, result: Result<(), ExecError>) -> bool {
        match result {
            Ok(()) => true,
            Err(error @ ExecError::DeliveryInvariant(_)) => {
                tracing::error!(exec_id = %self.context.exec_id, %error, "execution delivery stopped");
                false
            }
            Err(error) => self.fail_terminal(error).await,
        }
    }

    async fn handle(&mut self, command: ExecCommand) -> Result<(), ExecError> {
        match command {
            ExecCommand::SubmitInput {
                pending_id,
                data,
                reply,
            } => {
                let result = self.submit_input(pending_id, data).await;
                match result {
                    Ok(()) => {
                        let _ = reply.send(Ok(()));
                        Ok(())
                    }
                    Err(SubmitInputError::Expected(error)) => {
                        let _ = reply.send(Err(error));
                        Ok(())
                    }
                    Err(SubmitInputError::Fatal(error)) => {
                        let _ = reply.send(Err(error.clone()));
                        Err(error)
                    }
                }
            }
            ExecCommand::Terminate { reason, reply } => {
                let result = self.terminate(reason).await;
                let _ = reply.send(result);
                // A stop request can legitimately lose to a locally signed
                // shared proposal. Keep the proposal recoverable and let the
                // normal progress path continue; this command's reply is the
                // durable result boundary for that expected rejection.
                Ok(())
            }
            ExecCommand::Query {
                query_index,
                query,
                reply,
            } => {
                let result = self.query(query_index, query);
                let _ = reply.send(result);
                Ok(())
            }
            ExecCommand::View { viewport, reply } => {
                let result = self.view(viewport);
                let _ = reply.send(result);
                Ok(())
            }
            ExecCommand::Inbound { delivery } => self.inbound(delivery).await,
        }
    }

    pub(super) async fn recover(&mut self) -> Result<(), ExecError> {
        // Construct the resident only after the execution aggregate exists.
        // Recovery always starts from the committed state images; a pending
        // proposal remains durable evidence and is deliberately not installed
        // as live guest state while signatures are outstanding.
        self.restore_resident()?;
        let state = &self.state;

        if state.status().lifecycle() == ExecLifecycle::Activating {
            let mut next = self.state.clone();
            next.activate()?;
            self.persist(next, Change::State).await?;
        }

        self.progress().await
    }

    pub(super) async fn ensure_execution(
        context: &mut ActorContext,
    ) -> Result<ExecutionState, ExecError> {
        let producer = context.identity.peer_id();
        let request = context
            .store
            .load_execution_request()
            .await?
            .ok_or_else(|| {
                ExecError::InvalidState("execution request is not durably recorded".into())
            })?;
        if request.program_hash() != context.program.program().hash()
            || request
                .params()
                .is_some_and(|params| params != &context.params)
        {
            return Err(ExecError::InvalidState(
                "actor program or parameters differ from durable request".into(),
            ));
        }
        let activation = context.store.load_activation().await?.ok_or_else(|| {
            ExecError::InvalidState("execution activation is not durably committed".into())
        })?;
        let Some(durable_activation) = activation.activation() else {
            return Err(ExecError::InvalidState(
                "execution activation is only prepared".into(),
            ));
        };
        if durable_activation != &context.activation {
            return Err(ExecError::InvalidState(
                "actor activation differs from durable activation".into(),
            ));
        }
        if durable_activation.offer().data().program_hash != request.program_hash()
            || request
                .params()
                .is_some_and(|params| durable_activation.offer().data().params != *params)
            || durable_activation.offer().data().execution_profile
                != context.program.profile().hash()
        {
            return Err(ExecError::InvalidState(
                "durable activation terms differ from the execution request or local profile"
                    .into(),
            ));
        }
        let Some(local_ticket) = durable_activation
            .tickets()
            .iter()
            .find(|ticket| ticket.data.signer == producer)
        else {
            return Err(ExecError::InvalidState(
                "durable activation has no ticket for the local producer".into(),
            ));
        };
        let TicketAction::Active { execution_bls, .. } = &local_ticket.data.action else {
            return Err(ExecError::InvalidState(
                "local producer ticket is not active".into(),
            ));
        };
        if *execution_bls != context.execution_key.public_key() {
            return Err(ExecError::InvalidState(
                "execution signer does not match the local activation ticket".into(),
            ));
        }
        if let Some(state) = context.store.load_execution().await? {
            if state.producer() != producer || state.binding().activation() != &context.activation {
                return Err(ExecError::InvalidState(
                    "durable execution binding differs from actor context".into(),
                ));
            }
            return Ok(state);
        }
        let initialized = context.program.initialize(context.params.clone())?;
        context
            .store
            .create_execution(
                context.activation.clone(),
                producer,
                initialized.shared,
                initialized.local,
                now_ms(),
            )
            .await?;
        context
            .store
            .load_execution()
            .await?
            .ok_or(ExecError::NotFound(context.exec_id))
    }

    pub(super) async fn progress(&mut self) -> Result<(), ExecError> {
        let mut drove_events = false;
        let state = loop {
            let state = &self.state;
            match state.status().receipt_work() {
                ReceiptWork::NotTerminal if drove_events => {
                    break state;
                }
                ReceiptWork::NotTerminal => {
                    self.ensure_session_started().await?;
                    self.fire_due_timers().await?;
                    drove_events = true;
                }
                ReceiptWork::Assemble | ReceiptWork::Published => {
                    return self.progress_terminal_boundary().await;
                }
            }
        };
        if state.pending_shared().is_some() {
            self.ensure_step_signature().await?;
        } else if self.may_author()? {
            self.author_next_message().await?;
        }
        self.announce_callout().await?;
        self.deliver_frames()?;
        Ok(())
    }

    /// Announce the committed open callout when its identity changes.
    ///
    /// The open callout is durable in the execution aggregate, so this is only
    /// an observer-facing notification. A restart starts with no announcement
    /// and therefore re-announces the current callout once.
    async fn announce_callout(&mut self) -> Result<(), ExecError> {
        let state = &self.state;
        let Some(open) = state.callout() else {
            return Ok(());
        };
        if self.announced_callout == Some(open.id) {
            return Ok(());
        }
        let pending_id = open.id;
        let callout_index = open.callout_index;
        let context = open.context.clone();
        self.messages
            .send(super::callout_requested(pending_id, callout_index, context))
            .await
            .map_err(|_| ExecError::Unavailable("message receiver closed".into()))?;
        self.announced_callout = Some(pending_id);
        Ok(())
    }

    /// Publish the local result before waiting for remote acknowledgements.
    /// End confirmation remains local protocol state, independent of the
    /// receipt. A deadline ends this run while retaining any unconfirmed peers.
    pub(super) async fn progress_terminal_boundary(&mut self) -> Result<(), ExecError> {
        self.finalize_receipt().await?;
        self.emit_published_terminal().await?;
        if matches!(
            self.state.end_phase(),
            arena0_protocol::EndPhase::Ending { .. }
        ) && tokio::time::Instant::now() >= self.end_deadline
        {
            let mut next = self.state.clone();
            next.expire_end()?;
            self.persist(next, Change::State).await?;
        }
        if !self.end_run_finished() {
            self.deliver_frames()?;
        }
        Ok(())
    }

    pub(super) fn end_run_finished(&self) -> bool {
        matches!(self.state.status().receipt_work(), ReceiptWork::Published)
            && matches!(self.state.end_phase(), arena0_protocol::EndPhase::Ended { unconfirmed } if unconfirmed.is_empty() || tokio::time::Instant::now() >= self.end_deadline)
    }

    pub(super) async fn persist(
        &mut self,
        next: ExecutionState,
        change: Change,
    ) -> Result<(), ExecError> {
        let result =
            persist_transition(&mut self.context.store, &mut self.state, next, change).await;
        self.resync_on_error(result).await
    }

    /// After a failed durable write the store outcome is unknown: drop the
    /// resident and reload the last durable state before any further
    /// transition, then return the original error.
    pub(super) async fn resync_on_error(
        &mut self,
        result: Result<(), ExecError>,
    ) -> Result<(), ExecError> {
        let Err(error) = result else {
            return Ok(());
        };
        self.instance = None;
        self.state = self
            .context
            .store
            .load_execution()
            .await?
            .ok_or(ExecError::NotFound(self.context.exec_id))?;
        Err(error)
    }

    /// Construct or replace the execution-local resident from committed
    /// durable images.  Replacing the instance is required after an unknown
    /// store outcome; continuing with its candidate memory could duplicate a
    /// committed effect.
    pub(super) fn restore_resident(&mut self) -> Result<(), ExecError> {
        let instance = self.context.program.resident(
            self.state.shared_state().clone(),
            self.state.local_state().clone(),
        )?;
        self.instance = Some(instance);
        Ok(())
    }

    /// Reload the resident in place from `state`'s committed images after a
    /// transition replaced them without a resident commit (certification).
    /// A missing resident stays missing and is rebuilt on next use. On failure
    /// the resident is dropped so the next use rebuilds it.
    pub(super) fn reload_resident(&mut self) -> Result<(), ExecError> {
        let Some(instance) = self.instance.as_mut() else {
            return Ok(());
        };
        let reloaded = instance
            .restore_payloads(
                self.state.shared_state().clone(),
                self.state.local_state().clone(),
            )
            .map_err(ExecError::from);
        if reloaded.is_err() {
            self.instance = None;
        }
        reloaded
    }

    pub(super) fn resident_mut(&mut self) -> Result<&mut ProgramInstance, ExecError> {
        if self.instance.is_none() {
            self.restore_resident()?;
        }
        Ok(self.instance.as_mut().expect("resident was just restored"))
    }

    pub(super) fn ensemble(&self) -> Ensemble<Committed> {
        Ensemble::from_peers(
            self.context
                .activation
                .tickets()
                .iter()
                .map(|ticket| ticket.data.signer)
                .collect(),
        )
        .expect("validated activation has a committed ensemble")
    }
}

/// Persist `next` as the successor of `state`, advancing `state` only after
/// the transition is durable.
pub(super) async fn persist_transition(
    store: &mut arena0_store::ExecutionStore,
    state: &mut ExecutionState,
    next: ExecutionState,
    change: Change,
) -> Result<(), ExecError> {
    store
        .persist(TransitionRecord {
            expected: state.version(),
            next: next.clone(),
            change,
            now_ms: now_ms(),
        })
        .await?;
    *state = next;
    Ok(())
}

fn progress_ticker() -> Interval {
    let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker
}
