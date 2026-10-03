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
    EventRecordSummary as ApiEventRecordSummary, ExecLifecycle, ExecStatus, ExecStatusState,
    ExecutionInspection, HostInfo, NextEvent, PendingCalloutStatus, ProgramRefError, ReceiptRef,
    Response, ResponseOk, SessionProgress, SessionStatus, frame,
};
use arena0_api::{FileSource, HostRequest, HostStatus};
use arena0_crypto::{AgentPubKey, ExecutionKey, NodeKeys};
use arena0_node::{ActivatedSession, NegotiationBook};
use arena0_node::{
    HostExecutionStore, NegotiationAttempt, NegotiationEffects, NegotiationStart,
    NegotiationSupervision, store_activation_effects, unix_time_ms,
};
use arena0_program::{
    ABI_VERSION, JsonBytes, JsonSchemaDocument, ParticipantCount, ProgramHash, ProgramSchema,
};
use arena0_protocol::{
    ActivationAnnouncement, CalloutId, EventSource, ExecCreationOrigin, ExecId, ExecutionAdmission,
    ExecutionEvent, ExecutionFailureCode, FetchFrame, MAX_CLOCK_SKEW_MS, MAX_TICKET_LIFETIME_MS,
    NegotiationEvent, NegotiationFact, NegotiationGossip, NegotiationId, NegotiationTarget, Offer,
    OfferData, OfferHash, PREPARE_WINDOW_MS, PeerId, ReceiptArtifact, SessionHash, StateHash,
    TerminalKind, Ticket, TicketAction, TicketData, TicketHash, Viewport,
    system_event::SystemEvent,
};
use arena0_sandbox::{LoadedProgram, Program, WasmtimeEngine};
use arena0_transport::{NegotiationTopic, ProgramTopicEvent, Transport};
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
    project_lifecycle, satisfies,
};
use crate::offers::{OFFER_SWEEP, OfferBook};
use crate::schema;
use crate::startup::{StartupStage, StartupTimeline};
use crate::store::Keystore;
use arena0_api::{OfferClosedReason, OpenOffer};
use arena0_store::{
    ActivationRecord, ActivationRecordStatus, AdmissionBindingOutcome,
    EventRecordSummary as StoreEventRecordSummary, ExecutionRequest,
    ExecutionRequestFailureOutcome, MAX_EVENT_INSPECTION_RECORDS, RecoveryCandidate,
    RecoveryCursor, StoreHandle,
};

/// Capacity of the event broadcast bus. A slow subscriber that falls this far
/// behind gets a drop-oldest `stream.lagged` frame rather than blocking producers.
const EVENT_BUS_CAP: usize = 1024;
/// Capacity of the daemon-wide MCP activity bus. Slow monitors receive one
/// bounded lag marker and never hold up tool dispatch.
const ACTIVITY_BUS_CAP: usize = 1024;
const LOCAL_WITHDRAWAL_REASON: &str = "negotiation withdrawn locally";
const CREATION_ARRIVAL_GRACE: Duration = Duration::from_secs(1);
/// Bound independently arriving offer and creator-ticket facts per subscriber.
const PENDING_FACTS: usize = 128;

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
    },
    Join {
        target: Option<NegotiationTarget>,
    },
}

