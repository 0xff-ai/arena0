//! The daemon proper: state, the non-blocking `exec.new` flow, request dispatch,
//! the event bus and Unix socket listener.
//!
//! One accept loop dispatches inbound *transport* streams by protocol (negotiation
//! `Negotiation` streams to the in-flight exec creation, `Exec` streams to their
//! execution). Client requests arrive on a Unix socket. Negotiations serialize,
//! but run in the background, so
//! `exec.new` returns immediately.

use std::collections::HashMap;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant as StdInstant};
use std::{future::Future, path::PathBuf};

use anyhow::Context as _;
use arena0_api::{
    ActivationInspection, ActivationInspectionState, ActivationParticipant, ActivityData,
    ActivityFrame, ApiError, ApiErrorCode, EnsembleSpec, EventData, EventFilter, EventFrame,
    ExecLifecycle, ExecOrigin, ExecStatus, ExecStatusState, ExecutionFailureKind,
    ExecutionInspection, FullVerifiedTerminal, HostInfo, LightVerifiedTerminal, NegotiationStage,
    NextEvent, PendingCalloutStatus, PrivateCommitSummary as ApiPrivateCommitSummary,
    PrivateEffectKind as ApiPrivateEffectKind, PrivateEffectSummary as ApiPrivateEffectSummary,
    PrivateEventKind as ApiPrivateEventKind, ProgramRefError, ReceiptRef, Response, ResponseOk,
    SessionProgress, SessionStatus, VerifiedResult, frame,
};
use arena0_api::{HostRequest, HostStatus};
use arena0_crypto::{AgentPubKey, ExecutionKey, NodeKeys};
use arena0_node::{ActivatedSession, NegotiationBook};
use arena0_node::{
    DurableOutcome, HostExecutionStore, NegotiationAttempt, NegotiationEffects, NegotiationStart,
    NegotiationSupervision, PrepareOutcome, unix_time_ms,
};
use arena0_program::{
    ABI_VERSION, JsonBytes, JsonSchemaDocument, ParticipantCount, ProgramHash, ProgramSchema,
};
use arena0_protocol::{
    ActivationAnnouncement, EventSource, ExecCreationOrigin, ExecId, ExecutionAdmission,
    ExecutionEvent, ExecutionFailureCode, ExecutionStatus, FetchFrame, MAX_CLOCK_SKEW_MS,
    MAX_TICKET_LIFETIME_MS, NegotiationEvent, NegotiationFact, NegotiationGossip, NegotiationId,
    NegotiationTarget, Offer, OfferData, OfferHash, PREPARE_WINDOW_MS, PeerId, PeerIdSource,
    PendingId, ReceiptArtifact, SessionHash, StateHash, TerminalKind, Ticket, TicketAction,
    TicketData, TicketHash, Viewport, system_event::SystemEvent,
};
use arena0_sandbox::{InitializeCall, LoadedProgram, Program, ViewCall, WasmtimeEngine};
use arena0_transport::{NegotiationTopic, ProgramTopicEvent, Transport};
use arena0_verify::{LightVerifiedTerminal as VerifiedLightTerminal, verify_full, verify_light};
use retry::delay::{Exponential, jitter};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex as TokioMutex, broadcast, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, timeout_at};
use tracing::Instrument as _;

use crate::catalog::{CatalogError, ProgramCatalog};
use crate::exec_manager::{
    ExecutionHandle, ExecutionHandles, NEGOTIATION_TIMEOUT, Supervisor, project_durable_next,
    satisfies,
};
use crate::schema;
use crate::startup::{StartupStage, StartupTimeline};
use crate::store::{Keystore, KeystoreError};
use arena0_store::{
    ActivationRecord, ActivationRecordStatus, AdmissionBindingOutcome, ExecutionRequest,
    ExecutionRequestFailureOutcome, MAX_PRIVATE_INSPECTION_RECORDS,
    PrivateCommitSummary as StorePrivateCommitSummary, RecoveryCandidate, RecoveryCursor,
    StoreHandle,
};

/// Capacity of the event broadcast bus. A slow subscriber that falls this far
/// behind gets a drop-oldest `stream.lagged` frame rather than blocking producers.
const EVENT_BUS_CAP: usize = 1024;
/// Capacity of the daemon-wide MCP activity bus. Slow monitors receive one
/// bounded lag marker and never hold up tool dispatch.
const ACTIVITY_BUS_CAP: usize = 1024;
const LOCAL_WITHDRAWAL_REASON: &str = "negotiation withdrawn locally";
const CREATION_ARRIVAL_GRACE: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CreationState {
    Awaiting,
    Creating { cancelled: bool },
}

struct CreationEntry {
    state: CreationState,
    completion: tokio::sync::watch::Sender<Option<Result<(), ApiError>>>,
}

#[derive(Default)]
struct CreationStates(HashMap<ExecId, CreationEntry>);

impl CreationStates {
    fn begin(
        &mut self,
        exec_id: ExecId,
    ) -> Result<tokio::sync::watch::Receiver<Option<Result<(), ApiError>>>, ApiError> {
        if let Some(existing) = self.0.remove(&exec_id) {
            match existing.state {
                CreationState::Awaiting => {
                    let _ = existing.completion.send(Some(Ok(())));
                    return Err(ApiError::new(
                        ApiErrorCode::Negotiation,
                        "execution creation was cancelled before admission",
                    ));
                }
                CreationState::Creating { .. } => {
                    self.0.insert(exec_id, existing);
                    return Err(ApiError::new(
                        ApiErrorCode::BadRequest,
                        "execution creation is already in progress",
                    ));
                }
            }
        }
        let (completion, receiver) = tokio::sync::watch::channel(None);
        self.0.insert(
            exec_id,
            CreationEntry {
                state: CreationState::Creating { cancelled: false },
                completion,
            },
        );
        Ok(receiver)
    }

    fn is_cancelled(&self, exec_id: ExecId) -> bool {
        matches!(
            self.0.get(&exec_id).map(|entry| entry.state),
            Some(CreationState::Creating { cancelled: true })
        )
    }

    fn cancel_existing(
        &mut self,
        exec_id: ExecId,
    ) -> Option<tokio::sync::watch::Receiver<Option<Result<(), ApiError>>>> {
        let entry = self.0.get_mut(&exec_id)?;
        if let CreationState::Creating { cancelled } = &mut entry.state {
            *cancelled = true;
        }
        Some(entry.completion.subscribe())
    }

    fn await_creation(
        &mut self,
        exec_id: ExecId,
    ) -> tokio::sync::watch::Receiver<Option<Result<(), ApiError>>> {
        if let Some(receiver) = self.cancel_existing(exec_id) {
            return receiver;
        }
        let (completion, receiver) = tokio::sync::watch::channel(None);
        self.0.insert(
            exec_id,
            CreationEntry {
                state: CreationState::Awaiting,
                completion,
            },
        );
        receiver
    }

    fn begin_finish(&mut self, exec_id: ExecId, caller_cancelled: bool) -> bool {
        let Some(entry) = self.0.get_mut(&exec_id) else {
            return caller_cancelled;
        };
        let CreationState::Creating { cancelled } = &mut entry.state else {
            return true;
        };
        *cancelled |= caller_cancelled;
        if *cancelled {
            return true;
        }
        if let Some(entry) = self.0.remove(&exec_id) {
            let _ = entry.completion.send(Some(Ok(())));
        }
        false
    }

    fn complete_cancelled(&mut self, exec_id: ExecId, result: Result<(), ApiError>) {
        if let Some(entry) = self.0.remove(&exec_id) {
            let _ = entry.completion.send(Some(result));
        }
    }

    fn expire_waiter(&mut self, exec_id: ExecId, error: ApiError) {
        if self
            .0
            .get(&exec_id)
            .is_some_and(|entry| entry.state == CreationState::Awaiting)
            && let Some(entry) = self.0.remove(&exec_id)
        {
            let _ = entry.completion.send(Some(Err(error)));
        }
    }
}

async fn creation_completion(
    mut receiver: tokio::sync::watch::Receiver<Option<Result<(), ApiError>>>,
) -> Result<(), ApiError> {
    loop {
        if let Some(result) = receiver.borrow().clone() {
            return result;
        }
        receiver.changed().await.map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "execution creation ended without a cancellation result",
            )
        })?;
    }
}

struct CreationArrivalWait<'a> {
    states: &'a StdMutex<CreationStates>,
    exec_id: ExecId,
    armed: bool,
}

impl<'a> CreationArrivalWait<'a> {
    fn new(states: &'a StdMutex<CreationStates>, exec_id: ExecId) -> Self {
        Self {
            states,
            exec_id,
            armed: true,
        }
    }

    fn finish(mut self) {
        self.armed = false;
    }
}

impl Drop for CreationArrivalWait<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.states.lock().expect("creation states").expire_waiter(
                self.exec_id,
                ApiError::new(ApiErrorCode::NotFound, "no such execution"),
            );
        }
    }
}

#[derive(Clone, Copy)]
enum CreationDecision {
    Acknowledge,
    Cancel,
}

struct CreationWait {
    decision: Option<tokio::sync::oneshot::Sender<CreationDecision>>,
}

impl CreationWait {
    fn new(decision: tokio::sync::oneshot::Sender<CreationDecision>) -> Self {
        Self {
            decision: Some(decision),
        }
    }

    fn acknowledge(mut self) {
        if let Some(decision) = self.decision.take() {
            let _ = decision.send(CreationDecision::Acknowledge);
        }
    }

    fn cancel(mut self) {
        if let Some(decision) = self.decision.take() {
            let _ = decision.send(CreationDecision::Cancel);
        }
    }
}

impl Drop for CreationWait {
    fn drop(&mut self) {
        if let Some(decision) = self.decision.take() {
            let _ = decision.send(CreationDecision::Cancel);
        }
    }
}
/// How an execution joins one coordinatorless negotiation.
enum NegotiationPlan {
    Create {
        negotiation_id: arena0_protocol::NegotiationId,
        target_size: u16,
        peers: Vec<PeerId>,
    },
    Join {
        target: Option<NegotiationTarget>,
    },
}

impl NegotiationPlan {
    fn from_admission(admission: &ExecutionAdmission) -> Self {
        match admission {
            ExecutionAdmission::Explicit {
                negotiation_id,
                peers,
            } => Self::Create {
                negotiation_id: *negotiation_id,
                target_size: u16::try_from(peers.peers().len()).unwrap_or(u16::MAX),
                peers: peers.peers().to_vec(),
            },
            ExecutionAdmission::Create {
                negotiation_id,
                participant_count,
            } => Self::Create {
                negotiation_id: *negotiation_id,
                target_size: *participant_count,
                peers: Vec::new(),
            },
            ExecutionAdmission::Join { target } => Self::Join { target: *target },
        }
    }

    fn negotiation_id(&self) -> Option<NegotiationId> {
        match self {
            Self::Create { negotiation_id, .. } => Some(*negotiation_id),
            Self::Join { target } => target.map(|target| target.negotiation_id),
        }
    }
}

enum NextProjection {
    Ready(NextEvent),
    Waiting {
        entry: Arc<ExecutionHandle>,
        schema: ProgramSchema,
    },
}

/// Validate the exact participant target selected by admission against the
/// program's declared fixed size or inclusive range. The creator and every
/// joiner use this same boundary so a variable-size program cannot be
/// rejected by one side of negotiation while accepted by the other.
fn validate_participants(participants: ParticipantCount, target_size: u16) -> Result<(), ApiError> {
    if participants.accepts(target_size) {
        return Ok(());
    }
    Err(ApiError::new(
        ApiErrorCode::BadRequest,
        format!(
            "requested ensemble has {target_size} participants; program accepts {participants}"
        ),
    ))
}

/// A confirmed execution ready for the supervisor.
struct SpawnPlan {
    params: Vec<u8>,
    program: Arc<LoadedProgram>,
    committed: ActivatedSession,
    actor_execution_key: ExecutionKey,
    execution_store: HostExecutionStore,
}

/// One validated negotiation or execution occurrence. `Events` owns every
/// projection, so producers construct this value once.
#[derive(Debug, Clone)]
pub(crate) enum HostEvent {
    HostStopped {
        reason: Option<String>,
        uptime_secs: u64,
    },
    OfferSeen {
        program_id: ProgramHash,
        negotiation_id: NegotiationId,
        creator: PeerId,
        offer_seq: u64,
    },
    Negotiation {
        source: EventSource,
        event: NegotiationEvent,
    },
    Created {
        source: EventSource,
        negotiation_id: Option<NegotiationId>,
        queue_position: Option<usize>,
        origin: ExecCreationOrigin,
    },
    Failed {
        source: EventSource,
        reason: String,
        failure: ExecutionFailureCode,
    },
    SessionStarted {
        source: EventSource,
        ensemble: Vec<PeerId>,
    },
    SessionStep {
        source: EventSource,
        step: u64,
        pre_state: arena0_protocol::StateHash,
        post_state: arena0_protocol::StateHash,
        fuel_used: u64,
        signers: u16,
        participants: u16,
    },
    SessionCallout {
        source: EventSource,
        pending_id: PendingId,
        callout_index: u32,
        name: String,
        prompt: String,
        schema: JsonSchemaDocument,
        context: serde_json::Value,
    },
    SessionCalloutAnswered {
        source: EventSource,
        pending_id: PendingId,
    },
    SessionCompleted {
        source: EventSource,
        outcome: Option<serde_json::Value>,
    },
    SessionAborted {
        source: EventSource,
        step: u64,
        reason: String,
        failure: ExecutionFailureCode,
    },
}

impl HostEvent {
    fn source(&self) -> Option<&EventSource> {
        match self {
            Self::Negotiation { source, .. }
            | Self::Created { source, .. }
            | Self::Failed { source, .. }
            | Self::SessionStarted { source, .. }
            | Self::SessionStep { source, .. }
            | Self::SessionCallout { source, .. }
            | Self::SessionCalloutAnswered { source, .. }
            | Self::SessionCompleted { source, .. }
            | Self::SessionAborted { source, .. } => Some(source),
            Self::HostStopped { .. } | Self::OfferSeen { .. } => None,
        }
    }

    fn system_event(&self) -> Option<SystemEvent> {
        match self {
            Self::HostStopped { .. } | Self::OfferSeen { .. } => None,
            Self::Negotiation { source, event } => Some(SystemEvent::Negotiation {
                source: source.clone(),
                event: event.clone(),
            }),
            Self::Created { source, origin, .. } => Some(SystemEvent::Execution {
                source: source.clone(),
                event: ExecutionEvent::Created { origin: *origin },
            }),
            Self::Failed {
                source, failure, ..
            } => Some(SystemEvent::Execution {
                source: source.clone(),
                event: ExecutionEvent::Terminal {
                    kind: TerminalKind::Failed { failure: *failure },
                },
            }),
            Self::SessionStarted { source, ensemble } => Some(SystemEvent::Execution {
                source: source.clone(),
                event: ExecutionEvent::SessionStarted {
                    ensemble: ensemble.clone(),
                },
            }),
            Self::SessionStep {
                source,
                step,
                pre_state,
                post_state,
                fuel_used,
                signers,
                participants,
            } => Some(SystemEvent::Execution {
                source: source.clone(),
                event: ExecutionEvent::StepCommitted {
                    step: *step,
                    pre_state: *pre_state,
                    post_state: *post_state,
                    fuel_used: *fuel_used,
                    signer_count: *signers,
                    participant_count: *participants,
                },
            }),
            Self::SessionCallout {
                source,
                pending_id,
                callout_index,
                ..
            } => Some(SystemEvent::Execution {
                source: source.clone(),
                event: ExecutionEvent::CalloutRequested {
                    pending_id: *pending_id,
                    callout_index: *callout_index,
                },
            }),
            Self::SessionCalloutAnswered { source, pending_id } => Some(SystemEvent::Execution {
                source: source.clone(),
                event: ExecutionEvent::CalloutAnswered {
                    pending_id: *pending_id,
                },
            }),
            Self::SessionCompleted { source, .. } => Some(SystemEvent::Execution {
                source: source.clone(),
                event: ExecutionEvent::Terminal {
                    kind: TerminalKind::Completed,
                },
            }),
            Self::SessionAborted {
                source,
                step,
                failure,
                ..
            } => Some(SystemEvent::Execution {
                source: source.clone(),
                event: ExecutionEvent::Terminal {
                    kind: TerminalKind::Aborted {
                        step: *step,
                        failure: *failure,
                    },
                },
            }),
        }
    }

    fn correlations(&self) -> (Option<ExecId>, Option<SessionHash>) {
        let Some(source) = self.source() else {
            return (None, None);
        };
        let (exec_id, session_id) = match source {
            EventSource::Negotiation { exec_id, .. } | EventSource::Execution { exec_id, .. } => {
                (*exec_id, None)
            }
            EventSource::Session {
                exec_id,
                session_hash,
                ..
            } => (*exec_id, Some(*session_hash)),
        };
        let session_id = match self {
            Self::Negotiation {
                event:
                    NegotiationEvent::ActivationPrepared { session_hash, .. }
                    | NegotiationEvent::PreparedActivationResumed { session_hash, .. }
                    | NegotiationEvent::ActivationCommitted { session_hash, .. },
                ..
            } => Some(*session_hash),
            _ => session_id,
        };
        (Some(exec_id), session_id)
    }

    fn api_event(&self) -> EventData {
        match self {
            Self::HostStopped {
                reason,
                uptime_secs,
            } => EventData::HostStopped {
                reason: reason.clone(),
                uptime_secs: *uptime_secs,
            },
            Self::OfferSeen {
                program_id,
                negotiation_id,
                creator,
                offer_seq,
            } => EventData::OfferSeen {
                program_id: *program_id,
                negotiation_id: *negotiation_id,
                creator: *creator,
                offer_seq: *offer_seq,
            },
            Self::Negotiation { event, .. } => negotiation_api_event(event),
            Self::Created {
                source,
                negotiation_id,
                queue_position,
                origin,
            } => EventData::Created {
                program_id: match source {
                    EventSource::Execution { program_id, .. } => *program_id,
                    _ => unreachable!("created event requires an execution source"),
                },
                negotiation_id: *negotiation_id,
                queue_position: *queue_position,
                origin: match origin {
                    ExecCreationOrigin::Request => ExecOrigin::Request,
                    ExecCreationOrigin::Recovery => ExecOrigin::Recovery,
                },
            },
            Self::Failed {
                reason, failure, ..
            } => EventData::Terminated {
                reason: reason.clone(),
                failed_class: Some(api_failure(*failure)),
            },
            Self::SessionStarted { ensemble, .. } => EventData::SessionStarted {
                ensemble: ensemble.clone(),
            },
            Self::SessionStep {
                step,
                pre_state,
                post_state,
                fuel_used,
                signers,
                participants,
                ..
            } => EventData::SessionStep {
                step: *step,
                pre_state: *pre_state,
                post_state: *post_state,
                fuel_used: *fuel_used,
                signers: *signers,
                participants: *participants,
            },
            Self::SessionCallout {
                pending_id,
                callout_index,
                name,
                prompt,
                schema,
                context,
                ..
            } => EventData::SessionCallout {
                pending_id: *pending_id,
                callout_index: *callout_index,
                name: name.clone(),
                prompt: prompt.clone(),
                schema: schema.clone(),
                context: context.clone(),
            },
            Self::SessionCalloutAnswered { pending_id, .. } => EventData::SessionCalloutAnswered {
                pending_id: *pending_id,
            },
            Self::SessionCompleted { outcome, .. } => EventData::SessionEnded {
                terminal: arena0_api::SessionTerminal::Completed {
                    outcome: outcome.clone(),
                },
            },
            Self::SessionAborted {
                source,
                step,
                reason,
                failure,
            } => match source {
                EventSource::Session { .. } => EventData::SessionEnded {
                    terminal: arena0_api::SessionTerminal::Aborted {
                        step: *step,
                        reason: reason.clone(),
                    },
                },
                _ => EventData::Terminated {
                    reason: reason.clone(),
                    failed_class: Some(api_failure(*failure)),
                },
            },
        }
    }
}

