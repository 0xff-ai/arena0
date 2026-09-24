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
    ExecutionAdmission, ExecutionStatus, MessageId, NegotiationId, Offer, OfferData,
    ParticipantStepSignature, PeerId, PeerIdSource, PendingId, PreparedActivation, StateHash,
    Ticket, TicketAction, TicketData, TicketHash,
};
use arena0_sandbox::{InitializeCall, LoadedProgram, Program, WasmtimeEngine};
use arena0_store::{ApplyOutcome, OutboxPayloadKind, Store, StoreConfig};
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
    LocalSign,
    SignOnMessage,
    Broadcast,
    RejectMessage,
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
        let initialized = WasmtimeEngine::new()
            .expect("sandbox engine")
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
        assert_eq!(accepted, DispatchOutcome::Committed);
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

    let applied = actor
        .apply_message(fixture.remote_keys.peer_id(), frame, None)
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
async fn valid_writer_message_divergence_preserves_the_unconsumed_inbox() {
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
        let before = actor.load_state().await.expect("active state");
        let source = fixture.remote_keys.peer_id();
        // The identity is valid even when the advertised result is wrong.
        let poststate = before.agreed_state();
        let data = b"writer message".to_vec();
        let frame = ExecFrame::Message {
            message_id: MessageId::derive(
                before.binding().session_id(),
                source,
                before.agreed_step(),
                before.agreed_state(),
                poststate,
                &data,
            ),
            seq: before.agreed_step(),
            prestate: before.agreed_state(),
            data,
            poststate,
        };
        actor
            .context
            .store
            .accept_inbound(source, frame, 20)
            .await
            .expect("accept frame");
        let error = actor.resolve_pending_inbox().await.expect_err("divergence");
        let crate::ExecError::Diverged(reason) = &error else {
            panic!("expected divergence, got {error:?}");
        };
        assert!(reason.starts_with("diverged at step 1:"), "{reason}");
        assert!(reason.contains(cause), "{reason}");
        assert!(reason.len() <= arena0_protocol::MAX_TERMINAL_REASON_BYTES);
        assert_eq!(actor.load_state().await.expect("unchanged state"), before);
        assert_eq!(
            actor
                .context
                .store
                .list_pending_inbox(8)
                .await
                .expect("pending inbox")
                .len(),
            1
        );
        assert!(
            actor.fail_terminal(error).await,
            "failure must permit final delivery"
        );
        let stopped = actor.load_state().await.expect("failed state");
        assert!(stopped.status().is_terminal());
        assert_eq!(stopped.step_cursor(), before.step_cursor());
    }
}

#[tokio::test]
async fn invalid_messages_are_dropped_before_the_guest_can_diverge() {
    for invalid in ["stale", "prestate", "message_id", "writer"] {
        let fixture = Fixture::with_mode(invalid != "writer", GuestMode::RejectMessage).await;
        let (messages, _observations) = mpsc::channel(8);
        let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
        fixture.commit_session_started(&mut actor).await;
        let before = actor.load_state().await.expect("active state");
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
        let message_id = MessageId::derive(
            before.binding().session_id(),
            source,
            seq,
            prestate,
            poststate,
            &data,
        );
        let frame = ExecFrame::Message {
            message_id,
            seq,
            prestate,
            data: if invalid == "message_id" {
                b"different payload".to_vec()
            } else {
                data
            },
            poststate,
        };
        let accepted = actor
            .context
            .store
            .accept_inbound(source, frame.clone(), 20)
            .await;
        if invalid == "message_id" {
            assert!(matches!(
                accepted,
                Err(arena0_store::StoreError::UnauthenticatedSource(_))
            ));
            assert!(
                !actor
                    .apply_message(source, frame, None)
                    .await
                    .expect("drop mismatched id")
            );
        } else {
            accepted.expect("accept frame");
            actor
                .resolve_pending_inbox()
                .await
                .expect("drop invalid frame");
        }
        assert_eq!(
            actor.load_state().await.expect("unchanged state"),
            before,
            "{invalid}"
        );
        assert!(
            actor
                .context
                .store
                .list_pending_inbox(8)
                .await
                .expect("inbox")
                .is_empty(),
            "{invalid}"
        );
    }
}

