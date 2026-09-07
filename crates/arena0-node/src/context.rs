//! Public execution handles and the actor's construction boundary.
//!
//! The node deliberately exposes commands and observations, not a mutable
//! execution object. The private execution actor owns the only live guest and
//! transport capabilities for one execution identity; protocol state is loaded
//! from the store for each command.

use arena0_protocol::PendingId;
use std::sync::Arc;

use arena0_crypto::{ExecutionKey, ExecutionSalt, NodeKeys};
use arena0_program::{JsonBytes, ProgramHash};
use arena0_protocol::{
    Activation, Ensemble, ExecId, ExecutionAdmission, ExecutionInput, FrameId, LocalStateBytes,
    NegotiationTarget, PeerIdSource, PreparedActivation, ReceiptArtifact, SessionHash,
    SharedStateBytes, View,
};
use arena0_sandbox::AdmittedProgram;
use arena0_store::{
    AdmissionBindingOutcome, CommitActivationOutcome, CreateExecutionOutcome,
    ExecutionRequestFailureOutcome, ExecutionRequestOutcome, ExecutionStore,
    PrepareActivationOutcome, StoreError,
};
use arena0_transport::{ExecDelivery, RecvHandle, Transport};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Failures returned by one execution actor.
#[derive(Debug, Error)]
pub enum ExecError {
    /// No durable execution exists for the requested identity.
    #[error("execution {0} was not found")]
    NotFound(ExecId),
    /// An agent answer raced with another answer and no longer names the
    /// durable callout continuation.
    #[error("callout is no longer pending")]
    CalloutNotPending,
    /// Durable or guest state violates an execution invariant.
    #[error("invalid execution state: {0}")]
    InvalidState(String),
    /// An execution dependency or owned task is unavailable.
    #[error("execution resource unavailable: {0}")]
    Unavailable(String),
}

impl From<arena0_sandbox::SandboxError> for ExecError {
    fn from(error: arena0_sandbox::SandboxError) -> Self {
        Self::Unavailable(error.to_string())
    }
}

impl From<arena0_protocol::ProtocolError> for ExecError {
    fn from(error: arena0_protocol::ProtocolError) -> Self {
        Self::InvalidState(error.to_string())
    }
}

impl From<StoreError> for ExecError {
    fn from(error: StoreError) -> Self {
        Self::Unavailable(error.to_string())
    }
}

impl From<arena0_transport::TransportError> for ExecError {
    fn from(error: arena0_transport::TransportError) -> Self {
        Self::Unavailable(error.to_string())
    }
}

/// A command serialized by one execution actor.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ExecCommand {
    /// Submit a validated JSON answer to a pending program callout.
    SubmitInput {
        /// The durable continuation identity being answered.
        pending_id: PendingId,
        /// The advertised callout variant being answered.
        callout_index: u32,
        /// Complete agent-facing JSON input.
        data: JsonBytes,
        /// Command result.
        reply: oneshot::Sender<Result<(), ExecError>>,
    },
    /// Request a locally authored signed abort occurrence.
    Terminate {
        /// Human-readable bounded termination reason.
        reason: String,
        /// Command result.
        reply: oneshot::Sender<Result<(), ExecError>>,
    },
    /// Run a read-only program query against the latest durable shared state.
    Query {
        /// Advertised query index.
        query_index: u32,
        /// Complete agent-facing JSON query.
        query: JsonBytes,
        /// Query result.
        reply: oneshot::Sender<Result<JsonBytes, ExecError>>,
    },
    /// Render a read-only program view against the latest durable shared state.
    View {
        /// Complete agent-facing JSON viewport.
        viewport: JsonBytes,
        /// View result, paired with the durable public position observed.
        reply: oneshot::Sender<Result<(u64, View), ExecError>>,
    },
    /// Deliver one transport-authenticated frame after the stream reader has
    /// placed it on the actor's serialized command queue.
    Inbound { delivery: ExecDelivery },
    /// A previously authenticated execution stream closed. The actor treats
    /// this as a terminal transport failure rather than silently losing a
    /// peer's durable delivery path.
    InboundStreamClosed { peer: arena0_protocol::PeerId },
}

/// A stream handed from the Host accept router to one execution actor.
///
/// The peer value is routing metadata; the `RecvHandle` remains the transport
/// owner of frame authentication.  The reader never performs protocol or
/// durable work—it only forwards decoded [`ExecDelivery`] values to the actor.
pub(crate) type InboundStreamPayload = (arena0_protocol::PeerId, RecvHandle);