fn api_failure(failure: ExecutionFailureCode) -> ExecutionFailureKind {
    match failure {
        ExecutionFailureCode::Negotiation => ExecutionFailureKind::Negotiation,
        ExecutionFailureCode::HostStopped => ExecutionFailureKind::HostStopped,
        ExecutionFailureCode::ProgramAborted => ExecutionFailureKind::ProgramAborted,
        ExecutionFailureCode::Runtime => ExecutionFailureKind::Runtime,
        ExecutionFailureCode::InvalidGuestOutput => ExecutionFailureKind::InvalidGuestOutput,
    }
}

/// The daemon event junction. It derives each projection and sequences local
/// API frames before broadcast.
#[derive(Debug, Clone)]
pub(crate) struct Events {
    host: Arc<RwLock<HostInfo>>,
    boot_id: String,
    next_seq: Arc<AtomicU64>,
    events: broadcast::Sender<EventFrame>,
}

impl Events {
    pub(crate) fn new(host: HostInfo) -> Self {
        let (events, _keepalive) = broadcast::channel(EVENT_BUS_CAP);
        Self {
            host: Arc::new(RwLock::new(host)),
            boot_id: hex::encode(rand::random::<[u8; 16]>()),
            next_seq: Arc::new(AtomicU64::new(1)),
            events,
        }
    }

    fn frame(
        &self,
        data: EventData,
        exec_id: Option<ExecId>,
        session_id: Option<SessionHash>,
    ) -> EventFrame {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        self.frame_at(seq, data, exec_id, session_id)
    }

    pub(crate) fn emit(&self, event: HostEvent) {
        let (exec_id, session_id) = event.correlations();
        if let Some(system_event) = event.system_event() {
            crate::system_event::emit(system_event);
        }
        let _ = self
            .events
            .send(self.frame(event.api_event(), exec_id, session_id));
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<EventFrame> {
        self.events.subscribe()
    }

    pub(crate) fn snapshot(&self, data: EventData) -> EventFrame {
        self.frame_at(0, data, None, None)
    }

    pub(crate) fn lagged(&self, seq: u64, skipped: u64) -> EventFrame {
        self.frame_at(seq, EventData::Lagged { skipped }, None, None)
    }

    fn frame_at(
        &self,
        seq: u64,
        data: EventData,
        exec_id: Option<ExecId>,
        session_id: Option<SessionHash>,
    ) -> EventFrame {
        EventFrame::new(
            self.host
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
            self.boot_id.clone(),
            seq,
            unix_time_ms(),
            data,
            exec_id,
            session_id,
        )
        .expect("event junction emitted invalid correlation")
    }
}

/// Daemon-scoped MCP activity junction. Unlike [`Events`], this bus is shared
/// by every Host service in one process so one Unix subscription observes the
/// complete MCP surface.
#[derive(Debug)]
pub(crate) struct Activity {
    boot_id: String,
    next_seq: StdMutex<u64>,
    next_call_id: AtomicU64,
    activity: broadcast::Sender<ActivityFrame>,
}

impl Activity {
    pub(crate) fn new() -> Self {
        let (activity, _keepalive) = broadcast::channel(ACTIVITY_BUS_CAP);
        Self {
            boot_id: hex::encode(rand::random::<[u8; 16]>()),
            next_seq: StdMutex::new(1),
            next_call_id: AtomicU64::new(1),
            activity,
        }
    }

    pub(crate) fn next_call_id(&self) -> String {
        self.next_call_id
            .fetch_add(1, Ordering::Relaxed)
            .to_string()
    }

    pub(crate) fn emit(&self, data: ActivityData) {
        let mut next_seq = self.next_seq.lock().expect("activity publisher");
        let seq = *next_seq;
        *next_seq = seq.wrapping_add(1);
        let frame = ActivityFrame {
            boot_id: self.boot_id.clone(),
            seq,
            ts: unix_time_ms(),
            data,
        };
        let _ = self.activity.send(frame);
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<ActivityFrame> {
        self.activity.subscribe()
    }

    pub(crate) fn lagged(&self, seq: u64, skipped: u64) -> ActivityFrame {
        ActivityFrame {
            boot_id: self.boot_id.clone(),
            seq,
            ts: unix_time_ms(),
            data: ActivityData::Lagged { skipped },
        }
    }
}

/// Owns the daemon Unix listener path and every connection task spawned from it.
#[derive(Debug)]
pub(crate) struct UnixSocket {
    path: PathBuf,
    owned_inode: StdMutex<Option<(u64, u64)>>,
    lease: StdMutex<Option<crate::paths::FileLease>>,
}

impl UnixSocket {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            owned_inode: StdMutex::new(None),
            lease: StdMutex::new(None),
        }
    }

    pub(crate) fn remove_owned_path(&self) {
        let owned = self
            .owned_inode
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(inode) = owned
            && let Ok(metadata) = std::fs::symlink_metadata(&self.path)
            && (metadata.dev(), metadata.ino()) == inode
        {
            let _ = std::fs::remove_file(&self.path);
        }
        self.lease
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }

    pub(crate) fn bind(&self) -> anyhow::Result<UnixListener> {
        crate::paths::validate_path(&self.path).context("validate daemon socket")?;
        let parent = self.path.parent().context("daemon socket needs a parent")?;
        crate::paths::ensure_directory(parent).context("prepare daemon socket parent")?;
        let mut lock_path = self.path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lease = crate::paths::FileLease::acquire_path(std::path::Path::new(&lock_path))
            .context("acquire daemon socket ownership")?;
        match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.file_type().is_socket(),
                    "socket path is occupied: {}",
                    self.path.display()
                );
                match std::os::unix::net::UnixStream::connect(&self.path) {
                    Ok(_) => anyhow::bail!("another listener owns {}", self.path.display()),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                        ) => {}
                    Err(error) => return Err(error).context("probe existing daemon socket"),
                }
                std::fs::remove_file(&self.path).context("remove stale daemon socket")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect daemon socket"),
        }
        let listener = match UnixListener::bind(&self.path) {
            Ok(listener) => listener,
            Err(error) => {
                return Err(error).with_context(|| format!("bind {}", self.path.display()));
            }
        };
        let metadata =
            std::fs::symlink_metadata(&self.path).context("inspect bound daemon socket")?;
        *self
            .owned_inode
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some((metadata.dev(), metadata.ino()));
        *self.lease.lock().unwrap_or_else(|error| error.into_inner()) = Some(lease);
        if let Err(error) = std::fs::set_permissions(
            &self.path,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        ) {
            self.remove_owned_path();
            return Err(error).with_context(|| format!("chmod 0600 {}", self.path.display()));
        }
        tracing::info!(socket = %self.path.display(), "arena0d daemon socket listening");
        Ok(listener)
    }

    pub(crate) async fn listen<H, F>(
        &self,
        listener: UnixListener,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        handler: H,
    ) -> anyhow::Result<()>
    where
        H: Fn(UnixStream) -> F + Clone + Send + 'static,
        F: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let mut connections = JoinSet::new();
        let result = loop {
            if *shutdown.borrow() {
                break Ok(());
            }
            tokio::select! {
                joined = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Err(error)) = joined {
                        tracing::debug!(%error, "connection task failed");
                    }
                }
                accepted = listener.accept() => {
                    let (stream, _addr) = match accepted {
                        Ok(accepted) => accepted,
                        Err(error) => break Err(error).context("accept"),
                    };
                    let connection = handler.clone()(stream);
                    connections.spawn(async move {
                        if let Err(error) = connection.await {
                            tracing::debug!(%error, "connection closed");
                        }
                    });
                }
                _ = shutdown.changed() => break Ok(()),
            }
        };

        connections.abort_all();
        while connections.join_next().await.is_some() {}
        drop(connections);
        self.remove_owned_path();
        result
    }
}

impl Drop for UnixSocket {
    fn drop(&mut self) {
        self.remove_owned_path();
    }
}

fn negotiation_api_event(event: &NegotiationEvent) -> EventData {
    match event {
        NegotiationEvent::PeersChanged { lifecycle, peers } => EventData::NegotiationPeers {
            lifecycle: *lifecycle,
            peers: peers.clone(),
        },
        NegotiationEvent::Started { target_size } => EventData::NegotiationStarted {
            target_size: *target_size,
        },
        NegotiationEvent::OfferAccepted { creator, offer_seq } => {
            EventData::NegotiationOfferAccepted {
                creator: *creator,
                offer_seq: *offer_seq,
            }
        }
        NegotiationEvent::TicketAccepted {
            participant,
            ticket_hash,
            ticket_count,
            target_size,
        } => EventData::NegotiationTicketAccepted {
            participant: *participant,
            ticket_hash: *ticket_hash,
            ticket_count: *ticket_count,
            target_size: *target_size,
        },
        NegotiationEvent::ActivationPrepared {
            participant_count, ..
        } => EventData::NegotiationPrepared {
            participants: *participant_count,
        },
        NegotiationEvent::PreparedActivationResumed {
            participant_count, ..
        } => EventData::NegotiationResumed {
            participants: *participant_count,
        },
        NegotiationEvent::ActivationCommitted {
            participant_count, ..
        } => EventData::NegotiationCommitted {
            participants: *participant_count,
        },
        NegotiationEvent::Retry {
            attempt,
            stage,
            ticket_count,
            sig_count,
            target_size,
        } => EventData::NegotiationRetried {
            attempt: *attempt,
            stage: match stage {
                arena0_protocol::NegotiationStage::Gossiping => NegotiationStage::Gossiping,
                arena0_protocol::NegotiationStage::Prepared => NegotiationStage::Prepared,
            },
            ticket_count: *ticket_count,
            sig_count: *sig_count,
            target_size: *target_size,
        },
        NegotiationEvent::TopicRejoined => EventData::NegotiationRejoined {},
        NegotiationEvent::TimedOut {
            stage,
            ticket_count,
            sig_count,
            target_size,
        } => EventData::NegotiationTimedOut {
            stage: match stage {
                arena0_protocol::NegotiationStage::Gossiping => NegotiationStage::Gossiping,
                arena0_protocol::NegotiationStage::Prepared => NegotiationStage::Prepared,
            },
            ticket_count: *ticket_count,
            sig_count: *sig_count,
            target_size: *target_size,
        },
    }
}

/// A local negotiation window that produced no matching offer.
fn offer_timeout() -> ApiError {
    ApiError::new(
        ApiErrorCode::Negotiation,
        "no matching creator offer before the negotiation deadline",
    )
}

/// Load one imported program and initialize it with the exact JSON
/// parameters that will be bound by negotiation.  Initialization is a typed
/// guest call: no mutable Wasmtime instance crosses this daemon boundary.
fn load_and_initialize(
    program: &Program,
    engine: &WasmtimeEngine,
    params: Vec<u8>,
    context: &str,
) -> anyhow::Result<(Arc<LoadedProgram>, StateHash)> {
    let loaded = engine
        .load(program)
        .map_err(|error| anyhow::anyhow!("{context} program load: {error}"))?;
    let params =
        JsonBytes::try_new(params).map_err(|error| anyhow::anyhow!("{context} params: {error}"))?;
    let initialized = loaded
        .initialize(InitializeCall::new(params))
        .map_err(|error| anyhow::anyhow!("{context} initialize: {error}"))?;
    Ok((loaded, StateHash::of(initialized.shared.as_bytes())))
}

/// Load and initialize one program in a blocking worker. Wasmtime loading and
/// guest execution are synchronous, while request/negotiation orchestration
/// remains on the async daemon runtime.
async fn load_for(
    program: &Program,
    engine: Arc<WasmtimeEngine>,
    params: Vec<u8>,
    context: &str,
) -> Result<(Arc<LoadedProgram>, StateHash), ApiError> {
    let program = program.clone();
    let context = context.to_owned();
    let error_context = context.clone();
    tokio::task::spawn_blocking(move || load_and_initialize(&program, &engine, params, &context))
        .await
        .map_err(|error| {
            ApiError::new(ApiErrorCode::Internal, format!("{error_context}: {error}"))
        })?
        .map_err(|error| ApiError::new(ApiErrorCode::Execution, error.to_string()))
}

/// Load a committed program and verify the immutable guest's initial
/// shared state against the negotiation's locked boot fact.
async fn load_checked(
    program: &Program,
    offer_data: &OfferData,
    engine: Arc<WasmtimeEngine>,
    context: &str,
) -> Result<Arc<LoadedProgram>, ApiError> {
    let (loaded, initial_state) = load_for(
        program,
        engine,
        offer_data.params.as_bytes().to_vec(),
        context,
    )
    .await?;
    if initial_state != offer_data.initial_state {
        return Err(ApiError::new(
            ApiErrorCode::Negotiation,
            format!("{context} boot facts do not match the local runtime"),
        ));
    }
    Ok(loaded)
}

/// The API operation owner for one Host.
///
/// [`HostService`] owns one Host's execution supervisors and service tasks.
/// The public [`crate::Daemon`] owns API connections and the local runtime Ensemble;
/// this type deliberately does not stop the shared runtime or transport.
pub(crate) struct HostService {
    name: String,
    peer_id: PeerId,
    identity: Arc<NodeKeys>,
    transport: Arc<dyn Transport + Sync>,
    keystore: Arc<Keystore>,
    catalog: ProgramCatalog,
    store: StoreHandle,
    engine: Arc<WasmtimeEngine>,
    startup: Arc<StartupTimeline>,
    pub(crate) events: Events,
    started: StdInstant,
    /// Protocol ingress, negotiation, and execution machinery shared with the
    /// greybox harness.
    runtime: Arc<arena0_node::Host>,
    /// A standalone greybox service owns its Host; services installed into the
    /// public [`crate::Daemon`] borrow the supervisor-owned Host instead.
    owns_runtime: bool,
    execs: Arc<ExecutionHandles>,
    /// Per-id rendezvous between caller-owned creation and cancellation.
    creation_states: StdMutex<CreationStates>,
    negotiations_pending: AtomicUsize,
    /// Service tasks and negotiation drives. The runtime owns its accept path.
    tasks: TokioMutex<JoinSet<()>>,
    /// Guards service cleanup when both `serve` and the supervisor observe a
    /// shutdown at nearly the same time.
    stopped: AtomicBool,
    /// A provisional Host must be drained without publishing a lifecycle event.
    published: AtomicBool,
    host_stopped_emitted: AtomicBool,
}

/// Inputs needed to start one [`HostService`].
pub(crate) struct HostServiceInit {
    pub(crate) name: String,
    pub(crate) transport: Arc<dyn Transport + Sync>,
    pub(crate) keystore: Arc<Keystore>,
    pub(crate) catalog: ProgramCatalog,
    pub(crate) store: StoreHandle,
    pub(crate) engine: Arc<WasmtimeEngine>,
    pub(crate) startup: Arc<StartupTimeline>,
}

