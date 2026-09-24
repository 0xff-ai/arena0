use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arena0_crypto::bls::BlsSecretKey;
use arena0_crypto::{
    ExecutionKey, ExecutionSalt, NodeKeys, SecretKey, SignScheme, key_binding_message,
};
use arena0_program::{
    CalloutSchema, Capability, JsonBytes, JsonSchemaDocument, ProgramDefinition, ProgramHash,
    ProgramMetadata, ProgramSchema, SharedStateBytes, StateSchema,
};
use arena0_protocol::execution::GuestSignData;
use arena0_protocol::{
    AbortKind, AbortOccurrence, Activation, ActivationData, Effect, Ensemble, Event, ExecFrame,
    ExecId, ExecutionAdmission, MessageId, NegotiationId, Offer, OfferData,
    ParticipantStepSignature, ParticipantTerminalSignature, PeerId, PeerIdSource, PendingOperation,
    PreparedActivation, StateHash, StopCause, Ticket, TicketAction, TicketData, TicketHash,
};
use arena0_sandbox::{LoadedProgram, Program, WasmtimeEngine};
use arena0_store::{ApplyOutcome, OutboxPayloadKind, Store, StoreConfig};
use arena0_transport::local::{LocalNetwork, LocalTransport};
use arena0_transport::{
    AcceptedExecStream, NegotiationTopic, RecvHandle, SendHandle, Transport, TransportError,
};
use bytes::Bytes;
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot};

use super::ExecutionActor;
use super::guest::DispatchSource;
use crate::context::ExecContext;

const EXEC_ID: ExecId = ExecId([0x44; 32]);
const NEGOTIATION_ID: NegotiationId = NegotiationId([0x11; 32]);

#[test]
fn terminal_reason_truncation_preserves_utf8_boundaries() {
    let reason = super::truncate_reason("é".repeat(arena0_protocol::MAX_TERMINAL_REASON_BYTES));
    assert!(reason.len() <= arena0_protocol::MAX_TERMINAL_REASON_BYTES);
    assert!(std::str::from_utf8(reason.as_bytes()).is_ok());
}

#[derive(Clone, Copy)]
enum GuestMode {
    Plain,
    Timer,
    Callout,
    CalloutFault,
    CalloutRetryMessage,
    Sign,
    SignThenBroadcast,
    Broadcast,
    EndOnMessage,
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

/// Test transport that injects one terminal-classified stream-open failure,
/// then delegates every operation to the real local transport. This exercises
/// the actor's durable retry owner before an outbound send task exists.
#[derive(Debug)]
struct FailFirstExecTransport {
    inner: Arc<LocalTransport>,
    fail_next_open: AtomicBool,
}

impl FailFirstExecTransport {
    fn new(inner: Arc<LocalTransport>) -> Self {
        Self {
            inner,
            fail_next_open: AtomicBool::new(true),
        }
    }

    fn injected_failure_was_used(&self) -> bool {
        !self.fail_next_open.load(Ordering::Acquire)
    }
}

#[async_trait::async_trait]
impl Transport for FailFirstExecTransport {
    async fn subscribe_program(
        &self,
        program_id: ProgramHash,
        bootstrap: Vec<PeerId>,
    ) -> Result<Box<dyn NegotiationTopic>, TransportError> {
        self.inner.subscribe_program(program_id, bootstrap).await
    }

    async fn import_blob(
        &self,
        negotiation_id: NegotiationId,
        bytes: Bytes,
        max_bytes: u64,
    ) -> Result<[u8; 32], TransportError> {
        self.inner
            .import_blob(negotiation_id, bytes, max_bytes)
            .await
    }

    async fn fetch_blob(
        &self,
        negotiation_id: NegotiationId,
        hash: [u8; 32],
        declared_len: u64,
        providers: Vec<PeerId>,
        max_bytes: u64,
    ) -> Result<(), TransportError> {
        self.inner
            .fetch_blob(negotiation_id, hash, declared_len, providers, max_bytes)
            .await
    }

    async fn read_blob(&self, hash: [u8; 32], max_bytes: u64) -> Result<Bytes, TransportError> {
        self.inner.read_blob(hash, max_bytes).await
    }

    async fn retain_session_blob(
        &self,
        session_id: arena0_protocol::SessionHash,
        hash: [u8; 32],
    ) -> Result<(), TransportError> {
        self.inner.retain_session_blob(session_id, hash).await
    }

    async fn release_negotiation_blobs(
        &self,
        negotiation_id: NegotiationId,
    ) -> Result<(), TransportError> {
        self.inner.release_negotiation_blobs(negotiation_id).await
    }

    async fn release_session_blobs(
        &self,
        session_id: arena0_protocol::SessionHash,
    ) -> Result<(), TransportError> {
        self.inner.release_session_blobs(session_id).await
    }

    async fn open_exec(
        &self,
        peer: &PeerId,
        session_hash: arena0_protocol::SessionHash,
    ) -> Result<SendHandle, TransportError> {
        if self.fail_next_open.swap(false, Ordering::AcqRel) {
            return Err(TransportError::InvalidFrame(
                "injected first terminal stream-open failure".into(),
            ));
        }
        self.inner.open_exec(peer, session_hash).await
    }

    async fn accept_exec(&self) -> Result<AcceptedExecStream, TransportError> {
        self.inner.accept_exec().await
    }

    async fn open_fetch(&self, peer: &PeerId) -> Result<SendHandle, TransportError> {
        self.inner.open_fetch(peer).await
    }

    async fn accept_fetch(&self) -> Result<RecvHandle, TransportError> {
        self.inner.accept_fetch().await
    }

