//! Receive-side back-pressure through the public Host, transport, and store
//! boundaries. A future message receives NotYet and cannot move the public
//! cursor until its sender retries after the missing edge is available.

use std::time::Duration;

use arena0_crypto::NodeKeys;
use arena0_protocol::{ExecId, NegotiationId};
use arena0_tests::assert::wait_for_entry;
use arena0_tests::fixtures::{
    complete_pending_shared, establish_live_session, message_frame, ordering_program_wasm,
    provider, spawn_live_execution,
};
use arena0_transport::Transport;

const EXEC_ID: ExecId = ExecId([0xB0; 32]);
const NEGOTIATION_ID: NegotiationId = NegotiationId([0xC0; 32]);

fn cryptos() -> Vec<NodeKeys> {
    vec![provider(7), provider(8), provider(9)]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_claiming_unflushed_agreement_stays_behind_the_edge() {
    let participants = cryptos();
    let execution = spawn_live_execution(
        ordering_program_wasm(false),
        cryptos(),
        NEGOTIATION_ID,
        EXEC_ID,
        br#"{}"#.to_vec(),
    )
    .await;
    establish_live_session(&execution, &participants).await;
    let source = execution.peer_ids[1];

    let frame = message_frame(
        execution.session_hash,
        source,
        2,
        execution.initial_state,
        execution.agreed_link().await,
        7,
    );
    assert!(matches!(
        execution.participant_stream(1).send_exec(&frame).await,
        Err(arena0_transport::TransportError::ExecNotYet)
    ));
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
async fn ordered_message_passes_the_public_edge() {
    let participants = cryptos();
    let execution = spawn_live_execution(
        ordering_program_wasm(false),
        cryptos(),
        NegotiationId([0xC1; 32]),
        ExecId([0xB1; 32]),
        br#"{}"#.to_vec(),
    )
    .await;
    establish_live_session(&execution, &participants).await;
    let source = execution.peer_ids[1];

    execution
        .send_from(
            1,
            &message_frame(
                execution.session_hash,
                source,
                1,
                execution.initial_state,
                execution.agreed_link().await,
                7,
            ),
        )
        .await;
    complete_pending_shared(&execution, &participants).await;
    let trace = wait_for_entry(&execution.store_handle, execution.exec_id, 1).await;
    let message = trace.iter().find_map(|entry| match &entry.event {
        arena0_protocol::StepEvent::Message { data, .. } => data.first().copied(),
        _ => None,
    });
    assert_eq!(message, Some(7));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_sender_frame_is_rejected_before_back_pressure_state() {
    let participants = cryptos();
    let execution = spawn_live_execution(
        ordering_program_wasm(false),
        cryptos(),
        NegotiationId([0xC2; 32]),
        ExecId([0xB2; 32]),
        br#"{}"#.to_vec(),
    )
    .await;
    establish_live_session(&execution, &participants).await;
    let outsider = *execution.transports[3].peer_id();
    let stream = execution.transports[3]
        .open_exec(&execution.peer_ids[0], execution.session_hash)
        .await
        .expect("open outsider stream");
    let result = stream
        .send_exec(&message_frame(
            execution.session_hash,
            outsider,
            1,
            execution.initial_state,
            execution.agreed_link().await,
            7,
        ))
        .await;
    assert!(result.is_err());
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
