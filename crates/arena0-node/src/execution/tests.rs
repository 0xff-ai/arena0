use std::collections::HashMap;
use std::sync::Arc;

use arena0_crypto::bls::BlsSecretKey;
use arena0_crypto::{
    ExecutionKey, ExecutionSalt, NodeKeys, SecretKey, SignScheme, key_binding_message,
};
use arena0_program::{
    JsonBytes, JsonSchemaDocument, ProgramDefinition, ProgramHash, ProgramMetadata, ProgramSchema,
    StateSchema,
};
use arena0_protocol::execution::PrivateContext;
use arena0_protocol::{
    Activation, ActivationData, AggregateAttestation, DurableEffect, Ensemble, ExecFrame, ExecId,
    ExecutionAdmission, ExecutionInput, MessageId, NegotiationId, Offer, OfferData, PeerId,
    PeerIdSource, PreparedActivation, PrivateCause, PrivateDelta, PrivateEffect, PrivateEvent,
    PrivateRecord, PublicEvent, SharedDelta, StateHash, TRACE_FORMAT_VERSION, Ticket, TicketAction,
    TicketData, TicketHash, TraceEntry, WitnessCommitment,
};
use arena0_sandbox::{LoadedProgram, LocalEvent, Program, WasmtimeEngine};
use arena0_store::{Store, StoreConfig};
use arena0_transport::Transport;
use arena0_transport::local::{LocalNetwork, LocalTransport};
use tempfile::TempDir;
use tokio::sync::mpsc;

use super::ExecutionActor;
use crate::context::ExecContext;

const EXEC_ID: ExecId = ExecId([0x44; 32]);
const NEGOTIATION_ID: NegotiationId = NegotiationId([0x11; 32]);

