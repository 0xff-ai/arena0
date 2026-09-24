use super::*;
use arena0_crypto::bls::BlsSecretKey;
use arena0_crypto::{BlsSignature, NodeKeys, SecretKey, key_binding_message};
use arena0_program::{
    JsonBytes, LocalStateBytes, MAX_LOCAL_STATE_BYTES, MAX_SHARED_STATE_BYTES, SharedStateBytes,
};
use arena0_protocol::{
    AbortKind, AbortOccurrence, ActivationData, Effect, Ensemble, Event, ExecFrame, ExecutionState,
    ExecutionVersion, MessageId, NegotiationId, NegotiationTarget, Offer, OfferData,
    ParticipantStepSignature, PreparedActivation, ReceiptArtifact, ReceiptTermination,
    SessionHeader, StateHash, StepCursor, TerminalOutcome, Ticket, TicketAction, TicketData,
    TimerPayload,
};
use std::path::Path;

pub(crate) struct ActivationFixture {
    pub(crate) activation: Activation,
    prepared: PreparedActivation,
    program: Vec<u8>,
    pub(crate) producer: PeerId,
}

async fn create_execution(path: &Path, fixture: &ActivationFixture, execution_id: ExecId) -> Store {
    let store = Store::open(StoreConfig::new(path, fixture.producer)).expect("open");
    let program_hash = ProgramHash::of(&fixture.program);
    store
        .handle()
        .register_program(fixture.program.clone(), 1)
        .await
        .expect("program");
    let peers = fixture
        .prepared
        .tickets()
        .iter()
        .map(|ticket| ticket.data.signer)
        .collect();
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    writer
        .create_execution_request(
            program_hash,
            Some(JsonBytes::try_new(br#"{}"#.to_vec()).expect("params")),
            ExecutionAdmission::explicit(NegotiationId([0x11; 32]), peers).expect("admission"),
            2,
        )
        .await
        .expect("request");
    writer
        .prepare_activation(fixture.prepared.clone(), 3)
        .await
        .expect("prepare");
    writer
        .commit_activation(fixture.activation.clone(), 4)
        .await
        .expect("commit");
    let state = ExecutionState::new(
        execution_id,
        fixture.activation.clone(),
        fixture.producer,
        SharedStateBytes::try_new(vec![0]).expect("shared"),
        LocalStateBytes::try_new(Vec::new()).expect("local"),
    )
    .expect("state");
    writer
        .create_execution(
            state.binding().activation().clone(),
            state.producer(),
            state.shared_state().clone(),
            state.local_state().clone(),
            5,
        )
        .await
        .expect("execution");
    activate_record(&mut writer, ExecutionVersion::ZERO, 6)
        .await
        .expect("activate");
    drop(writer);
    store
}

fn other_peer(fixture: &ActivationFixture) -> PeerId {
    fixture
        .prepared
        .tickets()
        .iter()
        .map(|ticket| ticket.data.signer)
        .find(|peer| *peer != fixture.producer)
        .expect("second participant")
}

fn session_started_event(fixture: &ActivationFixture) -> Event<Vec<u8>> {
    let ensemble = Ensemble::from_peers(
        fixture
            .prepared
            .tickets()
            .iter()
            .map(|ticket| ticket.data.signer)
            .collect(),
    )
    .expect("ensemble");
    Event::SessionStarted { ensemble }
}

async fn sign_step(
    store: &Store,
    fixture: &ActivationFixture,
    execution_id: ExecId,
    writer: &mut ExecutionStore,
    first_now_ms: u64,
    second_now_ms: u64,
) -> ExecutionState {
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load pending proposal")
        .expect("pending proposal state");
    let commitment = state
        .pending_shared()
        .expect("pending shared proposal")
        .commitment()
        .clone();
    let producer_bls = BlsSecretKey::from_seed(&[11; 32]).expect("producer bls");
    let other_bls = BlsSecretKey::from_seed(&[12; 32]).expect("other bls");
    signature_record(
        writer,
        state.version(),
        ParticipantStepSignature::new(
            fixture.producer,
            commitment.step,
            producer_bls.sign(&commitment.signing_bytes()),
        ),
        first_now_ms,
    )
    .await
    .expect("producer step signature");
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load signed proposal")
        .expect("signed proposal state");
    let remote = other_peer(fixture);
    let remote_signature = other_bls.sign(&commitment.signing_bytes());
    signature_record(
        writer,
        state.version(),
        ParticipantStepSignature::new(remote, commitment.step, remote_signature),
        second_now_ms,
    )
    .await
    .expect("peer step signature");
    store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load committed step")
        .expect("committed step state")
}

fn terminal_dispatch(
    fixture: &ActivationFixture,
) -> (
    Event<Vec<u8>>,
    SharedStateBytes,
    LocalStateBytes,
    Vec<Effect>,
    Option<TerminalOutcome>,
) {
    let outcome = vec![9, 8, 7];
    let next_shared = SharedStateBytes::try_new(vec![1]).expect("next shared");
    (
        session_started_event(fixture),
        next_shared,
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        vec![Effect::SessionEnd {
            outcome: outcome.clone(),
        }],
        Some(TerminalOutcome::new(outcome, br#"null"#.to_vec()).expect("outcome")),
    )
}

async fn certify_terminal(store: &Store, fixture: &ActivationFixture, execution_id: ExecId) {
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load active")
        .expect("active state");
    let (event, shared, local, effects, terminal_outcome) = terminal_dispatch(fixture);
    dispatch_record(
        &mut writer,
        state.version(),
        event,
        shared,
        local,
        effects,
        terminal_outcome,
        None,
        None,
        None,
        7,
    )
    .await
    .expect("proposal");
    sign_step(store, fixture, execution_id, &mut writer, 8, 9).await;
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(state.status(), ExecutionStatus::Certified { .. }));
}

#[tokio::test]
async fn recovery_resumes_ending_and_skips_ended_with_unconfirmed_peers() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ending.sqlite");
    let execution_id = ExecId([0xe8; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    certify_terminal(&store, &fixture, execution_id).await;
    let mut writer = store.handle().claim_execution(execution_id).unwrap();
    publish_current(&store, &mut writer, execution_id, 20)
        .await
        .unwrap();
    let state = writer.load_execution().await.unwrap().unwrap();
    assert!(matches!(
        state.end_phase(),
        arena0_protocol::EndPhase::Ending { .. }
    ));
    assert_eq!(
        store
            .handle()
            .list_recovery_candidates(RecoveryCursor::start(), 8)
            .await
            .unwrap()
            .candidates()
            .len(),
        1
    );
    let mut ended = state.clone();
    ended.expire_end().unwrap();
    writer
        .persist(TransitionRecord {
            expected: state.version(),
            next: ended.clone(),
            change: Change::End,
            now_ms: 21,
        })
        .await
        .unwrap();
    drop(writer);
    store.shutdown().await.unwrap();
    let reopened = Store::open(StoreConfig::new(&path, fixture.producer)).unwrap();
    assert!(
        reopened
            .handle()
            .list_recovery_candidates(RecoveryCursor::start(), 8)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        reopened
            .handle()
            .execution_end(fixture.activation.session_hash())
            .await
            .unwrap(),
        Some((execution_id, ended.end_phase().clone()))
    );
    assert!(
        reopened
            .handle()
            .end_wake_candidate(execution_id)
            .await
            .unwrap()
            .is_some()
    );
    reopened.shutdown().await.unwrap();
    let database = Connection::open(&path).unwrap();
    database
        .execute("UPDATE executions SET end_phase = 1", [])
        .unwrap();
    drop(database);
    assert!(
        matches!(
            Store::open(StoreConfig::new(&path, fixture.producer)),
            Err(StoreError::Corruption(_))
        ),
        "routing projections must agree with the authoritative end phase"
    );
}

async fn load_published_receipt(store: &Store, fixture: &ActivationFixture) -> ReceiptArtifact {
    store
        .handle()
        .load_receipt(fixture.activation.session_hash())
        .await
        .expect("load receipt")
        .expect("receipt")
        .receipt
}

async fn publish_current(
    store: &Store,
    writer: &mut ExecutionStore,
    execution_id: ExecId,
    now_ms: u64,
) -> Result<ExecutionState, StoreError> {
    let state = store
        .handle()
        .load_execution(execution_id)
        .await?
        .ok_or(StoreError::ExecutionNotFound(execution_id))?;
    publication_record(writer, state.version(), now_ms).await
}

fn receipt_with_different_content(receipt: &ReceiptArtifact) -> ReceiptArtifact {
    let changed_outcome = vec![9, 8, 8];
    let mut trace = receipt.body().trace().to_vec();
    trace[0].terminal = Some(Effect::SessionEnd {
        outcome: changed_outcome.clone(),
    });
    let commitment = arena0_protocol::StepCommitment::for_entry(
        receipt.body().header().session_hash(),
        &trace[0],
        arena0_protocol::trace::CHAIN_START,
    );
    let producer_bls = BlsSecretKey::from_seed(&[11; 32]).expect("producer bls");
    let other_bls = BlsSecretKey::from_seed(&[12; 32]).expect("other bls");
    trace[0].agreement = arena0_protocol::AggregateAttestation::from_signatures(
        arena0_protocol::SignerSet::full(2).expect("signer set"),
        &[
            producer_bls.sign(&commitment.signing_bytes()),
            other_bls.sign(&commitment.signing_bytes()),
        ],
    )
    .expect("agreement");
    let header = SessionHeader::new(
        receipt.body().header().activation.clone(),
        ReceiptTermination::Completed,
    );
    let body = arena0_protocol::ReceiptBody::new(
        header,
        changed_outcome,
        receipt.body().params().to_vec(),
        trace,
    )
    .expect("receipt body");

    ReceiptArtifact::new(body).expect("different receipt")
}

pub(crate) fn activation_fixture() -> ActivationFixture {
    activation_fixture_with_initial_state(
        SharedStateBytes::try_new(vec![0]).expect("initial shared state"),
    )
}

fn activation_fixture_with_initial_state(initial_state: SharedStateBytes) -> ActivationFixture {
    let producer_keys = NodeKeys::from_secret(SecretKey::from_bytes([1; 32]));
    let other_keys = NodeKeys::from_secret(SecretKey::from_bytes([2; 32]));
    let creator_bls = BlsSecretKey::from_seed(&[11; 32]).expect("bls");
    let other_bls = BlsSecretKey::from_seed(&[12; 32]).expect("bls");
    let producer = PeerId::from_ed25519(&producer_keys.ed25519_public_key());
    let program = vec![0, 1, 2, 3];
    let offer_data = OfferData::new(
        NegotiationId([0x11; 32]),
        0,
        producer,
        ProgramHash::of(&program),
        arena0_program::ExecutionProfile::current().hash(),
        JsonBytes::try_new(br#"{}"#.to_vec()).expect("json"),
        2,
        StateHash::of_shared(&initial_state),
        1_000_000,
    )
    .expect("offer");
    let offer_hash = arena0_protocol::OfferHash::of(&offer_data);
    let make_ticket = |keys: &NodeKeys, bls: &BlsSecretKey| {
        let peer = PeerId::from_ed25519(&keys.ed25519_public_key());
        let public = bls.public_key();
        let binding = bls.sign_binding(&key_binding_message(&offer_hash.0, &peer.0, &public));
        let data = TicketData::new(
            NegotiationId([0x11; 32]),
            0,
            peer,
            0,
            TicketAction::Active {
                execution_bls: public,
                key_binding: binding,
                issued_at_unix_ms: 1,
                valid_for_ms: 60_000,
            },
        )
        .expect("ticket");
        Ticket {
            signature: keys.sign(&data.signing_bytes()),
            data,
        }
    };
    let tickets = vec![
        make_ticket(&producer_keys, &creator_bls),
        make_ticket(&other_keys, &other_bls),
    ];
    let hashes = tickets
        .iter()
        .map(|ticket| arena0_protocol::TicketHash::of(&ticket.data))
        .collect::<Vec<_>>();
    let data = ActivationData::new(offer_hash, hashes.clone()).expect("activation data");
    let aggregate = BlsSignature::aggregate(&[
        creator_bls.sign(&data.signing_bytes()),
        other_bls.sign(&data.signing_bytes()),
    ])
    .expect("aggregate");
    let offer = Offer::new(offer_data, hashes).expect("offer");
    let prepared = PreparedActivation::new(offer, tickets).expect("prepared");
    let activation = Activation::new(prepared.clone(), aggregate).expect("activation");
    ActivationFixture {
        activation,
        prepared,
        program,
        producer,
    }
}

fn host(byte: u8) -> PeerId {
    PeerId([byte; 32])
}

fn explicit_admission(local: PeerId, negotiation_id: NegotiationId) -> ExecutionAdmission {
    ExecutionAdmission::explicit(
        negotiation_id,
        vec![local, PeerId([local.0[0].wrapping_add(1); 32])],
    )
    .expect("explicit admission")
}

#[tokio::test]
async fn shutdown_closes_handles_and_releases_process_lock() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let config = StoreConfig::new(&path, host(1));
    let store = Store::open(config.clone()).expect("open");
    assert!(matches!(
        Store::open(config.clone()),
        Err(StoreError::AlreadyOwned { .. })
    ));
    let handle = store.handle().clone();
    let writer = handle.claim_execution(ExecId([1; 32])).expect("claim");
    store.shutdown().await.expect("shutdown");
    assert!(matches!(
        handle.load_user_agent().await,
        Err(StoreError::Closed)
    ));
    assert!(matches!(
        writer.load_execution().await,
        Err(StoreError::Closed)
    ));
    let reopened = Store::open(config).expect("reopen");
    reopened.shutdown().await.expect("shutdown reopened");
}

#[tokio::test]
async fn failed_rollback_closes_store_calls() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let store = Store::open(StoreConfig::new(&path, host(1))).expect("open");
    let handle = store.handle().clone();
    let (program, _) = handle.register_program(vec![1], 1).await.expect("program");
    let mut writer = handle.claim_execution(ExecId([1; 32])).expect("claim");
    let connection = Connection::open(&path).expect("fault injection connection");
    connection
        .execute_batch(
            "CREATE TRIGGER abort_request BEFORE INSERT ON exec_requests
             BEGIN SELECT RAISE(ROLLBACK, 'injected rollback'); END;",
        )
        .expect("rollback trigger");

    let result = writer
        .create_execution_request(
            program,
            Some(JsonBytes::try_new(b"{}".to_vec()).expect("params")),
            explicit_admission(host(1), NegotiationId([1; 32])),
            2,
        )
        .await;
    assert!(
        matches!(result, Err(StoreError::Corruption(reason)) if reason.contains("rollback failed"))
    );
    assert!(matches!(
        handle.load_user_agent().await,
        Err(StoreError::Closed)
    ));
    assert!(matches!(
        writer.load_execution().await,
        Err(StoreError::Closed)
    ));
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM exec_requests", [], |row| row.get(0))
        .expect("request count");
    assert_eq!(count, 0);
    drop(connection);
    store.shutdown().await.expect("shutdown poisoned store");
    let _reservation = Store::reserve(&path).expect("shutdown releases poisoned store lock");
}

#[tokio::test]
async fn reservation_holds_lock_before_open_and_releases_failed_open() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let reservation = Store::reserve(&path).expect("reserve");
    assert!(!path.exists(), "reservation must not open the database");
    assert!(matches!(
        Store::reserve(&path),
        Err(StoreError::AlreadyOwned { .. })
    ));

    let mismatch = reservation.open(StoreConfig::new(
        directory.path().join("different.sqlite"),
        host(1),
    ));
    assert!(matches!(
        mismatch,
        Err(StoreError::InvalidConfiguration(message))
            if message.contains("reservation path")
    ));
    let store = Store::reserve(&path)
        .expect("path mismatch must release reservation")
        .open(StoreConfig::new(&path, host(1)))
        .expect("open after mismatch");
    store.shutdown().await.expect("shutdown after mismatch");

    let reservation = Store::reserve(&path).expect("reserve after open failure");
    assert!(matches!(
        reservation.open(StoreConfig::new(&path, host(2))),
        Err(StoreError::IdentityMismatch { .. })
    ));
    let store = Store::reserve(&path)
        .expect("failed open must release reservation")
        .open(StoreConfig::new(&path, host(1)))
        .expect("open after failed open");
    store.shutdown().await.expect("shutdown after failed open");
}

#[tokio::test]
async fn user_agent_is_optional_durable_and_validated() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let config = StoreConfig::new(&path, host(1));
    let store = Store::open(config.clone()).expect("open");

    assert_eq!(store.handle().load_user_agent().await.expect("load"), None);
    for value in [
        String::new(),
        "   \t".to_owned(),
        "agent\nname".to_owned(),
        "x".repeat(MAX_USER_AGENT_BYTES + 1),
        "é".repeat(128 + 1),
    ] {
        assert!(matches!(
            store.handle().set_user_agent(value).await,
            Err(StoreError::InvalidConfiguration(_))
        ));
    }

    let value = "arena0-test/1.0".to_owned();
    store
        .handle()
        .set_user_agent(value.clone())
        .await
        .expect("set");
    assert_eq!(
        store.handle().load_user_agent().await.expect("load"),
        Some(value.clone())
    );
    store.shutdown().await.expect("shutdown");

    let reopened = Store::open(config).expect("reopen");
    assert_eq!(
        reopened
            .handle()
            .load_user_agent()
            .await
            .expect("load after reopen"),
        Some(value)
    );
    reopened.shutdown().await.expect("shutdown reopened");
}

#[tokio::test]
async fn corrupt_user_agent_metadata_is_rejected() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let store = Store::open(StoreConfig::new(&path, host(1))).expect("open");
    store
        .handle()
        .set_user_agent("valid-agent".to_owned())
        .await
        .expect("set");
    store.shutdown().await.expect("shutdown");

    let connection = Connection::open(&path).expect("inspect");
    connection
        .execute(
            "UPDATE meta SET value = ?1 WHERE key = 'user_agent'",
            params![vec![0xff_u8]],
        )
        .expect("corrupt metadata");
    drop(connection);

    assert!(matches!(
        Store::open(StoreConfig::new(&path, host(1))),
        Err(StoreError::Corruption(message)) if message.contains("user agent")
    ));
}