impl HostService {
    pub(crate) fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    /// One projection of identity and the latest persisted harness metadata.
    pub(crate) fn host_info(&self) -> HostInfo {
        self.events
            .host
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub(crate) async fn set_user_agent(&self, user_agent: String) -> anyhow::Result<()> {
        self.store.set_user_agent(user_agent.clone()).await?;
        self.events
            .host
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .user_agent = Some(user_agent);
        Ok(())
    }

    /// Start the daemon's protocol runtime and host services.
    #[cfg(test)]
    fn start(init: HostServiceInit) -> anyhow::Result<Arc<Self>> {
        let identity = Arc::new(init.keystore.active_crypto()?);
        let runtime =
            arena0_node::Host::start(identity, Arc::clone(&init.transport), init.store.clone());
        Self::start_with_runtime_owned(init, runtime, true)
    }

    /// Start one daemon service around an already-started runtime host.
    ///
    /// The local ensemble composition uses this seam to give every per-socket
    /// service the matching [`arena0_node::Host`] created by the shared
    /// runtime topology. This service's Host is installed before the accept
    /// router starts, so its socket and execution paths share the same Host.
    pub(crate) fn start_with_runtime(
        init: HostServiceInit,
        runtime: Arc<arena0_node::Host>,
    ) -> anyhow::Result<Arc<Self>> {
        Self::start_with_runtime_owned(init, runtime, false)
    }

    fn start_with_runtime_owned(
        init: HostServiceInit,
        runtime: Arc<arena0_node::Host>,
        owns_runtime: bool,
    ) -> anyhow::Result<Arc<Self>> {
        let HostServiceInit {
            name,
            transport,
            keystore,
            catalog,
            store,
            engine,
            startup,
        } = init;

        let identity = runtime.identity_keys();
        let peer_id = identity.peer_id();
        anyhow::ensure!(
            runtime.peer_id == peer_id,
            "runtime host identity {runtime_peer} does not match keystore identity {peer_id}",
            runtime_peer = runtime.peer_id,
        );
        let execs = Arc::new(ExecutionHandles::new(store.clone()));
        let events = Events::new(HostInfo {
            id: name.clone(),
            peer_id,
            user_agent: None,
        });
        let tasks = JoinSet::new();
        let this = Arc::new(Self {
            name,
            peer_id,
            identity,
            transport,
            keystore,
            catalog,
            store,
            engine,
            startup,
            events,
            started: StdInstant::now(),
            runtime,
            owns_runtime,
            execs,
            creation_states: StdMutex::new(CreationStates::default()),
            negotiations_pending: AtomicUsize::new(0),
            tasks: TokioMutex::new(tasks),
            stopped: AtomicBool::new(false),
            published: AtomicBool::new(false),
            host_stopped_emitted: AtomicBool::new(false),
        });
        Ok(this)
    }

    async fn publish_negotiation_peers(&self, entry: &ExecutionHandle, peers: Vec<PeerId>) {
        let Ok(Some(request)) = entry.request().await else {
            tracing::error!(exec_id = %entry.exec_id(), "read execution for negotiation event failed");
            return;
        };
        let Some(negotiation_id) = request.negotiation_id() else {
            // An open Join has no negotiation event identity until its first
            // authenticated offer is durably selected.
            return;
        };
        self.events.emit(HostEvent::Negotiation {
            source: EventSource::Negotiation {
                peer_id: self.peer_id,
                exec_id: entry.exec_id(),
                program_id: request.program_hash(),
                negotiation_id,
            },
            event: NegotiationEvent::PeersChanged {
                lifecycle: entry
                    .lifecycle()
                    .await
                    .unwrap_or(ExecLifecycle::Negotiating),
                peers,
            },
        });
    }

    /// Stop service tasks after first draining live execution actors. The actor
    /// owns terminal persistence; this layer never writes duplicate rows.
    ///
    /// Runtime and transport shutdown belongs to the owner that created them:
    /// a standalone greybox service owns both, while the public [`crate::Daemon`]
    /// stops its shared [`arena0_node::Ensemble`] once after all services
    /// have drained.
    pub(crate) async fn stop(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        let active = self.execs.all_live();
        // Signal and await actors before stopping negotiation/service tasks.
        self.execs.stop().await;
        let mut tasks = self.tasks.lock().await;
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        drop(tasks);
        for entry in &active {
            let Ok(Some(negotiation_id)) = entry.negotiation_id().await else {
                continue;
            };
            if let Err(error) = self
                .transport
                .release_negotiation_blobs(negotiation_id)
                .await
            {
                tracing::warn!(%negotiation_id, %error, "failed to release stopped negotiation blobs");
            }
        }
        for entry in &active {
            let Ok(Some(session_id)) = entry.session_id().await else {
                continue;
            };
            if let Err(error) = self.transport.release_session_blobs(session_id).await {
                tracing::warn!(%session_id, %error, "failed to release stopped session blobs");
            }
        }
        if self.owns_runtime {
            self.runtime.stop().await;
            self.transport.close().await;
        }
        if self.published.load(Ordering::Acquire)
            && !self.host_stopped_emitted.swap(true, Ordering::AcqRel)
        {
            self.events.emit(HostEvent::HostStopped {
                reason: Some("daemon.stop".into()),
                uptime_secs: self.started.elapsed().as_secs(),
            });
        }
        tracing::info!(host = %self.name, "arena0d Host stopped");
    }

    /// Publish the Host lifecycle after recovery and roster publication.
    pub(crate) fn mark_published(&self) {
        self.published.store(true, Ordering::Release);
        self.startup
            .host_progress(StartupStage::HostReady, &self.name);
    }

    /// Clear negotiation bookkeeping and record a failed drive.
    async fn finish_negotiation(
        self: &Arc<Self>,
        entry: &Arc<ExecutionHandle>,
        result: Result<(), ApiError>,
    ) {
        match entry.negotiation_id().await {
            Ok(Some(negotiation_id)) => {
                if let Err(error) = self
                    .transport
                    .release_negotiation_blobs(negotiation_id)
                    .await
                {
                    tracing::warn!(%negotiation_id, %error, "failed to release negotiation blobs");
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::error!(exec_id = %entry.exec_id(), %error, "read negotiation id for cleanup failed")
            }
        }
        self.negotiations_pending.fetch_sub(1, Ordering::SeqCst);
        if let Err(error) = result {
            if let Ok(Some(session_id)) = entry.session_id().await
                && let Err(release_error) = self.transport.release_session_blobs(session_id).await
            {
                tracing::warn!(%session_id, error = %release_error, "failed to release failed session blobs");
            }
            if let Err(store_error) = self.record_negotiation_failure(entry, &error.message).await {
                tracing::error!(exec_id = %entry.exec_id(), error = %store_error, "persist negotiation failure failed");
            } else if let Ok(source) = entry.event_source().await {
                self.events.emit(HostEvent::Failed {
                    source,
                    reason: error.message,
                    failure: ExecutionFailureCode::Negotiation,
                });
            }
            self.execs.remove(&entry.exec_id());
        }
        entry.notify();
    }

    /// Record a pre-activation negotiation failure through a fresh writer only
    /// when no durable activation exists. A prepared activation is resumable
    /// evidence and must never be converted into a terminal request failure.
    async fn record_negotiation_failure(
        &self,
        entry: &ExecutionHandle,
        reason: &str,
    ) -> anyhow::Result<()> {
        if entry.activation().await?.is_some() {
            return Ok(());
        }
        let mut writer = self.runtime.claim_execution(entry.exec_id())?;
        writer.record_execution_request_failure(reason).await?;
        Ok(())
    }

    /// Build the startup event for this Host service.
    #[must_use]
    pub(crate) fn host_started_frame(&self) -> EventFrame {
        self.events.snapshot(EventData::HostStarted {
            version: env!("CARGO_PKG_VERSION").to_string(),
            transport_key: AgentPubKey(self.peer_id.0),
            abi_version: ABI_VERSION,
        })
    }

    /// Finish recovery before the supervisor publishes this Host.
    pub(crate) async fn prepare(self: &Arc<Self>) -> anyhow::Result<()> {
        self.startup
            .host_progress(StartupStage::HostStarting, &self.name);
        let user_agent = self.store.load_user_agent().await?;
        self.events
            .host
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .user_agent = user_agent;
        if let Err(error) = self.resume_durable().await {
            self.startup.host_progress(StartupStage::Failed, &self.name);
            return Err(error);
        }
        Ok(())
    }

    /// Rebuild actors with unfinished protocol, proof, or delivery work from
    /// durable request, activation, and execution projections. A lost process-local handle is not a
    /// protocol failure: prepared activation evidence is resumed in place and
    /// committed activations go directly to the actor.
    async fn resume_durable(self: &Arc<Self>) -> anyhow::Result<()> {
        const RECOVERY_PAGE_SIZE: usize = 128;
        let mut cursor = RecoveryCursor::start();
        loop {
            let page = self
                .store
                .list_recovery_candidates(cursor, RECOVERY_PAGE_SIZE)
                .await?;
            let next = page.next_cursor();
            for candidate in page.into_candidates() {
                self.resume_candidate(candidate).await?;
            }
            let Some(next) = next else {
                return Ok(());
            };
            anyhow::ensure!(
                next > cursor,
                "recovery projection returned a non-advancing cursor"
            );
            cursor = next;
        }
    }

    /// Resume one store-owned candidate. All validation that can prove a
    /// committed execution is unrecoverable happens before a live handle is
    /// registered; failures then cross the same durable protocol boundary as
    /// actor failures.
    async fn resume_candidate(
        self: &Arc<Self>,
        candidate: RecoveryCandidate,
    ) -> anyhow::Result<()> {
        let request = candidate.request().clone();
        let exec_id = request.execution_id();
        if self.execs.get(&exec_id).is_some() {
            return Ok(());
        }
        self.events.emit(HostEvent::Created {
            source: EventSource::Execution {
                peer_id: self.peer_id,
                exec_id,
                program_id: request.program_hash(),
            },
            negotiation_id: request.negotiation_id(),
            queue_position: None,
            origin: ExecCreationOrigin::Recovery,
        });

        // The recovery page intentionally contains only bounded metadata.
        // Load each potentially large aggregate separately and complete all
        // validation before claiming or registering a live execution.
        let page_activation_status = candidate.activation_status();
        let page_session_id = candidate.session_id();
        let page_execution_present = candidate.has_execution();
        let page_program = candidate.program();
        let record = match self.store.load_activation(exec_id).await {
            Ok(record) => record,
            Err(error) => {
                return self
                    .fail_recovery_candidate(
                        candidate,
                        page_execution_present,
                        format!("activation cannot be recovered: {error}"),
                    )
                    .await;
            }
        };
        let loaded_activation_metadata = record
            .as_ref()
            .map(|record| (record.status(), record.session_id()));
        if loaded_activation_metadata != page_activation_status.zip(page_session_id) {
            return self
                .fail_recovery_candidate(
                    candidate,
                    page_execution_present,
                    "activation metadata changed during recovery",
                )
                .await;
        }
        let execution = match self.store.load_execution(exec_id).await {
            Ok(execution) => execution,
            Err(error) => {
                return self
                    .fail_recovery_candidate(
                        candidate,
                        page_execution_present,
                        format!("execution cannot be recovered: {error}"),
                    )
                    .await;
            }
        };
        let execution_present = execution.is_some();
        if execution_present != page_execution_present {
            return self
                .fail_recovery_candidate(
                    candidate,
                    execution_present || page_execution_present,
                    "execution presence changed during recovery",
                )
                .await;
        }

        if record.is_none() && execution_present {
            return self
                .fail_recovery_candidate(
                    candidate,
                    true,
                    "execution aggregate has no durable activation",
                )
                .await;
        }
        if record
            .as_ref()
            .is_some_and(|record| record.status() == ActivationRecordStatus::Prepared)
            && execution_present
        {
            return self
                .fail_recovery_candidate(
                    candidate,
                    true,
                    "prepared activation has a durable execution aggregate",
                )
                .await;
        }
        if let Some(execution) = &execution {
            let Some(activation) = record.as_ref().and_then(ActivationRecord::activation) else {
                return self
                    .fail_recovery_candidate(
                        candidate,
                        true,
                        "execution aggregate has no committed activation",
                    )
                    .await;
            };
            if execution.binding().activation() != activation
                || execution.binding().program_hash() != request.program_hash()
                || execution.producer() != self.peer_id
            {
                return self
                    .fail_recovery_candidate(
                        candidate,
                        true,
                        "execution aggregate does not match its request, activation, or Host",
                    )
                    .await;
            }
        }

        let stored_program = match self.store.load_program(request.program_hash()).await {
            Ok(program) => program,
            Err(error) => {
                return self
                    .fail_recovery_candidate(
                        candidate,
                        execution_present,
                        format!("program cannot be recovered: {error}"),
                    )
                    .await;
            }
        };
        if stored_program.is_some() != page_program.is_some() {
            return self
                .fail_recovery_candidate(
                    candidate,
                    execution_present,
                    "program presence changed during recovery",
                )
                .await;
        }
        let program = match stored_program {
            Some(stored) => match Program::try_from(stored.wasm().to_vec()) {
                Ok(program) => program,
                Err(error) => {
                    return self
                        .fail_recovery_candidate(
                            candidate,
                            execution_present,
                            format!("registered program cannot be recovered: {error}"),
                        )
                        .await;
                }
            },
            None => {
                return self
                    .fail_recovery_candidate(
                        candidate,
                        execution_present,
                        format!("program {} is not registered", request.program_hash()),
                    )
                    .await;
            }
        };

        let Some(record) = record else {
            let execution_store = self.runtime.claim_execution(exec_id)?;
            let entry = self.execs.register_live(exec_id);
            self.negotiations_pending.fetch_add(1, Ordering::SeqCst);
            let daemon = Arc::clone(self);
            let plan = NegotiationPlan::from_admission(request.admission());
            let params = request.params().map(|params| params.as_bytes().to_vec());
            let span = tracing::info_span!(
                "negotiation",
                exec_id = %exec_id,
                program_id = %request.program_hash(),
                negotiation_id = ?request.negotiation_id(),
            );
            self.tasks.lock().await.spawn(
                async move {
                    let result = daemon
                        .drive_negotiation(
                            &entry,
                            request.program_hash(),
                            params,
                            plan,
                            execution_store,
                        )
                        .await;
                    daemon.finish_negotiation(&entry, result).await;
                }
                .instrument(span),
            );
            return Ok(());
        };

        match record.status() {
            ActivationRecordStatus::Prepared => {
                let execution_store = self.runtime.claim_execution(exec_id)?;
                let entry = self.execs.register_live(exec_id);
                self.negotiations_pending.fetch_add(1, Ordering::SeqCst);
                let daemon = Arc::clone(self);
                let span = tracing::info_span!(
                    "negotiation",
                    exec_id = %exec_id,
                    program_id = %request.program_hash(),
                    negotiation_id = ?request.negotiation_id(),
                );
                self.tasks.lock().await.spawn(
                    async move {
                        let result = daemon
                            .drive_prepared_negotiation(&entry, record, execution_store)
                            .await;
                        daemon.finish_negotiation(&entry, result).await;
                    }
                    .instrument(span),
                );
                Ok(())
            }
            ActivationRecordStatus::Committed => {
                let Some(activation) = record.activation().cloned() else {
                    return self
                        .fail_recovery_candidate(
                            candidate,
                            execution_present,
                            "committed activation has no activation payload",
                        )
                        .await;
                };
                let (loaded, initial_state) = match load_for(
                    &program,
                    Arc::clone(&self.engine),
                    activation.offer().data().params.as_bytes().to_vec(),
                    "recovery",
                )
                .await
                {
                    Ok(result) => result,
                    Err(error) => {
                        return self
                            .fail_recovery_candidate(
                                candidate,
                                execution_present,
                                format!(
                                    "committed execution cannot load its program: {}",
                                    error.message
                                ),
                            )
                            .await;
                    }
                };
                if initial_state != activation.offer().data().initial_state {
                    return self
                        .fail_recovery_candidate(
                            candidate,
                            execution_present,
                            "committed execution initial state no longer matches activation",
                        )
                        .await;
                }
                let committed = match ActivatedSession::new(activation.clone()) {
                    Ok(committed) => committed,
                    Err(error) => {
                        return self
                            .fail_recovery_candidate(
                                candidate,
                                execution_present,
                                format!("committed activation cannot be reconstructed: {error}"),
                            )
                            .await;
                    }
                };
                let mut execution_store = self.runtime.claim_execution(exec_id)?;
                let salt = match execution_store
                    .load_or_create_execution_salt(unix_time_ms())
                    .await
                {
                    Ok(salt) => salt,
                    Err(error) => {
                        drop(execution_store);
                        return self
                            .fail_recovery_candidate(
                                candidate,
                                execution_present,
                                format!("committed execution salt cannot be recovered: {error}"),
                            )
                            .await;
                    }
                };
                let actor_key = match self.runtime.execution_key(
                    &salt,
                    &exec_id,
                    &activation.offer().data().negotiation_id,
                ) {
                    Ok(key) => key,
                    Err(error) => {
                        drop(execution_store);
                        return self
                            .fail_recovery_candidate(
                                candidate,
                                execution_present,
                                format!("committed execution crypto cannot be recovered: {error}"),
                            )
                            .await;
                    }
                };
                let entry = self.execs.register_live(exec_id);
                if let Err(error) = self
                    .spawn(
                        &entry,
                        SpawnPlan {
                            params: activation.offer().data().params.as_bytes().to_vec(),
                            program: loaded,
                            committed,
                            actor_execution_key: actor_key,
                            execution_store,
                        },
                    )
                    .await
                {
                    self.execs.remove(&exec_id);
                    return self
                        .fail_recovery_candidate(
                            candidate,
                            execution_present,
                            format!(
                                "committed execution actor cannot be spawned: {}",
                                error.message
                            ),
                        )
                        .await;
                }
                Ok(())
            }
        }
    }

    /// Persist a recovery failure at the authoritative lifecycle boundary.
    /// Requests without an execution aggregate use the request failure root;
    /// committed aggregates use the Host's authenticated fail/interrupt path.
    async fn fail_recovery_candidate(
        &self,
        candidate: RecoveryCandidate,
        execution_present: bool,
        reason: impl Into<String>,
    ) -> anyhow::Result<()> {
        let exec_id = candidate.request().execution_id();
        let reason = recovery_reason(reason.into());
        if execution_present {
            let execution_store = self.runtime.claim_execution(exec_id)?;
            self.runtime
                .fail_recovered_execution(execution_store, reason.clone())
                .await
                .map_err(|error| anyhow::anyhow!("fail recovered execution {exec_id}: {error}"))?;
        } else {
            let mut execution_store = self.runtime.claim_execution(exec_id)?;
            match execution_store
                .record_execution_request_failure(reason.clone())
                .await?
            {
                ExecutionRequestFailureOutcome::Recorded
                | ExecutionRequestFailureOutcome::AlreadyRecorded => {}
                ExecutionRequestFailureOutcome::Conflict => {
                    anyhow::bail!("recovery request {exec_id} has a conflicting durable failure")
                }
            }
        }
        self.events.emit(HostEvent::Failed {
            source: recovery_event_source(self.peer_id, &candidate),
            reason,
            failure: ExecutionFailureCode::Runtime,
        });
        self.execs.remove(&exec_id);
        Ok(())
    }

    /// Stream matching event frames on a unix connection until the client hangs up.
    pub(crate) async fn stream_events_unix<R, W>(
        &self,
        filter: EventFilter,
        mut rx: broadcast::Receiver<EventFrame>,
        read: &mut R,
        write: &mut W,
    ) -> anyhow::Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut pending_skipped = 0u64;
        let mut sink = [0u8; 256];
        loop {
            tokio::select! {
                recv = rx.recv() => match recv {
                    Ok(frame_msg) => {
                        if pending_skipped > 0 {
                            let lagged = self.events.lagged(
                                frame_msg.seq.saturating_sub(1),
                                pending_skipped,
                            );
                            frame::write_frame(write, &lagged).await?;
                            pending_skipped = 0;
                        }
                        if filter.matches(&frame_msg) {
                            frame::write_frame(write, &frame_msg).await?;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        pending_skipped = pending_skipped.saturating_add(skipped);
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
                // Detect client disconnect: a subscriber sends nothing, so any read
                // that returns 0 (EOF) or errors means the connection is gone.
                n = read.read(&mut sink) => {
                    if matches!(n, Ok(0) | Err(_)) {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Stream daemon-wide MCP activity until the client hangs up.
    pub(crate) async fn stream_activity_unix<R, W>(
        activity: &Activity,
        mut rx: broadcast::Receiver<ActivityFrame>,
        read: &mut R,
        write: &mut W,
    ) -> anyhow::Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut pending_skipped = 0u64;
        let mut sink = [0u8; 256];
        loop {
            tokio::select! {
                recv = rx.recv() => match recv {
                    Ok(frame_msg) => {
                        if pending_skipped > 0 {
                            let lagged = activity.lagged(
                                frame_msg.seq.saturating_sub(1),
                                pending_skipped,
                            );
                            frame::write_frame(write, &lagged).await?;
                            pending_skipped = 0;
                        }
                        frame::write_frame(write, &frame_msg).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        pending_skipped = pending_skipped.saturating_add(skipped);
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
                n = read.read(&mut sink) => {
                    if matches!(n, Ok(0) | Err(_)) {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Dispatch one non-streaming request to its handler. The single method table
    /// both transports share.
    pub(crate) async fn dispatch(self: &Arc<Self>, req: HostRequest) -> Response {
        match req {
            HostRequest::Info => self.host_status().await.map(ResponseOk::HostStatus),
            HostRequest::IdNew { label } => {
                let ks = Arc::clone(&self.keystore);
                blocking(move || ks.new_identity(label))
                    .await
                    .map(ResponseOk::Id)
            }
            HostRequest::IdList => {
                let ks = Arc::clone(&self.keystore);
                blocking(move || ks.list()).await.map(ResponseOk::IdList)
            }
            HostRequest::IdShow { id } => {
                let ks = Arc::clone(&self.keystore);
                blocking(move || ks.show(&id)).await.map(ResponseOk::Id)
            }
            HostRequest::IdRemove { id } => {
                let ks = Arc::clone(&self.keystore);
                blocking(move || ks.remove(&id))
                    .await
                    .map(|()| ResponseOk::Ack)
            }

            HostRequest::ProgramList => self
                .catalog
                .list()
                .await
                .map(ResponseOk::ProgramList)
                .map_err(|error| {
                    ApiError::new(ApiErrorCode::Storage, format!("list programs: {error}"))
                }),
            HostRequest::ProgramGet { program } => {
                let program_id = self.resolve_program(&program).await?;
                match self.catalog.detail(program_id).await.map_err(|error| {
                    ApiError::new(ApiErrorCode::Storage, format!("get program: {error}"))
                })? {
                    Some(detail) => Ok(ResponseOk::Program(Box::new(detail))),
                    None => Err(ApiError::new(ApiErrorCode::NotFound, "no such program")),
                }
            }
            HostRequest::ProgramImport { wasm } => {
                let (id, _) = self
                    .catalog
                    .import(wasm, &self.engine, unix_time_ms())
                    .await
                    .map_err(catalog_api_error)?;
                self.catalog
                    .detail(id)
                    .await
                    .map_err(|error| {
                        ApiError::new(
                            ApiErrorCode::Storage,
                            format!("get imported program: {error}"),
                        )
                    })?
                    .map(|detail| ResponseOk::Program(Box::new(detail)))
                    .ok_or_else(|| {
                        ApiError::new(ApiErrorCode::Internal, "imported program vanished")
                    })
            }
            HostRequest::ProgramRemove { program } => {
                let program_id = self.resolve_program(&program).await?;
                let removed = self
                    .catalog
                    .remove(program_id, unix_time_ms())
                    .await
                    .map_err(|error| {
                        ApiError::new(ApiErrorCode::Storage, format!("remove program: {error}"))
                    })?;
                if matches!(removed, arena0_store::ProgramRemoveOutcome::Removed) {
                    Ok(ResponseOk::Ack)
                } else {
                    Err(ApiError::new(ApiErrorCode::NotFound, "no such program"))
                }
            }

            HostRequest::ExecNew {
                exec_id,
                program,
                params,
                ensemble,
            } => self.new_exec(exec_id, program, params, ensemble).await,
            HostRequest::ExecList => self.exec_statuses().await.map(ResponseOk::ExecList),
            HostRequest::ExecStatus { exec_id } => {
                self.exec_status(exec_id).await.map(ResponseOk::Status)
            }
            HostRequest::ExecInspect {
                exec_id,
                private_from,
                private_limit,
            } => self
                .exec_inspect(exec_id, private_from, private_limit)
                .await
                .map(ResponseOk::Inspection),
            HostRequest::ExecAwait { exec_id, until } => {
                let status = self.exec_status(exec_id).await?;
                let lifecycle = if satisfies(status.lifecycle(), until) {
                    status.lifecycle()
                } else if let Some(entry) = self.execs.get(&exec_id) {
                    let deadline = Instant::now() + NEGOTIATION_TIMEOUT;
                    entry.await_state(until, deadline).await?
                } else {
                    return Err(ApiError::new(
                        ApiErrorCode::Execution,
                        "execution has no live driver",
                    ));
                };
                Ok(ResponseOk::Awaited {
                    exec_id,
                    exec_state: lifecycle,
                    reason: self
                        .store
                        .load_execution_request(exec_id)
                        .await
                        .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
                        .and_then(|request| request.failure().map(str::to_owned)),
                })
            }
            HostRequest::ExecNext { exec_id } => self.next(exec_id).await.map(ResponseOk::Next),
            HostRequest::ExecSubmit {
                exec_id,
                pending_id,
                answer,
            } => self.submit(exec_id, pending_id, answer).await,
            HostRequest::ExecQuery { exec_id, query } => self.query(exec_id, query).await,
            HostRequest::ExecView { exec, width, color } => self.view(exec, width, color).await,
            HostRequest::ExecTrace { exec_id, from, to } => {
                self.trace(exec_id, from, to).await.map(ResponseOk::Trace)
            }
            HostRequest::ExecCancelCreation { exec_id } => {
                self.withdraw_or_cancel_creation(exec_id).await
            }
            HostRequest::ExecWithdraw { exec_id } => self.withdraw_negotiation(exec_id).await,
            HostRequest::ExecTerminate { exec_id, reason } => match self.execs.get(&exec_id) {
                Some(entry) => entry.terminate(reason).await.map(|()| ResponseOk::Ack),
                None => Err(ApiError::new(ApiErrorCode::NotFound, "no such execution")),
            },

            // `events.subscribe` is handled by the daemon connection loop, not here.
            HostRequest::EventsSubscribe { .. } => Err(ApiError::new(
                ApiErrorCode::BadRequest,
                "events.subscribe must be the only method on its connection",
            )),
            HostRequest::ReceiptGet { receipt } => Ok(ResponseOk::Receipt(Box::new(
                self.resolve_receipt(receipt).await?,
            ))),
            HostRequest::ReceiptImport { receipt } => self.import_receipt(*receipt).await,
            HostRequest::ReceiptList => {
                let receipts = self
                    .store
                    .list_receipts(4_096)
                    .await
                    .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
                Ok(ResponseOk::ReceiptList(
                    receipts.into_iter().map(receipt_list_entry).collect(),
                ))
            }
            HostRequest::ReceiptVerify { receipt, full } => self.verify(receipt, full).await,
        }
    }

    /// Resolve a program handle / short hash / full id to a `ProgramHash`.
    async fn resolve_program(&self, reference: &str) -> Result<ProgramHash, ApiError> {
        let reference = reference.to_string();
        match self.catalog.resolve(&reference).await.map_err(|error| {
            ApiError::new(ApiErrorCode::Storage, format!("resolve program: {error}"))
        })? {
            Ok(id) => Ok(id),
            Err(ProgramRefError::NotFound { reference }) => Err(ApiError::new(
                ApiErrorCode::NotFound,
                format!("no program matches '{reference}'"),
            )),
            Err(e @ ProgramRefError::Ambiguous { .. }) => {
                Err(ApiError::new(ApiErrorCode::Ambiguous, e.to_string()))
            }
        }
    }

    async fn exec_statuses(&self) -> Result<Vec<ExecStatus>, ApiError> {
        let requests = self
            .store
            .list_execution_requests(4_096)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
        let mut statuses = Vec::with_capacity(requests.len());
        for request in requests {
            statuses.push(self.project_exec_status(request.execution_id()).await?);
        }
        Ok(statuses)
    }

    async fn exec_status(&self, exec_id: ExecId) -> Result<ExecStatus, ApiError> {
        self.project_exec_status(exec_id).await
    }

    async fn exec_inspect(
        &self,
        exec_id: ExecId,
        private_from: Option<u64>,
        private_limit: u16,
    ) -> Result<ExecutionInspection, ApiError> {
        let private_limit = usize::from(private_limit);
        if private_limit == 0 || private_limit > MAX_PRIVATE_INSPECTION_RECORDS {
            return Err(ApiError::new(
                ApiErrorCode::BadRequest,
                format!(
                    "private inspection limit must be between 1 and {}",
                    MAX_PRIVATE_INSPECTION_RECORDS
                ),
            ));
        }
        let status = self.project_exec_status(exec_id).await?;
        let activation = self
            .store
            .load_activation(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .map(project_activation_inspection);
        let page = match self
            .store
            .load_execution(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
        {
            Some(_) => self
                .store
                .read_private_summaries(exec_id, private_from, private_limit)
                .await
                .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?,
            None => {
                return Ok(empty_execution_inspection(
                    status,
                    activation,
                    private_from.unwrap_or(0),
                ));
            }
        };
        let private_from = page.from();
        let private_total = page.total();
        let private_next = page.next();
        let private = page
            .into_summaries()
            .into_iter()
            .map(project_private_commit_summary)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ExecutionInspection {
            status,
            activation,
            private_from,
            private,
            private_total,
            private_next,
        })
    }

    async fn project_exec_status(&self, exec_id: ExecId) -> Result<ExecStatus, ApiError> {
        let request = self
            .store
            .load_execution_request(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "no such execution"))?;
        let activation = self
            .store
            .load_activation(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
        let state = self
            .store
            .load_execution(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
        let pending = if state.is_some() {
            self.store
                .list_pending_requests(exec_id)
                .await
                .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
                .into_iter()
                .find(|request| matches!(request, arena0_store::PendingRequest::Callout { .. }))
        } else {
            None
        };
        let receipt_available = if let Some(state) = &state {
            self.store
                .load_receipt(state.binding().session_id())
                .await
                .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
                .is_some()
        } else {
            false
        };
        project_exec_status_facts(
            self.peer_id,
            request,
            activation,
            state,
            pending,
            receipt_available,
        )
        .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))
    }

    async fn active_execution_count(&self) -> Result<usize, ApiError> {
        let executions = self
            .store
            .list_executions(4_096)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
        Ok(executions
            .iter()
            .filter(|state| !state.status().is_terminal())
            .count())
    }

    pub(crate) async fn host_status(&self) -> Result<HostStatus, ApiError> {
        let programs = self
            .catalog
            .list()
            .await
            .map_err(|error| {
                ApiError::new(ApiErrorCode::Storage, format!("list programs: {error}"))
            })?
            .len();
        Ok(HostStatus {
            host: self.host_info(),
            transport_key: AgentPubKey(self.peer_id.0),
            programs,
            execs_active: self.active_execution_count().await?,
        })
    }

    async fn project_next(&self, exec_id: ExecId) -> Result<NextProjection, ApiError> {
        let request = self
            .store
            .load_execution_request(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "no such execution"))?;
        if let Some(reason) = request.failure() {
            return Ok(NextProjection::Ready(NextEvent::Failed {
                reason: reason.to_owned(),
            }));
        }
        let schema = self
            .catalog
            .schema(request.program_hash())
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .ok_or_else(|| {
                ApiError::new(ApiErrorCode::NotFound, "execution program was removed")
            })?;
        if let Some(entry) = self.execs.get(&exec_id) {
            return match entry.next_durable(&schema).await? {
                Some(event) => Ok(NextProjection::Ready(event)),
                None => Ok(NextProjection::Waiting { entry, schema }),
            };
        }
        let Some(state) = self
            .store
            .load_execution(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
        else {
            return Err(ApiError::new(
                ApiErrorCode::Execution,
                "execution has no live driver",
            ));
        };
        if let Some(event) = project_durable_next(
            state,
            self.store
                .list_pending_requests(exec_id)
                .await
                .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?,
            &schema,
        )? {
            return Ok(NextProjection::Ready(event));
        }
        Err(ApiError::new(
            ApiErrorCode::Execution,
            "execution has no live driver",
        ))
    }

    pub(crate) async fn next_ready(&self, exec_id: ExecId) -> Result<Option<NextEvent>, ApiError> {
        match self.project_next(exec_id).await? {
            NextProjection::Ready(event) => Ok(Some(event)),
            NextProjection::Waiting { .. } => Ok(None),
        }
    }

    async fn next(&self, exec_id: ExecId) -> Result<NextEvent, ApiError> {
        match self.project_next(exec_id).await? {
            NextProjection::Ready(event) => Ok(event),
            NextProjection::Waiting { entry, schema } => entry.next(&schema).await,
        }
    }

    /// Create an execution and return immediately; negotiation runs in the background.
    /// Plan resolution is synchronous so its failures (bad token, unknown program,
    /// unfetchable wasm) surface on `exec.new` itself.
    async fn new_exec(
        self: &Arc<Self>,
        exec_id: ExecId,
        program: String,
        params: Option<serde_json::Value>,
        ensemble: EnsembleSpec,
    ) -> Response {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let (decision_tx, decision_rx) = tokio::sync::oneshot::channel();
        let mut tasks = self.tasks.lock().await;
        while let Some(result) = tasks.try_join_next() {
            if let Err(error) = result
                && !error.is_cancelled()
            {
                tracing::error!(error = %error, "execution creation task failed");
            }
        }
        let completion = self
            .creation_states
            .lock()
            .expect("creation states")
            .begin(exec_id)?;
        let daemon = Arc::clone(self);
        tasks.spawn(async move {
            let response = daemon
                .new_exec_inner(exec_id, program, params, ensemble)
                .await;
            if response_tx.send(response).is_err() {
                daemon.finish_creation(exec_id, true).await;
                return;
            }
            let caller_cancelled = !matches!(decision_rx.await, Ok(CreationDecision::Acknowledge));
            daemon.finish_creation(exec_id, caller_cancelled).await;
        });
        drop(tasks);

        let wait = CreationWait::new(decision_tx);
        let response = response_rx.await.map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "execution creation task ended without a response",
            )
        })?;
        if self
            .creation_states
            .lock()
            .expect("creation states")
            .is_cancelled(exec_id)
        {
            wait.cancel();
            creation_completion(completion).await?;
            return Err(ApiError::new(
                ApiErrorCode::Negotiation,
                "execution creation was cancelled before acknowledgement",
            ));
        }
        wait.acknowledge();
        response
    }

    async fn finish_creation(&self, exec_id: ExecId, caller_cancelled: bool) {
        let cancelled = self
            .creation_states
            .lock()
            .expect("creation states")
            .begin_finish(exec_id, caller_cancelled);
        if !cancelled {
            return;
        }
        let result = self.stop_cancelled_creation(exec_id).await.map(|_| ());
        self.creation_states
            .lock()
            .expect("creation states")
            .complete_cancelled(exec_id, result.clone());
        if let Err(error) = result {
            tracing::error!(
                exec_id = %exec_id,
                error = %error,
                "failed to stop cancelled execution creation"
            );
        }
    }

    async fn new_exec_inner(
        self: &Arc<Self>,
        exec_id: ExecId,
        program: String,
        params: Option<serde_json::Value>,
        ensemble: EnsembleSpec,
    ) -> Response {
        let (program_id, plan, admission) = match ensemble {
            EnsembleSpec::Create { participant_count } => {
                let program_id = self.resolve_program(&program).await?;
                let negotiation_id = arena0_protocol::NegotiationId(rand::random());
                (
                    program_id,
                    NegotiationPlan::Create {
                        negotiation_id,
                        target_size: participant_count,
                        peers: Vec::new(),
                    },
                    ExecutionAdmission::create(negotiation_id, participant_count).map_err(
                        |error| ApiError::new(ApiErrorCode::BadRequest, error.to_string()),
                    )?,
                )
            }
            EnsembleSpec::Explicit { peers } => {
                if peers.contains(&self.peer_id)
                    || peers.iter().collect::<std::collections::HashSet<_>>().len() != peers.len()
                {
                    return Err(ApiError::new(
                        ApiErrorCode::BadRequest,
                        "explicit peers must be unique and exclude this Host",
                    ));
                }
                let target_size = u16::try_from(peers.len() + 1).map_err(|_| {
                    ApiError::new(ApiErrorCode::BadRequest, "too many explicit peers")
                })?;
                if !(2..=arena0_protocol::MAX_PARTICIPANTS as u16).contains(&target_size) {
                    return Err(ApiError::new(
                        ApiErrorCode::BadRequest,
                        "explicit negotiation needs 2 to 64 participants",
                    ));
                }
                let negotiation_id = arena0_protocol::NegotiationId(rand::random());
                let mut admission_peers = peers.clone();
                admission_peers.push(self.peer_id);
                (
                    self.resolve_program(&program).await?,
                    NegotiationPlan::Create {
                        negotiation_id,
                        target_size,
                        peers,
                    },
                    ExecutionAdmission::explicit(negotiation_id, admission_peers).map_err(
                        |error| ApiError::new(ApiErrorCode::BadRequest, error.to_string()),
                    )?,
                )
            }
            EnsembleSpec::Join { target } => {
                if target.is_some_and(|target| target.creator == self.peer_id) {
                    return Err(ApiError::new(
                        ApiErrorCode::BadRequest,
                        "join creator must be a different Host",
                    ));
                }
                let program_id = self.resolve_program(&program).await?;
                let admission = target.map_or_else(ExecutionAdmission::join_open, |target| {
                    ExecutionAdmission::join(target.creator, target.negotiation_id)
                });
                (program_id, NegotiationPlan::Join { target }, admission)
            }
        };

        let registered_program = self
            .catalog
            .load_program(program_id)
            .await
            .map_err(|error| {
                ApiError::new(ApiErrorCode::Storage, format!("load program: {error}"))
            })?;
        let definition = registered_program.definition();
        if let NegotiationPlan::Create { target_size, .. } = &plan {
            validate_participants(definition.metadata.participants, *target_size)?;
        }
        let contract = definition.schema.clone();
        // Keep params optional on join requests. The compact-ticket path cannot
        // materialize remote terms until negotiation gossip is available.
        let joins = matches!(&plan, NegotiationPlan::Join { .. });
        let params_bytes = if joins && params.is_none() {
            None
        } else {
            Some(schema::encode_json_field(
                &contract.params,
                params.as_ref(),
                "params",
            )?)
        };

        let request_params = params_bytes
            .clone()
            .map(JsonBytes::try_new)
            .transpose()
            .map_err(|error| ApiError::new(ApiErrorCode::BadRequest, error.to_string()))?;
        // Claim before creating the immutable request root. The same
        // capability remains owned by negotiation and is then moved into the
        // actor after activation commits.
        let mut execution_store = self
            .runtime
            .claim_execution(exec_id)
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
        match execution_store
            .create_execution_request(program_id, request_params, admission, unix_time_ms())
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
        {
            arena0_store::ExecutionRequestOutcome::Created
            | arena0_store::ExecutionRequestOutcome::AlreadyExists => {}
            arena0_store::ExecutionRequestOutcome::Conflict => {
                return Err(ApiError::new(
                    ApiErrorCode::BadRequest,
                    "execution id collided with a different request",
                ));
            }
        }
        let queue_slot = self.negotiations_pending.fetch_add(1, Ordering::SeqCst);
        let queue_position = (queue_slot > 0).then_some(queue_slot);
        let negotiation_id = plan.negotiation_id();
        let entry = self.execs.register_live(exec_id);
        self.events.emit(HostEvent::Created {
            source: EventSource::Execution {
                peer_id: self.peer_id,
                exec_id,
                program_id,
            },
            negotiation_id,
            queue_position,
            origin: ExecCreationOrigin::Request,
        });

        let daemon = Arc::clone(self);
        let entry_task = Arc::clone(&entry);
        let span = tracing::info_span!(
            "negotiation",
            exec_id = %exec_id,
            program_id = %program_id,
            negotiation_id = ?negotiation_id,
        );
        self.tasks.lock().await.spawn(
            async move {
                let result = daemon
                    .drive_negotiation(&entry_task, program_id, params_bytes, plan, execution_store)
                    .await;
                daemon.finish_negotiation(&entry_task, result).await;
            }
            .instrument(span),
        );

        Ok(ResponseOk::ExecCreated {
            exec_id,
            negotiation_id,
            session_id: None,
            exec_state: ExecLifecycle::Negotiating,
            queue_position,
        })
    }

    async fn withdraw_or_cancel_creation(&self, exec_id: ExecId) -> Response {
        let existing_completion = {
            let mut states = self.creation_states.lock().expect("creation states");
            states.cancel_existing(exec_id)
        };
        if let Some(completion) = existing_completion {
            creation_completion(completion).await?;
            return Ok(ResponseOk::Ack);
        }
        if self.execs.get(&exec_id).is_some() {
            return self.stop_cancelled_creation(exec_id).await;
        }
        let completion = self
            .creation_states
            .lock()
            .expect("creation states")
            .await_creation(exec_id);
        let arrival = CreationArrivalWait::new(&self.creation_states, exec_id);
        match tokio::time::timeout(CREATION_ARRIVAL_GRACE, creation_completion(completion)).await {
            Ok(result) => {
                arrival.finish();
                result.map(|()| ResponseOk::Ack)
            }
            Err(_) => {
                let error = ApiError::new(ApiErrorCode::NotFound, "no such execution");
                drop(arrival);
                Err(error)
            }
        }
    }

    async fn stop_cancelled_creation(&self, exec_id: ExecId) -> Response {
        let Some(entry) = self.execs.get(&exec_id) else {
            return Ok(ResponseOk::Ack);
        };
        match entry
            .lifecycle()
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
        {
            ExecLifecycle::Negotiating => self.withdraw_negotiation(exec_id).await,
            ExecLifecycle::Activating | ExecLifecycle::Waiting | ExecLifecycle::Active => entry
                .terminate("execution creation cancelled before acknowledgement".to_owned())
                .await
                .map(|()| ResponseOk::Ack),
            ExecLifecycle::Completed
            | ExecLifecycle::Aborted
            | ExecLifecycle::Incomplete
            | ExecLifecycle::Failed => Ok(ResponseOk::Ack),
        }
    }

    async fn withdraw_negotiation(&self, exec_id: ExecId) -> Response {
        let was_withdrawn = self
            .store
            .load_execution_request(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .is_some_and(|request| request.failure() == Some(LOCAL_WITHDRAWAL_REASON));
        if was_withdrawn {
            return Ok(ResponseOk::Ack);
        }
        let entry = self
            .execs
            .get(&exec_id)
            .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "no such execution"))?;
        if entry
            .lifecycle()
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            != ExecLifecycle::Negotiating
        {
            let was_withdrawn = self
                .store
                .load_execution_request(exec_id)
                .await
                .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
                .is_some_and(|request| request.failure() == Some(LOCAL_WITHDRAWAL_REASON));
            if was_withdrawn {
                return Ok(ResponseOk::Ack);
            }
            return Err(ApiError::new(
                ApiErrorCode::Negotiation,
                "ticket is no longer revocable",
            ));
        }
        let Some(current) = entry.negotiation_ticket().await else {
            // `exec.new` returns before the background driver has necessarily
            // built its offer and local ticket. Queue the intent so every
            // pre-ticket negotiating state remains cancellable.
            entry.request_withdrawal();
            self.await_withdrawal(&entry).await?;
            return Ok(ResponseOk::Ack);
        };
        if matches!(current.data.action, TicketAction::Withdrawn) {
            return Ok(ResponseOk::Ack);
        }
        self.withdraw_ticket(
            &entry,
            current.clone(),
            entry.negotiation_offer().await.ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::Negotiation,
                    "negotiation offer is not available for withdrawal",
                )
            })?,
        )
        .await?;
        Ok(ResponseOk::Ack)
    }

    async fn await_withdrawal(&self, entry: &ExecutionHandle) -> Result<(), ApiError> {
        let deadline = Instant::now() + NEGOTIATION_TIMEOUT;
        loop {
            let changed = entry.change_notified();
            match entry
                .lifecycle()
                .await
                .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            {
                ExecLifecycle::Negotiating => {}
                ExecLifecycle::Activating
                | ExecLifecycle::Waiting
                | ExecLifecycle::Active
                | ExecLifecycle::Completed
                | ExecLifecycle::Aborted
                | ExecLifecycle::Incomplete => {
                    return Err(ApiError::new(
                        ApiErrorCode::Negotiation,
                        "ticket is no longer revocable",
                    ));
                }
                ExecLifecycle::Failed => {
                    let reason = entry
                        .request()
                        .await
                        .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
                        .and_then(|request| request.failure().map(str::to_owned));
                    if reason.as_deref() == Some(LOCAL_WITHDRAWAL_REASON) {
                        return Ok(());
                    }
                    return Err(ApiError::new(
                        ApiErrorCode::Negotiation,
                        reason.unwrap_or_else(|| "negotiation failed while withdrawing".into()),
                    ));
                }
            }
            tokio::select! {
                () = changed => {}
                () = tokio::time::sleep_until(deadline) => {
                    return Err(ApiError::new(
                        ApiErrorCode::Timeout,
                        "negotiation withdrawal deadline elapsed",
                    ));
                }
            }
        }
    }

    async fn withdraw_ticket(
        &self,
        entry: &ExecutionHandle,
        current: Ticket,
        offer: Offer,
    ) -> Result<(), ApiError> {
        let revision =
            current.data.revision.checked_add(1).ok_or_else(|| {
                ApiError::new(ApiErrorCode::Negotiation, "ticket revision exhausted")
            })?;
        let now = unix_time_ms();
        let TicketAction::Active {
            issued_at_unix_ms,
            valid_for_ms,
            ..
        } = current.data.action
        else {
            return Err(ApiError::new(
                ApiErrorCode::Negotiation,
                "current ticket is already withdrawn",
            ));
        };
        if now >= issued_at_unix_ms.saturating_add(u64::from(valid_for_ms)) {
            return Err(ApiError::new(
                ApiErrorCode::Negotiation,
                "ticket has expired",
            ));
        }
        let data = TicketData::new(
            current.data.negotiation_id,
            current.data.offer_seq,
            current.data.signer,
            revision,
            TicketAction::Withdrawn,
        )
        .map_err(|error| ApiError::new(ApiErrorCode::Negotiation, error.to_string()))?;
        let ticket = Ticket {
            signature: self.identity.sign(&data.signing_bytes()),
            data,
        };
        entry
            .withdraw_negotiation_ticket(offer, ticket)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Negotiation, error))
    }

    /// The background negotiation driver: form a session (serialized daemon-wide),
    /// then attach the supervisor, or record the failure on the entry.
    async fn drive_prepared_negotiation(
        self: &Arc<Self>,
        entry: &Arc<ExecutionHandle>,
        record: ActivationRecord,
        execution_store: HostExecutionStore,
    ) -> Result<(), ApiError> {
        let _guard = self.runtime.negotiation_guard().await;
        let deadline = Instant::now() + NEGOTIATION_TIMEOUT;
        let prepared = record.prepared().clone();
        let offer = prepared.offer().clone();
        let program_id = offer.data().program_hash;
        let program = self
            .catalog
            .load_program(program_id)
            .await
            .map_err(|error| {
                ApiError::new(ApiErrorCode::Storage, format!("load program: {error}"))
            })?;
        let peers = prepared
            .tickets()
            .iter()
            .map(|ticket| ticket.data.signer)
            .filter(|peer| *peer != self.peer_id)
            .collect::<Vec<_>>();
        let bootstrap = negotiation_bootstrap(self.peer_id, None, peers.iter().copied());
        let topic = self
            .subscribe_negotiation(offer.data().program_hash, bootstrap)
            .await?;
        tracing::debug!(
            exec_id = %entry.exec_id(),
            negotiation_id = %offer.data().negotiation_id,
            "resuming prepared negotiation"
        );

        // Resume the durable prepared activation: the exact local ticket and
        // the prepared evidence come from the record, so the TicketHash
        // matches the durable prepare after the restart. The driver never
        // re-signs a fresh revision-0 ticket.
        let local_ticket = prepared
            .tickets()
            .iter()
            .find(|ticket| ticket.data.signer == self.peer_id)
            .cloned()
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    "prepared activation lacks the local ticket",
                )
            })?;
        self.run_negotiation_attempt(
            entry,
            program,
            topic,
            NegotiationStart::Resume {
                local_ticket,
                activation: Box::new(prepared),
            },
            Some(deadline),
            execution_store,
        )
        .await
    }

    /// Wait for one usable offer and its authenticated creator ticket on a program
    /// topic. Offers and tickets are broadcast independently, so the small maps
    /// below tolerate either fact arriving first. An open Join is bound only after
    /// the pair has passed the same program/profile/boot validation used by the
    /// negotiation driver.
    async fn receive_offer(
        &self,
        topic: &mut Box<dyn NegotiationTopic>,
        program: &Program,
        target: Option<NegotiationTarget>,
        bootstrap: &[PeerId],
        deadline: Option<Instant>,
    ) -> Result<(Offer, Ticket), ApiError> {
        let program_id = program.hash();
        let local_peer = self.peer_id;
        let expected_creator = target.map(|target| target.creator);
        let expected_negotiation = target.map(|target| target.negotiation_id);
        let events = &self.events;
        let engine = Arc::clone(&self.engine);
        const PENDING_FACTS: usize = 128;
        let mut pending_offers = HashMap::<(PeerId, NegotiationId, u64), Offer>::new();
        let mut pending_tickets = HashMap::<(PeerId, NegotiationId, u64), Ticket>::new();
        let mut retry_delays =
            Exponential::from_millis(100).map(|delay| jitter(delay.min(Duration::from_secs(2))));
        let mut retry_at = Instant::now()
            + retry_delays
                .next()
                .expect("an exponential retry iterator is infinite");
        loop {
            let event = match timeout_at(
                deadline.map_or(retry_at, |deadline| deadline.min(retry_at)),
                topic.recv(),
            )
            .await
            {
                Ok(event) => event
                    .map_err(|error| ApiError::new(ApiErrorCode::Negotiation, error.to_string()))?,
                Err(_) if deadline.is_some_and(|deadline| Instant::now() >= deadline) => {
                    return Err(offer_timeout());
                }
                Err(_) => {
                    let _ = topic.join_peers(bootstrap.to_vec()).await;
                    retry_at = Instant::now()
                        + retry_delays
                            .next()
                            .expect("an exponential retry iterator is infinite");
                    continue;
                }
            };
            let ProgramTopicEvent::Fact(fact) = event else {
                match event {
                    ProgramTopicEvent::Joined
                    | ProgramTopicEvent::NeighborUp(_)
                    | ProgramTopicEvent::NeighborDown(_) => continue,
                    ProgramTopicEvent::Lagged | ProgramTopicEvent::Closed => {
                        let operation_deadline =
                            deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(5));
                        let _ = timeout_at(operation_deadline, topic.close()).await;
                        *topic = timeout_at(
                            operation_deadline,
                            self.subscribe_negotiation(program_id, bootstrap.to_vec()),
                        )
                        .await
                        .map_err(|_| offer_timeout())??;
                        continue;
                    }
                    ProgramTopicEvent::Fact(_) => unreachable!(),
                }
            };
            let Ok(frame) = NegotiationGossip::decode(&fact.bytes) else {
                continue;
            };
            if frame.program_id != program_id {
                continue;
            }
            match frame.fact {
                NegotiationFact::Offer(offer) => {
                    let data = offer.data();
                    if offer.validate().is_err()
                        || data.program_hash != program_id
                        || data.creator == local_peer
                        || expected_creator.is_some_and(|creator| data.creator != creator)
                        || expected_negotiation
                            .is_some_and(|negotiation_id| data.negotiation_id != negotiation_id)
                        // A complete offer has already frozen its participant set;
                        // a fresh Join cannot safely add itself to that evidence.
                        || offer.is_complete()
                    {
                        continue;
                    }
                    let key = (data.creator, data.negotiation_id, data.offer_seq);
                    if let Some(ticket) = pending_tickets.get(&key)
                        && creator_ticket_matches(&offer, ticket)
                    {
                        // Decode and initialize the offered params only after the
                        // creator's Active ticket has authenticated this offer.
                        if offer_is_usable_for_join(&offer, program, Arc::clone(&engine), deadline)
                            .await
                            && creator_ticket_matches(&offer, ticket)
                        {
                            let ticket = ticket.clone();
                            pending_tickets.remove(&key);
                            tracing::debug!(
                                %program_id,
                                negotiation_id = %data.negotiation_id,
                                creator = %data.creator,
                                "accepted offer"
                            );
                            events.emit(HostEvent::OfferSeen {
                                program_id,
                                negotiation_id: data.negotiation_id,
                                creator: data.creator,
                                offer_seq: data.offer_seq,
                            });
                            return Ok((offer, ticket));
                        }
                        pending_tickets.remove(&key);
                        continue;
                    }
                    if pending_offers.len() >= PENDING_FACTS
                        && let Some(key) = pending_offers.keys().next().copied()
                    {
                        pending_offers.remove(&key);
                    }
                    pending_offers.insert(key, offer);
                }
                NegotiationFact::Ticket(ticket) => {
                    let data = &ticket.data;
                    if ticket.validate().is_err()
                        || !matches!(data.action, TicketAction::Active { .. })
                        || expected_creator.is_some_and(|creator| data.signer != creator)
                        || expected_negotiation
                            .is_some_and(|negotiation_id| data.negotiation_id != negotiation_id)
                    {
                        continue;
                    }
                    let key = (data.signer, data.negotiation_id, data.offer_seq);
                    if let Some(offer) = pending_offers.get(&key)
                        && creator_ticket_matches(offer, &ticket)
                    {
                        if offer_is_usable_for_join(offer, program, Arc::clone(&engine), deadline)
                            .await
                            && creator_ticket_matches(offer, &ticket)
                        {
                            let offer = offer.clone();
                            pending_offers.remove(&key);
                            tracing::debug!(
                                %program_id,
                                negotiation_id = %data.negotiation_id,
                                creator = %data.signer,
                                "accepted offer"
                            );
                            events.emit(HostEvent::OfferSeen {
                                program_id,
                                negotiation_id: data.negotiation_id,
                                creator: data.signer,
                                offer_seq: data.offer_seq,
                            });
                            return Ok((offer, ticket));
                        }
                        pending_offers.remove(&key);
                        continue;
                    }
                    if pending_tickets.len() >= PENDING_FACTS
                        && let Some(key) = pending_tickets.keys().next().copied()
                    {
                        pending_tickets.remove(&key);
                    }
                    pending_tickets.insert(key, ticket);
                }
                NegotiationFact::ActivationSignature(_)
                | NegotiationFact::ActivationAnnouncement(_)
                | NegotiationFact::Counteroffer(_) => {}
            }
        }
    }

    async fn drive_negotiation(
        self: &Arc<Self>,
        entry: &Arc<ExecutionHandle>,
        program_id: ProgramHash,
        preferred_params: Option<Vec<u8>>,
        plan: NegotiationPlan,
        mut execution_store: HostExecutionStore,
    ) -> Result<(), ApiError> {
        let mut withdrawal = entry.withdrawal_receiver();
        let _guard = tokio::select! {
            biased;
            () = wait_for_withdrawal(&mut withdrawal) => return Err(local_withdrawal()),
            guard = self.runtime.negotiation_guard() => guard,
        };
        let deadline = match &plan {
            NegotiationPlan::Create { peers, .. } if peers.is_empty() => None,
            NegotiationPlan::Join { target: None } => None,
            _ => Some(Instant::now() + NEGOTIATION_TIMEOUT),
        };

        let program = self
            .catalog
            .load_program(program_id)
            .await
            .map_err(|error| {
                ApiError::new(ApiErrorCode::Storage, format!("load program: {error}"))
            })?;
        let (offer, creator_ticket, topic, event_peers) = match plan {
            NegotiationPlan::Create {
                negotiation_id,
                target_size,
                peers,
            } => {
                let params = preferred_params.clone().ok_or_else(|| {
                    ApiError::new(ApiErrorCode::BadRequest, "create requires params")
                })?;
                let (_loaded, initial_state) =
                    load_for(&program, Arc::clone(&self.engine), params.clone(), "create").await?;
                let issued_at = unix_time_ms();
                // Leave the creator ticket's clock-skew and prepare margins
                // before the first offer is deserted. That gives the driver
                // a clean re-offer boundary instead of a late-ticket dead
                // zone between ticket expiry and offer expiry.
                let offer_lifetime =
                    MAX_TICKET_LIFETIME_MS.saturating_sub(MAX_CLOCK_SKEW_MS + PREPARE_WINDOW_MS);
                let deadline_unix_ms = issued_at.checked_add(offer_lifetime).ok_or_else(|| {
                    ApiError::new(
                        ApiErrorCode::Negotiation,
                        "offer deadline timestamp overflowed",
                    )
                })?;
                let offer_data = OfferData::new(
                    negotiation_id,
                    0,
                    self.peer_id,
                    program_id,
                    arena0_program::ExecutionProfile::current().hash(),
                    arena0_program::JsonBytes::try_new(params).map_err(|error| {
                        ApiError::new(ApiErrorCode::Negotiation, error.to_string())
                    })?,
                    target_size,
                    initial_state,
                    deadline_unix_ms,
                )
                .map_err(|error| ApiError::new(ApiErrorCode::Negotiation, error.to_string()))?;
                let execution_salt = execution_store
                    .load_or_create_execution_salt(unix_time_ms())
                    .await
                    .map_err(|error| {
                        ApiError::new(ApiErrorCode::Storage, format!("execution salt: {error}"))
                    })?;
                let execution_key = self
                    .runtime
                    .execution_key(&execution_salt, &entry.exec_id(), &negotiation_id)
                    .map_err(|error| {
                        ApiError::new(ApiErrorCode::Internal, format!("execution crypto: {error}"))
                    })?;
                let creator_book = NegotiationBook::new(&self.identity, &execution_key);
                let (offer, creator_ticket) = creator_book
                    .create_creator_offer(offer_data, issued_at)
                    .map_err(|error| ApiError::new(ApiErrorCode::Negotiation, error.to_string()))?;
                let bootstrap = negotiation_bootstrap(self.peer_id, None, peers.iter().copied());
                let topic = self.subscribe_negotiation(program_id, bootstrap).await?;
                (offer, Some(creator_ticket), topic, peers)
            }
            NegotiationPlan::Join { target } => {
                let bootstrap = negotiation_bootstrap(
                    self.peer_id,
                    target.map(|target| target.creator),
                    std::iter::empty(),
                );
                let mut topic = self
                    .subscribe_negotiation(program_id, bootstrap.clone())
                    .await?;
                let received = tokio::select! {
                    biased;
                    () = wait_for_withdrawal(&mut withdrawal) => return Err(local_withdrawal()),
                    result = self.receive_offer(&mut topic, &program, target, &bootstrap, deadline) => result,
                };
                let (offer, creator_ticket) = received?;
                if *withdrawal.borrow() {
                    return Err(local_withdrawal());
                }
                let selected_target =
                    NegotiationTarget::new(offer.data().creator, offer.data().negotiation_id);
                if target.is_none() {
                    match execution_store
                        .bind_join_target(selected_target)
                        .await
                        .map_err(|error| {
                            ApiError::new(
                                ApiErrorCode::Negotiation,
                                format!("could not bind selected join target: {error}"),
                            )
                        })? {
                        AdmissionBindingOutcome::Bound | AdmissionBindingOutcome::AlreadyBound => {}
                        AdmissionBindingOutcome::Conflict => {
                            return Err(ApiError::new(
                                ApiErrorCode::Negotiation,
                                "open Join was already bound to a different negotiation",
                            ));
                        }
                    }
                }
                (
                    offer,
                    Some(creator_ticket),
                    topic,
                    vec![selected_target.creator],
                )
            }
        };

        self.publish_negotiation_peers(entry, event_peers).await;
        // The local ticket is signed by the driver only after the offer is
        // authenticated (the creator's Active ticket for it verifies); the
        // handle slot starts empty and the driver publishes the ticket.
        self.run_negotiation_attempt(
            entry,
            program,
            topic,
            NegotiationStart::Fresh {
                offer: offer.clone(),
                creator_ticket,
                // The original preference, not the offer's params: a
                // differing offer is countered, never silently accepted.
                preferred_params,
            },
            deadline,
            execution_store,
        )
        .await
    }

    /// Run the shared negotiation lifecycle after fresh or recovery-specific
    /// offer setup has produced its topic and start state.
    async fn run_negotiation_attempt(
        self: &Arc<Self>,
        entry: &Arc<ExecutionHandle>,
        program: Program,
        topic: Box<dyn NegotiationTopic>,
        start: NegotiationStart,
        deadline: Option<Instant>,
        mut execution_store: HostExecutionStore,
    ) -> Result<(), ApiError> {
        let (offer, withdrawal_capacity, rebuild_stage) = match &start {
            NegotiationStart::Fresh { offer, .. } => (offer.clone(), 8, "activate"),
            NegotiationStart::Resume { activation, .. } => {
                (activation.offer().clone(), 1, "resume")
            }
        };
        let execution_salt = execution_store
            .load_or_create_execution_salt(unix_time_ms())
            .await
            .map_err(|error| {
                ApiError::new(ApiErrorCode::Storage, format!("execution salt: {error}"))
            })?;
        let execution_key = self
            .runtime
            .execution_key(
                &execution_salt,
                &entry.exec_id(),
                &offer.data().negotiation_id,
            )
            .map_err(|error| {
                ApiError::new(ApiErrorCode::Internal, format!("execution crypto: {error}"))
            })?;
        let actor_execution_key = self
            .runtime
            .execution_key(
                &execution_salt,
                &entry.exec_id(),
                &offer.data().negotiation_id,
            )
            .map_err(|error| {
                ApiError::new(ApiErrorCode::Internal, format!("execution crypto: {error}"))
            })?;
        let (ticket_tx, _ticket_rx) = watch::channel(None);
        let (withdrawals_tx, withdrawals_rx) = mpsc::channel(withdrawal_capacity);
        let offer_slot = entry
            .install_negotiation(offer.clone(), ticket_tx.clone(), withdrawals_tx)
            .await;

        let (prepare, persist_commit) = activation_callbacks();
        let publish_event = |source, event| {
            self.events.emit(HostEvent::Negotiation { source, event });
        };
        let engine = Arc::clone(&self.engine);
        let recompute_program = program.clone();
        let effects = NegotiationEffects {
            prepare,
            persist_activation: persist_commit,
            recompute_initial_state: Box::new(move |_params: &[u8]| {
                // The creator's re-offer hook: recompute the program's
                // initial state for the new params.
                load_and_initialize(&recompute_program, &engine, _params.to_vec(), "recompute")
                    .map(|(_, initial_state)| initial_state)
                    .map_err(|error| error.to_string())
            }),
            emit: &publish_event,
        };
        let attempt = NegotiationAttempt {
            topic,
            exec_id: entry.exec_id(),
            start,
            supervision: Some(NegotiationSupervision::new(
                withdrawals_rx,
                entry.withdrawal_receiver(),
                ticket_tx,
                offer_slot,
            )),
            deadline,
        };
        let (committed, execution_store) = self
            .runtime
            .negotiate(&execution_key, execution_store, attempt, effects)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Negotiation, error.to_string()))?;

        tracing::debug!(
            exec_id = %entry.exec_id(),
            negotiation_id = %offer.data().negotiation_id,
            session_id = %committed.session_hash(),
            stage = rebuild_stage,
            "negotiation committed"
        );

        let offer_data = committed.activation().offer().data().clone();
        let program = load_checked(
            &program,
            &offer_data,
            Arc::clone(&self.engine),
            rebuild_stage,
        )
        .await?;
        self.spawn(
            entry,
            SpawnPlan {
                params: offer_data.params.into_bytes(),
                program,
                committed,
                actor_execution_key,
                execution_store,
            },
        )
        .await?;
        Ok(())
    }

