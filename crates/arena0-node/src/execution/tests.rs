use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arena0_crypto::bls::BlsSecretKey;
use arena0_crypto::{
    ExecutionKey, ExecutionSalt, NodeKeys, SecretKey, SignScheme, key_binding_message,
};
use arena0_program::{
    CalloutSchema, Capability, JsonBytes, JsonSchemaDocument, ProgramDefinition, ProgramHash,
    ProgramMetadata, ProgramSchema, StateSchema,
};
use arena0_protocol::execution::GuestSignData;
use arena0_protocol::{
    AbortKind, AbortOccurrence, Activation, ActivationData, Ensemble, Event, ExecFrame, ExecId,
    ExecutionAdmission, ExecutionStatus, NegotiationId, Offer, OfferData, PeerId, PeerIdSource,
    PendingId, PreparedActivation, StateHash, Ticket, TicketAction, TicketData, TicketHash,
};
use arena0_sandbox::{InitializeCall, LoadedProgram, Program};
use arena0_store::{Change, Store, StoreConfig};
use arena0_test_engine::shared_test_engine;
use arena0_transport::Transport;
use arena0_transport::local::{LocalNetwork, LocalTransport};
use tempfile::TempDir;
use tokio::sync::mpsc;

use super::ExecutionActor;
use super::guest::{DispatchOutcome, DispatchSource, SubmitInputError};
use crate::context::ExecContext;

const EXEC_ID: ExecId = ExecId([0x44; 32]);
const NEGOTIATION_ID: NegotiationId = NegotiationId([0x11; 32]);

fn guest_wasm(stem: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../programs/target/wasm32-unknown-unknown/release")
        .join(format!("{stem}.wasm"));
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "cannot read required timer guest {}: {error}; run `just build-programs`",
            path.display()
        )
    })
}

#[test]
fn terminal_reason_truncation_preserves_utf8_boundaries() {
    let reason = super::truncate_reason(
        "é".repeat(arena0_protocol::MAX_TERMINAL_REASON_BYTES),
        arena0_protocol::MAX_TERMINAL_REASON_BYTES,
    );
    assert!(reason.len() <= arena0_protocol::MAX_TERMINAL_REASON_BYTES);
    assert!(std::str::from_utf8(reason.as_bytes()).is_ok());
}

#[derive(Clone, Copy)]
enum GuestMode {
    Plain,
    Timer,
    Callout,
    CalloutFault,
    CalloutReject,
    InputSharedChange,
    /// A local input handler that broadcasts a fixed payload size.
    InputBroadcast(u32),
    /// An agreed message handler that broadcasts, used to exercise agreed
    /// outgoing-queue overflow.
    MessageBroadcast,
    LocalSign,
    SignOnMessage,
    Broadcast,
    RejectMessage,
    EndOnMessage,
    EndOnSessionStarted,
}

struct Fixture {
    _directory: TempDir,
    store: Store,
    wasm: Vec<u8>,
    activation: Activation,
    params: JsonBytes,
    local_keys: Arc<NodeKeys>,
    remote_keys: Arc<NodeKeys>,
    local_salt: ExecutionSalt,
    remote_salt: ExecutionSalt,
    local_transport: Arc<LocalTransport>,
    remote_transport: Arc<LocalTransport>,
}

impl Fixture {
    async fn new(remote_is_writer: bool) -> Self {
        Self::with_mode(remote_is_writer, GuestMode::Plain).await
    }

    async fn with_mode(remote_is_writer: bool, mode: GuestMode) -> Self {
        let local_keys = NodeKeys::from_secret(SecretKey::from_bytes([1; 32]));
        let remote_keys = NodeKeys::from_secret(SecretKey::from_bytes([2; 32]));
        let ensemble = Ensemble::from_peers(vec![local_keys.peer_id(), remote_keys.peer_id()])
            .expect("ensemble");
        let writer_peer = if remote_is_writer {
            remote_keys.peer_id()
        } else {
            local_keys.peer_id()
        };
        let writer = ensemble
            .participant_of(&writer_peer)
            .expect("writer participant")
            .index() as u8;
        Self::from_wasm(test_wasm(Some(writer), mode)).await
    }

    async fn with_guest(stem: &str) -> Self {
        Self::from_wasm(guest_wasm(stem)).await
    }

    async fn from_wasm(wasm: Vec<u8>) -> Self {
        Self::from_wasm_with_peer(wasm, None).await
    }

    async fn from_wasm_with_peer(wasm: Vec<u8>, peer: Option<&Self>) -> Self {
        let (local_seed, remote_seed, local_salt, remote_salt) = if peer.is_some() {
            (2, 1, 10, 9)
        } else {
            (1, 2, 9, 10)
        };
        let local_keys = Arc::new(NodeKeys::from_secret(SecretKey::from_bytes(
            [local_seed; 32],
        )));
        let remote_keys = Arc::new(NodeKeys::from_secret(SecretKey::from_bytes(
            [remote_seed; 32],
        )));
        let local_peer = local_keys.peer_id();
        let remote_peer = remote_keys.peer_id();
        let local_salt =
            ExecutionSalt::try_from_bytes([local_salt; 32]).expect("non-zero test salt");
        let remote_salt =
            ExecutionSalt::try_from_bytes([remote_salt; 32]).expect("non-zero test salt");
        let program = Program::parse(wasm.clone()).expect("program");
        let params = JsonBytes::try_new(b"null".to_vec()).expect("params");
        let initialized = shared_test_engine()
            .load(&program)
            .expect("loaded program")
            .initialize(InitializeCall::new(params.clone()))
            .expect("initialize program");
        let activation = peer.map_or_else(
            || {
                activation(
                    program.hash(),
                    local_keys.as_ref(),
                    remote_keys.as_ref(),
                    &local_salt,
                    &remote_salt,
                    StateHash::of_shared(&initialized.shared),
                )
            },
            |peer| peer.activation.clone(),
        );
        let directory = tempfile::tempdir().expect("temporary store directory");
        let path = directory.path().join("execution.sqlite");
        let store = Store::open(StoreConfig::new(path, local_peer)).expect("store");
        let handle = store.handle();
        let (hash, _) = handle
            .register_program(wasm.clone(), 1)
            .await
            .expect("program registration");
        assert_eq!(hash, program.hash());
        let mut writer = handle.claim_execution(EXEC_ID).expect("execution writer");
        writer
            .create_execution_request(
                program.hash(),
                Some(params.clone()),
                peer.map_or_else(
                    || {
                        ExecutionAdmission::explicit(NEGOTIATION_ID, vec![local_peer, remote_peer])
                            .expect("admission")
                    },
                    |peer| ExecutionAdmission::join(peer.local_keys.peer_id(), NEGOTIATION_ID),
                ),
                2,
            )
            .await
            .expect("request");
        let prepared = activation.prepared().clone();
        writer
            .prepare_activation(prepared, 3)
            .await
            .expect("prepare");
        writer
            .commit_activation(activation.clone(), 4)
            .await
            .expect("commit");
        drop(writer);

        let network = LocalNetwork::new();
        let mut transports =
            LocalTransport::create_network(&network, vec![local_peer, remote_peer])
                .expect("attach local transports");
        let local_transport = Arc::new(transports.remove(0));
        let remote_transport = Arc::new(transports.remove(0));
        let (local_transport, remote_transport) =
            peer.map_or((local_transport, remote_transport), |peer| {
                (
                    Arc::clone(&peer.remote_transport),
                    Arc::clone(&peer.local_transport),
                )
            });
        Self {
            _directory: directory,
            store,
            wasm,
            activation,
            params,
            local_keys,
            remote_keys,
            local_salt,
            remote_salt,
            local_transport,
            remote_transport,
        }
    }

    fn loaded_program(&self) -> Arc<LoadedProgram> {
        let program = Program::parse(self.wasm.clone()).expect("program");
        shared_test_engine().load(&program).expect("loaded program")
    }

    fn context(&self) -> ExecContext {
        ExecContext {
            end_confirmation_window: Duration::from_secs(600),
            exec_id: EXEC_ID,
            program: self.loaded_program(),
            params: self.params.clone(),
            activation: self.activation.clone(),
            execution_key: self.local_execution_key(),
        }
    }

    fn local_execution_key(&self) -> ExecutionKey {
        ExecutionKey::derive(&self.local_salt, &EXEC_ID.0, &NEGOTIATION_ID.0).expect("local key")
    }

    fn remote_execution_key(&self) -> ExecutionKey {
        ExecutionKey::derive(&self.remote_salt, &EXEC_ID.0, &NEGOTIATION_ID.0).expect("remote key")
    }

    async fn actor_with_messages(
        &self,
        messages: mpsc::Sender<crate::SessionMessage>,
    ) -> ExecutionActor {
        let mut context = self.context().bind(
            Arc::clone(&self.local_keys),
            self.store
                .handle()
                .claim_execution(EXEC_ID)
                .expect("execution writer"),
            self.local_transport.clone() as Arc<dyn Transport + Sync>,
        );
        let state = ExecutionActor::ensure_execution(&mut context)
            .await
            .expect("execution");
        ExecutionActor {
            context,
            state,
            instance: None,
            messages,
            send_lanes: HashMap::new(),
            send_tasks: tokio::task::JoinSet::new(),
            end_deadline: tokio::time::Instant::now() + Duration::from_secs(600),
            session_started_emitted: false,
            terminal_emitted: false,
            announced_callout: None,
        }
    }

    async fn prepare_active_actor(&self) -> ExecutionActor {
        let (messages, _observations) = mpsc::channel(32);
        self.prepare_active_actor_with_messages(messages).await
    }

    async fn prepare_active_actor_with_messages(
        &self,
        messages: mpsc::Sender<crate::SessionMessage>,
    ) -> ExecutionActor {
        let mut actor = self.actor_with_messages(messages).await;
        let state = actor.state.clone();
        let mut next = state.clone();
        next.activate().expect("activate");
        actor
            .persist(next, Change::Activate)
            .await
            .expect("persist activation");
        actor.restore_resident().expect("restore resident");
        actor
    }

    async fn commit_session_started(&self, actor: &mut ExecutionActor) {
        let accepted = actor
            .dispatch_event(
                Event::SessionStarted {
                    ensemble: actor.ensemble(),
                },
                DispatchSource::Local,
            )
            .await
            .expect("session start dispatch");
        assert_eq!(accepted, DispatchOutcome::Committed);
        actor
            .ensure_step_signature()
            .await
            .expect("local session signature");
        let state = actor.state.clone();
        let proposal = state.proposal_commitment().expect("staged commitment");
        let frame = ExecFrame::StepSignature {
            commitment: proposal.clone(),
            signature: self.remote_execution_key().sign(&proposal.signing_bytes()),
        };
        assert_eq!(
            actor
                .accept_frame(self.remote_keys.peer_id(), frame)
                .await
                .expect("remote session signature"),
            None
        );
    }
}

fn message_state() -> arena0_program::SharedStateBytes {
    arena0_program::SharedStateBytes::try_new(vec![1]).expect("message state")
}

async fn ended_actor() -> (
    Fixture,
    ExecutionActor,
    mpsc::Receiver<crate::SessionMessage>,
) {
    let fixture = Fixture::with_mode(true, GuestMode::EndOnMessage).await;
    let (messages, mut observations) = mpsc::channel(16);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    let frame = terminal_message_frame(
        &actor.state,
        fixture.remote_keys.peer_id(),
        actor.state.agreed_step(),
        b"end".to_vec(),
    );
    assert_eq!(
        actor
            .accept_frame(fixture.remote_keys.peer_id(), frame)
            .await
            .unwrap(),
        None
    );
    actor.ensure_step_signature().await.unwrap();
    let commitment = actor.state.proposal_commitment().unwrap();
    let frame = ExecFrame::StepSignature {
        signature: fixture
            .remote_execution_key()
            .sign(&commitment.signing_bytes()),
        commitment,
    };
    assert_eq!(
        actor
            .accept_frame(fixture.remote_keys.peer_id(), frame)
            .await
            .unwrap(),
        None
    );
    assert!(matches!(
        actor.state.status(),
        ExecutionStatus::Certified { .. }
    ));
    while observations.try_recv().is_ok() {}
    (fixture, actor, observations)
}