#[tokio::test]
async fn execution_claim_is_exclusive_across_handles_and_released_on_drop() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store = Store::open(StoreConfig::new(
        directory.path().join("store.sqlite"),
        host(2),
    ))
    .expect("open");
    let first_handle = store.handle().clone();
    let second_handle = store.handle().clone();
    let execution_id = ExecId([0x55; 32]);
    let first = first_handle
        .claim_execution(execution_id)
        .expect("first claim");
    assert!(matches!(
        second_handle.claim_execution(execution_id),
        Err(StoreError::ExecutionAlreadyClaimed(id)) if id == execution_id
    ));
    drop(first);
    let reclaimed = second_handle
        .claim_execution(execution_id)
        .expect("claim after release");
    assert_eq!(reclaimed.execution_id(), execution_id);
    drop(reclaimed);
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn request_is_idempotent_and_salt_is_durable() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let store = Store::open(StoreConfig::new(&path, host(3))).expect("open");
    let wasm = vec![0, 1, 2, 3];
    let hash = ProgramHash::of(&wasm);
    store
        .handle()
        .register_program(wasm.clone(), 1)
        .await
        .expect("program");
    let id = ExecId([7; 32]);
    let negotiation_id = NegotiationId([8; 32]);
    let admission = explicit_admission(host(3), negotiation_id);
    let params = JsonBytes::try_new(br#"{"x":1}"#.to_vec()).expect("json");
    let mut writer = store
        .handle()
        .claim_execution(id)
        .expect("execution writer");
    assert_eq!(
        writer
            .create_execution_request(hash, Some(params.clone()), admission.clone(), 2)
            .await
            .expect("request"),
        ExecutionRequestOutcome::Created
    );
    assert_eq!(
        writer
            .create_execution_request(hash, Some(params), admission, 3)
            .await
            .expect("retry"),
        ExecutionRequestOutcome::AlreadyExists
    );
    let first = writer.load_or_create_execution_salt(4).await.expect("salt");
    let second = writer
        .load_or_create_execution_salt(5)
        .await
        .expect("salt retry");
    assert_eq!(first, second);
    drop(writer);
    store.shutdown().await.expect("shutdown");
    let reopened = Store::open(StoreConfig::new(&path, host(3))).expect("reopen");
    let mut writer = reopened
        .handle()
        .claim_execution(id)
        .expect("execution writer after reopen");
    assert_eq!(
        writer
            .load_or_create_execution_salt(6)
            .await
            .expect("salt reopen"),
        first
    );
    drop(writer);
    reopened.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn persisted_zero_execution_salt_is_store_corruption() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let host_id = host(3);
    let store = Store::open(StoreConfig::new(&path, host_id)).expect("open");
    let wasm = vec![0, 1, 2, 3];
    let program_hash = ProgramHash::of(&wasm);
    store
        .handle()
        .register_program(wasm, 1)
        .await
        .expect("program");
    let execution_id = ExecId([0x73; 32]);
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    writer
        .create_execution_request(
            program_hash,
            Some(JsonBytes::try_new(br#"{}"#.to_vec()).expect("params")),
            ExecutionAdmission::explicit(NegotiationId([0x74; 32]), vec![host_id, host(4)])
                .expect("admission"),
            2,
        )
        .await
        .expect("request");
    writer.load_or_create_execution_salt(3).await.expect("salt");
    drop(writer);
    store.shutdown().await.expect("shutdown");

    let connection = Connection::open(&path).expect("inspect");
    let encoded = envelope(EnvelopeKind::ExecutionSalt, &[0; 32]).expect("salt envelope");
    connection
        .execute(
            "UPDATE execution_salts SET salt = ?1 WHERE execution_id = ?2",
            params![encoded, execution_id.0.to_vec()],
        )
        .expect("tamper salt");
    drop(connection);

    match Store::open(StoreConfig::new(&path, host_id)) {
        Err(StoreError::Corruption(message)) => {
            assert!(
                message.contains("execution salt validation failed"),
                "{message}"
            );
            assert!(message.contains("must not be zero"), "{message}");
        }
        Err(error) => panic!("expected corruption, got {error:?}"),
        Ok(store) => {
            store.shutdown().await.expect("shutdown");
            panic!("zero execution salt was accepted");
        }
    }
}

#[tokio::test]
async fn explicit_admission_requires_params() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let store = Store::open(StoreConfig::new(&path, host(3))).expect("open");
    let wasm = vec![0, 1, 2, 3];
    let hash = ProgramHash::of(&wasm);
    store
        .handle()
        .register_program(wasm, 1)
        .await
        .expect("program");
    let execution_id = ExecId([0x71; 32]);
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    let result = writer
        .create_execution_request(
            hash,
            None,
            explicit_admission(host(3), NegotiationId([0x72; 32])),
            2,
        )
        .await;
    assert!(matches!(result, Err(StoreError::InvalidAdmission(_))));
    assert!(
        writer
            .load_execution_request()
            .await
            .expect("load")
            .is_none()
    );
    drop(writer);
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn create_admission_rejects_invalid_participant_counts() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let host_id = host(3);
    let store = Store::open(StoreConfig::new(&path, host_id)).expect("open");
    let wasm = vec![0, 1, 2, 3];
    let hash = ProgramHash::of(&wasm);
    store
        .handle()
        .register_program(wasm, 1)
        .await
        .expect("program");

    for (index, participant_count) in [0_u16, 1, u16::MAX].into_iter().enumerate() {
        let byte = 0x7a_u8 + u8::try_from(index).expect("test index");
        let execution_id = ExecId([byte; 32]);
        let mut writer = store
            .handle()
            .claim_execution(execution_id)
            .expect("execution writer");
        let result = writer
            .create_execution_request(
                hash,
                Some(JsonBytes::try_new(br#"{}"#.to_vec()).expect("params")),
                ExecutionAdmission::Create {
                    negotiation_id: NegotiationId([byte; 32]),
                    participant_count,
                },
                2,
            )
            .await;
        assert!(matches!(result, Err(StoreError::InvalidAdmission(_))));
        assert!(
            writer
                .load_execution_request()
                .await
                .expect("load")
                .is_none()
        );
        drop(writer);
        assert!(
            store
                .handle()
                .load_execution_request(execution_id)
                .await
                .expect("load after rollback")
                .is_none()
        );
    }

    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn join_preferred_params_must_match_creator_activation() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let store = Store::open(StoreConfig::new(&path, other_peer(&fixture))).expect("open");
    let hash = ProgramHash::of(&fixture.program);
    store
        .handle()
        .register_program(fixture.program.clone(), 1)
        .await
        .expect("program");
    let execution_id = ExecId([0x74; 32]);
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    writer
        .create_execution_request(
            hash,
            Some(JsonBytes::try_new(br#"{"preferred":true}"#.to_vec()).expect("params")),
            ExecutionAdmission::join(fixture.producer, NegotiationId([0x11; 32])),
            2,
        )
        .await
        .expect("request");
    assert!(matches!(
        writer.prepare_activation(fixture.prepared, 3).await,
        Err(StoreError::Corruption(_))
    ));
    drop(writer);
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn open_join_target_binding_is_compare_and_set_and_durable() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let joiner = other_peer(&fixture);
    let store = Store::open(StoreConfig::new(&path, joiner)).expect("open");
    let hash = ProgramHash::of(&fixture.program);
    store
        .handle()
        .register_program(fixture.program.clone(), 1)
        .await
        .expect("program");

    let target = NegotiationTarget::new(fixture.producer, NegotiationId([0x76; 32]));
    let other_target = NegotiationTarget::new(fixture.producer, NegotiationId([0x77; 32]));
    let execution_id = ExecId([0x78; 32]);
    let failed_execution_id = ExecId([0x79; 32]);
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    writer
        .create_execution_request(hash, None, ExecutionAdmission::join_open(), 2)
        .await
        .expect("request");
    let request = writer
        .load_execution_request()
        .await
        .expect("load open request")
        .expect("request");
    assert_eq!(request.negotiation_id(), None);
    assert_eq!(request.admission().target(), None);
    assert_eq!(
        writer.bind_join_target(target).await.expect("bind target"),
        AdmissionBindingOutcome::Bound
    );
    assert_eq!(
        writer.bind_join_target(target).await.expect("retry target"),
        AdmissionBindingOutcome::AlreadyBound
    );
    assert_eq!(
        writer
            .bind_join_target(other_target)
            .await
            .expect("conflicting target result"),
        AdmissionBindingOutcome::Conflict
    );
    let request = writer
        .load_execution_request()
        .await
        .expect("load bound request")
        .expect("request");
    assert_eq!(request.negotiation_id(), Some(target.negotiation_id));
    assert_eq!(request.admission().target(), Some(target));
    drop(writer);

    let mut failed_writer = store
        .handle()
        .claim_execution(failed_execution_id)
        .expect("failed execution writer");
    failed_writer
        .create_execution_request(hash, None, ExecutionAdmission::join_open(), 3)
        .await
        .expect("failed request");
    assert_eq!(
        failed_writer
            .record_execution_request_failure("negotiation unavailable")
            .await
            .expect("record failure"),
        ExecutionRequestFailureOutcome::Recorded
    );
    assert!(matches!(
        failed_writer.bind_join_target(target).await,
        Err(StoreError::ExecutionLifecycleStarted(id)) if id == failed_execution_id
    ));
    drop(failed_writer);
    store.shutdown().await.expect("shutdown");

    let reopened = Store::open(StoreConfig::new(&path, joiner)).expect("reopen");
    let request = reopened
        .handle()
        .load_execution_request(execution_id)
        .await
        .expect("load durable request")
        .expect("request");
    assert_eq!(request.admission().target(), Some(target));
    assert_eq!(request.negotiation_id(), Some(target.negotiation_id));
    let failed_request = reopened
        .handle()
        .load_execution_request(failed_execution_id)
        .await
        .expect("load failed request")
        .expect("failed request");
    assert_eq!(failed_request.failure(), Some("negotiation unavailable"));
    assert_eq!(failed_request.admission().target(), None);
    reopened.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn join_without_params_survives_reopen_recovery() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let joiner = other_peer(&fixture);
    let store = Store::open(StoreConfig::new(&path, joiner)).expect("open");
    let hash = ProgramHash::of(&fixture.program);
    store
        .handle()
        .register_program(fixture.program.clone(), 1)
        .await
        .expect("program");
    let execution_id = ExecId([0x75; 32]);
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    writer
        .create_execution_request(
            hash,
            None,
            ExecutionAdmission::join(fixture.producer, NegotiationId([0x11; 32])),
            2,
        )
        .await
        .expect("request");
    writer
        .prepare_activation(fixture.prepared.clone(), 3)
        .await
        .expect("prepare");
    writer
        .commit_activation(fixture.activation, 4)
        .await
        .expect("commit");
    drop(writer);
    store.shutdown().await.expect("shutdown");

    let reopened = Store::open(StoreConfig::new(&path, joiner)).expect("reopen");
    let request = reopened
        .handle()
        .load_execution_request(execution_id)
        .await
        .expect("load request")
        .expect("request");
    assert!(request.params().is_none());
    assert!(matches!(
        reopened
            .handle()
            .load_activation(execution_id)
            .await
            .expect("load activation")
            .expect("activation")
            .status(),
        ActivationRecordStatus::Committed
    ));
    reopened.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn registry_remove_retains_content_and_reactivate_is_exact() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store = Store::open(StoreConfig::new(
        directory.path().join("store.sqlite"),
        host(4),
    ))
    .expect("open");
    let wasm = vec![9, 8, 7];
    let hash = ProgramHash::of(&wasm);
    assert_eq!(
        store
            .handle()
            .register_program(wasm.clone(), 1)
            .await
            .expect("import")
            .0,
        hash
    );
    assert_eq!(
        store.handle().load_program(hash).await.expect("load"),
        Some(StoredProgram {
            hash,
            wasm: wasm.clone()
        })
    );
    store
        .handle()
        .remove_program(hash, 2)
        .await
        .expect("remove");
    assert_eq!(
        store.handle().load_program(hash).await.expect("retained"),
        Some(StoredProgram { hash, wasm })
    );
    store
        .handle()
        .register_program(vec![9, 8, 7], 3)
        .await
        .expect("reactivate");
    assert_eq!(
        store.handle().list_programs(10).await.expect("list"),
        vec![hash]
    );
    store.shutdown().await.expect("shutdown");
}

#[test]
fn envelopes_reject_wrong_kind_oversize_and_tampering() {
    let encoded = envelope(EnvelopeKind::Program, b"x").expect("encode");
    assert!(matches!(
        open_envelope(EnvelopeKind::ExecutionState, &encoded, 8),
        Err(StoreError::Corruption(_))
    ));
    assert!(matches!(
        open_envelope(EnvelopeKind::Program, &encoded, 0),
        Err(StoreError::Corruption(_))
    ));

    let mut tampered = envelope(EnvelopeKind::ExecutionState, b"state").expect("encode");
    let last = tampered.len() - 1;
    tampered[last] ^= 0x80;
    assert!(matches!(
        open_envelope(EnvelopeKind::ExecutionState, &tampered, 64),
        Err(StoreError::Corruption(_))
    ));
}

#[tokio::test]
async fn request_failure_is_compare_and_set() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store = Store::open(StoreConfig::new(
        directory.path().join("store.sqlite"),
        host(6),
    ))
    .expect("open");
    let wasm = vec![4, 5];
    let hash = ProgramHash::of(&wasm);
    store
        .handle()
        .register_program(wasm.clone(), 1)
        .await
        .expect("program");
    let id = ExecId([2; 32]);
    let admission = explicit_admission(host(6), NegotiationId([3; 32]));
    let mut writer = store
        .handle()
        .claim_execution(id)
        .expect("execution writer");
    writer
        .create_execution_request(
            hash,
            Some(JsonBytes::try_new(b"null".to_vec()).expect("json")),
            admission,
            1,
        )
        .await
        .expect("request");
    assert_eq!(
        writer
            .record_execution_request_failure("bad")
            .await
            .expect("failure"),
        ExecutionRequestFailureOutcome::Recorded
    );
    assert_eq!(
        writer
            .record_execution_request_failure("bad")
            .await
            .expect("retry"),
        ExecutionRequestFailureOutcome::AlreadyRecorded
    );
    assert_eq!(
        store
            .handle()
            .load_execution_request(id)
            .await
            .expect("load")
            .expect("request")
            .failure(),
        Some("bad")
    );
    drop(writer);
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn request_failure_cannot_compete_with_activation_authority() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let fixture = activation_fixture();
    let store = Store::open(StoreConfig::new(&path, fixture.producer)).expect("open");
    let program_hash = ProgramHash::of(&fixture.program);
    store
        .handle()
        .register_program(fixture.program.clone(), 1)
        .await
        .expect("program");
    let execution_id = ExecId([0x24; 32]);
    let peers = fixture
        .prepared
        .tickets()
        .iter()
        .map(|ticket| ticket.data.signer)
        .collect();
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    writer
        .create_execution_request(
            program_hash,
            Some(JsonBytes::try_new(br#"{}"#.to_vec()).expect("params")),
            ExecutionAdmission::explicit(NegotiationId([0x11; 32]), peers).expect("admission"),
            2,
        )
        .await
        .expect("request");
    writer
        .prepare_activation(fixture.prepared, 3)
        .await
        .expect("prepare activation");
    assert!(matches!(
        writer.record_execution_request_failure("late").await,
        Err(StoreError::ExecutionLifecycleStarted(id)) if id == execution_id
    ));
    drop(writer);
    store.shutdown().await.expect("shutdown");

    let connection = Connection::open(&path).expect("inspect");
    connection
        .execute(
            "UPDATE exec_requests SET failure = 'tampered' WHERE execution_id = ?1",
            params![execution_id.0.to_vec()],
        )
        .expect("inject impossible lifecycle state");
    drop(connection);
    assert!(matches!(
        Store::open(StoreConfig::new(&path, fixture.producer)),
        Err(StoreError::Corruption(message))
            if message.contains("failed execution request")
    ));
}

#[tokio::test]
async fn recovery_projection_filters_terminal_history_before_paging() {
    let directory = tempfile::tempdir().expect("tempdir");
    let host_id = host(7);
    let store = Store::open(StoreConfig::new(
        directory.path().join("store.sqlite"),
        host_id,
    ))
    .expect("open");
    let program = vec![4, 5];
    let (program_hash, _) = store
        .handle()
        .register_program(program, 1)
        .await
        .expect("program");

    let mut execution_ids = Vec::new();
    for index in 0_u8..4 {
        let execution_id = ExecId([index; 32]);
        execution_ids.push(execution_id);
        let mut writer = store
            .handle()
            .claim_execution(execution_id)
            .expect("execution writer");
        writer
            .create_execution_request(
                program_hash,
                Some(JsonBytes::try_new(b"null".to_vec()).expect("params")),
                explicit_admission(host_id, NegotiationId([index; 32])),
                u64::from(index),
            )
            .await
            .expect("request");
        if index < 3 {
            writer
                .record_execution_request_failure("terminal history")
                .await
                .expect("request failure");
        }
    }

    let page = store
        .handle()
        .list_recovery_candidates(RecoveryCursor::start(), 1)
        .await
        .expect("recovery page");
    assert_eq!(page.candidates().len(), 1);
    assert_eq!(
        page.candidates()[0].request().execution_id(),
        execution_ids[3]
    );
    assert_eq!(page.candidates()[0].program(), Some(program_hash));
    let next = page.next_cursor().expect("filled page cursor");
    let end = store
        .handle()
        .list_recovery_candidates(next, 1)
        .await
        .expect("end page");
    assert!(end.is_empty());
    assert_eq!(end.next_cursor(), None);

    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn recovery_projection_pages_a_maximal_execution_state() {
    let initial_shared =
        SharedStateBytes::try_new(vec![0; MAX_SHARED_STATE_BYTES]).expect("maximal shared state");
    let fixture = activation_fixture_with_initial_state(initial_shared.clone());
    let directory = tempfile::tempdir().expect("tempdir");
    let store = Store::open(StoreConfig::new(
        directory.path().join("store.sqlite"),
        fixture.producer,
    ))
    .expect("open");
    let program_hash = ProgramHash::of(&fixture.program);
    store
        .handle()
        .register_program(fixture.program.clone(), 1)
        .await
        .expect("program");

    let execution_id = ExecId([0x7f; 32]);
    let peers = fixture
        .prepared
        .tickets()
        .iter()
        .map(|ticket| ticket.data.signer)
        .collect();
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    writer
        .create_execution_request(
            program_hash,
            Some(JsonBytes::try_new(br#"{}"#.to_vec()).expect("params")),
            ExecutionAdmission::explicit(NegotiationId([0x11; 32]), peers).expect("admission"),
            2,
        )
        .await
        .expect("request");
    writer
        .prepare_activation(fixture.prepared.clone(), 3)
        .await
        .expect("prepare");
    writer
        .commit_activation(fixture.activation.clone(), 4)
        .await
        .expect("commit");
    writer
        .create_execution(
            fixture.activation.clone(),
            fixture.producer,
            initial_shared,
            LocalStateBytes::try_new(vec![0; MAX_LOCAL_STATE_BYTES]).expect("maximal local state"),
            5,
        )
        .await
        .expect("execution");
    drop(writer);

    let page = store
        .handle()
        .list_recovery_candidates(RecoveryCursor::start(), 1)
        .await
        .expect("large state does not overflow a recovery page");
    assert_eq!(page.candidates().len(), 1);
    let candidate = &page.candidates()[0];
    assert_eq!(candidate.request().execution_id(), execution_id);
    assert_eq!(candidate.program(), Some(program_hash));
    assert_eq!(
        candidate.activation_status(),
        Some(ActivationRecordStatus::Committed)
    );
    assert_eq!(
        candidate.session_id(),
        Some(fixture.activation.session_hash())
    );
    assert!(candidate.has_execution());

    let next = page.next_cursor().expect("filled page cursor");
    let end = store
        .handle()
        .list_recovery_candidates(next, 1)
        .await
        .expect("end page");
    assert!(end.is_empty());
    assert_eq!(end.next_cursor(), None);

    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn activation_prepare_commit_is_idempotent_and_recoverable() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let store = Store::open(StoreConfig::new(&path, fixture.producer)).expect("open");
    let hash = ProgramHash::of(&fixture.program);
    store
        .handle()
        .register_program(fixture.program.clone(), 1)
        .await
        .expect("program");
    let id = ExecId([0xee; 32]);
    let admission = ExecutionAdmission::explicit(
        NegotiationId([0x11; 32]),
        fixture
            .prepared
            .tickets()
            .iter()
            .map(|ticket| ticket.data.signer)
            .collect(),
    )
    .expect("fixture admission");
    let mut writer = store
        .handle()
        .claim_execution(id)
        .expect("execution writer");
    writer
        .create_execution_request(
            hash,
            Some(JsonBytes::try_new(b"{}".to_vec()).expect("json")),
            admission,
            1,
        )
        .await
        .expect("request");
    assert!(matches!(
        writer
            .prepare_activation(fixture.prepared.clone(), 2)
            .await
            .expect("prepare"),
        PrepareActivationOutcome::Prepared(_)
    ));
    assert!(matches!(
        writer
            .prepare_activation(fixture.prepared.clone(), 3)
            .await
            .expect("prepare retry"),
        PrepareActivationOutcome::AlreadyPrepared(_)
    ));
    assert!(matches!(
        writer
            .commit_activation(fixture.activation.clone(), 4)
            .await
            .expect("commit"),
        CommitActivationOutcome::Committed(_)
    ));
    assert!(matches!(
        writer
            .commit_activation(fixture.activation.clone(), 5)
            .await
            .expect("commit retry"),
        CommitActivationOutcome::AlreadyCommitted(_)
    ));
    let shared = SharedStateBytes::try_new(vec![0]).expect("shared");
    let local = LocalStateBytes::try_new(Vec::new()).expect("local");
    assert!(matches!(
        writer
            .create_execution(
                fixture.activation.clone(),
                fixture.producer,
                shared,
                local,
                6,
            )
            .await
            .expect("execution"),
        CreateExecutionOutcome::Created(_)
    ));
    let active = activate_record(&mut writer, ExecutionVersion::ZERO, 7)
        .await
        .expect("activate");
    assert!(matches!(active.status(), ExecutionStatus::Active));
    assert!(
        activate_record(&mut writer, ExecutionVersion::new(1), 8)
            .await
            .is_err()
    );
    let state = store
        .handle()
        .load_execution(id)
        .await
        .expect("load active")
        .expect("active state");
    let callout = arena0_program::CalloutRequest {
        callout_index: 0,
        context: vec![0xaa],
    };
    assert!(matches!(
        dispatch_record(&mut writer,
                state.version(),
                session_started_event(&fixture),
                SharedStateBytes::try_new(vec![0]).expect("shared state"),
                LocalStateBytes::try_new(Vec::new()).expect("local state"),
                vec![Effect::SetTimer {
                    delay_ms: 10,
                    timer: TimerPayload::unit(),
                },],
                None,
                None,
                None,
                Some(callout),
                9,
            )
            .await
            .expect("arm timer"),
        state if state.pending_shared().is_some()
    ));
    drop(writer);
    let mut writer = store
        .handle()
        .claim_execution(id)
        .expect("execution writer");
    sign_step(&store, &fixture, id, &mut writer, 10, 11).await;
    assert!(writer.due_timers(20, 1).await.expect("not due").is_empty());
    assert_eq!(writer.due_timers(21, 8).await.expect("due").len(), 1);
    drop(writer);
    store.shutdown().await.expect("shutdown");
    let reopened = Store::open(StoreConfig::new(&path, fixture.producer)).expect("reopen");
    assert!(
        reopened
            .handle()
            .load_activation(id)
            .await
            .expect("load")
            .is_some()
    );
    let writer = reopened
        .handle()
        .claim_execution(id)
        .expect("execution writer");
    assert_eq!(
        writer
            .due_timers(21, 8)
            .await
            .expect("due after reopen")
            .len(),
        1
    );
    let recovered = reopened.handle().load_execution(id).await.unwrap().unwrap();
    assert_eq!(recovered.callout().unwrap().context, vec![0xaa]);
    drop(writer);
    reopened.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn deferred_broadcast_commits_two_steps_at_one_event_position() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0xef; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load active")
        .expect("active state");
    assert!(matches!(
        dispatch_record(&mut writer,
                state.version(),
                session_started_event(&fixture),
                SharedStateBytes::try_new(vec![0]).expect("shared state"),
                LocalStateBytes::try_new(Vec::new()).expect("local state"),
                vec![Effect::Broadcast { data: vec![7, 8] },],
                None,
                None,
                None,
                Some(arena0_program::CalloutRequest {
                    callout_index: 0,
                    context: vec![3],
                }),
                7,
            )
            .await
            .expect("stage dispatch"),
        state if state.pending_shared().is_some()
    ));
    let proposed = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load proposal")
        .expect("proposal state");
    assert_eq!(proposed.event_position(), 1);
    assert_eq!(proposed.agreed_step(), 0);
    assert_eq!(
        proposed
            .pending_shared()
            .expect("proposal")
            .event_position(),
        0
    );

    let after_first = sign_step(&store, &fixture, execution_id, &mut writer, 8, 9).await;
    let successor = after_first.pending_shared().expect("deferred successor");
    assert_eq!(after_first.event_position(), 1);
    assert_eq!(after_first.agreed_step(), 1);
    assert_eq!(successor.event_position(), 0);
    assert_eq!(successor.entry().step, 1);
    assert!(successor.effects().is_empty());
    assert!(matches!(
        &successor.entry().event,
        Event::MessageReceived {
            position: 1,
            from,
            msg,
            ..
        } if *from == fixture.producer && msg == &[7, 8]
    ));

    let after_successor = sign_step(&store, &fixture, execution_id, &mut writer, 10, 11).await;
    assert_eq!(after_successor.event_position(), 1);
    assert_eq!(after_successor.agreed_step(), 2);
    assert!(after_successor.pending_shared().is_none());

    drop(writer);
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn pending_proposal_exposes_frames_but_withholds_callout() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0xf3; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");

    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load active")
        .expect("active state");
    dispatch_record(
        &mut writer,
        state.version(),
        session_started_event(&fixture),
        SharedStateBytes::try_new(vec![0]).expect("shared state"),
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        Vec::new(),
        None,
        None,
        None,
        None,
        7,
    )
    .await
    .expect("stage initial event");
    let after_start = sign_step(&store, &fixture, execution_id, &mut writer, 8, 9).await;
    let callout = arena0_program::CalloutRequest {
        callout_index: 0,
        context: vec![0x51],
    };
    let broadcast = vec![0x61, 0x62];
    dispatch_record(
        &mut writer,
        after_start.version(),
        Event::React,
        SharedStateBytes::try_new(vec![0]).expect("shared state"),
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        vec![Effect::Broadcast {
            data: broadcast.clone(),
        }],
        None,
        None,
        None,
        Some(callout),
        10,
    )
    .await
    .expect("stage broadcast proposal");

    let proposed = writer.load_execution().await.unwrap().unwrap();
    assert!(proposed.callout().is_none());
    assert!(
        proposed
            .current_frames(fixture.producer)
            .iter()
            .any(|frame| matches!(frame, ExecFrame::Message { data, .. } if data == &broadcast))
    );

    let committed = sign_step(&store, &fixture, execution_id, &mut writer, 12, 13).await;
    assert!(committed.pending_shared().is_none());
    assert_eq!(
        committed.callout().expect("callout pending").id,
        arena0_protocol::pending_id(execution_id, after_start.event_position())
    );

    assert_eq!(committed.callout().unwrap().context, vec![0x51]);
    drop(writer);
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn stopping_an_unsigned_proposal_recovers_only_current_evidence() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0xf1; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");

    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load active")
        .expect("active state");
    dispatch_record(
        &mut writer,
        state.version(),
        session_started_event(&fixture),
        SharedStateBytes::try_new(vec![0]).expect("shared state"),
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        Vec::new(),
        None,
        None,
        None,
        None,
        7,
    )
    .await
    .expect("stage session start");
    let after_start = sign_step(&store, &fixture, execution_id, &mut writer, 8, 9).await;
    let data = vec![0xa1, 0xa2];
    assert!(matches!(
        dispatch_record(&mut writer,
                after_start.version(),
                Event::React,
                SharedStateBytes::try_new(vec![0]).expect("shared state"),
                LocalStateBytes::try_new(Vec::new()).expect("local state"),
                vec![Effect::Broadcast { data: data.clone() }],
                None,
                None,
                None,
                None,
                11,
            )
            .await
            .expect("stage broadcast proposal"),
        state if state.pending_shared().is_some()
    ));
    let proposed = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load proposal")
        .expect("proposal state");
    let proposal = proposed
        .pending_shared()
        .expect("unsigned proposal")
        .clone();
    assert!(proposal.signatures().is_empty());
    let unsigned = AbortOccurrence::unsigned(
        fixture.activation.session_hash(),
        fixture.producer,
        AbortKind::Abort,
        91,
        "stop unsigned proposal",
        proposed.step_cursor(),
    )
    .expect("abort occurrence");
    let keys = NodeKeys::from_secret(SecretKey::from_bytes([1; 32]));
    let occurrence = unsigned
        .clone()
        .with_signature(keys.sign(&unsigned.signing_bytes().expect("abort bytes")))
        .expect("signed abort");
    stop_record(&mut writer, proposed.version(), occurrence, 13)
        .await
        .expect("stop unsigned proposal");

    let stopped = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load stopped state")
        .expect("stopped state");
    assert!(stopped.pending_shared().is_none());
    let frames = stopped.current_frames(fixture.producer);
    assert!(!frames.iter().any(|frame| matches!(
        frame,
        ExecFrame::Message { .. } | ExecFrame::StepSignature { .. }
    )));
    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, ExecFrame::Abort { .. }))
    );
    drop(writer);
    store.shutdown().await.expect("shutdown");

    let reopened = Store::open(StoreConfig::new(&path, fixture.producer)).expect("reopen");
    let recovered = reopened
        .handle()
        .load_execution(execution_id)
        .await
        .expect("recover stopped state")
        .expect("recovered state");
    assert!(recovered.pending_shared().is_none());
    assert_eq!(recovered.current_frames(fixture.producer), frames);
    reopened.shutdown().await.expect("shutdown reopened");
}

#[tokio::test]
async fn portable_events_stage_agreement_without_shared_state_delta() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0xf0; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load active")
        .expect("active state");
    assert!(matches!(
        dispatch_record(&mut writer,
                state.version(),
                session_started_event(&fixture),
                SharedStateBytes::try_new(vec![0]).expect("shared state"),
                LocalStateBytes::try_new(Vec::new()).expect("local state"),
                Vec::new(),
                None,
                None,
                None,
                None,
                7,
            )
            .await
            .expect("stage session start"),
        state if state.pending_shared().is_some()
    ));
    let state = sign_step(&store, &fixture, execution_id, &mut writer, 8, 9).await;
    assert_eq!(state.agreed_step(), 1);
    assert_eq!(state.event_position(), 1);

    let source = other_peer(&fixture);
    let position = state.agreed_step();
    let pre_state = state.agreed_state();
    let data = vec![0x42, 0x43];
    let message_id = MessageId::derive(
        state.binding().session_id(),
        source,
        position,
        pre_state,
        pre_state,
        &data,
    );
    assert!(matches!(
        dispatch_record(&mut writer,
                state.version(),
                Event::MessageReceived {
                    message_id,
                    from: source,
                    position,
                    pre_state,
                    msg: data,
                },
                SharedStateBytes::try_new(vec![0]).expect("shared state"),
                LocalStateBytes::try_new(Vec::new()).expect("local state"),
                Vec::new(),
                None,
                None,
                None,
                None,
                10,
            )
            .await
            .expect("stage message"),
        state if state.pending_shared().is_some()
    ));
    let state = sign_step(&store, &fixture, execution_id, &mut writer, 11, 12).await;
    assert_eq!(state.agreed_step(), 2);
    assert_eq!(state.event_position(), 2);
    let trace = store
        .handle()
        .read_trace(execution_id, 0, 2)
        .await
        .expect("read portable trace");
    assert_eq!(trace.len(), 2);
    assert!(matches!(trace[0].event, Event::SessionStarted { .. }));
    assert!(matches!(
        &trace[1].event,
        Event::MessageReceived {
            from,
            position: 1,
            msg,
            ..
        } if *from == source && msg == &[0x42, 0x43]
    ));
    drop(writer);
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn terminal_agreement_clears_open_callout() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0xf6; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    let callout = arena0_program::CalloutRequest {
        callout_index: 0,
        context: vec![0x41],
    };
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load active")
        .expect("active state");
    dispatch_record(
        &mut writer,
        state.version(),
        session_started_event(&fixture),
        SharedStateBytes::try_new(vec![0]).expect("shared state"),
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        vec![],
        None,
        None,
        None,
        Some(callout),
        7,
    )
    .await
    .expect("stage callout");
    let waiting = sign_step(&store, &fixture, execution_id, &mut writer, 8, 9).await;

    assert!(waiting.callout().is_some());
    let after_waiting = waiting;

    let data = vec![0x51, 0x52];
    let position = after_waiting.agreed_step();
    let pre_state = after_waiting.agreed_state();
    let message_id = MessageId::derive(
        after_waiting.binding().session_id(),
        fixture.producer,
        position,
        pre_state,
        pre_state,
        &data,
    );
    let outcome = vec![0x61, 0x62];
    dispatch_record(
        &mut writer,
        after_waiting.version(),
        Event::MessageReceived {
            message_id,
            from: fixture.producer,
            position,
            pre_state,
            msg: data,
        },
        SharedStateBytes::try_new(vec![0]).expect("terminal shared state"),
        LocalStateBytes::try_new(Vec::new()).expect("terminal local state"),
        vec![Effect::SessionEnd {
            outcome: outcome.clone(),
        }],
        Some(TerminalOutcome::new(outcome, br#"null"#.to_vec()).expect("terminal outcome")),
        None,
        None,
        None,
        12,
    )
    .await
    .expect("stage terminal agreement");
    let staged = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load terminal proposal")
        .expect("terminal proposal");
    assert!(staged.pending_shared().is_some());
    sign_step(&store, &fixture, execution_id, &mut writer, 13, 14).await;

    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .unwrap()
        .unwrap();
    assert!(state.callout().is_none());
    drop(writer);
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn authenticated_stop_clears_open_callout() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0xf7; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load active")
        .expect("active state");
    dispatch_record(
        &mut writer,
        state.version(),
        session_started_event(&fixture),
        SharedStateBytes::try_new(vec![0]).expect("shared state"),
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        vec![],
        None,
        None,
        None,
        Some(arena0_program::CalloutRequest {
            callout_index: 0,
            context: vec![0x71],
        }),
        7,
    )
    .await
    .expect("stage callout");
    let waiting = sign_step(&store, &fixture, execution_id, &mut writer, 8, 9).await;
    assert!(waiting.callout().is_some());
    let after_waiting = waiting;
    let unsigned = AbortOccurrence::unsigned(
        fixture.activation.session_hash(),
        fixture.producer,
        AbortKind::Abort,
        94,
        "stop stale continuation",
        after_waiting.step_cursor(),
    )
    .expect("abort occurrence");
    let keys = NodeKeys::from_secret(SecretKey::from_bytes([1; 32]));
    let occurrence = unsigned
        .clone()
        .with_signature(keys.sign(&unsigned.signing_bytes().expect("abort bytes")))
        .expect("signed abort");
    stop_record(&mut writer, after_waiting.version(), occurrence, 12)
        .await
        .expect("stop execution");
    let stopped = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load stopped state")
        .expect("stopped state");
    assert!(stopped.status().terminal_cause().is_some());

    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .unwrap()
        .unwrap();
    assert!(state.callout().is_none());
    drop(writer);
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn answered_callout_stays_open_while_dispatch_proposal_is_staged() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0xf5; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");

    let callout = arena0_program::CalloutRequest {
        callout_index: 0,
        context: vec![0x91],
    };
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load active")
        .expect("active state");
    dispatch_record(
        &mut writer,
        state.version(),
        session_started_event(&fixture),
        SharedStateBytes::try_new(vec![0]).expect("shared state"),
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        vec![],
        None,
        None,
        None,
        Some(callout),
        7,
    )
    .await
    .expect("stage callout");
    let waiting = sign_step(&store, &fixture, execution_id, &mut writer, 8, 9).await;
    let pending_id = waiting.callout().expect("pending callout").id;

    dispatch_record(
        &mut writer,
        waiting.version(),
        Event::InputReceived {
            callout_index: 0,
            data: vec![0x92],
        },
        SharedStateBytes::try_new(vec![1]).expect("updated shared state"),
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        vec![Effect::Broadcast { data: vec![0xa0] }],
        None,
        None,
        Some(pending_id),
        None,
        10,
    )
    .await
    .expect("stage input proposal");
    let proposed = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load staged input")
        .expect("staged input state");
    assert!(proposed.pending_shared().is_some());
    assert!(
        proposed
            .pending_shared()
            .expect("proposal")
            .callout()
            .is_none()
    );
    assert_eq!(proposed.callout(), waiting.callout());

    drop(writer);
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn flat_dispatch_persists_pending_request_and_event_summaries() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0x5c; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");

    let callout = arena0_program::CalloutRequest {
        callout_index: 0,
        context: vec![3, 4],
    };
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load active")
        .expect("active state");
    dispatch_record(
        &mut writer,
        state.version(),
        session_started_event(&fixture),
        SharedStateBytes::try_new(vec![0]).expect("shared state"),
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        vec![],
        None,
        None,
        None,
        Some(callout),
        20,
    )
    .await
    .expect("stage callout");
    sign_step(&store, &fixture, execution_id, &mut writer, 21, 22).await;
    let pending_id = arena0_protocol::pending_id(execution_id, 0);

    let committed = store
        .handle()
        .load_execution(execution_id)
        .await
        .unwrap()
        .unwrap();
    let request = committed.callout().unwrap();
    assert_eq!(request.id, pending_id);
    assert_eq!(request.callout_index, 0);
    assert_eq!(request.context, vec![3, 4]);

    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load waiting state")
        .expect("waiting state");
    dispatch_record(
        &mut writer,
        state.version(),
        Event::InputReceived {
            callout_index: 0,
            data: vec![0xfe],
        },
        SharedStateBytes::try_new(vec![0]).expect("shared state"),
        LocalStateBytes::try_new(vec![2]).expect("local state"),
        Vec::new(),
        None,
        None,
        Some(pending_id),
        None,
        24,
    )
    .await
    .expect("consume callout");

    let final_state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load final state")
        .expect("final state");
    assert_eq!(final_state.shared_state().as_bytes(), &[0]);
    assert_eq!(final_state.local_state().as_bytes(), &[2]);
    drop(writer);

    let page = store
        .handle()
        .read_event_summaries(execution_id, Some(0), 8)
        .await
        .expect("event inspection page");
    assert_eq!(page.total(), 2);
    assert_eq!(page.next(), None);
    assert_eq!(page.summaries().len(), 2);
    assert_eq!(page.summaries()[0].event_position, 0);
    assert_eq!(page.summaries()[0].agreed_steps, vec![0]);
    assert_eq!(page.summaries()[0].event, EventKind::SessionStarted);
    assert_eq!(page.summaries()[0].input_payload_bytes, None);
    assert!(page.summaries()[0].effects.is_empty());
    assert_eq!(page.summaries()[1].event_position, 1);
    assert_eq!(page.summaries()[1].agreed_steps, Vec::<u64>::new());
    assert_eq!(page.summaries()[1].event, EventKind::InputReceived);
    assert_eq!(page.summaries()[1].input_payload_bytes, Some(1));
    assert!(page.summaries()[1].effects.is_empty());
    assert!(final_state.callout().is_none());
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn activation_cannot_exceed_durable_admission_authority() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let store = Store::open(StoreConfig::new(
        directory.path().join("store.sqlite"),
        fixture.producer,
    ))
    .expect("open");
    let hash = ProgramHash::of(&fixture.program);
    store
        .handle()
        .register_program(fixture.program.clone(), 1)
        .await
        .expect("program");

    let explicit_id = ExecId([0xa1; 32]);
    let explicit = ExecutionAdmission::explicit(
        NegotiationId([0x11; 32]),
        vec![fixture.producer, PeerId([0xff; 32])],
    )
    .expect("explicit admission");
    let mut explicit_writer = store
        .handle()
        .claim_execution(explicit_id)
        .expect("execution writer");
    explicit_writer
        .create_execution_request(
            hash,
            Some(JsonBytes::try_new(b"{}".to_vec()).expect("json")),
            explicit,
            2,
        )
        .await
        .expect("request");
    assert!(matches!(
        explicit_writer
            .prepare_activation(fixture.prepared.clone(), 3)
            .await,
        Err(StoreError::InvalidAdmission(_))
    ));

    let join_id = ExecId([0xa2; 32]);
    let mut join_writer = store
        .handle()
        .claim_execution(join_id)
        .expect("execution writer");
    join_writer
        .create_execution_request(
            hash,
            Some(JsonBytes::try_new(b"{}".to_vec()).expect("json")),
            ExecutionAdmission::join(PeerId([0xfe; 32]), NegotiationId([0x11; 32])),
            4,
        )
        .await
        .expect("request");
    assert!(matches!(
        join_writer.prepare_activation(fixture.prepared, 5).await,
        Err(StoreError::InvalidAdmission(_))
    ));
    drop(explicit_writer);
    drop(join_writer);
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn registry_integrity_fails_closed_after_database_tamper() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let store = Store::open(StoreConfig::new(&path, host(9))).expect("open");
    let wasm = vec![6, 7, 8];
    let hash = ProgramHash::of(&wasm);
    store
        .handle()
        .register_program(wasm, 1)
        .await
        .expect("program");
    store.shutdown().await.expect("shutdown");
    let connection = Connection::open(&path).expect("inspect");
    connection
        .execute(
            "UPDATE programs SET wasm = x'00' WHERE program_hash = ?1",
            rusqlite::params![hash.as_bytes().to_vec()],
        )
        .expect("tamper");
    drop(connection);
    assert!(matches!(
        Store::open(StoreConfig::new(&path, host(9))),
        Err(StoreError::Corruption(_))
    ));
}

#[tokio::test]
async fn receipt_is_published_from_durable_rows_after_restart() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0x31; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    certify_terminal(&store, &fixture, execution_id).await;
    store.shutdown().await.expect("shutdown before assembly");

    // The certified final step and all agreed trace rows are durable before
    // publication is requested. This exercises restart recovery rather than
    // an in-memory execution shortcut.
    let store = Store::open(StoreConfig::new(&path, fixture.producer)).expect("reopen");
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load terminal state")
        .expect("terminal state");
    let published = publication_record(&mut writer, state.version(), 12)
        .await
        .expect("publish");
    assert!(matches!(
        published.status(),
        ExecutionStatus::Completed { .. }
    ));
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load published state")
        .expect("published execution");
    let published = store
        .handle()
        .load_receipt_by_id(state.published_receipt_id().expect("published id"))
        .await
        .expect("load")
        .expect("artifact");
    assert_eq!(published.receipt.body().trace().len(), 1);
    drop(writer);
    store.shutdown().await.expect("shutdown after assembly");

    let store = Store::open(StoreConfig::new(&path, fixture.producer)).expect("reopen published");
    let receipt = load_published_receipt(&store, &fixture).await;
    let key = fixture.activation.session_hash();
    let by_id = store
        .handle()
        .load_receipt_by_id(receipt.receipt_id())
        .await
        .expect("secondary lookup")
        .expect("receipt by id");
    assert_eq!(by_id.receipt.body().header().session_hash(), key);
    assert_eq!(by_id.receipt, receipt);
    assert_eq!(
        store.handle().list_receipts(8).await.expect("list"),
        vec![by_id]
    );
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn stopped_receipt_is_published_after_restart_and_verifies() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0x32; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load active")
        .expect("active execution");
    let producer_keys = NodeKeys::from_secret(SecretKey::from_bytes([1; 32]));
    let unsigned = AbortOccurrence::unsigned(
        fixture.activation.session_hash(),
        fixture.producer,
        AbortKind::Abort,
        77,
        "operator stop",
        state.step_cursor(),
    )
    .expect("abort occurrence");
    let occurrence = unsigned
        .clone()
        .with_signature(producer_keys.sign(&unsigned.signing_bytes().expect("abort bytes")))
        .expect("signed abort");
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    stop_record(&mut writer, state.version(), occurrence, 7)
        .await
        .expect("abort");
    drop(writer);
    store
        .shutdown()
        .await
        .expect("shutdown before stopped publication");

    let store = Store::open(StoreConfig::new(&path, fixture.producer)).expect("reopen stopped");
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load stopped state")
        .expect("stopped state");
    publication_record(&mut writer, state.version(), 8)
        .await
        .expect("publish stopped");
    drop(writer);
    store
        .shutdown()
        .await
        .expect("shutdown after stopped publication");

    let store = Store::open(StoreConfig::new(&path, fixture.producer)).expect("reopen published");
    let receipt = load_published_receipt(&store, &fixture).await;
    let encoded = receipt.encode().expect("encode receipt");
    let verified = ReceiptArtifact::decode(&encoded).expect("verify stopped receipt");
    assert!(matches!(
        verified.body().termination(),
        ReceiptTermination::Stopped {
            cause: arena0_protocol::StopCause::Authenticated(found),
        } if found.reason() == "operator stop"
    ));
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn persisted_receipt_tampering_fails_closed_on_restart() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0x33; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load active")
        .expect("active execution");
    let producer_keys = NodeKeys::from_secret(SecretKey::from_bytes([1; 32]));
    let unsigned = AbortOccurrence::unsigned(
        fixture.activation.session_hash(),
        fixture.producer,
        AbortKind::Fail,
        88,
        "tamper fixture",
        state.step_cursor(),
    )
    .expect("abort occurrence");
    let occurrence = unsigned
        .clone()
        .with_signature(producer_keys.sign(&unsigned.signing_bytes().expect("abort bytes")))
        .expect("signed abort");
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    stop_record(&mut writer, state.version(), occurrence, 7)
        .await
        .expect("abort");
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load stopped state")
        .expect("stopped state");
    publication_record(&mut writer, state.version(), 8)
        .await
        .expect("publish stopped");
    drop(writer);
    let published = load_published_receipt(&store, &fixture).await;
    store.shutdown().await.expect("shutdown");

    let connection = Connection::open(&path).expect("inspect");
    let mut artifact: Vec<u8> = connection
        .query_row(
            "SELECT artifact FROM receipts WHERE receipt_id = ?1",
            rusqlite::params![published.receipt_id().as_bytes().to_vec()],
            |row| row.get(0),
        )
        .expect("receipt artifact");
    let last = artifact.len().checked_sub(1).expect("nonempty artifact");
    artifact[last] ^= 0x80;
    connection
        .execute(
            "UPDATE receipts SET artifact = ?1 WHERE receipt_id = ?2",
            rusqlite::params![artifact, published.receipt_id().as_bytes().to_vec()],
        )
        .expect("tamper");
    drop(connection);
    assert!(matches!(
        Store::open(StoreConfig::new(&path, fixture.producer)),
        Err(StoreError::Corruption(_))
    ));
}

#[tokio::test]
async fn unpublished_execution_rejects_terminal_projection_rows_on_restart() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0x37; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    store.shutdown().await.expect("shutdown");

    let connection = Connection::open(&path).expect("inspect");
    connection
        .execute(
            "INSERT INTO terminal_proofs
             (execution_id, version, receipt_id, publication)
             VALUES (?1, 1, ?2, ?3)",
            rusqlite::params![execution_id.0.to_vec(), [2u8; 32].to_vec(), [3u8],],
        )
        .expect("insert unexpected terminal row");
    drop(connection);
    assert!(matches!(
        Store::open(StoreConfig::new(&path, fixture.producer)),
        Err(StoreError::Corruption(_))
    ));
}

#[tokio::test]
async fn published_receipts_require_terminal_proof_and_production_rows_on_restart() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    for (byte, row, delete) in [
        (
            0x34,
            "receipt-production",
            "DELETE FROM receipt_productions WHERE execution_id = ?1",
        ),
        (
            0x35,
            "terminal-proof",
            "DELETE FROM terminal_proofs WHERE execution_id = ?1",
        ),
    ] {
        let path = directory.path().join(format!("missing-{row}.sqlite"));
        let execution_id = ExecId([byte; 32]);
        let store = create_execution(&path, &fixture, execution_id).await;
        certify_terminal(&store, &fixture, execution_id).await;
        let mut writer = store
            .handle()
            .claim_execution(execution_id)
            .expect("execution writer");
        publish_current(&store, &mut writer, execution_id, 12)
            .await
            .expect("publish");
        drop(writer);
        load_published_receipt(&store, &fixture).await;
        store.shutdown().await.expect("shutdown");

        let connection = Connection::open(&path).expect("inspect");
        connection
            .execute(delete, rusqlite::params![execution_id.0.to_vec()])
            .expect("remove required publication row");
        drop(connection);
        assert!(matches!(
            Store::open(StoreConfig::new(&path, fixture.producer)),
            Err(StoreError::Corruption(_))
        ));
    }
}

#[tokio::test]
async fn imported_receipt_is_durable_and_reimport_is_idempotent() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let produced_path = directory.path().join("produced.sqlite");
    let imported_path = directory.path().join("imported.sqlite");
    let produced = create_execution(&produced_path, &fixture, ExecId([0x41; 32])).await;
    certify_terminal(&produced, &fixture, ExecId([0x41; 32])).await;
    let mut writer = produced
        .handle()
        .claim_execution(ExecId([0x41; 32]))
        .expect("execution writer");
    publish_current(&produced, &mut writer, ExecId([0x41; 32]), 12)
        .await
        .expect("publish");
    drop(writer);
    let receipt = load_published_receipt(&produced, &fixture).await;
    produced.shutdown().await.expect("shutdown produced");

    let imported_host = other_peer(&fixture);
    let imported = Store::open(StoreConfig::new(&imported_path, imported_host)).expect("open");
    assert_eq!(
        imported
            .handle()
            .import_receipt(receipt.clone(), 20)
            .await
            .expect("import"),
        ReceiptImportOutcome::Imported
    );
    assert_eq!(
        imported
            .handle()
            .import_receipt(receipt.clone(), 21)
            .await
            .expect("re-import"),
        ReceiptImportOutcome::AlreadyImported
    );
    let key = receipt.receipt_id();
    let stored = imported
        .handle()
        .load_receipt_by_id(key)
        .await
        .expect("load imported")
        .expect("stored imported receipt");
    assert_eq!(stored.receipt, receipt);
    assert_eq!(stored.provenance, ReceiptProvenance::Imported);
    assert_eq!(
        imported
            .handle()
            .list_receipts(8)
            .await
            .expect("list")
            .len(),
        1
    );
    imported.shutdown().await.expect("shutdown imported");

    let connection = Connection::open(&imported_path).expect("inspect imported row");
    let execution: Option<Vec<u8>> = connection
        .query_row(
            "SELECT execution_id FROM receipt_productions WHERE receipt_id = ?1",
            rusqlite::params![receipt.receipt_id().as_bytes().to_vec()],
            |row| row.get(0),
        )
        .optional()
        .expect("imported production relation");
    assert_eq!(execution, None);
    let imported_fact: Option<Vec<u8>> = connection
        .query_row(
            "SELECT receipt_id FROM receipt_imports WHERE receipt_id = ?1",
            rusqlite::params![receipt.receipt_id().as_bytes().to_vec()],
            |row| row.get(0),
        )
        .optional()
        .expect("imported fact");
    assert_eq!(
        imported_fact,
        Some(receipt.receipt_id().as_bytes().to_vec())
    );
    assert!(
        connection
            .execute(
                "INSERT INTO receipt_productions (receipt_id, execution_id)
                 VALUES (?1, ?2)",
                rusqlite::params![receipt.receipt_id().as_bytes().to_vec(), [0u8; 32].to_vec()],
            )
            .is_err()
    );
    drop(connection);

    let reopened = Store::open(StoreConfig::new(&imported_path, imported_host)).expect("reopen");
    let recovered = reopened
        .handle()
        .load_receipt_by_id(receipt.receipt_id())
        .await
        .expect("load by id")
        .expect("recovered imported receipt");
    assert_eq!(recovered.provenance, ReceiptProvenance::Imported);
    assert_eq!(recovered.receipt, receipt);
    reopened.shutdown().await.expect("shutdown reopened");

    let connection = Connection::open(&imported_path).expect("inspect");
    let mut artifact: Vec<u8> = connection
        .query_row(
            "SELECT artifact FROM receipts WHERE receipt_id = ?1",
            rusqlite::params![receipt.receipt_id().as_bytes().to_vec()],
            |row| row.get(0),
        )
        .expect("imported artifact");
    let last = artifact.len().checked_sub(1).expect("nonempty artifact");
    artifact[last] ^= 0x80;
    connection
        .execute(
            "UPDATE receipts SET artifact = ?1 WHERE receipt_id = ?2",
            rusqlite::params![artifact, receipt.receipt_id().as_bytes().to_vec()],
        )
        .expect("tamper imported artifact");
    drop(connection);
    assert!(matches!(
        Store::open(StoreConfig::new(&imported_path, imported_host)),
        Err(StoreError::Corruption(_))
    ));
}

#[tokio::test]
async fn imported_receipt_rejects_canonical_conflict_without_overwrite() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let produced_path = directory.path().join("produced.sqlite");
    let produced = create_execution(&produced_path, &fixture, ExecId([0x42; 32])).await;
    certify_terminal(&produced, &fixture, ExecId([0x42; 32])).await;
    let mut writer = produced
        .handle()
        .claim_execution(ExecId([0x42; 32]))
        .expect("execution writer");
    publish_current(&produced, &mut writer, ExecId([0x42; 32]), 12)
        .await
        .expect("publish");
    drop(writer);
    let receipt = load_published_receipt(&produced, &fixture).await;
    produced.shutdown().await.expect("shutdown produced");

    let store = Store::open(StoreConfig::new(&path, other_peer(&fixture))).expect("open");
    store
        .handle()
        .import_receipt(receipt.clone(), 20)
        .await
        .expect("import");
    let different = receipt_with_different_content(&receipt);
    assert_ne!(different.receipt_id(), receipt.receipt_id());
    assert_eq!(
        different.body().header().session_hash(),
        receipt.body().header().session_hash()
    );
    let error = store
        .handle()
        .import_receipt(different, 21)
        .await
        .expect_err("key conflict");
    assert!(
        matches!(error, StoreError::Corruption(message) if message.contains("different canonical receipt"))
    );
    let listed = store.handle().list_receipts(8).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].receipt, receipt);
    assert_eq!(listed[0].provenance, ReceiptProvenance::Imported);
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn exact_imported_receipt_is_promoted_by_local_publication() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let source_path = directory.path().join("source.sqlite");
    let target_path = directory.path().join("target.sqlite");
    let source = create_execution(&source_path, &fixture, ExecId([0x43; 32])).await;
    certify_terminal(&source, &fixture, ExecId([0x43; 32])).await;
    let mut writer = source
        .handle()
        .claim_execution(ExecId([0x43; 32]))
        .expect("execution writer");
    publish_current(&source, &mut writer, ExecId([0x43; 32]), 12)
        .await
        .expect("publish");
    drop(writer);
    let receipt = load_published_receipt(&source, &fixture).await;
    source.shutdown().await.expect("shutdown source");

    let imported = Store::open(StoreConfig::new(&target_path, fixture.producer)).expect("open");
    assert_eq!(
        imported
            .handle()
            .import_receipt(receipt.clone(), 20)
            .await
            .expect("import"),
        ReceiptImportOutcome::Imported
    );
    imported.shutdown().await.expect("shutdown imported");

    let target = create_execution(&target_path, &fixture, ExecId([0x44; 32])).await;
    certify_terminal(&target, &fixture, ExecId([0x44; 32])).await;
    let mut writer = target
        .handle()
        .claim_execution(ExecId([0x44; 32]))
        .expect("execution writer");
    publish_current(&target, &mut writer, ExecId([0x44; 32]), 12)
        .await
        .expect("publish");
    drop(writer);
    let published = load_published_receipt(&target, &fixture).await;
    assert_eq!(published, receipt);
    let stored = target
        .handle()
        .load_receipt_by_id(receipt.receipt_id())
        .await
        .expect("load promoted")
        .expect("promoted receipt");
    assert_eq!(stored.provenance, ReceiptProvenance::Both);
    let connection = Connection::open(&target_path).expect("inspect promoted facts");
    let import_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM receipt_imports WHERE receipt_id = ?1",
            rusqlite::params![receipt.receipt_id().as_bytes().to_vec()],
            |row| row.get(0),
        )
        .expect("import fact count");
    let production_execution: Vec<u8> = connection
        .query_row(
            "SELECT execution_id FROM receipt_productions WHERE receipt_id = ?1",
            rusqlite::params![receipt.receipt_id().as_bytes().to_vec()],
            |row| row.get(0),
        )
        .expect("production fact");
    assert_eq!(import_count, 1);
    assert_eq!(production_execution, [0x44; 32].to_vec());
    drop(connection);
    target.shutdown().await.expect("shutdown target");
}

