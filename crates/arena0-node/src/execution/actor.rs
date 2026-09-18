//! Actor lifecycle, recovery, and serialized command dispatch.
//!
//! This module owns task startup and the actor loop. Guest transitions,
//! inbox resolution, outbox delivery, and terminal boundaries live in their
//! respective modules and extend the same private actor state owner.

use std::collections::HashMap;
use std::sync::Arc;

use arena0_protocol::{
    Committed, Ensemble, ExecLifecycle, ExecutionState, PeerIdSource, ReceiptWork, TicketAction,
};
use arena0_sandbox::{InitializeCall, ProgramInstance};
use arena0_store::PendingRequest;
use arena0_transport::RecvHandle;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Interval, MissedTickBehavior};

use crate::Host;
use crate::context::{ActorContext, ExecCommand, ExecError, InboundStreamPayload, SpawnedExec};

use super::guest::SubmitInputError;
use super::{
    COMMAND_CAPACITY, ExecutionActor, PROGRESS_INTERVAL, STREAM_CAPACITY, callout_requested, now_ms,
};

const MAX_PROGRESS_PASSES: usize = 64;

/// Spawn one actor and one concurrent reader supervisor.
#[must_use]
pub(crate) fn spawn_execution(context: ActorContext, host: Arc<Host>) -> SpawnedExec {
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (message_tx, message_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (stream_tx, stream_rx) = mpsc::channel(STREAM_CAPACITY);

    let actor = ExecutionActor {
        context,
        instance: None,
        messages: message_tx,
        send_streams: HashMap::new(),
        inflight_send: None,
        session_started_emitted: false,
        terminal_emitted: false,
    };
    let actor_task = tokio::spawn(actor.run(command_rx));
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
            Some((peer, recv)) = streams.recv() => {
                let commands = commands.clone();
                readers.spawn(read_stream(peer, recv, commands));
            }
            joined = readers.join_next(), if !readers.is_empty() => {
                let _ = joined;
            }
            else => break,
        }
    }
}

async fn read_stream(
    peer: arena0_protocol::PeerId,
    recv: RecvHandle,
    commands: mpsc::Sender<ExecCommand>,
) {
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
            Err(_) => {
                let _ = commands
                    .send(ExecCommand::InboundStreamClosed { peer })
                    .await;
                return;
            }
        }
    }
}

impl std::fmt::Debug for ExecutionActor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExecutionActor")
            .field("exec_id", &self.context.exec_id)
            .field("session", &self.context.activation.session_hash())
            .field("producer", &self.context.producer)
            .finish_non_exhaustive()
    }
}