impl NegotiationPlan {
    fn from_admission(admission: &ExecutionAdmission) -> Self {
        match admission {
            ExecutionAdmission::Create {
                negotiation_id,
                participant_count,
            } => Self::Create {
                negotiation_id: *negotiation_id,
                target_size: *participant_count,
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
    /// Whether the actor's observations replay a session this Host already
    /// reported (an end wake), so the supervisor must not project them again
    /// and the ended session gets no relay.
    replay: bool,
}

/// Why a durable execution is being resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumeCause {
    /// Startup recovery reports each resumed execution to a new observer.
    Startup,
    /// A peer poked an ended session. The actor only finishes end
    /// confirmation; its creation and terminal were already reported.
    EndWake,
}

/// One validated negotiation or execution occurrence. `Events` owns every
/// projection, so producers construct this value once.
#[derive(Debug, Clone)]
pub(crate) enum HostEvent {
    SessionEndProgress {
        source: EventSource,
        end: arena0_api::ExecEndStatus,
    },
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
    OfferClosed {
        program_id: ProgramHash,
        negotiation_id: NegotiationId,
        creator: PeerId,
        reason: OfferClosedReason,
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
        signers: u16,
        participants: u16,
    },
    SessionCallout {
        source: EventSource,
        pending_id: CalloutId,
        callout_index: u32,
        name: String,
        prompt: String,
        schema: JsonSchemaDocument,
        context: serde_json::Value,
    },
    SessionCalloutAnswered {
        source: EventSource,
        pending_id: CalloutId,
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
            | Self::SessionEndProgress { source, .. }
            | Self::Created { source, .. }
            | Self::Failed { source, .. }
            | Self::SessionStarted { source, .. }
            | Self::SessionStep { source, .. }
            | Self::SessionCallout { source, .. }
            | Self::SessionCalloutAnswered { source, .. }
            | Self::SessionCompleted { source, .. }
            | Self::SessionAborted { source, .. } => Some(source),
            Self::HostStopped { .. } | Self::OfferSeen { .. } | Self::OfferClosed { .. } => None,
        }
    }

    fn system_event(&self) -> Option<SystemEvent> {
        match self {
            Self::SessionEndProgress { .. } => None,
            Self::HostStopped { .. } | Self::OfferSeen { .. } | Self::OfferClosed { .. } => None,
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
            // The durable trace no longer carries fuel telemetry. Keep the
            // semantic step on the API event stream, but do not fabricate an
            // operational value for the process-local system projection.
            Self::SessionStep { .. } => None,
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
            Self::OfferClosed {
                program_id,
                negotiation_id,
                creator,
                reason,
            } => EventData::OfferClosed {
                program_id: *program_id,
                negotiation_id: *negotiation_id,
                creator: *creator,
                reason: *reason,
            },
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
                origin: (*origin).into(),
            },
            Self::Failed {
                reason, failure, ..
            } => EventData::Terminated {
                reason: reason.clone(),
                failed_class: Some((*failure).into()),
            },
            Self::SessionStarted { ensemble, .. } => EventData::SessionStarted {
                ensemble: ensemble.clone(),
            },
            Self::SessionStep {
                step,
                pre_state,
                post_state,
                signers,
                participants,
                ..
            } => EventData::SessionStep {
                step: *step,
                pre_state: *pre_state,
                post_state: *post_state,
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
            Self::SessionEndProgress { end, .. } => EventData::SessionEndProgress {
                phase: end.phase,
                unconfirmed: end.unconfirmed.clone(),
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
                    failed_class: Some((*failure).into()),
                },
            },
        }
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
    /// `boot_id` is the Host store's lifetime id (`StoreHandle::boot_id`), so
    /// event frames and `/sync` cursors name the same Host lifetime.
    pub(crate) fn new(host: HostInfo, boot_id: String) -> Self {
        let (events, _keepalive) = broadcast::channel(EVENT_BUS_CAP);
        Self {
            host: Arc::new(RwLock::new(host)),
            boot_id,
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
            stage: (*stage).into(),
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
            stage: (*stage).into(),
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
        .initialize(params)
        .map_err(|error| anyhow::anyhow!("{context} initialize: {error}"))?;
    Ok((loaded, StateHash::of_shared(&initialized.shared)))
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

/// The program's turn at one agreed step, as the status projection reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Turn {
    turn: Option<PeerId>,
    phase: Option<String>,
}

/// Activity frames with a synthesized lag marker before the next frame after
/// loss. The receiver and activity owner live for the subscription.
pub(crate) fn activity_stream(
    activity: Arc<Activity>,
    rx: broadcast::Receiver<ActivityFrame>,
) -> impl futures::Stream<Item = ActivityFrame> + Send + 'static {
    futures::stream::unfold(
        (activity, rx, None),
        |(activity, mut rx, pending)| async move {
            if let Some(frame) = pending {
                return Some((frame, (activity, rx, None)));
            }
            let mut skipped = 0u64;
            loop {
                match rx.recv().await {
                    Ok(frame) => {
                        if skipped > 0 {
                            let lagged = activity.lagged(frame.seq.saturating_sub(1), skipped);
                            return Some((lagged, (activity, rx, Some(frame))));
                        }
                        return Some((frame, (activity, rx, None)));
                    }
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        skipped = skipped.saturating_add(count)
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    )
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
    pub(crate) store: StoreHandle,
    engine: Arc<WasmtimeEngine>,
    startup: Arc<StartupTimeline>,
    pub(crate) events: Events,
    offers: Arc<OfferBook>,
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
    /// Each execution's latest turn, read from its view, with the agreed step
    /// it was computed at. A turn is a pure function of the agreed shared
    /// state, so it is reused until the step changes; concurrent misses may
    /// both compute it and the identical results overwrite each other.
    turns: StdMutex<HashMap<ExecId, (u64, Turn)>>,
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
        let identity = init.keystore.node_keys();
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
        let peer_id = runtime.peer_id();
        let execs = Arc::new(ExecutionHandles::new(store.clone()));
        let events = Events::new(
            HostInfo {
                id: name.clone(),
                peer_id,
                user_agent: None,
            },
            store.boot_id().to_owned(),
        );
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
            offers: OfferBook::new(),
            started: StdInstant::now(),
            runtime,
            owns_runtime,
            execs,
            creation_states: StdMutex::new(CreationStates::default()),
            turns: StdMutex::new(HashMap::new()),
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
        if let Some(mut wakes) = self.runtime.take_end_wakes() {
            let service = Arc::downgrade(self);
            self.tasks.lock().await.spawn(async move {
                while let Some(execution_id) = wakes.recv().await {
                    let Some(service) = service.upgrade() else {
                        return;
                    };
                    let result = async {
                        if let Some(candidate) =
                            service.store.end_wake_candidate(execution_id).await?
                        {
                            service
                                .resume_candidate(candidate, ResumeCause::EndWake)
                                .await?;
                        }
                        Ok::<(), anyhow::Error>(())
                    }
                    .await;
                    if let Err(error) = result {
                        tracing::error!(%execution_id, %error, "unable to resume end handshake");
                    }
                }
            });
        }
        if let Err(error) = self.resume_durable().await {
            self.startup.host_progress(StartupStage::Failed, &self.name);
            return Err(error);
        }
        // Discovery needs every registered hash, not parsed guest metadata or
        // the UI's bounded catalog projection. The store still enforces its
        // response-byte budget; exceeding it fails startup instead of silently
        // leaving some programs unwatched. SQLite requires a signed limit.
        for program_id in self.store.list_programs(isize::MAX as usize).await? {
            self.watch_offers(program_id).await;
        }
        let offers = Arc::clone(&self.offers);
        let events = self.events.clone();
        self.tasks.lock().await.spawn(async move {
            let mut sweep = tokio::time::interval(OFFER_SWEEP);
            loop {
                sweep.tick().await;
                offers.expire(unix_time_ms(), &events);
            }
        });
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
                self.resume_candidate(candidate, ResumeCause::Startup)
                    .await?;
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
        cause: ResumeCause,
    ) -> anyhow::Result<()> {
        let request = candidate.request().clone();
        let exec_id = request.execution_id();
        if self.execs.get(&exec_id).is_some() {
            return Ok(());
        }
        if cause == ResumeCause::Startup {
            self.emit_created(
                exec_id,
                request.program_hash(),
                request.negotiation_id(),
                None,
                ExecCreationOrigin::Recovery,
            );
        }

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
        // Store open validated every aggregate, and the actor loads it at
        // startup; recovery needs only its presence.
        let execution_present = match self.store.execution_exists(exec_id).await {
            Ok(present) => present,
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
        // The actor's startup check (`ensure_execution`) compares the
        // aggregate's binding and producer with the request, the committed
        // activation, and this Host, and fails the execution on a mismatch.

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
                            replay: cause == ResumeCause::EndWake,
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

    /// Announce an execution this Host created, from a request or recovery.
    /// Creation precedes any negotiation or session, so the event names the
    /// bare execution source.
    fn emit_created(
        &self,
        exec_id: ExecId,
        program_id: ProgramHash,
        negotiation_id: Option<NegotiationId>,
        queue_position: Option<usize>,
        origin: ExecCreationOrigin,
    ) {
        self.events.emit(HostEvent::Created {
            source: EventSource::Execution {
                peer_id: self.peer_id,
                exec_id,
                program_id,
            },
            negotiation_id,
            queue_position,
            origin,
        });
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

    /// Matching events, with a lag marker before the next received frame after
    /// loss. The owned receiver ends when the Host event channel closes.
    pub(crate) fn event_stream(
        self: &Arc<Self>,
        filter: EventFilter,
        rx: broadcast::Receiver<EventFrame>,
    ) -> impl futures::Stream<Item = EventFrame> + Send + 'static {
        let service = Arc::clone(self);
        futures::stream::unfold(
            (service, filter, rx, None),
            |(service, filter, mut rx, pending)| async move {
                if let Some(frame) = pending {
                    return Some((frame, (service, filter, rx, None)));
                }
                let mut skipped = 0u64;
                loop {
                    match rx.recv().await {
                        Ok(frame) => {
                            if skipped > 0 {
                                let lagged =
                                    service.events.lagged(frame.seq.saturating_sub(1), skipped);
                                let pending = filter.matches(&frame).then_some(frame);
                                return Some((lagged, (service, filter, rx, pending)));
                            }
                            if filter.matches(&frame) {
                                return Some((frame, (service, filter, rx, None)));
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(count)) => {
                            skipped = skipped.saturating_add(count)
                        }
                        Err(broadcast::error::RecvError::Closed) => return None,
                    }
                }
            },
        )
    }

    /// Stream matching events until EOF, retaining the socket's disconnect semantics.
    pub(crate) async fn stream_events_unix<R, W>(
        self: &Arc<Self>,
        filter: EventFilter,
        rx: broadcast::Receiver<EventFrame>,
        read: &mut R,
        write: &mut W,
    ) -> anyhow::Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        use futures::StreamExt as _;
        let stream = self.event_stream(filter, rx);
        futures::pin_mut!(stream);
        let mut sink = [0u8; 256];
        loop {
            tokio::select! {
                message = stream.next() => match message {
                    Some(message) => frame::write_frame(write, &message).await?,
                    None => return Ok(()),
                },
                n = read.read(&mut sink) => {
                    if matches!(n, Ok(0) | Err(_)) { return Ok(()); }
                }
            }
        }
    }

    /// Stream daemon-wide activity until the client hangs up.
    pub(crate) async fn stream_activity_unix<R, W>(
        activity: &Arc<Activity>,
        rx: broadcast::Receiver<ActivityFrame>,
        read: &mut R,
        write: &mut W,
    ) -> anyhow::Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        use futures::StreamExt as _;
        let stream = activity_stream(Arc::clone(activity), rx);
        futures::pin_mut!(stream);
        let mut sink = [0u8; 256];
        loop {
            tokio::select! {
                message = stream.next() => match message {
                    Some(message) => frame::write_frame(write, &message).await?,
                    None => return Ok(()),
                },
                n = read.read(&mut sink) => {
                    if matches!(n, Ok(0) | Err(_)) { return Ok(()); }
                }
            }
        }
    }

    /// Dispatch one non-streaming request to its handler. The single method table
    /// both transports share.
    pub(crate) async fn dispatch(self: &Arc<Self>, req: HostRequest) -> Response {
        let span = tracing::debug_span!("host_request", host = %self.name, method = req.method());
        async {
            match req {
                HostRequest::Info => self.host_status().await.map(ResponseOk::HostStatus),
                HostRequest::IdShow => Ok(ResponseOk::Id(self.keystore.info())),
                HostRequest::NegotiationOffers => Ok(ResponseOk::Offers(
                    self.offers.list(unix_time_ms(), &self.events),
                )),

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
                HostRequest::ProgramImport {
                    source: FileSource::Upload(_),
                }
                | HostRequest::BlobImport {
                    source: FileSource::Upload(_),
                } => {
                    unreachable!(
                        "Daemon::handle resolves process-owned uploads before Host dispatch"
                    )
                }
                HostRequest::ProgramImport {
                    source: FileSource::Path(path),
                } => {
                    let wasm = tokio::fs::read(path).await.map_err(|error| {
                        ApiError::new(ApiErrorCode::BadRequest, format!("read program: {error}"))
                    })?;
                    let (id, _) = self
                        .catalog
                        .import(wasm, &self.engine, unix_time_ms())
                        .await
                        .map_err(catalog_api_error)?;
                    self.watch_offers(id).await;
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
                HostRequest::BlobImport {
                    source: FileSource::Path(path),
                } => {
                    let (hash, length) = self.store.link_blob(path).await.map_err(|error| {
                        let code = match error {
                            arena0_store::StoreError::BlobTooLarge { .. }
                            | arena0_store::StoreError::Io(_) => ApiErrorCode::BadRequest,
                            _ => ApiErrorCode::Storage,
                        };
                        ApiError::new(code, format!("import blob: {error}"))
                    })?;
                    Ok(ResponseOk::BlobImported { hash, length })
                }
                HostRequest::BlobExport { hash, path } => self
                    .store
                    .export_blob(hash, path)
                    .await
                    .map_err(|error| {
                        let code = match error {
                            arena0_store::StoreError::Io(_)
                            | arena0_store::StoreError::BlobUnreadable(_) => {
                                ApiErrorCode::BadRequest
                            }
                            _ => ApiErrorCode::Storage,
                        };
                        ApiError::new(code, format!("export blob: {error}"))
                    })?
                    .map(|length| ResponseOk::BlobExported { length })
                    .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "no such blob")),
                HostRequest::BlobList => self
                    .store
                    .list_blobs()
                    .await
                    .map(|blobs| {
                        ResponseOk::BlobList(blobs.into_iter().map(sync_blob_entry).collect())
                    })
                    .map_err(|error| {
                        ApiError::new(ApiErrorCode::Storage, format!("list blobs: {error}"))
                    }),
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
                        self.unwatch_offers(program_id);
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
                    blobs,
                } => {
                    self.new_exec(exec_id, program, params, ensemble, blobs)
                        .await
                }
                HostRequest::ExecList => self.exec_list().await.map(ResponseOk::ExecList),
                HostRequest::ExecStatus { exec_id } => {
                    self.exec_status(exec_id).await.map(ResponseOk::Status)
                }
                HostRequest::ExecInspect {
                    exec_id,
                    events_from,
                    events_limit,
                } => self
                    .exec_inspect(exec_id, events_from, events_limit)
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
                            .map_err(|error| {
                                ApiError::new(ApiErrorCode::Storage, error.to_string())
                            })?
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
                HostRequest::ExecView {
                    exec,
                    width,
                    color,
                    at_step,
                } => self.view(exec, width, color, at_step).await,
                HostRequest::ExecTrace { exec_id, from, to } => {
                    self.trace(exec_id, from, to).await.map(ResponseOk::Trace)
                }
                HostRequest::ExecRecords {
                    exec_id,
                    from,
                    limit,
                } => self
                    .exec_records(exec_id, from, limit)
                    .await
                    .map(ResponseOk::Records),
                HostRequest::Resolve { kind, reference } => self
                    .resolve(kind, &reference)
                    .await
                    .map(ResponseOk::Resolved),
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
                        .list_receipt_summaries(4_096)
                        .await
                        .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
                    Ok(ResponseOk::ReceiptList(
                        receipts.into_iter().map(sync_receipt_entry).collect(),
                    ))
                }
                HostRequest::ReceiptVerify { receipt } => self.verify(receipt).await,
            }
        }
        .instrument(span)
        .await
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

    async fn exec_list(&self) -> Result<Vec<arena0_api::ExecSummary>, ApiError> {
        let rows = self
            .store
            .list_exec_summaries(4_096)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
        let mut statuses = Vec::with_capacity(rows.len());
        for row in rows {
            statuses.push(self.project_exec_summary(row).await?);
        }
        Ok(statuses)
    }

    /// Build one `exec.list` entry from its store summary row (see
    /// `arena0_api::ExecSummary` for the field-by-field equivalence with
    /// `exec.status`). Decodes no receipt and no activation record. The only
    /// non-column inputs: the callout `name` from the program schema
    /// (`self.catalog.schema`, cached by hash), and `turn`/`phase` from
    /// `self.turns` for non-terminal executions. A turn-memo miss loads that
    /// execution's state once (as `project_turn` does) and memoizes it, so
    /// only the first list after a daemon start decodes, and only
    /// non-terminal executions; terminal executions never decode.
    async fn project_exec_summary(
        &self,
        row: arena0_store::ExecSummaryRow,
    ) -> Result<arena0_api::ExecSummary, ApiError> {
        let execution = row.execution.as_ref();
        let lifecycle = project_lifecycle(
            row.request_failure.is_some(),
            row.activation.is_some(),
            execution.map(|execution| execution.lifecycle),
        );
        let session_execution = execution.filter(|_| lifecycle_has_session(lifecycle));
        let activation = row
            .activation
            .as_ref()
            .map(activation_index_inspection)
            .transpose()?;
        let committed_session = row
            .activation
            .as_ref()
            .and_then(|activation| activation.committed.then_some(activation.session_id));
        let session_id = match lifecycle {
            ExecLifecycle::Negotiating => None,
            _ => execution
                .map(|execution| execution.session_id)
                .or(committed_session),
        };
        let turn = if let Some(execution) =
            session_execution.filter(|execution| !execution.lifecycle.is_terminal())
        {
            // Drop the memo lock before an async store or sandbox operation.
            // A memo is usable only for the step captured by this index row.
            let memo = self
                .turns
                .lock()
                .expect("turn memo")
                .get(&row.execution_id)
                .filter(|(step, _)| *step == execution.agreed_step)
                .map(|(_, turn)| turn.clone());
            match memo {
                Some(turn) => turn,
                None => {
                    let state = self
                        .store
                        .load_execution(row.execution_id)
                        .await
                        .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
                        .ok_or_else(|| {
                            ApiError::new(ApiErrorCode::Storage, "execution summary has no state")
                        })?;
                    self.project_turn(row.execution_id, &state).await?
                }
            }
        } else {
            Turn {
                turn: None,
                phase: None,
            }
        };
        let pending_callout =
            if let Some(callout) = session_execution.and_then(|execution| execution.callout) {
                let schema = self
                    .catalog
                    .schema(row.program_hash)
                    .await
                    .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
                    .ok_or_else(|| {
                        ApiError::new(ApiErrorCode::Storage, "execution program schema is missing")
                    })?;
                let index = usize::try_from(callout.callout_index)
                    .map_err(|_| ApiError::new(ApiErrorCode::Storage, "callout index overflow"))?;
                let declaration = schema.callouts.get(index).ok_or_else(|| {
                    ApiError::new(ApiErrorCode::Storage, "callout index out of range")
                })?;
                Some(arena0_api::CalloutSummary {
                    pending_id: callout.id,
                    callout_index: callout.callout_index,
                    name: declaration.name.clone(),
                    opened_at_ms: callout.opened_at_ms,
                })
            } else {
                None
            };
        let outcome = execution
            .and_then(|execution| execution.outcome_json.as_deref())
            .map(serde_json::from_slice)
            .transpose()
            .map_err(|_| {
                ApiError::new(ApiErrorCode::Storage, "execution outcome is invalid JSON")
            })?;
        let reason = match lifecycle {
            ExecLifecycle::Failed => row
                .request_failure
                .clone()
                .or_else(|| execution.and_then(|execution| execution.terminal_reason.clone())),
            ExecLifecycle::Aborted => {
                execution.and_then(|execution| execution.terminal_reason.clone())
            }
            _ => None,
        };
        let peers: Vec<PeerId> = session_execution
            .map(|execution| {
                execution
                    .participant_ids
                    .iter()
                    .copied()
                    .filter(|peer| *peer != self.peer_id)
                    .collect()
            })
            .unwrap_or_default();
        Ok(arena0_api::ExecSummary {
            exec_id: row.execution_id,
            negotiation_id: row.negotiation_id,
            program_id: row.program_hash,
            lifecycle,
            session_id,
            step: session_execution.map(|execution| execution.agreed_step),
            last_step_at_ms: session_execution.and_then(|execution| execution.last_step_at_ms),
            participants: session_execution.map(|execution| execution.participants),
            peers,
            pending_callout,
            receipt_available: session_execution
                .is_some_and(|execution| execution.receipt_produced),
            turn: turn.turn,
            phase: turn.phase,
            end: execution
                .map(|execution| arena0_api::ExecEndStatus::from(&execution.end))
                .unwrap_or_default(),
            reason,
            outcome,
            activation,
            created_at_ms: row.created_at_ms,
            updated_at_ms: execution
                .map(|execution| execution.updated_at_ms)
                .or_else(|| {
                    row.activation
                        .as_ref()
                        .map(|activation| activation.updated_at_ms)
                })
                .unwrap_or(row.created_at_ms),
        })
    }

    async fn exec_status(&self, exec_id: ExecId) -> Result<ExecStatus, ApiError> {
        self.project_exec_status(exec_id).await
    }

    async fn exec_inspect(
        &self,
        exec_id: ExecId,
        events_from: Option<u64>,
        events_limit: u16,
    ) -> Result<ExecutionInspection, ApiError> {
        let events_limit = usize::from(events_limit);
        if events_limit == 0 || events_limit > MAX_EVENT_INSPECTION_RECORDS {
            return Err(ApiError::new(
                ApiErrorCode::BadRequest,
                format!(
                    "event inspection limit must be between 1 and {}",
                    MAX_EVENT_INSPECTION_RECORDS
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
        let page = self
            .record_page(exec_id, events_from, events_limit as u16)
            .await?;
        if page.total == 0 {
            return Ok(empty_execution_inspection(status, activation, page.from));
        }
        Ok(ExecutionInspection {
            status,
            activation,
            events_from: page.from,
            events: page.records,
            events_total: page.total,
            events_next: page.next,
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
        let receipt_available = self
            .store
            .exec_summary(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .and_then(|row| row.execution)
            .is_some_and(|execution| execution.receipt_produced);
        let execution_updated_at_ms = self
            .store
            .execution_updated_at_ms(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
        let lifecycle = project_lifecycle(
            request.failure().is_some(),
            activation.is_some(),
            state
                .as_ref()
                .map(arena0_protocol::execution::ExecutionState::lifecycle),
        );
        let turn = match &state {
            Some(state) if lifecycle_has_session(lifecycle) && !state.lifecycle().is_terminal() => {
                Some(self.project_turn(exec_id, state).await?)
            }
            Some(_) => Some(Turn {
                turn: None,
                phase: None,
            }),
            None => None,
        };
        let callout = if let Some(open) = state
            .as_ref()
            .filter(|_| lifecycle_has_session(lifecycle))
            .and_then(|state| state.callout())
        {
            let schema = self
                .catalog
                .schema(request.program_hash())
                .await
                .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
                .ok_or_else(|| {
                    ApiError::new(ApiErrorCode::Storage, "execution program schema is missing")
                })?;
            Some(
                crate::exec_manager::project_callout(
                    open.id,
                    open.callout_index,
                    &open.context,
                    &schema,
                )
                .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?,
            )
        } else {
            None
        };
        project_exec_status_facts(
            self.peer_id,
            request,
            activation,
            state.zip(turn),
            execution_updated_at_ms,
            receipt_available,
            callout,
        )
        .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))
    }

    /// Read the turn and phase from the program's view at the execution's
    /// agreed step. A failed projection fails the status call rather than
    /// reporting an empty turn.
    async fn project_turn(
        &self,
        exec_id: ExecId,
        state: &arena0_protocol::execution::ExecutionState,
    ) -> Result<Turn, ApiError> {
        let step = state.agreed_step();
        if let Some((memo_step, turn)) = self.turns.lock().expect("turn memo").get(&exec_id)
            && *memo_step == step
        {
            return Ok(turn.clone());
        }
        let program = self
            .catalog
            .load_program(state.binding().program_hash())
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
        let ensemble = state
            .binding()
            .ensemble()
            .map_err(|error| ApiError::new(ApiErrorCode::Execution, error.to_string()))?;
        let shared = state.shared_state().clone();
        let engine = Arc::clone(&self.engine);
        // The turn and phase do not depend on the viewport; any fixed one
        // serves.
        let viewport = JsonBytes::try_new(
            serde_json::to_vec(&Viewport {
                width: 80,
                color: arena0_protocol::ColorDepth::Mono,
            })
            .expect("viewport JSON"),
        )
        .expect("viewport fits");
        let projection_ensemble = ensemble.clone();
        let projection = tokio::task::spawn_blocking(move || {
            engine
                .load(&program)?
                .view(&shared, &projection_ensemble, viewport)
        })
        .await
        .map_err(|error| ApiError::new(ApiErrorCode::Internal, error.to_string()))?
        .map_err(|error| {
            ApiError::new(ApiErrorCode::Execution, format!("view projection: {error}"))
        })?;
        let view = parse_view(&projection, ensemble.len())?;
        let turn = Turn {
            turn: view
                .turn
                .and_then(|index| ensemble.peer_at(arena0_protocol::Participant::new(index))),
            phase: view.phase,
        };
        self.turns
            .lock()
            .expect("turn memo")
            .insert(exec_id, (step, turn.clone()));
        Ok(turn)
    }

    pub(crate) async fn host_status(&self) -> Result<HostStatus, ApiError> {
        let programs = self.store.count_programs().await.map_err(|error| {
            ApiError::new(ApiErrorCode::Storage, format!("list programs: {error}"))
        })?;
        Ok(HostStatus {
            host: self.host_info(),
            transport_key: AgentPubKey(self.peer_id.0),
            programs,
            execs_active: self
                .store
                .count_active_executions()
                .await
                .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?,
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
        if let Some(event) = project_durable_next(state, &schema)? {
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
        blobs: Vec<arena0_protocol::BlobHash>,
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
                .new_exec_inner(exec_id, program, params, ensemble, blobs)
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
        blobs: Vec<arena0_protocol::BlobHash>,
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
                    },
                    ExecutionAdmission::create(negotiation_id, participant_count).map_err(
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
            .create_execution_request(
                program_id,
                request_params,
                admission,
                &blobs,
                unix_time_ms(),
            )
            .await
            .map_err(|error| {
                let code = if matches!(error, arena0_store::StoreError::BlobNotFound(_)) {
                    ApiErrorCode::NotFound
                } else {
                    ApiErrorCode::Storage
                };
                ApiError::new(code, error.to_string())
            })? {
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
        self.emit_created(
            exec_id,
            program_id,
            negotiation_id,
            queue_position,
            ExecCreationOrigin::Request,
        );

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
            ExecLifecycle::Completed | ExecLifecycle::Aborted | ExecLifecycle::Failed => {
                Ok(ResponseOk::Ack)
            }
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
                | ExecLifecycle::Aborted => {
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
            .signers()
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
        let engine = Arc::clone(&self.engine);
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
                    if !authenticated_offer(&offer, program_id, local_peer)
                        || expected_creator.is_some_and(|creator| data.creator != creator)
                        || expected_negotiation
                            .is_some_and(|negotiation_id| data.negotiation_id != negotiation_id)
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
            NegotiationPlan::Create { .. } | NegotiationPlan::Join { target: None } => None,
            NegotiationPlan::Join { target: Some(_) } => Some(Instant::now() + NEGOTIATION_TIMEOUT),
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
                let bootstrap = negotiation_bootstrap(self.peer_id, None, std::iter::empty());
                let topic = self.subscribe_negotiation(program_id, bootstrap).await?;
                (offer, Some(creator_ticket), topic, Vec::new())
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

        let (prepare, persist_commit) = store_activation_effects();
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
                replay: false,
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
            replay,
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
            session: None,
            replay,
        });

        // The post-commit relay: session-lived, re-emits the final offer and
        // the exact tickets on the program topic and serves the convergence
        // fetch from the committed ActivationRecord. The creator is the
        // convergence authority: only it relays. An end wake resumes a
        // session that already ended, so it has nothing to relay.
        if !replay && self.peer_id == committed.activation().offer().data().creator {
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
        pending_id: CalloutId,
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
        pending_id: CalloutId,
    ) -> Result<Option<u32>, ApiError> {
        Ok(self
            .store
            .load_execution(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .and_then(|state| {
                state
                    .callout()
                    .filter(|callout| callout.id == pending_id)
                    .map(|callout| callout.callout_index)
            }))
    }

    async fn reclassify_submit_failure(
        &self,
        exec_id: ExecId,
        pending_id: CalloutId,
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

    /// Render the program view from live or terminal shared state, or, with
    /// `at_step`, from the state after that agreed step. Terminal projection
    /// uses its durable snapshot after the execution actor exits.
    async fn view(
        &self,
        exec_id: ExecId,
        width: u16,
        color: arena0_protocol::ColorDepth,
        at_step: Option<u64>,
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
        let terminal = state.lifecycle().is_terminal();
        if !terminal
            && !matches!(
                state.lifecycle(),
                ExecLifecycle::Active | ExecLifecycle::Waiting
            )
        {
            return Err(ApiError::new(
                ApiErrorCode::Execution,
                format!(
                    "execution is {:?}; view is available only while Active",
                    state.lifecycle()
                ),
            ));
        }
        if let Some(at_step) = at_step {
            return self.view_at_step(exec_id, &state, at_step, viewport).await;
        }
        if terminal {
            let program = self.load_session_program(&state).await?;
            let ensemble = state
                .binding()
                .ensemble()
                .map_err(|error| ApiError::new(ApiErrorCode::Execution, error.to_string()))?;
            let ensemble_len = ensemble.len();
            let engine = Arc::clone(&self.engine);
            let step = state.agreed_step().checked_sub(1);
            let shared = state.shared_state().clone();
            let projection = tokio::task::spawn_blocking(move || {
                engine.load(&program)?.view(&shared, &ensemble, viewport)
            })
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Internal, error.to_string()))?
            .map_err(|error| ApiError::new(ApiErrorCode::Execution, error.to_string()))?;
            return Ok(ResponseOk::ExecView {
                step,
                view: parse_view(&projection, ensemble_len)?,
            });
        }
        let entry = self.execs.get(&exec_id).ok_or_else(|| {
            ApiError::new(ApiErrorCode::Execution, "execution has no live driver")
        })?;

        let (step, view) = entry.view(viewport).await?;
        Ok(ResponseOk::ExecView { step, view })
    }

    /// One page of local event records at positions `from..from + limit`
    /// (bounded as `exec.inspect` bounds `events_limit`), read the way
    /// `exec.inspect` reads its records, with no status or activation
    /// projection: it decodes no execution state. `NotFound` for an unknown
    /// execution; `BadRequest` for a limit of 0 or above the bound.
    async fn exec_records(
        &self,
        exec_id: ExecId,
        from: u64,
        limit: u16,
    ) -> Result<arena0_api::RecordsPage, ApiError> {
        self.record_page(exec_id, Some(from), limit).await
    }

    /// Shared bounded record projection. A known request without active state
    /// has an empty journal; existence and totals require no state or activation
    /// decode. Payload envelopes are read only for records in this page.
    async fn record_page(
        &self,
        exec_id: ExecId,
        from: Option<u64>,
        limit: u16,
    ) -> Result<arena0_api::RecordsPage, ApiError> {
        if limit == 0 || usize::from(limit) > MAX_EVENT_INSPECTION_RECORDS {
            return Err(ApiError::new(
                ApiErrorCode::BadRequest,
                format!(
                    "event inspection limit must be between 1 and {}",
                    MAX_EVENT_INSPECTION_RECORDS
                ),
            ));
        }
        self.store
            .load_execution_request(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "no such execution"))?;
        let page = match self
            .store
            .read_event_summaries(exec_id, from, usize::from(limit))
            .await
        {
            Ok(page) => page,
            Err(arena0_store::StoreError::ExecutionNotFound(_)) => {
                return Ok(arena0_api::RecordsPage {
                    from: from.unwrap_or(0),
                    records: Vec::new(),
                    total: 0,
                    next: None,
                });
            }
            Err(error) => return Err(ApiError::new(ApiErrorCode::Storage, error.to_string())),
        };
        Ok(arena0_api::RecordsPage {
            from: page.from(),
            total: page.total(),
            next: page.next(),
            records: page
                .into_summaries()
                .into_iter()
                .map(project_event_record_summary)
                .collect(),
        })
    }

    /// Resolve `reference` as `kind` on this Host with the matching rule on
    /// `Resolved` (indexed store lookup, `StoreHandle::resolve_ids`). A unique
    /// receipt resolves to its `receipt.list` entry.
    async fn resolve(
        &self,
        kind: arena0_api::RefKind,
        reference: &str,
    ) -> Result<arena0_api::Resolved, ApiError> {
        use arena0_api::{RefKind, Resolved};
        use arena0_store::IdSpace;
        let needle = reference.trim().to_ascii_lowercase();
        if needle.is_empty() || needle.len() > 64 || !needle.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Ok(Resolved::None);
        }
        let space = match kind {
            RefKind::Exec => IdSpace::Exec,
            RefKind::Session => IdSpace::Session,
            RefKind::Receipt => IdSpace::Receipt,
        };
        let matches = self
            .store
            .resolve_ids(space, needle.clone(), 8)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?;
        if matches.total == 0 {
            return Ok(Resolved::None);
        }
        let exact = if needle.len() == 64 {
            matches.ids.iter().find(|id| hex::encode(id) == needle)
        } else {
            None
        };
        let id = match exact.or_else(|| (matches.total == 1).then(|| &matches.ids[0])) {
            Some(id) => *id,
            None => {
                return Ok(Resolved::Ambiguous {
                    candidates: matches.ids.into_iter().map(hex::encode).collect(),
                    matches: matches.total,
                });
            }
        };
        Ok(match kind {
            RefKind::Exec => Resolved::Exec {
                exec_id: ExecId(id),
            },
            RefKind::Session => Resolved::Session {
                session_id: arena0_protocol::SessionHash(id),
            },
            RefKind::Receipt => {
                let row = self
                    .store
                    .receipt_summary(arena0_protocol::ReceiptId(id))
                    .await
                    .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
                    .ok_or_else(|| {
                        ApiError::new(ApiErrorCode::Storage, "resolved receipt summary is missing")
                    })?;
                Resolved::Receipt {
                    entry: arena0_api::ReceiptListEntry {
                        receipt_id: row.receipt_id.to_string(),
                        session_id: row.session_id,
                        kind: row.kind,
                        program_id: row.program_hash,
                        completed: row.completed,
                        provenance: row.provenance,
                    },
                }
            }
        })
    }

    /// Render the shared state after agreed step `at_step` of a session that
    /// has started, live or terminal. Read the state stored atomically with the
    /// agreed step, verifying its envelope and expected hash before rendering.
    async fn view_at_step(
        &self,
        exec_id: ExecId,
        state: &arena0_protocol::execution::ExecutionState,
        at_step: u64,
        viewport: JsonBytes,
    ) -> Response {
        let Some(latest) = state.agreed_step().checked_sub(1) else {
            return Err(ApiError::new(
                ApiErrorCode::Execution,
                "execution has no agreed step yet",
            ));
        };
        if at_step > latest {
            return Err(ApiError::new(
                ApiErrorCode::BadRequest,
                format!("step {at_step} is beyond the latest agreed step {latest}"),
            ));
        }
        let program = async {
            let started = StdInstant::now();
            let result = self.load_session_program(state).await;
            tracing::debug!(target: "arena0::performance", operation = "view.phase", phase = "program", elapsed_us = started.elapsed().as_micros() as u64);
            result
        }
        .instrument(tracing::debug_span!(target: "arena0::performance", "view.program"))
        .await?;
        let ensemble = state
            .binding()
            .ensemble()
            .map_err(|error| ApiError::new(ApiErrorCode::Execution, error.to_string()))?;
        let ensemble_len = ensemble.len();
        let shared = async {
            let started = StdInstant::now();
            let result = self.store
                .step_state(exec_id, at_step)
                .await
                .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()));
            tracing::debug!(target: "arena0::performance", operation = "view.phase", phase = "state", elapsed_us = started.elapsed().as_micros() as u64);
            result
        }.instrument(tracing::debug_span!(target: "arena0::performance", "view.state")).await?;
        let engine = Arc::clone(&self.engine);
        fn execution_error(error: impl std::fmt::Display) -> ApiError {
            ApiError::new(ApiErrorCode::Execution, error.to_string())
        }
        // Keep guest load/render work attributable to this request on the
        // blocking pool, distinct from concurrent actor projections.
        let request_span = tracing::Span::current();
        let projection = tokio::task::spawn_blocking(move || {
            let _request_guard = request_span.enter();
            let program = tracing::debug_span!(target: "arena0::performance", "view.load").in_scope(|| {
                let started = StdInstant::now();
                let result = engine.load(&program).map_err(execution_error);
                tracing::debug!(target: "arena0::performance", operation = "view.phase", phase = "load", elapsed_us = started.elapsed().as_micros() as u64);
                result
            })?;
            let projection = tracing::debug_span!(target: "arena0::performance", "view.render").in_scope(|| {
                let started = StdInstant::now();
                let result = program
                .view(&shared, &ensemble, viewport)
                .map_err(execution_error);
                tracing::debug!(target: "arena0::performance", operation = "view.phase", phase = "render", elapsed_us = started.elapsed().as_micros() as u64);
                result
            })?;
            Ok::<_, ApiError>(projection)
        })
        .await
        .map_err(|error| ApiError::new(ApiErrorCode::Internal, error.to_string()))??;
        Ok(ResponseOk::ExecView {
            step: Some(at_step),
            view: parse_view(&projection, ensemble_len)?,
        })
    }

    /// Load the program a started execution runs, refusing an execution
    /// profile this runtime does not support.
    async fn load_session_program(
        &self,
        state: &arena0_protocol::execution::ExecutionState,
    ) -> Result<Program, ApiError> {
        if state.binding().execution_profile() != arena0_program::ExecutionProfile::current().hash()
        {
            return Err(ApiError::new(
                ApiErrorCode::Execution,
                "execution profile is not supported by this runtime",
            ));
        }
        self.catalog
            .load_program(state.binding().program_hash())
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))
    }

    /// Read agreed steps and their local certification times directly from
    /// the durable runtime journal.
    async fn trace(
        &self,
        exec_id: ExecId,
        from: u64,
        to: u64,
    ) -> Result<Vec<arena0_api::AgreedStep>, ApiError> {
        let state = self
            .store
            .load_execution(exec_id)
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "no such execution"))?;
        let schema = self
            .catalog
            .schema(state.binding().program_hash())
            .await
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))?
            .ok_or_else(|| {
                ApiError::new(ApiErrorCode::Storage, "execution program schema is missing")
            })?;
        self.store
            .read_agreed_steps(exec_id, from, to)
            .await
            .map(|steps| {
                steps
                    .into_iter()
                    .map(|step| arena0_api::AgreedStep {
                        certified_at_ms: step.certified_at_ms,
                        message: match &step.entry.event {
                            arena0_protocol::StepEvent::Message { data, .. } => {
                                Some(match schema.messages.first() {
                                    Some(message) => match message.borsh.decode_json(data) {
                                        Ok(value) => arena0_api::DecodedMessage::Json(value),
                                        Err(error) => arena0_api::DecodedMessage::Undecodable {
                                            error: error.to_string(),
                                        },
                                    },
                                    None => arena0_api::DecodedMessage::Undecodable {
                                        error: "program declares no message schema".to_owned(),
                                    },
                                })
                            }
                            _ => None,
                        },
                        entry: step.entry,
                    })
                    .collect()
            })
            .map_err(|error| ApiError::new(ApiErrorCode::Storage, error.to_string()))
    }

    /// Verify and persist a foreign receipt as an immutable artifact.
    async fn import_receipt(&self, receipt: ReceiptArtifact) -> Response {
        // Deserialization authenticated the artifact.
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

    /// Verify a receipt and return its portable structural evidence.
    async fn verify(&self, receipt: ReceiptRef) -> Response {
        let receipt = self.resolve_receipt(receipt).await?;
        Ok(ResponseOk::Verified(receipt.summary()))
    }
}

/// Decode a guest view projection into the protocol's [`View`](arena0_protocol::View)
/// and check its blocks against the limits for a session of `participants`.
/// The view is guest output, so a violation fails the request.
fn parse_view(
    projection: &arena0_sandbox::GuestProjectionResult,
    participants: usize,
) -> Result<arena0_protocol::View, ApiError> {
    let view = serde_json::from_slice::<arena0_protocol::View>(projection.output.as_bytes())
        .map_err(|error| {
            ApiError::new(ApiErrorCode::Execution, format!("view projection: {error}"))
        })?;
    view.validate(participants).map_err(|error| {
        ApiError::new(ApiErrorCode::Execution, format!("view projection: {error}"))
    })?;
    Ok(view)
}

/// Whether the published lifecycle exposes a started SessionStatus. Failed
/// executions expose one only if an aggregate exists; callers apply this rule
/// to their optional aggregate, preserving activation-only session identities.
fn lifecycle_has_session(lifecycle: ExecLifecycle) -> bool {
    matches!(
        lifecycle,
        ExecLifecycle::Active
            | ExecLifecycle::Completed
            | ExecLifecycle::Aborted
            | ExecLifecycle::Failed
    )
}

fn project_exec_status_facts(
    peer_id: PeerId,
    request: ExecutionRequest,
    activation: Option<ActivationRecord>,
    state: Option<(arena0_protocol::execution::ExecutionState, Turn)>,
    execution_updated_at_ms: Option<u64>,
    receipt_available: bool,
    callout: Option<PendingCalloutStatus>,
) -> anyhow::Result<ExecStatus> {
    let (state, turn) = state.unzip();
    let exec_id = request.execution_id();
    let program_id = request.program_hash();
    let negotiation_id = request.negotiation_id();
    // `turn` is `Some` exactly when `state` is: they arrive as one pair.
    let session_status = |state: &arena0_protocol::execution::ExecutionState| {
        let turn = turn
            .as_ref()
            .expect("an execution aggregate is projected with its turn");
        let binding = state.binding();
        SessionStatus {
            session_id: binding.session_id(),
            step: state.agreed_step(),
            peers: binding
                .participants()
                .filter(|peer| *peer != peer_id)
                .collect(),
            participants: binding.activation().tickets().len(),
            pending_callout: callout.clone(),
            receipt_available,
            turn: turn.turn,
            phase: turn.phase.clone(),
        }
    };

    let end = state
        .as_ref()
        .map(|state| arena0_api::ExecEndStatus::from(state.end_phase()))
        .unwrap_or_default();
    // A prepared activation fixes a candidate hash but is not yet a formed
    // session. Expose the SessionHash only after commit.
    let committed_session = activation
        .as_ref()
        .and_then(|record| record.is_committed().then(|| record.session_id()));
    let lifecycle = project_lifecycle(
        request.failure().is_some(),
        activation.is_some(),
        state
            .as_ref()
            .map(arena0_protocol::execution::ExecutionState::lifecycle),
    );
    let session = state
        .as_ref()
        .filter(|_| lifecycle_has_session(lifecycle))
        .map(session_status);
    let state = match (lifecycle, state) {
        (ExecLifecycle::Waiting, _) => {
            unreachable!("project_lifecycle publishes Waiting as Active")
        }
        (ExecLifecycle::Negotiating, _) => ExecStatusState::Negotiating {
            queue_position: None,
        },
        (ExecLifecycle::Activating, state) => ExecStatusState::Activating {
            session_id: state
                .map(|state| state.binding().session_id())
                .or(committed_session),
        },
        (ExecLifecycle::Active, Some(_)) => ExecStatusState::Active {
            session: session.expect("active aggregate has a session"),
        },
        (ExecLifecycle::Completed, Some(state)) => ExecStatusState::Completed {
            session: session.expect("completed aggregate has a session"),
            outcome: state
                .terminal_outcome_json()
                .map(serde_json::from_slice)
                .transpose()?,
        },
        (ExecLifecycle::Aborted, Some(state)) => ExecStatusState::Aborted {
            session: session.expect("aborted aggregate has a session"),
            reason: state
                .status()
                .terminal_cause()
                .context("aborted execution has no terminal cause")?
                .reason()
                .to_owned(),
        },
        (ExecLifecycle::Failed, Some(state)) => ExecStatusState::Failed {
            reason: request.failure().map(str::to_owned).or_else(|| {
                state
                    .status()
                    .terminal_cause()
                    .map(|cause| cause.reason().to_owned())
            }),
            session: Some(SessionProgress::Started {
                session: session.expect("failed aggregate has a session"),
            }),
        },
        (ExecLifecycle::Failed, None) => ExecStatusState::Failed {
            reason: request.failure().map(str::to_owned),
            session: committed_session.map(|session_id| SessionProgress::Activated { session_id }),
        },
        (lifecycle, None) => {
            anyhow::bail!("{lifecycle:?} execution {exec_id} has no execution aggregate")
        }
    };
    // The latest durable transition is the most advanced record that exists:
    // the execution row, else the activation record, else the request itself.
    let updated_at_ms = execution_updated_at_ms
        .or_else(|| activation.as_ref().map(ActivationRecord::updated_at_ms))
        .unwrap_or_else(|| request.created_at_ms());
    Ok(ExecStatus {
        end,
        exec_id,
        negotiation_id,
        program_id,
        state,
        created_at_ms: request.created_at_ms(),
        updated_at_ms,
    })
}

fn empty_execution_inspection(
    status: ExecStatus,
    activation: Option<ActivationInspection>,
    events_from: u64,
) -> ExecutionInspection {
    ExecutionInspection {
        status,
        activation,
        events_from,
        events: Vec::new(),
        events_total: 0,
        events_next: None,
    }
}

/// The activation projection mapped from `ActivationIndex::facts`; decodes nothing.
fn activation_index_inspection(
    index: &arena0_store::ActivationIndex,
) -> Result<ActivationInspection, ApiError> {
    let facts = &index.facts;
    Ok(ActivationInspection {
        state: if index.committed {
            ActivationInspectionState::Committed
        } else {
            ActivationInspectionState::Prepared
        },
        negotiation_id: facts.negotiation_id,
        session_id: index.committed.then_some(index.session_id),
        offer_hash: facts.offer_hash,
        creator: facts.creator,
        target_size: facts.target_size,
        initial_state: facts.initial_state,
        participants: facts
            .participants
            .iter()
            .map(|&(peer_id, ticket_hash)| ActivationParticipant {
                peer_id,
                ticket_hash,
            })
            .collect(),
        params: serde_json::from_slice(&facts.params).map_err(|_| {
            ApiError::new(ApiErrorCode::Storage, "activation params are invalid JSON")
        })?,
    })
}

fn project_activation_inspection(record: ActivationRecord) -> ActivationInspection {
    let prepared = record.prepared();
    let offer = prepared.offer();
    let data = offer.data();
    let params = serde_json::from_slice(data.params.as_bytes())
        .expect("a loaded offer's params are validated JSON");
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
        params,
    }
}

fn project_event_record_summary(summary: StoreEventRecordSummary) -> ApiEventRecordSummary {
    ApiEventRecordSummary {
        event_position: summary.event_position,
        agreed_steps: summary.agreed_steps,
        event: summary.event,
        input_payload_bytes: summary.input_payload_bytes,
        effects: summary.effects,
    }
}

fn receipt_list_entry(stored: arena0_store::StoredReceipt) -> arena0_api::ReceiptListEntry {
    let provenance = stored.provenance();
    arena0_api::ReceiptListEntry {
        receipt_id: hex::encode(stored.receipt_id.as_bytes()),
        session_id: stored.receipt.body().header().session_hash(),
        kind: stored.receipt.kind(),
        program_id: stored.receipt.body().header().program_hash(),
        completed: matches!(
            stored.receipt.body().termination(),
            arena0_protocol::ReceiptTermination::Completed
        ),
        provenance,
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
    /// Start watching this program unless it already has a discovery task.
    pub(crate) async fn watch_offers(self: &Arc<Self>, program_id: ProgramHash) {
        let mut tasks = self.tasks.lock().await;
        let mut watchers = self.offers.watchers.lock().expect("offer watchers");
        if watchers.contains_key(&program_id) {
            return;
        }
        let service = Arc::clone(self);
        let handle = tasks.spawn(async move {
            let result: Result<(), ApiError> = async {
                let bootstrap = negotiation_bootstrap(service.peer_id, None, std::iter::empty());
                let mut topic = service
                    .subscribe_negotiation(program_id, bootstrap.clone())
                    .await?;
                let mut pending_offers = HashMap::<(PeerId, NegotiationId, u64), Offer>::new();
                let mut pending_tickets = HashMap::<(PeerId, NegotiationId, u64), Ticket>::new();
                // Active tickets seen on this topic, bounded by PENDING_FACTS.
                // Complete offers may arrive before or after their ticket facts.
                let mut seen_tickets = HashMap::<TicketHash, Ticket>::new();
                let mut pending_complete = HashMap::<(PeerId, NegotiationId), Offer>::new();
                let mut retry_delays = Exponential::from_millis(100)
                    .map(|delay| jitter(delay.min(Duration::from_secs(2))));
                let mut retry_at =
                    Instant::now() + retry_delays.next().expect("infinite retry iterator");
                loop {
                    let event = match timeout_at(retry_at, topic.recv()).await {
                        Ok(event) => event.map_err(|error| {
                            ApiError::new(ApiErrorCode::Negotiation, error.to_string())
                        })?,
                        Err(_) => {
                            let _ = topic.join_peers(bootstrap.clone()).await;
                            retry_at = Instant::now()
                                + retry_delays.next().expect("infinite retry iterator");
                            continue;
                        }
                    };
                    let ProgramTopicEvent::Fact(fact) = event else {
                        match event {
                            ProgramTopicEvent::Joined
                            | ProgramTopicEvent::NeighborUp(_)
                            | ProgramTopicEvent::NeighborDown(_) => continue,
                            ProgramTopicEvent::Lagged | ProgramTopicEvent::Closed => {
                                let deadline = Instant::now() + Duration::from_secs(5);
                                let _ = timeout_at(deadline, topic.close()).await;
                                topic = timeout_at(
                                    deadline,
                                    service.subscribe_negotiation(program_id, bootstrap.clone()),
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
                    // Abort is cooperative. Hold the registry lock through each
                    // synchronous mutation so unwatch cannot clear the entries
                    // and then race with a final insertion from an aborted task.
                    let watchers = service.offers.watchers.lock().expect("offer watchers");
                    if !watchers.contains_key(&program_id) {
                        return Ok(());
                    }
                    let pair = match frame.fact {
                        NegotiationFact::Offer(offer) => {
                            let data = offer.data();
                            if offer.is_complete()
                                && offer.validate().is_ok()
                                && data.program_hash == program_id
                            {
                                if data.creator == service.peer_id {
                                    continue;
                                }
                                let key = (data.creator, data.negotiation_id);
                                if complete_offer_is_proven(&offer, &seen_tickets) {
                                    service.offers.close(
                                        (program_id, data.creator, data.negotiation_id),
                                        OfferClosedReason::Complete,
                                        &service.events,
                                    );
                                    pending_complete.remove(&key);
                                    pending_offers.retain(|key, _| {
                                        key.0 != data.creator || key.1 != data.negotiation_id
                                    });
                                    pending_tickets.retain(|key, _| {
                                        key.0 != data.creator || key.1 != data.negotiation_id
                                    });
                                } else {
                                    if pending_complete.len() >= PENDING_FACTS
                                        && let Some(key) = pending_complete.keys().next().copied()
                                    {
                                        pending_complete.remove(&key);
                                    }
                                    pending_complete.insert(key, offer);
                                }
                                continue;
                            }
                            if !authenticated_offer(&offer, program_id, service.peer_id) {
                                continue;
                            }
                            let key = (data.creator, data.negotiation_id, data.offer_seq);
                            if let Some(ticket) = pending_tickets.get(&key)
                                && creator_ticket_matches(&offer, ticket)
                            {
                                pending_tickets.remove(&key);
                                Some(offer)
                            } else {
                                if pending_offers.len() >= PENDING_FACTS
                                    && let Some(key) = pending_offers.keys().next().copied()
                                {
                                    pending_offers.remove(&key);
                                }
                                pending_offers.insert(key, offer);
                                None
                            }
                        }
                        NegotiationFact::Ticket(ticket) => {
                            let data = &ticket.data;
                            if ticket.validate().is_err()
                                || !matches!(data.action, TicketAction::Active { .. })
                            {
                                continue;
                            }
                            if seen_tickets.len() >= PENDING_FACTS
                                && let Some(key) = seen_tickets.keys().next().copied()
                            {
                                seen_tickets.remove(&key);
                            }
                            seen_tickets.insert(TicketHash::of(data), ticket.clone());
                            // The ticket identifies its signer, not the creator.
                            // Recheck every pending offer for this negotiation.
                            pending_complete.retain(|&(creator, negotiation_id), offer| {
                                if negotiation_id != data.negotiation_id
                                    || !complete_offer_is_proven(offer, &seen_tickets)
                                {
                                    return true;
                                }
                                service.offers.close(
                                    (program_id, creator, negotiation_id),
                                    OfferClosedReason::Complete,
                                    &service.events,
                                );
                                pending_offers
                                    .retain(|key, _| key.0 != creator || key.1 != negotiation_id);
                                pending_tickets
                                    .retain(|key, _| key.0 != creator || key.1 != negotiation_id);
                                false
                            });
                            let key = (data.signer, data.negotiation_id, data.offer_seq);
                            if let Some(offer) = pending_offers.get(&key)
                                && creator_ticket_matches(offer, &ticket)
                            {
                                pending_offers.remove(&key)
                            } else {
                                if pending_tickets.len() >= PENDING_FACTS
                                    && let Some(key) = pending_tickets.keys().next().copied()
                                {
                                    pending_tickets.remove(&key);
                                }
                                pending_tickets.insert(key, ticket);
                                None
                            }
                        }
                        NegotiationFact::ActivationSignature(_)
                        | NegotiationFact::ActivationAnnouncement(_)
                        | NegotiationFact::Counteroffer(_) => None,
                    };
                    if let Some(offer) = pair {
                        let data = offer.data();
                        let Ok(params) = serde_json::from_slice(data.params.as_bytes()) else {
                            continue;
                        };
                        service.offers.insert(
                            OpenOffer {
                                program_id,
                                negotiation_id: data.negotiation_id,
                                creator: data.creator,
                                offer_seq: data.offer_seq,
                                target_size: data.target_size,
                                params,
                                deadline_unix_ms: data.deadline_unix_ms,
                                first_seen_ms: unix_time_ms(),
                            },
                            &service.events,
                        );
                    }
                }
            }
            .await;
            if let Err(error) = result {
                tracing::warn!(%program_id, %error, "offer watcher stopped");
            }
        });
        watchers.insert(program_id, handle);
    }

    /// Stop watching and close every local discovery entry for this program.
    pub(crate) fn unwatch_offers(&self, program_id: ProgramHash) {
        let mut watchers = self.offers.watchers.lock().expect("offer watchers");
        if let Some(handle) = watchers.remove(&program_id) {
            handle.abort();
        }
        self.offers.drop_program(program_id, &self.events);
    }

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

/// `/sync` row projection (design §3.2). Rows are read from summary index
/// columns through the store's read connections and projected with the same
/// functions as the list reads, so a `/sync` row equals the matching
/// `exec.list`, `receipt.list`, `program.list` and `blob.list` entry.
impl HostService {
    /// The current rows for `keys`, in key order, one op per distinct row:
    /// - `Exec(id)`: `store.exec_summary(id)` through `project_exec_summary`;
    /// - `Step { exec_id, step }`: all Step keys of one execution coalesce into
    ///   one `StepTimes` from the smallest such step to its latest stored step
    ///   (`store.list_step_times`), `state_prefix` = first four bytes of each
    ///   post-state hash, big-endian;
    /// - `Receipt(id)`: `store.receipt_summary(id)` mapped as `ReceiptList`;
    /// - `Program(hash)`: `ProgramDetail` from the catalog (`catalog.detail`)
    ///   when the program is active, else `ProgramRemoved`;
    /// - `Blob(hash)`: `store.blob_record(hash)` mapped as `blob.list` does.
    ///
    /// A missing exec, step or receipt row for a published key is store
    /// corruption (`ApiErrorCode::Storage`): those rows are never deleted.
    pub(crate) async fn sync_rows(
        &self,
        keys: &[arena0_store::ChangeKey],
    ) -> Result<Vec<arena0_api::RowOp>, ApiError> {
        use arena0_api::RowOp;
        use arena0_store::ChangeKey;
        // Preserve first-key order while folding step ranges and duplicate
        // identities. A later Step key can name an earlier step, so compute
        // each execution's lower bound before projecting its first key.
        let mut starts = std::collections::BTreeMap::new();
        for key in keys {
            if let ChangeKey::Step { exec_id, step } = key {
                starts
                    .entry(*exec_id)
                    .and_modify(|range: &mut (u64, u64)| {
                        range.0 = range.0.min(*step);
                        range.1 = range.1.max(*step);
                    })
                    .or_insert((*step, *step));
            }
        }
        // Membership, unlike detail, excludes removed programs. Fetch it
        // once for the batch; using the catalog's display limit here could
        // misclassify an active key beyond that limit as a removal. SQLite's
        // LIMIT is signed, so cap the request at the target's valid maximum.
        let active_programs: std::collections::BTreeSet<_> =
            if keys.iter().any(|key| matches!(key, ChangeKey::Program(_))) {
                self.store
                    .list_programs(usize::try_from(i64::MAX).unwrap_or(usize::MAX))
                    .await
                    .map_err(|e| ApiError::new(ApiErrorCode::Storage, e.to_string()))?
                    .into_iter()
                    .collect()
            } else {
                std::collections::BTreeSet::new()
            };
        let mut seen = std::collections::BTreeSet::new();
        let mut rows = Vec::new();
        for key in keys {
            let identity = match key {
                ChangeKey::Step { exec_id, .. } => ChangeKey::Step {
                    exec_id: *exec_id,
                    step: starts[exec_id].0,
                },
                key => *key,
            };
            if !seen.insert(identity) {
                continue;
            }
            let op = match identity {
                ChangeKey::Exec(id) => {
                    let row = self
                        .store
                        .exec_summary(id)
                        .await
                        .map_err(|e| ApiError::new(ApiErrorCode::Storage, e.to_string()))?
                        .ok_or_else(|| {
                            ApiError::new(
                                ApiErrorCode::Storage,
                                "published execution row is missing",
                            )
                        })?;
                    RowOp::Exec(Box::new(self.project_exec_summary(row).await?))
                }
                ChangeKey::Step { exec_id, step } => {
                    let row = self
                        .store
                        .list_step_times(exec_id, step)
                        .await
                        .map_err(|e| ApiError::new(ApiErrorCode::Storage, e.to_string()))?;
                    if starts[&exec_id].1 - step >= row.certified_at_ms.len() as u64 {
                        return Err(ApiError::new(
                            ApiErrorCode::Storage,
                            "published step row is missing",
                        ));
                    }
                    RowOp::Steps(sync_step_times(row))
                }
                ChangeKey::Receipt(id) => {
                    let row = self
                        .store
                        .receipt_summary(id)
                        .await
                        .map_err(|e| ApiError::new(ApiErrorCode::Storage, e.to_string()))?
                        .ok_or_else(|| {
                            ApiError::new(ApiErrorCode::Storage, "published receipt row is missing")
                        })?;
                    RowOp::Receipt(sync_receipt_entry(row))
                }
                ChangeKey::Program(hash) => {
                    if active_programs.contains(&hash) {
                        let detail = self
                            .catalog
                            .detail(hash)
                            .await
                            .map_err(|e| ApiError::new(ApiErrorCode::Storage, e.to_string()))?
                            .ok_or_else(|| {
                                ApiError::new(
                                    ApiErrorCode::Storage,
                                    "active program row is missing",
                                )
                            })?;
                        RowOp::Program(Box::new(detail))
                    } else {
                        RowOp::ProgramRemoved { program_hash: hash }
                    }
                }
                ChangeKey::Blob(hash) => {
                    let row = self
                        .store
                        .blob_record(hash)
                        .await
                        .map_err(|e| ApiError::new(ApiErrorCode::Storage, e.to_string()))?
                        .ok_or_else(|| {
                            ApiError::new(ApiErrorCode::Storage, "published blob row is missing")
                        })?;
                    RowOp::Blob(sync_blob_entry(row))
                }
            };
            rows.push(op);
        }
        Ok(rows)
    }

    /// Every row of this Host: `Exec` for every `exec.list` entry, `Steps` from
    /// step 0 for every execution with at least one stored step, every
    /// `Receipt`, every active `Program`, every `Blob`. Bounded by the same
    /// limits as the list reads.
    pub(crate) async fn sync_snapshot(&self) -> Result<Vec<arena0_api::RowOp>, ApiError> {
        use arena0_api::RowOp;
        let mut rows = Vec::new();
        for exec in self.exec_list().await? {
            if exec.step.is_some_and(|step| step > 0) {
                let steps = self
                    .store
                    .list_step_times(exec.exec_id, 0)
                    .await
                    .map_err(|e| ApiError::new(ApiErrorCode::Storage, e.to_string()))?;
                if steps.certified_at_ms.is_empty() {
                    return Err(ApiError::new(
                        ApiErrorCode::Storage,
                        "execution's stored steps are missing",
                    ));
                }
                rows.push(RowOp::Steps(sync_step_times(steps)));
            }
            rows.push(RowOp::Exec(Box::new(exec)));
        }
        rows.extend(
            self.store
                .list_receipt_summaries(4_096)
                .await
                .map_err(|e| ApiError::new(ApiErrorCode::Storage, e.to_string()))?
                .into_iter()
                .map(sync_receipt_entry)
                .map(RowOp::Receipt),
        );
        for program in self
            .catalog
            .list()
            .await
            .map_err(|e| ApiError::new(ApiErrorCode::Storage, e.to_string()))?
        {
            let detail = self
                .catalog
                .detail(program.program_hash)
                .await
                .map_err(|e| ApiError::new(ApiErrorCode::Storage, e.to_string()))?
                .ok_or_else(|| {
                    ApiError::new(ApiErrorCode::Storage, "active program row is missing")
                })?;
            rows.push(RowOp::Program(Box::new(detail)));
        }
        rows.extend(
            self.store
                .list_blobs()
                .await
                .map_err(|e| ApiError::new(ApiErrorCode::Storage, e.to_string()))?
                .into_iter()
                .map(sync_blob_entry)
                .map(RowOp::Blob),
        );
        Ok(rows)
    }

    /// This Host's open offers now, as an `Observation::Offers`, read the way
    /// `negotiation.offers` reads them.
    #[expect(
        clippy::unused_async,
        reason = "The committed sync projection API is asynchronous; offers are held in memory."
    )]
    pub(crate) async fn sync_offers(&self) -> Result<arena0_api::Observation, ApiError> {
        Ok(arena0_api::Observation::Offers {
            host: self.name.clone(),
            offers: self.offers.list(unix_time_ms(), &self.events),
        })
    }
}

fn sync_receipt_entry(row: arena0_store::ReceiptSummaryRow) -> arena0_api::ReceiptListEntry {
    arena0_api::ReceiptListEntry {
        receipt_id: row.receipt_id.to_string(),
        session_id: row.session_id,
        kind: row.kind,
        program_id: row.program_hash,
        completed: row.completed,
        provenance: row.provenance,
    }
}

fn sync_blob_entry(blob: arena0_store::BlobRecord) -> arena0_api::BlobEntry {
    arena0_api::BlobEntry {
        hash: blob.hash,
        length: blob.length,
        linked: blob.linked,
    }
}

fn sync_step_times(row: arena0_store::StepTimesRow) -> arena0_api::StepTimes {
    arena0_api::StepTimes {
        exec_id: row.exec_id,
        from_step: row.from_step,
        certified_at_ms: row.certified_at_ms,
        state_prefix: row
            .post_state
            .into_iter()
            .map(|hash| u32::from_be_bytes(hash.0[..4].try_into().expect("four-byte hash prefix")))
            .collect(),
    }
}

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
        if entry
            .lifecycle()
            .await
            .is_ok_and(ExecLifecycle::is_terminal)
        {
            return;
        }
        let mut fetch_rx = self.runtime.register_fetch_handler(session_hash);
        let bootstrap = committed
            .activation()
            .prepared()
            .signers()
            .collect::<Vec<_>>();
        let mut topic = match self
            .subscribe_negotiation(program_id, bootstrap.clone())
            .await
        {
            Ok(topic) => Some(topic),
            Err(error) => {
                tracing::warn!(%session_hash, %error, "relay could not subscribe to the program topic");
                None
            }
        };
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
    EventSource::most_specific(
        peer_id,
        request.execution_id(),
        request.program_hash(),
        request.negotiation_id(),
        candidate.session_id().filter(|_| candidate.has_execution()),
    )
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

/// Structural eligibility shared by discovery and Join. Authentication still
/// requires the creator's matching Active ticket; this alone proves no origin.
fn authenticated_offer(offer: &Offer, program_id: ProgramHash, local_peer: PeerId) -> bool {
    offer.validate().is_ok()
        && offer.data().program_hash == program_id
        && offer.data().creator != local_peer
        // A complete offer has frozen its participant set and cannot admit a Join.
        && !offer.is_complete()
}

/// True when `offer` is complete and every ticket it lists was seen, signs this
/// exact offer body, and is Active; the first is the creator's ticket.
fn complete_offer_is_proven(offer: &Offer, seen: &HashMap<TicketHash, Ticket>) -> bool {
    let offer_hash = OfferHash::of(offer.data());
    offer.is_complete()
        && offer.validate().is_ok()
        && offer.tickets().iter().all(|hash| {
            seen.get(hash).is_some_and(|ticket| {
                TicketHash::of(&ticket.data) == *hash
                    && ticket.data.negotiation_id == offer.data().negotiation_id
                    && matches!(ticket.data.action, TicketAction::Active { .. })
                    && ticket.verify_for_offer(&offer_hash).is_ok()
            })
        })
        // A structurally valid complete offer is nonempty, and the preceding
        // check established that every listed hash is present in `seen`.
        && creator_ticket_matches(offer, &seen[&offer.tickets()[0]])
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

#[cfg(test)]
mod tests {
    use super::*;

    use arena0_api::NextEvent;
    use arena0_crypto::bls::BlsSecretKey;
    use arena0_crypto::{BlsSignature, SecretKey, key_binding_message};
    use arena0_program::{LocalStateBytes, SharedStateBytes};
    use arena0_protocol::execution::ExecutionState;
    use arena0_protocol::{AbortKind, Activation, ActivationData, PreparedActivation};
    use arena0_test_engine::shared_test_engine;
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
        let keystore = Arc::new(Keystore::create(keys).unwrap());
        let peer_id = keystore.peer_id();

        let store = arena0_store::Store::open(arena0_store::StoreConfig::new(
            dir.path().join("arena0.sqlite"),
            peer_id,
        ))
        .unwrap();
        let store_handle = store.handle().clone();
        let catalog = ProgramCatalog::new(store_handle.clone());
        let engine = shared_test_engine();

        let network = LocalNetwork::new();
        let mut transports =
            LocalTransport::create_network(&network, vec![peer_id]).expect("test network");
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
        (dir, store, daemon, peer_id)
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
    fn event_inspection_projection_has_no_payload_fields() {
        let summary = arena0_store::EventRecordSummary {
            event_position: 4,
            agreed_steps: vec![3],
            event: arena0_store::EventKind::InputReceived,
            input_payload_bytes: Some(2),
            effects: vec![arena0_store::EffectSummary {
                kind: arena0_store::EffectKind::Broadcast,
                payload_bytes: Some(8),
            }],
        };
        let projected = project_event_record_summary(summary);
        assert_eq!(projected.event_position, 4);
        assert_eq!(projected.agreed_steps, vec![3]);
        assert_eq!(projected.event, arena0_api::EventKind::InputReceived);
        assert_eq!(projected.input_payload_bytes, Some(2));
        assert_eq!(projected.effects[0].kind, arena0_api::EffectKind::Broadcast);
        assert_eq!(projected.effects[0].payload_bytes, Some(8));
        let encoded = serde_json::to_value(projected).expect("projection JSON");
        assert_eq!(encoded["event"], "input_received");
        assert_eq!(
            encoded["effects"],
            serde_json::json!([{ "kind": "broadcast", "payload_bytes": 8 }])
        );
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
        daemon.emit_created(
            exec_id,
            ProgramHash([0xA2; 32]),
            None,
            None,
            ExecCreationOrigin::Request,
        );

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
        let feed = Events::new(
            HostInfo {
                id: "paired".into(),
                peer_id: PeerId([1; 32]),
                user_agent: None,
            },
            "00112233445566778899aabbccddeeff".into(),
        );
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
        let (_dir, _store, daemon, _peer) = test_daemon();
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
                ensemble: EnsembleSpec::Create {
                    participant_count: 2,
                },
                blobs: vec![],
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
                &[],
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
                events_from: Some(0),
                events_limit: MAX_EVENT_INSPECTION_RECORDS as u16,
            })
            .await
        {
            Ok(ResponseOk::Inspection(inspection)) => {
                assert_eq!(inspection.status.exec_id, exec_id);
                assert_eq!(inspection.status.lifecycle(), ExecLifecycle::Failed);
                assert!(inspection.activation.is_none());
                assert!(inspection.events.is_empty());
                assert_eq!(inspection.events_total, 0);
                assert_eq!(inspection.events_next, None);
            }
            other => panic!("expected exec.inspect, got {other:?}"),
        }
        assert!(matches!(
            daemon
                .dispatch(HostRequest::ExecInspect {
                    exec_id,
                    events_from: Some(0),
                    events_limit: (MAX_EVENT_INSPECTION_RECORDS + 1) as u16,
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
                    events_from: Some(0),
                    events_limit: 0,
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
    async fn recovery_visits_unfinished_requests_after_terminal_history() {
        let (_dir, store, daemon, _peer) = test_daemon();
        let (program_hash, _) = store
            .handle()
            .register_program(vec![1, 2, 3], 1)
            .await
            .expect("program");
        let tail = 17_u64;
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
                    ExecutionAdmission::create(
                        NegotiationId({
                            let mut bytes = [0; 32];
                            bytes[..8].copy_from_slice(&index.to_le_bytes());
                            bytes
                        }),
                        2,
                    )
                    .expect("admission"),
                    &[],
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

    /// A committed two-party activation created by the test daemon's Host,
    /// with the fixed BLS execution keys both participants signed with.
    struct TwoPartyActivation {
        other: PeerId,
        producer_bls: BlsSecretKey,
        other_bls: BlsSecretKey,
        prepared: PreparedActivation,
        activation: Activation,
    }

    /// Claim `execution_id` and durably record the creator's
    /// request and `prepared`, as negotiation does before signing.
    async fn record_prepared(
        daemon: &HostService,
        execution_id: ExecId,
        prepared: PreparedActivation,
    ) -> HostExecutionStore {
        let offer = prepared.offer().data();
        let (program_hash, params) = (offer.program_hash, offer.params.clone());
        let admission =
            ExecutionAdmission::create(offer.negotiation_id, offer.target_size).expect("admission");
        let mut writer = daemon.runtime.claim_execution(execution_id).unwrap();
        writer
            .create_execution_request(program_hash, Some(params), admission, &[], 1)
            .await
            .expect("request");
        writer
            .prepare_activation(prepared, 2)
            .await
            .expect("prepare");
        writer
    }

    /// Commit `activation` and create its execution aggregate from the
    /// initial state images.
    async fn commit_execution(
        writer: &mut HostExecutionStore,
        activation: Activation,
        producer: PeerId,
        shared: SharedStateBytes,
        local: LocalStateBytes,
    ) {
        writer
            .commit_activation(activation.clone(), 3)
            .await
            .expect("commit");
        writer
            .create_execution(activation, producer, shared, local, 4)
            .await
            .expect("execution");
    }

    fn two_party_activation(
        daemon: &HostService,
        peer: PeerId,
        program_hash: ProgramHash,
        negotiation_id: NegotiationId,
        initial_shared: &SharedStateBytes,
    ) -> TwoPartyActivation {
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
            StateHash::of_shared(initial_shared),
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
        TwoPartyActivation {
            other,
            producer_bls,
            other_bls,
            prepared,
            activation,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn activating_summary_matches_status_before_actor_activation() {
        let (_dir, store, daemon, peer) = test_daemon();
        let wasm =
            std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
                "../../programs/target/wasm32-unknown-unknown/release/rock_paper_scissors.wasm",
            ))
            .expect("built rock_paper_scissors guest; run `just build-programs`");
        let program = Program::try_from(wasm.clone()).unwrap();
        let (program_hash, _) = store.handle().register_program(wasm, 1).await.unwrap();
        let shared = daemon
            .engine
            .load(&program)
            .unwrap()
            .initialize(JsonBytes::try_new(b"null".to_vec()).unwrap())
            .unwrap()
            .shared;
        let exec_id = ExecId([0xD4; 32]);
        let TwoPartyActivation {
            prepared,
            activation,
            ..
        } = two_party_activation(
            &daemon,
            peer,
            program_hash,
            NegotiationId([0xD5; 32]),
            &shared,
        );
        // Commit the aggregate without starting an actor. This holds the real
        // durable interval before next.activate() without racing task scheduling.
        let mut writer = record_prepared(&daemon, exec_id, prepared).await;
        commit_execution(
            &mut writer,
            activation,
            peer,
            shared,
            LocalStateBytes::try_new(Vec::new()).unwrap(),
        )
        .await;
        drop(writer);

        let ResponseOk::ExecList(entries) = daemon.dispatch(HostRequest::ExecList).await.unwrap()
        else {
            panic!("expected execution list");
        };
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        let ResponseOk::Status(status) = daemon
            .dispatch(HostRequest::ExecStatus { exec_id })
            .await
            .unwrap()
        else {
            panic!("expected status");
        };
        let ResponseOk::Inspection(inspection) = daemon
            .dispatch(HostRequest::ExecInspect {
                exec_id,
                events_from: None,
                events_limit: 16,
            })
            .await
            .unwrap()
        else {
            panic!("expected inspection");
        };
        assert_eq!(status.lifecycle(), ExecLifecycle::Activating);
        assert!(status.session_id().is_some());
        assert!(status.session().is_none());
        assert_eq!(entry.exec_id, status.exec_id);
        assert_eq!(entry.negotiation_id, status.negotiation_id);
        assert_eq!(entry.program_id, status.program_id);
        assert_eq!(entry.lifecycle, status.lifecycle());
        assert_eq!(entry.session_id, status.session_id());
        assert_eq!(entry.created_at_ms, status.created_at_ms);
        assert_eq!(entry.updated_at_ms, status.updated_at_ms);
        assert_eq!(entry.end, status.end);
        assert_eq!(entry.activation, inspection.activation);
        let session = status.session();
        assert_eq!(entry.step, session.map(|session| session.step));
        assert_eq!(
            entry.participants,
            session.map(|session| session.participants)
        );
        assert_eq!(
            entry.peers,
            session
                .map(|session| session.peers.clone())
                .unwrap_or_default()
        );
        assert_eq!(
            entry.receipt_available,
            session.is_some_and(|session| session.receipt_available)
        );
        assert_eq!(entry.turn, session.and_then(|session| session.turn));
        assert_eq!(
            entry.phase,
            session.and_then(|session| session.phase.clone())
        );
        assert_eq!(
            entry.pending_callout.as_ref().map(|callout| (
                callout.pending_id,
                callout.callout_index,
                callout.name.as_str()
            )),
            status.pending_callout().map(|callout| (
                callout.pending_id,
                callout.callout_index,
                callout.name.as_str()
            ))
        );
        assert_eq!(entry.last_step_at_ms, None);
        assert_eq!(entry.reason, None);
        assert_eq!(entry.outcome, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn certified_execution_is_active_until_its_receipt_is_published() {
        use arena0_protocol::{Effect, Event, ParticipantStepSignature, TerminalOutcome};
        use arena0_store::{Change, TransitionRecord};

        let (_dir, store, daemon, peer) = test_daemon();
        // The status projection asks the program for its turn, so the shared
        // state must be one the real guest can decode.
        let wasm =
            std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
                "../../programs/target/wasm32-unknown-unknown/release/rock_paper_scissors.wasm",
            ))
            .expect("built rock_paper_scissors guest; run `just build-programs`");
        let program = Program::try_from(wasm.clone()).expect("program");
        let (program_hash, _) = store
            .handle()
            .register_program(wasm, 1)
            .await
            .expect("program");
        let initial_shared = daemon
            .engine
            .load(&program)
            .expect("load program")
            .initialize(JsonBytes::try_new(b"null".to_vec()).expect("params"))
            .expect("initialize program")
            .shared;
        let execution_id = ExecId([0xB4; 32]);
        let negotiation_id = NegotiationId([0xB5; 32]);
        let TwoPartyActivation {
            other,
            producer_bls,
            other_bls,
            prepared,
            activation,
        } = two_party_activation(&daemon, peer, program_hash, negotiation_id, &initial_shared);
        let mut writer = record_prepared(&daemon, execution_id, prepared).await;
        commit_execution(
            &mut writer,
            activation,
            peer,
            initial_shared.clone(),
            LocalStateBytes::try_new(Vec::new()).expect("local state"),
        )
        .await;
        drop(writer);

        // Certify a session-start step that ends the session, leaving the
        // receipt unpublished.
        let mut writer = store.handle().claim_execution(execution_id).unwrap();
        let mut state = writer.load_execution().await.unwrap().unwrap();
        let mut next = state.clone();
        next.activate().expect("activate");
        writer
            .persist(TransitionRecord {
                expected: state.version(),
                next: next.clone(),
                change: Change::State,
                now_ms: 5,
            })
            .await
            .expect("persist activation");
        state = next;
        let event = Event::SessionStarted {
            ensemble: state.binding().ensemble().expect("ensemble"),
        };
        let effects = vec![Effect::SessionEnd {
            outcome: Vec::new(),
        }];
        let mut next = state.clone();
        next.apply_dispatch(
            &event,
            initial_shared.clone(),
            LocalStateBytes::try_new(Vec::new()).expect("local state"),
            &effects,
            Some(TerminalOutcome::new(Vec::new(), b"null".to_vec()).expect("outcome")),
            None,
            None,
        )
        .expect("dispatch");
        writer
            .persist(TransitionRecord {
                expected: state.version(),
                next: next.clone(),
                change: Change::Dispatch {
                    event,
                    effects,
                    timer_id: None,
                    blobs: Vec::new(),
                },
                now_ms: 5,
            })
            .await
            .expect("persist proposal");
        state = next;
        let commitment = state.proposal_commitment().expect("proposal");
        for (signer, key) in [(peer, &producer_bls), (other, &other_bls)] {
            let mut next = state.clone();
            let certified = next
                .add_step_signature(ParticipantStepSignature::new(
                    signer,
                    commitment.step,
                    key.sign(&commitment.signing_bytes()),
                ))
                .expect("step signature");
            writer
                .persist(TransitionRecord {
                    expected: state.version(),
                    next: next.clone(),
                    change: Change::StepSignature { certified },
                    now_ms: 6,
                })
                .await
                .expect("persist signature");
            state = next;
        }
        assert!(matches!(
            state.status(),
            arena0_protocol::ExecutionStatus::Certified { .. }
        ));
        drop(writer);

        // The status projection and the active count agree: certification
        // stays active until the receipt is published.
        match daemon
            .dispatch(HostRequest::ExecStatus {
                exec_id: execution_id,
            })
            .await
        {
            Ok(ResponseOk::Status(status)) => {
                assert_eq!(status.lifecycle(), ExecLifecycle::Active);
            }
            other => panic!("expected exec.status, got {other:?}"),
        }
        assert_eq!(
            daemon
                .store
                .count_active_executions()
                .await
                .expect("active execution count"),
            1
        );
        daemon.stop().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn end_wake_resumes_without_replaying_observations() {
        use arena0_protocol::AbortOccurrence;
        use arena0_store::{Change, TransitionRecord};

        let (_dir, store, daemon, peer) = test_daemon();
        let wasm =
            std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
                "../../programs/target/wasm32-unknown-unknown/release/rock_paper_scissors.wasm",
            ))
            .expect("built rock_paper_scissors guest; run `just build-programs`");
        let program = Program::try_from(wasm.clone()).expect("program");
        let (program_hash, _) = store
            .handle()
            .register_program(wasm, 1)
            .await
            .expect("program");
        let params = JsonBytes::try_new(b"null".to_vec()).expect("params");
        let initialized = daemon
            .engine
            .load(&program)
            .expect("load program")
            .initialize(params.clone())
            .expect("initialize program");
        let execution_id = ExecId([0xC4; 32]);
        let negotiation_id = NegotiationId([0xC5; 32]);
        let TwoPartyActivation {
            other,
            prepared,
            activation,
            ..
        } = two_party_activation(
            &daemon,
            peer,
            program_hash,
            negotiation_id,
            &initialized.shared,
        );
        let mut writer = record_prepared(&daemon, execution_id, prepared).await;
        commit_execution(
            &mut writer,
            activation,
            peer,
            initialized.shared,
            initialized.local,
        )
        .await;
        drop(writer);

        // Stop, publish, and close the confirmation window with the other
        // participant still unconfirmed: the state an end wake resumes.
        async fn persist(
            writer: &mut arena0_store::ExecutionStore,
            state: &mut ExecutionState,
            next: ExecutionState,
            change: Change,
        ) {
            writer
                .persist(TransitionRecord {
                    expected: state.version(),
                    next: next.clone(),
                    change,
                    now_ms: 5,
                })
                .await
                .expect("persist transition");
            *state = next;
        }
        let mut writer = store.handle().claim_execution(execution_id).unwrap();
        let mut state = writer.load_execution().await.unwrap().unwrap();
        let mut next = state.clone();
        next.activate().expect("activate");
        let unsigned = AbortOccurrence::unsigned(
            next.binding().session_id(),
            peer,
            AbortKind::Abort,
            0,
            "operator stop".to_owned(),
            next.step_cursor(),
        )
        .expect("abort occurrence");
        let signature = daemon
            .identity
            .sign(&unsigned.signing_bytes().expect("abort bytes"));
        next.stop(unsigned.with_signature(signature).expect("signed abort"))
            .expect("stop");
        persist(&mut writer, &mut state, next, Change::State).await;
        let artifact = writer.assemble_receipt(&state).await.expect("receipt");
        let mut next = state.clone();
        next.publish_receipt(artifact.clone()).expect("publish");
        persist(&mut writer, &mut state, next, Change::Publish { artifact }).await;
        let mut next = state.clone();
        next.expire_end().expect("expire end");
        persist(&mut writer, &mut state, next, Change::State).await;
        assert!(matches!(
            state.end_phase(),
            arena0_protocol::EndPhase::Ended { unconfirmed } if unconfirmed.contains(&other)
        ));
        drop(writer);

        let replayed = |frame: &EventFrame| {
            frame.exec_id == Some(execution_id)
                && matches!(
                    frame.data,
                    EventData::Created { .. }
                        | EventData::SessionStarted { .. }
                        | EventData::SessionEnded { .. }
                        | EventData::Terminated { .. }
                )
        };
        let wake = |cause| {
            let daemon = &daemon;
            async move {
                let candidate = daemon
                    .store
                    .end_wake_candidate(execution_id)
                    .await
                    .expect("load wake candidate")
                    .expect("ended execution is a wake candidate");
                let mut events = daemon.events.subscribe();
                let tasks_before = daemon.tasks.lock().await.len();
                daemon
                    .resume_candidate(candidate, cause)
                    .await
                    .expect("resume");
                let spawned_tasks = daemon.tasks.lock().await.len() - tasks_before;
                assert!(daemon.execs.get(&execution_id).is_some(), "actor resumed");
                let mut observed = Vec::new();
                let _ = tokio::time::timeout(Duration::from_secs(2), async {
                    while let Ok(frame) = events.recv().await {
                        if replayed(&frame) {
                            observed.push(frame.data);
                        }
                    }
                })
                .await;
                daemon.execs.stop().await;
                (observed, spawned_tasks)
            }
        };

        // A peer's wake only finishes end confirmation: no replayed
        // observations and no session relay for the creator.
        assert_eq!(wake(ResumeCause::EndWake).await, (Vec::new(), 0));
        // Startup recovery reports the same execution to a new observer.
        let (observed, spawned_tasks) = wake(ResumeCause::Startup).await;
        assert_eq!(
            spawned_tasks, 1,
            "startup recovery starts the creator's relay"
        );
        assert!(
            observed
                .iter()
                .any(|data| matches!(data, EventData::Created { .. })),
            "{observed:?}"
        );
        assert!(
            observed
                .iter()
                .any(|data| matches!(data, EventData::SessionEnded { .. })),
            "{observed:?}"
        );
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
        let initial_shared = SharedStateBytes::try_new(vec![0]).expect("shared state");
        let TwoPartyActivation {
            other,
            prepared,
            activation,
            ..
        } = two_party_activation(&daemon, peer, program_hash, negotiation_id, &initial_shared);
        let mut writer = record_prepared(&daemon, execution_id, prepared).await;
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
            None,
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
            None,
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
                ExecutionAdmission::create(negotiation_id, 2).expect("admission"),
                &[],
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
            None,
        )
        .expect("project post-commit failure");
        assert!(matches!(
            failed_status.state,
            ExecStatusState::Failed {
                session: Some(SessionProgress::Activated { session_id }), ..
            } if session_id == activation.session_hash()
        ));
        let state = ExecutionState::new(
            execution_id,
            activation.clone(),
            peer,
            initial_shared,
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