    /// Spawn the execution on a confirmed activation and attach its supervisor.
    async fn spawn(
        self: &Arc<Self>,
        entry: &Arc<ExecutionHandle>,
        plan: SpawnPlan,
    ) -> Result<(), ApiError> {
        let SpawnPlan {
            params,
            program,
            committed,
            actor_execution_key,
            execution_store,
        } = plan;
        let exec_id = entry.exec_id();
        let params = JsonBytes::try_new(params)
            .map_err(|error| ApiError::new(ApiErrorCode::Execution, error.to_string()))?;
        let context = arena0_node::ExecContext::new(
            exec_id,
            program,
            params,
            committed.activation().clone(),
            actor_execution_key,
        );
        let spawned = self
            .runtime
            .spawn(context, execution_store)
            .map_err(|error| ApiError::new(ApiErrorCode::Execution, error.to_string()))?;

        self.execs.attach(Supervisor {
            entry: Arc::clone(entry),
            spawned,
            events: self.events.clone(),
            transport: Arc::clone(&self.transport),
        });

        // The post-commit relay: session-lived, re-emits the final offer and
        // the exact tickets on the program topic and serves the convergence
        // fetch from the committed ActivationRecord. The creator is the
        // convergence authority: only it relays.
        if self.peer_id == committed.activation().offer().data().creator {
            let relay = Arc::clone(self);
            self.tasks
                .lock()
                .await
                .spawn(relay.run_session_relay(Arc::clone(entry), committed.clone()));
        }
        Ok(())
    }