impl ExecutionActor {
    async fn run(mut self, mut commands: mpsc::Receiver<ExecCommand>) {
        let recovered = self.recover().await;
        if !self.continue_after(recovered).await {
            return;
        }

        let mut ticker = progress_ticker();
        loop {
            let has_inflight_send = self.inflight_send.is_some();
            tokio::select! {
                result = async {
                    self.inflight_send
                        .as_mut()
                        .expect("in-flight send exists while selected")
                        .wait()
                        .await
                }, if has_inflight_send => {
                    let settled = self.settle_inflight_send(result).await;
                    if !self.continue_after(settled).await {
                        return;
                    }
                    let progressed = self.progress().await;
                    if !self.continue_after(progressed).await {
                        return;
                    }
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
    /// loop so inbound acknowledgements and outbox retries cannot deadlock.
    async fn continue_after(&mut self, result: Result<(), ExecError>) -> bool {
        match result {
            Ok(()) => true,
            Err(error) => self.fail_terminal(error).await,
        }
    }

    async fn handle(&mut self, command: ExecCommand) -> Result<(), ExecError> {
        match command {
            ExecCommand::SubmitInput {
                pending_id,
                callout_index,
                data,
                reply,
            } => {
                let result = self.submit_input(pending_id, callout_index, data).await;
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
                let result = self.query(query_index, query).await;
                let _ = reply.send(result);
                Ok(())
            }
            ExecCommand::View { viewport, reply } => {
                let result = self.view(viewport).await;
                let _ = reply.send(result);
                Ok(())
            }
            ExecCommand::Inbound { delivery } => self.inbound(delivery).await,
            ExecCommand::InboundStreamClosed { peer } => {
                if self.ensemble().peers().contains(&peer) {
                    Err(ExecError::Unavailable(format!(
                        "execution stream from {peer} closed"
                    )))
                } else {
                    // A route keyed only by session identity may receive an
                    // outsider stream. Its closure is not evidence about the
                    // activated participants and cannot terminate this actor.
                    Ok(())
                }
            }
        }
    }

    pub(super) async fn recover(&mut self) -> Result<(), ExecError> {
        self.context.store.recover_expired_leases(now_ms()).await?;
        self.ensure_execution().await?;

        // Construct the resident only after the execution aggregate exists.
        // Recovery always starts from the committed state images; a pending
        // proposal remains durable evidence and is deliberately not installed
        // as live guest state while signatures are outstanding.
        let state = self.load_state().await?;
        self.restore_resident(&state)?;

        // An acknowledged callout may have reached the observer immediately
        // before the actor crashed, leaving the durable continuation waiting
        // for an answer while its outbox row is no longer pending. Capture
        // that projection before the normal outbox drain; rows that are still
        // pending/leased will be delivered by the drain itself. Signature
        // requests never cross this boundary: the actor signs and resumes
        // them as one leased outbox operation.
        let replay_requests = self
            .context
            .store
            .pending_requests()
            .await?
            .into_iter()
            .filter(|request| matches!(request, PendingRequest::Callout { .. }))
            .filter(|request| request.status() == arena0_store::OutboxStatus::Acknowledged)
            .collect::<Vec<_>>();

        let state = self.load_state().await?;

        if state.status().lifecycle() == ExecLifecycle::Activating {
            // Activation is a store-owned lifecycle transition.  The direct
            // store API supplies this operation once the execution writer has
            // adopted the flat dispatch contract.
            self.activate_execution(state.version()).await?;
        }

        self.progress().await?;
        self.emit_recovered_pending_requests(&replay_requests).await
    }

    /// Re-emit an agent request whose durable outbox effect was acknowledged
    /// before the previous actor lifetime could receive its answer.
    async fn emit_recovered_pending_requests(
        &self,
        requests: &[PendingRequest],
    ) -> Result<(), ExecError> {
        for request in requests {
            if let PendingRequest::Callout {
                pending_id,
                callout_index,
                context,
                expected_type,
                ..
            } = request
            {
                self.messages
                    .send(callout_requested(
                        *pending_id,
                        *callout_index,
                        context.clone(),
                        expected_type.clone(),
                    ))
                    .await
                    .map_err(|_| ExecError::Unavailable("message receiver closed".into()))?;
            }
        }
        Ok(())
    }

    pub(super) async fn ensure_execution(&mut self) -> Result<(), ExecError> {
        if self.context.identity.peer_id() != self.context.producer {
            return Err(ExecError::InvalidState(
                "actor identity and producer identity differ".into(),
            ));
        }
        let request = self
            .context
            .store
            .load_execution_request()
            .await?
            .ok_or_else(|| {
                ExecError::InvalidState("execution request is not durably recorded".into())
            })?;
        if request.program_hash() != self.context.program.program().hash()
            || request
                .params()
                .is_some_and(|params| params != &self.context.params)
        {
            return Err(ExecError::InvalidState(
                "actor program or parameters differ from durable request".into(),
            ));
        }
        let activation = self.context.store.load_activation().await?.ok_or_else(|| {
            ExecError::InvalidState("execution activation is not durably committed".into())
        })?;
        let Some(durable_activation) = activation.activation() else {
            return Err(ExecError::InvalidState(
                "execution activation is only prepared".into(),
            ));
        };
        if durable_activation != &self.context.activation {
            return Err(ExecError::InvalidState(
                "actor activation differs from durable activation".into(),
            ));
        }
        if durable_activation.offer().data().program_hash != request.program_hash()
            || request
                .params()
                .is_some_and(|params| durable_activation.offer().data().params != *params)
            || durable_activation.offer().data().execution_profile
                != self.context.program.profile().hash()
        {
            return Err(ExecError::InvalidState(
                "durable activation terms differ from the execution request or local profile"
                    .into(),
            ));
        }
        let Some(local_ticket) = durable_activation
            .tickets()
            .iter()
            .find(|ticket| ticket.data.signer == self.context.producer)
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
        if *execution_bls != self.context.execution_key.public_key() {
            return Err(ExecError::InvalidState(
                "execution signer does not match the local activation ticket".into(),
            ));
        }
        if let Some(state) = self.context.store.load_execution().await? {
            if state.producer() != self.context.producer
                || state.binding().activation() != &self.context.activation
            {
                return Err(ExecError::InvalidState(
                    "durable execution binding differs from actor context".into(),
                ));
            }
            return Ok(());
        }
        let initialized = self
            .context
            .program
            .initialize(InitializeCall::new(self.context.params.clone()))?;
        self.context
            .store
            .create_execution(
                self.context.activation.clone(),
                self.context.producer,
                initialized.shared,
                initialized.local,
                now_ms(),
            )
            .await?;
        Ok(())
    }

    pub(super) async fn progress(&mut self) -> Result<(), ExecError> {
        'progress: for _ in 0..MAX_PROGRESS_PASSES {
            let mut drove_events = false;
            let state = loop {
                let state = self.load_state().await?;
                match state.status().receipt_work() {
                    ReceiptWork::NotTerminal | ReceiptWork::CollectSignatures if drove_events => {
                        break state;
                    }
                    ReceiptWork::NotTerminal | ReceiptWork::CollectSignatures => {
                        self.ensure_session_started().await?;
                        self.resolve_pending_inbox().await?;
                        self.fire_due_timers().await?;
                        drove_events = true;
                    }
                    ReceiptWork::Incomplete => return Ok(()),
                    ReceiptWork::Assemble | ReceiptWork::Published => {
                        if !self.progress_terminal_boundary().await? {
                            return Ok(());
                        }
                        continue 'progress;
                    }
                }
            };
            if state.pending_shared().is_some() {
                self.ensure_step_signature().await?;
            } else if state.terminal_pending() {
                self.ensure_terminal_signature().await?;
            } else if state.status().lifecycle() == ExecLifecycle::Active
                && state.status().pending().is_none()
                && state.agreed_step() > 0
                && state.last_reacted_step() != Some(state.agreed_step() - 1)
            {
                self.dispatch_event(
                    arena0_protocol::Event::React,
                    super::guest::DispatchSource::default(),
                )
                .await?;
            }
            let summary = self.drain_outbox_report().await?;
            if !summary.sign_consumed {
                return Ok(());
            }
        }
        tracing::debug!(
            exec_id = %self.context.exec_id,
            passes = MAX_PROGRESS_PASSES,
            "progress trampoline yielded after bounded Sign continuations"
        );
        Ok(())
    }

    /// Finish local terminal work before exposing the observer-facing
    /// publication. Protocol frames remain durable obligations after a receipt
    /// is published: an in-flight send is leased and a delayed retry is still
    /// pending, so either must settle before this actor reports completion.
    pub(super) async fn progress_terminal_boundary(&mut self) -> Result<bool, ExecError> {
        super::terminal::finalize_receipt(&mut self.context.store).await?;
        let summary = self.drain_outbox_report().await?;
        if self.context.store.has_unsettled_frames().await? {
            return Ok(false);
        }
        self.emit_published_terminal().await?;
        Ok(summary.sign_consumed)
    }

    pub(super) async fn load_state(&self) -> Result<ExecutionState, ExecError> {
        self.context
            .store
            .load_execution()
            .await?
            .ok_or(ExecError::NotFound(self.context.exec_id))
    }

    /// Construct or replace the execution-local resident from committed
    /// durable images.  Replacing the instance is required after an unknown
    /// store outcome; continuing with its candidate memory could duplicate a
    /// committed effect.
    pub(super) fn restore_resident(&mut self, state: &ExecutionState) -> Result<(), ExecError> {
        let instance = self
            .context
            .program
            .resident(state.shared_state().clone(), state.local_state().clone())?;
        self.instance = Some(instance);
        Ok(())
    }

    pub(super) fn resident_mut(&mut self) -> Result<&mut ProgramInstance, ExecError> {
        self.instance.as_mut().ok_or_else(|| {
            ExecError::InvalidState("execution resident has not been initialized".into())
        })
    }

    /// Activate the durable aggregate.  This method is intentionally kept as
    /// a node seam while the store owns the focused lifecycle transition.
    async fn activate_execution(
        &mut self,
        expected_version: arena0_protocol::ExecutionVersion,
    ) -> Result<(), ExecError> {
        self.context
            .store
            .activate(expected_version, now_ms())
            .await?;
        Ok(())
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

fn progress_ticker() -> Interval {
    let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker
}
