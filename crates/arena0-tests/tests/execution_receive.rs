//! Greybox observations over the public Host execution boundary.
//!
//! These tests use a real loaded Wasm guest and a real SQLite execution
//! store. Peer frames enter through authenticated `LocalTransport` streams;
//! assertions observe durable traces, inbox projections, and lifecycle
//! messages rather than a mutable sandbox mock.

use std::time::Duration;

use arena0_crypto::NodeKeys;
use arena0_node::{SessionMessage, SpawnedExec};
use arena0_protocol::{
    ExecFrame, ExecId, FetchActivationTickets, FetchFrame, MessageId, NegotiationId, PeerId,
    SessionHash, StateHash, StepCommitment, WitnessCommitment,
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
    let witness = WitnessCommitment([0xCD; 32]);
    let data = vec![payload];
    ExecFrame::Message {
        message_id: MessageId::derive(session_hash, source, sequence, prestate, &data, witness),
        seq: sequence,
        prestate,
        data,
        witness,
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
async fn future_message_stays_durable_until_public_head_catches_up() {
    let execution = harness(false).await;
    establish_session(&execution).await;
    let source = execution.peer_ids[1];

    // Public position 2 is accepted by the transport/store but cannot apply
    // while the public cursor is at position 1.
    send_message(
        &execution,
        1,
        message_frame(
            execution.session_hash,
            source,
            2,
            execution.initial_state,
            0xA2,
        ),
    )
    .await;
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
    assert!(
        !execution
            .store_handle
            .list_pending_inbox(execution.exec_id, 16)
            .await
            .expect("pending inbox")
            .is_empty(),
        "the accepted future frame remains durable"
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
    // The first message opens a new public proposal after the initial quorum
    // is committed. Drive that quorum as well before expecting the already
    // durable future message to resolve.
    complete_pending_shared(&execution, &cryptos()).await;
    let trace = wait_for_entry(&execution.store_handle, execution.exec_id, 2).await;
    let payloads = trace
        .iter()
        .filter_map(|entry| match &entry.event {
            arena0_protocol::PublicEvent::MessageReceived { msg, .. } => msg.first().copied(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(payloads, vec![0xA1, 0xA2]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_shared_message_leaves_no_public_trace() {
    let execution = harness(true).await;
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
    tokio::time::sleep(Duration::from_millis(100)).await;
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
    assert!(
        execution
            .store_handle
            .list_pending_inbox(execution.exec_id, 16)
            .await
            .expect("pending inbox")
            .is_empty()
    );
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
            arena0_protocol::PublicEvent::MessageReceived { msg, .. } => msg.first().copied(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(payloads, vec![0x10]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_participant_frame_is_rejected_before_durable_inbox_acceptance() {
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
    assert!(
        execution
            .store_handle
            .list_pending_inbox(execution.exec_id, 16)
            .await
            .expect("pending inbox")
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_step_signature_is_consumed_without_changing_public_trace() {
    let execution = harness(false).await;
    establish_session(&execution).await;
    let commitment = StepCommitment {
        domain: arena0_protocol::STEP_COMMIT_DOMAIN,
        session_id: execution.session_hash,
        step: u64::MAX,
        entry_hash: [0xA5; 32],
        pre_state: execution.initial_state,
        post_state: execution.initial_state,
        link: [0xB6; 32],
    };
    let send = execution.transports[1]
        .open_exec(&execution.peer_ids[0], execution.session_hash)
        .await
        .expect("open signature stream");
    // The actor accepts the authenticated packet into SQLite, then consumes it
    // because no matching shared proposal exists. It cannot create a trace.
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
async fn participant_stream_closure_is_a_host_terminal_observation() {
    let mut execution = harness(false).await;
    establish_session(&execution).await;
    let send = execution.transports[1]
        .open_exec(&execution.peer_ids[0], execution.session_hash)
        .await
        .expect("open participant stream");
    drop(send);
    let reason = terminal_reason(&mut execution.spawned).await;
    assert!(reason.contains("execution stream") || reason.contains("connection"));
    let receipts = execution
        .store_handle
        .list_receipts(10)
        .await
        .expect("receipts");
    assert_eq!(
        receipts.len(),
        1,
        "failure must persist its stop report before notification"
    );
    assert_eq!(
        receipts[0].provenance,
        arena0_store::ReceiptProvenance::Produced
    );
    assert!(matches!(
        receipts[0].receipt,
        arena0_protocol::ReceiptArtifact::StopReport(_)
    ));
    let verified = arena0_verify::verify_full(
        &execution.wasm,
        &receipts[0].receipt.encode().expect("receipt encoding"),
    )
    .expect("failed execution receipt must replay");
    assert!(matches!(
        verified.terminal,
        arena0_verify::VerifiedTerminal::Stopped { .. }
    ));
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
