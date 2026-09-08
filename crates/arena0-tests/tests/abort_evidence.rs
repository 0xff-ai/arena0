//! Authenticated stop evidence through a real Wasm-backed Host execution.
//!
//! A peer abort is accepted only after transport attribution, durable inbox
//! acceptance, and occurrence validation. The light verifier then checks the
//! resulting stopped receipt.

use arena0_crypto::NodeKeys;
use arena0_node::SessionMessage;
use arena0_protocol::{
    AbortKind, AbortOccurrence, ExecFrame, ExecId, NegotiationId, PeerIdSource, PublicCursor,
    StepCommitment,
};
use arena0_tests::fixtures::{
    LIVE_EXECUTION_TIMEOUT, establish_live_session, provider, spawn_live_execution,
};
use arena0_tests::wasm::program_wasm;
use arena0_verify::{LightVerifiedTerminal, verify_full, verify_light};

const EXEC_ID: ExecId = ExecId([0xA0; 32]);
const NEGOTIATION_ID: NegotiationId = NegotiationId([0xA1; 32]);

fn cryptos() -> Vec<NodeKeys> {
    vec![provider(7), provider(8)]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bilateral_peer_abort_publishes_a_stopped_receipt() {
    let wasm = program_wasm("rock_paper_scissors");
    let participants = cryptos();
    let mut execution = spawn_live_execution(
        wasm.clone(),
        cryptos(),
        NEGOTIATION_ID,
        EXEC_ID,
        br#"null"#.to_vec(),
    )
    .await;
    establish_live_session(&execution, &participants).await;
    let trace = execution
        .store_handle
        .read_trace(execution.exec_id, 0, u64::MAX)
        .await
        .expect("read session trace");
    let last = trace.last().expect("session boundary");
    let link = trace
        .iter()
        .fold(arena0_protocol::CHAIN_START, |link, entry| {
            StepCommitment::for_entry(execution.session_hash, entry, link).link_hash()
        });
    let cursor = PublicCursor::new(trace.len() as u64, last.post_state, link);
    let sender = execution.peer_ids[1];
    let sender_crypto = participants
        .iter()
        .find(|crypto| crypto.peer_id() == sender)
        .expect("attacker identity");
    let unsigned = AbortOccurrence::unsigned(
        execution.session_hash,
        sender,
        AbortKind::Fail,
        7,
        "peer reported divergence",
        cursor,
    )
    .expect("abort occurrence");
    let signature = sender_crypto.sign(&unsigned.signing_bytes().expect("abort bytes"));
    let occurrence = unsigned
        .with_signature(signature)
        .expect("signed abort occurrence");
    let stream = execution.participant_stream(1);
    stream
        .send_exec(&ExecFrame::Abort { occurrence })
        .await
        .expect("send authenticated abort");

    let deadline = tokio::time::Instant::now() + LIVE_EXECUTION_TIMEOUT;
    let mut receipt = None;
    let mut stopped = false;
    while !stopped || receipt.is_none() {
        match tokio::time::timeout_at(deadline, execution.spawned.message_rx.recv())
            .await
            .expect("timed out waiting for failed receipt")
        {
            Some(SessionMessage::Failed { reason }) => {
                assert_eq!(reason, "peer reported divergence");
                stopped = true;
            }
            Some(SessionMessage::Aborted { .. }) => {
                panic!("peer failure reported as a program abort")
            }
            Some(SessionMessage::ReceiptPublished { receipt: value }) => receipt = Some(value),
            Some(_) => {}
            None => panic!("execution event stream closed"),
        }
    }
    let receipt = receipt.expect("stopped receipt");
    assert!(matches!(
        receipt.body().termination(),
        arena0_protocol::ReceiptTermination::Stopped { .. }
    ));
    let bytes = receipt.encode().expect("encode stopped receipt");
    let light = verify_light(&bytes).expect("light verify stopped receipt");
    assert!(matches!(
        light.terminal,
        LightVerifiedTerminal::Stopped { .. }
    ));
    let full = verify_full(&wasm, &bytes).expect("full verify stopped receipt");
    assert!(matches!(
        full.terminal,
        arena0_verify::VerifiedTerminal::Stopped { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn shared_program_stop_produces_one_canonical_receipt() {
    let wasm = arena0_tests::fixtures::shared_stop_program_wasm();
    let mut arena = arena0_tests::arena::Arena::new();
    arena
        .program(wasm.clone())
        .participants(3)
        .params(b"null".to_vec());
    let mut run = arena.run().await;
    run.wait_all_terminal().await;
    run.wait_all_receipts().await;
    let expected = run.receipt_bytes(0);
    for i in 0..run.node_count() {
        assert!(matches!(
            run.receipt(i),
            arena0_protocol::ReceiptArtifact::Receipt(_)
        ));
        assert_eq!(run.receipt_bytes(i), expected);
        assert_eq!(run.receipt(i).receipt_id(), run.receipt(0).receipt_id());
        let verified = verify_full(&wasm, &run.receipt_bytes(i)).expect("shared stop replay");
        assert!(matches!(
            verified.terminal,
            arena0_verify::VerifiedTerminal::Stopped {
                cause: arena0_protocol::StopCause::Shared { .. }
            }
        ));
    }
}