    async fn close(&self) {
        self.inner.close().await;
    }
}

impl Fixture {
    async fn new(remote_is_writer: bool) -> Self {
        Self::with_mode(remote_is_writer, GuestMode::Plain).await
    }

    async fn with_mode(remote_is_writer: bool, mode: GuestMode) -> Self {
        let local_keys = Arc::new(NodeKeys::from_secret(SecretKey::from_bytes([1; 32])));
        let remote_keys = Arc::new(NodeKeys::from_secret(SecretKey::from_bytes([2; 32])));
        let local_peer = local_keys.peer_id();
        let remote_peer = remote_keys.peer_id();
        let local_salt = ExecutionSalt::try_from_bytes([9; 32]).expect("non-zero test salt");
        let remote_salt = ExecutionSalt::try_from_bytes([10; 32]).expect("non-zero test salt");
        let ensemble = Ensemble::from_peers(vec![local_peer, remote_peer]).expect("ensemble");
        let writer_peer = if remote_is_writer {
            remote_peer
        } else {
            local_peer
        };
        let writer = ensemble
            .participant_of(&writer_peer)
            .expect("writer participant")
            .index() as u8;
        let wasm = test_wasm(Some(writer), mode);
        let program = Program::parse(wasm.clone()).expect("program");
        let activation = activation(
            program.hash(),
            local_keys.as_ref(),
            remote_keys.as_ref(),
            &local_salt,
            &remote_salt,
        );
        let params = JsonBytes::try_new(b"null".to_vec()).expect("params");
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
                ExecutionAdmission::explicit(NEGOTIATION_ID, vec![local_peer, remote_peer])
                    .expect("admission"),
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
        WasmtimeEngine::new()
            .expect("sandbox engine")
            .load(&program)
            .expect("loaded program")
    }