/// Reliable observations emitted after the corresponding durable boundary.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum SessionMessage {
    /// A shared trace step became durable.
    TraceAppended { step: u64 },
    /// Activation was durable and the guest received the session-start
    /// boundary.
    SessionStarted {
        session_id: SessionHash,
        ensemble: Ensemble,
    },
    /// A durable callout is ready for an agent answer.
    CalloutRequested {
        pending_id: PendingId,
        callout_index: u32,
        context: Vec<u8>,
        expected_type: Option<String>,
    },
    /// A local notification became deliverable.
    Notification { frame_id: FrameId, payload: Vec<u8> },
    /// A final receipt was durably published locally.
    ReceiptPublished { receipt: ReceiptArtifact },
    /// The execution reached a completed terminal boundary.
    Completed {
        result: Vec<u8>,
        result_json: Option<Vec<u8>>,
    },
    /// An abort occurrence became durable.
    Aborted { step: u64, reason: String },
    /// A failed execution or supervisor boundary.
    Failed { reason: String },
}

/// All capabilities needed to construct one execution actor.
///
/// The context is consumed by [`crate::Host::spawn`].  It contains no
/// mutable protocol state: the SQLite [`ExecutionStore`] is the sole writer
/// for that execution, while the actor owns only this admitted guest and its
/// live external capabilities.
pub struct ExecContext {
    /// Stable execution identity.
    pub(crate) exec_id: ExecId,
    /// Immutable Wasm after sandbox admission.
    pub(crate) program: Arc<AdmittedProgram>,
    /// Exact agent-facing parameters bound by activation.
    pub(crate) params: JsonBytes,
    /// Cryptographically validated activation.
    pub(crate) activation: Activation,
    /// Execution-scoped BLS signer.
    ///
    /// This key is consumed by the one actor for this execution. Keeping it
    /// non-cloneable makes the actor the sole live signing authority.
    pub(crate) execution_key: ExecutionKey,
}

impl ExecContext {
    /// Collect the already-validated capabilities consumed by one execution
    /// actor. Mutable protocol state remains owned by [`HostExecutionStore`].
    #[must_use]
    pub fn new(
        exec_id: ExecId,
        program: Arc<AdmittedProgram>,
        params: JsonBytes,
        activation: Activation,
        execution_key: ExecutionKey,
    ) -> Self {
        Self {
            exec_id,
            program,
            params,
            activation,
            execution_key,
        }
    }
}

/// Host-issued, non-clone lifecycle capability for one execution.
///
/// The inner [`ExecutionStore`] and its private Host token prevent a caller
/// from handing [`crate::Host::spawn`] a writer acquired from another Host or
/// store.
/// The capability is issued by [`crate::Host::claim_execution`], consumed by
/// [`crate::Host::negotiate`], and finally consumed by [`crate::Host::spawn`].
/// State-changing operations require `&mut self`; read access stays inside the
/// node or uses the explicit methods exposed for daemon setup.
#[derive(Debug)]
pub struct HostExecutionStore {
    store: ExecutionStore,
    owner: Arc<()>,
}

impl HostExecutionStore {
    /// Return the execution identity fixed by this capability.
    #[must_use]
    pub const fn execution_id(&self) -> ExecId {
        self.store.execution_id()
    }

    /// Create the durable admission root for this execution.
    pub async fn create_execution_request(
        &mut self,
        program_hash: ProgramHash,
        params: Option<JsonBytes>,
        admission: ExecutionAdmission,
        created_at_ms: u64,
    ) -> Result<ExecutionRequestOutcome, StoreError> {
        self.store
            .create_execution_request(program_hash, params, admission, created_at_ms)
            .await
    }

    /// Record a bounded failure on this execution's admission root.
    pub async fn record_execution_request_failure(
        &mut self,
        reason: impl Into<String>,
    ) -> Result<ExecutionRequestFailureOutcome, StoreError> {
        self.store.record_execution_request_failure(reason).await
    }

    /// Bind an open Join request to its first accepted offer before ticket
    /// signing. The compare-and-set in the store makes retries idempotent and
    /// prevents a later offer from replacing the selected target.
    pub async fn bind_join_target(
        &mut self,
        target: NegotiationTarget,
    ) -> Result<AdmissionBindingOutcome, StoreError> {
        self.store.bind_join_target(target).await
    }

    /// Load or create the execution's durable local secret.
    pub async fn load_or_create_execution_salt(
        &mut self,
        now_ms: u64,
    ) -> Result<ExecutionSalt, StoreError> {
        self.store.load_or_create_execution_salt(now_ms).await
    }