#[tokio::test]
async fn peer_divergence_ends_the_writer_after_it_signed_its_proposal() {
    let writer = Fixture::with_mode(false, GuestMode::RejectMessage).await;
    let receiver = Fixture::from_wasm_with_peer(writer.wasm.clone(), Some(&writer)).await;
    for fixture in [&writer, &receiver] {
        let (messages, _observations) = mpsc::channel(16);
        let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
        fixture.commit_session_started(&mut actor).await;
        if fixture.local_keys.peer_id() == writer.local_keys.peer_id() {
            assert_eq!(
                actor
                    .dispatch_event(Event::React, DispatchSource::default())
                    .await
                    .expect("writer proposal"),
                DispatchOutcome::Committed
            );
            actor
                .ensure_step_signature()
                .await
                .expect("sign writer proposal");
            let state = actor.load_state().await.expect("signed proposal");
            let signatures = state
                .pending_shared()
                .expect("staged proposal")
                .signatures();
            assert_eq!(signatures.len(), 1);
            assert_eq!(signatures[0].participant(), writer.local_keys.peer_id());
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
                        assert!(
                            reason.contains(
                                "diverged at step 1: program rejected the writer message"
                            ),
                            "{reason}"
                        );
                        break;
                    }
                    crate::SessionMessage::Completed { .. }
                    | crate::SessionMessage::Aborted { .. } => {
                        panic!("expected authenticated failure")
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
    assert_eq!(accepted, DispatchOutcome::Committed);
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
        DispatchOutcome::Committed
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
    assert_eq!(state.local_state().as_bytes(), &[8]);
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
async fn recovered_sdk_timers_dispatch_typed_and_unit_payloads() {
    for stem in ["timer_dispatch_typed", "timer_dispatch_unit"] {
        let fixture = Fixture::with_guest(stem).await;
        let mut actor = fixture.prepare_active_actor().await;
        fixture.commit_session_started(&mut actor).await;
        assert_eq!(
            actor
                .dispatch_event(Event::React, DispatchSource::default())
                .await
                .expect("schedule timer"),
            DispatchOutcome::Committed,
            "{stem} React dispatch"
        );
        drop(actor);

        let (messages, _observations) = mpsc::channel(8);
        let mut restarted = fixture.actor_with_messages(messages);
        restarted
            .fire_due_timers()
            .await
            .expect("fire recovered timer");

        let state = restarted.load_state().await.expect("load timer state");
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
                .dispatch_event(Event::React, DispatchSource::default())
                .await
                .expect("dispatch callout"),
            DispatchOutcome::Committed
        );
        actor
            .load_state()
            .await
            .expect("load callout state")
            .callout()
            .expect("pending callout")
            .id
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
    let mut restarted = fixture.actor_with_messages(messages);
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
async fn react_runs_with_an_open_callout_and_replaces_the_question() {
    let fixture = Fixture::with_mode(false, GuestMode::Callout).await;
    let (messages, _observations) = mpsc::channel(32);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    assert_eq!(
        actor
            .dispatch_event(
                Event::TimerFired {
                    timer: arena0_protocol::TimerPayload::unit()
                },
                DispatchSource::default(),
            )
            .await
            .unwrap(),
        DispatchOutcome::Committed
    );
    let before = actor.load_state().await.unwrap();
    let first = before.callout().unwrap().clone();
    assert_eq!(first.context, b"true");
    actor.progress().await.unwrap();
    let after = actor.load_state().await.unwrap();
    assert_eq!(after.last_reacted_step(), Some(after.agreed_step() - 1));
    assert_eq!(after.callout().unwrap().context, b"null");
    assert_ne!(after.callout().unwrap().id, first.id);
    assert!(!after.status().is_terminal());
    assert!(matches!(
        actor
            .submit_input(first.id, JsonBytes::try_new(b"null".to_vec()).unwrap())
            .await,
        Err(SubmitInputError::Expected(
            crate::ExecError::CalloutNotPending
        ))
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
        .dispatch_event(Event::React, DispatchSource::default())
        .await
        .unwrap();
    let before = actor.load_state().await.unwrap();
    let open = before.callout().unwrap().clone();
    let source = fixture.remote_keys.peer_id();
    let frame = message_frame(&before, source, before.agreed_step(), vec![1, 2, 3]);
    actor
        .context
        .store
        .accept_inbound(source, frame, 14)
        .await
        .unwrap();
    actor.resolve_pending_inbox().await.unwrap();
    let staged = actor.load_state().await.unwrap();
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
    assert_eq!(actor.load_state().await.unwrap(), staged);
}

#[tokio::test]
async fn rejected_callout_answer_preserves_pending_continuation_until_valid_input() {
    let fixture = Fixture::with_mode(false, GuestMode::CalloutReject).await;
    let (messages, _observations) = mpsc::channel(32);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    assert_eq!(
        actor
            .dispatch_event(Event::React, DispatchSource::default())
            .await
            .expect("dispatch callout"),
        DispatchOutcome::Committed
    );

    let before = actor.load_state().await.expect("load pending state");
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

    let after_rejection = actor.load_state().await.expect("load rejected state");
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
    let committed = actor.load_state().await.expect("load committed answer");
    assert!(committed.callout().is_none());
    assert!(!committed.status().is_terminal());
}

#[tokio::test]
async fn input_handler_trap_rejects_without_ending_session() {
    let fixture = Fixture::with_mode(false, GuestMode::CalloutFault).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;
    assert_eq!(
        actor
            .dispatch_event(Event::React, DispatchSource::default())
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
        let before = actor.load_state().await.expect("agreed prefix");
        let frame = message_frame(
            &before,
            fixture.remote_keys.peer_id(),
            before.agreed_step(),
            vec![1; arena0_protocol::MAX_EFFECT_PAYLOAD_BYTES],
        );
        actor
            .context
            .store
            .accept_inbound(
                fixture.remote_keys.peer_id(),
                frame.clone(),
                super::now_ms(),
            )
            .await
            .expect("accept writer message");
        let inbox = actor
            .context
            .store
            .list_pending_inbox(8)
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.frame() == &frame)
            .unwrap();
        let result = actor
            .apply_message(fixture.remote_keys.peer_id(), frame, Some(inbox.inbox_id()))
            .await;
        if let Err(error @ crate::ExecError::ReceiptBudgetExhausted { .. }) = result {
            assert_eq!(actor.load_state().await.unwrap(), before);
            assert!(before.agreed_step() > 1);
            assert!(actor.fail_terminal(error).await);
            break;
        }
        assert!(result.expect("dispatch within budget"));
        actor
            .ensure_step_signature()
            .await
            .expect("local signature");
        let staged = actor.load_state().await.unwrap();
        let commitment = staged.pending_shared().unwrap().commitment().clone();
        actor
            .context
            .store
            .accept_inbound(
                fixture.remote_keys.peer_id(),
                ExecFrame::StepSignature {
                    signature: fixture
                        .remote_execution_key()
                        .sign(&commitment.signing_bytes()),
                    commitment,
                },
                super::now_ms(),
            )
            .await
            .expect("accept peer signature");
        actor.resolve_pending_inbox().await.expect("certify step");
        assert!(actor.load_state().await.unwrap().pending_shared().is_none());
        fixture.clear_outbox(&mut actor).await;
    }
    let stopped = actor.load_state().await.unwrap();
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
async fn terminal_observation_waits_for_final_step_delivery() {
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

    let ended = actor.load_state().await.expect("load certified end");
    assert!(matches!(ended.status(), ExecutionStatus::Ended { .. }));
    assert!(
        actor
            .fail_terminal(crate::ExecError::Unavailable("peer disconnected".into()))
            .await
    );
    let published = actor.load_state().await.expect("load preserved completion");
    assert!(matches!(
        published.status(),
        ExecutionStatus::Completed { .. }
    ));
    assert_eq!(published.terminal_outcome(), ended.terminal_outcome());

    // Receipt publication may finish locally, but progress must retain the
    // actor's final step-signature send and withhold terminal observations until the
    // remote transport grants durable responsibility.
    actor.progress().await.expect("start final step delivery");
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
        .expect("accept final step stream");
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
        DispatchOutcome::Committed
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
async fn react_handler_signs_with_the_participant_identity_key() {
    let fixture = Fixture::with_mode(false, GuestMode::LocalSign).await;
    let (messages, _observations) = mpsc::channel(8);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    let position = actor
        .load_state()
        .await
        .expect("load pre-sign state")
        .event_position();
    assert_eq!(
        actor
            .dispatch_event(Event::React, DispatchSource::default())
            .await
            .expect("dispatch local sign"),
        DispatchOutcome::Committed
    );
    let state = actor.load_state().await.expect("load signed state");
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
    let state = actor.load_state().await.expect("load active state");
    let source = fixture.local_keys.peer_id();
    let sequence = state.agreed_step();
    let frame = message_frame(&state, source, sequence, vec![4, 5, 6]);
    let error = actor
        .apply_message(source, frame, None)
        .await
        .expect_err("a message handler must not reach the signer");
    assert!(
        matches!(&error, crate::context::ExecError::Diverged(reason)
            if reason.contains("diverged at step 1") && reason.contains("message handler trapped")),
        "{error:?}"
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
        DispatchOutcome::Committed
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
        GuestMode::Plain | GuestMode::EndOnMessage => Vec::new(),
        GuestMode::Timer => vec![Capability::Timers],
        GuestMode::Callout | GuestMode::CalloutFault | GuestMode::CalloutReject => Vec::new(),
        GuestMode::LocalSign | GuestMode::SignOnMessage => vec![Capability::Sign {
            schemes: vec![SignScheme::Ed25519],
        }],
        GuestMode::Broadcast | GuestMode::RejectMessage => vec![Capability::Messaging],
    };
    let callouts = match mode {
        GuestMode::Callout | GuestMode::CalloutFault | GuestMode::CalloutReject => {
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
              i32.const 1090
              i32.const 4
              i32.const 1070
              i32.const 5
              call $set_timer
            "#
        }
        GuestMode::Callout | GuestMode::CalloutFault | GuestMode::CalloutReject => {
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
            "#
        }
        GuestMode::EndOnMessage | GuestMode::SignOnMessage => "",
    };
    let session_started_body = "";
    let react_body = if matches!(mode, GuestMode::RejectMessage) {
        // Both participants run React; only the designated writer broadcasts.
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
          {react_body}
        "#,
            i64::from_le_bytes(writer_peer.expect("writer").0[..8].try_into().unwrap())
        )
    } else {
        react_body.to_owned()
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
    let timer_body = match mode {
        GuestMode::Callout => {
            r#"
              i32.const 33100
              i32.const 15
              call $pack
              return
            "#
        }
        GuestMode::Timer => {
            r#"
              i32.const 1
              i32.const 1140
              i32.const 1
              call $state_write
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
                else
                  local.get $event_ptr
                  i32.load8_u
                  i32.const 4
                  i32.eq
                  if
                    {react_body}
                  end
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
        react_body = react_body,
        session_started_body = session_started_body,
        input_fault_body = input_fault_body,
        message_body = message_body,
        timer_body = timer_body,
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
