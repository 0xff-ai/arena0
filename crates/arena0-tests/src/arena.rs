//! In-process test harness for real Host, SQLite, transport, and Wasm paths.
//!
//! The harness deliberately follows the public construction boundary: each
//! participant has a persistent SQLite [`arena0_store::Store`], a Host-issued
//! execution capability, an admitted immutable Wasm program, and a local
//! transport endpoint. There is no mutable sandbox or in-memory store mock in
//! this test layer.
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arena0_crypto::{BlsSignature, ExecutionKey, ExecutionSalt, NodeKeys, SecretKey};
use arena0_node::{ActivatedSession, ExecContext, NegotiationBook};
use arena0_node::{
    DurableOutcome, ExecCommand, Host, NegotiationAttempt, NegotiationEffects, NegotiationStart,
    PrepareOutcome, SessionMessage, SpawnedExec,
};
use arena0_program::JsonBytes;
use arena0_protocol::{
    EventSource, ExecId, ExecutionAdmission, NegotiationEvent, NegotiationId, OfferData, PeerId,
    PeerIdSource, ReceiptArtifact, SessionHash, SessionHeader, SessionTermination, StateHash,
    TraceEntry,
};
use arena0_sandbox::{InitializeCall, Program, WasmtimeEngine};
use arena0_store::{Store, StoreConfig, StoreHandle};
use arena0_transport::Transport;
use arena0_transport::local::LocalTransport;
use arena0_verify::verify_full;
use tempfile::TempDir;
use tokio::sync::Barrier;

/// Configuration phase.
#[allow(missing_debug_implementations)]
pub struct Arena {
    wasm: Option<Vec<u8>>,
    participant_count: usize,
    params: Vec<u8>,
    timeout: Duration,
}

struct HarnessNode {
    identity: Arc<NodeKeys>,
    peer_id: PeerId,
}

/// One safe progress fact captured by the direct greybox harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArenaProgress {
    NegotiationCommitted,
    SessionStarted,
    CertifiedStep { step: u64 },
    TerminalPublished,
    ReceiptReplay { elapsed_us: u64, success: bool },
}

/// A monotonic, participant-scoped point in an [`Arena`] run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedProgress {
    pub participant: usize,
    pub elapsed_us: u64,
    pub progress: ArenaProgress,
}

fn record_progress(
    timeline: &Mutex<Vec<TimedProgress>>,
    started: Instant,
    participant: usize,
    progress: ArenaProgress,
) {
    let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    timeline
        .lock()
        .expect("progress timeline lock")
        .push(TimedProgress {
            participant,
            elapsed_us,
            progress,
        });
}

fn record_session_progress(
    timeline: &Mutex<Vec<TimedProgress>>,
    started: Instant,
    participant: usize,
    event: &SessionMessage,
) {
    let progress = match event {
        SessionMessage::SessionStarted { .. } => Some(ArenaProgress::SessionStarted),
        SessionMessage::TraceAppended { step } => {
            Some(ArenaProgress::CertifiedStep { step: *step })
        }
        SessionMessage::ReceiptPublished { .. } => Some(ArenaProgress::TerminalPublished),
        SessionMessage::CalloutRequested { .. }
        | SessionMessage::Notification { .. }
        | SessionMessage::Completed { .. }
        | SessionMessage::Aborted { .. }
        | SessionMessage::Failed { .. } => None,
    };
    if let Some(progress) = progress {
        record_progress(timeline, started, participant, progress);
    }
}