#[tokio::test]
async fn rejected_terminal_frame_keeps_peer_unconfirmed_and_actor_alive() {
    let (fixture, mut actor, _observations) = ended_actor().await;
    actor.progress().await.unwrap();
    let recv = fixture
        .remote_transport
        .accept_exec()
        .await
        .unwrap()
        .into_parts()
        .1;
    let delivery = recv.recv_exec().await.unwrap();
    delivery
        .reject(arena0_transport::ExecDeliveryRejection::Rejected)
        .unwrap();
    let result = actor.send_tasks.join_next().await.unwrap().unwrap();
    actor.settle_send(result).await.unwrap();
    actor.progress().await.unwrap();
    assert!(
        actor
            .state
            .end_phase()
            .unconfirmed()
            .unwrap()
            .contains(&fixture.remote_keys.peer_id())
    );
    assert!(
        actor.send_tasks.is_empty(),
        "rejected peer is suppressed for this run"
    );
    assert!(!actor.end_run_finished());
    assert_eq!(
        fixture
            .store
            .handle()
            .list_recovery_candidates(arena0_store::RecoveryCursor::start(), 8)
            .await
            .unwrap()
            .candidates()
            .len(),
        1
    );
}

#[tokio::test]
async fn verified_final_certificate_waits_for_proposal_then_commits_without_rejection() {
    let (_source_fixture, source, _source_observations) = ended_actor().await;
    let certificate = source
        .state
        .current_frames(source.context.producer)
        .into_iter()
        .next()
        .unwrap();
    let fixture = Fixture::with_mode(true, GuestMode::EndOnMessage).await;
    let (messages, _observations) = mpsc::channel(16);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    assert_eq!(actor.state.binding(), source.state.binding());
    let before = actor.state.clone();
    assert_eq!(
        actor
            .accept_frame(fixture.remote_keys.peer_id(), certificate.clone())
            .await
            .unwrap(),
        Some(arena0_transport::ExecDeliveryRejection::NotYet)
    );
    assert_eq!(actor.state, before);
    let message = terminal_message_frame(
        &actor.state,
        fixture.remote_keys.peer_id(),
        actor.state.agreed_step(),
        b"end".to_vec(),
    );
    assert_eq!(
        actor
            .accept_frame(fixture.remote_keys.peer_id(), message)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        actor
            .accept_frame(fixture.remote_keys.peer_id(), certificate.clone())
            .await
            .unwrap(),
        None
    );
    assert!(matches!(
        actor.state.status(),
        ExecutionStatus::Certified { .. }
    ));
    assert_eq!(
        actor
            .accept_frame(fixture.remote_keys.peer_id(), certificate)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn retired_session_router_acknowledges_final_frames() {
    let (fixture, mut actor, _observations) = ended_actor().await;
    actor.finalize_receipt().await.unwrap();
    let frame = actor
        .state
        .current_frames(actor.context.producer)
        .into_iter()
        .next()
        .unwrap();
    drop(actor);
    let host = crate::Host::start(
        fixture.local_keys.clone(),
        fixture.local_transport.clone(),
        fixture.store.handle().clone(),
    );
    let mut spawned = host
        .spawn(fixture.context(), host.claim_execution(EXEC_ID).unwrap())
        .unwrap();
    let outbound = fixture
        .remote_transport
        .accept_exec()
        .await
        .unwrap()
        .into_parts()
        .1;
    outbound.recv_exec().await.unwrap().acknowledge().unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while spawned.message_rx.recv().await.is_some() {}
    })
    .await
    .expect("actor retires after final acknowledgement");

    // Keep the public execution handle alive after its actor has retired.
    // Forwarders must still release the live route for stale acknowledgements.
    let stream = fixture
        .remote_transport
        .open_exec(
            &fixture.local_keys.peer_id(),
            fixture.activation.session_hash(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), stream.send_exec(&frame))
        .await
        .unwrap()
        .expect("retired execution acknowledges stale evidence");
    spawned.shutdown().await;
    host.stop().await;
}

#[tokio::test]
async fn signature_and_message_classification_uses_actor_state() {
    use arena0_transport::ExecDeliveryRejection::{Conflict, NotYet, Rejected};
    let fixture = Fixture::new(true).await;
    let (messages, _observations) = mpsc::channel(16);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    let source = fixture.remote_keys.peer_id();
    let before = actor.state.clone();
    let message = message_frame(&before, source, before.agreed_step(), vec![7]);
    assert_eq!(
        actor.accept_frame(source, message.clone()).await.unwrap(),
        None
    );
    let staged = actor.state.clone();
    assert_eq!(actor.accept_frame(source, message).await.unwrap(), None);
    let conflicting = message_frame(&before, source, before.agreed_step(), vec![8]);
    assert_eq!(
        actor.accept_frame(source, conflicting).await.unwrap(),
        Some(Conflict)
    );
    let commitment = staged.proposal_commitment().unwrap();
    let signature = ExecFrame::StepSignature {
        signature: fixture
            .remote_execution_key()
            .sign(&commitment.signing_bytes()),
        commitment: commitment.clone(),
    };
    assert_eq!(
        actor.accept_frame(source, signature.clone()).await.unwrap(),
        None
    );
    let signed = actor.state.clone();
    // Shutdown proves the remaining classification paths do not load state.
    let remote_key = fixture.remote_execution_key();
    fixture.store.shutdown().await.unwrap();
    assert_eq!(actor.accept_frame(source, signature).await.unwrap(), None);
    let mut mismatch = commitment.clone();
    mismatch.entry_hash[0] ^= 1;
    assert_eq!(
        actor
            .accept_frame(
                source,
                ExecFrame::StepSignature {
                    signature: remote_key.sign(&mismatch.signing_bytes()),
                    commitment: mismatch
                }
            )
            .await
            .unwrap(),
        Some(Rejected)
    );
    let mut future = commitment;
    future.step += 1;
    assert_eq!(
        actor
            .accept_frame(
                source,
                ExecFrame::StepSignature {
                    signature: remote_key.sign(&future.signing_bytes()),
                    commitment: future
                }
            )
            .await
            .unwrap(),
        Some(NotYet)
    );
    assert_eq!(actor.state, signed);
    actor
        .deliver_frames()
        .expect("outbound computation uses actor state");
}

#[tokio::test]
async fn expired_end_wakes_on_unconfirmed_peer_and_simultaneous_evidence_confirms_it() {
    let (fixture, mut actor, _observations) = ended_actor().await;
    actor.finalize_receipt().await.unwrap();
    let frame = actor.state.terminal_evidence().unwrap();
    drop(actor);
    let host = crate::Host::start(
        fixture.local_keys.clone(),
        fixture.local_transport.clone(),
        fixture.store.handle().clone(),
    );
    let mut wakes = host.take_end_wakes().unwrap();
    let mut context = fixture.context();
    context.end_confirmation_window = Duration::ZERO;
    let mut spawned = host
        .spawn(context, host.claim_execution(EXEC_ID).unwrap())
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while spawned.message_rx.recv().await.is_some() {}
    })
    .await
    .expect("window expires without waiting for the silent peer");
    spawned.shutdown().await;
    let (_, end) = fixture
        .store
        .handle()
        .execution_end(fixture.activation.session_hash())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(end, arena0_protocol::EndPhase::Ended { unconfirmed } if unconfirmed.contains(&fixture.remote_keys.peer_id()))
    );
    assert!(
        fixture
            .store
            .handle()
            .list_recovery_candidates(arena0_store::RecoveryCursor::start(), 8)
            .await
            .unwrap()
            .is_empty()
    );

    let open = || async {
        fixture
            .remote_transport
            .open_exec(
                &fixture.local_keys.peer_id(),
                fixture.activation.session_hash(),
            )
            .await
    };
    let stream = open().await.unwrap();
    assert!(matches!(
        stream.send_exec(&frame).await,
        Err(arena0_transport::TransportError::ExecNotYet)
    ));
    let wake = tokio::time::timeout(Duration::from_secs(2), wakes.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(wake, EXEC_ID);
    // The embedding supervisor reconstructs through the same public spawn
    // boundary used at startup, after the router has returned NotYet.
    let mut resumed = host
        .spawn(fixture.context(), host.claim_execution(wake).unwrap())
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        open().await.unwrap().send_exec(&frame).await.unwrap();
    })
    .await
    .expect("matching evidence is acknowledged across retirement");
    tokio::time::timeout(Duration::from_secs(2), async {
        while resumed.message_rx.recv().await.is_some() {}
    })
    .await
    .expect("matching peer evidence confirms without an outbound ack");
    resumed.shutdown().await;
    let (_, end) = fixture
        .store
        .handle()
        .execution_end(fixture.activation.session_hash())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(end, arena0_protocol::EndPhase::Ended { unconfirmed } if unconfirmed.is_empty())
    );
    open()
        .await
        .unwrap()
        .send_exec(&frame)
        .await
        .expect("confirmed peer is stale");
    host.stop().await;
}

#[tokio::test]
async fn authenticated_abort_is_deferred_adopted_or_stale() {
    use arena0_transport::ExecDeliveryRejection::NotYet;
    let fixture = Fixture::new(true).await;
    let (messages, _observations) = mpsc::channel(16);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    let source = fixture.remote_keys.peer_id();
    let frame = |cursor| {
        let unsigned = AbortOccurrence::unsigned(
            fixture.activation.session_hash(),
            source,
            AbortKind::Abort,
            1,
            "peer stop",
            cursor,
        )
        .unwrap();
        let signature = fixture.remote_keys.sign(&unsigned.signing_bytes().unwrap());
        ExecFrame::Abort {
            occurrence: unsigned.with_signature(signature).unwrap(),
        }
    };
    let before = actor.state.clone();
    let ahead = arena0_protocol::StepCursor::new(
        before.agreed_step() + 1,
        before.agreed_state(),
        before.agreed_link(),
    );
    assert_eq!(
        actor.accept_frame(source, frame(ahead)).await.unwrap(),
        Some(NotYet)
    );
    assert_eq!(actor.state, before);
    let valid = frame(before.step_cursor());
    assert_eq!(
        actor
            .accept_frame(fixture.local_keys.peer_id(), valid.clone())
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        actor.accept_frame(source, frame(ahead)).await.unwrap(),
        Some(arena0_transport::ExecDeliveryRejection::Conflict)
    );
    assert!(
        actor
            .state
            .end_phase()
            .unconfirmed()
            .unwrap()
            .contains(&source)
    );
    let alternative = AbortOccurrence::unsigned(
        fixture.activation.session_hash(),
        source,
        AbortKind::Fail,
        2,
        "independent stop report",
        before.step_cursor(),
    )
    .unwrap();
    let signature = fixture
        .remote_keys
        .sign(&alternative.signing_bytes().unwrap());
    assert_eq!(
        actor
            .accept_frame(
                source,
                ExecFrame::Abort {
                    occurrence: alternative.with_signature(signature).unwrap(),
                }
            )
            .await
            .unwrap(),
        None
    );
    assert!(actor.state.end_phase().unconfirmed().unwrap().is_empty());
    assert_eq!(
        actor.accept_frame(source, valid.clone()).await.unwrap(),
        None
    );
    assert!(actor.state.status().is_terminal());
    let stopped = actor.state.clone();
    assert_eq!(actor.accept_frame(source, valid).await.unwrap(), None);
    assert_eq!(actor.state, stopped);
}