    /// Prepare an activation under this execution's permanent key.
    pub async fn prepare_activation(
        &mut self,
        prepared: PreparedActivation,
        now_ms: u64,
    ) -> Result<PrepareActivationOutcome, StoreError> {
        self.store.prepare_activation(prepared, now_ms).await
    }

    /// Commit an activation under this execution's permanent key.
    pub async fn commit_activation(
        &mut self,
        activation: Activation,
        now_ms: u64,
    ) -> Result<CommitActivationOutcome, StoreError> {
        self.store.commit_activation(activation, now_ms).await
    }

    /// Insert the initial execution aggregate from the committed activation.
    pub async fn create_execution(
        &mut self,
        activation: Activation,
        producer: arena0_protocol::PeerId,
        shared_state: SharedStateBytes,
        local_state: LocalStateBytes,
        now_ms: u64,
    ) -> Result<CreateExecutionOutcome, StoreError> {
        self.store
            .create_execution(activation, producer, shared_state, local_state, now_ms)
            .await
    }

    /// Apply one protocol input to this execution.
    pub async fn apply_input(
        &mut self,
        input: ExecutionInput,
        now_ms: u64,
    ) -> Result<arena0_store::ApplyOutcome, StoreError> {
        self.store.apply_input(input, now_ms).await
    }

    pub(crate) fn store_mut(&mut self) -> &mut ExecutionStore {
        &mut self.store
    }

    pub(crate) fn from_store(owner: Arc<()>, store: ExecutionStore) -> Self {
        Self { store, owner }
    }

    pub(crate) fn into_store(self) -> ExecutionStore {
        self.store
    }

    pub(crate) fn belongs_to(&self, owner: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.owner, owner)
    }
}

impl std::fmt::Debug for ExecContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExecContext")
            .field("exec_id", &self.exec_id)
            .field("program", &self.program)
            .field("params_len", &self.params.len())
            .field("session_id", &self.activation.session_hash())
            .finish_non_exhaustive()
    }
}

/// Host-bound execution capabilities.
///
/// This type is deliberately crate-private. A caller can describe an
/// execution with [`ExecContext`], but only [`crate::Host`] can bind it to the
/// Host identity, durable store, and transport before the actor is spawned.
/// That keeps an execution from being constructed with a store or transport
/// unrelated to its Host.
pub(crate) struct ActorContext {
    pub(crate) exec_id: ExecId,
    pub(crate) program: Arc<AdmittedProgram>,
    pub(crate) params: JsonBytes,
    pub(crate) activation: Activation,
    pub(crate) producer: arena0_protocol::PeerId,
    pub(crate) execution_key: ExecutionKey,
    pub(crate) identity: Arc<NodeKeys>,
    pub(crate) store: ExecutionStore,
    pub(crate) transport: Arc<dyn Transport + Sync>,
}

impl ExecContext {
    pub(crate) fn bind(
        self,
        identity: Arc<NodeKeys>,
        store: ExecutionStore,
        transport: Arc<dyn Transport + Sync>,
    ) -> ActorContext {
        let producer = identity.peer_id();
        ActorContext {
            exec_id: self.exec_id,
            program: self.program,
            params: self.params,
            activation: self.activation,
            producer,
            execution_key: self.execution_key,
            identity,
            store,
            transport,
        }
    }
}

/// Public handles for one actor task.
pub struct SpawnedExec {
    /// Keeps the Host accept routers alive for the lifetime of this actor.
    ///
    /// `Host::start` launches those routers as Host-owned tasks. Retaining the
    /// Host here closes the ownership gap where a caller could drop its last
    /// `Arc<Host>` immediately after spawning and strand the actor's inbound
    /// streams. [`Host::stop`](crate::Host::stop) remains the explicit way to
    /// stop routing while an execution handle is live.
    _host: Arc<crate::Host>,
    /// Command queue.  Its bounded capacity is the actor's back-pressure
    /// boundary; callers must not bypass it.
    pub cmd_tx: mpsc::Sender<ExecCommand>,
    /// Durable observations emitted by the actor.
    pub message_rx: mpsc::Receiver<SessionMessage>,
    /// Stream handoff queue used by [`crate::Host`].
    pub(crate) stream_tx: mpsc::Sender<InboundStreamPayload>,
    task: Option<ExecutionTask>,
    forwarder: Option<JoinHandle<()>>,
    session_claim: Option<SessionStreamClaim>,
}