#[test]
fn terminal_reason_truncation_preserves_utf8_boundaries() {
    let reason = super::truncate_reason("é".repeat(arena0_protocol::MAX_TERMINAL_REASON_BYTES));
    assert!(reason.len() <= arena0_protocol::MAX_TERMINAL_REASON_BYTES);
    assert!(std::str::from_utf8(reason.as_bytes()).is_ok());
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
        let local_keys = Arc::new(NodeKeys::from_secret(SecretKey::from_bytes([1; 32])));
        let remote_keys = Arc::new(NodeKeys::from_secret(SecretKey::from_bytes([2; 32])));
        let local_peer = local_keys.peer_id();
        let remote_peer = remote_keys.peer_id();
        let local_salt = ExecutionSalt::try_from_bytes([9; 32]).expect("non-zero test salt");
        let remote_salt = ExecutionSalt::try_from_bytes([10; 32]).expect("non-zero test salt");
        let writer = remote_is_writer.then(|| {
            Ensemble::from_peers(vec![local_peer, remote_peer])
                .expect("ensemble")
                .participant_of(&remote_peer)
                .expect("remote participant")
                .index() as u8
        });
        let wasm = test_wasm(writer);
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

    fn actor(&self) -> ExecutionActor {
        let (messages, _observations) = mpsc::channel(32);
        self.actor_with_messages(messages)
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
            messages,
            send_streams: HashMap::new(),
            inflight_send: None,
            session_started_emitted: false,
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
        actor
            .apply_input(ExecutionInput::Activate)
            .await
            .expect("activate");
        actor
    }

    async fn commit_session_started(&self, actor: &mut ExecutionActor) {
        let state = actor.load_state().await.expect("load active state");
        let ensemble = actor.ensemble();
        let result = actor
            .context
            .program
            .apply_shared(arena0_sandbox::SharedCall::session_started(
                state.shared_state().clone(),
                ensemble.clone(),
            ))
            .expect("session start");
        let entry = TraceEntry {
            trace_version: TRACE_FORMAT_VERSION,
            step: 0,
            event: PublicEvent::SessionStarted { ensemble },
            effects: Vec::new(),
            pre_state: state.public().state_hash(),
            post_state: StateHash::of(result.shared.as_bytes()),
            fuel_used: result.observations.fuel_used,
            witness: None,
            agreement: AggregateAttestation::empty(),
        };
        let delta = SharedDelta::new(entry, result.shared, None).expect("session delta");
        actor
            .context
            .store
            .apply_input(ExecutionInput::ProposeShared(delta), 10)
            .await
            .expect("session proposal");
        let proposal = self
            .store
            .handle()
            .load_execution(EXEC_ID)
            .await
            .expect("load proposal")
            .expect("proposal")
            .pending_shared()
            .expect("pending session proposal")
            .commitment()
            .clone();
        actor
            .context
            .store
            .apply_input(
                ExecutionInput::StepSignature(arena0_protocol::ParticipantStepSignature::new(
                    self.local_keys.peer_id(),
                    proposal.step,
                    self.local_execution_key().sign(&proposal.signing_bytes()),
                )),
                11,
            )
            .await
            .expect("local session signature");
        actor
            .context
            .store
            .apply_input(
                ExecutionInput::StepSignature(arena0_protocol::ParticipantStepSignature::new(
                    self.remote_keys.peer_id(),
                    proposal.step,
                    self.remote_execution_key().sign(&proposal.signing_bytes()),
                )),
                12,
            )
            .await
            .expect("remote session signature");
        self.clear_outbox(actor).await;
    }

    async fn clear_outbox(&self, actor: &mut ExecutionActor) {
        while let Some(leased) = actor
            .context
            .store
            .lease_next_outbox(100)
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

async fn stage_signature_request(actor: &mut ExecutionActor) -> (arena0_protocol::PendingId, u64) {
    let state = actor.load_state().await.expect("load active state");
    let private_sequence = state.private().next_record();
    let effect = PrivateEffect::Sign {
        scheme: SignScheme::Ed25519,
        data: vec![4, 5, 6],
        pending_label: Some("signature".into()),
        expected_type: Some("bytes".into()),
        continuation_tag: Some(7),
    };
    let pending_id = arena0_protocol::pending_id(EXEC_ID, private_sequence, 0);
    let pending =
        arena0_protocol::PendingRecord::from_effects(pending_id, std::slice::from_ref(&effect))
            .expect("pending signature");
    let delta = PrivateDelta::from_record(
        EXEC_ID,
        PrivateRecord {
            seq: private_sequence,
            after_position: state.public().next_step(),
            event: PrivateEvent::React,
            effects: vec![effect],
            draws: Vec::new(),
            fuel_used: 0,
            pending: Some(pending),
        },
        arena0_protocol::LocalStateBytes::try_new(Vec::new()).expect("local state"),
        PrivateContext::new(1),
        PrivateCause::react(),
    )
    .expect("signature delta");
    actor
        .context
        .store
        .apply_input(ExecutionInput::Private(delta), 2)
        .await
        .expect("durable signature");
    (pending_id, private_sequence)
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

    let error = actor
        .apply_message(
            fixture.remote_keys.peer_id(),
            ExecFrame::Message {
                message_id: MessageId([7; 32]),
                seq: 0,
                prestate: StateHash::of(&[]),
                data: Vec::new(),
                witness: WitnessCommitment([8; 32]),
            },
            None,
        )
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
    // Keep the host-issued lifecycle token while a negotiation owner would
    // be exchanging tickets. A second owner is rejected before the token is
    // handed to the actor, proving there is no negotiation-to-spawn gap.
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

    // The returned handle owns the Host router now. Dropping the caller's
    // Arc must not stop the stream path before the actor has finished its
    // first durable lifecycle boundary.
    drop(host);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match spawned.message_rx.recv().await {
                Some(crate::SessionMessage::SessionStarted { .. }) => break,
                Some(crate::SessionMessage::Failed { reason }) => {
                    panic!("actor failed before session start: {reason}")
                }
                Some(_) => {}
                None => panic!("actor observation channel closed before session start"),
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
    let sequence = state.public().next_step();
    let pre_state = state.public().state_hash();
    let data = Vec::new();
    let witness = WitnessCommitment([13; 32]);
    let frame = ExecFrame::Message {
        message_id: MessageId::derive(
            state.binding().session_id(),
            source,
            sequence,
            pre_state,
            &data,
            witness,
        ),
        seq: sequence,
        prestate: pre_state,
        data,
        witness,
    };
    let send = fixture
        .remote_transport
        .open_exec(
            &fixture.local_keys.peer_id(),
            fixture.activation.session_hash(),
        )
        .await
        .expect("open inbound stream");
    tokio::time::timeout(std::time::Duration::from_secs(5), send.send_exec(&frame))
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
async fn restart_resumes_a_durable_timer_and_accepted_inbox() {
    let fixture = Fixture::new(true).await;
    let mut actor = fixture.prepare_active_actor().await;
    fixture.commit_session_started(&mut actor).await;

    let timer = PrivateDelta::from_record(
        EXEC_ID,
        PrivateRecord {
            seq: 0,
            after_position: 1,
            event: PrivateEvent::React,
            effects: vec![PrivateEffect::SetTimer {
                delay_ms: 0,
                timer: None,
            }],
            draws: Vec::new(),
            fuel_used: 0,
            pending: None,
        },
        arena0_protocol::LocalStateBytes::try_new(Vec::new()).expect("local state"),
        PrivateContext::new(1),
        PrivateCause::react(),
    )
    .expect("timer delta");
    actor
        .context
        .store
        .apply_input(ExecutionInput::Private(timer), 13)
        .await
        .expect("arm timer");
    drop(actor);

    let mut restarted = fixture.actor();
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

    let state = fixture
        .store
        .handle()
        .load_execution(EXEC_ID)
        .await
        .expect("load state")
        .expect("state");
    let source = fixture.remote_keys.peer_id();
    let witness = WitnessCommitment([8; 32]);
    let data = vec![1, 2, 3];
    let message_id = MessageId::derive(
        state.binding().session_id(),
        source,
        state.public().next_step(),
        state.public().state_hash(),
        &data,
        witness,
    );
    let frame = ExecFrame::Message {
        message_id,
        seq: state.public().next_step(),
        prestate: state.public().state_hash(),
        data,
        witness,
    };
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
    let fixture = Fixture::new(false).await;
    let (first_messages, mut first_observations) = mpsc::channel(8);
    let mut actor = fixture
        .prepare_active_actor_with_messages(first_messages)
        .await;
    let state = actor.load_state().await.expect("load active state");
    let effect = PrivateEffect::Callout {
        callout_index: 0,
        context: vec![9, 8, 7],
        pending_label: Some("test".into()),
        expected_type: Some("null".into()),
        continuation_tag: Some(3),
    };
    let effects = vec![effect.clone()];
    let pending_id = arena0_protocol::pending_id(EXEC_ID, state.private().next_record(), 0);
    let pending =
        arena0_protocol::PendingRecord::from_effects(pending_id, &effects).expect("pending record");
    let delta = PrivateDelta::from_record(
        EXEC_ID,
        PrivateRecord {
            seq: state.private().next_record(),
            after_position: state.public().next_step(),
            event: PrivateEvent::React,
            effects,
            draws: Vec::new(),
            fuel_used: 0,
            pending: Some(pending),
        },
        arena0_protocol::LocalStateBytes::try_new(Vec::new()).expect("local state"),
        PrivateContext::new(1),
        PrivateCause::react(),
    )
    .expect("callout delta");
    actor
        .context
        .store
        .apply_input(ExecutionInput::Private(delta), 2)
        .await
        .expect("durable callout");
    actor.drain_outbox().await.expect("deliver initial callout");
    assert!(matches!(
        first_observations.recv().await.expect("initial callout message"),
        crate::SessionMessage::CalloutRequested {
            pending_id: id,
            callout_index: 0,
            context,
            ..
        } if id == pending_id && context == vec![9, 8, 7]
    ));
    drop(actor);

    let (messages, mut observations) = mpsc::channel(8);
    let mut restarted = fixture.actor_with_messages(messages);
    restarted.recover().await.expect("recover callout outbox");
    let message = observations.recv().await.expect("callout message");
    assert!(matches!(
        message,
        crate::SessionMessage::CalloutRequested {
            pending_id: id,
            callout_index: 0,
            context,
            ..
        } if id == pending_id && context == vec![9, 8, 7]
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
async fn crash_before_signature_application_is_recovered_inside_actor() {
    let fixture = Fixture::new(false).await;
    let mut actor = fixture.prepare_active_actor().await;
    actor
        .run_local(
            LocalEvent::React,
            PrivateEvent::React,
            PrivateCause::react(),
        )
        .await
        .expect("record reaction before signature");
    fixture.commit_session_started(&mut actor).await;
    let (pending_id, private_sequence) = stage_signature_request(&mut actor).await;
    let leased = actor
        .context
        .store
        .lease_next_outbox(super::now_ms())
        .await
        .expect("lease signature request")
        .expect("signature request outbox");
    assert_eq!(leased.item.effect.payload_len(), 3);
    assert!(matches!(
        &leased.item.effect,
        DurableEffect::RequestSignature { pending, .. } if pending.id == pending_id
    ));
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
    assert_eq!(state.private().next_record(), private_sequence + 2);
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
async fn crash_after_signature_application_is_idempotently_acknowledged() {
    let fixture = Fixture::new(false).await;
    let mut actor = fixture.prepare_active_actor().await;
    actor
        .run_local(
            LocalEvent::React,
            PrivateEvent::React,
            PrivateCause::react(),
        )
        .await
        .expect("record reaction before signature");
    fixture.commit_session_started(&mut actor).await;
    let (pending_id, private_sequence) = stage_signature_request(&mut actor).await;
    let leased = actor
        .context
        .store
        .lease_next_outbox(super::now_ms())
        .await
        .expect("lease signature request")
        .expect("signature request outbox");
    assert!(matches!(
        &leased.item.effect,
        DurableEffect::RequestSignature { pending, .. } if pending.id == pending_id
    ));
    actor
        .deliver_effect_for_test(&leased.item.effect)
        .await
        .expect("apply signature before crash");
    let applied = actor.load_state().await.expect("load applied state");
    assert_eq!(applied.private().next_record(), private_sequence + 2);
    assert!(applied.status().pending().is_none());
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
    assert_eq!(
        state.private().next_record(),
        private_sequence + 2,
        "replayed delivery must not run the continuation twice"
    );
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
async fn inbound_transport_ack_follows_durable_acceptance() {
    let fixture = Fixture::new(true).await;
    let (messages, _observations) = mpsc::channel(32);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;
    let state = actor.load_state().await.expect("load active state");
    let source = fixture.remote_keys.peer_id();
    let sequence = state.public().next_step() + 1;
    let pre_state = state.public().state_hash();
    let data = vec![4, 5, 6];
    let witness = WitnessCommitment([12; 32]);
    let frame = ExecFrame::Message {
        message_id: MessageId::derive(
            state.binding().session_id(),
            source,
            sequence,
            pre_state,
            &data,
            witness,
        ),
        seq: sequence,
        prestate: pre_state,
        data,
        witness,
    };
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
    let mut actor = actor;
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
    assert!(matches!(
        pending[0].frame(),
        ExecFrame::Message { seq, .. } if *seq == sequence
    ));

    resolver
        .await
        .expect("resolver task")
        .expect("resolve future delivery");
}

#[tokio::test]
async fn trace_observation_waits_for_the_certified_step() {
    let fixture = Fixture::new(true).await;
    let (messages, mut observations) = mpsc::channel(8);
    let mut actor = fixture.prepare_active_actor_with_messages(messages).await;
    fixture.commit_session_started(&mut actor).await;

    let state = actor.load_state().await.expect("load session state");
    let source = fixture.remote_keys.peer_id();
    let sequence = state.public().next_step();
    let pre_state = state.public().state_hash();
    let data = vec![1, 2, 3];
    let witness = WitnessCommitment([14; 32]);
    assert!(
        actor
            .apply_message(
                source,
                ExecFrame::Message {
                    message_id: MessageId::derive(
                        state.binding().session_id(),
                        source,
                        sequence,
                        pre_state,
                        &data,
                        witness,
                    ),
                    seq: sequence,
                    prestate: pre_state,
                    data,
                    witness,
                },
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
    let remote = fixture.remote_keys.peer_id();
    actor
        .context
        .store
        .accept_inbound(
            remote,
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
    assert_eq!(state.public().next_step(), sequence + 1);
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
        StateHash::of(&[]),
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

fn test_wasm(writer: Option<u8>) -> Vec<u8> {
    let definition = ProgramDefinition {
        metadata: ProgramMetadata {
            name: "actor-test".into(),
            version: "1".into(),
            description: "actor test guest".into(),
            author: None,
            capabilities: Vec::new(),
            display_name: "Actor test".into(),
            participants: arena0_program::ParticipantCount::Exact { count: 2 },
        },
        schema: ProgramSchema {
            state: StateSchema {
                schema: unit_schema(),
                max_bytes: 64,
            },
            callouts: Vec::new(),
            messages: Vec::new(),
            params: unit_schema(),
            queries: Vec::new(),
            outcome: unit_schema(),
        },
    };
    let metadata = definition.encode().expect("metadata");
    let accepted = [0, 0, 0, 0, 0];
    let initialized = [0, 0, 0, 0, 0, 0, 0, 0];
    let writer = writer.map_or_else(|| vec![0], |index| vec![1, index]);
    let outcome = [0, 0, 0, 0, 4, 0, 0, 0, b'n', b'u', b'l', b'l'];
    let view = [4, 0, 0, 0, b'n', b'u', b'l', b'l'];
    let query = [0, 0, 0, 0, 4, 0, 0, 0, b'n', b'u', b'l', b'l'];
    let wat = format!(
        r#"
        (module
          (memory (export "memory") 2)
          (global (export "arena0_abi_version") i32 (i32.const 20))
          (data (i32.const 2048) "{initialized}")
          (data (i32.const 2080) "{accepted}")
          (data (i32.const 2112) "{writer}")
          (data (i32.const 2144) "{outcome}")
          (data (i32.const 2176) "{view}")
          (data (i32.const 2208) "{query}")
          (func $pack (param $ptr i32) (param $len i32) (result i64)
            local.get $ptr
            i64.extend_i32_s
            i64.const 32
            i64.shl
            local.get $len
            i64.extend_i32_s
            i64.const 4294967295
            i64.and
            i64.or)
          (func (export "arena0_alloc") (param i32) (result i32) i32.const 4096)
          (func (export "arena0_dealloc") (param i32 i32))
          (func (export "arena0_initialize") (param i32 i32) (result i64)
            i32.const 2048 i32.const {initialized_len} call $pack)
          (func (export "arena0_shared") (param i32 i32) (result i64)
            i32.const 2080 i32.const {accepted_len} call $pack)
          (func (export "arena0_local") (param i32 i32) (result i64)
            i32.const 2080 i32.const {accepted_len} call $pack)
          (func (export "arena0_writer") (param i32 i32) (result i64)
            i32.const 2112 i32.const {writer_len} call $pack)
          (func (export "arena0_outcome") (param i32 i32) (result i64)
            i32.const 2144 i32.const {outcome_len} call $pack)
          (func (export "arena0_view") (param i32 i32) (result i64)
            i32.const 2176 i32.const {view_len} call $pack)
          (func (export "arena0_query") (param i32 i32) (result i64)
            i32.const 2208 i32.const {query_len} call $pack)
          (func (export "arena0_metadata") (result i64)
            i32.const 0 i32.const 0 call $pack))
        "#,
        initialized = wat_data(&initialized),
        accepted = wat_data(&accepted),
        writer = wat_data(&writer),
        outcome = wat_data(&outcome),
        view = wat_data(&view),
        query = wat_data(&query),
        initialized_len = initialized.len(),
        accepted_len = accepted.len(),
        writer_len = writer.len(),
        outcome_len = outcome.len(),
        view_len = view.len(),
        query_len = query.len(),
    );
    let binary = wat::parse_str(wat).expect("wat");
    append_metadata(&binary, &metadata)
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

fn append_metadata(binary: &[u8], metadata: &[u8]) -> Vec<u8> {
    let mut output = binary.to_vec();
    let mut payload = Vec::new();
    push_leb128(&mut payload, b"arena0.metadata".len() as u64);
    payload.extend_from_slice(b"arena0.metadata");
    payload.extend_from_slice(metadata);
    output.push(0);
    push_leb128(&mut output, payload.len() as u64);
    output.extend_from_slice(&payload);
    output
}

fn push_leb128(output: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            break;
        }
    }
}
