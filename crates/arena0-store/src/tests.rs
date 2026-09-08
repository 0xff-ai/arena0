use super::*;
use arena0_crypto::bls::BlsSecretKey;
use arena0_crypto::{BlsSignature, NodeKeys, SecretKey, key_binding_message};
use arena0_program::{
    JsonBytes, LocalStateBytes, MAX_LOCAL_STATE_BYTES, MAX_SHARED_STATE_BYTES, SharedStateBytes,
};
use arena0_protocol::execution::{
    AbortKind, AbortOccurrence, ExecutionInput, ExecutionState, ParticipantStepSignature,
    ParticipantTerminalSignature, PrivateCause, PrivateContext, PrivateDelta, TerminalOutcome,
};
use arena0_protocol::{
    Activation, ActivationData, AggregateAttestation, Ensemble, ExecutionAdmission, FrameId,
    NegotiationId, NegotiationTarget, Offer, OfferData, PreparedActivation, PrivateEffect,
    PrivateEvent, PrivateRecord, PublicEffect, PublicEvent, TRACE_FORMAT_VERSION, Ticket,
    TicketAction, TicketData,
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
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    writer
        .create_execution_request(
            program_hash,
            Some(JsonBytes::try_new(br#"{}"#.to_vec()).expect("params")),
            ExecutionAdmission::create(NegotiationId([0x11; 32]), 2).expect("admission"),
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
        .apply_input(ExecutionInput::Activate, 6)
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

fn terminal_delta(
    fixture: &ActivationFixture,
    state: &ExecutionState,
) -> arena0_protocol::SharedDelta {
    let outcome = vec![9, 8, 7];
    let ensemble = Ensemble::from_peers(
        fixture
            .prepared
            .tickets()
            .iter()
            .map(|ticket| ticket.data.signer)
            .collect(),
    )
    .expect("ensemble");
    let next_shared = SharedStateBytes::try_new(vec![1]).expect("next shared");
    let entry = arena0_protocol::TraceEntry {
        trace_version: TRACE_FORMAT_VERSION,
        step: 0,
        event: PublicEvent::SessionStarted { ensemble },
        effects: vec![PublicEffect::SessionEnd {
            outcome: outcome.clone(),
        }],
        pre_state: state.public().state_hash(),
        post_state: StateHash::of(next_shared.as_bytes()),
        fuel_used: 0,
        witness: None,
        agreement: AggregateAttestation::empty(),
    };
    arena0_protocol::SharedDelta::new(
        entry,
        next_shared,
        Some(TerminalOutcome::new(outcome, br#"null"#.to_vec()).expect("outcome")),
    )
    .expect("terminal delta")
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
    writer
        .apply_input(
            ExecutionInput::ProposeShared(terminal_delta(fixture, &state)),
            7,
        )
        .await
        .expect("proposal");
    let state = store
        .handle()
        .load_execution(execution_id)
        .await
        .expect("load proposal")
        .expect("proposal state");
    let commitment = state
        .pending_shared()
        .expect("pending shared")
        .commitment()
        .clone();
    let producer_bls = BlsSecretKey::from_seed(&[11; 32]).expect("producer bls");
    let other_bls = BlsSecretKey::from_seed(&[12; 32]).expect("other bls");
    writer
        .apply_input(
            ExecutionInput::StepSignature(ParticipantStepSignature::new(
                fixture.producer,
                commitment.step,
                producer_bls.sign(&commitment.signing_bytes()),
            )),
            8,
        )
        .await
        .expect("producer step signature");
    writer
        .apply_input(
            ExecutionInput::StepSignature(ParticipantStepSignature::new(
                other_peer(fixture),
                commitment.step,
                other_bls.sign(&commitment.signing_bytes()),
            )),
            9,
        )
        .await
        .expect("peer step signature");
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
        .apply_input(
            ExecutionInput::TerminalSignature(ParticipantTerminalSignature::new(
                fixture.producer,
                producer_bls.sign(&terminal_commitment.signing_bytes()),
            )),
            10,
        )
        .await
        .expect("producer terminal signature");
    writer
        .apply_input(
            ExecutionInput::TerminalSignature(ParticipantTerminalSignature::new(
                other_peer(fixture),
                other_bls.sign(&terminal_commitment.signing_bytes()),
            )),
            11,
        )
        .await
        .expect("peer terminal signature");
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
    writer.assemble_receipt(13).await.expect("publish");
    let key = fixture.activation.session_hash();
    store
        .handle()
        .load_receipt(key)
        .await
        .expect("load receipt")
        .expect("receipt")
        .receipt
}

fn receipt_with_different_content(receipt: &ReceiptArtifact) -> ReceiptArtifact {
    let mut trace = receipt.body().trace().to_vec();
    trace[0].fuel_used = trace[0].fuel_used.saturating_add(1);
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
    let body = arena0_protocol::ReceiptBody::new(
        receipt.body().header().clone(),
        receipt.body().outcome().to_vec(),
        receipt.body().params().to_vec(),
        trace,
    )
    .expect("receipt body");

    ReceiptArtifact::new(body).expect("different receipt")
}

fn authenticated_stop_report(
    fixture: &ActivationFixture,
    identity: &NodeKeys,
    kind: AbortKind,
    code: u32,
    reason: &str,
) -> ReceiptArtifact {
    let unsigned = AbortOccurrence::unsigned(
        fixture.activation.session_hash(),
        PeerId(identity.ed25519_public_key().0),
        kind,
        code,
        reason,
        arena0_protocol::PublicCursor::new(
            0,
            fixture.activation.offer().data().initial_state,
            arena0_protocol::CHAIN_START,
        ),
    )
    .expect("abort occurrence");
    let signature = identity.sign(&unsigned.signing_bytes().expect("abort bytes"));
    let body = arena0_protocol::ReceiptBody::new(
        arena0_protocol::SessionHeader::new(
            fixture.activation.clone(),
            arena0_protocol::ReceiptTermination::Stopped {
                cause: arena0_protocol::StopCause::Authenticated(
                    unsigned.with_signature(signature).expect("signed abort"),
                ),
            },
        ),
        Vec::new(),
        fixture.activation.offer().data().params.as_bytes().to_vec(),
        Vec::new(),
    )
    .expect("receipt body");
    ReceiptArtifact::new(body).expect("stop report")
}

fn completed_receipt_fixture(fixture: &ActivationFixture) -> ReceiptArtifact {
    let state = ExecutionState::new(
        ExecId([0; 32]),
        fixture.activation.clone(),
        fixture.producer,
        SharedStateBytes::try_new(vec![0]).expect("shared"),
        LocalStateBytes::try_new(Vec::new()).expect("local"),
    )
    .expect("state");
    let delta = terminal_delta(fixture, &state);
    let mut entry = delta.entry().clone();
    let outcome = delta
        .terminal_outcome()
        .expect("terminal outcome")
        .borsh()
        .to_vec();
    let step_commitment = arena0_protocol::StepCommitment::for_entry(
        fixture.activation.session_hash(),
        &entry,
        arena0_protocol::trace::CHAIN_START,
    );
    let producer_bls = BlsSecretKey::from_seed(&[11; 32]).expect("producer bls");
    let other_bls = BlsSecretKey::from_seed(&[12; 32]).expect("other bls");
    entry.agreement = AggregateAttestation::from_signatures(
        arena0_protocol::SignerSet::full(2).expect("signer set"),
        &[
            producer_bls.sign(&step_commitment.signing_bytes()),
            other_bls.sign(&step_commitment.signing_bytes()),
        ],
    )
    .expect("step agreement");
    let outcome_hash = arena0_protocol::OutcomeHash::of(&outcome);
    let terminal_commitment = arena0_protocol::TerminalCommitment::new(
        fixture.activation.session_hash(),
        0,
        entry.post_state,
        outcome_hash,
    );
    let terminal_agreement = AggregateAttestation::from_signatures(
        arena0_protocol::SignerSet::full(2).expect("signer set"),
        &[
            producer_bls.sign(&terminal_commitment.signing_bytes()),
            other_bls.sign(&terminal_commitment.signing_bytes()),
        ],
    )
    .expect("terminal agreement");
    let body = arena0_protocol::ReceiptBody::new(
        arena0_protocol::SessionHeader::new(
            fixture.activation.clone(),
            arena0_protocol::ReceiptTermination::Completed {
                terminal: arena0_protocol::SessionTerminal {
                    final_step: 0,
                    final_state: entry.post_state,
                    outcome_hash,
                    agreement: terminal_agreement,
                },
            },
        ),
        outcome,
        fixture.activation.offer().data().params.as_bytes().to_vec(),
        vec![entry],
    )
    .expect("receipt body");
    ReceiptArtifact::new(body).expect("receipt fixture")
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
        StateHash::of(initial_state.as_bytes()),
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

fn creator_admission(negotiation_id: NegotiationId) -> ExecutionAdmission {
    ExecutionAdmission::create(negotiation_id, 2).expect("creator admission")
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
    let admission = creator_admission(negotiation_id);
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
            ExecutionAdmission::create(NegotiationId([0x74; 32]), 2).expect("admission"),
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
async fn creator_admission_requires_params() {
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
        .create_execution_request(hash, None, creator_admission(NegotiationId([0x72; 32])), 2)
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
    let admission = creator_admission(NegotiationId([3; 32]));
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
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    writer
        .create_execution_request(
            program_hash,
            Some(JsonBytes::try_new(br#"{}"#.to_vec()).expect("params")),
            ExecutionAdmission::create(NegotiationId([0x11; 32]), 2).expect("admission"),
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
async fn applied_state_without_occurrence_fact_is_corruption() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let fixture = activation_fixture();
    let execution_id = ExecId([0x25; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    store.shutdown().await.expect("shutdown");

    let connection = Connection::open(&path).expect("inspect");
    connection
        .execute(
            "DELETE FROM occurrences WHERE execution_id = ?1",
            params![execution_id.0.to_vec()],
        )
        .expect("remove occurrence fact");
    drop(connection);

    let store = Store::open(StoreConfig::new(&path, fixture.producer)).expect("reopen");
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    assert!(matches!(
        writer.apply_input(ExecutionInput::Activate, 7).await,
        Err(StoreError::Corruption(message)) if message.contains("without its occurrence fact")
    ));
    drop(writer);
    store.shutdown().await.expect("shutdown reopened");
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
                creator_admission(NegotiationId([index; 32])),
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
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    writer
        .create_execution_request(
            program_hash,
            Some(JsonBytes::try_new(br#"{}"#.to_vec()).expect("params")),
            ExecutionAdmission::create(NegotiationId([0x11; 32]), 2).expect("admission"),
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
async fn activation_timer_and_outbox_recovery_is_idempotent() {
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
    let admission =
        ExecutionAdmission::create(NegotiationId([0x11; 32]), 2).expect("fixture admission");
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
            .apply_input(ExecutionInput::Activate, 7)
            .await
            .expect("activate"),
        ApplyOutcome::Committed(_)
    ));
    assert!(matches!(
        writer
            .apply_input(ExecutionInput::Activate, 8)
            .await
            .expect("retry"),
        ApplyOutcome::AlreadyApplied
    ));
    let timer = PrivateDelta::from_record(
        id,
        PrivateRecord {
            seq: 0,
            after_position: 0,
            event: PrivateEvent::React,
            effects: vec![PrivateEffect::SetTimer {
                delay_ms: 10,
                timer: None,
            }],
            draws: Vec::new(),
            fuel_used: 0,
            pending: None,
        },
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        PrivateContext::new(100),
        PrivateCause::react(),
    )
    .expect("timer delta");
    assert!(matches!(
        writer
            .apply_input(ExecutionInput::Private(timer), 9)
            .await
            .expect("arm timer"),
        ApplyOutcome::Committed(_)
    ));
    drop(writer);
    let writer = store
        .handle()
        .claim_execution(id)
        .expect("execution writer");
    assert!(writer.due_timers(109, 1).await.expect("not due").is_empty());
    assert_eq!(writer.due_timers(110, 1).await.expect("due").len(), 1);
    drop(writer);
    store.shutdown().await.expect("shutdown");

    // There is no public owner operation for inserting arbitrary ordered
    // outbox effects without also dispatching a guest/network effect. Keep
    // this direct SQL fixture limited to those two effects, after the Store
    // has closed, so recovery and lease ordering still run through
    // Store::open and StoreHandle.
    let connection = Connection::open(&path).expect("inspect");
    let effect0 = DurableEffect::notify(FrameId::derive(b"first"), b"a".to_vec()).expect("effect");
    let effect1 = DurableEffect::notify(FrameId::derive(b"second"), b"b".to_vec()).expect("effect");
    for (ordinal, effect) in [(0_u32, effect0), (1_u32, effect1)] {
        let outbox_id =
            OutboxId::derive(id, ExecutionVersion::new(1), ordinal, &effect).expect("id");
        let payload = borsh::to_vec(&effect).expect("effect bytes");
        connection.execute("INSERT INTO outbox (execution_id,outbox_id,version,ordinal,effect,attempts,status,available_at_ms,lease_id,lease_until_ms,last_error) VALUES (?1,?2,1,?3,?4,0,'pending',9,NULL,NULL,NULL)", rusqlite::params![id.0.to_vec(), outbox_id.as_bytes().to_vec(), i64::from(ordinal), envelope(EnvelopeKind::Effect, &payload).expect("envelope")]).expect("outbox");
    }
    drop(connection);
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
            .due_timers(110, 1)
            .await
            .expect("due after reopen")
            .len(),
        1
    );
    let first = writer
        .lease_next_outbox(9)
        .await
        .expect("lease first")
        .expect("first item");
    let first_id = first.item.outbox_id;
    let first_lease = first.lease_id;
    assert!(
        writer
            .lease_next_outbox(9)
            .await
            .expect("blocked")
            .is_none()
    );
    assert_eq!(
        writer
            .acknowledge_outbox(first_id, first_lease)
            .await
            .expect("ack"),
        OutboxDeliveryOutcome::Acknowledged
    );
    assert!(
        writer
            .lease_next_outbox(9)
            .await
            .expect("lease second")
            .is_some()
    );
    drop(writer);
    reopened.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn private_inspection_projects_bounded_redacted_summaries() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0x5c; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");

    let callout = PrivateEffect::Callout {
        callout_index: 0,
        context: vec![3, 4],
        pending_label: None,
        expected_type: None,
        continuation_tag: None,
    };
    let pending_id = arena0_protocol::execution::pending_id(execution_id, 0, 0);
    let pending =
        arena0_protocol::PendingRecord::from_effect(pending_id, &callout).expect("pending callout");
    let first = PrivateDelta::from_record(
        execution_id,
        PrivateRecord {
            seq: 0,
            after_position: 0,
            event: PrivateEvent::React,
            effects: vec![callout],
            draws: Vec::new(),
            fuel_used: 100,
            pending: Some(pending),
        },
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        PrivateContext::new(100),
        PrivateCause::react(),
    )
    .expect("first private delta");
    writer
        .apply_input(ExecutionInput::Private(first), 20)
        .await
        .expect("first private commit");

    let second = PrivateDelta::from_record(
        execution_id,
        PrivateRecord {
            seq: 1,
            after_position: 0,
            event: PrivateEvent::InputReceived {
                callout_index: 0,
                data: vec![0xff],
                continuation_tag: None,
            },
            effects: vec![PrivateEffect::RetryInput {
                reason: "try again".into(),
            }],
            draws: Vec::new(),
            fuel_used: 101,
            pending: None,
        },
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        PrivateContext::new(100),
        PrivateCause::resume(pending_id, arena0_protocol::PendingKind::Callout),
    )
    .expect("second private delta");
    writer
        .apply_input(ExecutionInput::Private(second), 21)
        .await
        .expect("second private commit");
    drop(writer);

    let page = store
        .handle()
        .read_private_summaries(execution_id, Some(0), 1)
        .await
        .expect("private inspection page");
    assert_eq!(page.total(), 2);
    assert_eq!(page.next(), Some(1));
    assert_eq!(page.summaries().len(), 1);
    assert_eq!(page.summaries()[0].sequence, 0);
    assert_eq!(page.summaries()[0].public_position, 0);
    assert_eq!(page.summaries()[0].event, PrivateEventKind::React);
    assert_eq!(page.summaries()[0].input_payload_bytes, None);
    assert_eq!(page.summaries()[0].fuel_used, 100);
    assert_eq!(page.summaries()[0].effects.len(), 1);
    assert_eq!(
        page.summaries()[0].effects[0],
        PrivateEffectSummary {
            kind: PrivateEffectKind::Callout,
            payload_bytes: Some(2),
        }
    );

    let tail = store
        .handle()
        .read_private_summaries(execution_id, Some(1), MAX_PRIVATE_INSPECTION_RECORDS)
        .await
        .expect("private inspection tail");
    assert_eq!(tail.from(), 1);
    assert_eq!(tail.total(), 2);
    assert_eq!(tail.next(), None);
    assert_eq!(tail.summaries().len(), 1);
    assert_eq!(tail.summaries()[0].sequence, 1);
    assert_eq!(tail.summaries()[0].event, PrivateEventKind::InputReceived);
    assert_eq!(tail.summaries()[0].input_payload_bytes, Some(1));
    assert_eq!(tail.summaries()[0].fuel_used, 101);
    assert_eq!(
        tail.summaries()[0].effects[0],
        PrivateEffectSummary {
            kind: PrivateEffectKind::RetryInput,
            payload_bytes: Some(9),
        }
    );

    let latest = store
        .handle()
        .read_private_summaries(execution_id, None, 1)
        .await
        .expect("latest private inspection window");
    assert_eq!(latest.from(), 1);
    assert_eq!(latest.total(), 2);
    assert_eq!(latest.next(), None);
    assert_eq!(latest.summaries()[0].sequence, 1);

    assert!(matches!(
        store
            .handle()
            .read_private_summaries(execution_id, Some(0), MAX_PRIVATE_INSPECTION_RECORDS + 1,)
            .await,
        Err(StoreError::InvalidConfiguration(_))
    ));
    assert!(matches!(
        store
            .handle()
            .read_private_summaries(execution_id, Some(0), 0)
            .await,
        Err(StoreError::InvalidConfiguration(_))
    ));
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
    writer
        .apply_input(
            ExecutionInput::ProposeShared(terminal_delta(&fixture, &state)),
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
        commitment,
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
            .apply_inbound(inbox_id, 10)
            .await
            .expect("apply accepted frame"),
        ApplyOutcome::Committed(_)
    ));
    assert_eq!(
        writer
            .accept_inbound(source, frame, 11)
            .await
            .expect("redeliver applied frame"),
        InboxAcceptOutcome::AlreadyApplied
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
    let seq = state.public().next_step();
    let prestate = state.public().state_hash();
    let session_id = state.binding().session_id();
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    for byte in 0..65_u8 {
        let data = vec![byte];
        let witness = WitnessCommitment([byte; 32]);
        let frame = ExecFrame::Message {
            message_id: MessageId::derive(session_id, source, seq, prestate, &data, witness),
            seq,
            prestate,
            data,
            witness,
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
    let witness = WitnessCommitment([0x77; 32]);
    let frame = ExecFrame::Message {
        message_id: MessageId::derive(
            fixture.activation.session_hash(),
            source,
            state.public().next_step(),
            state.public().state_hash(),
            &data,
            witness,
        ),
        seq: state.public().next_step(),
        prestate: state.public().state_hash(),
        data,
        witness,
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

    let creator_id = ExecId([0xa1; 32]);
    let creator =
        ExecutionAdmission::create(NegotiationId([0x11; 32]), 3).expect("creator admission");
    let mut creator_writer = store
        .handle()
        .claim_execution(creator_id)
        .expect("execution writer");
    creator_writer
        .create_execution_request(
            hash,
            Some(JsonBytes::try_new(b"{}".to_vec()).expect("json")),
            creator,
            2,
        )
        .await
        .expect("request");
    assert!(matches!(
        creator_writer
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
    drop(creator_writer);
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
async fn receipt_body_is_assembled_from_durable_rows_after_restart() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let execution_id = ExecId([0x31; 32]);
    let store = create_execution(&path, &fixture, execution_id).await;
    certify_terminal(&store, &fixture, execution_id).await;
    store.shutdown().await.expect("shutdown before assembly");

    // The terminal certificate and all public commit rows are durable before
    // the receipt body is requested. Assembly therefore exercises restart
    // recovery rather than an in-memory execution shortcut.
    let store = Store::open(StoreConfig::new(&path, fixture.producer)).expect("reopen");
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    assert!(matches!(
        writer.assemble_receipt(12).await.expect("assemble"),
        ApplyOutcome::Committed(_)
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
    assert_eq!(published.provenance, ReceiptProvenance::Produced);
    assert_eq!(
        published.receipt.body().header().session_hash(),
        fixture.activation.session_hash()
    );
    assert!(matches!(
        writer
            .apply_input(
                ExecutionInput::ReceiptBody(Box::new(published.receipt.body().clone())),
                12,
            )
            .await,
        Err(StoreError::ReceiptBodyRequiresAssembly)
    ));
    drop(writer);
    store.shutdown().await.expect("shutdown after assembly");
}

#[tokio::test]
async fn stopped_receipt_is_assembled_after_restart_and_verifies() {
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
        state.public(),
    )
    .expect("abort occurrence");
    let occurrence = unsigned
        .clone()
        .with_signature(producer_keys.sign(&unsigned.signing_bytes().expect("abort bytes")))
        .expect("signed abort");
    let expected_occurrence = occurrence.clone();
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    writer
        .apply_input(ExecutionInput::Abort(occurrence), 7)
        .await
        .expect("abort");
    drop(writer);
    store
        .shutdown()
        .await
        .expect("shutdown before stopped assembly");

    let store = Store::open(StoreConfig::new(&path, fixture.producer)).expect("reopen stopped");
    let mut writer = store
        .handle()
        .claim_execution(execution_id)
        .expect("execution writer");
    assert!(matches!(
        writer.assemble_receipt(8).await.expect("assemble stopped"),
        ApplyOutcome::Committed(_)
    ));
    drop(writer);
    let stored = store
        .handle()
        .load_receipt(fixture.activation.session_hash())
        .await
        .expect("load stopped receipt")
        .expect("stopped receipt");
    assert_eq!(stored.provenance, ReceiptProvenance::Produced);
    let encoded = stored.receipt.encode().expect("encode receipt");
    let verified = arena0_verify::verify_light(&encoded).expect("verify stopped receipt");
    assert!(matches!(
        verified.terminal,
        arena0_verify::LightVerifiedTerminal::Stopped {
            cause: arena0_protocol::StopCause::Authenticated(actual),
        } if actual.reason() == "operator stop"
            && actual == expected_occurrence
    ));
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn persisted_receipt_tampering_fails_closed_on_restart() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let receipt = authenticated_stop_report(
        &fixture,
        &NodeKeys::from_secret(SecretKey::from_bytes([1; 32])),
        AbortKind::Fail,
        88,
        "tamper fixture",
    );
    let imported_host = other_peer(&fixture);
    let store = Store::open(StoreConfig::new(&path, imported_host)).expect("open");
    assert_eq!(
        store
            .handle()
            .import_receipt(receipt.clone(), 8)
            .await
            .expect("import"),
        ReceiptImportOutcome::Imported
    );
    store.shutdown().await.expect("shutdown");

    // Direct SQL is intentional here: the test must inject corruption into
    // the persisted envelope after the real import owner has committed it.
    let connection = Connection::open(&path).expect("inspect");
    let mut artifact: Vec<u8> = connection
        .query_row(
            "SELECT artifact FROM receipts WHERE receipt_id = ?1",
            rusqlite::params![receipt.receipt_id().as_bytes().to_vec()],
            |row| row.get(0),
        )
        .expect("receipt artifact");
    let last = artifact.len().checked_sub(1).expect("nonempty artifact");
    artifact[last] ^= 0x80;
    connection
        .execute(
            "UPDATE receipts SET artifact = ?1 WHERE receipt_id = ?2",
            rusqlite::params![artifact, receipt.receipt_id().as_bytes().to_vec()],
        )
        .expect("tamper");
    drop(connection);
    assert!(matches!(
        Store::open(StoreConfig::new(&path, imported_host)),
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
    let source_path = directory.path().join("published.sqlite");
    let execution_id = ExecId([0x34; 32]);
    let store = create_execution(&source_path, &fixture, execution_id).await;
    certify_terminal(&store, &fixture, execution_id).await;
    publish_receipt(&store, &fixture, execution_id).await;
    store.shutdown().await.expect("shutdown published source");

    for (row, delete) in [
        (
            "receipt-production",
            "DELETE FROM receipt_productions WHERE execution_id = ?1",
        ),
        (
            "terminal-proof",
            "DELETE FROM terminal_proofs WHERE execution_id = ?1",
        ),
    ] {
        let path = directory.path().join(format!("missing-{row}.sqlite"));

        // The Store owner is closed before this copy. Checkpoint the source
        // WAL, then let SQLite write a self-contained destination; copying
        // only the main file while a Store is live could omit WAL/SHM pages.
        let source = Connection::open(&source_path).expect("open closed source");
        source
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .expect("checkpoint source");
        source
            .execute(
                "VACUUM INTO ?1",
                rusqlite::params![path.to_str().expect("utf8 database path")],
            )
            .expect("copy checkpointed database");
        drop(source);

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
    let imported_path = directory.path().join("imported.sqlite");
    let receipt = authenticated_stop_report(
        &fixture,
        &NodeKeys::from_secret(SecretKey::from_bytes([1; 32])),
        AbortKind::Abort,
        0,
        "import fixture",
    );
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
    imported.shutdown().await.expect("shutdown imported");

    let reopened = Store::open(StoreConfig::new(&imported_path, imported_host)).expect("reopen");
    let recovered = reopened
        .handle()
        .load_receipt_by_id(receipt.receipt_id())
        .await
        .expect("load by id")
        .expect("recovered imported receipt");
    assert_eq!(recovered.provenance, ReceiptProvenance::Imported);
    assert_eq!(recovered.receipt, receipt);
    assert!(
        reopened
            .handle()
            .load_receipt(receipt.body().header().session_hash())
            .await
            .expect("load local publication")
            .is_none()
    );
    reopened.shutdown().await.expect("shutdown reopened");
}

#[tokio::test]
async fn imported_receipt_rejects_canonical_conflict_without_overwrite() {
    let fixture = activation_fixture();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("store.sqlite");
    let receipt = completed_receipt_fixture(&fixture);
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
    let stored = store
        .handle()
        .load_receipt_by_id(receipt.receipt_id())
        .await
        .expect("load original")
        .expect("original receipt");
    assert_eq!(stored.receipt, receipt);
    assert_eq!(stored.provenance, ReceiptProvenance::Imported);
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
    let receipt = publish_receipt(&source, &fixture, ExecId([0x43; 32])).await;
    source.shutdown().await.expect("shutdown source");

    let target = Store::open(StoreConfig::new(&target_path, fixture.producer)).expect("open");
    assert_eq!(
        target
            .handle()
            .import_receipt(receipt.clone(), 20)
            .await
            .expect("import"),
        ReceiptImportOutcome::Imported
    );
    target.shutdown().await.expect("shutdown imported target");
    let target = create_execution(&target_path, &fixture, ExecId([0x44; 32])).await;
    certify_terminal(&target, &fixture, ExecId([0x44; 32])).await;
    let published = publish_receipt(&target, &fixture, ExecId([0x44; 32])).await;
    assert_eq!(published, receipt);
    let stored = target
        .handle()
        .load_receipt_by_id(receipt.receipt_id())
        .await
        .expect("load promoted")
        .expect("promoted receipt");
    assert_eq!(stored.provenance, ReceiptProvenance::Both);
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
    let receipt = publish_receipt(&store, &fixture, execution_id).await;

    assert_eq!(
        store
            .handle()
            .import_receipt(receipt.clone(), 14)
            .await
            .expect("import produced receipt"),
        ReceiptImportOutcome::AlreadyProduced
    );
    store.shutdown().await.expect("shutdown");

    let reopened = Store::open(StoreConfig::new(&path, fixture.producer)).expect("reopen");
    let recovered = reopened
        .handle()
        .load_receipt_by_id(receipt.receipt_id())
        .await
        .expect("load recovered receipt")
        .expect("recovered receipt");
    assert_eq!(recovered.receipt, receipt);
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
        let report = authenticated_stop_report(
            &fixture,
            &identity,
            AbortKind::Abort,
            0,
            "local observation",
        );
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
    assert!(
        store
            .handle()
            .load_receipt(fixture.activation.session_hash())
            .await
            .unwrap()
            .is_none()
    );
    for report in &reports {
        let loaded = store
            .handle()
            .load_receipt_by_id(report.receipt_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&loaded.receipt, report);
        assert_eq!(loaded.provenance, ReceiptProvenance::Imported);
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