#[tokio::test]
async fn certificate_authentication_rejects_bad_evidence_but_local_contradiction_errors() {
    let (_source_fixture, source, _observations) = ended_actor().await;
    let certificate = source
        .state
        .current_frames(source.context.producer)
        .into_iter()
        .next()
        .unwrap();
    let fixture = Fixture::with_mode(true, GuestMode::EndOnMessage).await;
    let (messages, _observations) = mpsc::channel(16);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    let mut raw = arena0_wire::ExecFrame::try_from(&certificate).unwrap();
    if let arena0_wire::ExecFrame::StepCertificate { aggregate, .. } = &mut raw {
        aggregate.0[0] ^= 1;
    }
    assert_eq!(
        actor
            .accept_frame(
                fixture.remote_keys.peer_id(),
                ExecFrame::try_from(raw).unwrap()
            )
            .await
            .unwrap(),
        Some(arena0_transport::ExecDeliveryRejection::Rejected)
    );
    let different = terminal_message_frame(
        &actor.state,
        fixture.remote_keys.peer_id(),
        actor.state.agreed_step(),
        b"different".to_vec(),
    );
    actor
        .accept_frame(fixture.remote_keys.peer_id(), different)
        .await
        .unwrap();
    assert!(matches!(
        actor
            .accept_frame(fixture.remote_keys.peer_id(), certificate)
            .await,
        Err(crate::ExecError::DeliveryInvariant(_))
    ));
}

fn message_commitment(
    state: &arena0_protocol::ExecutionState,
    source: PeerId,
    step: u64,
    pre_state: StateHash,
    post_state: StateHash,
    data: &[u8],
) -> arena0_protocol::StepCommitment {
    message_commitment_with_terminal(state, source, step, pre_state, post_state, None, data)
}

#[allow(clippy::too_many_arguments)]
fn message_commitment_with_terminal(
    state: &arena0_protocol::ExecutionState,
    source: PeerId,
    step: u64,
    pre_state: StateHash,
    post_state: StateHash,
    terminal: Option<arena0_protocol::StepTerminal>,
    data: &[u8],
) -> arena0_protocol::StepCommitment {
    let entry = arena0_protocol::TraceEntry {
        trace_version: arena0_protocol::TRACE_FORMAT_VERSION,
        step,
        event: arena0_protocol::StepEvent::Message {
            from: source,
            data: data.to_vec(),
        },
        pre_state,
        post_state,
        terminal,
        agreement: arena0_protocol::AggregateAttestation::empty(),
    };
    arena0_protocol::StepCommitment::for_entry(
        state.binding().session_id(),
        &entry,
        state.agreed_link(),
    )
}

fn message_frame(
    state: &arena0_protocol::ExecutionState,
    source: PeerId,
    sequence: u64,
    data: Vec<u8>,
) -> ExecFrame {
    let poststate = StateHash::of_shared(&message_state());
    let commitment = message_commitment(
        state,
        source,
        sequence,
        state.agreed_state(),
        poststate,
        &data,
    );
    ExecFrame::Message { commitment, data }
}

/// A message frame whose step ends the session with an empty outcome, matching
/// the `EndOnMessage` fixture.
fn terminal_message_frame(
    state: &arena0_protocol::ExecutionState,
    source: PeerId,
    sequence: u64,
    data: Vec<u8>,
) -> ExecFrame {
    let poststate = StateHash::of_shared(&message_state());
    let commitment = message_commitment_with_terminal(
        state,
        source,
        sequence,
        state.agreed_state(),
        poststate,
        Some(arena0_protocol::StepTerminal::End { outcome: vec![] }),
        &data,
    );
    ExecFrame::Message { commitment, data }
}

#[tokio::test]
async fn rejected_writer_leaves_durable_state_unchanged() {
    let fixture = Fixture::new(false).await;
    let mut actor = fixture.prepare_active_actor().await;
    let before = fixture
        .store
        .handle()
        .load_execution(EXEC_ID)
        .await
        .expect("load before")
        .expect("execution before");
    let frame = message_frame(
        &before,
        fixture.remote_keys.peer_id(),
        before.agreed_step(),
        Vec::new(),
    );

    let applied = actor
        .apply_message(fixture.remote_keys.peer_id(), frame)
        .await
        .expect("drop wrong writer");
    assert!(!applied);

    let after = fixture
        .store
        .handle()
        .load_execution(EXEC_ID)
        .await
        .expect("load after")
        .expect("execution after");
    assert_eq!(before, after);
}

#[tokio::test]
async fn host_rejects_a_duplicate_actor_for_one_execution() {
    let fixture = Fixture::new(false).await;
    let host = crate::Host::start(
        Arc::clone(&fixture.local_keys),
        fixture.local_transport.clone() as Arc<dyn Transport + Sync>,
        fixture.store.handle().clone(),
    );
    let lifecycle = host.claim_execution(EXEC_ID).expect("lifecycle claim");
    let duplicate = host
        .claim_execution(EXEC_ID)
        .expect_err("duplicate lifecycle owner during negotiation");
    assert!(matches!(
        duplicate,
        crate::HostError::DuplicateExecution(exec_id) if exec_id == EXEC_ID
    ));
    let first = host
        .spawn(fixture.context(), lifecycle)
        .expect("first actor");
    let duplicate = host.claim_execution(EXEC_ID).expect_err("duplicate actor");
    assert!(matches!(
        duplicate,
        crate::HostError::DuplicateExecution(exec_id) if exec_id == EXEC_ID
    ));
    drop(first);
    tokio::task::yield_now().await;
    let reclaimed = host
        .claim_execution(EXEC_ID)
        .expect("actor drop releases lifecycle claim");
    drop(reclaimed);
    host.stop().await;
}

#[tokio::test]
async fn valid_writer_message_divergence_preserves_state_until_failure_is_persisted() {
    for (mode, cause) in [
        (
            GuestMode::RejectMessage,
            "program rejected the writer message",
        ),
        (GuestMode::SignOnMessage, "message handler trapped"),
        (GuestMode::Plain, "post-state mismatch"),
    ] {
        let fixture = Fixture::with_mode(true, mode).await;
        let (messages, _observations) = mpsc::channel(8);
        let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
        fixture.commit_session_started(&mut actor).await;
        let before = actor.state.clone();
        let source = fixture.remote_keys.peer_id();
        // The identity is valid even when the advertised result is wrong.
        let data = b"writer message".to_vec();
        let commitment = message_commitment(
            &before,
            source,
            before.agreed_step(),
            before.agreed_state(),
            before.agreed_state(),
            &data,
        );
        let frame = ExecFrame::Message { commitment, data };
        let error = actor
            .accept_frame(source, frame)
            .await
            .expect_err("divergence");
        let crate::ExecError::Diverged(reason) = &error else {
            panic!("expected divergence, got {error:?}");
        };
        assert!(reason.starts_with("diverged at step 1:"), "{reason}");
        assert!(reason.contains(cause), "{reason}");
        assert!(reason.len() <= arena0_protocol::MAX_TERMINAL_REASON_BYTES);
        // A rejection, trap, or commitment mismatch persists nothing, so no
        // proposal can be signed after a restart.
        assert_eq!(actor.state, before);
        assert!(
            actor.fail_terminal(error).await,
            "failure must permit final delivery"
        );
        let stopped = actor.state.clone();
        assert!(stopped.status().is_terminal());
        assert_eq!(stopped.step_cursor(), before.step_cursor());
    }
}

#[tokio::test]
async fn terminal_only_commitment_mismatch_diverges_before_signing() {
    let fixture = Fixture::with_mode(true, GuestMode::Plain).await;
    let (messages, _observations) = mpsc::channel(8);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    let before = actor.state.clone();
    let source = fixture.remote_keys.peer_id();
    let data = b"terminal only".to_vec();
    let poststate = StateHash::of_shared(&message_state());
    // The guest's post-state matches; only the terminal differs.
    let commitment = message_commitment_with_terminal(
        &before,
        source,
        before.agreed_step(),
        before.agreed_state(),
        poststate,
        Some(arena0_protocol::StepTerminal::End { outcome: vec![] }),
        &data,
    );
    let frame = ExecFrame::Message { commitment, data };
    let error = actor
        .accept_frame(source, frame)
        .await
        .expect_err("divergence");
    let crate::ExecError::Diverged(reason) = &error else {
        panic!("expected divergence, got {error:?}");
    };
    assert!(reason.contains("entry mismatch"), "{reason}");
    // Nothing is persisted, so the receiver has no proposal to sign.
    assert_eq!(actor.state, before);
    assert!(actor.state.pending_shared().is_none());
    assert!(actor.fail_terminal(error).await);
    assert!(actor.state.status().is_terminal());
    assert_eq!(actor.state.agreed_step(), before.agreed_step());
}

#[tokio::test]
async fn a_commitment_mismatch_leaves_no_proposal_for_recovery_to_sign() {
    let fixture = Fixture::with_mode(true, GuestMode::Plain).await;
    let (messages, _observations) = mpsc::channel(8);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    let before = actor.state.clone();
    let source = fixture.remote_keys.peer_id();
    let data = b"terminal only".to_vec();
    let poststate = StateHash::of_shared(&message_state());
    let commitment = message_commitment_with_terminal(
        &before,
        source,
        before.agreed_step(),
        before.agreed_state(),
        poststate,
        Some(arena0_protocol::StepTerminal::End { outcome: vec![] }),
        &data,
    );
    let frame = ExecFrame::Message { commitment, data };
    assert!(matches!(
        actor.accept_frame(source, frame).await,
        Err(crate::ExecError::Diverged(_))
    ));
    // No terminal stop is persisted yet; recover the actor from the store.
    drop(actor);
    let (messages, _observations) = mpsc::channel(8);
    let mut recovered = fixture.actor_with_messages(messages).await;
    recovered.recover().await.expect("recover");
    assert!(recovered.state.pending_shared().is_none());
    assert_eq!(recovered.state.agreed_step(), before.agreed_step());
    // Recovery has nothing to sign.
    recovered
        .ensure_step_signature()
        .await
        .expect("no proposal to sign");
    assert!(recovered.state.pending_shared().is_none());
}

#[tokio::test]
async fn invalid_messages_are_dropped_before_the_guest_can_diverge() {
    for invalid in ["stale", "prestate", "writer"] {
        let fixture = Fixture::with_mode(invalid != "writer", GuestMode::RejectMessage).await;
        let (messages, _observations) = mpsc::channel(8);
        let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
        fixture.commit_session_started(&mut actor).await;
        let before = actor.state.clone();
        let source = fixture.remote_keys.peer_id();
        let seq = if invalid == "stale" {
            before.agreed_step() - 1
        } else {
            before.agreed_step()
        };
        let prestate = if invalid == "prestate" {
            StateHash([0xff; 32])
        } else {
            before.agreed_state()
        };
        let poststate = StateHash::of_shared(&message_state());
        let data = b"invalid frame".to_vec();
        let commitment = message_commitment(&before, source, seq, prestate, poststate, &data);
        let frame = ExecFrame::Message { commitment, data };
        let decision = actor
            .accept_frame(source, frame)
            .await
            .expect("classification");
        assert_eq!(
            decision,
            if invalid == "stale" {
                None
            } else {
                Some(arena0_transport::ExecDeliveryRejection::Rejected)
            }
        );
        assert_eq!(actor.state.clone(), before, "{invalid}");
    }
}

#[tokio::test]
async fn host_rejects_a_lifecycle_token_issued_by_another_host() {
    let fixture = Fixture::new(false).await;
    let host = crate::Host::start(
        Arc::clone(&fixture.local_keys),
        fixture.local_transport.clone() as Arc<dyn Transport + Sync>,
        fixture.store.handle().clone(),
    );
    let other_host = crate::Host::start(
        Arc::clone(&fixture.local_keys),
        fixture.local_transport.clone() as Arc<dyn Transport + Sync>,
        fixture.store.handle().clone(),
    );
    let lifecycle = host.claim_execution(EXEC_ID).expect("lifecycle claim");
    assert!(matches!(
        other_host.spawn(fixture.context(), lifecycle),
        Err(crate::HostError::ExecutionStoreHostMismatch)
    ));
    drop(
        host.claim_execution(EXEC_ID)
            .expect("failed cross-host handoff releases claim"),
    );
    host.stop().await;
    other_host.stop().await;
}