fn progress_summary(timeline: &[TimedProgress], participant_count: usize) -> String {
    let mut summary = String::new();
    for participant in 0..participant_count {
        let points = timeline
            .iter()
            .filter(|point| point.participant == participant)
            .collect::<Vec<_>>();
        let negotiation = points.iter().find_map(|point| {
            matches!(&point.progress, ArenaProgress::NegotiationCommitted)
                .then_some(point.elapsed_us)
        });
        let started = points.iter().find_map(|point| {
            matches!(&point.progress, ArenaProgress::SessionStarted).then_some(point.elapsed_us)
        });
        let certified = points
            .iter()
            .filter_map(|point| match &point.progress {
                ArenaProgress::CertifiedStep { step } => Some((*step, point.elapsed_us)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let terminal = points.iter().find_map(|point| {
            matches!(&point.progress, ArenaProgress::TerminalPublished).then_some(point.elapsed_us)
        });
        let replay = points.iter().find_map(|point| match &point.progress {
            ArenaProgress::ReceiptReplay {
                elapsed_us,
                success,
            } => Some((*elapsed_us, *success)),
            _ => None,
        });
        let (last_step, last_step_at) = certified.last().copied().unzip();
        let _ = write!(
            summary,
            "p{participant}[neg@{negotiation:?} start@{started:?} steps={count} last={last_step:?}@{last_step_at:?} terminal@{terminal:?} replay={replay:?}] ",
            count = certified.len(),
        );
    }
    summary.pop();
    summary
}

/// The deterministic execution id for participant `i`.
fn exec_id_for(i: usize) -> ExecId {
    let mut exec_id = [0u8; 32];
    exec_id[0] = i as u8;
    exec_id[31] = 0xEE;
    ExecId(exec_id)
}

fn execution_salt_for(i: usize) -> ExecutionSalt {
    let byte = u8::try_from(i + 1).expect("test participant index fits in a non-zero byte");
    ExecutionSalt::try_from_bytes([byte; 32]).expect("non-zero test salt")
}

impl Default for Arena {
    fn default() -> Self {
        Self::new()
    }
}

impl Arena {
    pub fn new() -> Self {
        Self {
            wasm: None,
            participant_count: 2,
            // Generated programs without a Params DTO use stock Serde's unit
            // representation (`null`). Programs with parameters override this
            // through `params`.
            params: br#"null"#.to_vec(),
            timeout: Duration::from_secs(15),
        }
    }

    pub fn program(&mut self, wasm: Vec<u8>) -> &mut Self {
        self.wasm = Some(wasm);
        self
    }

    pub fn participants(&mut self, n: usize) -> &mut Self {
        self.participant_count = n;
        self
    }

    pub fn timeout(&mut self, timeout: Duration) -> &mut Self {
        self.timeout = timeout;
        self
    }

    /// Set the immutable parameters shared by every participant.
    pub fn params(&mut self, params: Vec<u8>) -> &mut Self {
        self.params = params;
        self
    }

    /// Form and run one real multiparty session through Host and LocalTransport.
    pub async fn run(&self) -> Run {
        let timeline_started = Instant::now();
        let timeline = Arc::new(Mutex::new(Vec::new()));
        let wasm = self.wasm.clone().expect("program not set");
        let n = self.participant_count;

        let mut identities: Vec<HarnessNode> = (0..n)
            .map(|i| {
                let mut seed = [0u8; 32];
                seed[0] = i as u8;
                seed[31] = 0xFF;
                let identity = Arc::new(NodeKeys::from_secret(SecretKey::from_bytes(seed)));
                HarnessNode {
                    peer_id: identity.peer_id(),
                    identity,
                }
            })
            .collect();
        identities.sort_by_key(|node| node.peer_id);
        let peer_ids: Vec<PeerId> = identities.iter().map(|node| node.peer_id).collect();

        let program = Program::try_from(wasm.clone()).expect("program");
        let program_id = program.hash();
        let creator_params = self.params.clone();
        let creator_program = WasmtimeEngine::new()
            .expect("sandbox")
            .admit(&program)
            .expect("admit");
        let initialized = creator_program
            .initialize(InitializeCall::new(
                JsonBytes::try_new(creator_params.clone()).expect("valid creator params"),
            ))
            .expect("initialize program");
        let initial_state = StateHash::of(initialized.shared.as_bytes());

        // Every Host gets a real SQLite owner and a registry entry before the
        // negotiation driver is allowed to prepare activation evidence.
        let mut stores: Vec<(TempDir, Store)> = Vec::with_capacity(n);
        let mut host_specs = Vec::with_capacity(n);
        for node in &identities {
            let directory = tempfile::tempdir().expect("store directory");
            let store = Store::open(StoreConfig::new(
                directory.path().join("arena0.sqlite"),
                node.peer_id,
            ))
            .expect("open store");
            store
                .handle()
                .register_program(wasm.clone(), unix_time_ms())
                .await
                .expect("register program");
            host_specs.push((Arc::clone(&node.identity), store.handle().clone()));
            stores.push((directory, store));
        }

        let negotiation_id = NegotiationId([0xA7; 32]);
        let offer_data = OfferData::new(
            negotiation_id,
            0,
            identities[0].peer_id,
            program_id,
            arena0_program::ExecutionProfile::current().hash(),
            JsonBytes::try_new(creator_params.clone()).expect("valid offer params"),
            u16::try_from(n).expect("participant count fits u16"),
            initial_state,
            u64::MAX,
        )
        .expect("valid offer data");
        let creator_keys = Arc::clone(&identities[0].identity);
        let creator_execution =
            ExecutionKey::derive(&execution_salt_for(0), &exec_id_for(0).0, &negotiation_id.0)
                .expect("creator execution key");
        let creator_book = NegotiationBook::new(&creator_keys, &creator_execution);
        let (offer, creator_ticket) = creator_book
            .create_creator_offer(offer_data, unix_time_ms())
            .expect("creator offer");

        let ensemble = arena0_node::Ensemble::start(host_specs).expect("valid local ensemble");
        assert_eq!(ensemble.peer_ids(), peer_ids);
        let transports: Vec<Arc<LocalTransport>> = peer_ids
            .iter()
            .map(|peer_id| ensemble.transport(peer_id).expect("host transport"))
            .collect();
        let hosts: Vec<Arc<Host>> = peer_ids
            .iter()
            .map(|peer_id| ensemble.host(peer_id).expect("host"))
            .collect();

        let deadline = tokio::time::Instant::now() + self.timeout;
        let negotiation_events =
            Arc::new(Mutex::new(Vec::<(EventSource, NegotiationEvent)>::new()));
        // Let every participant install its topic subscription before any
        // drive emits the initial offer. LocalTransport has no replay for a
        // fact published before a peer joins, so starting one drive ahead of
        // the rest can strand a participant in Gossiping indefinitely.
        let negotiation_barrier = Arc::new(Barrier::new(n));
        let mut negotiation_tasks = Vec::with_capacity(n);
        for (i, host) in hosts.iter().enumerate() {
            let host = Arc::clone(host);
            let negotiation_events = Arc::clone(&negotiation_events);
            let progress_timeline = Arc::clone(&timeline);
            let progress_started = timeline_started;
            let negotiation_barrier = Arc::clone(&negotiation_barrier);
            let task_offer = offer.clone();
            let bootstrap = peer_ids
                .iter()
                .copied()
                .filter(|peer| *peer != host.peer_id)
                .collect::<Vec<_>>();
            let exec_id = exec_id_for(i);
            let transport = Arc::clone(&transports[i]);
            let task_creator_ticket = (i == 0).then(|| creator_ticket.clone());
            let recompute_wasm = wasm.clone();
            let mut execution_store = host.claim_execution(exec_id).expect("execution claim");
            let admission = if i == 0 {
                ExecutionAdmission::explicit(negotiation_id, peer_ids.clone())
                    .expect("valid creator admission")
            } else {
                ExecutionAdmission::join(identities[0].peer_id, negotiation_id)
            };
            execution_store
                .create_execution_request(
                    program_id,
                    Some(JsonBytes::try_new(creator_params.clone()).expect("valid request params")),
                    admission,
                    unix_time_ms(),
                )
                .await
                .expect("durable execution request");

            negotiation_tasks.push(tokio::spawn(async move {
                let emit = move |source, event| {
                    if matches!(event, NegotiationEvent::ActivationCommitted { .. }) {
                        record_progress(
                            &progress_timeline,
                            progress_started,
                            i,
                            ArenaProgress::NegotiationCommitted,
                        );
                    }
                    negotiation_events
                        .lock()
                        .expect("negotiation event lock")
                        .push((source, event));
                };
                let execution = host
                    .execution_key(&execution_salt_for(i), &exec_id, &negotiation_id)
                    .expect("execution key");
                let topic = transport
                    .subscribe_program(task_offer.data().program_hash, bootstrap)
                    .await
                    .expect("subscribe program topic");
                negotiation_barrier.wait().await;
                let attempt = NegotiationAttempt {
                    topic,
                    exec_id,
                    start: NegotiationStart::Fresh {
                        offer: task_offer,
                        creator_ticket: task_creator_ticket,
                        preferred_params: None,
                    },
                    supervision: None,
                    deadline: Some(deadline),
                };
                let prepare: arena0_node::PrepareEffect = Box::new(
                    |store: &mut arena0_store::ExecutionStore,
                     prepared|
                     -> Pin<
                        Box<dyn Future<Output = Result<PrepareOutcome, String>> + Send + '_>,
                    > {
                        Box::pin(async move {
                            match store
                                .prepare_activation(prepared, unix_time_ms())
                                .await
                                .map_err(|error| error.to_string())?
                            {
                                arena0_store::PrepareActivationOutcome::Prepared(_)
                                | arena0_store::PrepareActivationOutcome::AlreadyPrepared(_)
                                | arena0_store::PrepareActivationOutcome::AlreadyCommitted(_) => {
                                    Ok(PrepareOutcome::Accepted)
                                }
                                arena0_store::PrepareActivationOutcome::Conflict { .. } => {
                                    Ok(PrepareOutcome::Conflict)
                                }
                            }
                        })
                    },
                );
                let persist_activation: arena0_node::PersistActivationEffect = Box::new(
                    |store: &mut arena0_store::ExecutionStore,
                     activation|
                     -> Pin<
                        Box<dyn Future<Output = Result<DurableOutcome, String>> + Send + '_>,
                    > {
                        Box::pin(async move {
                            match store
                                .commit_activation(activation, unix_time_ms())
                                .await
                                .map_err(|error| error.to_string())?
                            {
                                arena0_store::CommitActivationOutcome::Committed(_)
                                | arena0_store::CommitActivationOutcome::AlreadyCommitted(_) => {
                                    Ok(DurableOutcome::Accepted)
                                }
                                arena0_store::CommitActivationOutcome::Conflict { .. } => {
                                    Ok(DurableOutcome::Conflict)
                                }
                            }
                        })
                    },
                );
                let recompute_initial_state = Box::new(move |params: &[u8]| {
                    let program = Program::try_from(recompute_wasm.clone())
                        .map_err(|error| error.to_string())?;
                    let admitted = WasmtimeEngine::new()
                        .map_err(|error| error.to_string())?
                        .admit(&program)
                        .map_err(|error| error.to_string())?;
                    let initialized = admitted
                        .initialize(InitializeCall::new(
                            JsonBytes::try_new(params.to_vec())
                                .map_err(|error| error.to_string())?,
                        ))
                        .map_err(|error| error.to_string())?;
                    Ok(StateHash::of(initialized.shared.as_bytes()))
                });
                let effects = NegotiationEffects {
                    prepare,
                    persist_activation,
                    recompute_initial_state,
                    emit: &emit,
                };
                host.negotiate(&execution, execution_store, attempt, effects)
                    .await
            }));
        }

        let mut negotiation_results = Vec::with_capacity(n);
        for task in negotiation_tasks {
            negotiation_results.push(task.await.expect("negotiation task panicked"));
        }
        let negotiation_errors = negotiation_results
            .iter()
            .enumerate()
            .filter_map(|(i, result)| {
                result
                    .as_ref()
                    .err()
                    .map(|error| format!("node {i}: {error:?}"))
            })
            .collect::<Vec<_>>();
        let mut committeds = Vec::with_capacity(n);
        for (i, result) in negotiation_results.into_iter().enumerate() {
            let negotiated = result.unwrap_or_else(|error| {
                let all_events = negotiation_events
                    .lock()
                    .expect("negotiation event lock")
                    .iter()
                    .map(|(source, event)| {
                        let exec_id = match source {
                            EventSource::Negotiation { exec_id, .. } => *exec_id,
                            EventSource::Execution { exec_id, .. } => *exec_id,
                            EventSource::Session { exec_id, .. } => *exec_id,
                        };
                        (exec_id.0[0], event.clone())
                    })
                    .collect::<Vec<_>>();
                panic!("node {i} negotiation failed: {error}; results: {negotiation_errors:?}; events: {all_events:?}");
            });
            committeds.push(negotiated);
        }
        for (i, (committed, _)) in committeds.iter().enumerate() {
            assert_eq!(
                committed.session_hash(),
                committeds[0].0.session_hash(),
                "node {i} confirmed a different session hash"
            );
        }
        let committed = committeds[0].0.clone();
        let mut negotiated = committeds.into_iter();

        // The negotiation returns the same Host-bound writer capability. Move
        // it directly into Host::spawn; no second store claim or mutable guest
        // adapter exists between activation and execution.
        let mut participants = Vec::with_capacity(n);
        for (i, ((directory, store), host)) in stores.into_iter().zip(hosts.iter()).enumerate() {
            let node = &identities[i];
            let node_params = creator_params.clone();
            let program = Program::try_from(wasm.clone()).expect("program");
            let admitted = WasmtimeEngine::new()
                .expect("sandbox creation")
                .admit(&program)
                .expect("admit program");
            let exec_id = exec_id_for(i);
            let execution_key = host
                .execution_key(&execution_salt_for(i), &exec_id, &negotiation_id)
                .expect("execution key");
            let (node_committed, execution_store) = negotiated.next().expect("committed execution");
            let context = ExecContext::new(
                exec_id,
                admitted,
                JsonBytes::try_new(node_params.clone()).expect("valid node params"),
                node_committed.activation().clone(),
                execution_key,
            );
            let spawned = host
                .spawn(context, execution_store)
                .expect("spawn execution");
            let store_handle = store.handle().clone();
            participants.push(ParticipantHandle {
                peer_id: node.peer_id,
                spawned,
                exec_id,
                negotiation_id,
                execution_salt: execution_salt_for(i),
                _directory: directory,
                _store: store,
                store_handle,
                events: Vec::new(),
                termination: None,
                session_hash: None,
                receipt: None,
            });
        }

        let deadline = tokio::time::Instant::now() + self.timeout;
        let ready = |participant: &ParticipantHandle| {
            participant.session_hash.is_some() || participant.termination.is_some()
        };
        while !participants.iter().all(ready) {
            if tokio::time::Instant::now() >= deadline {
                let progress =
                    progress_summary(&timeline.lock().expect("progress timeline lock"), n);
                panic!(
                    "timeout waiting for session establishment ({}/{}) ready; progress: {progress}",
                    participants
                        .iter()
                        .filter(|participant| ready(participant))
                        .count(),
                    n
                );
            }
            for (i, participant) in participants.iter_mut().enumerate() {
                while let Ok(event) = participant.spawned.message_rx.try_recv() {
                    record_event(
                        &mut participant.session_hash,
                        &mut participant.termination,
                        &mut participant.receipt,
                        &event,
                    );
                    record_session_progress(&timeline, timeline_started, i, &event);
                    participant.events.push(event);
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        Run {
            participants,
            program: wasm,
            params: creator_params,
            timeout: self.timeout,
            committed,
            timeline_started,
            timeline,
        }
    }
}

fn unix_time_ms() -> u64 {
    arena0_node::unix_time_ms()
}

fn record_event(
    session_hash: &mut Option<SessionHash>,
    termination: &mut Option<SessionTermination>,
    receipt: &mut Option<ReceiptArtifact>,
    event: &SessionMessage,
) {
    match event {
        SessionMessage::SessionStarted {
            session_id: sid, ..
        } => *session_hash = Some(*sid),
        SessionMessage::ReceiptPublished { receipt: published } => {
            *receipt = Some(published.clone());
        }
        SessionMessage::Completed { result, .. } => {
            *termination = Some(SessionTermination::Completed {
                outcome: result.clone(),
            });
        }
        SessionMessage::Aborted { reason, .. } => {
            *termination = Some(SessionTermination::Terminated {
                reason: reason.clone(),
            });
        }
        SessionMessage::Failed { reason } => {
            *termination = Some(SessionTermination::FailedHost {
                subsystem: "runtime".into(),
                message: reason.clone(),
            });
        }
        _ => {}
    }
}

/// Interaction phase.
pub struct Run {
    participants: Vec<ParticipantHandle>,
    program: Vec<u8>,
    params: Vec<u8>,
    timeout: Duration,
    committed: ActivatedSession,
    timeline_started: Instant,
    timeline: Arc<Mutex<Vec<TimedProgress>>>,
}

impl Run {
    pub fn expect_input(&mut self, participant: usize) -> Expect<'_> {
        Expect {
            run: self,
            participant,
        }
    }

    async fn wait_for_all_terminal(&mut self, timeout_action: &str) -> Vec<SessionTermination> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        while self
            .participants
            .iter()
            .any(|participant| participant.termination.is_none())
        {
            if tokio::time::Instant::now() >= deadline {
                let pending: Vec<usize> = self
                    .participants
                    .iter()
                    .enumerate()
                    .filter(|(_, participant)| participant.termination.is_none())
                    .map(|(i, _)| i)
                    .collect();
                let mut diagnostics = Vec::with_capacity(self.participants.len());
                for (i, participant) in self.participants.iter().enumerate() {
                    let loaded_state = participant
                        .store_handle
                        .load_execution(participant.exec_id)
                        .await
                        .expect("execution state query");
                    let inbox = match loaded_state.as_ref() {
                        Some(_) => participant
                            .store_handle
                            .list_pending_inbox(participant.exec_id, 64)
                            .await
                            .map(|items| {
                                items
                                    .iter()
                                    .map(|item| {
                                        let source = self
                                            .participants
                                            .iter()
                                            .position(|candidate| {
                                                candidate.peer_id == item.source()
                                            })
                                            .map_or_else(
                                                || item.source().to_string(),
                                                |index| format!("p{index}"),
                                            );
                                        let frame = match item.frame() {
                                            arena0_protocol::ExecFrame::Message { seq, .. } => {
                                                format!("message@{seq}")
                                            }
                                            arena0_protocol::ExecFrame::StepSignature {
                                                commitment,
                                                ..
                                            } => {
                                                format!("step-signature@{}", commitment.step)
                                            }
                                            arena0_protocol::ExecFrame::End { .. } => "end".into(),
                                            arena0_protocol::ExecFrame::Abort { .. } => {
                                                "abort".into()
                                            }
                                        };
                                        format!("{source}:{frame}")
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_else(|error| vec![format!("error:{error}")]),
                        None => vec!["execution-missing".into()],
                    };
                    let state = loaded_state.as_ref().map(|state| {
                        let signatures = state
                            .pending_shared()
                            .map(|proposal| {
                                proposal
                                    .signatures()
                                    .iter()
                                    .map(|signature| {
                                        self.participants
                                            .iter()
                                            .position(|candidate| {
                                                candidate.peer_id == signature.participant()
                                            })
                                            .map_or_else(
                                                || signature.participant().to_string(),
                                                |index| format!("p{index}"),
                                            )
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        format!(
                            "lifecycle={:?} version={} step={} private={} reacted={:?} proposal_step={:?} shared_signatures={signatures:?} terminal_pending={} timers={}",
                            state.lifecycle(),
                            state.version(),
                            state.public().next_step(),
                            state.private().next_record(),
                            state.private().last_reaction_position(),
                            state.pending_shared().map(|proposal| proposal.commitment().step),
                            state.terminal_pending(),
                            state.active_timers().count(),
                        )
                    });
                    let trace = match loaded_state.as_ref() {
                        Some(_) => participant
                            .store_handle
                            .read_trace(participant.exec_id, 0, u64::MAX)
                            .await
                            .map(|entries| {
                                format!(
                                    "count={} last_step={:?}",
                                    entries.len(),
                                    entries.last().map(|entry| entry.step)
                                )
                            })
                            .unwrap_or_else(|error| format!("error:{error}")),
                        None => "execution-missing".into(),
                    };
                    diagnostics.push(format!(
                        "node {i} state={state:?} trace={trace:?} inbox={inbox:?} events={}",
                        participant.events.len(),
                    ));
                }
                let progress = self.progress_summary();
                panic!(
                    "timeout: participants {pending:?} did not {timeout_action}; progress: {progress}; diagnostics: {diagnostics:?}"
                );
            }
            self.drain_events();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.participants
            .iter()
            .map(|participant| participant.termination.clone().expect("terminal"))
            .collect()
    }

    /// Wait for every participant to complete and return opaque Borsh outcome bytes.
    pub async fn expect_completed_all(&mut self) -> Vec<Vec<u8>> {
        let terminations = self.wait_for_all_terminal("complete").await;
        for (i, termination) in terminations.iter().enumerate() {
            if !matches!(termination, SessionTermination::Completed { .. }) {
                panic!("participant {i}: expected Completed, got {termination:?}");
            }
        }
        terminations
            .iter()
            .map(|termination| match termination {
                SessionTermination::Completed { outcome } => outcome.clone(),
                _ => unreachable!(),
            })
            .collect()
    }

    /// Wait until every node reaches a terminal state without asserting success.
    pub async fn wait_all_terminal(&mut self) -> Vec<SessionTermination> {
        self.wait_for_all_terminal("terminate").await
    }

    /// Wait for portable artifacts, which may follow stopped lifecycle notifications.
    pub async fn wait_all_receipts(&mut self) {
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            self.drain_events();
            if self
                .participants
                .iter()
                .all(|participant| participant.receipt.is_some())
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timeout waiting for receipt publication: {:?}",
                self.participants
                    .iter()
                    .map(|participant| &participant.events)
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub fn node_count(&self) -> usize {
        self.participants.len()
    }

    pub fn peer_id(&self, i: usize) -> PeerId {
        self.participants[i].peer_id
    }

    pub fn session_hash(&self, i: usize) -> SessionHash {
        self.participants[i].session_hash.expect("session hash set")
    }

    pub fn program(&self) -> &[u8] {
        &self.program
    }

    pub fn params(&self) -> &[u8] {
        &self.params
    }

    /// Return the exact committed activation used by each Host.
    pub fn committed_activation(&self) -> arena0_protocol::Activation {
        self.committed.activation().clone()
    }

    /// Return participant zero's durable receipt header.
    pub fn session_header(&self) -> SessionHeader {
        self.participants[0]
            .receipt
            .as_ref()
            .expect("participant 0 receipt")
            .body()
            .header()
            .clone()
    }

    /// Attempt to construct an activation with only the selected BLS signers.
    pub fn activation_with_activation_signers(
        &self,
        signer_indices: &[usize],
    ) -> Result<arena0_protocol::Activation, arena0_protocol::ActivationError> {
        let activation = self.committed.activation();
        let msg = activation.activation_data().signing_bytes();
        let sigs = signer_indices
            .iter()
            .map(|&i| {
                ExecutionKey::derive(
                    &self.participants[i].execution_salt,
                    &self.participants[i].exec_id.0,
                    &self.participants[i].negotiation_id.0,
                )
                .expect("execution key")
                .sign(&msg)
            })
            .collect::<Vec<_>>();
        let aggregate = BlsSignature::aggregate(&sigs).expect("aggregate");
        arena0_protocol::Activation::new(activation.prepared().clone(), aggregate)
    }

    pub fn completed_outcome(&self, i: usize) -> Option<Vec<u8>> {
        self.participants[i]
            .receipt
            .as_ref()
            .filter(|receipt| {
                matches!(
                    receipt.body().termination(),
                    arena0_protocol::ReceiptTermination::Completed { .. }
                )
            })
            .map(|receipt| receipt.body().outcome().to_vec())
    }

    /// Return participant `i`'s public trace from its durable receipt.
    pub fn trace(&self, i: usize) -> Vec<TraceEntry> {
        self.participants[i]
            .receipt
            .as_ref()
            .map(|receipt| receipt.body().trace().to_vec())
            .unwrap_or_default()
    }

    /// Return a clone of participant `i`'s portable artifact.
    pub fn receipt(&self, i: usize) -> ReceiptArtifact {
        self.participants[i]
            .receipt
            .clone()
            .expect("participant receipt")
    }

    /// Encode participant `i`'s portable artifact for the verifier API.
    pub fn receipt_bytes(&self, i: usize) -> Vec<u8> {
        self.receipt(i).encode().expect("encode receipt")
    }

    /// Replay-verify every participant's complete durable receipt, returning
    /// the verified outcomes in participant order.
    pub fn verify_all(&self, wasm: &[u8]) -> Result<Vec<arena0_verify::VerifiedOutcome>, String> {
        let jobs = (0..self.node_count())
            .map(|i| {
                let receipt = self.participants[i]
                    .receipt
                    .as_ref()
                    .ok_or_else(|| format!("node {i}: missing receipt"))?;
                let bytes = receipt
                    .encode()
                    .map_err(|error| format!("node {i}: {error}"))?;
                Ok((i, bytes, self.session_hash(i)))
            })
            .collect::<Result<Vec<_>, String>>()?;
        std::thread::scope(|scope| {
            let mut replays = Vec::with_capacity(jobs.len());
            for (i, bytes, expected_session) in jobs {
                let timeline = Arc::clone(&self.timeline);
                let timeline_started = self.timeline_started;
                replays.push(scope.spawn(move || {
                    let replay_started = Instant::now();
                    let result = verify_full(wasm, &bytes)
                        .map_err(|error| format!("node {i}: {error:?}"))
                        .and_then(|verified| {
                            if verified.session_id != expected_session {
                                return Err(format!(
                                    "node {i}: verifier returned session {}, expected {}",
                                    verified.session_id, expected_session
                                ));
                            }
                            Ok(verified)
                        });
                    let replay_elapsed_us =
                        u64::try_from(replay_started.elapsed().as_micros()).unwrap_or(u64::MAX);
                    record_progress(
                        &timeline,
                        timeline_started,
                        i,
                        ArenaProgress::ReceiptReplay {
                            elapsed_us: replay_elapsed_us,
                            success: result.is_ok(),
                        },
                    );
                    result
                }));
            }
            replays
                .into_iter()
                .map(|replay| {
                    replay
                        .join()
                        .map_err(|_| "full-replay worker panicked".to_owned())?
                })
                .collect()
        })
    }

    fn drain_events(&mut self) {
        for (i, participant) in self.participants.iter_mut().enumerate() {
            while let Ok(event) = participant.spawned.message_rx.try_recv() {
                record_event(
                    &mut participant.session_hash,
                    &mut participant.termination,
                    &mut participant.receipt,
                    &event,
                );
                record_session_progress(&self.timeline, self.timeline_started, i, &event);
                participant.events.push(event);
            }
        }
    }

    /// Return the test-only monotonic progress timeline.
    pub fn progress_timeline(&self) -> Vec<TimedProgress> {
        self.timeline
            .lock()
            .expect("progress timeline lock")
            .clone()
    }

    fn progress_summary(&self) -> String {
        progress_summary(&self.progress_timeline(), self.node_count())
    }
}

#[allow(missing_debug_implementations)]
pub struct Expect<'a> {
    run: &'a mut Run,
    participant: usize,
}

impl Expect<'_> {
    /// Respond to the expected callout with validated agent-facing JSON bytes.
    pub async fn respond_bytes(self, data: Vec<u8>) {
        let json = JsonBytes::try_new(data).expect("callout response must be valid JSON");
        let deadline = tokio::time::Instant::now() + self.run.timeout;
        loop {
            self.run.drain_events();
            let participant = &mut self.run.participants[self.participant];
            let pending = participant.events.iter().find_map(|event| match event {
                SessionMessage::CalloutRequested {
                    pending_id,
                    callout_index,
                    ..
                } => Some((*pending_id, *callout_index)),
                _ => None,
            });
            if let Some((pending_id, callout_index)) = pending {
                participant
                    .events
                    .retain(|event| !matches!(event, SessionMessage::CalloutRequested { .. }));
                let (tx, rx) = tokio::sync::oneshot::channel();
                participant
                    .spawned
                    .cmd_tx
                    .send(ExecCommand::SubmitInput {
                        pending_id,
                        callout_index,
                        data: json.clone(),
                        reply: tx,
                    })
                    .await
                    .expect("execution gone");
                match rx.await {
                    Ok(Ok(())) => return,
                    Ok(Err(error)) => {
                        panic!("participant {}: input rejected: {error}", self.participant)
                    }
                    Err(_) => panic!("participant {}: execution gone", self.participant),
                }
            }
            if let Some(termination) = &participant.termination {
                panic!(
                    "participant {}: expected CalloutRequested, got termination: {termination:?}",
                    self.participant
                );
            }
            if tokio::time::Instant::now() >= deadline {
                let participant_events = participant.events.clone();
                let mut states = Vec::with_capacity(self.run.participants.len());
                let mut inboxes = Vec::with_capacity(self.run.participants.len());
                for participant in &self.run.participants {
                    states.push(
                        participant
                            .store_handle
                            .load_execution(participant.exec_id)
                            .await
                            .expect("execution state"),
                    );
                    inboxes.push(
                        participant
                            .store_handle
                            .list_pending_inbox(participant.exec_id, 64)
                            .await
                            .expect("pending inbox"),
                    );
                }
                let progress = self.run.progress_summary();
                panic!(
                    "participant {}: timeout waiting for CalloutRequested; progress: {progress}; events: {:?}; states: {states:?}; pending inboxes: {inboxes:?}",
                    self.participant, participant_events,
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

struct ParticipantHandle {
    peer_id: PeerId,
    spawned: SpawnedExec,
    exec_id: ExecId,
    negotiation_id: NegotiationId,
    execution_salt: ExecutionSalt,
    _directory: TempDir,
    _store: Store,
    store_handle: StoreHandle,
    events: Vec<SessionMessage>,
    termination: Option<SessionTermination>,
    session_hash: Option<SessionHash>,
    receipt: Option<ReceiptArtifact>,
}