    /// Validate an answer against the callout's public output schema and submit it.
    async fn submit(
        &self,
        exec_id: ExecId,
        pending_id: PendingId,
        answer: Option<serde_json::Value>,
    ) -> Response {
        let request = self
            .store
            .load_execution_request(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "no such execution"))?;
        let callout_index = self
            .pending_callout_index(exec_id, pending_id)
            .await?
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::CalloutNotPending,
                    format!("no pending {pending_id}"),
                )
            })?;
        let entry = match self.execs.get(&exec_id) {
            Some(entry) => entry,
            None => {
                let error = ApiError::new(ApiErrorCode::Execution, "execution has no live driver");
                return Err(self
                    .reclassify_submit_failure(exec_id, pending_id, error)
                    .await);
            }
        };
        let program_id = request.program_hash();
        let contract = self
            .catalog
            .schema(program_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .ok_or_else(|| {
                ApiError::new(ApiErrorCode::NotFound, "execution program was removed")
            })?;
        let output_schema = contract
            .callouts
            .get(callout_index as usize)
            .map(|c| c.output.clone())
            .ok_or_else(|| ApiError::new(ApiErrorCode::Internal, "callout index out of range"))?;
        let bytes = schema::encode_json_field(&output_schema, answer.as_ref(), "answer")?;
        let bytes = JsonBytes::try_new(bytes)
            .map_err(|error| ApiError::new(ApiErrorCode::BadRequest, error.to_string()))?;
        if let Err(error) = entry.submit(pending_id, bytes).await {
            return Err(self
                .reclassify_submit_failure(exec_id, pending_id, error)
                .await);
        }
        self.events.emit(HostEvent::SessionCalloutAnswered {
            source: entry
                .event_source()
                .await
                .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?,
            pending_id,
        });
        Ok(ResponseOk::Ack)
    }

    async fn pending_callout_index(
        &self,
        exec_id: ExecId,
        pending_id: PendingId,
    ) -> Result<Option<u32>, ApiError> {
        Ok(self
            .store
            .list_pending_requests(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .into_iter()
            .find_map(|request| match request {
                arena0_store::PendingRequest::Callout {
                    pending_id: id,
                    callout_index,
                    ..
                } if id == pending_id => Some(callout_index),
                _ => None,
            }))
    }

    async fn reclassify_submit_failure(
        &self,
        exec_id: ExecId,
        pending_id: PendingId,
        error: ApiError,
    ) -> ApiError {
        match self.pending_callout_index(exec_id, pending_id).await {
            Ok(None) => ApiError::new(
                ApiErrorCode::CalloutNotPending,
                format!("no pending {pending_id}"),
            ),
            Ok(Some(_)) | Err(_) => error,
        }
    }

    /// Validate the query against the program's public request schema, run it,
    /// and return the guest-produced JSON response. A program must expose one
    /// query schema because the local request carries no guest-specific type tag.
    async fn query(&self, exec_id: ExecId, query: Option<serde_json::Value>) -> Response {
        let entry = self
            .execs
            .get(&exec_id)
            .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "no such execution"))?;
        let program_id = entry
            .program_id()
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
        let contract = self
            .catalog
            .schema(program_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .ok_or_else(|| {
                ApiError::new(ApiErrorCode::NotFound, "execution program was removed")
            })?;
        let queries = &contract.queries;
        let [only] = queries.as_slice() else {
            return Err(ApiError::new(
                ApiErrorCode::BadRequest,
                "program must expose exactly one query schema",
            ));
        };
        let bytes = schema::encode_json_field(&only.request, query.as_ref(), "query")?;
        let bytes = JsonBytes::try_new(bytes)
            .map_err(|error| ApiError::new(ApiErrorCode::BadRequest, error.to_string()))?;
        let result_bytes = entry.query(bytes).await?;
        Ok(ResponseOk::Query {
            result: schema::decode_guest_json(result_bytes.as_bytes(), "query result")?,
        })
    }

    /// Render the program view from live or terminal shared state. Terminal
    /// projection uses its durable snapshot after the execution actor exits.
    async fn view(
        &self,
        exec_id: ExecId,
        width: u16,
        color: arena0_protocol::ColorDepth,
    ) -> Response {
        self.store
            .load_execution_request(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "no such execution"))?;
        let state = self
            .store
            .load_execution(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::Execution,
                    "execution has no active state; view is available only while Active",
                )
            })?;
        let viewport = serde_json::to_vec(&Viewport { width, color })
            .map_err(|error| ApiError::new(ApiErrorCode::BadRequest, error.to_string()))?;
        let viewport = JsonBytes::try_new(viewport)
            .map_err(|error| ApiError::new(ApiErrorCode::BadRequest, error.to_string()))?;
        if state.lifecycle().is_terminal() {
            if state.binding().execution_profile()
                != arena0_program::ExecutionProfile::current().hash()
            {
                return Err(ApiError::new(
                    ApiErrorCode::Execution,
                    "execution profile is not supported by this runtime",
                ));
            }
            let program = self
                .catalog
                .load_program(state.binding().program_hash())
                .await
                .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
            let ensemble = arena0_protocol::Ensemble::from_peers(
                state
                    .binding()
                    .activation()
                    .tickets()
                    .iter()
                    .map(|ticket| ticket.data.signer)
                    .collect(),
            )
            .map_err(|error| ApiError::new(ApiErrorCode::Execution, error.to_string()))?;
            let engine = Arc::clone(&self.engine);
            let step = state.public().next_step();
            let shared = state.shared_state().clone();
            let projection = tokio::task::spawn_blocking(move || {
                engine
                    .load(&program)?
                    .view(ViewCall::new(shared, ensemble, viewport))
            })
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Internal, error.to_string()))?
            .map_err(|error| ApiError::new(ApiErrorCode::Execution, error.to_string()))?;
            let view = serde_json::from_slice(projection.output.as_bytes()).map_err(|error| {
                ApiError::new(ApiErrorCode::Execution, format!("view projection: {error}"))
            })?;
            return Ok(ResponseOk::ExecView { step, view });
        }
        if !matches!(
            state.lifecycle(),
            ExecLifecycle::Active | ExecLifecycle::Waiting
        ) {
            return Err(ApiError::new(
                ApiErrorCode::Execution,
                format!(
                    "execution is {:?}; view is available only while Active",
                    state.lifecycle()
                ),
            ));
        }
        let entry = self.execs.get(&exec_id).ok_or_else(|| {
            ApiError::new(ApiErrorCode::Execution, "execution has no live driver")
        })?;

        let (step, view) = entry.view(viewport).await?;
        Ok(ResponseOk::ExecView { step, view })
    }

    /// Read trace entries directly from the durable runtime journal.
    async fn trace(
        &self,
        exec_id: ExecId,
        from: u64,
        to: u64,
    ) -> Result<Vec<arena0_protocol::TraceEntry>, ApiError> {
        self.store
            .load_execution(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "no such execution"))?;
        self.store
            .read_trace(exec_id, from, to)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))
    }

    /// Verify and persist a foreign receipt as an immutable artifact.
    async fn import_receipt(&self, receipt: ReceiptArtifact) -> Response {
        let receipt_bytes = receipt
            .encode()
            .map_err(|error| ApiError::new(ApiErrorCode::Verification, error.to_string()))?;
        verify_light(&receipt_bytes)
            .map_err(|e| ApiError::new(ApiErrorCode::Verification, format!("light: {e:?}")))?;
        let receipt_id = receipt.receipt_id();
        self.store
            .import_receipt(receipt, unix_time_ms())
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
        let stored = self
            .store
            .load_receipt_by_id(receipt_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .ok_or_else(|| {
                ApiError::new(ApiErrorCode::Internal, "imported receipt was not persisted")
            })?;
        Ok(ResponseOk::ReceiptList(vec![receipt_list_entry(stored)]))
    }

    async fn resolve_receipt(&self, reference: ReceiptRef) -> Result<ReceiptArtifact, ApiError> {
        let stored = match reference {
            ReceiptRef::Inline(receipt) => return Ok(*receipt),
            ReceiptRef::Produced(session_id) => self.store.load_receipt(session_id).await,
            ReceiptRef::Stored(receipt_id) => self.store.load_receipt_by_id(receipt_id).await,
        }
        .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
        stored
            .map(|stored| stored.receipt)
            .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "no such receipt or stop report"))
    }

    /// Verify a receipt and return its evidence: light by default, full on request.
    async fn verify(&self, receipt: ReceiptRef, full: bool) -> Response {
        let receipt = self.resolve_receipt(receipt).await?;
        let receipt_bytes = receipt
            .encode()
            .map_err(|error| ApiError::new(ApiErrorCode::Verification, error.to_string()))?;
        let light = verify_light(&receipt_bytes)
            .map_err(|e| ApiError::new(ApiErrorCode::Verification, format!("light: {e:?}")))?;

        if full {
            let program = self
                .catalog
                .load_program(light.program_id)
                .await
                .map_err(|error| {
                    ApiError::new(ApiErrorCode::Storage, format!("load program: {error}"))
                })?;
            let receipt_bytes = receipt_bytes.clone();
            let full =
                tokio::task::spawn_blocking(move || verify_full(program.bytes(), &receipt_bytes))
                    .await
                    .map_err(|error| {
                        ApiError::new(ApiErrorCode::Internal, format!("join: {error}"))
                    })?
                    .map_err(|error| {
                        ApiError::new(ApiErrorCode::Verification, format!("full: {error:?}"))
                    })?;
            let terminal = match full.terminal {
                arena0_verify::VerifiedTerminal::Completed {
                    outcome_borsh,
                    outcome_json,
                } => FullVerifiedTerminal::Completed {
                    outcome_borsh,
                    outcome_json: schema::decode_guest_json(
                        outcome_json.as_bytes(),
                        "verified outcome",
                    )?,
                },
                arena0_verify::VerifiedTerminal::Stopped { cause } => {
                    FullVerifiedTerminal::Stopped { cause }
                }
            };
            return Ok(ResponseOk::Verified {
                receipt_id: receipt.receipt_id(),
                program_id: light.program_id,
                session_id: light.session_id,
                ensemble: light.ensemble,
                steps: light.steps,
                result: VerifiedResult::Full { terminal },
            });
        }

        let terminal = match light.terminal {
            VerifiedLightTerminal::Completed { outcome_borsh } => {
                LightVerifiedTerminal::Completed { outcome_borsh }
            }
            VerifiedLightTerminal::Stopped { cause } => LightVerifiedTerminal::Stopped { cause },
        };
        Ok(ResponseOk::Verified {
            receipt_id: receipt.receipt_id(),
            program_id: light.program_id,
            session_id: light.session_id,
            ensemble: light.ensemble,
            steps: light.steps,
            result: VerifiedResult::Light { terminal },
        })
    }
}

