//! Greybox observations over the public Host execution boundary.
//!
//! These tests use a real loaded Wasm guest and a real SQLite execution
//! store. Peer frames enter through authenticated `LocalTransport` streams;
//! assertions observe durable traces, retryable transport decisions, and lifecycle
//! messages rather than a mutable sandbox mock.

use std::time::Duration;

use arena0_crypto::NodeKeys;
use arena0_node::{SessionMessage, SpawnedExec};
use arena0_protocol::{
    Event, ExecFrame, ExecId, FetchActivationTickets, FetchFrame, MessageId, NegotiationId, PeerId,
    PeerIdSource, SessionHash, StateHash, StepCommitment,
};
use arena0_tests::assert::wait_for_entry;
use arena0_tests::fixtures::{
    LIVE_EXECUTION_TIMEOUT, LiveExecution, complete_pending_shared, establish_live_session,
    ordering_program_wasm, provider, spawn_live_execution,
};
use arena0_transport::Transport;

const EXEC_ID: ExecId = ExecId([0xE0; 32]);
const NEGOTIATION_ID: NegotiationId = NegotiationId([0xD0; 32]);

fn cryptos() -> Vec<NodeKeys> {
    vec![provider(7), provider(8), provider(9)]
}

async fn harness(reject_shared: bool) -> LiveExecution {
    spawn_live_execution(
        ordering_program_wasm(reject_shared),
        cryptos(),
        NEGOTIATION_ID,
        EXEC_ID,
        br#"{}"#.to_vec(),
    )
    .await
}

async fn establish_session(execution: &LiveExecution) {
    let participants = cryptos();
    establish_live_session(execution, &participants).await;
}

fn message_frame(
    session_hash: SessionHash,
    source: PeerId,
    sequence: u64,
    prestate: StateHash,
    payload: u8,
) -> ExecFrame {
    let data = vec![payload];
    ExecFrame::Message {
        message_id: MessageId::derive(session_hash, source, sequence, prestate, prestate, &data),
        seq: sequence,
        prestate,
        data,
        poststate: prestate,
    }
}

async fn send_message(execution: &LiveExecution, participant: usize, frame: ExecFrame) {
    execution
        .participant_stream(participant)
        .send_exec(&frame)
        .await
        .expect("send message frame");
}