#[tokio::test]
async fn spawned_execution_keeps_host_router_alive_after_host_arc_drop() {
    let fixture = Fixture::new(true).await;
    let host = crate::Host::start(
        Arc::clone(&fixture.local_keys),
        fixture.local_transport.clone() as Arc<dyn Transport + Sync>,
        fixture.store.handle().clone(),
    );
    let lifecycle = host.claim_execution(EXEC_ID).expect("lifecycle claim");
    let mut spawned = host
        .spawn(fixture.context(), lifecycle)
        .expect("spawn actor");
    drop(host);

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match spawned.message_rx.recv().await {
                Some(crate::SessionMessage::SessionStarted { .. }) => break,
                Some(crate::SessionMessage::Failed { reason }) => {
                    panic!("actor failed before session start: {reason}")
                }
                Some(_) => {}
                None => panic!("observation channel closed before session start"),
            }
        }
    })
    .await
    .expect("session start observation");

    let state = fixture
        .store
        .handle()
        .load_execution(EXEC_ID)
        .await
        .expect("load execution")
        .expect("execution state");
    let commitment = state.proposal_commitment().unwrap();
    let frame = ExecFrame::StepSignature {
        signature: fixture
            .remote_execution_key()
            .sign(&commitment.signing_bytes()),
        commitment,
    };
    let send = fixture
        .remote_transport
        .open_exec(
            &fixture.local_keys.peer_id(),
            fixture.activation.session_hash(),
        )
        .await
        .expect("open inbound stream");
    tokio::time::timeout(Duration::from_secs(5), send.send_exec(&frame))
        .await
        .expect("transport acknowledgement timeout")
        .expect("durable inbound acknowledgement");

    let committed = fixture
        .store
        .handle()
        .load_execution(EXEC_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.agreed_step(), 1);
    spawned.shutdown().await;
}

#[tokio::test]
async fn flat_dispatch_commits_local_state_in_the_resident() {
    let fixture = Fixture::new(false).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;

    let accepted = actor
        .dispatch_event(
            Event::TimerFired {
                timer: arena0_protocol::TimerPayload::unit(),
            },
            DispatchSource::Local,
        )
        .await
        .expect("dispatch local event");
    assert_eq!(accepted, DispatchOutcome::Committed);
    let state = actor.state.clone();
    assert_eq!(state.local_state().as_bytes(), &[9]);
    let instance = actor.instance.as_ref().expect("resident instance");
    assert_eq!(instance.committed_payloads().1.as_bytes(), &[9]);
}

#[tokio::test]
async fn restart_resumes_a_durable_timer_and_accepts_a_message() {
    let fixture = Fixture::with_mode(true, GuestMode::Timer).await;
    let mut actor = fixture.prepare_active_actor().await;
    // The fixture arms its timer from the agreed session boundary.
    fixture.commit_session_started(&mut actor).await;
    drop(actor);

    let (messages, _observations) = mpsc::channel(32);
    let mut restarted = fixture.actor_with_messages(messages).await;
    restarted
        .fire_due_timers()
        .await
        .expect("fire recovered timer");
    assert!(
        restarted
            .context
            .store
            .due_timers(super::now_ms(), 16)
            .await
            .expect("load timers")
            .is_empty()
    );

    let state = restarted.state.clone();
    assert_eq!(state.local_state().as_bytes(), &[8]);
    let source = fixture.remote_keys.peer_id();
    let frame = message_frame(&state, source, state.agreed_step(), vec![1, 2, 3]);
    restarted
        .accept_frame(source, frame)
        .await
        .expect("accept inbound");

    let resolved = fixture
        .store
        .handle()
        .load_execution(EXEC_ID)
        .await
        .expect("load resolved state")
        .expect("resolved state");
    assert!(resolved.pending_shared().is_some());
}

#[tokio::test]
async fn recovered_sdk_timers_dispatch_typed_and_unit_payloads() {
    for stem in ["timer_dispatch_typed", "timer_dispatch_unit"] {
        let fixture = Fixture::with_guest(stem).await;
        let mut actor = fixture.prepare_active_actor().await;
        // The fixture arms its timer from the agreed session boundary.
        fixture.commit_session_started(&mut actor).await;
        drop(actor);

        let (messages, _observations) = mpsc::channel(8);
        let mut restarted = fixture.actor_with_messages(messages).await;
        restarted
            .fire_due_timers()
            .await
            .expect("fire recovered timer");

        let state = restarted.state.clone();
        assert_eq!(state.local_state().as_bytes(), &[1], "{stem} timer handler");
        assert!(
            restarted
                .context
                .store
                .due_timers(super::now_ms(), 16)
                .await
                .expect("load timers")
                .is_empty(),
            "{stem} timer row"
        );
    }
}

#[tokio::test]
async fn restart_reannounces_committed_callout_once() {
    let fixture = Fixture::with_mode(false, GuestMode::Callout).await;
    let (first_messages, mut first_observations) = mpsc::channel(8);
    let mut actor = fixture
        .prepare_active_actor_with_messages(first_messages)
        .await;
    fixture.commit_session_started(&mut actor).await;
    let pending_id = {
        assert_eq!(
            actor
                .dispatch_event(
                    Event::TimerFired {
                        timer: arena0_protocol::TimerPayload::unit()
                    },
                    DispatchSource::Local
                )
                .await
                .expect("dispatch callout"),
            DispatchOutcome::Committed
        );
        actor.state.clone().callout().expect("pending callout").id
    };
    actor.progress().await.expect("deliver initial callout");
    let initial = std::iter::from_fn(|| first_observations.try_recv().ok())
        .find(|message| matches!(message, crate::SessionMessage::CalloutRequested { .. }))
        .expect("initial callout message");
    assert!(matches!(
        initial,
        crate::SessionMessage::CalloutRequested {
            pending_id: id,
            callout_index: 0,
            context,
            ..
        } if id == pending_id
            && context == b"null"
    ));
    drop(actor);

    let (messages, mut observations) = mpsc::channel(8);
    let mut restarted = fixture.actor_with_messages(messages).await;
    restarted.recover().await.expect("recover open callout");
    let mut found = None;
    while let Ok(message) = observations.try_recv() {
        if let crate::SessionMessage::CalloutRequested { .. } = message {
            found = Some(message);
            break;
        }
    }
    let message = found.expect("recovered callout message");
    restarted.progress().await.expect("repeat progress");
    while let Ok(message) = observations.try_recv() {
        assert!(!matches!(
            message,
            crate::SessionMessage::CalloutRequested { .. }
        ));
    }
    assert!(matches!(
        message,
        crate::SessionMessage::CalloutRequested {
            pending_id: id,
            callout_index: 0,
            context,
            ..
        } if id == pending_id
            && context == b"null"
    ));
}

#[tokio::test]
async fn staged_result_preserves_callout_and_reports_agreement_pending() {
    let fixture = Fixture::with_mode(true, GuestMode::Callout).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;
    let answer = JsonBytes::try_new(b"null".to_vec()).unwrap();
    assert!(matches!(
        actor.submit_input(PendingId::new(0), answer.clone()).await,
        Err(SubmitInputError::Expected(
            crate::ExecError::CalloutNotPending
        ))
    ));
    actor
        .dispatch_event(
            Event::TimerFired {
                timer: arena0_protocol::TimerPayload::unit(),
            },
            DispatchSource::Local,
        )
        .await
        .unwrap();
    let before = actor.state.clone();
    let open = before.callout().unwrap().clone();
    let source = fixture.remote_keys.peer_id();
    let frame = message_frame(&before, source, before.agreed_step(), vec![1, 2, 3]);
    actor.accept_frame(source, frame).await.unwrap();

    let staged = actor.state.clone();
    assert!(staged.pending_shared().is_some());
    assert_eq!(staged.callout(), Some(&open));
    assert!(matches!(
        actor.submit_input(open.id, answer.clone()).await,
        Err(SubmitInputError::Expected(
            crate::ExecError::AgreementPending
        ))
    ));
    assert!(matches!(
        actor
            .submit_input(PendingId::new(open.id.get().wrapping_add(1)), answer)
            .await,
        Err(SubmitInputError::Expected(
            crate::ExecError::CalloutNotPending
        ))
    ));
    assert_eq!(actor.state.clone(), staged);
}

#[tokio::test]
async fn rejected_callout_answer_preserves_pending_continuation_until_valid_input() {
    let fixture = Fixture::with_mode(false, GuestMode::CalloutReject).await;
    let (messages, _observations) = mpsc::channel(32);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    assert_eq!(
        actor
            .dispatch_event(
                Event::TimerFired {
                    timer: arena0_protocol::TimerPayload::unit()
                },
                DispatchSource::Local
            )
            .await
            .expect("dispatch callout"),
        DispatchOutcome::Committed
    );

    let before = actor.state.clone();
    let pending_id = before.callout().expect("pending callout").id;
    let result = actor
        .submit_input(
            pending_id,
            JsonBytes::try_new(b"null".to_vec()).expect("answer"),
        )
        .await;
    assert!(matches!(
        result,
        Err(SubmitInputError::Expected(crate::ExecError::InputRejected(reason)))
            if reason == "input handler rejected the answer"
    ));

    let after_rejection = actor.state.clone();
    assert_eq!(after_rejection.version(), before.version());
    assert_eq!(after_rejection.event_position(), before.event_position());
    assert_eq!(after_rejection.shared_state(), before.shared_state());
    assert_eq!(after_rejection.local_state(), before.local_state());
    assert_eq!(after_rejection.callout(), before.callout());
    assert_eq!(
        after_rejection
            .callout()
            .expect("pending after rejection")
            .id,
        pending_id
    );
    assert!(!after_rejection.status().is_terminal());

    actor
        .submit_input(
            pending_id,
            JsonBytes::try_new(b"true".to_vec()).expect("valid answer"),
        )
        .await
        .expect("valid answer after rejection");
    let committed = actor.state.clone();
    assert!(committed.callout().is_none());
    assert!(!committed.status().is_terminal());
    // Exactly the accepted result committed: one transition past the
    // rejection snapshot, whose images the rejection left untouched.
    assert_eq!(
        committed.version(),
        before.version().next().expect("version")
    );
    assert_eq!(committed.event_position(), before.event_position() + 1);
}

#[tokio::test]
async fn input_handler_trap_rejects_without_ending_session() {
    let fixture = Fixture::with_mode(false, GuestMode::CalloutFault).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;
    assert_eq!(
        actor
            .dispatch_event(
                Event::TimerFired {
                    timer: arena0_protocol::TimerPayload::unit()
                },
                DispatchSource::Local
            )
            .await
            .expect("dispatch callout"),
        DispatchOutcome::Committed
    );
    let before = fixture
        .store
        .handle()
        .load_execution(EXEC_ID)
        .await
        .expect("load state before trap")
        .expect("execution before trap");
    let pending_id = before.callout().expect("callout pending").id;
    assert_eq!(
        before.callout().expect("callout remains pending").id,
        pending_id
    );
    assert_eq!(before.shared_state().as_bytes(), &[] as &[u8]);
    assert_eq!(before.local_state().as_bytes(), &[9]);

    let result = actor
        .submit_input(
            pending_id,
            JsonBytes::try_new(b"null".to_vec()).expect("answer"),
        )
        .await;
    assert!(matches!(
        result,
        Err(SubmitInputError::Expected(crate::ExecError::InputRejected(reason)))
            if reason.starts_with("input handler trapped:")
                && reason.len() <= arena0_program::MAX_REJECTION_REASON_BYTES
    ));

    let after = fixture
        .store
        .handle()
        .load_execution(EXEC_ID)
        .await
        .expect("load state after trap")
        .expect("execution after trap");
    assert_eq!(after.shared_state(), before.shared_state());
    assert_eq!(after.local_state(), before.local_state());
    assert_eq!(after.version(), before.version());
    assert_eq!(after.event_position(), before.event_position());
    assert_eq!(after.callout(), before.callout());
    assert_eq!(after.callout().expect("pending after trap").id, pending_id);
    assert!(!after.status().is_terminal());
}