    fn context(&self) -> ExecContext {
        ExecContext {
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

    fn actor_with_messages(&self, messages: mpsc::Sender<crate::SessionMessage>) -> ExecutionActor {
        ExecutionActor {
            context: self.context().bind(
                Arc::clone(&self.local_keys),
                self.store
                    .handle()
                    .claim_execution(EXEC_ID)
                    .expect("execution writer"),
                self.local_transport.clone() as Arc<dyn Transport + Sync>,
            ),
            instance: None,
            messages,
            send_streams: HashMap::new(),
            inflight_send: None,
            session_started_emitted: false,
            terminal_emitted: false,
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
        let mut actor = self.actor_with_messages(messages);
        actor.ensure_execution().await.expect("execution");
        let state = actor.load_state().await.expect("load activating state");
        let outcome = actor
            .context
            .store
            .activate(state.version(), 4)
            .await
            .expect("activate");
        assert!(matches!(outcome, ApplyOutcome::Committed { .. }));
        actor.reload_resident().await.expect("restore resident");
        actor
    }

    async fn commit_session_started(&self, actor: &mut ExecutionActor) {
        let accepted = actor
            .dispatch_event(
                Event::SessionStarted {
                    ensemble: actor.ensemble(),
                },
                DispatchSource::default(),
            )
            .await
            .expect("session start dispatch");
        assert_eq!(accepted, Some(true));
        actor
            .ensure_step_signature()
            .await
            .expect("local session signature");
        let state = actor.load_state().await.expect("load session proposal");
        let proposal = state
            .pending_shared()
            .expect("pending session proposal")
            .commitment()
            .clone();
        let frame = ExecFrame::StepSignature {
            commitment: proposal.clone(),
            signature: self.remote_execution_key().sign(&proposal.signing_bytes()),
        };
        actor
            .context
            .store
            .accept_inbound(self.remote_keys.peer_id(), frame, 11)
            .await
            .expect("accept remote session signature");
        let inbox = actor
            .context
            .store
            .list_pending_inbox(16)
            .await
            .expect("list remote session signature")
            .into_iter()
            .next()
            .expect("remote session signature inbox");
        let outcome = actor
            .context
            .store
            .commit_step_signature(
                state.version(),
                ParticipantStepSignature::new(
                    self.remote_keys.peer_id(),
                    proposal.step,
                    self.remote_execution_key().sign(&proposal.signing_bytes()),
                ),
                Some(inbox.inbox_id()),
                11,
            )
            .await
            .expect("remote session signature");
        assert!(matches!(outcome, ApplyOutcome::Committed { .. }));
        actor
            .reload_resident()
            .await
            .expect("promote session state");
        self.clear_outbox(actor).await;
    }

    async fn clear_outbox(&self, actor: &mut ExecutionActor) {
        while let Some(leased) = actor
            .context
            .store
            .lease_next_outbox(super::now_ms())
            .await
            .expect("lease outbox")
        {
            actor
                .context
                .store
                .acknowledge_outbox(leased.item.outbox_id, leased.lease_id)
                .await
                .expect("ack outbox");
        }
    }
}

async fn stage_signature_request(actor: &mut ExecutionActor) -> arena0_protocol::PendingId {
    let accepted = actor
        .dispatch_event(Event::React, DispatchSource::default())
        .await
        .expect("dispatch sign request");
    assert_eq!(accepted, Some(true));
    let state = actor.load_state().await.expect("load signing state");
    let pending = state.status().pending().expect("pending sign");
    assert_eq!(pending.operation, PendingOperation::Sign);
    pending.id
}

fn message_state() -> arena0_program::SharedStateBytes {
    arena0_program::SharedStateBytes::try_new(vec![1]).expect("message state")
}

fn message_frame(
    state: &arena0_protocol::ExecutionState,
    source: PeerId,
    sequence: u64,
    data: Vec<u8>,
) -> ExecFrame {
    let poststate = StateHash::of_shared(&message_state());
    let message_id = MessageId::derive(
        state.binding().session_id(),
        source,
        sequence,
        state.agreed_state(),
        poststate,
        &data,
    );
    ExecFrame::Message {
        message_id,
        seq: sequence,
        prestate: state.agreed_state(),
        data,
        poststate,
    }
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

    let error = actor
        .apply_message(fixture.remote_keys.peer_id(), frame, None)
        .await
        .expect_err("writer rejection");
    assert!(matches!(error, crate::ExecError::InvalidState(_)));

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
    let source = fixture.remote_keys.peer_id();
    let sequence = state.agreed_step();
    let frame = message_frame(&state, source, sequence, Vec::new());
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

    let pending = fixture
        .store
        .handle()
        .list_pending_inbox(EXEC_ID, 16)
        .await
        .expect("list pending inbox");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].source(), source);
    spawned.shutdown().await;
}

#[tokio::test]
async fn flat_dispatch_commits_local_state_in_the_resident() {
    let fixture = Fixture::new(false).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;

    let accepted = actor
        .dispatch_event(Event::React, DispatchSource::default())
        .await
        .expect("dispatch local reaction");
    assert_eq!(accepted, Some(true));
    let state = actor.load_state().await.expect("load committed state");
    assert_eq!(state.local_state().as_bytes(), &[9]);
    let instance = actor.instance.as_ref().expect("resident instance");
    assert_eq!(instance.committed_payloads().1.as_bytes(), &[9]);
}

#[tokio::test]
async fn restart_resumes_a_durable_timer_and_accepted_inbox() {
    let fixture = Fixture::with_mode(true, GuestMode::Timer).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;
    assert_eq!(
        actor
            .dispatch_event(Event::React, DispatchSource::default())
            .await
            .expect("arm timer"),
        Some(true)
    );
    drop(actor);

    let (messages, _observations) = mpsc::channel(32);
    let mut restarted = fixture.actor_with_messages(messages);
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

    let state = restarted.load_state().await.expect("load state");
    let source = fixture.remote_keys.peer_id();
    let frame = message_frame(&state, source, state.agreed_step(), vec![1, 2, 3]);
    restarted
        .context
        .store
        .accept_inbound(source, frame, 14)
        .await
        .expect("accept inbound");
    restarted
        .resolve_pending_inbox()
        .await
        .expect("resolve recovered inbox");
    assert!(
        fixture
            .store
            .handle()
            .list_pending_inbox(EXEC_ID, 16)
            .await
            .expect("load pending inbox")
            .is_empty()
    );
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
async fn restart_drains_a_durable_callout_outbox() {
    let fixture = Fixture::with_mode(false, GuestMode::Callout).await;
    let (first_messages, mut first_observations) = mpsc::channel(8);
    let mut actor = fixture
        .prepare_active_actor_with_messages(first_messages)
        .await;
    fixture.commit_session_started(&mut actor).await;
    let pending_id = {
        assert_eq!(
            actor
                .dispatch_event(Event::React, DispatchSource::default())
                .await
                .expect("dispatch callout"),
            Some(true)
        );
        actor
            .load_state()
            .await
            .expect("load callout state")
            .status()
            .pending()
            .expect("pending callout")
            .id
    };
    actor
        .drain_outbox_report()
        .await
        .expect("deliver initial callout");
    assert!(matches!(
        first_observations.recv().await.expect("initial callout message"),
        crate::SessionMessage::CalloutRequested {
            pending_id: id,
            callout_index: 0,
            context,
            expected_type,
            ..
        } if id == pending_id
            && context == b"null"
            && expected_type.as_deref() == Some("bytes")
    ));
    drop(actor);

    let (messages, mut observations) = mpsc::channel(8);
    let mut restarted = fixture.actor_with_messages(messages);
    restarted.recover().await.expect("recover callout outbox");
    let mut found = None;
    while let Ok(message) = observations.try_recv() {
        if let crate::SessionMessage::CalloutRequested { .. } = message {
            found = Some(message);
            break;
        }
    }
    let message = found.expect("recovered callout message");
    assert!(matches!(
        message,
        crate::SessionMessage::CalloutRequested {
            pending_id: id,
            callout_index: 0,
            context,
            expected_type,
            ..
        } if id == pending_id
            && context == b"null"
            && expected_type.as_deref() == Some("bytes")
    ));
    assert!(
        restarted
            .context
            .store
            .lease_next_outbox(super::now_ms())
            .await
            .expect("load outbox")
            .is_none()
    );
}

#[tokio::test]
async fn unrecoverable_callout_answer_rolls_back_and_fails_actor() {
    let fixture = Fixture::with_mode(false, GuestMode::CalloutFault).await;
    let mut setup = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut setup).await;
    drop(setup);

    let flaky_transport = Arc::new(FailFirstExecTransport::new(Arc::clone(
        &fixture.local_transport,
    )));
    let host = crate::Host::start(
        Arc::clone(&fixture.local_keys),
        Arc::clone(&flaky_transport) as Arc<dyn Transport + Sync>,
        fixture.store.handle().clone(),
    );
    let lifecycle = host.claim_execution(EXEC_ID).expect("lifecycle claim");
    let mut spawned = host
        .spawn(fixture.context(), lifecycle)
        .expect("spawn actor");

    let pending_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match spawned.message_rx.recv().await {
                Some(crate::SessionMessage::CalloutRequested { pending_id, .. }) => {
                    break pending_id;
                }
                Some(crate::SessionMessage::Failed { reason }) => {
                    panic!("actor failed before callout answer: {reason}")
                }
                Some(_) => {}
                None => panic!("observation channel closed before callout request"),
            }
        }
    })
    .await
    .expect("callout request timeout");
    let before = fixture
        .store
        .handle()
        .load_execution(EXEC_ID)
        .await
        .expect("load state before fault")
        .expect("execution before fault");
    assert_eq!(
        before
            .status()
            .pending()
            .expect("callout remains pending")
            .id,
        pending_id
    );
    assert_eq!(before.shared_state().as_bytes(), &[] as &[u8]);
    assert_eq!(before.local_state().as_bytes(), &[9]);

    let unsigned_peer_abort = AbortOccurrence::unsigned(
        fixture.activation.session_hash(),
        fixture.remote_keys.peer_id(),
        AbortKind::Fail,
        1,
        "peer failed concurrently",
        before.step_cursor(),
    )
    .expect("peer abort occurrence");
    let peer_abort = ExecFrame::Abort {
        occurrence: unsigned_peer_abort
            .clone()
            .with_signature(
                fixture.remote_keys.sign(
                    &unsigned_peer_abort
                        .signing_bytes()
                        .expect("peer abort signing bytes"),
                ),
            )
            .expect("signed peer abort"),
    };
    let remote_transport = Arc::clone(&fixture.remote_transport);
    let local_peer = fixture.local_keys.peer_id();
    let session_id = fixture.activation.session_hash();
    let terminal_receiver = tokio::spawn(async move {
        let accepted = remote_transport
            .accept_exec()
            .await
            .expect("accept failure stream");
        let recv = accepted.into_parts().1;
        let delivery = recv.recv_exec().await.expect("receive failure frame");
        assert!(matches!(delivery.frame(), ExecFrame::Abort { .. }));

        // Hold the local actor's Abort acknowledgement while sending the
        // peer's own Abort in the other direction. The actor must keep its
        // normal select loop alive to durably accept this inbound frame; a
        // private terminal-delivery loop would deadlock here.
        let reverse = remote_transport
            .open_exec(&local_peer, session_id)
            .await
            .expect("open concurrent peer failure stream");
        tokio::time::timeout(Duration::from_secs(5), reverse.send_exec(&peer_abort))
            .await
            .expect("actor stopped accepting inbound frames while delivering failure")
            .expect("concurrent peer failure was durably accepted");
        delivery
            .acknowledge()
            .expect("acknowledge durable failure responsibility");
        recv
    });
    let (reply, response) = oneshot::channel();
    spawned
        .cmd_tx
        .send(crate::ExecCommand::SubmitInput {
            pending_id,
            callout_index: 0,
            data: JsonBytes::try_new(b"null".to_vec()).expect("answer"),
            reply,
        })
        .await
        .expect("submit answer");
    let result = tokio::time::timeout(Duration::from_secs(5), response)
        .await
        .expect("answer response timeout")
        .expect("answer response");
    assert!(matches!(result, Err(crate::ExecError::Unavailable(_))));
    let _terminal_stream = terminal_receiver
        .await
        .expect("failure receiver task completed");
    assert!(flaky_transport.injected_failure_was_used());

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match spawned.message_rx.recv().await {
                Some(crate::SessionMessage::Failed { .. }) => break,
                Some(_) => {}
                None => panic!("observation channel closed before failure"),
            }
        }
    })
    .await
    .expect("durable failure observation timeout");

    let after = fixture
        .store
        .handle()
        .load_execution(EXEC_ID)
        .await
        .expect("load state after fault")
        .expect("execution after fault");
    assert_eq!(after.shared_state(), before.shared_state());
    assert_eq!(after.local_state(), before.local_state());
    assert!(after.status().is_terminal());
    assert!(matches!(
        after.status().terminal_cause(),
        Some(StopCause::Authenticated(occurrence))
            if occurrence.sender() == fixture.local_keys.peer_id()
                && occurrence.kind() == AbortKind::Fail
    ));

    spawned.shutdown().await;
    host.stop().await;
}