#[tokio::test]
async fn locally_produced_receipt_import_retains_both_provenance() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0x45; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    certify_terminal(&store, &fixture, execution_id).await;
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    publish_current(&store, &mut writer, execution_id, 12)
        .await
        .expect("publish");
    drop(writer);
    let receipt = load_published_receipt(&store, &fixture).await;

    assert_eq!(
        store
            .handle()
            .import_receipt(receipt.clone(), 14)
            .await
            .expect("import produced receipt"),
        ReceiptImportOutcome::AlreadyProduced
    );
    let stored = store
        .handle()
        .load_receipt_by_id(receipt.receipt_id())
        .await
        .expect("load imported receipt")
        .expect("stored receipt");
    assert_eq!(stored.receipt, receipt);
    assert_eq!(stored.provenance, ReceiptProvenance::Both);
    store.shutdown().await.expect("shutdown");

    let connection = Connection::open(&path).expect("inspect facts");
    let import_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM receipt_imports WHERE receipt_id = ?1",
            rusqlite::params![receipt.receipt_id().as_bytes().to_vec()],
            |row| row.get(0),
        )
        .expect("import fact count");
    let production_execution: Vec<u8> = connection
        .query_row(
            "SELECT execution_id FROM receipt_productions WHERE receipt_id = ?1",
            rusqlite::params![receipt.receipt_id().as_bytes().to_vec()],
            |row| row.get(0),
        )
        .expect("production fact");
    assert_eq!(import_count, 1);
    assert_eq!(production_execution, execution_id.0.to_vec());
    drop(connection);

    let reopened = Store::open(StoreConfig::new(&path, fixture.producer)).expect("reopen");
    let recovered = reopened
        .handle()
        .load_receipt_by_id(receipt.receipt_id())
        .await
        .expect("load recovered receipt")
        .expect("recovered receipt");
    assert_eq!(recovered.provenance, ReceiptProvenance::Both);
    reopened.shutdown().await.expect("shutdown reopened");
}