fn project_exec_status_facts(
    peer_id: PeerId,
    request: ExecutionRequest,
    activation: Option<ActivationRecord>,
    state: Option<arena0_protocol::execution::ExecutionState>,
    pending: Option<arena0_store::PendingRequest>,
    receipt_available: bool,
) -> anyhow::Result<ExecStatus> {
    let exec_id = request.execution_id();
    let program_id = request.program_hash();
    let negotiation_id = request.negotiation_id();
    let pending_callout = pending.and_then(|request| match request {
        arena0_store::PendingRequest::Callout {
            pending_id,
            callout_index,
            expected_type,
            ..
        } => Some(PendingCalloutStatus {
            pending_id,
            callout_index,
            expected_type,
        }),
        arena0_store::PendingRequest::Signature { .. } => None,
    });
    let session_status = |state: &arena0_protocol::execution::ExecutionState| {
        let activation = state.binding().activation();
        SessionStatus {
            session_id: state.binding().session_id(),
            step: state.public().next_step(),
            peers: activation
                .tickets()
                .iter()
                .map(|ticket| ticket.data.signer)
                .filter(|peer| *peer != peer_id)
                .collect(),
            participants: activation.tickets().len(),
            pending_callout: pending_callout.clone(),
            receipt_available,
        }
    };

    let state = match state {
        Some(state) => match state.status() {
            ExecutionStatus::Activating => ExecStatusState::Activating {
                session_id: Some(state.binding().session_id()),
            },
            ExecutionStatus::Active
            | ExecutionStatus::Waiting { .. }
            | ExecutionStatus::TerminalProof { .. } => ExecStatusState::Active {
                session: session_status(&state),
            },
            ExecutionStatus::Completed { .. } => ExecStatusState::Completed {
                session: session_status(&state),
            },
            ExecutionStatus::Stopped { cause }
            | ExecutionStatus::StoppedPublished { cause, .. } => {
                if cause.kind() == arena0_protocol::AbortKind::Abort {
                    ExecStatusState::Aborted {
                        session: session_status(&state),
                    }
                } else {
                    ExecStatusState::Failed {
                        session: Some(SessionProgress::Started {
                            session: session_status(&state),
                        }),
                    }
                }
            }
            ExecutionStatus::Incomplete { .. } => ExecStatusState::Failed {
                session: Some(SessionProgress::Started {
                    session: session_status(&state),
                }),
            },
        },
        None if request.failure().is_some() => ExecStatusState::Failed {
            session: activation.as_ref().and_then(|record| {
                record.is_committed().then(|| SessionProgress::Activated {
                    session_id: record.session_id(),
                })
            }),
        },
        None if activation.is_some() => ExecStatusState::Activating {
            // A prepared activation fixes a candidate hash but is not yet a
            // formed session. Expose the SessionHash only after commit.
            session_id: activation
                .and_then(|record| record.is_committed().then(|| record.session_id())),
        },
        None => ExecStatusState::Negotiating {
            queue_position: None,
        },
    };
    Ok(ExecStatus {
        exec_id,
        negotiation_id,
        program_id,
        state,
    })
}

fn empty_execution_inspection(
    status: ExecStatus,
    activation: Option<ActivationInspection>,
    private_from: u64,
) -> ExecutionInspection {
    ExecutionInspection {
        status,
        activation,
        private_from,
        private: Vec::new(),
        private_total: 0,
        private_next: None,
    }
}

fn project_activation_inspection(record: ActivationRecord) -> ActivationInspection {
    let prepared = record.prepared();
    let offer = prepared.offer();
    let data = offer.data();
    ActivationInspection {
        state: match record.status() {
            ActivationRecordStatus::Prepared => ActivationInspectionState::Prepared,
            ActivationRecordStatus::Committed => ActivationInspectionState::Committed,
        },
        negotiation_id: data.negotiation_id,
        session_id: record.is_committed().then(|| record.session_id()),
        offer_hash: arena0_protocol::OfferHash::of(data),
        creator: data.creator,
        target_size: data.target_size,
        initial_state: data.initial_state,
        participants: prepared
            .tickets()
            .iter()
            .map(|ticket| ActivationParticipant {
                peer_id: ticket.data.signer,
                ticket_hash: arena0_protocol::TicketHash::of(&ticket.data),
            })
            .collect(),
    }
}