async fn terminal_reason(spawned: &mut SpawnedExec) -> String {
    let deadline = tokio::time::Instant::now() + LIVE_EXECUTION_TIMEOUT;
    loop {
        let message = tokio::time::timeout_at(deadline, spawned.message_rx.recv())
            .await
            .expect("timed out waiting for terminal event");
        match message {
            Some(SessionMessage::Failed { reason }) => return reason,
            Some(SessionMessage::Aborted { .. }) => {
                panic!("runtime failure reported as a program abort")
            }
            Some(_) => {}
            None => panic!("execution event channel closed"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contradictory_abort_cursors_leave_receiver_serving_other_peers() {
    let execution = harness(false).await;
    establish_session(&execution).await;
    let state = execution
        .store_handle
        .load_execution(EXEC_ID)
        .await
        .unwrap()
        .unwrap();
    let cursor = state.step_cursor();
    let mut wrong_state = cursor.state_hash();
    wrong_state.0[0] ^= 1;
    let mut wrong_chain = cursor.chain_hash();
    wrong_chain[0] ^= 1;
    let origin = execution.peer_ids[1];
    let keys = cryptos()
        .into_iter()
        .find(|keys| keys.peer_id() == origin)
        .unwrap();
    for coordinate in [
        arena0_protocol::StepCursor::new(cursor.next_step(), wrong_state, cursor.chain_hash()),
        arena0_protocol::StepCursor::new(cursor.next_step(), cursor.state_hash(), wrong_chain),
    ] {
        let unsigned = arena0_protocol::AbortOccurrence::unsigned(
            execution.session_hash,
            origin,
            arena0_protocol::AbortKind::Abort,
            1,
            "stop",
            coordinate,
        )
        .unwrap();
        let signature = keys.sign(&unsigned.signing_bytes().unwrap());
        let frame = ExecFrame::Abort {
            occurrence: unsigned.with_signature(signature).unwrap(),
        };
        assert!(matches!(
            execution.participant_stream(1).send_exec(&frame).await,
            Err(arena0_transport::TransportError::ExecConflict)
        ));
        let unchanged = execution
            .store_handle
            .load_execution(EXEC_ID)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unchanged.step_cursor(), cursor);
        assert!(!unchanged.status().is_terminal());
    }
    // A different participant can still stop at the actual agreed cursor.
    let origin = execution.peer_ids[2];
    let keys = cryptos()
        .into_iter()
        .find(|keys| keys.peer_id() == origin)
        .unwrap();
    let unsigned = arena0_protocol::AbortOccurrence::unsigned(
        execution.session_hash,
        origin,
        arena0_protocol::AbortKind::Abort,
        1,
        "stop",
        cursor,
    )
    .unwrap();
    let signature = keys.sign(&unsigned.signing_bytes().unwrap());
    execution
        .participant_stream(2)
        .send_exec(&ExecFrame::Abort {
            occurrence: unsigned.with_signature(signature).unwrap(),
        })
        .await
        .unwrap();
    assert!(
        execution
            .store_handle
            .load_execution(EXEC_ID)
            .await
            .unwrap()
            .unwrap()
            .status()
            .is_terminal()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn future_message_is_retried_after_public_head_catches_up() {
    let execution = harness(false).await;
    establish_session(&execution).await;
    let source = execution.peer_ids[1];

    let future = message_frame(
        execution.session_hash,
        source,
        2,
        execution.initial_state,
        0xA2,
    );
    assert!(matches!(
        execution.participant_stream(1).send_exec(&future).await,
        Err(arena0_transport::TransportError::ExecNotYet)
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        execution
            .store_handle
            .read_trace(execution.exec_id, 0, u64::MAX)
            .await
            .expect("read trace")
            .len(),
        1,
        "future frame cannot advance the public head"
    );

    send_message(
        &execution,
        1,
        message_frame(
            execution.session_hash,
            source,
            1,
            execution.initial_state,
            0xA1,
        ),
    )
    .await;
    complete_pending_shared(&execution, &cryptos()).await;
    let _ = wait_for_entry(&execution.store_handle, execution.exec_id, 1).await;
    send_message(&execution, 1, future).await;
    complete_pending_shared(&execution, &cryptos()).await;
    let trace = wait_for_entry(&execution.store_handle, execution.exec_id, 2).await;
    let payloads = trace
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::MessageReceived { msg, .. } => msg.first().copied(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(payloads, vec![0xA1, 0xA2]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_shared_message_fails_without_advancing_the_public_trace() {
    let mut execution = harness(true).await;
    establish_session(&execution).await;
    let source = execution.peer_ids[1];
    send_message(
        &execution,
        1,
        message_frame(
            execution.session_hash,
            source,
            1,
            execution.initial_state,
            0x01,
        ),
    )
    .await;
    let reason = terminal_reason(&mut execution.spawned).await;
    assert!(
        reason.starts_with("diverged at step 1: program rejected the writer message"),
        "{reason}"
    );
    let trace = execution
        .store_handle
        .read_trace(execution.exec_id, 0, u64::MAX)
        .await
        .expect("read trace");
    assert_eq!(
        trace.len(),
        1,
        "rejected call does not append a trace entry"
    );
    let state = execution
        .store_handle
        .load_execution(execution.exec_id)
        .await
        .expect("load failed execution")
        .expect("execution");
    assert!(state.status().is_terminal());
    assert_eq!(state.agreed_step(), 1);
    assert!(state.pending_shared().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_position_does_not_replace_the_first_public_entry() {
    let execution = harness(false).await;
    establish_session(&execution).await;
    let source = execution.peer_ids[1];
    send_message(
        &execution,
        1,
        message_frame(
            execution.session_hash,
            source,
            1,
            execution.initial_state,
            0x10,
        ),
    )
    .await;
    complete_pending_shared(&execution, &cryptos()).await;
    let _ = wait_for_entry(&execution.store_handle, execution.exec_id, 1).await;
    send_message(
        &execution,
        1,
        message_frame(
            execution.session_hash,
            source,
            1,
            execution.initial_state,
            0x20,
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let trace = execution
        .store_handle
        .read_trace(execution.exec_id, 0, u64::MAX)
        .await
        .expect("read trace");
    let payloads = trace
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::MessageReceived { msg, .. } => msg.first().copied(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(payloads, vec![0x10]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_participant_frame_is_rejected_without_mutating_execution() {
    let execution = harness(false).await;
    establish_session(&execution).await;
    let outsider = *execution.transports[3].peer_id();
    let send = execution.transports[3]
        .open_exec(&execution.peer_ids[0], execution.session_hash)
        .await
        .expect("open outsider stream");
    let result = send
        .send_exec(&message_frame(
            execution.session_hash,
            outsider,
            1,
            execution.initial_state,
            0xEE,
        ))
        .await;
    assert!(
        result.is_err(),
        "an unauthenticated source cannot be acknowledged"
    );
    assert_eq!(
        execution
            .store_handle
            .load_execution(execution.exec_id)
            .await
            .unwrap()
            .unwrap()
            .agreed_step(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_step_signature_is_consumed_without_changing_public_trace() {
    let execution = harness(false).await;
    establish_session(&execution).await;
    let commitment = StepCommitment {
        domain: arena0_protocol::STEP_COMMIT_DOMAIN,
        session_id: execution.session_hash,
        step: 0,
        entry_hash: [0xA5; 32],
        pre_state: execution.initial_state,
        post_state: execution.initial_state,
        link: [0xB6; 32],
    };
    let send = execution.transports[1]
        .open_exec(&execution.peer_ids[0], execution.session_hash)
        .await
        .expect("open signature stream");
    // Stale signatures cannot create another trace entry.
    let _ = send
        .send_exec(&ExecFrame::StepSignature {
            commitment,
            signature: arena0_crypto::BlsSignature([0x11; 48]),
        })
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        execution
            .store_handle
            .read_trace(execution.exec_id, 0, u64::MAX)
            .await
            .expect("read trace")
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn participant_stream_closure_allows_reconnection_and_progress() {
    let execution = harness(false).await;
    establish_session(&execution).await;
    let send = execution.transports[1]
        .open_exec(&execution.peer_ids[0], execution.session_hash)
        .await
        .unwrap();
    drop(send);
    let send = execution.transports[1]
        .open_exec(&execution.peer_ids[0], execution.session_hash)
        .await
        .unwrap();
    let frame = message_frame(
        execution.session_hash,
        execution.peer_ids[1],
        1,
        execution.initial_state,
        0x42,
    );
    send.send_exec(&frame).await.expect("apply after reconnect");
    complete_pending_shared(&execution, &cryptos()).await;
    let trace = wait_for_entry(&execution.store_handle, execution.exec_id, 1).await;
    assert!(matches!(&trace[1].event, Event::MessageReceived { msg, .. } if msg == &[0x42]));
}

#[tokio::test]
async fn debug_fetch_router_delivers_registered_request() {
    let execution = harness(false).await;
    let mut requests = execution
        .host
        .register_fetch_handler(execution.session_hash);
    let send = execution.transports[1]
        .open_fetch(&execution.peer_ids[0])
        .await
        .expect("open fetch stream");
    send.send_fetch(&FetchFrame::FetchActivationTickets(
        FetchActivationTickets {
            session_hash: execution.session_hash,
        },
    ))
    .await
    .expect("send fetch request");
    let (recv, frame) = tokio::time::timeout(Duration::from_secs(2), requests.recv())
        .await
        .expect("fetch router timeout")
        .expect("fetch registry closed");
    assert_eq!(*recv.remote_peer(), execution.peer_ids[1]);
    assert!(matches!(frame, FetchFrame::FetchActivationTickets(_)));
}

#[tokio::test]
async fn a_silent_participant_does_not_stall_another_delivery_lane() {
    let execution = arena0_tests::fixtures::spawn_live_execution_with_delivery(
        ordering_program_wasm(false),
        cryptos(),
        NEGOTIATION_ID,
        EXEC_ID,
        br#"{}"#.to_vec(),
        false,
    )
    .await;
    let silent = tokio::time::timeout(
        Duration::from_secs(2),
        execution.transports[1].accept_exec(),
    )
    .await
    .unwrap()
    .unwrap()
    .into_parts()
    .1;
    let held = silent.recv_exec().await.unwrap();
    assert!(matches!(held.frame(), ExecFrame::StepSignature { .. }));
    let responsive = tokio::time::timeout(
        Duration::from_secs(1),
        execution.transports[2].accept_exec(),
    )
    .await
    .expect("responsive lane must not wait for the silent lane's five-second deadline")
    .unwrap()
    .into_parts()
    .1;
    let delivery = tokio::time::timeout(Duration::from_secs(1), responsive.recv_exec())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivery.frame(), held.frame());
    delivery.acknowledge().unwrap();
    drop(held);
}

#[tokio::test]
async fn nonterminal_conflict_receipt_does_not_stop_progress_on_other_lane() {
    let execution = arena0_tests::fixtures::spawn_live_execution_with_delivery(
        ordering_program_wasm(false),
        cryptos(),
        NEGOTIATION_ID,
        EXEC_ID,
        br#"{}"#.to_vec(),
        false,
    )
    .await;
    let conflicting = execution.transports[1]
        .accept_exec()
        .await
        .unwrap()
        .into_parts()
        .1;
    let delivery = conflicting.recv_exec().await.unwrap();
    assert!(matches!(delivery.frame(), ExecFrame::StepSignature { .. }));
    delivery
        .reject(arena0_transport::ExecDeliveryRejection::Conflict)
        .unwrap();
    // Keep the honest lane responsive and observe a later certificate on it.
    let responsive = execution.transports[2]
        .accept_exec()
        .await
        .unwrap()
        .into_parts()
        .1;
    let reader = tokio::spawn(async move {
        loop {
            let delivery = responsive.recv_exec().await.unwrap();
            let later = matches!(delivery.frame(), ExecFrame::StepCertificate { certificate } if certificate.commitment().step == 1);
            delivery.acknowledge().unwrap();
            if later {
                return responsive;
            }
        }
    });
    establish_session(&execution).await;
    send_message(
        &execution,
        1,
        message_frame(
            execution.session_hash,
            execution.peer_ids[1],
            1,
            execution.initial_state,
            0xA1,
        ),
    )
    .await;
    complete_pending_shared(&execution, &cryptos()).await;
    let _ = wait_for_entry(&execution.store_handle, EXEC_ID, 1).await;
    let _responsive = tokio::time::timeout(Duration::from_secs(2), reader)
        .await
        .expect("honest lane receives the later certificate")
        .unwrap();
    let state = execution
        .store_handle
        .load_execution(EXEC_ID)
        .await
        .unwrap()
        .unwrap();
    assert!(!state.status().is_terminal());
}

#[tokio::test]
async fn forwarded_abort_and_rejected_terminal_lane_leave_honest_peer_confirmed() {
    let execution = arena0_tests::fixtures::spawn_live_execution_with_delivery(
        ordering_program_wasm(false),
        cryptos(),
        NEGOTIATION_ID,
        EXEC_ID,
        br#"{}"#.to_vec(),
        false,
    )
    .await;
    let mut receivers = Vec::new();
    for index in [1, 2] {
        let transport = execution.transports[index].clone();
        receivers.push(tokio::spawn(async move {
            let recv = transport.accept_exec().await.unwrap().into_parts().1;
            loop {
                let delivery = recv.recv_exec().await.unwrap();
                if matches!(delivery.frame(), ExecFrame::Abort { .. }) {
                    return (recv, delivery);
                }
                delivery.acknowledge().unwrap();
            }
        }));
    }
    establish_session(&execution).await;
    let state = execution
        .store_handle
        .load_execution(EXEC_ID)
        .await
        .unwrap()
        .unwrap();
    let origin = execution.peer_ids[1];
    let keys = cryptos()
        .into_iter()
        .find(|keys| keys.peer_id() == origin)
        .unwrap();
    let unsigned = arena0_protocol::AbortOccurrence::unsigned(
        execution.session_hash,
        origin,
        arena0_protocol::AbortKind::Abort,
        1,
        "stop",
        state.step_cursor(),
    )
    .unwrap();
    let signature = keys.sign(&unsigned.signing_bytes().unwrap());
    let frame = ExecFrame::Abort {
        occurrence: unsigned.with_signature(signature).unwrap(),
    };
    // Participant two forwards participant one's authenticated occurrence.
    execution
        .participant_stream(2)
        .send_exec(&frame)
        .await
        .unwrap();
    let (rejected_stream, rejected) =
        tokio::time::timeout(Duration::from_secs(2), receivers.remove(0))
            .await
            .unwrap()
            .unwrap();
    let (_responsive_stream, accepted) =
        tokio::time::timeout(Duration::from_secs(2), receivers.remove(0))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(rejected.frame(), &frame);
    assert_eq!(accepted.frame(), &frame);
    rejected
        .reject(arena0_transport::ExecDeliveryRejection::Rejected)
        .unwrap();
    accepted.acknowledge().unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let (_, end) = execution.store_handle.execution_end(execution.session_hash).await.unwrap().unwrap();
            if matches!(end, arena0_protocol::EndPhase::Ending { ref unconfirmed } if unconfirmed.len() == 1 && unconfirmed.contains(&origin)) { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("honest peer confirms despite the other peer's rejection");
    assert!(
        matches!(
            tokio::time::timeout(Duration::from_millis(150), rejected_stream.recv_exec()).await,
            Err(_) | Ok(Err(arena0_transport::TransportError::ConnectionClosed))
        ),
        "no more frames on the rejected stream"
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(150),
            execution.transports[1].accept_exec()
        )
        .await
        .is_err(),
        "no replacement stream for the rejecting peer this run"
    );
}
