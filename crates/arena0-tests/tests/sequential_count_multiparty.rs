//! Five replicas select a starting participant through N-party commit-reveal,
//! then count in single-writer round-robin order over `LocalTransport`.

use std::time::Duration;

use arena0_protocol::{ColorDepth, Slot, Viewport};
use arena0_tests::arena::{Arena, ArenaProgress};
use arena0_tests::wasm::program_wasm;
use arena0_verify::LightVerifiedTerminal;

fn encode_params(target_size: u32, count_to: u32) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "target_size": target_size, "count_to": count_to }))
        .expect("params serialize as JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn five_peers_count_in_commit_reveal_selected_round_robin_order() {
    const PARTICIPANTS: usize = 5;
    const ROUNDS: usize = 3;
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

    // The program-state projection fills every slot in both color depths,
    // with no escape sequences in mono. The game selects its order and
    // counts without agent input, so the state slot shows either the
    // commit-reveal selection or the running count.
    let view = run
        .view(
            0,
            Viewport {
                width: 80,
                color: ColorDepth::Mono,
            },
        )
        .await;
    assert!(view.slots[&Slot::Header].contains("Sequential count - "));
    assert!(view.slots[&Slot::Header].contains(" of 15"));
    assert!(view.slots[&Slot::Agents].contains("P0"));
    assert!(view.slots[&Slot::Agents].contains("P4"));
    let state = view.slots[&Slot::State].clone();
    assert!(
        state.contains("commit-reveal") || state.contains("Count:"),
        "state should show order selection or the running count: {state:?}"
    );
    assert!(view.slots[&Slot::StatusBar].contains("5 participants"));
    assert!(
        view.slots.values().all(|text| !text.contains("\x1b[")),
        "mono view contains SGR"
    );

    let outcomes = run.expect_completed_all().await;
    assert!(
        outcomes.iter().all(|outcome| outcome == &outcomes[0]),
        "all five participants must derive the same outcome"
    );

    let verified = run
        .verify_all()
        .expect("all five round-robin traces must verify");

    for (participant, (verified, expected_outcome)) in
        verified.into_iter().zip(&outcomes).enumerate()
    {
        let LightVerifiedTerminal::Completed { outcome_borsh } = verified.terminal else {
            panic!("participant {participant}: expected completed receipt");
        };
        assert_eq!(&outcome_borsh, expected_outcome);
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