#[tokio::test]
async fn terminal_observation_waits_for_final_end_delivery() {
    let fixture = Fixture::with_mode(true, GuestMode::EndOnMessage).await;
    let (messages, mut observations) = mpsc::channel(8);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;

    let state = actor.load_state().await.expect("load active state");
    let frame = message_frame(
        &state,
        fixture.remote_keys.peer_id(),
        state.agreed_step(),
        b"end".to_vec(),
    );
    assert!(
        actor
            .apply_message(fixture.remote_keys.peer_id(), frame, None)
            .await
            .expect("apply terminal message")
    );
    actor
        .ensure_step_signature()
        .await
        .expect("commit local terminal-step signature");
    let state = actor.load_state().await.expect("load terminal proposal");
    let proposal = state
        .pending_shared()
        .expect("terminal proposal")
        .commitment()
        .clone();
    let remote_step = ParticipantStepSignature::new(
        fixture.remote_keys.peer_id(),
        proposal.step,
        fixture
            .remote_execution_key()
            .sign(&proposal.signing_bytes()),
    );
    actor
        .context
        .store
        .accept_inbound(
            fixture.remote_keys.peer_id(),
            ExecFrame::StepSignature {
                commitment: proposal,
                signature: remote_step.signature().sig,
            },
            19,
        )
        .await
        .expect("accept remote terminal-step signature");
    let inbox = actor
        .context
        .store
        .list_pending_inbox(8)
        .await
        .expect("list terminal-step inbox")
        .into_iter()
        .next()
        .expect("remote terminal-step signature inbox");
    actor
        .context
        .store
        .commit_step_signature(state.version(), remote_step, Some(inbox.inbox_id()), 19)
        .await
        .expect("commit remote terminal-step signature");
    actor
        .reload_resident()
        .await
        .expect("promote terminal-step memories");

    actor
        .ensure_terminal_signature()
        .await
        .expect("commit local terminal signature");
    let state = actor
        .load_state()
        .await
        .expect("load pending terminal proof");
    let commitment = state
        .pending_terminal()
        .expect("pending terminal commitment")
        .clone();
    let remote_signature = ParticipantTerminalSignature::new(
        fixture.remote_keys.peer_id(),
        fixture
            .remote_execution_key()
            .sign(&commitment.signing_bytes()),
    );
    actor
        .context
        .store
        .accept_inbound(
            fixture.remote_keys.peer_id(),
            ExecFrame::End {
                commitment,
                signature: remote_signature.signature(),
            },
            20,
        )
        .await
        .expect("accept remote terminal signature");
    let inbox = actor
        .context
        .store
        .list_pending_inbox(8)
        .await
        .expect("list terminal inbox")
        .into_iter()
        .next()
        .expect("remote terminal signature inbox");
    actor
        .context
        .store
        .commit_terminal_signature(
            state.version(),
            remote_signature,
            Some(inbox.inbox_id()),
            20,
        )
        .await
        .expect("commit remote terminal signature");

    // Receipt publication may finish locally, but progress must retain the
    // actor's final End send and withhold terminal observations until the
    // remote transport grants durable responsibility.
    actor.progress().await.expect("start final End delivery");
    assert!(actor.inflight_send.is_some());
    assert!(
        actor
            .context
            .store
            .has_unsettled_frames()
            .await
            .expect("inspect final frame")
    );
    assert!(observations.try_recv().is_err());

    let accepted = fixture
        .remote_transport
        .accept_exec()
        .await
        .expect("accept final End stream");
    let recv = accepted.into_parts().1;
    let step_delivery = recv
        .recv_exec()
        .await
        .expect("receive terminal-step signature");
    assert!(matches!(
        step_delivery.frame(),
        ExecFrame::StepSignature { .. }
    ));
    assert!(observations.try_recv().is_err());
    step_delivery
        .acknowledge()
        .expect("acknowledge terminal-step responsibility");

    let joined = actor
        .inflight_send
        .as_mut()
        .expect("in-flight send")
        .wait()
        .await;
    actor
        .settle_inflight_send(joined)
        .await
        .expect("settle terminal-step delivery");
    actor.progress().await.expect("start final End delivery");
    let end_delivery = recv.recv_exec().await.expect("receive final End");
    assert!(matches!(end_delivery.frame(), ExecFrame::End { .. }));
    assert!(observations.try_recv().is_err());
    end_delivery
        .acknowledge()
        .expect("acknowledge durable End responsibility");

    let joined = actor
        .inflight_send
        .as_mut()
        .expect("in-flight send")
        .wait()
        .await;
    actor
        .settle_inflight_send(joined)
        .await
        .expect("settle final End delivery");
    actor
        .progress()
        .await
        .expect("publish terminal observation");
    assert!(
        !actor
            .context
            .store
            .has_unsettled_frames()
            .await
            .expect("final frame settled")
    );
    assert!(matches!(
        observations.recv().await,
        Some(crate::SessionMessage::ReceiptPublished { .. })
    ));
    assert!(matches!(
        observations.recv().await,
        Some(crate::SessionMessage::Completed { .. })
    ));
}