#[tokio::test]
async fn distinct_stop_reports_coexist_without_impersonating_local_publication() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("reports.sqlite");
    let store = Store::open(StoreConfig::new(&path, host(9))).unwrap();
    let mut reports = Vec::new();
    for index in [1u8, 2] {
        let identity = NodeKeys::from_secret(SecretKey::from_bytes([index; 32]));
        let unsigned = arena0_protocol::AbortOccurrence::unsigned(
            fixture.activation.session_hash(),
            arena0_protocol::PeerId(identity.ed25519_public_key().0),
            arena0_protocol::AbortKind::Abort,
            0,
            "local observation",
            StepCursor::new(
                0,
                fixture.activation.offer().data().initial_state,
                arena0_protocol::CHAIN_START,
            ),
        )
        .unwrap();
        let signature = identity.sign(&unsigned.signing_bytes().unwrap());
        let body = arena0_protocol::ReceiptBody::new(
            arena0_protocol::SessionHeader::new(
                fixture.activation.clone(),
                arena0_protocol::ReceiptTermination::Stopped {
                    cause: arena0_protocol::StopCause::Authenticated(
                        unsigned.with_signature(signature).unwrap(),
                    ),
                },
            ),
            Vec::new(),
            fixture.activation.offer().data().params.as_bytes().to_vec(),
            Vec::new(),
        )
        .unwrap();
        let report = ReceiptArtifact::new(body).unwrap();
        assert!(matches!(report, ReceiptArtifact::StopReport(_)));
        assert_eq!(
            store
                .handle()
                .import_receipt(report.clone(), 1)
                .await
                .unwrap(),
            ReceiptImportOutcome::Imported
        );
        reports.push(report);
    }
    assert_ne!(reports[0].receipt_id(), reports[1].receipt_id());
    assert!(
        store
            .handle()
            .load_receipt(fixture.activation.session_hash())
            .await
            .unwrap()
            .is_none()
    );
    store.shutdown().await.unwrap();
    let store = Store::open(StoreConfig::new(&path, host(9))).unwrap();
    assert_eq!(store.handle().list_receipts(10).await.unwrap().len(), 2);
    for report in reports {
        let loaded = store
            .handle()
            .load_receipt_by_id(report.receipt_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.receipt.encode().unwrap(), report.encode().unwrap());
        assert_eq!(loaded.provenance, ReceiptProvenance::Imported);
        assert_eq!(
            store.handle().import_receipt(report, 2).await.unwrap(),
            ReceiptImportOutcome::AlreadyImported
        );
    }
    store.shutdown().await.unwrap();
}