#[tokio::test]
async fn receipt_budget_failure_publishes_a_stop_report_that_survives_restart() {
    let fixture = Fixture::new(true).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;
    loop {
        let before = actor.state.clone();
        let frame = message_frame(
            &before,
            fixture.remote_keys.peer_id(),
            before.agreed_step(),
            vec![1; arena0_protocol::MAX_EFFECT_PAYLOAD_BYTES],
        );
        let result = actor
            .apply_message(fixture.remote_keys.peer_id(), frame)
            .await;
        if let Err(error @ crate::ExecError::ReceiptBudgetExhausted { .. }) = result {
            assert_eq!(actor.state.clone(), before);
            assert!(before.agreed_step() > 1);
            assert!(actor.fail_terminal(error).await);
            break;
        }
        assert!(result.expect("dispatch within budget"));
        actor
            .ensure_step_signature()
            .await
            .expect("local signature");
        let staged = actor.state.clone();
        let commitment = staged.proposal_commitment().unwrap();
        actor
            .accept_frame(
                fixture.remote_keys.peer_id(),
                ExecFrame::StepSignature {
                    signature: fixture
                        .remote_execution_key()
                        .sign(&commitment.signing_bytes()),
                    commitment,
                },
            )
            .await
            .expect("accept peer signature");

        assert!(actor.state.clone().pending_shared().is_none());
    }
    let stopped = actor.state.clone();
    let receipt_id = stopped
        .published_receipt_id()
        .expect("published failure report");
    let stored = fixture
        .store
        .handle()
        .load_receipt_by_id(receipt_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.receipt.kind(),
        arena0_protocol::ReceiptKind::StopReport
    );
    let arena0_protocol::ReceiptTermination::Stopped {
        cause: arena0_protocol::StopCause::Authenticated(occurrence),
    } = stored.receipt.body().termination()
    else {
        panic!("authenticated failure")
    };
    assert_eq!(occurrence.kind(), AbortKind::Fail);
    assert_eq!(
        occurrence.reason(),
        format!("receipt budget exhausted at step {}", stopped.agreed_step())
    );
    assert_eq!(*occurrence.coordinate(), stopped.step_cursor());
    let bytes = stored.receipt.encode().expect("bounded stop report");
    assert!(bytes.len() <= arena0_protocol::MAX_RECEIPT_BYTES);
    arena0_protocol::ReceiptArtifact::decode(&bytes).expect("authenticated prefix verifies");
    drop(actor);
    fixture.store.shutdown().await.expect("shutdown");
    let reopened = Store::open(StoreConfig::new(
        fixture._directory.path().join("execution.sqlite"),
        fixture.local_keys.peer_id(),
    ))
    .expect("reopen bounded report");
    assert_eq!(
        reopened
            .handle()
            .load_execution(EXEC_ID)
            .await
            .unwrap()
            .unwrap(),
        stopped
    );
    assert_eq!(
        reopened
            .handle()
            .load_receipt_by_id(receipt_id)
            .await
            .unwrap()
            .unwrap()
            .receipt
            .encode()
            .unwrap(),
        bytes,
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn terminal_publication_precedes_delivery_and_restart_resends_final_frames() {
    let (fixture, mut actor, mut observations) = ended_actor().await;
    actor
        .progress()
        .await
        .expect("publish and start final delivery");
    assert!(matches!(
        observations.recv().await,
        Some(crate::SessionMessage::ReceiptPublished { .. })
    ));
    assert!(matches!(
        observations.recv().await,
        Some(crate::SessionMessage::Completed { .. })
    ));
    assert!(!actor.end_run_finished());
    let recv = fixture
        .remote_transport
        .accept_exec()
        .await
        .unwrap()
        .into_parts()
        .1;
    let delivery = recv.recv_exec().await.unwrap();
    let frame = delivery.frame().clone();
    assert!(matches!(frame, ExecFrame::StepCertificate { .. }));
    drop(delivery);
    drop(recv);
    drop(actor);

    let (messages, mut observations) = mpsc::channel(16);
    let mut restarted = fixture.actor_with_messages(messages).await;
    restarted.recover().await.expect("resume final delivery");
    assert!(matches!(
        observations.recv().await,
        Some(crate::SessionMessage::ReceiptPublished { .. })
    ));
    assert!(matches!(
        observations.recv().await,
        Some(crate::SessionMessage::Completed { .. })
    ));
    let recv = fixture
        .remote_transport
        .accept_exec()
        .await
        .unwrap()
        .into_parts()
        .1;
    let delivery = recv.recv_exec().await.unwrap();
    assert_eq!(delivery.frame(), &frame);
    delivery.acknowledge().unwrap();
    let result = restarted.send_tasks.join_next().await.unwrap().unwrap();
    restarted.settle_send(result).await.unwrap();
    restarted.progress().await.unwrap();
    assert!(restarted.end_run_finished());
    assert!(
        fixture
            .store
            .handle()
            .list_recovery_candidates(arena0_store::RecoveryCursor::start(), 8)
            .await
            .unwrap()
            .candidates()
            .is_empty()
    );
}

#[tokio::test]
async fn stale_abort_is_acked_across_restart_and_signed_proposal_still_commits() {
    let fixture = Fixture::with_mode(false, GuestMode::Broadcast).await;
    let mut actor = fixture.prepare_active_actor().await;
    let stale_cursor = actor.state.clone().step_cursor();
    fixture.commit_session_started(&mut actor).await;
    assert_eq!(
        actor
            .dispatch_event(
                Event::TimerFired {
                    timer: arena0_protocol::TimerPayload::unit()
                },
                DispatchSource::Local
            )
            .await
            .expect("queue broadcast"),
        DispatchOutcome::Committed
    );
    actor
        .author_next_message()
        .await
        .expect("author queued message");
    actor
        .ensure_step_signature()
        .await
        .expect("sign local proposal");
    let proposed = actor.state.clone();
    let proposal = proposed
        .pending_shared()
        .expect("pending proposal after local signature");
    assert_eq!(proposal.signature_count(), 1);
    let commitment = proposed.proposal_commitment().expect("staged commitment");

    let unsigned = AbortOccurrence::unsigned(
        fixture.activation.session_hash(),
        fixture.remote_keys.peer_id(),
        AbortKind::Abort,
        17,
        "stale remote stop",
        stale_cursor,
    )
    .expect("abort occurrence");
    let occurrence = unsigned
        .clone()
        .with_signature(
            fixture
                .remote_keys
                .sign(&unsigned.signing_bytes().expect("abort signing bytes")),
        )
        .expect("signed abort");
    actor
        .accept_frame(
            fixture.remote_keys.peer_id(),
            ExecFrame::Abort { occurrence },
        )
        .await
        .expect("accept stale abort");
    drop(actor);

    let (messages, _observations) = mpsc::channel(32);
    let mut restarted = fixture.actor_with_messages(messages).await;
    restarted.recover().await.expect("recover signed proposal");

    let recovered = restarted.state.clone();
    assert_eq!(recovered.proposal_commitment().as_ref(), Some(&commitment));

    restarted
        .accept_frame(
            fixture.remote_keys.peer_id(),
            ExecFrame::StepSignature {
                commitment: commitment.clone(),
                signature: fixture
                    .remote_execution_key()
                    .sign(&commitment.signing_bytes()),
            },
        )
        .await
        .expect("accept final step signature");

    let committed = restarted.state.clone();
    assert!(committed.pending_shared().is_none());
    assert_eq!(committed.agreed_step(), 2);
}

#[tokio::test]
async fn local_handler_signs_with_the_participant_identity_key() {
    let fixture = Fixture::with_mode(false, GuestMode::LocalSign).await;
    let (messages, _observations) = mpsc::channel(8);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    let position = actor.state.clone().event_position();
    assert_eq!(
        actor
            .dispatch_event(
                Event::TimerFired {
                    timer: arena0_protocol::TimerPayload::unit()
                },
                DispatchSource::Local
            )
            .await
            .expect("dispatch local sign"),
        DispatchOutcome::Committed
    );
    let state = actor.state.clone();
    let (signed_bytes, signature): (Vec<u8>, Vec<u8>) =
        borsh::from_slice(state.local_state().as_bytes()).expect("decode sign result");
    let expected = GuestSignData::new(
        fixture.activation.session_hash(),
        Program::parse(fixture.wasm.clone())
            .expect("program")
            .hash(),
        EXEC_ID,
        position,
        0,
        SignScheme::Ed25519,
        b"sig".to_vec(),
    )
    .expect("guest sign data")
    .signing_bytes()
    .expect("signing bytes");
    assert_eq!(signed_bytes, expected);
    assert!(
        arena0_crypto::verify(
            SignScheme::Ed25519,
            &fixture.local_keys.ed25519_public_key().0,
            &signed_bytes,
            &signature,
        )
        .expect("verify")
    );
}

#[tokio::test]
async fn message_handler_cannot_sign() {
    let fixture = Fixture::with_mode(false, GuestMode::SignOnMessage).await;
    let (messages, _observations) = mpsc::channel(8);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    let state = actor.state.clone();
    let source = fixture.local_keys.peer_id();
    let sequence = state.agreed_step();
    let frame = message_frame(&state, source, sequence, vec![4, 5, 6]);
    let error = actor
        .apply_message(source, frame)
        .await
        .expect_err("a message handler must not reach the signer");
    assert!(
        matches!(&error, crate::context::ExecError::Diverged(reason)
            if reason.contains("diverged at step 1") && reason.contains("message handler trapped")),
        "{error:?}"
    );
}

#[tokio::test]
async fn future_message_gets_not_yet_without_persistence() {
    let fixture = Fixture::new(true).await;
    let (messages, _observations) = mpsc::channel(32);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    let state = actor.state.clone();
    let source = fixture.remote_keys.peer_id();
    let sequence = state.agreed_step() + 1;
    let frame = message_frame(&state, source, sequence, vec![4, 5, 6]);
    let send = fixture
        .remote_transport
        .open_exec(
            &fixture.local_keys.peer_id(),
            fixture.activation.session_hash(),
        )
        .await
        .expect("open inbound stream");
    let accepted = fixture
        .local_transport
        .accept_exec()
        .await
        .expect("accept inbound stream");
    let recv = accepted.into_parts().1;
    let sender = tokio::spawn(async move { send.send_exec(&frame).await });
    let delivery = recv.recv_exec().await.expect("receive delivery");
    let resolver = tokio::spawn(async move { actor.inbound(delivery).await });

    assert!(matches!(
        sender.await.expect("sender task"),
        Err(arena0_transport::TransportError::ExecNotYet)
    ));
    let persisted = fixture
        .store
        .handle()
        .load_execution(EXEC_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.agreed_step(), state.agreed_step());
    assert!(persisted.pending_shared().is_none());

    resolver
        .await
        .expect("resolver task")
        .expect("resolve future delivery");
}

#[tokio::test]
async fn future_step_signature_waits_behind_the_current_proposal() {
    let fixture = Fixture::new(true).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;

    let state = actor.state.clone();
    let source = fixture.remote_keys.peer_id();
    let sequence = state.agreed_step();
    assert!(
        actor
            .apply_message(
                source,
                message_frame(&state, source, sequence, vec![1, 2, 3]),
            )
            .await
            .expect("stage current proposal")
    );

    let state = actor.state.clone();
    let mut future_commitment = state.proposal_commitment().expect("staged commitment");
    future_commitment.step += 1;
    let frame = ExecFrame::StepSignature {
        signature: fixture
            .remote_execution_key()
            .sign(&future_commitment.signing_bytes()),
        commitment: future_commitment,
    };
    assert_eq!(
        actor.accept_frame(source, frame).await.unwrap(),
        Some(arena0_transport::ExecDeliveryRejection::NotYet)
    );
    assert_eq!(actor.state, state);
}

#[tokio::test]
async fn trace_observation_waits_for_the_certified_step() {
    let fixture = Fixture::new(true).await;
    let (messages, mut observations) = mpsc::channel(8);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    assert!(matches!(
        observations.try_recv(),
        Ok(crate::SessionMessage::TraceAppended { step: 0 })
    ));

    let state = actor.state.clone();
    let source = fixture.remote_keys.peer_id();
    let sequence = state.agreed_step();
    assert!(
        actor
            .apply_message(
                source,
                message_frame(&state, source, sequence, vec![1, 2, 3]),
            )
            .await
            .expect("stage shared proposal")
    );
    assert!(
        observations.try_recv().is_err(),
        "proposal must not emit a step"
    );

    actor
        .ensure_step_signature()
        .await
        .expect("apply local partial signature");
    assert!(
        observations.try_recv().is_err(),
        "partial signatures must not emit a step"
    );

    let proposal = actor
        .state
        .clone()
        .proposal_commitment()
        .expect("pending proposal");
    actor
        .accept_frame(
            source,
            ExecFrame::StepSignature {
                commitment: proposal.clone(),
                signature: fixture
                    .remote_execution_key()
                    .sign(&proposal.signing_bytes()),
            },
        )
        .await
        .expect("accept remote signature");

    assert!(matches!(
        observations.try_recv(),
        Ok(crate::SessionMessage::TraceAppended { step }) if step == sequence
    ));
    assert!(
        observations.try_recv().is_err(),
        "one certified commit must emit exactly one step"
    );
    let state = actor.state.clone();
    assert_eq!(state.agreed_step(), sequence + 1);
}

#[tokio::test]
async fn local_broadcast_is_queued_then_authored() {
    let fixture = Fixture::with_mode(false, GuestMode::Broadcast).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;
    let before = actor.state.clone();
    assert_eq!(
        actor
            .dispatch_event(
                Event::TimerFired {
                    timer: arena0_protocol::TimerPayload::unit()
                },
                DispatchSource::Local
            )
            .await
            .expect("dispatch broadcast"),
        DispatchOutcome::Committed
    );

    // The local event queues the broadcast and installs immediately.
    let queued = actor.state.clone();
    assert!(queued.pending_shared().is_none());
    assert_eq!(queued.outgoing().len(), 1);
    assert_eq!(queued.agreed_step(), before.agreed_step());

    // The author dispatches its own queued message and stages the proposal.
    actor
        .author_next_message()
        .await
        .expect("author queued message");
    let state = actor.state.clone();
    assert!(state.pending_shared().is_some());
    let frame = state
        .current_frames(fixture.local_keys.peer_id())
        .into_iter()
        .find(|frame| matches!(frame, ExecFrame::Message { .. }))
        .expect("authored message frame");
    assert!(matches!(
        frame,
        ExecFrame::Message { commitment, .. }
            if commitment.step == before.agreed_step()
                && Some(&commitment) == state.proposal_commitment().as_ref()
    ));
    assert_eq!(
        state.event_position(),
        before.event_position().saturating_add(2)
    );
    actor.deliver_frames().unwrap();
    assert_eq!(actor.send_lanes.len(), 1);
    assert!(
        actor
            .send_lanes
            .contains_key(&fixture.remote_keys.peer_id())
    );
    assert_eq!(actor.state, state);
}

#[tokio::test]
async fn author_waits_until_the_local_node_is_the_writer() {
    let fixture = Fixture::with_mode(true, GuestMode::Broadcast).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;
    assert_eq!(
        actor
            .dispatch_event(
                Event::TimerFired {
                    timer: arena0_protocol::TimerPayload::unit()
                },
                DispatchSource::Local
            )
            .await
            .expect("queue broadcast"),
        DispatchOutcome::Committed
    );
    assert_eq!(actor.state.outgoing().len(), 1);
    assert!(!actor.may_author().expect("writer projection"));
    actor
        .author_next_message()
        .await
        .expect("a non-writer authors nothing");
    assert!(actor.state.pending_shared().is_none());
    assert_eq!(actor.state.outgoing().len(), 1);
}

#[tokio::test]
async fn restart_authors_a_non_empty_outgoing_queue() {
    let fixture = Fixture::with_mode(false, GuestMode::Broadcast).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;
    assert_eq!(
        actor
            .dispatch_event(
                Event::TimerFired {
                    timer: arena0_protocol::TimerPayload::unit()
                },
                DispatchSource::Local
            )
            .await
            .expect("queue broadcast"),
        DispatchOutcome::Committed
    );
    assert_eq!(actor.state.outgoing().len(), 1);
    drop(actor);

    let (messages, _observations) = mpsc::channel(32);
    let mut restarted = fixture.actor_with_messages(messages).await;
    assert_eq!(restarted.state.outgoing().len(), 1);
    restarted
        .author_next_message()
        .await
        .expect("author the recovered queue");
    assert!(restarted.state.pending_shared().is_some());
}

#[tokio::test]
async fn own_rejected_message_is_dropped_from_the_queue() {
    let fixture = Fixture::with_mode(false, GuestMode::RejectMessage).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;
    assert_eq!(
        actor
            .dispatch_event(
                Event::TimerFired {
                    timer: arena0_protocol::TimerPayload::unit()
                },
                DispatchSource::Local
            )
            .await
            .expect("queue broadcast"),
        DispatchOutcome::Committed
    );
    assert_eq!(actor.state.outgoing().len(), 1);
    actor
        .author_next_message()
        .await
        .expect("a rejected own message is dropped");
    assert!(actor.state.pending_shared().is_none());
    assert!(actor.state.outgoing().is_empty());

    // The drop is durable, not only an in-memory edit.
    let reloaded = fixture
        .store
        .handle()
        .load_execution(EXEC_ID)
        .await
        .expect("load execution")
        .expect("execution");
    assert!(reloaded.outgoing().is_empty());
    assert!(reloaded.pending_shared().is_none());
}

#[tokio::test]
async fn local_input_that_mutates_shared_state_is_rejected_and_changes_nothing() {
    let fixture = Fixture::with_mode(false, GuestMode::InputSharedChange).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;
    let before = actor.state.clone();
    let pending_id = before.callout().expect("open callout").id;

    assert!(matches!(
        actor
            .submit_input(pending_id, JsonBytes::try_new(b"null".to_vec()).unwrap())
            .await,
        Err(SubmitInputError::Expected(crate::ExecError::InputRejected(
            _
        )))
    ));
    assert_eq!(actor.state, before);
    assert_eq!(actor.state.callout().map(|open| open.id), Some(pending_id));
}

#[tokio::test]
async fn session_started_may_end_the_session() {
    let fixture = Fixture::with_mode(false, GuestMode::EndOnSessionStarted).await;
    let mut actor = fixture.prepare_active_actor().await;
    let ensemble = actor.ensemble();
    assert_eq!(
        actor
            .dispatch_event(Event::SessionStarted { ensemble }, DispatchSource::Local)
            .await
            .expect("session start"),
        DispatchOutcome::Committed
    );
    let proposal = actor.state.pending_shared().expect("terminal proposal");
    assert!(proposal.entry().is_terminal());
}

#[tokio::test]
async fn input_broadcast_at_the_payload_bound_is_accepted() {
    let at_limit = arena0_protocol::execution::MAX_EFFECT_PAYLOAD_BYTES as u32;
    let fixture = Fixture::with_mode(false, GuestMode::InputBroadcast(at_limit)).await;
    let (messages, observations) = mpsc::channel(32);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    let pending_id = actor.state.callout().expect("open callout").id;
    actor
        .submit_input(pending_id, JsonBytes::try_new(b"null".to_vec()).unwrap())
        .await
        .expect("at-limit broadcast is accepted");
    assert_eq!(actor.state.outgoing().len(), 1);
    assert!(!actor.state.status().is_terminal());
    drop(observations);
}

#[tokio::test]
async fn input_broadcast_over_the_payload_bound_is_rejected_and_changes_nothing() {
    let over = arena0_protocol::execution::MAX_EFFECT_PAYLOAD_BYTES as u32 + 1;
    let fixture = Fixture::with_mode(false, GuestMode::InputBroadcast(over)).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;
    let before = actor.state.clone();
    let pending_id = before.callout().expect("open callout").id;
    assert!(matches!(
        actor
            .submit_input(pending_id, JsonBytes::try_new(b"null".to_vec()).unwrap())
            .await,
        Err(SubmitInputError::Expected(crate::ExecError::InputRejected(
            _
        )))
    ));
    assert_eq!(actor.state, before);
    assert_eq!(actor.state.callout().map(|open| open.id), Some(pending_id));
    assert!(!actor.state.status().is_terminal());
}

#[tokio::test]
async fn agreed_broadcast_overflow_fails_the_session() {
    let fixture = Fixture::with_mode(true, GuestMode::MessageBroadcast).await;
    let (messages, _observations) = mpsc::channel(16);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    // Fill the local outgoing queue to the bound with local broadcasts.
    for _ in 0..arena0_protocol::execution::MAX_OUTGOING_MESSAGES {
        assert_eq!(
            actor
                .dispatch_event(
                    Event::TimerFired {
                        timer: arena0_protocol::TimerPayload::unit()
                    },
                    DispatchSource::Local,
                )
                .await
                .expect("queue local broadcast"),
            DispatchOutcome::Committed
        );
    }
    assert_eq!(
        actor.state.outgoing().len(),
        arena0_protocol::execution::MAX_OUTGOING_MESSAGES
    );

    // A peer message whose agreed handler broadcasts would overflow.
    let source = fixture.remote_keys.peer_id();
    let data = b"overflow".to_vec();
    let frame = message_frame(&actor.state, source, actor.state.agreed_step(), data);
    let error = actor
        .accept_frame(source, frame)
        .await
        .expect_err("agreed overflow");
    assert!(matches!(error, crate::ExecError::OutgoingQueueOverflow));
    assert!(actor.fail_terminal(error).await);
    assert!(actor.state.status().is_terminal());
    let reason = actor
        .state
        .status()
        .terminal_cause()
        .map(arena0_protocol::StopCause::reason)
        .unwrap_or_default();
    assert!(reason.contains("outgoing queue overflow"), "{reason}");
}

fn activation(
    program_hash: ProgramHash,
    local_keys: &NodeKeys,
    remote_keys: &NodeKeys,
    local_salt: &ExecutionSalt,
    remote_salt: &ExecutionSalt,
    initial_state: StateHash,
) -> Activation {
    let local_peer = local_keys.peer_id();
    let remote_peer = remote_keys.peer_id();
    let offer_data = OfferData::new(
        NEGOTIATION_ID,
        0,
        local_peer,
        program_hash,
        arena0_program::ExecutionProfile::current().hash(),
        JsonBytes::try_new(b"null".to_vec()).expect("params"),
        2,
        // StateHash commits the canonical fixed-width state-memory image, not
        // the semantic payload bytes alone.
        initial_state,
        1_000_000,
    )
    .expect("offer data");
    let offer_hash = arena0_protocol::OfferHash::of(&offer_data);
    let local_bls = execution_secret(local_salt);
    let remote_bls = execution_secret(remote_salt);
    let local_ticket = ticket(local_keys, &local_bls, offer_hash, local_peer);
    let remote_ticket = ticket(remote_keys, &remote_bls, offer_hash, remote_peer);
    let tickets = vec![local_ticket, remote_ticket];
    let hashes = tickets
        .iter()
        .map(|ticket| TicketHash::of(&ticket.data))
        .collect::<Vec<_>>();
    let activation_data = ActivationData::new(offer_hash, hashes.clone()).expect("activation data");
    let aggregate = arena0_crypto::BlsSignature::aggregate(&[
        local_bls.sign(&activation_data.signing_bytes()),
        remote_bls.sign(&activation_data.signing_bytes()),
    ])
    .expect("activation aggregate");
    let offer = Offer::new(offer_data, hashes).expect("offer");
    let prepared = PreparedActivation::new(offer, tickets).expect("prepared activation");
    Activation::new(prepared, aggregate).expect("activation")
}

fn ticket(
    node_keys: &NodeKeys,
    execution_secret: &BlsSecretKey,
    offer_hash: arena0_protocol::OfferHash,
    peer: PeerId,
) -> Ticket {
    let execution_bls = execution_secret.public_key();
    let key_binding =
        execution_secret.sign_binding(&key_binding_message(&offer_hash.0, &peer.0, &execution_bls));
    let data = TicketData::new(
        NEGOTIATION_ID,
        0,
        peer,
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
        signature: node_keys.sign(&data.signing_bytes()),
        data,
    }
}

fn execution_secret(salt: &ExecutionSalt) -> BlsSecretKey {
    let mut material = [0u8; 96];
    material[..32].copy_from_slice(salt.as_bytes());
    material[32..64].copy_from_slice(&EXEC_ID.0);
    material[64..].copy_from_slice(&NEGOTIATION_ID.0);
    let seed = blake3::derive_key("arena0 execution BLS key v2", &material);
    BlsSecretKey::from_seed(&seed).expect("execution secret")
}

fn test_wasm(writer: Option<u8>, mode: GuestMode) -> Vec<u8> {
    let peers = Ensemble::from_peers(vec![
        NodeKeys::from_secret(SecretKey::from_bytes([1; 32])).peer_id(),
        NodeKeys::from_secret(SecretKey::from_bytes([2; 32])).peer_id(),
    ])
    .expect("ensemble");
    let writer_peer = writer.map(|index| peers.peers()[usize::from(index)]);
    let unit = unit_schema();
    let capabilities = match mode {
        GuestMode::Plain
        | GuestMode::EndOnMessage
        | GuestMode::InputSharedChange
        | GuestMode::EndOnSessionStarted => Vec::new(),
        GuestMode::InputBroadcast(_) | GuestMode::MessageBroadcast => vec![Capability::Messaging],
        GuestMode::Timer => vec![Capability::Timers],
        GuestMode::Callout | GuestMode::CalloutFault | GuestMode::CalloutReject => Vec::new(),
        GuestMode::LocalSign | GuestMode::SignOnMessage => vec![Capability::Sign {
            schemes: vec![SignScheme::Ed25519],
        }],
        GuestMode::Broadcast | GuestMode::RejectMessage => vec![Capability::Messaging],
    };
    let callouts = match mode {
        GuestMode::Callout
        | GuestMode::CalloutFault
        | GuestMode::CalloutReject
        | GuestMode::InputSharedChange
        | GuestMode::InputBroadcast(_) => {
            vec![CalloutSchema {
                name: "request".into(),
                prompt: "request".into(),
                input: JsonSchemaDocument::new(serde_json::json!({"$schema": "https://json-schema.org/draft/2020-12/schema", "type": ["null", "boolean"]})).unwrap(),
                output: unit.clone(),
            }]
        }
        _ => Vec::new(),
    };
    let definition = ProgramDefinition {
        metadata: ProgramMetadata {
            name: "actor-test".into(),
            version: "1".into(),
            description: "actor test guest".into(),
            author: None,
            capabilities,
            display_name: "Actor test".into(),
            participants: arena0_program::ParticipantCount::Exact { count: 2 },
        },
        schema: ProgramSchema {
            state: StateSchema {
                schema: unit.clone(),
                max_bytes: 64,
            },
            callouts,
            messages: Vec::new(),
            params: unit.clone(),
            queries: Vec::new(),
            outcome: unit,
        },
    };
    let metadata = definition.encode().expect("metadata");
    let writer = writer.map_or_else(|| vec![0], |index| vec![1, index]);
    let extra_imports = match mode {
        GuestMode::Timer => {
            r#"(import "arena0" "set_timer" (func $set_timer (param i64 i32 i32 i32 i32)))"#
        }
        GuestMode::Callout | GuestMode::CalloutFault | GuestMode::CalloutReject => "",
        GuestMode::LocalSign | GuestMode::SignOnMessage => {
            r#"(import "arena0" "sign"
            (func $sign (param i32 i32 i32 i32 i32) (result i32)))"#
        }
        GuestMode::Broadcast | GuestMode::RejectMessage => {
            r#"(import "arena0" "broadcast" (func $broadcast (param i32 i32) (result i32)))"#
        }
        GuestMode::EndOnMessage | GuestMode::EndOnSessionStarted => {
            r#"(import "arena0" "end_session" (func $end_session (param i32 i32)))"#
        }
        GuestMode::Plain | GuestMode::InputSharedChange => "",
        GuestMode::InputBroadcast(_) | GuestMode::MessageBroadcast => {
            r#"(import "arena0" "broadcast" (func $broadcast (param i32 i32) (result i32)))"#
        }
    };
    let local_body = match mode {
        GuestMode::Plain => {
            r#"
              i32.const 1
              i32.const 1040
              i32.const 1
              call $state_write
            "#
        }
        GuestMode::Timer => {
            r#"
              i32.const 1
              i32.const 1040
              i32.const 1
              call $state_write
              i64.const 0
              i32.const 1090
              i32.const 4
              i32.const 1070
              i32.const 5
              call $set_timer
            "#
        }
        GuestMode::Callout
        | GuestMode::CalloutFault
        | GuestMode::CalloutReject
        | GuestMode::InputSharedChange
        | GuestMode::InputBroadcast(_)
        | GuestMode::MessageBroadcast => {
            r#"
              i32.const 1
              i32.const 1040
              i32.const 1
              call $state_write
              i32.const 33000
              i32.const 15
              call $pack
              return
            "#
        }
        GuestMode::LocalSign => {
            r#"
              i32.const 1
              i32.const 1040
              i32.const 1
              call $state_write
              i32.const 1
              i32.const 2000
              i32.const 0
              i32.const 1050
              i32.const 3
              i32.const 2000
              i32.const 512
              call $sign
              call $state_write
            "#
        }
        GuestMode::Broadcast | GuestMode::RejectMessage => {
            r#"
              i32.const 1
              i32.const 1040
              i32.const 1
              call $state_write
              i32.const 1110
              i32.const 9
              call $broadcast
              drop
            "#
        }
        GuestMode::EndOnMessage | GuestMode::SignOnMessage | GuestMode::EndOnSessionStarted => "",
    };
    // The timer fixture arms its timer from the session boundary, which is an
    // agreed event, so no local dispatch is needed to schedule it.
    let session_started_body = if matches!(mode, GuestMode::Timer) {
        r#"
          i32.const 1
          i32.const 1040
          i32.const 1
          call $state_write
          i64.const 0
          i32.const 1090
          i32.const 4
          i32.const 1070
          i32.const 5
          call $set_timer
        "#
    } else if matches!(
        mode,
        GuestMode::InputSharedChange | GuestMode::InputBroadcast(_)
    ) {
        r#"
          i32.const 1
          i32.const 1040
          i32.const 1
          call $state_write
          i32.const 33000
          i32.const 15
          call $pack
          return
        "#
    } else if matches!(mode, GuestMode::EndOnSessionStarted) {
        r#"
          i32.const 0
          i32.const 1024
          i32.const 1
          call $state_write
          i32.const 1
          i32.const 1030
          i32.const 1
          call $state_write
          i32.const 1080
          i32.const 0
          call $end_session
        "#
    } else {
        ""
    };
    let local_body = if matches!(mode, GuestMode::RejectMessage) {
        // Both participants run the local handler; only the designated writer broadcasts.
        format!(
            r#"
          local.get $input
          i64.load
          i64.const {}
          i64.ne
          if
            i32.const 32768
            i32.const 3
            call $pack
            return
          end
          {local_body}
        "#,
            i64::from_le_bytes(writer_peer.expect("writer").0[..8].try_into().unwrap())
        )
    } else {
        local_body.to_owned()
    };
    let input_fault_body = match mode {
        GuestMode::CalloutFault => {
            r#"
              i32.const 0
              i32.const 1024
              i32.const 1
              call $state_write
              i32.const 1
              i32.const 1030
              i32.const 1
              call $state_write
              unreachable
            "#
        }
        GuestMode::InputSharedChange => {
            // A local input handler that changes agreed shared state.
            r#"
              i32.const 0
              i32.const 1024
              i32.const 1
              call $state_write
            "#
        }
        GuestMode::InputBroadcast(len) => &format!(
            r#"
              i32.const 0
              i32.const {len}
              call $broadcast
              drop
            "#
        ),
        GuestMode::CalloutReject => {
            r#"
              local.get $event_ptr
              i32.load8_u offset=9
              i32.const 110
              i32.eq
              if
                i32.const 32768
                i32.const 1
                i32.store8
              else
                i32.const 32768
                i32.const 0
                i32.store8
              end
            "#
        }
        _ => "",
    };
    let message_body = match mode {
        GuestMode::RejectMessage => {
            r#"
              i32.const 32768
              i32.const 1
              i32.store8
            "#
        }
        GuestMode::EndOnMessage | GuestMode::EndOnSessionStarted => {
            r#"
              i32.const 0
              i32.const 1024
              i32.const 1
              call $state_write
              i32.const 1
              i32.const 1030
              i32.const 1
              call $state_write
              i32.const 1080
              i32.const 0
              call $end_session
            "#
        }
        GuestMode::MessageBroadcast => {
            r#"
              i32.const 0
              i32.const 1024
              i32.const 1
              call $state_write
              i32.const 1
              i32.const 1030
              i32.const 1
              call $state_write
              i32.const 1110
              i32.const 9
              call $broadcast
              drop
            "#
        }
        GuestMode::SignOnMessage => {
            r#"
              i32.const 0
              i32.const 1024
              i32.const 1
              call $state_write
              i32.const 0
              i32.const 1050
              i32.const 3
              i32.const 2000
              i32.const 512
              call $sign
              drop
            "#
        }
        _ => {
            r#"
              i32.const 0
              i32.const 1024
              i32.const 1
              call $state_write
              i32.const 1
              i32.const 1030
              i32.const 1
              call $state_write
            "#
        }
    };
    // The fired-timer arm runs the timer handler for the timer fixture and the
    // former local handler body for every other mode.
    let timer_body = match mode {
        GuestMode::Timer => r#"
              i32.const 1
              i32.const 1140
              i32.const 1
              call $state_write
            "#
        .to_owned(),
        GuestMode::MessageBroadcast => r#"
              i32.const 1110
              i32.const 9
              call $broadcast
              drop
            "#
        .to_owned(),
        _ => local_body.clone(),
    };
    let wat = format!(
        r#"
        (module
          (import "arena0" "state_len" (func $state_len (param i32) (result i32)))
          (import "arena0" "state_read" (func $state_read (param i32 i32 i32)))
          (import "arena0" "state_write" (func $state_write (param i32 i32 i32)))
          {extra_imports}
          (memory (export "memory") 1)
          (global (export "arena0_abi_version") i32 (i32.const 22))
          (data (i32.const 1024) "\01")
          (data (i32.const 1030) "\02")
          (data (i32.const 1040) "\09")
          (data (i32.const 1050) "sig")
          (data (i32.const 1060) "sig")
          (data (i32.const 1070) "bytes")
          (data (i32.const 1080) "null")
          (data (i32.const 1090) "test")
          (data (i32.const 1100) "null")
          (data (i32.const 1110) "broadcast")
          (data (i32.const 1120) "\00\00\00\00\04\00\00\00null")
          (data (i32.const 1140) "\08")
          (data (i32.const 2000) "{writer}")
          (data (i32.const 3000) "\00\00\00\00\00\00\00\00")
          (data (i32.const 32768) "\00\00\00")
          (data (i32.const 33000) "\00\00\01\00\00\00\00\04\00\00\00null")
          (data (i32.const 33100) "\00\00\01\00\00\00\00\04\00\00\00true")
          (data (i32.const 40000) "{metadata}")
          (func $pack (param $ptr i32) (param $len i32) (result i64)
            local.get $ptr
            i64.extend_i32_u
            i64.const 32
            i64.shl
            local.get $len
            i64.extend_i32_u
            i64.or)
          (func (export "arena0_alloc") (param i32) (result i32) i32.const 1048576)
          (func (export "arena0_dealloc") (param i32 i32))
          (func (export "arena0_prepare") (result i32)
            i32.const 895
            memory.grow
            drop
            i32.const 1)
          (func (export "arena0_initialize") (param i32 i32) (result i64)
            i32.const 3000
            i32.const 8
            call $pack)
          (func (export "arena0_dispatch") (param $input i32) (param i32) (result i64)
            (local $session_len i32)
            (local $event_ptr i32)
            local.get $input
            i32.const 32
            i32.add
            i32.load
            local.set $session_len
            local.get $input
            i32.const 36
            i32.add
            local.get $session_len
            i32.add
            i32.const 4
            i32.add
            local.set $event_ptr
            local.get $event_ptr
            i32.load8_u
            i32.const 3
            i32.eq
            if
              {timer_body}
            else
            local.get $event_ptr
            i32.load8_u
            i32.eqz
            if
              {session_started_body}
            else
              local.get $event_ptr
              i32.load8_u
              i32.const 1
              i32.eq
              if
                {message_body}
              else
                local.get $event_ptr
                i32.load8_u
                i32.const 2
                i32.eq
                if
                  {input_fault_body}
                end
              end
            end
            end
            i32.const 32768
            i32.const 3
            call $pack)
          (func (export "arena0_writer") (param i32 i32) (result i64)
            i32.const 2000
            i32.const {writer_len}
            call $pack)
          (func (export "arena0_query") (param i32 i32) (result i64) i64.const 0)
          (func (export "arena0_view") (param i32 i32) (result i64) i64.const 0)
          (func (export "arena0_outcome") (param i32 i32) (result i64)
            i32.const 1120
            i32.const 12
            call $pack)
          (func (export "arena0_metadata") (result i64)
            i32.const 40000
            i32.const {metadata_len}
            call $pack))
        "#,
        extra_imports = extra_imports,
        writer = wat_data(&writer),
        writer_len = writer.len(),
        metadata = wat_data(&metadata),
        metadata_len = metadata.len(),
        session_started_body = session_started_body,
        input_fault_body = input_fault_body,
        message_body = message_body,
        timer_body = timer_body,
    );
    let raw = wat::parse_str(wat).expect("wat");
    shared_test_engine()
        .build_program(&raw)
        .expect("finalize test program")
        .bytes()
        .to_vec()
}

fn unit_schema() -> JsonSchemaDocument {
    JsonSchemaDocument::new(serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "null"
    }))
    .expect("unit schema")
}