#[tokio::test]
async fn stale_abort_is_rejected_across_restart_and_signed_proposal_still_commits() {
    let fixture = Fixture::with_mode(false, GuestMode::Broadcast).await;
    let mut actor = fixture.prepare_active_actor().await;
    let stale_cursor = actor
        .load_state()
        .await
        .expect("load pre-session state")
        .step_cursor();
    fixture.commit_session_started(&mut actor).await;
    assert_eq!(
        actor
            .dispatch_event(Event::React, DispatchSource::default())
            .await
            .expect("stage broadcast proposal"),
        Some(true)
    );
    actor
        .ensure_step_signature()
        .await
        .expect("sign local proposal");
    let proposed = actor.load_state().await.expect("load signed proposal");
    let proposal = proposed
        .pending_shared()
        .expect("pending proposal after local signature");
    assert_eq!(proposal.signature_count(), 1);
    let commitment = proposal.commitment().clone();

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
        .context
        .store
        .accept_inbound(
            fixture.remote_keys.peer_id(),
            ExecFrame::Abort { occurrence },
            20,
        )
        .await
        .expect("accept stale abort");
    drop(actor);

    let (messages, _observations) = mpsc::channel(32);
    let mut restarted = fixture.actor_with_messages(messages);
    restarted.recover().await.expect("recover signed proposal");
    assert!(
        restarted
            .context
            .store
            .list_pending_inbox(8)
            .await
            .expect("list inbox after recovery")
            .is_empty()
    );
    let recovered = restarted
        .load_state()
        .await
        .expect("load recovered proposal");
    assert_eq!(
        recovered
            .pending_shared()
            .expect("signed proposal survives stale abort")
            .commitment(),
        &commitment
    );

    restarted
        .context
        .store
        .accept_inbound(
            fixture.remote_keys.peer_id(),
            ExecFrame::StepSignature {
                commitment: commitment.clone(),
                signature: fixture
                    .remote_execution_key()
                    .sign(&commitment.signing_bytes()),
            },
            21,
        )
        .await
        .expect("accept final step signature");
    restarted
        .resolve_pending_inbox()
        .await
        .expect("commit final signature");
    let committed = restarted.load_state().await.expect("load committed state");
    assert!(committed.pending_shared().is_none());
    assert_eq!(committed.agreed_step(), 2);
}

