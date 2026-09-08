use super::*;
use crate::*;
use arena0_crypto::bls::BlsSecretKey;
use arena0_crypto::{BlsSignature, NodeKeys, SecretKey, key_binding_message};
use arena0_program::{JsonBytes, ProgramHash};

fn commit(outcome: TransitionOutcome) -> CommitPlan {
    match outcome {
        TransitionOutcome::Commit(plan) => *plan,
        TransitionOutcome::AlreadyApplied => panic!("test expected a new commit plan"),
    }
}

struct Fixture {
    activation: Activation,
    producer: PeerId,
    producer_keys: NodeKeys,
    other_keys: NodeKeys,
    creator_bls: BlsSecretKey,
    other_bls: BlsSecretKey,
    peers: [PeerId; 2],
}

fn fixture() -> Fixture {
    fixture_for_program(0x22)
}

fn fixture_for_program(program_byte: u8) -> Fixture {
    let producer_keys = NodeKeys::from_secret(SecretKey::from_bytes([1; 32]));
    let other_keys = NodeKeys::from_secret(SecretKey::from_bytes([2; 32]));
    let creator_bls = BlsSecretKey::from_seed(&[11; 32]).expect("creator BLS key");
    let other_bls = BlsSecretKey::from_seed(&[12; 32]).expect("other BLS key");
    let producer = PeerId::from_ed25519(&producer_keys.ed25519_public_key());
    let other = PeerId::from_ed25519(&other_keys.ed25519_public_key());
    let profile = arena0_program::ExecutionProfile::current().hash();
    let offer_data = crate::OfferData::new(
        NegotiationId([0x11; 32]),
        0,
        producer,
        ProgramHash([program_byte; 32]),
        profile,
        JsonBytes::try_new(br#"{}"#.to_vec()).expect("JSON params"),
        2,
        StateHash::of(&[0]),
        1_000_000,
    )
    .expect("offer data");
    let offer_hash = crate::OfferHash::of(&offer_data);
    let make_ticket = |keys: &NodeKeys, bls: &BlsSecretKey| {
        let peer = PeerId::from_ed25519(&keys.ed25519_public_key());
        let public = bls.public_key();
        let binding = bls.sign_binding(&key_binding_message(&offer_hash.0, &peer.0, &public));
        let data = crate::TicketData::new(
            NegotiationId([0x11; 32]),
            0,
            peer,
            0,
            crate::TicketAction::Active {
                execution_bls: public,
                key_binding: binding,
                issued_at_unix_ms: 1,
                valid_for_ms: 60_000,
            },
        )
        .expect("ticket data");
        crate::Ticket {
            signature: keys.sign(&data.signing_bytes()),
            data,
        }
    };
    let tickets = vec![
        make_ticket(&producer_keys, &creator_bls),
        make_ticket(&other_keys, &other_bls),
    ];
    let ticket_hashes = tickets
        .iter()
        .map(|ticket| crate::TicketHash::of(&ticket.data))
        .collect::<Vec<_>>();
    let activation_data =
        crate::ActivationData::new(offer_hash, ticket_hashes.clone()).expect("activation data");
    let aggregate = BlsSignature::aggregate(&[
        creator_bls.sign(&activation_data.signing_bytes()),
        other_bls.sign(&activation_data.signing_bytes()),
    ])
    .expect("activation aggregate");
    let offer = crate::Offer::new(offer_data, ticket_hashes).expect("offer");
    let prepared = crate::PreparedActivation::new(offer, tickets).expect("prepared");
    let activation = Activation::new(prepared, aggregate).expect("activation structure");
    activation.validate().expect("validated activation");
    Fixture {
        activation,
        producer,
        producer_keys,
        other_keys,
        creator_bls,
        other_bls,
        peers: [producer, other],
    }
}

fn signed_abort(
    state: &ExecutionState,
    keys: &NodeKeys,
    sender: PeerId,
    kind: AbortKind,
    code: u32,
    reason: &str,
) -> AbortOccurrence {
    let unsigned = AbortOccurrence::unsigned(
        state.binding().session_id(),
        sender,
        kind,
        code,
        reason,
        state.public(),
    )
    .expect("abort occurrence");
    let signature = keys.sign(&unsigned.signing_bytes().expect("abort signing bytes"));
    unsigned.with_signature(signature).expect("signed abort")
}

fn active_state(fixture: &Fixture) -> ExecutionState {
    let state = ExecutionState::new(
        ExecId([0xEE; 32]),
        fixture.activation.clone(),
        fixture.producer,
        SharedStateBytes::try_new(vec![0]).expect("shared state"),
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
    )
    .expect("initial state");
    commit(transition(&state, ExecutionInput::Activate).expect("activation plan"))
        .next_state()
        .clone()
}

fn shared_delta(
    state: &ExecutionState,
    effects: Vec<PublicEffect>,
    next_bytes: Vec<u8>,
) -> SharedDelta {
    let next = SharedStateBytes::try_new(next_bytes).expect("shared state");
    let terminal_outcome = effects.iter().find_map(|effect| match effect {
        PublicEffect::SessionEnd { outcome } => {
            Some(TerminalOutcome::new(outcome.clone(), b"null".to_vec()).expect("terminal outcome"))
        }
        _ => None,
    });
    SharedDelta::new(
        TraceEntry {
            trace_version: TRACE_FORMAT_VERSION,
            step: state.public().next_step(),
            event: if state.public().next_step() == 0 {
                PublicEvent::SessionStarted {
                    ensemble: Ensemble::from_peers(
                        state
                            .binding()
                            .activation()
                            .tickets()
                            .iter()
                            .map(|ticket| ticket.data.signer)
                            .collect(),
                    )
                    .expect("ensemble"),
                }
            } else {
                PublicEvent::MessageReceived {
                    message_id: MessageId([1; 32]),
                    from: state.producer(),
                    position: state.public().next_step(),
                    pre_state: state.public().state_hash(),
                    msg: vec![1],
                }
            },
            effects,
            pre_state: state.public().state_hash(),
            post_state: StateHash::of(next.as_bytes()),
            fuel_used: 0,
            witness: None,
            agreement: AggregateAttestation::empty(),
        },
        next,
        terminal_outcome,
    )
    .expect("shared delta")
}

fn certify_shared(
    fixture: &Fixture,
    mut state: ExecutionState,
    delta: SharedDelta,
) -> ExecutionState {
    state =
        commit(transition(&state, ExecutionInput::ProposeShared(delta)).expect("shared proposal"))
            .next_state()
            .clone();
    let commitment = state
        .pending_shared()
        .expect("pending proposal")
        .commitment()
        .clone();
    let first = ParticipantStepSignature::new(
        fixture.peers[0],
        commitment.step,
        fixture.creator_bls.sign(&commitment.signing_bytes()),
    );
    state = commit(
        transition(&state, ExecutionInput::StepSignature(first)).expect("first step signature"),
    )
    .next_state()
    .clone();
    let second = ParticipantStepSignature::new(
        fixture.peers[1],
        commitment.step,
        fixture.other_bls.sign(&commitment.signing_bytes()),
    );
    commit(
        transition(&state, ExecutionInput::StepSignature(second)).expect("second step signature"),
    )
    .next_state()
    .clone()
}

fn pending_terminal(fixture: &Fixture) -> ExecutionState {
    let state = active_state(fixture);
    let delta = shared_delta(
        &state,
        vec![PublicEffect::SessionEnd {
            outcome: vec![9, 8, 7],
        }],
        vec![1],
    );
    certify_shared(fixture, state, delta)
}

#[test]
fn outbox_occurrence_id_binds_execution_version_ordinal_and_effect() {
    let effect = DurableEffect::notify(FrameId::derive(b"frame"), b"payload".to_vec()).unwrap();
    let changed = DurableEffect::notify(FrameId::derive(b"frame"), b"other".to_vec()).unwrap();
    let id = OutboxId::derive(ExecId([1; 32]), ExecutionVersion::new(3), 0, &effect).unwrap();
    for (execution, version, ordinal, effect) in [
        (ExecId([2; 32]), 3, 0, &effect),
        (ExecId([1; 32]), 4, 0, &effect),
        (ExecId([1; 32]), 3, 1, &effect),
        (ExecId([1; 32]), 3, 0, &changed),
    ] {
        assert_ne!(
            id,
            OutboxId::derive(execution, ExecutionVersion::new(version), ordinal, effect).unwrap()
        );
    }
}

#[test]
fn one_shot_timer_can_be_rearmed_after_cancellation() {
    let id = TimerId::derive(b"timer");
    let mut active = Vec::new();
    for deadline in 0..=MAX_ACTIVE_TIMERS as u64 {
        apply_timer_mutations(
            &mut active,
            &[TimerMutation::arm(id, deadline, Vec::new()).unwrap()],
        )
        .unwrap();
        assert_eq!(active.len(), 1);
        apply_timer_mutations(&mut active, &[TimerMutation::cancel(id)]).unwrap();
        assert!(active.is_empty());
    }
}

#[test]
fn malformed_input_is_rejected_by_total_bound_before_decode() {
    let bytes = vec![0; MAX_EXECUTION_INPUT_BYTES + 1];
    assert!(matches!(
        ExecutionInput::decode(&bytes),
        Err(ProtocolError::EncodedTooLarge { .. })
    ));
}

#[test]
fn commit_plan_rejects_changes_to_immutable_execution_roots() {
    let fixture = fixture();
    let state = active_state(&fixture);
    let plan = |next| build_plan(&state, next, None, None, None, Vec::new(), Vec::new());

    let mut changed_execution = state.clone();
    changed_execution.execution_id = ExecId([0xEF; 32]);
    assert!(matches!(
        plan(changed_execution),
        Err(ProtocolError::BindingMismatch)
    ));

    let mut changed_producer = state.clone();
    changed_producer.producer = fixture.peers[1];
    assert!(matches!(
        plan(changed_producer),
        Err(ProtocolError::BindingMismatch)
    ));

    let mut changed_binding = state.clone();
    changed_binding.binding =
        ExecutionBinding::new(fixture_for_program(0x23).activation).expect("alternate binding");
    assert!(matches!(
        plan(changed_binding),
        Err(ProtocolError::BindingMismatch)
    ));
}

#[test]
fn lifecycle_rejects_execution_before_activation() {
    let fixture = fixture();
    let state = ExecutionState::new(
        ExecId([0xEE; 32]),
        fixture.activation,
        fixture.producer,
        SharedStateBytes::try_new(vec![0]).expect("shared state"),
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
    )
    .expect("initial state");
    assert_eq!(state.lifecycle(), ExecLifecycle::Activating);
    assert_eq!(state.version(), ExecutionVersion::ZERO);
    assert_eq!(state.public().next_step(), 0);
    assert_eq!(state.private().next_record(), 0);
    let error = transition(
        &state,
        ExecutionInput::ProposeShared(shared_delta(&state, Vec::new(), vec![1])),
    )
    .expect_err("proposal before activation");
    assert!(matches!(
        error,
        ProtocolError::IllegalLifecycle {
            current: ExecLifecycle::Activating,
            ..
        }
    ));
}

#[test]
fn terminal_status_owns_authenticated_causes_and_incomplete_proof() {
    let fixture = fixture();
    let state = active_state(&fixture);

    let local_occurrence = signed_abort(
        &state,
        &fixture.producer_keys,
        fixture.producer,
        AbortKind::Abort,
        7,
        "operator stop",
    );
    let aborted = commit(
        transition(&state, ExecutionInput::Abort(local_occurrence.clone())).expect("abort plan"),
    );
    assert!(matches!(
        aborted.next_state().status().terminal_cause(),
        Some(StopCause::Authenticated(occurrence))
            if occurrence.reason() == "operator stop"
                && occurrence.sender() == fixture.producer
    ));

    let peer_occurrence = signed_abort(
        &state,
        &fixture.other_keys,
        fixture.peers[1],
        AbortKind::Fail,
        8,
        "peer failure",
    );
    let failed =
        commit(transition(&state, ExecutionInput::Abort(peer_occurrence)).expect("peer plan"));
    assert!(matches!(
        failed.next_state().status().terminal_cause(),
        Some(StopCause::Authenticated(occurrence))
            if occurrence.reason() == "peer failure"
                && occurrence.sender() == fixture.peers[1]
    ));

    let wrong_key = signed_abort(
        &state,
        &fixture.other_keys,
        fixture.producer,
        AbortKind::Abort,
        9,
        "wrong key",
    );
    assert!(matches!(
        transition(&state, ExecutionInput::Abort(wrong_key)),
        Err(ProtocolError::UnauthenticatedAbort)
    ));
    let outsider_keys = NodeKeys::from_secret(SecretKey::from_bytes([3; 32]));
    let outsider = PeerId::from_ed25519(&outsider_keys.ed25519_public_key());
    let outsider_occurrence = signed_abort(
        &state,
        &outsider_keys,
        outsider,
        AbortKind::Abort,
        10,
        "outsider",
    );
    assert!(matches!(
        transition(&state, ExecutionInput::Abort(outsider_occurrence)),
        Err(ProtocolError::UnauthenticatedAbort)
    ));
    let wrong_coordinate = AbortOccurrence::unsigned(
        state.binding().session_id(),
        fixture.producer,
        AbortKind::Abort,
        11,
        "wrong coordinate",
        PublicCursor::new(
            state.public().next_step() + 1,
            state.public().state_hash(),
            state.public().chain_hash(),
        ),
    )
    .expect("abort occurrence");
    let wrong_coordinate_bytes = wrong_coordinate
        .signing_bytes()
        .expect("abort signing bytes");
    let wrong_coordinate = wrong_coordinate
        .with_signature(fixture.producer_keys.sign(&wrong_coordinate_bytes))
        .expect("signed abort");
    assert!(matches!(
        transition(&state, ExecutionInput::Abort(wrong_coordinate)),
        Err(ProtocolError::InvalidAbortCoordinate)
    ));

    let pending = pending_terminal(&fixture);
    let incomplete = commit(
        transition(
            &pending,
            ExecutionInput::InterruptTerminal("transport stopped".into()),
        )
        .expect("incomplete plan"),
    );
    assert_eq!(
        incomplete.next_state().lifecycle(),
        ExecLifecycle::Incomplete
    );
    assert!(incomplete.next_state().status().terminal_proof().is_some());
    assert!(matches!(
        incomplete.next_state().status().receipt_work(),
        ReceiptWork::Incomplete
    ));
    assert!(matches!(
        incomplete.next_state().status(),
        ExecutionStatus::Incomplete { reason, .. } if reason == "transport stopped"
    ));
}

#[test]
fn authenticated_abort_discards_an_uncertified_shared_proposal() {
    let fixture = fixture();
    let active = active_state(&fixture);
    let proposed = commit(
        transition(
            &active,
            ExecutionInput::ProposeShared(shared_delta(&active, Vec::new(), vec![1])),
        )
        .expect("proposal plan"),
    )
    .next_state()
    .clone();
    assert!(proposed.pending_shared().is_some());

    let occurrence = signed_abort(
        &proposed,
        &fixture.other_keys,
        fixture.peers[1],
        AbortKind::Abort,
        12,
        "cannot certify proposal",
    );
    let stopped = commit(
        transition(&proposed, ExecutionInput::Abort(occurrence)).expect("abort preempts proposal"),
    );

    assert!(stopped.next_state().pending_shared().is_none());
    assert_eq!(stopped.next_state().lifecycle(), ExecLifecycle::Aborted);
    assert_eq!(stopped.next_state().public(), active.public());
}

#[test]
fn private_progress_does_not_advance_public_cursor() {
    let fixture = fixture();
    let state = active_state(&fixture);
    let record = PrivateRecord {
        seq: 0,
        after_position: 0,
        event: PrivateEvent::React,
        effects: vec![PrivateEffect::Broadcast { data: vec![1, 2] }],
        draws: Vec::new(),
        fuel_used: 0,
        pending: None,
    };
    let delta = PrivateDelta::from_record(
        state.execution_id(),
        record,
        LocalStateBytes::try_new(vec![3]).expect("local state"),
        PrivateContext::new(0),
        PrivateCause::react(),
    )
    .expect("private delta");
    let plan = commit(transition(&state, ExecutionInput::Private(delta)).expect("private plan"));
    assert_eq!(plan.next_state().version(), ExecutionVersion::new(2));
    assert_eq!(plan.next_state().private().next_record(), 1);
    assert_eq!(plan.next_state().public(), state.public());
    assert_eq!(plan.outbox().len(), fixture.peers.len());
    assert!(plan.shared().is_none());
}

#[test]
fn automatic_reaction_is_once_per_durable_public_position() {
    let fixture = fixture();
    let state = active_state(&fixture);
    let first = PrivateDelta::from_record(
        state.execution_id(),
        PrivateRecord {
            seq: 0,
            after_position: state.public().next_step(),
            event: PrivateEvent::React,
            effects: Vec::new(),
            draws: Vec::new(),
            fuel_used: 0,
            pending: None,
        },
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        PrivateContext::new(0),
        PrivateCause::react(),
    )
    .expect("first reaction");
    let reacted =
        commit(transition(&state, ExecutionInput::Private(first)).expect("commit reaction"))
            .next_state()
            .clone();
    assert_eq!(
        reacted.private().last_reaction_position(),
        Some(state.public().next_step())
    );

    let duplicate = PrivateDelta::from_record(
        reacted.execution_id(),
        PrivateRecord {
            seq: reacted.private().next_record(),
            after_position: reacted.public().next_step(),
            event: PrivateEvent::React,
            effects: Vec::new(),
            draws: Vec::new(),
            fuel_used: 0,
            pending: None,
        },
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        PrivateContext::new(1),
        PrivateCause::react(),
    )
    .expect("duplicate reaction input");
    assert!(matches!(
        transition(&reacted, ExecutionInput::Private(duplicate)),
        Err(ProtocolError::PrivateReactionAlreadyCommitted { position })
            if position == reacted.public().next_step()
    ));
}

#[test]
fn shared_cursor_advances_only_after_all_valid_signatures() {
    let fixture = fixture();
    let state = active_state(&fixture);
    let delta = shared_delta(&state, Vec::new(), vec![1]);
    let proposal =
        commit(transition(&state, ExecutionInput::ProposeShared(delta)).expect("proposal plan"))
            .next_state()
            .clone();
    assert_eq!(proposal.public(), state.public());
    let commitment = proposal
        .pending_shared()
        .expect("proposal")
        .commitment()
        .clone();
    let first = ParticipantStepSignature::new(
        fixture.peers[0],
        commitment.step,
        fixture.creator_bls.sign(&commitment.signing_bytes()),
    );
    let partial = commit(
        transition(&proposal, ExecutionInput::StepSignature(first.clone()))
            .expect("partial certificate"),
    )
    .next_state()
    .clone();
    assert_eq!(partial.public(), state.public());
    assert_eq!(partial.pending_shared().unwrap().signature_count(), 1);
    assert!(matches!(
        transition(&partial, ExecutionInput::StepSignature(first)),
        Ok(TransitionOutcome::AlreadyApplied)
    ));
    let conflicting =
        ParticipantStepSignature::new(fixture.peers[0], commitment.step, BlsSignature([0xAB; 48]));
    assert!(matches!(
        transition(&partial, ExecutionInput::StepSignature(conflicting)),
        Err(ProtocolError::ConflictingStepSignature { .. })
    ));
    let second = ParticipantStepSignature::new(
        fixture.peers[1],
        commitment.step,
        fixture.other_bls.sign(&commitment.signing_bytes()),
    );
    let full = commit(
        transition(&partial, ExecutionInput::StepSignature(second)).expect("complete certificate"),
    )
    .next_state()
    .clone();
    assert_eq!(full.public().next_step(), 1);
    assert_eq!(full.private().next_record(), 0);
    assert!(full.pending_shared().is_none());
}

#[test]
fn shared_start_boundary_is_enforced_and_classes_are_typed() {
    let fixture = fixture();
    let state = active_state(&fixture);

    let wrong_ensemble =
        Ensemble::from_peers(vec![fixture.peers[0], PeerId([3; 32])]).expect("two-peer ensemble");
    let mut wrong_start = shared_delta(&state, Vec::new(), vec![1]);
    wrong_start.entry.event = PublicEvent::SessionStarted {
        ensemble: wrong_ensemble,
    };
    assert!(matches!(
        transition(&state, ExecutionInput::ProposeShared(wrong_start)),
        Err(ProtocolError::SessionStartMismatch)
    ));

    assert!(PrivateEvent::try_from(Event::React).is_ok());
    assert!(PublicEvent::try_from(Event::React).is_err());
    assert!(PrivateEffect::try_from(Effect::Broadcast { data: vec![1] }).is_ok());
    assert!(PublicEffect::try_from(Effect::Broadcast { data: vec![1] }).is_err());

    let after_step = certify_shared(
        &fixture,
        state,
        shared_delta(&active_state(&fixture), Vec::new(), vec![1]),
    );
    let mut late_start = shared_delta(&after_step, Vec::new(), vec![2]);
    late_start.entry.event = PublicEvent::SessionStarted {
        ensemble: Ensemble::from_peers(fixture.peers.to_vec()).expect("ensemble"),
    };
    assert!(matches!(
        transition(&after_step, ExecutionInput::ProposeShared(late_start)),
        Err(ProtocolError::SessionStartPosition)
    ));
}

#[test]
fn timer_firing_cancels_the_active_timer_with_its_private_result() {
    let fixture = fixture();
    let state = active_state(&fixture);
    let arm = PrivateDelta::from_record(
        state.execution_id(),
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
    .expect("timer arm");
    let armed = commit(transition(&state, ExecutionInput::Private(arm)).expect("arm plan"));
    let timer_id = armed
        .next_state()
        .active_timers()
        .next()
        .expect("active timer");
    let firing = PrivateDelta::from_record(
        armed.execution_id(),
        PrivateRecord {
            seq: 1,
            after_position: 0,
            event: PrivateEvent::TimerFired,
            effects: Vec::new(),
            draws: Vec::new(),
            fuel_used: 0,
            pending: None,
        },
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        PrivateContext::new(0),
        PrivateCause::timer(TimerFiring::new(timer_id)),
    )
    .expect("timer result");
    let fired = commit(
        transition(armed.next_state(), ExecutionInput::Private(firing)).expect("timer result plan"),
    );
    assert!(fired.next_state().active_timers().next().is_none());
    assert!(matches!(
        fired.timers(),
        [TimerMutation::Cancel { timer_id: id }]
            if *id == timer_id
    ));
}

#[test]
fn retry_input_preserves_the_stored_continuation() {
    let fixture = fixture();
    let state = active_state(&fixture);
    let callout = PrivateEffect::Callout {
        callout_index: 2,
        context: vec![7, 8],
        pending_label: Some("choice".into()),
        expected_type: Some("Choice".into()),
        continuation_tag: Some(4),
    };
    let id = pending_id(state.execution_id(), 0, 0);
    let pending = PendingRecord::from_effect(id, &callout).expect("pending callout");
    let first = PrivateDelta::from_record(
        state.execution_id(),
        PrivateRecord {
            seq: 0,
            after_position: 0,
            event: PrivateEvent::React,
            effects: vec![callout],
            draws: Vec::new(),
            fuel_used: 0,
            pending: Some(pending.clone()),
        },
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        PrivateContext::new(0),
        PrivateCause::react(),
    )
    .expect("callout delta");
    let waiting = commit(transition(&state, ExecutionInput::Private(first)).expect("callout plan"));
    assert_eq!(
        waiting
            .next_state()
            .status()
            .pending()
            .map(|(pending, _)| pending),
        Some(&pending)
    );

    let retry = PrivateDelta::from_record(
        waiting.next_state().execution_id(),
        PrivateRecord {
            seq: 1,
            after_position: 0,
            event: PrivateEvent::InputReceived {
                callout_index: 2,
                data: vec![0xFF],
                continuation_tag: Some(4),
            },
            effects: vec![PrivateEffect::RetryInput {
                reason: "try again".into(),
            }],
            draws: Vec::new(),
            fuel_used: 0,
            pending: None,
        },
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        PrivateContext::new(0),
        PrivateCause::resume(id, PendingKind::Callout),
    )
    .expect("retry delta");
    let retried = commit(
        transition(waiting.next_state(), ExecutionInput::Private(retry)).expect("retry plan"),
    );
    assert_eq!(
        retried
            .next_state()
            .status()
            .pending()
            .map(|(pending, _)| pending),
        Some(&pending)
    );
    assert!(matches!(
        retried.outbox(),
        [OutboxIntent { effect: DurableEffect::RequestCallout { pending, context }, .. }]
            if pending.id == id && context.is_empty()
    ));
}

#[test]
fn terminal_outcome_requires_matching_borsh_and_json_projections() {
    let fixture = fixture();
    let state = active_state(&fixture);
    let mut delta = shared_delta(
        &state,
        vec![PublicEffect::SessionEnd {
            outcome: vec![9, 8, 7],
        }],
        vec![1],
    );
    delta.terminal_outcome =
        Some(TerminalOutcome::new(vec![1, 2, 3], b"null".to_vec()).expect("outcome projection"));
    assert!(matches!(
        transition(&state, ExecutionInput::ProposeShared(delta)),
        Err(ProtocolError::OutcomeProjectionMismatch)
    ));
    assert!(matches!(
        TerminalOutcome::new(vec![9, 8, 7], b"not-json".to_vec()),
        Err(ProtocolError::InvalidOutcomeProjection(_))
    ));
}

#[test]
fn retrying_the_same_pre_state_reproduces_the_same_plan_identity() {
    let fixture = fixture();
    let state = active_state(&fixture);
    let input = ExecutionInput::ProposeShared(shared_delta(&state, Vec::new(), vec![1]));
    let first = commit(transition(&state, input.clone()).expect("first plan"));
    let retry = commit(transition(&state, input).expect("retry plan"));
    assert_eq!(first.id(), retry.id());
    assert_eq!(first.next_version(), retry.next_version());
    assert_eq!(first.next_state(), retry.next_state());
}

#[test]
fn terminal_proof_requires_certificate_then_atomic_publication() {
    let fixture = fixture();
    let state = pending_terminal(&fixture);
    let commitment = state.pending_terminal().unwrap().clone();
    let first = ParticipantTerminalSignature::new(
        fixture.peers[0],
        fixture.creator_bls.sign(&commitment.signing_bytes()),
    );
    let partial = commit(
        transition(&state, ExecutionInput::TerminalSignature(first))
            .expect("partial terminal certificate"),
    )
    .next_state()
    .clone();
    assert!(matches!(
        partial.status().receipt_work(),
        ReceiptWork::CollectSignatures
    ));
    let second = ParticipantTerminalSignature::new(
        fixture.peers[1],
        fixture.other_bls.sign(&commitment.signing_bytes()),
    );
    let certified = commit(
        transition(&partial, ExecutionInput::TerminalSignature(second))
            .expect("terminal certificate"),
    )
    .next_state()
    .clone();
    assert!(matches!(
        certified.status().receipt_work(),
        ReceiptWork::Assemble
    ));
    let public_state = active_state(&fixture);
    let public_plan = commit(
        transition(
            &public_state,
            ExecutionInput::ProposeShared(shared_delta(
                &public_state,
                vec![PublicEffect::SessionEnd {
                    outcome: vec![9, 8, 7],
                }],
                vec![1],
            )),
        )
        .expect("proposal"),
    );
    let public_commitment = public_plan
        .next_state()
        .pending_shared()
        .expect("proposal")
        .commitment()
        .clone();
    let first = ParticipantStepSignature::new(
        fixture.peers[0],
        public_commitment.step,
        fixture.creator_bls.sign(&public_commitment.signing_bytes()),
    );
    let partial_public = commit(
        transition(
            public_plan.next_state(),
            ExecutionInput::StepSignature(first),
        )
        .expect("first public signature"),
    );
    let second = ParticipantStepSignature::new(
        fixture.peers[1],
        public_commitment.step,
        fixture.other_bls.sign(&public_commitment.signing_bytes()),
    );
    let final_public = commit(
        transition(
            partial_public.next_state(),
            ExecutionInput::StepSignature(second),
        )
        .expect("second public signature"),
    );
    let public_entry = final_public
        .shared()
        .expect("public commit")
        .entry()
        .clone();
    let terminal = SessionTerminal {
        final_step: commitment.final_step,
        final_state: commitment.final_state,
        outcome_hash: commitment.outcome_hash,
        agreement: certified
            .terminal_certificate()
            .expect("terminal certificate")
            .agreement()
            .clone(),
    };
    let body = ReceiptBody::new(
        SessionHeader::new(
            fixture.activation.clone(),
            ReceiptTermination::Completed { terminal },
        ),
        vec![9, 8, 7],
        br#"{}"#.to_vec(),
        vec![public_entry],
    )
    .expect("receipt body");
    let mut fuel_trace = body.trace().to_vec();
    fuel_trace[0].fuel_used = 17;
    let fuel_body = ReceiptBody::new(
        body.header().clone(),
        body.outcome().to_vec(),
        body.params().to_vec(),
        fuel_trace,
    )
    .expect("fuel receipt body");
    let receipt_id = ReceiptId::derive_body(&body).expect("receipt id");
    assert_ne!(
        receipt_id,
        ReceiptId::derive_body(&fuel_body).expect("fuel-bound id")
    );
    validate_receipt_body(certified.binding(), &fuel_body).expect_err("fuel is signed evidence");
    assert!(ReceiptArtifact::new(fuel_body).is_err());
    let assembled = commit(
        transition(
            &certified,
            ExecutionInput::ReceiptBody(Box::new(body.clone())),
        )
        .expect("receipt body"),
    )
    .next_state()
    .clone();
    assert!(matches!(
        assembled.status().receipt_work(),
        ReceiptWork::Published
    ));
    assert_eq!(assembled.published_receipt_id(), Some(receipt_id));
    assert_eq!(assembled.lifecycle(), ExecLifecycle::Completed);
    let receipt = ReceiptArtifact::new(body).expect("canonical receipt");
    assert!(matches!(receipt, ReceiptArtifact::Receipt(_)));
    let bytes = receipt.encode().expect("encode");
    assert_eq!(ReceiptArtifact::decode(&bytes).expect("decode"), receipt);
    let mut tampered = bytes.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 1;
    assert!(ReceiptArtifact::decode(&tampered).is_err());
    let mut old_version = bytes;
    old_version[0] = 1;
    assert!(ReceiptArtifact::decode(&old_version).is_err());
    let json = serde_json::to_value(&receipt).expect("json");
    assert_eq!(
        serde_json::from_value::<ReceiptArtifact>(json.clone()).expect("JSON round trip"),
        receipt
    );
    let mut unknown_field = json.clone();
    unknown_field["extra"] = true.into();
    assert!(serde_json::from_value::<ReceiptArtifact>(unknown_field).is_err());
    let mut wrong_kind = json;
    wrong_kind["kind"] = "stop_report".into();
    assert!(serde_json::from_value::<ReceiptArtifact>(wrong_kind).is_err());
}

#[test]
fn terminal_progress_excludes_other_execution_inputs() {
    let fixture = fixture();
    let state = pending_terminal(&fixture);
    let private = PrivateDelta::from_record(
        state.execution_id(),
        PrivateRecord {
            seq: 0,
            after_position: 1,
            event: PrivateEvent::React,
            effects: Vec::new(),
            draws: Vec::new(),
            fuel_used: 0,
            pending: None,
        },
        LocalStateBytes::try_new(Vec::new()).expect("local state"),
        PrivateContext::new(0),
        PrivateCause::react(),
    )
    .expect("private delta");
    assert!(matches!(
        transition(&state, ExecutionInput::Private(private)),
        Err(ProtocolError::TerminalProofPending)
    ));
    assert!(matches!(
        transition(
            &state,
            ExecutionInput::ProposeShared(shared_delta(&state, Vec::new(), vec![2]))
        ),
        Err(ProtocolError::TerminalProofPending)
    ));
}

#[test]
fn receipt_codecs_reject_oversized_fields_and_unknown_versions() {
    let fixture = fixture();
    let header = SessionHeader::new(
        fixture.activation.clone(),
        ReceiptTermination::Stopped {
            cause: StopCause::Shared {
                kind: AbortKind::Abort,
                commitment: StepCommitment {
                    domain: STEP_COMMIT_DOMAIN,
                    session_id: fixture.activation.session_hash(),
                    step: 0,
                    entry_hash: [1; 32],
                    pre_state: StateHash([2; 32]),
                    post_state: StateHash([3; 32]),
                    link: CHAIN_START,
                },
                reason: "stopped".to_owned(),
            },
        },
    );
    assert!(matches!(
        ReceiptBody::new(
            header.clone(),
            vec![0; MAX_TERMINAL_OUTCOME_BYTES + 1],
            Vec::new(),
            Vec::new(),
        ),
        Err(ProtocolError::PayloadTooLarge {
            kind: "receipt outcome",
            ..
        })
    ));
    assert!(matches!(
        ReceiptBody::new(
            header.clone(),
            Vec::new(),
            vec![0; crate::MAX_PARAMS_LEN + 1],
            Vec::new(),
        ),
        Err(ProtocolError::PayloadTooLarge {
            kind: "receipt params",
            ..
        })
    ));

    let entry = TraceEntry {
        trace_version: TRACE_FORMAT_VERSION,
        step: 0,
        event: PublicEvent::SessionStarted {
            ensemble: Ensemble::from_peers(fixture.peers.to_vec()).expect("ensemble"),
        },
        effects: Vec::new(),
        pre_state: StateHash([1; 32]),
        post_state: StateHash([2; 32]),
        fuel_used: 0,
        witness: None,
        agreement: AggregateAttestation::empty(),
    };
    assert!(matches!(
        ReceiptBody::new(
            header.clone(),
            Vec::new(),
            Vec::new(),
            vec![entry; MAX_RECEIPT_TRACE_ENTRIES + 1],
        ),
        Err(ProtocolError::CollectionTooLarge {
            kind: "receipt trace entries",
            ..
        })
    ));

    let body = ReceiptBody::new(header.clone(), Vec::new(), Vec::new(), Vec::new())
        .expect("empty receipt body");
    let mut unknown_body_version = borsh::to_vec(&body).expect("body bytes");
    unknown_body_version[0] = 0xff;
    assert!(borsh::from_slice::<ReceiptBody>(&unknown_body_version).is_err());

    let mut hostile_outcome = vec![2u8, 2u8];
    hostile_outcome.extend(borsh::to_vec(body.header()).expect("header bytes"));
    hostile_outcome.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(ReceiptArtifact::decode(&hostile_outcome).is_err());

    let mut hostile_trace = vec![2u8, 2u8];
    hostile_trace.extend(borsh::to_vec(body.header()).expect("header bytes"));
    hostile_trace.extend_from_slice(&0u32.to_le_bytes());
    hostile_trace.extend_from_slice(&0u32.to_le_bytes());
    hostile_trace.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(ReceiptArtifact::decode(&hostile_trace).is_err());

    assert!(borsh::from_slice::<ReceiptArtifact>(&[0xff]).is_err());
}

#[test]
fn persisted_execution_enums_use_fixed_tags_and_reject_unknown_tags() {
    let timer_id = TimerId::from_bytes([7; 32]);
    let checks = [
        (
            borsh::to_vec(&ExecutionInput::Activate).expect("input tag"),
            0,
        ),
        (borsh::to_vec(&PrivateCause::react()).expect("cause tag"), 0),
        (
            borsh::to_vec(&TimerMutation::cancel(timer_id)).expect("timer tag"),
            1,
        ),
        (
            borsh::to_vec(&ExecutionStatus::Activating).expect("status tag"),
            0,
        ),
        (
            borsh::to_vec(&StopCause::Shared {
                kind: AbortKind::Abort,
                commitment: StepCommitment {
                    domain: STEP_COMMIT_DOMAIN,
                    session_id: SessionHash([2; 32]),
                    step: 0,
                    entry_hash: [1; 32],
                    pre_state: StateHash([3; 32]),
                    post_state: StateHash([4; 32]),
                    link: CHAIN_START,
                },
                reason: String::new(),
            })
            .expect("cause tag"),
            1,
        ),
        (
            borsh::to_vec(&DurableEffect::Notify {
                frame_id: FrameId::from_bytes([8; 32]),
                payload: Vec::new(),
            })
            .expect("effect tag"),
            2,
        ),
    ];
    for (encoded, tag) in checks {
        assert_eq!(encoded.first().copied(), Some(tag));
    }
    assert!(borsh::from_slice::<ExecutionInput>(&[0xff]).is_err());
    assert!(borsh::from_slice::<PrivateCause>(&[0xff]).is_err());
    assert!(borsh::from_slice::<TimerMutation>(&[0xff]).is_err());
    assert!(borsh::from_slice::<ExecutionStatus>(&[0xff]).is_err());
    assert!(borsh::from_slice::<TerminalProof>(&[0xff]).is_err());
    assert!(borsh::from_slice::<StopCause>(&[0xff]).is_err());
    assert!(borsh::from_slice::<DurableEffect>(&[0xff]).is_err());
}

#[cfg(feature = "performance-tracing")]
mod performance_tracing {
    use std::fmt::Debug;
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::{Event, Level, Subscriber};
    use tracing_subscriber::layer::{Context, Layer};
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::registry::LookupSpan;

    use super::*;

    #[derive(Debug, Clone)]
    struct RecordedEvent {
        target: String,
        level: Level,
        fields: Vec<(String, String)>,
    }

    #[derive(Clone, Default)]
    struct Recorder {
        events: Arc<Mutex<Vec<RecordedEvent>>>,
    }

    impl Recorder {
        fn install(&self) -> tracing::subscriber::DefaultGuard {
            let guard =
                tracing::subscriber::set_default(tracing_subscriber::registry().with(self.clone()));
            tracing::callsite::rebuild_interest_cache();
            guard
        }

        fn events(&self) -> Vec<RecordedEvent> {
            self.events
                .lock()
                .expect("recorder is not poisoned")
                .clone()
        }
    }

    #[derive(Default)]
    struct Fields {
        values: Vec<(String, String)>,
    }

    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
            self.values
                .push((field.name().to_owned(), format!("{value:?}")));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.values
                .push((field.name().to_owned(), value.to_owned()));
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            self.values
                .push((field.name().to_owned(), value.to_string()));
        }

        fn record_i64(&mut self, field: &Field, value: i64) {
            self.values
                .push((field.name().to_owned(), value.to_string()));
        }

        fn record_bool(&mut self, field: &Field, value: bool) {
            self.values
                .push((field.name().to_owned(), value.to_string()));
        }
    }

    impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Recorder {
        fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            self.events
                .lock()
                .expect("recorder is not poisoned")
                .push(RecordedEvent {
                    target: event.metadata().target().to_owned(),
                    level: *event.metadata().level(),
                    fields: fields.values,
                });
        }
    }

    #[test]
    fn performance_events_are_targeted_and_redacted() {
        const TARGET: &str = "arena0::performance";
        const ALLOWED_FIELDS: &[&str] = &[
            "operation",
            "exec_id",
            "session_id",
            "version",
            "public_step",
            "input_kind",
            "encoded_size",
            "batch",
            "count",
            "success",
            "result_class",
            "elapsed_us",
        ];
        const FORBIDDEN_FIELD_PARTS: &[&str] = &[
            "payload",
            "param",
            "outcome",
            "context",
            "signature",
            "program",
            "private",
            "sql",
        ];
        const FORBIDDEN_VALUE: &str = "SECRET_PAYLOAD_PARAMS_OUTCOME_SIGNATURE";

        let recorder = Recorder::default();
        let _guard = recorder.install();
        let fixture = fixture();
        let state = active_state(&fixture);
        tracing::callsite::rebuild_interest_cache();
        let encoded = state.encode().expect("execution state encoding");
        ExecutionState::decode(&encoded).expect("execution state decoding");
        assert!(ExecutionState::decode(FORBIDDEN_VALUE.as_bytes()).is_err());
        assert!(
            transition(
                &state,
                ExecutionInput::InterruptTerminal(FORBIDDEN_VALUE.to_owned())
            )
            .is_err()
        );

        let terminal = pending_terminal(&fixture);
        let commitment = terminal
            .pending_terminal()
            .expect("terminal commitment")
            .clone();
        tracing::callsite::rebuild_interest_cache();
        let signature = ParticipantTerminalSignature::new(
            fixture.peers[0],
            fixture.creator_bls.sign(&commitment.signing_bytes()),
        );
        transition(&terminal, ExecutionInput::TerminalSignature(signature))
            .expect("terminal signature");

        let events = recorder.events();
        assert!(
            events.iter().any(|event| event.level == Level::DEBUG
                && event
                    .fields
                    .iter()
                    .any(|(name, value)| name == "operation" && value == "execution_state_decode")),
            "state decode event missing: {events:?}"
        );
        assert!(
            events.iter().any(|event| event.level == Level::DEBUG
                && event
                    .fields
                    .iter()
                    .any(|(name, value)| name == "operation" && value == "execution_reducer")),
            "reducer event missing: {events:?}"
        );
        assert!(
            events.iter().any(|event| event.level == Level::TRACE
                && event
                    .fields
                    .iter()
                    .any(|(name, value)| name == "operation" && value == "step_signature_apply")),
            "step signature event missing: {events:?}"
        );
        assert!(
            events.iter().any(|event| event.level == Level::TRACE
                && event.fields.iter().any(
                    |(name, value)| name == "operation" && value == "terminal_signature_apply"
                )),
            "terminal signature event missing: {events:?}"
        );
        assert!(
            events.iter().all(|event| {
                event.level != Level::DEBUG
                    || !event.fields.iter().any(|(name, value)| {
                        name == "input_kind"
                            && matches!(value.as_str(), "step_signature" | "terminal_signature")
                    })
            }),
            "signature application must remain trace-only: {events:?}"
        );

        for event in events {
            assert_eq!(event.target, TARGET);
            assert!(matches!(event.level, Level::DEBUG | Level::TRACE));
            for (name, value) in event.fields {
                assert!(
                    ALLOWED_FIELDS.contains(&name.as_str()),
                    "unexpected performance field {name:?}"
                );
                assert!(
                    !FORBIDDEN_FIELD_PARTS.iter().any(|part| name.contains(part)),
                    "forbidden performance field {name:?}"
                );
                assert!(
                    !value.contains(FORBIDDEN_VALUE),
                    "performance value leaked sentinel: {value:?}"
                );
            }
        }
    }
}