fn wat_data(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("\\{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

#[tokio::test]
async fn failed_persist_reloads_state_and_rebuilds_resident_on_next_dispatch() {
    let fixture = Fixture::new(false).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;
    let before = actor.state.clone();
    // Advance the durable owner without installing its result in the actor.
    // A subsequent guest dispatch must hit the version tripwire and recover.
    let mut durable = before.clone();
    durable
        .apply_dispatch(
            &Event::TimerFired {
                timer: arena0_protocol::TimerPayload::unit(),
            },
            before.shared_state().clone(),
            arena0_program::LocalStateBytes::try_new(vec![7]).unwrap(),
            &[],
            None,
            None,
            None,
        )
        .unwrap();
    actor
        .context
        .store
        .persist(arena0_store::TransitionRecord {
            expected: before.version(),
            next: durable.clone(),
            change: Change::Dispatch {
                event: Event::TimerFired {
                    timer: arena0_protocol::TimerPayload::unit(),
                },
                effects: vec![],
                timer_id: None,
            },
            now_ms: super::now_ms(),
        })
        .await
        .unwrap();
    let result = actor
        .dispatch_event(
            Event::TimerFired {
                timer: arena0_protocol::TimerPayload::unit(),
            },
            DispatchSource::Local,
        )
        .await;
    assert!(
        result.is_err(),
        "stale actor must not overwrite durable state"
    );
    assert_eq!(actor.state, durable);
    assert!(
        actor.instance.is_none(),
        "a failed persist drops the resident; the next use rebuilds it"
    );
    assert_eq!(
        actor
            .dispatch_event(
                Event::TimerFired {
                    timer: arena0_protocol::TimerPayload::unit()
                },
                DispatchSource::Local
            )
            .await
            .unwrap(),
        DispatchOutcome::Committed
    );
    let resident = actor
        .instance
        .as_ref()
        .expect("rebuilt resident")
        .committed_payloads();
    assert_eq!(resident.0, actor.state.shared_state());
    assert_eq!(resident.1, actor.state.local_state());
    assert_eq!(actor.state.local_state().as_bytes(), &[9]);
    assert_eq!(
        actor.context.store.load_execution().await.unwrap().unwrap(),
        actor.state
    );
}

/// Two live peers reach the same agreed cursor, but one has a full outgoing
/// queue. The author's establishing message makes the receiver overflow, and
/// the receiver's signed failure ends both peers through normal delivery.
#[tokio::test]
async fn agreed_overflow_converges_both_peers_on_failure() {
    let writer = Fixture::with_mode(false, GuestMode::MessageBroadcast).await;
    let receiver = Fixture::from_wasm_with_peer(writer.wasm.clone(), Some(&writer)).await;
    let writer_peer = writer.local_keys.peer_id();
    for fixture in [&writer, &receiver] {
        let (messages, _observations) = mpsc::channel(16);
        let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
        fixture.commit_session_started(&mut actor).await;
        if fixture.local_keys.peer_id() == writer_peer {
            assert_eq!(
                actor
                    .dispatch_event(
                        Event::TimerFired {
                            timer: arena0_protocol::TimerPayload::unit()
                        },
                        DispatchSource::Local
                    )
                    .await
                    .expect("queue writer broadcast"),
                DispatchOutcome::Committed
            );
            actor
                .author_next_message()
                .await
                .expect("author writer message");
            actor
                .ensure_step_signature()
                .await
                .expect("sign writer proposal");
            assert_eq!(
                actor
                    .state
                    .pending_shared()
                    .expect("staged writer proposal")
                    .signature_count(),
                1
            );
        } else {
            for _ in 0..arena0_protocol::execution::MAX_OUTGOING_MESSAGES {
                assert_eq!(
                    actor
                        .dispatch_event(
                            Event::TimerFired {
                                timer: arena0_protocol::TimerPayload::unit()
                            },
                            DispatchSource::Local
                        )
                        .await
                        .expect("queue receiver broadcast"),
                    DispatchOutcome::Committed
                );
            }
            assert_eq!(
                actor.state.outgoing().len(),
                arena0_protocol::execution::MAX_OUTGOING_MESSAGES
            );
            // The overflowing peer has signed no step beyond session start.
            assert!(actor.state.pending_shared().is_none());
        }
    }
    let writer_host = crate::Host::start(
        Arc::clone(&writer.local_keys),
        writer.local_transport.clone() as Arc<dyn Transport + Sync>,
        writer.store.handle().clone(),
    );
    let receiver_host = crate::Host::start(
        Arc::clone(&receiver.local_keys),
        receiver.local_transport.clone() as Arc<dyn Transport + Sync>,
        receiver.store.handle().clone(),
    );
    let mut writer_exec = writer_host
        .spawn(
            writer.context(),
            writer_host.claim_execution(EXEC_ID).expect("writer claim"),
        )
        .expect("writer actor");
    let mut receiver_exec = receiver_host
        .spawn(
            receiver.context(),
            receiver_host
                .claim_execution(EXEC_ID)
                .expect("receiver claim"),
        )
        .expect("receiver actor");
    tokio::time::timeout(Duration::from_secs(10), async {
        for execution in [&mut receiver_exec, &mut writer_exec] {
            loop {
                match execution
                    .message_rx
                    .recv()
                    .await
                    .expect("terminal observation")
                {
                    crate::SessionMessage::Failed { reason } => {
                        assert!(reason.contains("outgoing queue overflow"), "{reason}");
                        break;
                    }
                    crate::SessionMessage::Completed { .. }
                    | crate::SessionMessage::Aborted { .. } => {
                        panic!("expected an authenticated failure")
                    }
                    _ => {}
                }
            }
        }
    })
    .await
    .expect("both peers must finish without wedging");
    for fixture in [&writer, &receiver] {
        let state = fixture
            .store
            .handle()
            .load_execution(EXEC_ID)
            .await
            .expect("load terminal")
            .expect("execution");
        assert!(state.status().is_terminal());
        assert!(state.pending_shared().is_none());
    }
    let receiver_state = receiver
        .store
        .handle()
        .load_execution(EXEC_ID)
        .await
        .expect("load receiver")
        .expect("receiver execution");
    // The overflowing peer only ever signed session start.
    assert_eq!(receiver_state.agreed_step(), 1);
    writer_exec.shutdown().await;
    receiver_exec.shutdown().await;
    writer_host.stop().await;
    receiver_host.stop().await;
}

/// Two live peers receive a message frame whose only divergence is its
/// terminal. The receiver fails before signing, and its signed failure ends the
/// author through normal delivery.
#[tokio::test]
async fn terminal_commitment_mismatch_converges_both_peers_on_failure() {
    let writer = Fixture::with_mode(false, GuestMode::Plain).await;
    let receiver = Fixture::from_wasm_with_peer(writer.wasm.clone(), Some(&writer)).await;
    let writer_peer = writer.local_keys.peer_id();
    let receiver_peer = receiver.local_keys.peer_id();
    for fixture in [&writer, &receiver] {
        let (messages, _observations) = mpsc::channel(16);
        let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
        fixture.commit_session_started(&mut actor).await;
    }
    let writer_host = crate::Host::start(
        Arc::clone(&writer.local_keys),
        writer.local_transport.clone() as Arc<dyn Transport + Sync>,
        writer.store.handle().clone(),
    );
    let receiver_host = crate::Host::start(
        Arc::clone(&receiver.local_keys),
        receiver.local_transport.clone() as Arc<dyn Transport + Sync>,
        receiver.store.handle().clone(),
    );
    let mut writer_exec = writer_host
        .spawn(
            writer.context(),
            writer_host.claim_execution(EXEC_ID).expect("writer claim"),
        )
        .expect("writer actor");
    let mut receiver_exec = receiver_host
        .spawn(
            receiver.context(),
            receiver_host
                .claim_execution(EXEC_ID)
                .expect("receiver claim"),
        )
        .expect("receiver actor");
    let receiver_state = receiver
        .store
        .handle()
        .load_execution(EXEC_ID)
        .await
        .expect("load receiver")
        .expect("receiver execution");
    let data = b"terminal only".to_vec();
    let poststate = StateHash::of_shared(&message_state());
    let commitment = message_commitment_with_terminal(
        &receiver_state,
        writer_peer,
        receiver_state.agreed_step(),
        receiver_state.agreed_state(),
        poststate,
        Some(arena0_protocol::StepTerminal::End { outcome: vec![] }),
        &data,
    );
    let frame = ExecFrame::Message { commitment, data };
    let stream = writer
        .local_transport
        .open_exec(&receiver_peer, writer.activation.session_hash())
        .await
        .expect("open writer stream");
    stream
        .send_exec(&frame)
        .await
        .expect("send mismatched frame");
    tokio::time::timeout(Duration::from_secs(10), async {
        for execution in [&mut receiver_exec, &mut writer_exec] {
            loop {
                match execution
                    .message_rx
                    .recv()
                    .await
                    .expect("terminal observation")
                {
                    crate::SessionMessage::Failed { reason } => {
                        assert!(reason.contains("entry mismatch"), "{reason}");
                        break;
                    }
                    crate::SessionMessage::Completed { .. }
                    | crate::SessionMessage::Aborted { .. } => {
                        panic!("expected an authenticated failure")
                    }
                    _ => {}
                }
            }
        }
    })
    .await
    .expect("both peers must finish without wedging");
    for fixture in [&writer, &receiver] {
        let state = fixture
            .store
            .handle()
            .load_execution(EXEC_ID)
            .await
            .expect("load terminal")
            .expect("execution");
        assert!(state.status().is_terminal());
        assert!(state.pending_shared().is_none());
        assert_eq!(state.agreed_step(), 1);
    }
    writer_exec.shutdown().await;
    receiver_exec.shutdown().await;
    writer_host.stop().await;
    receiver_host.stop().await;
}