#[tokio::test]
async fn retry_input_from_message_preserves_callout_without_source_pending_id() {
    let fixture = Fixture::with_mode(true, GuestMode::CalloutRetryMessage).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;
    assert_eq!(
        actor
            .dispatch_event(Event::React, DispatchSource::default())
            .await
            .expect("dispatch callout"),
        Some(true)
    );
    let before = actor.load_state().await.expect("load callout state");
    let pending_id = before.status().pending().expect("callout pending").id;
    // MessageReceived has no pending source id. RetryInput is nevertheless
    // valid because the durable continuation is a Callout.
    let source = fixture.remote_keys.peer_id();
    let sequence = before.agreed_step();
    let poststate = StateHash::of_shared(before.shared_state());
    let data = vec![4, 5, 6];
    let message_id = MessageId::derive(
        before.binding().session_id(),
        source,
        sequence,
        before.agreed_state(),
        poststate,
        &data,
    );
    let frame = ExecFrame::Message {
        message_id,
        seq: sequence,
        prestate: before.agreed_state(),
        data,
        poststate,
    };
    assert!(
        actor
            .apply_message(source, frame, None)
            .await
            .expect("retry from message")
    );
    let after = actor.load_state().await.expect("load retried state");
    assert_eq!(
        after.status().pending().expect("callout preserved").id,
        pending_id
    );
    assert_eq!(after.shared_state(), before.shared_state());
    assert_eq!(after.local_state(), before.local_state());
}

#[tokio::test]
async fn crash_before_signature_application_is_recovered_inside_actor() {
    let fixture = Fixture::with_mode(false, GuestMode::Sign).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;
    let pending_id = stage_signature_request(&mut actor).await;
    let leased = actor
        .context
        .store
        .lease_next_outbox(super::now_ms())
        .await
        .expect("lease signature request")
        .expect("signature request outbox");
    assert_eq!(leased.item.payload_kind, OutboxPayloadKind::Effect);
    let effect: Effect = borsh::from_slice(&leased.item.payload).expect("decode sign effect");
    assert!(matches!(
        effect,
        Effect::Sign { ref data, .. } if data == b"sig"
    ));
    assert_eq!(
        arena0_protocol::pending_id(EXEC_ID, leased.item.event_position, leased.item.ordinal),
        pending_id
    );
    let lease_until = leased.lease_until_ms;
    drop(actor);

    let mut recovery_store = fixture
        .store
        .handle()
        .claim_execution(EXEC_ID)
        .expect("reclaim execution after crash");
    recovery_store
        .recover_expired_leases(lease_until)
        .await
        .expect("recover signature lease");
    drop(recovery_store);

    let (messages, _observations) = mpsc::channel(8);
    let mut restarted = fixture.actor_with_messages(messages);
    restarted.recover().await.expect("recover signature outbox");
    let state = restarted.load_state().await.expect("load resumed state");
    assert!(state.status().pending().is_none());
    assert_eq!(state.local_state().as_bytes(), &[9]);
    assert!(
        restarted
            .context
            .store
            .lease_next_outbox(super::now_ms())
            .await
            .expect("check acknowledged signature request")
            .is_none()
    );
}

#[tokio::test]
async fn crash_after_signature_application_is_idempotently_acknowledged() {
    let fixture = Fixture::with_mode(false, GuestMode::Sign).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;
    let pending_id = stage_signature_request(&mut actor).await;
    let leased = actor
        .context
        .store
        .lease_next_outbox(super::now_ms())
        .await
        .expect("lease signature request")
        .expect("signature request outbox");
    let effect: Effect = borsh::from_slice(&leased.item.payload).expect("decode sign effect");
    let Effect::Sign {
        scheme,
        data: payload,
        ..
    } = effect
    else {
        panic!("expected sign effect");
    };
    let guest_data = GuestSignData::new(
        fixture.activation.session_hash(),
        Program::parse(fixture.wasm.clone())
            .expect("program")
            .hash(),
        EXEC_ID,
        leased.item.event_position,
        leased.item.ordinal,
        scheme,
        payload,
    )
    .expect("guest sign data");
    assert_eq!(
        arena0_protocol::pending_id(EXEC_ID, leased.item.event_position, leased.item.ordinal),
        pending_id
    );
    assert!(
        actor
            .sign_and_resume(pending_id, &guest_data)
            .await
            .expect("apply signature before crash")
    );
    assert!(
        actor
            .load_state()
            .await
            .expect("load applied state")
            .status()
            .pending()
            .is_none()
    );
    let lease_until = leased.lease_until_ms;
    drop(actor);

    let mut recovery_store = fixture
        .store
        .handle()
        .claim_execution(EXEC_ID)
        .expect("reclaim execution after crash");
    recovery_store
        .recover_expired_leases(lease_until)
        .await
        .expect("recover signature lease");
    drop(recovery_store);

    let (messages, _observations) = mpsc::channel(8);
    let mut restarted = fixture.actor_with_messages(messages);
    restarted
        .recover()
        .await
        .expect("recover applied signature");
    let state = restarted.load_state().await.expect("load resumed state");
    assert!(state.status().pending().is_none());
    assert!(
        restarted
            .context
            .store
            .lease_next_outbox(super::now_ms())
            .await
            .expect("check acknowledged signature request")
            .is_none()
    );
}