/// Host-owned uniqueness claim for one authenticated execution stream.
///
/// Execution identity ownership lives in [`arena0_store::ExecutionStore`];
/// this guard only releases the independent session routing entry when the
/// public actor handle is retired.
pub(crate) struct SessionStreamClaim {
    release: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl SessionStreamClaim {
    pub(crate) fn new(release: impl FnOnce() + Send + 'static) -> Self {
        Self {
            release: Some(Box::new(release)),
        }
    }
}

impl Drop for SessionStreamClaim {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// The actor and its stream-reader task.
#[derive(Debug)]
pub(crate) struct ExecutionTask {
    actor: JoinHandle<()>,
    streams: JoinHandle<()>,
}

impl ExecutionTask {
    pub(crate) fn new(actor: JoinHandle<()>, streams: JoinHandle<()>) -> Self {
        Self { actor, streams }
    }
}

impl SpawnedExec {
    pub(crate) fn new(
        host: Arc<crate::Host>,
        cmd_tx: mpsc::Sender<ExecCommand>,
        message_rx: mpsc::Receiver<SessionMessage>,
        stream_tx: mpsc::Sender<InboundStreamPayload>,
        task: ExecutionTask,
    ) -> Self {
        Self {
            _host: host,
            cmd_tx,
            message_rx,
            stream_tx,
            task: Some(task),
            forwarder: None,
            session_claim: None,
        }
    }

    pub(crate) fn with_session_claim(mut self, claim: SessionStreamClaim) -> Self {
        self.session_claim = Some(claim);
        self
    }

    pub(crate) fn with_forwarder(mut self, forwarder: JoinHandle<()>) -> Self {
        self.forwarder = Some(forwarder);
        self
    }

    /// Shut down the owned task pair.  A command-channel closure is treated as
    /// a normal supervisor stop; the actor reports the durable boundary before
    /// exiting and the bounded join prevents a stuck guest from being leaked.
    pub async fn shutdown(&mut self) {
        if let Some(task) = self.task.take() {
            // `forward_streams` holds a command sender while it supervises
            // readers. Close both ingress queues first so the actor can
            // observe command-channel closure instead of waiting forever on a
            // sender retained by its stream supervisor.
            let (replacement_commands, _) = mpsc::channel(1);
            let commands = std::mem::replace(&mut self.cmd_tx, replacement_commands);
            drop(commands);
            let (replacement_streams, _) = mpsc::channel(1);
            let streams = std::mem::replace(&mut self.stream_tx, replacement_streams);
            drop(streams);

            if let Some(forwarder) = self.forwarder.take() {
                forwarder.abort();
                let _ = forwarder.await;
            }

            let ExecutionTask {
                actor,
                streams: stream_supervisor,
            } = task;
            let mut actor = actor;
            let mut stream_supervisor = stream_supervisor;
            // Selecting a JoinHandle consumes its output. Track which branch
            // completed so shutdown never polls that handle a second time.
            enum ShutdownWinner {
                Actor,
                StreamSupervisor,
                Timeout,
            }
            let winner = tokio::select! {
                _ = &mut actor => ShutdownWinner::Actor,
                _ = &mut stream_supervisor => ShutdownWinner::StreamSupervisor,
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => ShutdownWinner::Timeout,
            };
            match winner {
                ShutdownWinner::Actor => {
                    stream_supervisor.abort();
                    let _ = stream_supervisor.await;
                }
                ShutdownWinner::StreamSupervisor => {
                    actor.abort();
                    let _ = actor.await;
                }
                ShutdownWinner::Timeout => {
                    // Dropping a JoinHandle detaches its task. Abort
                    // explicitly on the timeout path so shutdown cannot leak
                    // a live actor or reader supervisor.
                    actor.abort();
                    stream_supervisor.abort();
                    let _ = actor.await;
                    let _ = stream_supervisor.await;
                }
            }
            drop(self.session_claim.take());
        }
    }
}

impl Drop for SpawnedExec {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.actor.abort();
            task.streams.abort();
        }
        if let Some(forwarder) = self.forwarder.take() {
            forwarder.abort();
        }
        drop(self.session_claim.take());
    }
}

impl std::fmt::Debug for SpawnedExec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SpawnedExec")
            .field("cmd_tx", &self.cmd_tx)
            .field("message_rx", &"<receiver>")
            .field("stream_tx", &self.stream_tx)
            .finish_non_exhaustive()
    }
}
