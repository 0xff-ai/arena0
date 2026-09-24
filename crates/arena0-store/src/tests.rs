use super::*;
use arena0_crypto::bls::BlsSecretKey;
use arena0_crypto::{BlsSignature, NodeKeys, SecretKey, key_binding_message};
use arena0_program::{
    JsonBytes, LocalStateBytes, MAX_LOCAL_STATE_BYTES, MAX_SHARED_STATE_BYTES, SharedStateBytes,
};
use arena0_protocol::{
    AbortKind, AbortOccurrence, ActivationData, Effect, Ensemble, Event, ExecFrame, ExecutionState,
    ExecutionVersion, MessageId, NegotiationId, NegotiationTarget, Offer, OfferData,
    ParticipantStepSignature, ParticipantTerminalSignature, PreparedActivation, ReceiptArtifact,
    ReceiptTermination, SessionHeader, SessionTerminal, StateHash, StepCursor, TerminalOutcome,
    Ticket, TicketAction, TicketData,
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
    writer
        .activate(ExecutionVersion::ZERO, 6)
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

fn local_continuation_rows(path: &Path, execution_id: ExecId) -> Vec<(String, Effect)> {
    let connection = Connection::open(path).expect("inspect database");
    let mut statement = connection
        .prepare(
            "SELECT status, payload
             FROM outbox
             WHERE execution_id = ?1 AND destination IS NULL
               AND payload_kind = 'effect'
             ORDER BY event_position, version, ordinal, outbox_id",
        )
        .expect("prepare continuation rows");
    let mut rows = statement
        .query(rusqlite::params![execution_id.0.to_vec()])
        .expect("query continuation rows");
    let mut result = Vec::new();
    while let Some(row) = rows.next().expect("read continuation row") {
        let status = row.get::<_, String>(0).expect("continuation status");
        let payload = row.get::<_, Vec<u8>>(1).expect("continuation payload");
        let effect = borsh::from_slice::<Effect>(&payload).expect("decode continuation effect");
        result.push((status, effect));
    }
    result
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
    writer
        .commit_step_signature(
            state.version(),
            ParticipantStepSignature::new(
                fixture.producer,
                commitment.step,
                producer_bls.sign(&commitment.signing_bytes()),
            ),
            None,
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
    let remote_frame = ExecFrame::StepSignature {
        commitment: commitment.clone(),
        signature: remote_signature,
    };
    assert!(matches!(
        writer
            .commit_step_signature(
                state.version(),
                ParticipantStepSignature::new(remote, commitment.step, remote_signature),
                None,
                second_now_ms,
            )
            .await,
        Err(StoreError::UnauthenticatedSource(_))
    ));
    assert_eq!(
        writer
            .accept_inbound(remote, remote_frame.clone(), second_now_ms)
            .await
            .expect("accept peer step signature"),
        InboxAcceptOutcome::Accepted
    );
    let inbox_id = store
        .handle()
        .list_pending_inbox(execution_id, 8)
        .await
        .expect("list peer step signature")
        .into_iter()
        .find(|item| item.source() == remote && item.frame() == &remote_frame)
        .map(|item| item.inbox_id())
        .expect("peer step signature inbox");
    writer
        .commit_step_signature(
            state.version(),
            ParticipantStepSignature::new(remote, commitment.step, remote_signature),
            Some(inbox_id),
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
    writer
        .commit_dispatch(
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
    let producer_bls = BlsSecretKey::from_seed(&[11; 32]).expect("producer bls");
    let other_bls = BlsSecretKey::from_seed(&[12; 32]).expect("other bls");
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load terminal pending")
        .expect("terminal state");
    let terminal_commitment = state
        .pending_terminal()
        .expect("terminal commitment")
        .clone();
    writer
        .commit_terminal_signature(
            state.version(),
            ParticipantTerminalSignature::new(
                fixture.producer,
                producer_bls.sign(&terminal_commitment.signing_bytes()),
            ),
            None,
            10,
        )
        .await
        .expect("producer terminal signature");
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load signed terminal")
        .expect("signed terminal state");
    let remote = other_peer(fixture);
    let remote_signature = other_bls.sign(&terminal_commitment.signing_bytes());
    let remote_frame = ExecFrame::End {
        commitment: terminal_commitment.clone(),
        signature: remote_signature,
    };
    assert!(matches!(
        writer
            .commit_terminal_signature(
                state.version(),
                ParticipantTerminalSignature::new(remote, remote_signature),
                None,
                11,
            )
            .await,
        Err(StoreError::UnauthenticatedSource(_))
    ));
    assert_eq!(
        writer
            .accept_inbound(remote, remote_frame.clone(), 11)
            .await
            .expect("accept peer terminal signature"),
        InboxAcceptOutcome::Accepted
    );
    let inbox_id = store
        .handle()
        .list_pending_inbox(execution_id, 8)
        .await
        .expect("list peer terminal signature")
        .into_iter()
        .find(|item| item.source() == remote && item.frame() == &remote_frame)
        .map(|item| item.inbox_id())
        .expect("peer terminal signature inbox");
    writer
        .commit_terminal_signature(
            state.version(),
            ParticipantTerminalSignature::new(remote, remote_signature),
            Some(inbox_id),
            11,
        )
        .await
        .expect("peer terminal signature");
    assert!(
        store
            .handle()
            .list_pending_inbox(execution_id, 8)
            .await
            .expect("list consumed terminal evidence")
            .is_empty()
    );
    drop(writer);
}

async fn publish_receipt(
    store: &Store,
    fixture: &ActivationFixture,
    execution_id: ExecId,
) -> ReceiptArtifact {
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load terminal")
        .expect("terminal state");
    writer
        .publish_terminal(state.version(), 13)
        .await
        .expect("publish");
    let key = fixture.activation.session_hash();
    store
        .handle()
        .load_receipt(key)
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
) -> Result<ApplyOutcome, StoreError> {
    let state = store
        .handle()
        .load_execution(execution_id)
        .await?
        .ok_or(StoreError::ExecutionNotFound(execution_id))?;
    writer.publish_terminal(state.version(), now_ms).await
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
    let terminal_commitment = arena0_protocol::TerminalCommitment::new(
        receipt.body().header().session_hash(),
        trace[0].step,
        trace[0].post_state,
        arena0_protocol::OutcomeHash::of(&changed_outcome),
    );
    let terminal = SessionTerminal {
        final_step: terminal_commitment.final_step,
        final_state: terminal_commitment.final_state,
        outcome_hash: terminal_commitment.outcome_hash,
        agreement: arena0_protocol::AggregateAttestation::from_signatures(
            arena0_protocol::SignerSet::full(2).expect("signer set"),
            &[
                producer_bls.sign(&terminal_commitment.signing_bytes()),
                other_bls.sign(&terminal_commitment.signing_bytes()),
            ],
        )
        .expect("terminal agreement"),
    };
    let header = SessionHeader::new(
        receipt.body().header().activation.clone(),
        ReceiptTermination::Completed { terminal },
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
async fn sqlite_owner_survives_reopen() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let config = StoreConfig::new(&path, host(1));
    let store = Store::open(config.clone()).expect("open");
    assert!(matches!(
        Store::open(config.clone()),
        Err(StoreError::AlreadyOwned { .. })
    ));
    store.shutdown().await.expect("shutdown");
    let reopened = Store::open(config).expect("reopen");
    reopened.shutdown().await.expect("shutdown reopened");
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
        reservation.open(StoreConfig::new(&path, host(1)).with_queue_capacity(0)),
        Err(StoreError::InvalidConfiguration(_))
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

#[tokio::test]
async fn queue_byte_budget_rejects_before_enqueue() {
    let directory = tempfile::tempdir().expect("tempdir");
    let config =
        StoreConfig::new(directory.path().join("store.sqlite"), host(5)).with_queue_bytes(32);
    let store = Store::open(config).expect("open");
    let (reply, _response) = oneshot::channel();
    let result = store
        .handle()
        .send(
            Command::LoadExecution {
                execution_id: ExecId([1; 32]),
                reply,
            },
            33,
        )
        .await;
    assert!(matches!(result, Err(StoreError::CommandTooLarge { .. })));
    store.shutdown().await.expect("shutdown");
}

#[test]
fn default_queue_budget_covers_maximal_program_command() {
    let directory = tempfile::tempdir().expect("tempdir");
    let config = StoreConfig::new(directory.path().join("store.sqlite"), host(5));
    let maximal_program = usize::try_from(arena0_program::PROGRAM_MAX_LEN).expect("usize");
    let required = maximal_program + 512;
    assert!(config.queue_bytes >= required);
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
    let store = Store::open(
        StoreConfig::new(directory.path().join("store.sqlite"), fixture.producer)
            .with_queue_bytes(16 * 1024 * 1024),
    )
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
    assert!(matches!(
        writer
            .activate(ExecutionVersion::ZERO, 7)
            .await
            .expect("activate"),
        ApplyOutcome::Committed { .. }
    ));
    assert!(writer.activate(ExecutionVersion::new(1), 8).await.is_err());
    let state = store
        .handle()
        .load_execution(id)
        .await
        .expect("load active")
        .expect("active state");
    let callout = Effect::Callout {
        callout_index: 0,
        context: vec![0xaa],
        expected_type: None,
    };
    assert!(matches!(
        writer
            .commit_dispatch(
                state.version(),
                session_started_event(&fixture),
                SharedStateBytes::try_new(vec![0]).expect("shared state"),
                LocalStateBytes::try_new(Vec::new()).expect("local state"),
                vec![
                    Effect::SetTimer {
                        delay_ms: 10,
                        timer: None,
                    },
                    callout,
                ],
                None,
                None,
                None,
                None,
                9,
            )
            .await
            .expect("arm timer"),
        ApplyOutcome::Committed {
            proposal_staged: true,
            ..
        }
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
    let mut writer = reopened
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
    for _ in 0..1 {
        let signature_frame = writer
            .lease_next_outbox(11)
            .await
            .expect("lease signature frame")
            .expect("signature frame");
        assert_eq!(signature_frame.item.event_position, 0);
        assert_eq!(signature_frame.item.ordinal, 0);
        assert_eq!(signature_frame.item.destination, Some(other_peer(&fixture)));
        assert_eq!(signature_frame.item.payload_kind, OutboxPayloadKind::Frame);
        assert!(matches!(
            borsh::from_slice::<ExecFrame>(&signature_frame.item.payload)
                .expect("signature frame payload"),
            ExecFrame::StepSignature { .. }
        ));
        assert_eq!(
            writer
                .acknowledge_outbox(signature_frame.item.outbox_id, signature_frame.lease_id)
                .await
                .expect("ack signature frame"),
            OutboxDeliveryOutcome::Acknowledged
        );
    }
    let first = writer
        .lease_next_outbox(11)
        .await
        .expect("lease callout")
        .expect("callout item");
    assert_eq!(first.item.event_position, 0);
    assert_eq!(first.item.ordinal, 1);
    assert_eq!(first.item.payload_kind, OutboxPayloadKind::Effect);
    assert!(matches!(
        borsh::from_slice::<Effect>(&first.item.payload).expect("callout effect"),
        Effect::Callout {
            callout_index: 0,
            context,
            ..
        } if context == [0xaa]
    ));
    assert_eq!(
        writer
            .acknowledge_outbox(first.item.outbox_id, first.lease_id)
            .await
            .expect("ack callout"),
        OutboxDeliveryOutcome::Acknowledged
    );
    assert!(
        writer
            .lease_next_outbox(11)
            .await
            .expect("no remaining outbox")
            .is_none()
    );
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
        writer
            .commit_dispatch(
                state.version(),
                session_started_event(&fixture),
                SharedStateBytes::try_new(vec![0]).expect("shared state"),
                LocalStateBytes::try_new(Vec::new()).expect("local state"),
                vec![
                    Effect::Callout {
                        callout_index: 0,
                        context: vec![3],
                        expected_type: None,
                    },
                    Effect::Broadcast { data: vec![7, 8] },
                ],
                None,
                None,
                None,
                None,
                7,
            )
            .await
            .expect("stage dispatch"),
        ApplyOutcome::Committed {
            proposal_staged: true,
            ..
        }
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

    // The establishing frame is pending before delivery and leased while a
    // worker owns it. Both states must keep terminal observation deferred.
    assert!(
        writer
            .has_unsettled_frames()
            .await
            .expect("pending frame is unsettled")
    );
    let probe = writer
        .lease_next_outbox(11)
        .await
        .expect("lease frame probe")
        .expect("frame probe");
    assert_eq!(probe.item.payload_kind, OutboxPayloadKind::Frame);
    assert!(
        writer
            .has_unsettled_frames()
            .await
            .expect("leased frame is unsettled")
    );
    assert_eq!(
        writer
            .acknowledge_outbox(probe.item.outbox_id, probe.lease_id)
            .await
            .expect("ack frame probe"),
        OutboxDeliveryOutcome::Acknowledged
    );

    let after_successor = sign_step(&store, &fixture, execution_id, &mut writer, 10, 11).await;
    assert_eq!(after_successor.event_position(), 1);
    assert_eq!(after_successor.agreed_step(), 2);
    assert!(after_successor.pending_shared().is_none());

    let mut signatures = 0;
    let mut callout = false;
    let mut broadcast = false;
    for _ in 0..3 {
        let leased = writer
            .lease_next_outbox(11)
            .await
            .expect("lease outbox")
            .expect("outbox row");
        assert_eq!(leased.item.event_position, 0);
        match leased.item.payload_kind {
            OutboxPayloadKind::Effect => {
                assert_eq!(leased.item.destination, None);
                assert_eq!(leased.item.ordinal, 0);
                assert!(matches!(
                    borsh::from_slice::<Effect>(&leased.item.payload).expect("callout payload"),
                    Effect::Callout {
                        callout_index: 0,
                        context,
                        ..
                    } if context == [3]
                ));
                callout = true;
            }
            OutboxPayloadKind::Frame => {
                assert_eq!(leased.item.destination, Some(other_peer(&fixture)));
                match borsh::from_slice::<ExecFrame>(&leased.item.payload).expect("frame payload") {
                    ExecFrame::StepSignature { .. } => {
                        assert_eq!(leased.item.ordinal, 0);
                        signatures += 1;
                    }
                    ExecFrame::Message {
                        seq,
                        prestate,
                        poststate,
                        data,
                        ..
                    } => {
                        assert_eq!(leased.item.ordinal, 2);
                        assert_eq!(seq, 1);
                        assert_eq!(prestate, after_first.agreed_state());
                        assert_eq!(poststate, after_first.agreed_state());
                        assert_eq!(data, vec![7, 8]);
                        broadcast = true;
                    }
                    ExecFrame::End { .. } | ExecFrame::Abort { .. } => {
                        panic!("unexpected terminal frame")
                    }
                }
            }
        }
        assert_eq!(
            writer
                .acknowledge_outbox(leased.item.outbox_id, leased.lease_id)
                .await
                .expect("ack outbox"),
            OutboxDeliveryOutcome::Acknowledged
        );
    }
    assert_eq!(signatures, 1);
    assert!(callout);
    assert!(broadcast);
    assert!(
        writer
            .lease_next_outbox(11)
            .await
            .expect("no remaining outbox")
            .is_none()
    );
    assert!(
        !writer
            .has_unsettled_frames()
            .await
            .expect("all frames acknowledged")
    );
    let page = store
        .handle()
        .read_event_summaries(execution_id, Some(0), 1)
        .await
        .expect("inspect deferred event");
    assert_eq!(page.total(), 1);
    assert_eq!(page.summaries()[0].agreed_steps, vec![0, 1]);
    drop(writer);
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn pending_proposal_leases_frames_but_withholds_local_effects() {
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
    writer
        .commit_dispatch(
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
    let initial_signature = writer
        .lease_next_outbox(10)
        .await
        .expect("lease initial signature")
        .expect("initial signature frame");
    assert!(matches!(
        borsh::from_slice::<ExecFrame>(&initial_signature.item.payload).expect("initial frame"),
        ExecFrame::StepSignature { .. }
    ));
    writer
        .acknowledge_outbox(initial_signature.item.outbox_id, initial_signature.lease_id)
        .await
        .expect("ack initial signature");

    let callout = Effect::Callout {
        callout_index: 0,
        context: vec![0x51],
        expected_type: None,
    };
    let broadcast = vec![0x61, 0x62];
    writer
        .commit_dispatch(
            after_start.version(),
            Event::React,
            SharedStateBytes::try_new(vec![0]).expect("shared state"),
            LocalStateBytes::try_new(Vec::new()).expect("local state"),
            vec![
                callout,
                Effect::Broadcast {
                    data: broadcast.clone(),
                },
            ],
            None,
            None,
            None,
            None,
            10,
        )
        .await
        .expect("stage broadcast proposal");

    let establishing = writer
        .lease_next_outbox(11)
        .await
        .expect("lease establishing frame")
        .expect("establishing frame");
    assert_eq!(establishing.item.payload_kind, OutboxPayloadKind::Frame);
    assert_eq!(establishing.item.destination, Some(other_peer(&fixture)));
    assert_eq!(
        establishing.item.event_position,
        after_start.event_position()
    );
    assert_eq!(establishing.item.ordinal, 2);
    assert!(matches!(
        borsh::from_slice::<ExecFrame>(&establishing.item.payload).expect("frame payload"),
        ExecFrame::Message { data, .. } if data == broadcast
    ));
    writer
        .acknowledge_outbox(establishing.item.outbox_id, establishing.lease_id)
        .await
        .expect("ack establishing frame");
    assert!(
        writer
            .lease_next_outbox(11)
            .await
            .expect("withhold local effect while proposal is pending")
            .is_none()
    );

    let committed = sign_step(&store, &fixture, execution_id, &mut writer, 12, 13).await;
    assert!(committed.pending_shared().is_none());
    assert_eq!(
        committed.status().pending().expect("callout pending").id,
        arena0_protocol::pending_id(execution_id, after_start.event_position(), 0)
    );

    let mut found_callout = false;
    for _ in 0..2 {
        let Some(leased) = writer
            .lease_next_outbox(13)
            .await
            .expect("lease committed outbox")
        else {
            break;
        };
        if leased.item.payload_kind == OutboxPayloadKind::Effect {
            assert_eq!(leased.item.event_position, after_start.event_position());
            assert_eq!(leased.item.ordinal, 0);
            assert!(matches!(
                borsh::from_slice::<Effect>(&leased.item.payload).expect("callout payload"),
                Effect::Callout { context, .. } if context == vec![0x51]
            ));
            found_callout = true;
        }
        writer
            .acknowledge_outbox(leased.item.outbox_id, leased.lease_id)
            .await
            .expect("ack committed outbox");
    }
    assert!(found_callout, "callout becomes leasable after agreement");
    drop(writer);
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn interrupt_terminal_freezes_proof_and_cancels_timers_after_restart() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0xf4; 32]);
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
    writer
        .commit_dispatch(
            state.version(),
            session_started_event(&fixture),
            SharedStateBytes::try_new(vec![0]).expect("shared state"),
            LocalStateBytes::try_new(Vec::new()).expect("local state"),
            vec![Effect::SetTimer {
                delay_ms: 100,
                timer: None,
            }],
            None,
            None,
            None,
            None,
            7,
        )
        .await
        .expect("stage timer event");
    let after_start = sign_step(&store, &fixture, execution_id, &mut writer, 8, 9).await;
    assert_eq!(
        writer.due_timers(200, 8).await.expect("active timer").len(),
        1
    );

    let position = after_start.agreed_step();
    let pre_state = after_start.agreed_state();
    let data = vec![0x71, 0x72];
    let message_id = MessageId::derive(
        after_start.binding().session_id(),
        fixture.producer,
        position,
        pre_state,
        pre_state,
        &data,
    );
    let outcome = vec![0x81, 0x82];
    writer
        .commit_dispatch(
            after_start.version(),
            Event::MessageReceived {
                message_id,
                from: fixture.producer,
                position,
                pre_state,
                msg: data,
            },
            SharedStateBytes::try_new(vec![0]).expect("shared state"),
            LocalStateBytes::try_new(Vec::new()).expect("local state"),
            vec![Effect::SessionEnd {
                outcome: outcome.clone(),
            }],
            Some(TerminalOutcome::new(outcome, br#"null"#.to_vec()).expect("outcome")),
            None,
            None,
            None,
            10,
        )
        .await
        .expect("stage terminal proof");
    let terminal = sign_step(&store, &fixture, execution_id, &mut writer, 11, 12).await;
    assert!(terminal.terminal_pending());
    let commitment = terminal
        .pending_terminal()
        .expect("pending terminal commitment")
        .clone();
    assert_eq!(
        writer
            .due_timers(200, 8)
            .await
            .expect("timer before interrupt")
            .len(),
        1
    );

    assert!(matches!(
        writer
            .interrupt_terminal(terminal.version(), "operator interrupted", 13)
            .await
            .expect("interrupt terminal"),
        ApplyOutcome::Committed {
            proposal_staged: false,
            ..
        }
    ));
    let interrupted = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load interrupted state")
        .expect("interrupted state");
    assert_eq!(interrupted.lifecycle(), ExecLifecycle::Incomplete);
    assert_eq!(interrupted.pending_terminal(), Some(&commitment));
    assert!(
        writer
            .due_timers(200, 8)
            .await
            .expect("timers after interrupt")
            .is_empty()
    );
    drop(writer);
    store.shutdown().await.expect("shutdown before restart");

    let reopened = Store::open(StoreConfig::new(&path, fixture.producer)).expect("reopen");
    let recovered = reopened
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load recovered interrupted state")
        .expect("recovered interrupted state");
    assert_eq!(recovered.lifecycle(), ExecLifecycle::Incomplete);
    assert_eq!(recovered.pending_terminal(), Some(&commitment));
    let recovered_writer = reopened
        .handle()
        .claim_execution(execution_id)
        .expect("recovered execution writer");
    assert!(
        recovered_writer
            .due_timers(200, 8)
            .await
            .expect("timers after restart")
            .is_empty()
    );
    drop(recovered_writer);
    reopened.shutdown().await.expect("shutdown after restart");
}

#[tokio::test]
async fn stopping_an_unsigned_proposal_cancels_exact_frames_and_recovers() {
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
    writer
        .commit_dispatch(
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
    let start_frame = writer
        .lease_next_outbox(10)
        .await
        .expect("lease producer signature")
        .expect("producer signature frame");
    assert!(matches!(
        borsh::from_slice::<ExecFrame>(&start_frame.item.payload).expect("step frame"),
        ExecFrame::StepSignature { .. }
    ));
    assert_eq!(
        writer
            .acknowledge_outbox(start_frame.item.outbox_id, start_frame.lease_id)
            .await
            .expect("ack producer signature"),
        OutboxDeliveryOutcome::Acknowledged
    );

    let data = vec![0xa1, 0xa2];
    assert!(matches!(
        writer
            .commit_dispatch(
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
        ApplyOutcome::Committed {
            proposal_staged: true,
            ..
        }
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
    let leased = writer
        .lease_next_outbox(12)
        .await
        .expect("lease establishing message")
        .expect("establishing message frame");
    assert_eq!(leased.item.payload_kind, OutboxPayloadKind::Frame);
    assert_eq!(leased.item.destination, Some(other_peer(&fixture)));
    assert_eq!(leased.item.ordinal, 1);
    assert!(matches!(
        borsh::from_slice::<ExecFrame>(&leased.item.payload).expect("message frame"),
        ExecFrame::Message { data: found, .. } if found == data
    ));
    assert!(
        writer
            .has_unsettled_frames()
            .await
            .expect("leased proposal frame is unsettled")
    );

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
    writer
        .stop_execution(proposed.version(), occurrence, None, 13)
        .await
        .expect("stop unsigned proposal");

    let stopped = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load stopped state")
        .expect("stopped state");
    assert!(stopped.pending_shared().is_none());
    assert_eq!(
        writer
            .acknowledge_outbox(leased.item.outbox_id, leased.lease_id)
            .await
            .expect("late ack is idempotent"),
        OutboxDeliveryOutcome::AlreadyCancelled
    );
    assert_eq!(
        writer
            .retry_outbox(leased.item.outbox_id, leased.lease_id, 14, "late retry")
            .await
            .expect("late retry is idempotent"),
        OutboxDeliveryOutcome::AlreadyCancelled
    );
    drop(writer);
    store.shutdown().await.expect("shutdown");

    let connection = Connection::open(&path).expect("inspect database");
    let (status, lease_id, lease_until, payload): (String, Option<Vec<u8>>, Option<i64>, Vec<u8>) =
        connection
            .query_row(
                "SELECT status, lease_id, lease_until_ms, payload
                 FROM outbox WHERE outbox_id = ?1",
                rusqlite::params![leased.item.outbox_id.as_bytes().to_vec()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("cancelled frame");
    assert_eq!(status, "cancelled");
    assert!(lease_id.is_none());
    assert!(lease_until.is_none());
    assert!(matches!(
        borsh::from_slice::<ExecFrame>(&payload).expect("cancelled payload"),
        ExecFrame::Message { .. }
    ));
    drop(connection);

    let reopened = Store::open(StoreConfig::new(&path, fixture.producer)).expect("reopen");
    let recovered = reopened
        .handle()
        .load_execution(execution_id)
        .await
        .expect("recover stopped state")
        .expect("recovered state");
    assert!(recovered.pending_shared().is_none());
    let mut writer = reopened
        .handle()
        .claim_execution(execution_id)
        .expect("recovered writer");
    let abort = writer
        .lease_next_outbox(15)
        .await
        .expect("lease stop frame")
        .expect("stop frame remains deliverable");
    assert_ne!(abort.item.outbox_id, leased.item.outbox_id);
    assert!(matches!(
        borsh::from_slice::<ExecFrame>(&abort.item.payload).expect("stop frame payload"),
        ExecFrame::Abort { .. }
    ));
    writer
        .acknowledge_outbox(abort.item.outbox_id, abort.lease_id)
        .await
        .expect("ack stop frame");
    assert!(
        !writer
            .has_unsettled_frames()
            .await
            .expect("cancelled and acknowledged frames are settled")
    );
    assert!(
        writer
            .lease_next_outbox(15)
            .await
            .expect("cancelled frame is not leasable")
            .is_none()
    );
    drop(writer);
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
        writer
            .commit_dispatch(
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
        ApplyOutcome::Committed {
            proposal_staged: true,
            ..
        }
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
        writer
            .commit_dispatch(
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
        ApplyOutcome::Committed {
            proposal_staged: true,
            ..
        }
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
async fn unrelated_event_can_retry_an_existing_callout() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0xf2; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    let callout = Effect::Callout {
        callout_index: 0,
        context: vec![0x31],
        expected_type: None,
    };
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load active")
        .expect("active state");
    writer
        .commit_dispatch(
            state.version(),
            session_started_event(&fixture),
            SharedStateBytes::try_new(vec![0]).expect("shared state"),
            LocalStateBytes::try_new(Vec::new()).expect("local state"),
            vec![callout],
            None,
            None,
            None,
            None,
            7,
        )
        .await
        .expect("stage callout");
    let after_start = sign_step(&store, &fixture, execution_id, &mut writer, 8, 9).await;
    let pending = after_start.status().pending().expect("callout pending");
    let pending_id = pending.id;
    assert_eq!(pending_id, arena0_protocol::pending_id(execution_id, 0, 0));

    writer
        .commit_dispatch(
            after_start.version(),
            Event::React,
            SharedStateBytes::try_new(vec![0]).expect("shared state"),
            LocalStateBytes::try_new(Vec::new()).expect("local state"),
            vec![Effect::RetryInput {
                reason: "transient input fault".into(),
            }],
            None,
            None,
            None,
            None,
            10,
        )
        .await
        .expect("retry from unrelated event");
    let retried = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load retried state")
        .expect("retried state");
    assert_eq!(retried.event_position(), 2);
    assert_eq!(
        retried.status().pending().expect("retained callout").id,
        pending_id
    );
    let requests = store
        .handle()
        .list_pending_requests(execution_id)
        .await
        .expect("list retained callout");
    assert!(matches!(
        &requests[..],
        [PendingRequest::Callout { pending_id: found, .. }] if *found == pending_id
    ));

    // The originating request must be acknowledged before its later retry
    // marker can reach the causal head of the local lane. Leave that retry
    // leased, then consume the continuation; the answer transaction retires
    // both the exact request and every RetryInput marker for it.
    let originating = loop {
        let candidate = writer
            .lease_next_outbox(11)
            .await
            .expect("lease originating callout")
            .expect("originating callout");
        if candidate.item.payload_kind == OutboxPayloadKind::Frame {
            writer
                .acknowledge_outbox(candidate.item.outbox_id, candidate.lease_id)
                .await
                .expect("ack preceding frame");
            continue;
        }
        break candidate;
    };
    assert!(matches!(
        borsh::from_slice::<Effect>(&originating.item.payload).expect("originating payload"),
        Effect::Callout { .. }
    ));
    assert_eq!(
        writer
            .acknowledge_outbox(originating.item.outbox_id, originating.lease_id)
            .await
            .expect("ack originating callout"),
        OutboxDeliveryOutcome::Acknowledged
    );
    let retry = writer
        .lease_next_outbox(11)
        .await
        .expect("lease retry marker")
        .expect("retry marker");
    assert!(matches!(
        borsh::from_slice::<Effect>(&retry.item.payload).expect("retry payload"),
        Effect::RetryInput { .. }
    ));
    let answered = writer
        .commit_dispatch(
            retried.version(),
            Event::InputReceived {
                callout_index: 0,
                data: vec![0x32],
            },
            SharedStateBytes::try_new(vec![0]).expect("answer shared state"),
            LocalStateBytes::try_new(Vec::new()).expect("answer local state"),
            Vec::new(),
            None,
            None,
            None,
            Some(pending_id),
            12,
        )
        .await
        .expect("consume callout answer");
    assert!(matches!(
        answered,
        ApplyOutcome::Committed {
            proposal_staged: false,
            ..
        }
    ));
    let answered_state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load answered state")
        .expect("answered state");
    assert!(answered_state.status().pending().is_none());
    assert_eq!(
        writer
            .acknowledge_outbox(retry.item.outbox_id, retry.lease_id)
            .await
            .expect("late retry acknowledgement"),
        OutboxDeliveryOutcome::AlreadyCancelled
    );
    assert!(
        writer
            .lease_next_outbox(12)
            .await
            .expect("no continuation effects remain")
            .is_none()
    );
    drop(writer);
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn terminal_agreement_retires_stale_continuation_effects() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0xf6; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    let callout = Effect::Callout {
        callout_index: 0,
        context: vec![0x41],
        expected_type: None,
    };
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load active")
        .expect("active state");
    writer
        .commit_dispatch(
            state.version(),
            session_started_event(&fixture),
            SharedStateBytes::try_new(vec![0]).expect("shared state"),
            LocalStateBytes::try_new(Vec::new()).expect("local state"),
            vec![callout],
            None,
            None,
            None,
            None,
            7,
        )
        .await
        .expect("stage callout");
    let waiting = sign_step(&store, &fixture, execution_id, &mut writer, 8, 9).await;

    // Leave the originating request leased while unrelated execution creates
    // a retry marker. The terminal agreement below must retire both rows.
    let leased_callout = loop {
        let candidate = writer
            .lease_next_outbox(10)
            .await
            .expect("lease callout")
            .expect("callout row");
        if candidate.item.payload_kind == OutboxPayloadKind::Frame {
            writer
                .acknowledge_outbox(candidate.item.outbox_id, candidate.lease_id)
                .await
                .expect("ack preceding frame");
            continue;
        }
        break candidate;
    };
    assert!(matches!(
        borsh::from_slice::<Effect>(&leased_callout.item.payload).expect("callout payload"),
        Effect::Callout { .. }
    ));
    writer
        .commit_dispatch(
            waiting.version(),
            Event::React,
            SharedStateBytes::try_new(vec![0]).expect("retry shared state"),
            LocalStateBytes::try_new(Vec::new()).expect("retry local state"),
            vec![Effect::RetryInput {
                reason: "temporary delivery fault".into(),
            }],
            None,
            None,
            None,
            None,
            11,
        )
        .await
        .expect("persist retry marker");
    let after_retry = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load retry state")
        .expect("retry state");

    let data = vec![0x51, 0x52];
    let position = after_retry.agreed_step();
    let pre_state = after_retry.agreed_state();
    let message_id = MessageId::derive(
        after_retry.binding().session_id(),
        fixture.producer,
        position,
        pre_state,
        pre_state,
        &data,
    );
    let outcome = vec![0x61, 0x62];
    writer
        .commit_dispatch(
            after_retry.version(),
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

    drop(writer);
    store.shutdown().await.expect("shutdown");
    let rows = local_continuation_rows(&path, execution_id);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|(status, _)| status == "cancelled"));
    assert!(
        rows.iter()
            .any(|(_, effect)| matches!(effect, Effect::Callout { .. }))
    );
    assert!(
        rows.iter()
            .any(|(_, effect)| matches!(effect, Effect::RetryInput { .. }))
    );
}

#[tokio::test]
async fn authenticated_stop_retires_stale_continuation_effects() {
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
    writer
        .commit_dispatch(
            state.version(),
            session_started_event(&fixture),
            SharedStateBytes::try_new(vec![0]).expect("shared state"),
            LocalStateBytes::try_new(Vec::new()).expect("local state"),
            vec![Effect::Callout {
                callout_index: 0,
                context: vec![0x71],
                expected_type: None,
            }],
            None,
            None,
            None,
            None,
            7,
        )
        .await
        .expect("stage callout");
    let waiting = sign_step(&store, &fixture, execution_id, &mut writer, 8, 9).await;
    let leased_callout = loop {
        let candidate = writer
            .lease_next_outbox(10)
            .await
            .expect("lease callout")
            .expect("callout row");
        if candidate.item.payload_kind == OutboxPayloadKind::Frame {
            writer
                .acknowledge_outbox(candidate.item.outbox_id, candidate.lease_id)
                .await
                .expect("ack preceding frame");
            continue;
        }
        break candidate;
    };
    assert!(matches!(
        borsh::from_slice::<Effect>(&leased_callout.item.payload).expect("callout payload"),
        Effect::Callout { .. }
    ));
    writer
        .commit_dispatch(
            waiting.version(),
            Event::React,
            SharedStateBytes::try_new(vec![0]).expect("retry shared state"),
            LocalStateBytes::try_new(Vec::new()).expect("retry local state"),
            vec![Effect::RetryInput {
                reason: "temporary delivery fault".into(),
            }],
            None,
            None,
            None,
            None,
            11,
        )
        .await
        .expect("persist retry marker");
    let after_retry = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load retry state")
        .expect("retry state");
    let unsigned = AbortOccurrence::unsigned(
        fixture.activation.session_hash(),
        fixture.producer,
        AbortKind::Abort,
        94,
        "stop stale continuation",
        after_retry.step_cursor(),
    )
    .expect("abort occurrence");
    let keys = NodeKeys::from_secret(SecretKey::from_bytes([1; 32]));
    let occurrence = unsigned
        .clone()
        .with_signature(keys.sign(&unsigned.signing_bytes().expect("abort bytes")))
        .expect("signed abort");
    writer
        .stop_execution(after_retry.version(), occurrence, None, 12)
        .await
        .expect("stop execution");
    let stopped = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load stopped state")
        .expect("stopped state");
    assert!(stopped.status().terminal_cause().is_some());

    drop(writer);
    store.shutdown().await.expect("shutdown");
    let rows = local_continuation_rows(&path, execution_id);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|(status, _)| status == "cancelled"));
    assert!(
        rows.iter()
            .any(|(_, effect)| matches!(effect, Effect::Callout { .. }))
    );
    assert!(
        rows.iter()
            .any(|(_, effect)| matches!(effect, Effect::RetryInput { .. }))
    );
}

#[tokio::test]
async fn consumed_pending_request_is_hidden_while_dispatch_proposal_is_staged() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0xf5; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");

    let callout = Effect::Callout {
        callout_index: 0,
        context: vec![0x91],
        expected_type: None,
    };
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load active")
        .expect("active state");
    writer
        .commit_dispatch(
            state.version(),
            session_started_event(&fixture),
            SharedStateBytes::try_new(vec![0]).expect("shared state"),
            LocalStateBytes::try_new(Vec::new()).expect("local state"),
            vec![callout],
            None,
            None,
            None,
            None,
            7,
        )
        .await
        .expect("stage callout");
    let waiting = sign_step(&store, &fixture, execution_id, &mut writer, 8, 9).await;
    let pending_id = waiting.status().pending().expect("pending callout").id;

    writer
        .commit_dispatch(
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
            None,
            Some(pending_id),
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
            .status()
            .pending()
            .is_none()
    );
    assert!(
        store
            .handle()
            .list_pending_requests(execution_id)
            .await
            .expect("list pending requests")
            .is_empty()
    );

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

    let callout = Effect::Callout {
        callout_index: 0,
        context: vec![3, 4],
        expected_type: Some("u8".into()),
    };
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load active")
        .expect("active state");
    writer
        .commit_dispatch(
            state.version(),
            session_started_event(&fixture),
            SharedStateBytes::try_new(vec![0]).expect("shared state"),
            LocalStateBytes::try_new(Vec::new()).expect("local state"),
            vec![callout],
            None,
            None,
            None,
            None,
            20,
        )
        .await
        .expect("stage callout");
    sign_step(&store, &fixture, execution_id, &mut writer, 21, 22).await;
    let pending_id = arena0_protocol::pending_id(execution_id, 0, 0);

    let requests = store
        .handle()
        .list_pending_requests(execution_id)
        .await
        .expect("pending callout request");
    assert_eq!(requests.len(), 1);
    assert!(matches!(
        &requests[0],
        PendingRequest::Callout {
            pending_id: found,
            callout_index: 0,
            context,
            expected_type: Some(expected),
            ..
        } if *found == pending_id && context == &vec![3, 4] && expected == "u8"
    ));

    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load waiting state")
        .expect("waiting state");
    writer
        .commit_dispatch(
            state.version(),
            Event::InputReceived {
                callout_index: 0,
                data: vec![0xff],
            },
            SharedStateBytes::try_new(vec![0]).expect("shared state"),
            LocalStateBytes::try_new(vec![1]).expect("local state"),
            vec![Effect::RetryInput {
                reason: "try again".into(),
            }],
            None,
            None,
            None,
            Some(pending_id),
            23,
        )
        .await
        .expect("retry callout");
    assert_eq!(
        store
            .handle()
            .list_pending_requests(execution_id)
            .await
            .expect("retained pending request")
            .len(),
        1
    );

    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load retried state")
        .expect("retried state");
    writer
        .commit_dispatch(
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
            None,
            Some(pending_id),
            24,
        )
        .await
        .expect("consume callout");

    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load active after callout")
        .expect("active state after callout");
    writer
        .commit_dispatch(
            state.version(),
            Event::React,
            SharedStateBytes::try_new(vec![0]).expect("shared state"),
            LocalStateBytes::try_new(vec![3]).expect("local state"),
            vec![Effect::Sign {
                scheme: arena0_crypto::SignScheme::Ed25519,
                data: vec![5, 6],
                expected_type: Some("bytes".into()),
            }],
            None,
            None,
            None,
            None,
            25,
        )
        .await
        .expect("stage signing request");
    let sign_effect_id = arena0_protocol::pending_id(execution_id, 3, 0);
    let requests = store
        .handle()
        .list_pending_requests(execution_id)
        .await
        .expect("pending signing request");
    assert!(matches!(
        &requests[..],
        [PendingRequest::Signature {
            pending_id,
            data,
            ..
        }] if *pending_id == sign_effect_id
            && data.execution_id() == execution_id
            && data.event_position() == 3
            && data.effect_index() == 0
            && data.payload() == [5, 6]
    ));
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load signing state")
        .expect("signing state");
    writer
        .commit_dispatch(
            state.version(),
            Event::Signed {
                signature: vec![0xaa],
            },
            SharedStateBytes::try_new(vec![0]).expect("shared state"),
            LocalStateBytes::try_new(vec![4]).expect("local state"),
            Vec::new(),
            None,
            None,
            None,
            Some(sign_effect_id),
            26,
        )
        .await
        .expect("consume signing request");
    let final_state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load final state")
        .expect("final state");
    assert_eq!(final_state.shared_state().as_bytes(), &[0]);
    assert_eq!(final_state.local_state().as_bytes(), &[4]);
    drop(writer);

    let page = store
        .handle()
        .read_event_summaries(execution_id, Some(0), 8)
        .await
        .expect("event inspection page");
    assert_eq!(page.total(), 5);
    assert_eq!(page.next(), None);
    assert_eq!(page.summaries().len(), 5);
    assert_eq!(page.summaries()[0].event_position, 0);
    assert_eq!(page.summaries()[0].agreed_steps, vec![0]);
    assert_eq!(page.summaries()[0].event, EventKind::SessionStarted);
    assert_eq!(page.summaries()[0].input_payload_bytes, None);
    assert_eq!(
        page.summaries()[0].effects[0],
        EffectSummary {
            kind: EffectKind::Callout,
            payload_bytes: Some(2),
        }
    );
    assert_eq!(page.summaries()[1].event_position, 1);
    assert_eq!(page.summaries()[1].agreed_steps, Vec::<u64>::new());
    assert_eq!(page.summaries()[1].event, EventKind::InputReceived);
    assert_eq!(page.summaries()[1].input_payload_bytes, Some(1));
    assert_eq!(
        page.summaries()[1].effects[0],
        EffectSummary {
            kind: EffectKind::RetryInput,
            payload_bytes: Some(9),
        }
    );
    assert_eq!(page.summaries()[2].event_position, 2);
    assert_eq!(page.summaries()[2].event, EventKind::InputReceived);
    assert!(page.summaries()[2].effects.is_empty());
    assert_eq!(page.summaries()[3].event_position, 3);
    assert_eq!(page.summaries()[3].event, EventKind::React);
    assert_eq!(
        page.summaries()[3].effects[0],
        EffectSummary {
            kind: EffectKind::Sign,
            payload_bytes: Some(2),
        }
    );
    assert_eq!(page.summaries()[4].event_position, 4);
    assert_eq!(page.summaries()[4].event, EventKind::Signed);
    assert!(page.summaries()[4].effects.is_empty());
    assert!(
        store
            .handle()
            .list_pending_requests(execution_id)
            .await
            .expect("no pending request after consumption")
            .is_empty()
    );
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn accepted_inbound_signature_survives_restart_and_applies_once() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0x9a; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;

    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load active execution")
        .expect("active execution");
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    let (event, shared, local, effects, terminal_outcome) = terminal_dispatch(&fixture);
    writer
        .commit_dispatch(
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
        .expect("stage shared proposal");
    let proposed = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load proposal")
        .expect("proposed execution");
    let commitment = proposed
        .pending_shared()
        .expect("pending shared proposal")
        .commitment()
        .clone();
    let source = other_peer(&fixture);
    let signature = BlsSecretKey::from_seed(&[12; 32])
        .expect("peer key")
        .sign(&commitment.signing_bytes());
    let frame = ExecFrame::StepSignature {
        commitment: commitment.clone(),
        signature,
    };

    assert_eq!(
        writer
            .accept_inbound(source, frame.clone(), 8)
            .await
            .expect("accept inbound"),
        InboxAcceptOutcome::Accepted
    );
    let pending = store
        .handle()
        .list_pending_inbox(execution_id, 1)
        .await
        .expect("list pending inbox");
    assert_eq!(pending.len(), 1);
    let inbox_id = pending[0].inbox_id();
    assert_eq!(pending[0].source(), source);
    assert_eq!(pending[0].frame(), &frame);
    drop(writer);
    store.shutdown().await.expect("shutdown");

    let reopened = Store::open(StoreConfig::new(&path, fixture.producer)).expect("reopen");
    let mut writer = reopened
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    assert_eq!(
        writer
            .accept_inbound(source, frame.clone(), 9)
            .await
            .expect("redeliver accepted frame"),
        InboxAcceptOutcome::AlreadyAccepted
    );
    assert!(matches!(
        writer
            .commit_step_signature(
                proposed.version(),
                ParticipantStepSignature::new(source, commitment.step, signature),
                Some(inbox_id),
                10,
            )
            .await
            .expect("apply accepted signature"),
        ApplyOutcome::Committed { .. }
    ));
    assert!(
        writer
            .lease_next_outbox(10)
            .await
            .expect("no rebroadcast for accepted signature")
            .is_none()
    );
    assert_eq!(
        writer
            .accept_inbound(source, frame, 11)
            .await
            .expect("redeliver applied frame"),
        InboxAcceptOutcome::AlreadyConsumed
    );
    assert!(
        reopened
            .handle()
            .list_pending_inbox(execution_id, 1)
            .await
            .expect("pending inbox after apply")
            .is_empty()
    );
    drop(writer);
    reopened.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn later_page_inbound_corruption_fails_closed_on_restart() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0x9c; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load execution")
        .expect("execution");
    let source = other_peer(&fixture);
    let seq = state.agreed_step();
    let prestate = state.agreed_state();
    let session_id = state.binding().session_id();
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    for byte in 0..65_u8 {
        let data = vec![byte];
        let frame = ExecFrame::Message {
            message_id: MessageId::derive(session_id, source, seq, prestate, prestate, &data),
            seq,
            prestate,
            poststate: prestate,
            data,
        };
        assert_eq!(
            writer
                .accept_inbound(source, frame, 8 + u64::from(byte))
                .await
                .expect("accept inbound frame"),
            InboxAcceptOutcome::Accepted
        );
    }
    drop(writer);
    store.shutdown().await.expect("shutdown before validation");

    let reopened = Store::open(StoreConfig::new(&path, fixture.producer))
        .expect("valid populated inbox rows reopen");
    reopened
        .shutdown()
        .await
        .expect("shutdown after validation");

    let connection = Connection::open(&path).expect("inspect");
    let inbox_id: Vec<u8> = connection
        .query_row(
            "SELECT inbox_id FROM inbox
             WHERE execution_id = ?1
             ORDER BY inbox_id LIMIT 1 OFFSET 64",
            params![execution_id.0.to_vec()],
            |row| row.get(0),
        )
        .expect("later-page inbox row");
    connection
        .execute(
            "UPDATE inbox SET frame = x'00' WHERE execution_id = ?1 AND inbox_id = ?2",
            params![execution_id.0.to_vec(), inbox_id],
        )
        .expect("tamper later-page frame");
    drop(connection);

    assert!(matches!(
        Store::open(StoreConfig::new(&path, fixture.producer)),
        Err(StoreError::Corruption(_))
    ));
}

#[tokio::test]
async fn any_accepted_inbound_fact_can_be_durably_rejected() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0x9b; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load execution")
        .expect("execution");
    let source = other_peer(&fixture);
    let data = vec![3, 2, 1];
    let frame = ExecFrame::Message {
        message_id: MessageId::derive(
            fixture.activation.session_hash(),
            source,
            state.agreed_step(),
            state.agreed_state(),
            state.agreed_state(),
            &data,
        ),
        seq: state.agreed_step(),
        prestate: state.agreed_state(),
        poststate: state.agreed_state(),
        data,
    };
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    assert_eq!(
        writer
            .accept_inbound(source, frame.clone(), 7)
            .await
            .expect("accept frame"),
        InboxAcceptOutcome::Accepted
    );
    let inbox_id = store
        .handle()
        .list_pending_inbox(execution_id, 1)
        .await
        .expect("pending inbox")[0]
        .inbox_id();
    assert_eq!(
        writer
            .reject_inbound(inbox_id, 8)
            .await
            .expect("reject frame"),
        InboxRejectOutcome::Rejected
    );
    assert!(
        store
            .handle()
            .list_pending_inbox(execution_id, 1)
            .await
            .expect("pending after rejection")
            .is_empty()
    );
    assert_eq!(
        writer
            .accept_inbound(source, frame, 9)
            .await
            .expect("redeliver rejected frame"),
        InboxAcceptOutcome::AlreadyConsumed
    );
    drop(writer);
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

    // The terminal certificate and all agreed trace rows are durable before
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
    assert!(matches!(
        writer
            .publish_terminal(state.version(), 12)
            .await
            .expect("publish"),
        ApplyOutcome::Committed { .. }
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
    let receipt = publish_receipt(&store, &fixture, execution_id).await;
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
    writer
        .stop_execution(state.version(), occurrence, None, 7)
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
    assert!(matches!(
        writer
            .publish_terminal(state.version(), 8)
            .await
            .expect("publish stopped"),
        ApplyOutcome::Committed { .. }
    ));
    drop(writer);
    store
        .shutdown()
        .await
        .expect("shutdown after stopped publication");

    let store = Store::open(StoreConfig::new(&path, fixture.producer)).expect("reopen published");
    let receipt = publish_receipt(&store, &fixture, execution_id).await;
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
    writer
        .stop_execution(state.version(), occurrence, None, 7)
        .await
        .expect("abort");
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load stopped state")
        .expect("stopped state");
    writer
        .publish_terminal(state.version(), 8)
        .await
        .expect("publish stopped");
    drop(writer);
    let published = publish_receipt(&store, &fixture, execution_id).await;
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
        publish_receipt(&store, &fixture, execution_id).await;
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
    let receipt = publish_receipt(&produced, &fixture, ExecId([0x41; 32])).await;
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
    let receipt = publish_receipt(&produced, &fixture, ExecId([0x42; 32])).await;
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
    let receipt = publish_receipt(&source, &fixture, ExecId([0x43; 32])).await;
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
    let published = publish_receipt(&target, &fixture, ExecId([0x44; 32])).await;
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
    let receipt = publish_receipt(&store, &fixture, execution_id).await;

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
