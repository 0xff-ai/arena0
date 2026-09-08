//! Three replicas select a starting participant through N-party commit-reveal,
//! then count in single-writer round-robin order over `LocalTransport`.

use std::time::Duration;

use arena0_tests::arena::{Arena, ArenaProgress};
use arena0_tests::wasm::program_wasm;
use arena0_verify::{VerifiedTerminal, verify_full};

fn encode_params(target_size: u32, count_to: u32) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "target_size": target_size, "count_to": count_to }))
        .expect("params serialize as JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn three_peers_count_in_commit_reveal_selected_round_robin_order() {
    const PARTICIPANTS: usize = 3;
    const ROUNDS: usize = 2;
    const COUNT_TO: usize = PARTICIPANTS * ROUNDS;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();

    let wasm = program_wasm("sequential_count");

    let mut arena = Arena::new();
    arena
        .program(wasm.clone())
        .participants(PARTICIPANTS)
        .timeout(Duration::from_secs(90))
        .params(encode_params(PARTICIPANTS as u32, COUNT_TO as u32));
    let mut run = arena.run().await;

    let outcomes = run.expect_completed_all().await;
    run.wait_all_receipts().await;
    assert!(
        outcomes.iter().all(|outcome| outcome == &outcomes[0]),
        "all three participants must derive the same outcome"
    );

    // ponytail: assert canonical bytes/IDs before the single replay below.
    let (_canonical_receipt, canonical_bytes) = run.assert_canonical_receipt_equality();
    let verified = verify_full(&wasm, &canonical_bytes).expect("canonical trace must verify");

    let VerifiedTerminal::Completed {
        outcome_borsh,
        outcome_json,
    } = verified.terminal
    else {
        panic!("canonical receipt must be completed");
    };
    assert_eq!(&outcome_borsh, &outcomes[0]);
    let outcome: serde_json::Value = serde_json::from_slice(outcome_json.as_bytes())
        .expect("guest outcome projection is valid JSON");

    let counted = outcome
        .get("Counted")
        .expect("counter outcome has the Counted variant");
    assert_eq!(counted["final_count"], COUNT_TO as u32);
    let order = counted["order"].as_array().expect("order is a JSON array");
    let history = counted["history"]
        .as_array()
        .expect("history is a JSON array");
    assert_eq!(order.len(), PARTICIPANTS);
    assert_eq!(history.len(), COUNT_TO);
    for round in history.chunks_exact(PARTICIPANTS) {
        assert_eq!(round, order, "each round must follow the selected order");
    }

    let timeline = run.progress_timeline();
    for participant in 0..PARTICIPANTS {
        let progress = timeline
            .iter()
            .filter(|point| point.participant == participant)
            .map(|point| &point.progress)
            .collect::<Vec<_>>();
        assert_eq!(
            progress
                .iter()
                .filter(|event| matches!(event, ArenaProgress::NegotiationCommitted))
                .count(),
            1
        );
        assert_eq!(
            progress
                .iter()
                .filter(|event| matches!(event, ArenaProgress::SessionStarted))
                .count(),
            1
        );
        let certified_steps = progress
            .iter()
            .filter_map(|event| match event {
                ArenaProgress::CertifiedStep { step } => Some(*step),
                _ => None,
            })
            .collect::<Vec<_>>();
        let durable_steps = run
            .trace(participant)
            .iter()
            .map(|entry| entry.step)
            .collect::<Vec<_>>();
        assert_eq!(certified_steps, durable_steps);
        assert_eq!(
            progress
                .iter()
                .filter(|event| matches!(event, ArenaProgress::TerminalPublished))
                .count(),
            1
        );
    }
}