#[tokio::test]
async fn progress_consumes_sign_and_signs_followup_proposal_without_another_tick() {
    let fixture = Fixture::with_mode(false, GuestMode::SignThenBroadcast).await;
    let (messages, _observations) = mpsc::channel(8);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    let _pending_id = stage_signature_request(&mut actor).await;

    // One progress pass must drain the durable Sign effect, dispatch Signed,
    // stage the shared result emitted by that continuation, and add this
    // participant's step signature. A second ticker iteration would hide a
    // missing Sign trampoline.
    actor
        .progress()
        .await
        .expect("progress signed continuation");

    let state = actor.load_state().await.expect("load follow-up proposal");
    let proposal = state
        .pending_shared()
        .expect("signed continuation stages a proposal");
    assert!(matches!(
        proposal.entry().event,
        Event::MessageReceived { ref msg, .. } if msg == b"broadcast"
    ));
    assert_eq!(proposal.signature_count(), 1);
    assert!(proposal.status().pending().is_none());
    let requests = actor
        .context
        .store
        .pending_requests()
        .await
        .expect("inspect consumed sign request");
    assert!(
        requests.is_empty(),
        "consumed sign request remained observable: {requests:?}; status={:?}",
        state.status()
    );
}

#[tokio::test]
async fn inbound_transport_ack_follows_durable_acceptance() {
    let fixture = Fixture::new(true).await;
    let (messages, _observations) = mpsc::channel(32);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    let state = actor.load_state().await.expect("load active state");
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

    sender
        .await
        .expect("sender task")
        .expect("durable receiver acknowledgement");
    let pending = fixture
        .store
        .handle()
        .list_pending_inbox(EXEC_ID, 16)
        .await
        .expect("list accepted inbox");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].source(), source);
    assert!(
        matches!(pending[0].frame(), ExecFrame::Message { seq, poststate, .. } if *seq == sequence && *poststate == StateHash::of_shared(&message_state()))
    );

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

    let state = actor.load_state().await.expect("load session state");
    let source = fixture.remote_keys.peer_id();
    let sequence = state.agreed_step();
    assert!(
        actor
            .apply_message(
                source,
                message_frame(&state, source, sequence, vec![1, 2, 3]),
                None,
            )
            .await
            .expect("stage current proposal")
    );

    let state = actor.load_state().await.expect("load current proposal");
    let mut future_commitment = state
        .pending_shared()
        .expect("current proposal")
        .commitment()
        .clone();
    future_commitment.step += 1;
    let frame = ExecFrame::StepSignature {
        signature: fixture
            .remote_execution_key()
            .sign(&future_commitment.signing_bytes()),
        commitment: future_commitment,
    };
    actor
        .context
        .store
        .accept_inbound(source, frame, 20)
        .await
        .expect("accept future signature");
    actor
        .resolve_pending_inbox()
        .await
        .expect("defer future signature");

    let pending = fixture
        .store
        .handle()
        .list_pending_inbox(EXEC_ID, 16)
        .await
        .expect("load deferred signature");
    assert_eq!(pending.len(), 1);
}