fn project_private_commit_summary(
    summary: StorePrivateCommitSummary,
) -> Result<ApiPrivateCommitSummary, ApiError> {
    let input_payload_bytes = summary
        .input_payload_bytes
        .map(u64::try_from)
        .transpose()
        .map_err(|_| ApiError::new(ApiErrorCode::Internal, "private input size overflows u64"))?;
    let effects = summary
        .effects
        .into_iter()
        .map(|effect| {
            let payload_bytes = effect
                .payload_bytes
                .map(u64::try_from)
                .transpose()
                .map_err(|_| {
                    ApiError::new(ApiErrorCode::Internal, "private effect size overflows u64")
                })?;
            Ok(ApiPrivateEffectSummary {
                kind: match effect.kind {
                    arena0_store::PrivateEffectKind::Broadcast => ApiPrivateEffectKind::Broadcast,
                    arena0_store::PrivateEffectKind::Callout => ApiPrivateEffectKind::Callout,
                    arena0_store::PrivateEffectKind::SetTimer => ApiPrivateEffectKind::SetTimer,
                    arena0_store::PrivateEffectKind::Sign => ApiPrivateEffectKind::Sign,
                    arena0_store::PrivateEffectKind::RetryInput => ApiPrivateEffectKind::RetryInput,
                },
                payload_bytes,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(ApiPrivateCommitSummary {
        sequence: summary.sequence,
        public_position: summary.public_position,
        event: match summary.event {
            arena0_store::PrivateEventKind::InputReceived => ApiPrivateEventKind::InputReceived,
            arena0_store::PrivateEventKind::TimerFired => ApiPrivateEventKind::TimerFired,
            arena0_store::PrivateEventKind::TypedTimerFired => ApiPrivateEventKind::TypedTimerFired,
            arena0_store::PrivateEventKind::Signed => ApiPrivateEventKind::Signed,
            arena0_store::PrivateEventKind::React => ApiPrivateEventKind::React,
        },
        input_payload_bytes,
        effects,
        fuel_used: summary.fuel_used,
    })
}

fn receipt_list_entry(stored: arena0_store::StoredReceipt) -> arena0_api::ReceiptListEntry {
    let provenance = match stored.provenance() {
        arena0_store::ReceiptProvenance::Produced => arena0_api::ReceiptProvenance::Produced,
        arena0_store::ReceiptProvenance::Imported => arena0_api::ReceiptProvenance::Imported,
        arena0_store::ReceiptProvenance::Both => arena0_api::ReceiptProvenance::Both,
    };
    arena0_api::ReceiptListEntry {
        receipt_id: hex::encode(stored.receipt_id.as_bytes()),
        session_id: stored.receipt.body().header().session_hash(),
        kind: stored.receipt.kind(),
        program_id: stored.receipt.body().header().program_hash(),
        completed: stored
            .receipt
            .body()
            .header()
            .terminal
            .completed()
            .is_some(),
        provenance,
    }
}

/// Run a blocking store call off the async runtime, mapping errors to `ApiError`.
async fn blocking<T, F, E>(f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, E> + Send + 'static,
    E: Into<ApiError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::new(ApiErrorCode::Internal, format!("join: {e}")))?
        .map_err(Into::into)
}

impl From<KeystoreError> for ApiError {
    fn from(error: KeystoreError) -> Self {
        match error {
            KeystoreError::NotFound { reference } => Self::new(
                ApiErrorCode::NotFound,
                format!("no identity matches {reference:?}"),
            ),
            KeystoreError::Ambiguous { reference, matches } => Self::new(
                ApiErrorCode::Ambiguous,
                format!(
                    "identity reference '{reference}' is ambiguous: {matches} identities match"
                ),
            ),
            KeystoreError::InvalidLabel(message) => Self::new(ApiErrorCode::BadRequest, message),
            KeystoreError::ActiveIdentityRemoval => Self::new(
                ApiErrorCode::BadRequest,
                "cannot remove the active Host identity; rotate it through a lifecycle-aware operation",
            ),
            KeystoreError::Storage(error) => Self::new(ApiErrorCode::Storage, error.to_string()),
        }
    }
}

fn catalog_api_error(error: CatalogError) -> ApiError {
    match error {
        CatalogError::InvalidProgram(error) => {
            ApiError::new(ApiErrorCode::BadRequest, format!("import program: {error}"))
        }
        CatalogError::Storage(error) => {
            ApiError::new(ApiErrorCode::Storage, format!("import program: {error}"))
        }
    }
}

/// Select one bounded negotiation bootstrap while retaining an explicit target.
fn negotiation_bootstrap(
    local_peer: PeerId,
    preferred_peer: Option<PeerId>,
    peers: impl IntoIterator<Item = PeerId>,
) -> Vec<PeerId> {
    let preferred_peer = preferred_peer.filter(|peer| *peer != local_peer);
    let mut peers = peers
        .into_iter()
        .filter(|peer| *peer != local_peer && Some(*peer) != preferred_peer)
        .collect::<Vec<_>>();
    peers.sort_unstable();
    peers.dedup();
    peers.truncate(
        arena0_transport::MAX_PROGRAM_BOOTSTRAP_PEERS - usize::from(preferred_peer.is_some()),
    );
    if let Some(preferred_peer) = preferred_peer {
        peers.push(preferred_peer);
    }
    peers
}

/// Subscribe to the one program-scoped negotiation topic.
impl HostService {
    async fn subscribe_negotiation(
        &self,
        program_id: ProgramHash,
        bootstrap: Vec<PeerId>,
    ) -> Result<Box<dyn NegotiationTopic>, ApiError> {
        tracing::debug!(
            local_peer = %self.peer_id,
            %program_id,
            bootstrap = ?bootstrap,
            "subscribing to negotiation topic"
        );
        self.transport
            .subscribe_program(program_id, bootstrap)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Negotiation, error.to_string()))
    }
}

/// The post-commit relay cadence: re-emit the final offer and the exact
/// tickets while the session is live.
const RELAY_CADENCE_MS: u64 = 2_000;

impl HostService {
    /// The post-commit relay: session-lived, re-emits the final offer and the
    /// exact tickets on the program topic and serves the convergence fetch from
    /// the committed `ActivationRecord`. Stops when the session ends and
    /// unregisters its fetch handler.
    async fn run_session_relay(
        self: Arc<Self>,
        entry: Arc<ExecutionHandle>,
        committed: ActivatedSession,
    ) {
        let session_hash = committed.session_hash();
        let program_id = match entry.program_id().await {
            Ok(program_id) => program_id,
            Err(error) => {
                tracing::error!(exec_id = %entry.exec_id(), %error, "read relay program id failed");
                return;
            }
        };
        let mut fetch_rx = self.runtime.register_fetch_handler(session_hash);
        let bootstrap = committed
            .activation()
            .tickets()
            .iter()
            .map(|ticket| ticket.data.signer)
            .collect::<Vec<_>>();
        let topic = match self
            .subscribe_negotiation(program_id, bootstrap.clone())
            .await
        {
            Ok(topic) => Some(topic),
            Err(error) => {
                tracing::warn!(%session_hash, %error, "relay could not subscribe to the program topic");
                None
            }
        };
        if entry
            .lifecycle()
            .await
            .is_ok_and(ExecLifecycle::is_terminal)
        {
            self.runtime.unregister_fetch_handler(session_hash);
            return;
        }
        let mut topic = topic;
        let mut cadence = tokio::time::interval(Duration::from_millis(RELAY_CADENCE_MS));
        loop {
            tokio::select! {
                () = entry.wait_for_change() => {
                    if entry
                        .lifecycle()
                        .await
                        .is_ok_and(ExecLifecycle::is_terminal)
                    {
                        break;
                    }
                }
                routed = fetch_rx.recv() => {
                    let Some((recv, frame)) = routed else { break };
                    let Ok(Some(record)) = self.store.load_activation(entry.exec_id()).await else {
                        continue;
                    };
                    let FetchFrame::FetchActivationTickets(request) = frame else {
                        continue;
                    };
                    arena0_node::serve_fetch_evidence(
                        &self.transport,
                        &recv,
                        request,
                        session_hash,
                        record.prepared().tickets(),
                        Instant::now() + arena0_node::FETCH_TIMEOUT,
                    )
                    .await;
                }
                _ = cadence.tick() => {
                    if entry
                        .lifecycle()
                        .await
                        .is_ok_and(ExecLifecycle::is_terminal)
                    {
                        break;
                    }
                    let Ok(Some(record)) = self.store.load_activation(entry.exec_id()).await else {
                        continue;
                    };
                    let Some(activation) = record.activation().cloned() else {
                        continue;
                    };
                    let negotiation_id = activation.offer().data().negotiation_id;
                    // Rejoin a closed topic: the relay must keep serving late
                    // joiners for the session's whole life.
                    if topic.is_none() {
                        match self
                            .subscribe_negotiation(program_id, bootstrap.clone())
                            .await
                        {
                            Ok(new_topic) => topic = Some(new_topic),
                            Err(error) => {
                                tracing::warn!(%session_hash, %error, "relay could not rejoin the program topic");
                                continue;
                            }
                        }
                    }
                    let mut closed = false;
                    if let Some(topic) = &mut topic {
                        let frame = NegotiationGossip::new(
                            program_id,
                            negotiation_id,
                            NegotiationFact::ActivationAnnouncement(
                                ActivationAnnouncement::from_activation(&activation),
                            ),
                        )
                        .signing_bytes();
                        if topic.publish(frame.into()).await.is_err() {
                            closed = true;
                        }
                        if !closed {
                            for ticket in activation.tickets() {
                                let frame = NegotiationGossip::new(
                                    program_id,
                                    negotiation_id,
                                    NegotiationFact::Ticket(ticket.clone()),
                                )
                                .signing_bytes();
                                if topic.publish(frame.into()).await.is_err() {
                                    closed = true;
                                    break;
                                }
                            }
                        }
                    }
                    if closed {
                        if let Some(topic) = &mut topic {
                            let _ = topic.close().await;
                        }
                        topic = None;
                    }
                }
            }
        }
        self.runtime.unregister_fetch_handler(session_hash);
    }
}

fn recovery_event_source(peer_id: PeerId, candidate: &RecoveryCandidate) -> EventSource {
    let request = candidate.request();
    match (candidate.has_execution(), candidate.session_id()) {
        (true, Some(session_hash)) => EventSource::Session {
            peer_id,
            exec_id: request.execution_id(),
            program_id: request.program_hash(),
            session_hash,
        },
        _ => match request.negotiation_id() {
            Some(negotiation_id) => EventSource::Negotiation {
                peer_id,
                exec_id: request.execution_id(),
                program_id: request.program_hash(),
                negotiation_id,
            },
            // An open Join may fail before any offer is accepted, so no
            // negotiation identity exists for its recovery event.
            None => EventSource::Execution {
                peer_id,
                exec_id: request.execution_id(),
                program_id: request.program_hash(),
            },
        },
    }
}

fn recovery_reason(reason: String) -> String {
    let mut reason = format!("recovery: {reason}");
    while reason.len() > arena0_protocol::MAX_TERMINAL_REASON_BYTES {
        let _ = reason.pop();
    }
    reason
}

fn local_withdrawal() -> ApiError {
    ApiError::new(ApiErrorCode::Negotiation, LOCAL_WITHDRAWAL_REASON)
}

async fn wait_for_withdrawal(withdrawal: &mut watch::Receiver<bool>) {
    loop {
        if *withdrawal.borrow_and_update() {
            return;
        }
        if withdrawal.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Check the creator's Active ticket against the exact offer body and the
/// creator-first hash committed by that offer. This is the authentication
/// boundary before an open Join is durably bound or allowed to issue a ticket.
fn creator_ticket_matches(offer: &Offer, ticket: &Ticket) -> bool {
    ticket.data.signer == offer.data().creator
        && ticket.data.negotiation_id == offer.data().negotiation_id
        && ticket.data.offer_seq == offer.data().offer_seq
        && matches!(ticket.data.action, TicketAction::Active { .. })
        && offer
            .tickets()
            .first()
            .is_some_and(|hash| *hash == TicketHash::of(&ticket.data))
        && ticket
            .verify_for_offer(&OfferHash::of(offer.data()))
            .is_ok()
        && creator_ticket_has_window(ticket)
}

fn creator_ticket_has_window(ticket: &Ticket) -> bool {
    let TicketAction::Active {
        issued_at_unix_ms,
        valid_for_ms,
        ..
    } = &ticket.data.action
    else {
        return false;
    };
    let now = unix_time_ms();
    *issued_at_unix_ms <= now.saturating_add(MAX_CLOCK_SKEW_MS)
        && issued_at_unix_ms.saturating_add(u64::from(*valid_for_ms))
            >= now.saturating_add(MAX_CLOCK_SKEW_MS + PREPARE_WINDOW_MS)
}

async fn offer_is_usable_for_join(
    offer: &Offer,
    program: &Program,
    engine: Arc<WasmtimeEngine>,
    deadline: Option<Instant>,
) -> bool {
    let data = offer.data();
    let now = unix_time_ms();
    if data.deadline_unix_ms <= now.saturating_add(PREPARE_WINDOW_MS)
        || data
            .validate_for_profile(arena0_program::ExecutionProfile::current().hash())
            .is_err()
        || !program
            .definition()
            .metadata
            .participants
            .accepts(data.target_size)
        || deadline.is_some_and(|deadline| Instant::now() >= deadline)
    {
        return false;
    }
    // Decode and initialize the offered params before binding the durable
    // target. A preference mismatch remains eligible: the creator may counter
    // it after this offer is authenticated.
    let Ok((_, initial_state)) = load_for(
        program,
        engine,
        data.params.as_bytes().to_vec(),
        "join offer",
    )
    .await
    else {
        return false;
    };
    initial_state == data.initial_state
        && data.deadline_unix_ms > unix_time_ms().saturating_add(PREPARE_WINDOW_MS)
        && deadline.is_none_or(|deadline| Instant::now() < deadline)
}

/// Wire negotiation's durable boundaries directly to the supplied execution
/// writer. No daemon aggregate or callback-side state is retained.
fn activation_callbacks() -> (
    arena0_node::PrepareEffect,
    arena0_node::PersistActivationEffect,
) {
    let prepare: arena0_node::PrepareEffect = Box::new(|store, prepared| {
        Box::pin(async move {
            match store.prepare_activation(prepared, unix_time_ms()).await {
                Ok(
                    arena0_store::PrepareActivationOutcome::Prepared(_)
                    | arena0_store::PrepareActivationOutcome::AlreadyPrepared(_)
                    | arena0_store::PrepareActivationOutcome::AlreadyCommitted(_),
                ) => Ok(PrepareOutcome::Accepted),
                Ok(arena0_store::PrepareActivationOutcome::Conflict { .. }) => {
                    Ok(PrepareOutcome::Conflict)
                }
                Err(error) => Err(error.to_string()),
            }
        })
    });
    let persist_activation: arena0_node::PersistActivationEffect = Box::new(|store, activation| {
        Box::pin(async move {
            match store.commit_activation(activation, unix_time_ms()).await {
                Ok(
                    arena0_store::CommitActivationOutcome::Committed(_)
                    | arena0_store::CommitActivationOutcome::AlreadyCommitted(_),
                ) => Ok(DurableOutcome::Accepted),
                Ok(arena0_store::CommitActivationOutcome::Conflict { .. }) => {
                    Ok(DurableOutcome::Conflict)
                }
                Err(error) => Err(error.to_string()),
            }
        })
    });
    (prepare, persist_activation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_api::NextEvent;
    use arena0_crypto::bls::BlsSecretKey;
    use arena0_crypto::{BlsSignature, SecretKey, key_binding_message};
    use arena0_program::{LocalStateBytes, SharedStateBytes};
    use arena0_protocol::execution::{ExecutionInput, ExecutionState};
    use arena0_protocol::{AbortKind, Activation, ActivationData, PreparedActivation};
    use arena0_transport::local::{LocalNetwork, LocalTransport};

    fn test_daemon() -> (
        tempfile::TempDir,
        arena0_store::Store,
        Arc<HostService>,
        PeerId,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let keys = dir.path().join("keys");
        std::fs::create_dir_all(&keys).unwrap();
        let keystore = Arc::new(Keystore::open(keys).unwrap());
        let identity = keystore.new_identity(Some("host-01".into())).unwrap();

        let store = arena0_store::Store::open(arena0_store::StoreConfig::new(
            dir.path().join("arena0.sqlite"),
            identity.peer_id,
        ))
        .unwrap();
        let store_handle = store.handle().clone();
        let catalog = ProgramCatalog::new(store_handle.clone());
        let engine = Arc::new(WasmtimeEngine::new().unwrap());

        let network = LocalNetwork::new();
        let mut transports =
            LocalTransport::create_network(&network, vec![identity.peer_id]).expect("test network");
        let daemon = HostService::start(HostServiceInit {
            name: "host-01".into(),
            transport: Arc::new(transports.remove(0)),
            keystore,
            catalog,
            store: store_handle,
            engine,
            startup: Arc::new(StartupTimeline::new(1, 0)),
        })
        .unwrap();
        (dir, store, daemon, identity.peer_id)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn register_live_reuses_one_handle_per_execution() {
        let (_dir, _store, daemon, _peer) = test_daemon();
        let execution_id = ExecId([0xA5; 32]);
        let first = daemon.execs.register_live(execution_id);
        let second = daemon.execs.register_live(execution_id);
        let distinct = daemon.execs.register_live(ExecId([0xA6; 32]));

        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &distinct));
        daemon.stop().await;
    }

    #[test]
    fn private_inspection_projection_has_no_private_payload_fields() {
        let summary = arena0_store::PrivateCommitSummary {
            sequence: 4,
            public_position: 3,
            event: arena0_store::PrivateEventKind::InputReceived,
            input_payload_bytes: Some(2),
            effects: vec![arena0_store::PrivateEffectSummary {
                kind: arena0_store::PrivateEffectKind::Callout,
                payload_bytes: Some(8),
            }],
            fuel_used: 17,
        };
        let projected = project_private_commit_summary(summary).expect("projection");
        assert_eq!(projected.sequence, 4);
        assert_eq!(projected.public_position, 3);
        assert_eq!(projected.event, ApiPrivateEventKind::InputReceived);
        assert_eq!(projected.input_payload_bytes, Some(2));
        assert_eq!(projected.effects[0].kind, ApiPrivateEffectKind::Callout);
        assert_eq!(projected.effects[0].payload_bytes, Some(8));
        let encoded = serde_json::to_value(projected).expect("projection JSON");
        assert!(encoded.get("data").is_none());
        assert!(encoded.get("context").is_none());
        assert!(encoded.get("signature").is_none());
        assert!(encoded.get("local_state").is_none());
    }

    #[test]
    fn negotiation_bootstrap_retains_the_explicit_creator() {
        let local = PeerId([0xff; 32]);
        let creator = PeerId([0xfe; 32]);
        let candidates = (0_u8..16).map(|value| PeerId([value; 32]));

        let bootstrap = negotiation_bootstrap(local, Some(creator), candidates);

        assert_eq!(
            bootstrap.len(),
            arena0_transport::MAX_PROGRAM_BOOTSTRAP_PEERS
        );
        assert!(bootstrap.contains(&creator));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn activity_publisher_sequences_are_monotonic_under_concurrent_emitters() {
        const EMITTERS: usize = 8;
        const FRAMES_PER_EMITTER: usize = 64;
        let activity = Arc::new(Activity::new());
        let mut receiver = activity.subscribe();
        let mut emitters = Vec::with_capacity(EMITTERS);
        for emitter in 0..EMITTERS {
            let activity = Arc::clone(&activity);
            emitters.push(tokio::spawn(async move {
                for frame in 0..FRAMES_PER_EMITTER {
                    activity.emit(ActivityData::Started {
                        call_id: format!("{emitter}-{frame}"),
                        tool: "test".into(),
                        host: None,
                        exec_id: None,
                    });
                    tokio::task::yield_now().await;
                }
            }));
        }
        for emitter in emitters {
            emitter.await.expect("activity emitter joined");
        }

        let mut sequences = Vec::with_capacity(EMITTERS * FRAMES_PER_EMITTER);
        for _ in 0..(EMITTERS * FRAMES_PER_EMITTER) {
            sequences.push(receiver.recv().await.expect("activity frame received").seq);
        }
        assert_eq!(
            sequences,
            (1..=(EMITTERS * FRAMES_PER_EMITTER) as u64).collect::<Vec<_>>()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn host_started_snapshot_bypasses_filter_and_uses_zero_sequence() {
        let (_dir, _store, daemon, _peer) = test_daemon();
        let filter =
            EventFilter::try_new(vec!["exec.*".into()], vec!["host.started".into()]).unwrap();
        let started = daemon.host_started_frame();
        assert_eq!(started.kind(), "host.started");
        assert_eq!(started.seq, 0);
        assert_eq!(started.host.id, "host-01");
        assert_eq!(started.boot_id.len(), 32);
        assert!(filter.matches(&started));
        daemon.stop().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unix_subscription_filters_without_collapsing_publisher_sequences() {
        let (_dir, _store, daemon, _peer) = test_daemon();
        let receiver = daemon.events.subscribe();
        let filter = EventFilter::try_new(vec!["exec.created".into()], vec![]).unwrap();
        let exec_id = ExecId([0xA1; 32]);
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let (mut server_read, mut server_write) = tokio::io::split(server_io);
        let (mut client_read, _client_write) = tokio::io::split(client_io);
        let stream_daemon = Arc::clone(&daemon);
        let task = tokio::spawn(async move {
            stream_daemon
                .stream_events_unix(filter, receiver, &mut server_read, &mut server_write)
                .await
        });

        daemon.events.emit(HostEvent::HostStopped {
            reason: Some("filtered".into()),
            uptime_secs: 1,
        });
        daemon.events.emit(HostEvent::Created {
            source: EventSource::Execution {
                peer_id: daemon.peer_id,
                exec_id,
                program_id: ProgramHash([0xA2; 32]),
            },
            negotiation_id: None,
            queue_position: None,
            origin: ExecCreationOrigin::Request,
        });

        let received: EventFrame =
            tokio::time::timeout(Duration::from_secs(1), frame::read_frame(&mut client_read))
                .await
                .expect("filtered subscription response")
                .expect("filtered subscription frame")
                .expect("subscription frame body");
        assert_eq!(received.kind(), "exec.created");
        assert_eq!(
            received.seq, 2,
            "filtered frames still consume publisher seq"
        );
        assert_eq!(received.exec_id, Some(exec_id));
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                frame::read_frame::<_, EventFrame>(&mut client_read)
            )
            .await
            .is_err()
        );
        task.abort();
        let _ = task.await;
        daemon.stop().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unix_subscription_reports_exact_last_skipped_sequence() {
        let (_dir, _store, daemon, _peer) = test_daemon();
        for uptime_secs in 0..100 {
            daemon.events.emit(HostEvent::HostStopped {
                reason: Some("before-subscribe".into()),
                uptime_secs,
            });
        }
        let receiver = daemon.events.subscribe();
        let filter = EventFilter::try_new(vec!["exec.*".into()], vec![]).unwrap();
        for uptime_secs in 0..=(EVENT_BUS_CAP as u64 + 1) {
            daemon.events.emit(HostEvent::HostStopped {
                reason: Some("lag-test".into()),
                uptime_secs,
            });
        }
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let (mut server_read, mut server_write) = tokio::io::split(server_io);
        let (mut client_read, _client_write) = tokio::io::split(client_io);
        let stream_daemon = Arc::clone(&daemon);
        let task = tokio::spawn(async move {
            stream_daemon
                .stream_events_unix(filter, receiver, &mut server_read, &mut server_write)
                .await
        });

        let received: EventFrame =
            tokio::time::timeout(Duration::from_secs(1), frame::read_frame(&mut client_read))
                .await
                .expect("lagged subscription response")
                .expect("lagged subscription frame")
                .expect("lagged frame body");
        assert_eq!(received.kind(), "stream.lagged");
        assert_eq!(
            received.seq, 102,
            "seq is the last skipped publisher sequence"
        );
        assert!(matches!(received.data, EventData::Lagged { skipped: 2 }));
        assert!(
            EventFilter::try_new(vec!["exec.*".into()], vec!["stream.lagged".into()])
                .unwrap()
                .matches(&received)
        );
        task.abort();
        let _ = task.await;
        daemon.stop().await;
    }

    #[tokio::test]
    async fn released_socket_owner_cannot_unlink_a_replacement() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("host.sock");
        let old = UnixSocket::new(path.clone());
        let listener = old.bind().unwrap();
        old.remove_owned_path();
        drop(listener);
        let replacement = UnixListener::bind(&path).unwrap();
        drop(old);
        assert!(UnixStream::connect(&path).await.is_ok());
        drop(replacement);
    }

    #[tokio::test]
    async fn shutdown_before_listener_poll_closes_without_accepting() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("daemon.sock");
        let socket = UnixSocket::new(path.clone());
        let listener = socket.bind().unwrap();
        let _queued = UnixStream::connect(&path).await.unwrap();
        let (stop, stopped) = tokio::sync::watch::channel(false);
        stop.send_replace(true);
        let accepted = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&accepted);
        tokio::time::timeout(
            Duration::from_secs(1),
            socket.listen(listener, stopped, move |_| {
                observed.store(true, Ordering::Release);
                async { Ok(()) }
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!accepted.load(Ordering::Acquire));
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn shared_socket_rejects_competing_owners_and_recovers_stale_path() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("nested/daemon.sock");
        let owner = UnixSocket::new(path.clone());
        let listener = owner.bind().unwrap();
        let competing = UnixSocket::new(path.clone());
        assert!(competing.bind().is_err());
        assert!(UnixStream::connect(&path).await.is_ok());
        drop(listener);
        drop(owner);

        // A listener that does not participate in the sidecar lease is also
        // protected; only a refused, stale socket may be removed.
        let external = UnixListener::bind(&path).unwrap();
        assert!(competing.bind().is_err());
        assert!(UnixStream::connect(&path).await.is_ok());
        drop(external);
        let recovered = competing.bind().unwrap();
        assert!(UnixStream::connect(&path).await.is_ok());
        drop(recovered);
        drop(competing);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn shared_socket_rejects_symlinked_parent() {
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = home.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let socket = UnixSocket::new(link.join("daemon.sock"));
        assert!(socket.bind().is_err());
        assert!(!target.join("daemon.sock").exists());
        assert!(!target.join("daemon.sock.lock").exists());
    }

    #[tokio::test]
    async fn failed_socket_bind_preserves_an_unowned_path() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("host.sock");
        std::fs::write(&path, b"occupied").unwrap();
        let socket = UnixSocket::new(path.clone());
        assert!(socket.bind().is_err());
        drop(socket);
        assert_eq!(std::fs::read(&path).unwrap(), b"occupied");
    }

    #[tokio::test]
    async fn host_event_projects_to_api_once() {
        let feed = Events::new(HostInfo {
            id: "paired".into(),
            peer_id: PeerId([1; 32]),
            user_agent: None,
        });
        let mut api_events = feed.subscribe();
        let event = HostEvent::Created {
            source: EventSource::Execution {
                peer_id: PeerId([0x31; 32]),
                exec_id: ExecId([0x32; 32]),
                program_id: ProgramHash([0x33; 32]),
            },
            negotiation_id: None,
            queue_position: None,
            origin: ExecCreationOrigin::Request,
        };
        assert!(event.system_event().is_some());

        feed.emit(event.clone());

        let received = api_events.recv().await.unwrap();
        assert!(matches!(received.data, EventData::Created { .. }));
        assert_eq!(received.exec_id, Some(ExecId([0x32; 32])));
        assert_eq!(received.seq, 1);
        assert_eq!(received.host.id, "paired");
        assert_eq!(received.boot_id.len(), 32);

        let mut lagged = feed.subscribe();
        for _ in 0..=EVENT_BUS_CAP {
            feed.emit(event.clone());
        }
        assert!(matches!(
            lagged.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(1))
        ));
    }

    #[tokio::test]
    async fn event_metadata_is_captured_after_persistence_without_relabelling_old_frames() {
        let (_dir, _store, daemon, peer) = test_daemon();
        let mut frames = daemon.events.subscribe();
        daemon.set_user_agent("claude-code/1".into()).await.unwrap();
        daemon.events.emit(HostEvent::HostStopped {
            reason: None,
            uptime_secs: 1,
        });
        daemon.set_user_agent("codex/2".into()).await.unwrap();
        daemon.events.emit(HostEvent::HostStopped {
            reason: None,
            uptime_secs: 2,
        });
        let first = frames.recv().await.unwrap();
        let second = frames.recv().await.unwrap();
        assert_eq!(first.host.peer_id, peer);
        assert_eq!(first.host.user_agent.as_deref(), Some("claude-code/1"));
        assert_eq!(second.host.user_agent.as_deref(), Some("codex/2"));
        assert_eq!(second.seq, first.seq + 1);
        assert_eq!(
            daemon.store.load_user_agent().await.unwrap().as_deref(),
            Some("codex/2")
        );
        let snapshot = daemon.host_started_frame();
        assert_eq!(snapshot.seq, 0);
        assert_eq!(snapshot.host, second.host);
        daemon.stop().await;
    }

    #[tokio::test]
    async fn host_info_reports_the_node_identity() {
        let (_dir, _store, daemon, peer) = test_daemon();
        match daemon.dispatch(HostRequest::Info).await {
            Ok(ResponseOk::HostStatus(info)) => {
                assert_eq!(info.host.id, "host-01");
                assert_eq!(info.host.peer_id, peer);
                assert_eq!(info.programs, 0);
            }
            other => panic!("expected host.info, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn withdrawal_intercepts_a_caller_owned_id_before_creation() {
        let (_dir, _store, daemon, peer) = test_daemon();
        let exec_id = ExecId([0x3a; 32]);
        let cancelling_daemon = Arc::clone(&daemon);
        let cancellation = tokio::spawn(async move {
            cancelling_daemon
                .dispatch(HostRequest::ExecCancelCreation { exec_id })
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if matches!(
                    daemon
                        .creation_states
                        .lock()
                        .expect("creation states")
                        .0
                        .get(&exec_id)
                        .map(|entry| entry.state),
                    Some(CreationState::Awaiting)
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancellation registered its arrival rendezvous");

        let response = daemon
            .dispatch(HostRequest::ExecNew {
                exec_id,
                program: "missing".to_owned(),
                params: None,
                ensemble: EnsembleSpec::Explicit {
                    peers: vec![PeerId([peer.0[0].wrapping_add(1); 32])],
                },
            })
            .await;
        assert!(matches!(
            response,
            Err(ApiError {
                code: ApiErrorCode::Negotiation,
                ..
            })
        ));
        assert!(matches!(
            cancellation.await.expect("cancellation task"),
            Ok(ResponseOk::Ack)
        ));
        assert!(
            daemon
                .store
                .load_execution_request(exec_id)
                .await
                .expect("load request")
                .is_none()
        );
        daemon.stop().await;
    }

    #[tokio::test]
    async fn cancellation_completion_propagates_cleanup_failure_and_releases_the_id() {
        let mut states = CreationStates::default();
        let active = ExecId([0x3b; 32]);
        states.begin(active).expect("begin active creation");
        let completion = states.cancel_existing(active).expect("active creation");
        assert!(states.is_cancelled(active));
        assert!(states.begin_finish(active, false));
        states.complete_cancelled(
            active,
            Err(ApiError::new(ApiErrorCode::Storage, "cleanup failed")),
        );
        assert!(
            matches!(
                creation_completion(completion).await,
                Err(ApiError {
                    code: ApiErrorCode::Storage,
                    ..
                })
            ),
            "withdrawal observes the cleanup failure"
        );
        assert!(states.begin(active).is_ok(), "completed state was released");
    }

    #[tokio::test]
    async fn dropped_arrival_wait_releases_the_unknown_id() {
        let states = StdMutex::new(CreationStates::default());
        let exec_id = ExecId([0x3c; 32]);
        let completion = states
            .lock()
            .expect("creation states")
            .await_creation(exec_id);
        drop(CreationArrivalWait::new(&states, exec_id));
        assert!(matches!(
            creation_completion(completion).await,
            Err(ApiError {
                code: ApiErrorCode::NotFound,
                ..
            })
        ));
        assert!(
            states
                .lock()
                .expect("creation states")
                .begin(exec_id)
                .is_ok(),
            "dropped waiter released the id"
        );
    }

    #[tokio::test]
    async fn exec_history_does_not_depend_on_a_live_entry() {
        let (_dir, store, daemon, _peer) = test_daemon();
        let exec_id = ExecId([0x42; 32]);
        let negotiation_id = NegotiationId([0x24; 32]);
        let (program_hash, _) = store
            .handle()
            .register_program(vec![1, 2, 3], 1)
            .await
            .unwrap();
        let mut writer = daemon.runtime.claim_execution(exec_id).unwrap();
        writer
            .create_execution_request(
                program_hash,
                Some(JsonBytes::try_new(b"null".to_vec()).unwrap()),
                ExecutionAdmission::join(PeerId([0x11; 32]), negotiation_id),
                1,
            )
            .await
            .unwrap();
        writer
            .record_execution_request_failure("stored failure")
            .await
            .unwrap();

        match daemon.dispatch(HostRequest::ExecStatus { exec_id }).await {
            Ok(ResponseOk::Status(status)) => {
                assert_eq!(status.exec_id, exec_id);
                assert_eq!(status.lifecycle(), ExecLifecycle::Failed);
            }
            other => panic!("expected exec.status, got {other:?}"),
        }
        match daemon
            .dispatch(HostRequest::ExecInspect {
                exec_id,
                private_from: Some(0),
                private_limit: MAX_PRIVATE_INSPECTION_RECORDS as u16,
            })
            .await
        {
            Ok(ResponseOk::Inspection(inspection)) => {
                assert_eq!(inspection.status.exec_id, exec_id);
                assert_eq!(inspection.status.lifecycle(), ExecLifecycle::Failed);
                assert!(inspection.activation.is_none());
                assert!(inspection.private.is_empty());
                assert_eq!(inspection.private_total, 0);
                assert_eq!(inspection.private_next, None);
            }
            other => panic!("expected exec.inspect, got {other:?}"),
        }
        assert!(matches!(
            daemon
                .dispatch(HostRequest::ExecInspect {
                    exec_id,
                    private_from: Some(0),
                    private_limit: (MAX_PRIVATE_INSPECTION_RECORDS + 1) as u16,
                })
                .await,
            Err(ApiError {
                code: ApiErrorCode::BadRequest,
                ..
            })
        ));
        assert!(matches!(
            daemon
                .dispatch(HostRequest::ExecInspect {
                    exec_id,
                    private_from: Some(0),
                    private_limit: 0,
                })
                .await,
            Err(ApiError {
                code: ApiErrorCode::BadRequest,
                ..
            })
        ));
        match daemon.dispatch(HostRequest::ExecList).await {
            Ok(ResponseOk::ExecList(statuses)) => {
                assert_eq!(statuses.len(), 1);
                assert_eq!(statuses[0].exec_id, exec_id);
            }
            other => panic!("expected exec.list, got {other:?}"),
        }
        match daemon.dispatch(HostRequest::ExecNext { exec_id }).await {
            Ok(ResponseOk::Next(NextEvent::Failed { reason })) => {
                assert_eq!(reason, "stored failure");
            }
            other => panic!("expected durable exec.next, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recovery_visits_requests_after_more_than_4096_terminal_rows() {
        let (_dir, store, daemon, peer) = test_daemon();
        let (program_hash, _) = store
            .handle()
            .register_program(vec![1, 2, 3], 1)
            .await
            .expect("program");
        let other = PeerId([peer.0[0].wrapping_add(1); 32]);
        let tail = 4_097_u64;
        let tail_id = ExecId({
            let mut bytes = [0; 32];
            bytes[..8].copy_from_slice(&tail.to_le_bytes());
            bytes
        });
        for index in 1..=tail {
            let execution_id = ExecId({
                let mut bytes = [0; 32];
                bytes[..8].copy_from_slice(&index.to_le_bytes());
                bytes
            });
            let mut writer = daemon.runtime.claim_execution(execution_id).unwrap();
            writer
                .create_execution_request(
                    program_hash,
                    Some(JsonBytes::try_new(b"null".to_vec()).expect("params")),
                    ExecutionAdmission::explicit(
                        NegotiationId({
                            let mut bytes = [0; 32];
                            bytes[..8].copy_from_slice(&index.to_le_bytes());
                            bytes
                        }),
                        vec![peer, other],
                    )
                    .expect("admission"),
                    index,
                )
                .await
                .expect("request");
            if index < tail {
                writer
                    .record_execution_request_failure("terminal history")
                    .await
                    .expect("terminal request");
            }
        }

        daemon.resume_durable().await.expect("recovery");

        let request = store
            .handle()
            .load_execution_request(tail_id)
            .await
            .expect("tail request")
            .expect("tail exists");
        assert!(request.failure().is_some(), "tail request was not failed");
        assert!(daemon.execs.get(&tail_id).is_none());
        daemon.stop().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recovery_fails_unrecoverable_committed_execution_durably() {
        let (_dir, store, daemon, peer) = test_daemon();
        let program_bytes = vec![1, 2, 3];
        let (program_hash, _) = store
            .handle()
            .register_program(program_bytes, 1)
            .await
            .expect("program");
        let execution_id = ExecId([0xA4; 32]);
        let negotiation_id = NegotiationId([0xA5; 32]);
        let other_keys = NodeKeys::from_secret(SecretKey::from_bytes([2; 32]));
        let other = PeerId::from_ed25519(&other_keys.ed25519_public_key());
        let producer_bls = BlsSecretKey::from_seed(&[11; 32]).expect("producer bls");
        let other_bls = BlsSecretKey::from_seed(&[12; 32]).expect("other bls");
        let offer_data = OfferData::new(
            negotiation_id,
            0,
            peer,
            program_hash,
            arena0_program::ExecutionProfile::current().hash(),
            JsonBytes::try_new(b"null".to_vec()).expect("params"),
            2,
            StateHash::of(&[0]),
            u64::MAX,
        )
        .expect("offer");
        let offer_hash = arena0_protocol::OfferHash::of(&offer_data);
        let make_ticket = |keys: &NodeKeys, bls: &BlsSecretKey| {
            let signer = PeerId::from_ed25519(&keys.ed25519_public_key());
            let execution_bls = bls.public_key();
            let key_binding = bls.sign_binding(&key_binding_message(
                &offer_hash.0,
                &signer.0,
                &execution_bls,
            ));
            let data = TicketData::new(
                negotiation_id,
                0,
                signer,
                0,
                TicketAction::Active {
                    execution_bls,
                    key_binding,
                    issued_at_unix_ms: 1,
                    valid_for_ms: 60_000,
                },
            )
            .expect("ticket data");
            Ticket {
                signature: keys.sign(&data.signing_bytes()),
                data,
            }
        };
        let tickets = vec![
            make_ticket(daemon.identity.as_ref(), &producer_bls),
            make_ticket(&other_keys, &other_bls),
        ];
        let ticket_hashes = tickets
            .iter()
            .map(|ticket| arena0_protocol::TicketHash::of(&ticket.data))
            .collect::<Vec<_>>();
        let activation_data =
            ActivationData::new(offer_hash, ticket_hashes.clone()).expect("activation data");
        let aggregate = BlsSignature::aggregate(&[
            producer_bls.sign(&activation_data.signing_bytes()),
            other_bls.sign(&activation_data.signing_bytes()),
        ])
        .expect("activation aggregate");
        let offer = Offer::new(offer_data, ticket_hashes).expect("offer");
        let prepared = PreparedActivation::new(offer, tickets).expect("prepared");
        let activation = Activation::new(prepared.clone(), aggregate).expect("activation");
        let mut writer = daemon.runtime.claim_execution(execution_id).unwrap();
        writer
            .create_execution_request(
                program_hash,
                Some(JsonBytes::try_new(b"null".to_vec()).expect("params")),
                ExecutionAdmission::explicit(negotiation_id, vec![peer, other]).expect("admission"),
                1,
            )
            .await
            .expect("request");
        writer
            .prepare_activation(prepared, 2)
            .await
            .expect("prepare");
        let request = store
            .handle()
            .load_execution_request(execution_id)
            .await
            .expect("load prepared request")
            .expect("prepared request exists");
        let prepared_record = store
            .handle()
            .load_activation(execution_id)
            .await
            .expect("load prepared activation")
            .expect("prepared activation exists");
        let prepared_status = project_exec_status_facts(
            peer,
            request.clone(),
            Some(prepared_record),
            None,
            None,
            false,
        )
        .expect("project prepared status");
        assert!(matches!(
            prepared_status.state,
            ExecStatusState::Activating { session_id: None }
        ));
        writer
            .commit_activation(activation.clone(), 3)
            .await
            .expect("commit");
        let committed_record = store
            .handle()
            .load_activation(execution_id)
            .await
            .expect("load committed activation")
            .expect("committed activation exists");
        let activation_inspection = project_activation_inspection(committed_record.clone());
        assert_eq!(
            activation_inspection.state,
            ActivationInspectionState::Committed
        );
        assert_eq!(
            activation_inspection.negotiation_id, negotiation_id,
            "inspection uses the offer's negotiation identity"
        );
        assert_eq!(
            activation_inspection.session_id,
            Some(activation.session_hash())
        );
        assert_eq!(activation_inspection.creator, peer);
        assert_eq!(activation_inspection.target_size, 2);
        assert_eq!(activation_inspection.participants.len(), 2);
        assert_eq!(
            activation_inspection
                .participants
                .iter()
                .map(|participant| participant.peer_id)
                .collect::<Vec<_>>(),
            vec![peer, other]
        );
        let committed_status = project_exec_status_facts(
            peer,
            request,
            Some(committed_record.clone()),
            None,
            None,
            false,
        )
        .expect("project committed status");
        assert!(matches!(
            committed_status.state,
            ExecStatusState::Activating {
                session_id: Some(id)
            } if id == activation.session_hash()
        ));

        let failed_execution_id = ExecId([0xA6; 32]);
        let mut failed_writer = daemon
            .runtime
            .claim_execution(failed_execution_id)
            .expect("claim failed request");
        failed_writer
            .create_execution_request(
                program_hash,
                Some(JsonBytes::try_new(b"null".to_vec()).expect("params")),
                ExecutionAdmission::explicit(negotiation_id, vec![peer, other]).expect("admission"),
                4,
            )
            .await
            .expect("failed request");
        failed_writer
            .record_execution_request_failure("recovery failed after commit")
            .await
            .expect("record failure");
        let failed_request = store
            .handle()
            .load_execution_request(failed_execution_id)
            .await
            .expect("load failed request")
            .expect("failed request exists");
        let failed_status = project_exec_status_facts(
            peer,
            failed_request,
            Some(committed_record),
            None,
            None,
            false,
        )
        .expect("project post-commit failure");
        assert!(matches!(
            failed_status.state,
            ExecStatusState::Failed {
                session: Some(SessionProgress::Activated { session_id })
            } if session_id == activation.session_hash()
        ));
        let state = ExecutionState::new(
            execution_id,
            activation.clone(),
            peer,
            SharedStateBytes::try_new(vec![0]).expect("shared state"),
            LocalStateBytes::try_new(Vec::new()).expect("local state"),
        )
        .expect("execution state");
        writer
            .create_execution(
                activation,
                peer,
                state.shared_state().clone(),
                state.local_state().clone(),
                4,
            )
            .await
            .expect("execution");
        writer
            .apply_input(ExecutionInput::Activate, 5)
            .await
            .expect("activate");
        drop(writer);

        daemon.resume_durable().await.expect("recovery");

        let recovered = store
            .handle()
            .load_execution(execution_id)
            .await
            .expect("load execution")
            .expect("execution exists");
        assert_eq!(recovered.lifecycle(), ExecLifecycle::Failed);
        assert!(
            recovered
                .status()
                .terminal_cause()
                .is_some_and(|cause| cause.kind() == AbortKind::Fail)
        );
        assert!(daemon.execs.get(&execution_id).is_none());
        daemon.stop().await;
    }
}