#[test]
fn previous_store_schema_is_rejected_without_rewriting_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("old.sqlite");
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("PRAGMA user_version = 1; CREATE TABLE retained (artifact BLOB NOT NULL); INSERT INTO retained VALUES (x'010203');").unwrap();
    drop(connection);
    assert!(matches!(
        Store::open(StoreConfig::new(&path, host(9))),
        Err(StoreError::UnsupportedSchema(1))
    ));
    let connection = Connection::open(&path).unwrap();
    let bytes: Vec<u8> = connection
        .query_row("SELECT artifact FROM retained", [], |row| row.get(0))
        .unwrap();
    assert_eq!(bytes, [1, 2, 3]);
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 1);
}

async fn activate_record(
    writer: &mut ExecutionStore,
    expected: ExecutionVersion,
    now_ms: u64,
) -> Result<ExecutionState, StoreError> {
    let mut next = writer.load_execution().await?.expect("execution");
    next.activate()?;
    writer
        .persist(TransitionRecord {
            expected,
            next: next.clone(),
            change: Change::Activate,
            now_ms,
        })
        .await?;
    Ok(next)
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_record(
    writer: &mut ExecutionStore,
    expected: ExecutionVersion,
    event: Event<Vec<u8>>,
    shared: SharedStateBytes,
    local: LocalStateBytes,
    effects: Vec<Effect>,
    outcome: Option<TerminalOutcome>,
    timer_id: Option<TimerId>,
    pending_id: Option<arena0_protocol::PendingId>,
    callout: Option<arena0_program::CalloutRequest>,
    now_ms: u64,
) -> Result<ExecutionState, StoreError> {
    let mut next = writer.load_execution().await?.expect("execution");
    next.apply_dispatch(
        &event, shared, local, &effects, outcome, pending_id, callout,
    )?;
    writer
        .persist(TransitionRecord {
            expected,
            next: next.clone(),
            change: Change::Dispatch {
                event,
                effects,
                timer_id,
            },
            now_ms,
        })
        .await?;
    Ok(next)
}

async fn signature_record(
    writer: &mut ExecutionStore,
    expected: ExecutionVersion,
    signature: ParticipantStepSignature,
    now_ms: u64,
) -> Result<ExecutionState, StoreError> {
    let mut next = writer.load_execution().await?.expect("execution");
    let certified = next.add_step_signature(signature)?;
    writer
        .persist(TransitionRecord {
            expected,
            next: next.clone(),
            change: Change::StepSignature { certified },
            now_ms,
        })
        .await?;
    Ok(next)
}

async fn stop_record(
    writer: &mut ExecutionStore,
    expected: ExecutionVersion,
    occurrence: AbortOccurrence,
    now_ms: u64,
) -> Result<ExecutionState, StoreError> {
    let mut next = writer.load_execution().await?.expect("execution");
    next.stop(occurrence)?;
    writer
        .persist(TransitionRecord {
            expected,
            next: next.clone(),
            change: Change::Stop,
            now_ms,
        })
        .await?;
    Ok(next)
}

async fn publication_record(
    writer: &mut ExecutionStore,
    expected: ExecutionVersion,
    now_ms: u64,
) -> Result<ExecutionState, StoreError> {
    let mut next = writer.load_execution().await?.expect("execution");
    let artifact = writer.assemble_receipt(&next).await?;
    next.publish_receipt(artifact.clone())?;
    writer
        .persist(TransitionRecord {
            expected,
            next: next.clone(),
            change: Change::Publish { artifact },
            now_ms,
        })
        .await?;
    Ok(next)
}

#[tokio::test]
async fn stale_transition_writes_neither_state_nor_side_rows() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("stale.sqlite");
    let execution_id = ExecId([0x91; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    let mut writer = store.handle().claim_execution(execution_id).unwrap();
    let before = writer.load_execution().await.unwrap().unwrap();
    let event = session_started_event(&fixture);
    let mut next = before.clone();
    next.apply_dispatch(
        &event,
        before.shared_state().clone(),
        before.local_state().clone(),
        &[],
        None,
        None,
        None,
    )
    .unwrap();
    let result = writer
        .persist(TransitionRecord {
            expected: ExecutionVersion::ZERO,
            next,
            change: Change::Dispatch {
                event,
                effects: Vec::new(),
                timer_id: None,
            },
            now_ms: 10,
        })
        .await;
    assert!(
        matches!(result, Err(StoreError::Corruption(reason)) if reason == "execution version moved")
    );
    assert_eq!(writer.load_execution().await.unwrap().unwrap(), before);
    let connection = Connection::open(&path).unwrap();
    for table in ["event_records", "agreed_steps", "active_timers"] {
        let count: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "stale transition wrote {table}");
    }
    drop(connection);
    drop(writer);
    store.shutdown().await.unwrap();
}