#[tokio::test]
async fn trace_observation_waits_for_the_certified_step() {
    let fixture = Fixture::new(true).await;
    let (messages, mut observations) = mpsc::channel(8);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;

    let state = actor.load_state().await.expect("load session state");
    let source = fixture.remote_keys.peer_id();
    let sequence = state.agreed_step();
    assert!(
        actor
            .apply_message(
                source,
                message_frame(&state, source, sequence, vec![1, 2, 3]),
                None,
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
        .load_state()
        .await
        .expect("load partial proposal")
        .pending_shared()
        .expect("pending proposal")
        .commitment()
        .clone();
    actor
        .context
        .store
        .accept_inbound(
            source,
            ExecFrame::StepSignature {
                commitment: proposal.clone(),
                signature: fixture
                    .remote_execution_key()
                    .sign(&proposal.signing_bytes()),
            },
            20,
        )
        .await
        .expect("accept remote signature");
    actor
        .resolve_pending_inbox()
        .await
        .expect("resolve remote signature");

    assert!(matches!(
        observations.try_recv(),
        Ok(crate::SessionMessage::TraceAppended { step }) if step == sequence
    ));
    assert!(
        observations.try_recv().is_err(),
        "one certified commit must emit exactly one step"
    );
    let state = actor.load_state().await.expect("load certified state");
    assert_eq!(state.agreed_step(), sequence + 1);
}

#[tokio::test]
async fn broadcast_outbox_has_only_remote_destinations_and_no_self_apply() {
    let fixture = Fixture::with_mode(false, GuestMode::Broadcast).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;
    let before = actor
        .load_state()
        .await
        .expect("load state before broadcast");
    assert_eq!(
        actor
            .dispatch_event(Event::React, DispatchSource::default())
            .await
            .expect("dispatch broadcast"),
        Some(true)
    );

    let state = actor.load_state().await.expect("load broadcast proposal");
    assert!(state.pending_shared().is_some());
    assert_eq!(state.agreed_step(), before.agreed_step());
    let leased = actor
        .context
        .store
        .lease_next_outbox(super::now_ms())
        .await
        .expect("lease broadcast")
        .expect("remote broadcast row");
    assert_eq!(leased.item.payload_kind, OutboxPayloadKind::Frame);
    assert_eq!(leased.item.destination, Some(fixture.remote_keys.peer_id()));
    let frame: ExecFrame = borsh::from_slice(&leased.item.payload).expect("decode broadcast frame");
    assert!(matches!(
        frame,
        ExecFrame::Message {
            seq,
            poststate,
            ..
        } if seq == before.agreed_step() && poststate == before.agreed_state()
    ));
    assert_eq!(
        state.event_position(),
        before.event_position().saturating_add(1)
    );
    actor
        .context
        .store
        .acknowledge_outbox(leased.item.outbox_id, leased.lease_id)
        .await
        .expect("ack remote broadcast");
    assert!(
        actor
            .context
            .store
            .lease_next_outbox(super::now_ms())
            .await
            .expect("check no self broadcast")
            .is_none()
    );
}

fn activation(
    program_hash: ProgramHash,
    local_keys: &NodeKeys,
    remote_keys: &NodeKeys,
    local_salt: &ExecutionSalt,
    remote_salt: &ExecutionSalt,
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
        StateHash::of_shared(&SharedStateBytes::try_new(Vec::new()).expect("empty shared state")),
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
    let unit = unit_schema();
    let capabilities = match mode {
        GuestMode::Plain | GuestMode::EndOnMessage => Vec::new(),
        GuestMode::Timer => vec![Capability::Timers],
        GuestMode::Callout | GuestMode::CalloutFault => vec![Capability::Input],
        GuestMode::CalloutRetryMessage => vec![Capability::Input],
        GuestMode::Sign => vec![Capability::Sign {
            schemes: vec![SignScheme::Ed25519],
        }],
        GuestMode::SignThenBroadcast => vec![
            Capability::Sign {
                schemes: vec![SignScheme::Ed25519],
            },
            Capability::Messaging,
        ],
        GuestMode::Broadcast => vec![Capability::Messaging],
    };
    let callouts = match mode {
        GuestMode::Callout | GuestMode::CalloutFault | GuestMode::CalloutRetryMessage => {
            vec![CalloutSchema {
                name: "request".into(),
                prompt: "request".into(),
                input: unit.clone(),
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
        GuestMode::Timer => r#"(import "arena0" "set_timer" (func $set_timer (param i64)))"#,
        GuestMode::Callout | GuestMode::CalloutFault => {
            r#"(import "arena0" "request_input"
            (func $request_input (param i32 i32 i32 i32 i32)))"#
        }
        GuestMode::CalloutRetryMessage => {
            r#"(import "arena0" "request_input"
            (func $request_input (param i32 i32 i32 i32 i32)))
            (import "arena0" "retry_input" (func $retry_input (param i32 i32)))"#
        }
        GuestMode::Sign => {
            r#"(import "arena0" "sign"
            (func $sign (param i32 i32 i32 i32 i32)))"#
        }
        GuestMode::SignThenBroadcast => {
            r#"(import "arena0" "sign"
            (func $sign (param i32 i32 i32 i32 i32)))
            (import "arena0" "broadcast" (func $broadcast (param i32 i32)))"#
        }
        GuestMode::Broadcast => {
            r#"(import "arena0" "broadcast" (func $broadcast (param i32 i32)))"#
        }
        GuestMode::EndOnMessage => {
            r#"(import "arena0" "end_session" (func $end_session (param i32 i32)))"#
        }
        GuestMode::Plain => "",
    };
    let react_body = match mode {
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
              call $set_timer
            "#
        }
        GuestMode::Callout | GuestMode::CalloutFault => {
            r#"
              i32.const 1
              i32.const 1040
              i32.const 1
              call $state_write
              i32.const 0
              i32.const 1080
              i32.const 4
              i32.const 1070
              i32.const 5
              call $request_input
            "#
        }
        GuestMode::CalloutRetryMessage => {
            r#"
              i32.const 1
              i32.const 1040
              i32.const 1
              call $state_write
              i32.const 0
              i32.const 1080
              i32.const 4
              i32.const 1070
              i32.const 5
              call $request_input
            "#
        }
        GuestMode::Sign | GuestMode::SignThenBroadcast => {
            r#"
              i32.const 1
              i32.const 1040
              i32.const 1
              call $state_write
              i32.const 0
              i32.const 1050
              i32.const 3
              i32.const 1070
              i32.const 5
              call $sign
            "#
        }
        GuestMode::Broadcast => {
            r#"
              i32.const 1
              i32.const 1040
              i32.const 1
              call $state_write
              i32.const 1110
              i32.const 9
              call $broadcast
            "#
        }
        GuestMode::EndOnMessage => "",
    };
    let session_started_body = "";
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
        _ => "",
    };
    let message_body = match mode {
        GuestMode::CalloutRetryMessage => {
            r#"
              i32.const 1050
              i32.const 3
              call $retry_input
            "#
        }
        GuestMode::EndOnMessage => {
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
    let signed_body = match mode {
        GuestMode::SignThenBroadcast => {
            r#"
              i32.const 0
              i32.const 1040
              i32.const 1
              call $state_write
              i32.const 1110
              i32.const 9
              call $broadcast
            "#
        }
        _ => "",
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
          (data (i32.const 2000) "{writer}")
          (data (i32.const 3000) "\00\00\00\00\00\00\00\00")
          (data (i32.const 32768) "\00")
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
                i32.const 5
                i32.eq
                if
                  {signed_body}
                else
                  local.get $event_ptr
                  i32.load8_u
                  i32.const 2
                  i32.eq
                  if
                    {input_fault_body}
                  else
                    local.get $event_ptr
                    i32.load8_u
                    i32.const 6
                    i32.eq
                    if
                      {react_body}
                    end
                  end
                end
              end
            end
            i32.const 32768
            i32.const 1
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
        react_body = react_body,
        session_started_body = session_started_body,
        signed_body = signed_body,
        input_fault_body = input_fault_body,
        message_body = message_body,
    );
    let raw = wat::parse_str(wat).expect("wat");
    WasmtimeEngine::new()
        .expect("sandbox engine")
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
