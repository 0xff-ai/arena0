//! M4: three in-process executions run the cumulative-sum program as an explicit
//! local ensemble. The path exercises N-party broadcast routing, the entropy
//! record path, and the all-ensemble session-end gate; every node
//! converges on the same total and every trace verifies.

use std::time::Duration;

use arena0_protocol::{ColorDepth, Slot, Viewport};
use arena0_tests::arena::Arena;
use arena0_tests::fixtures::encode_params;
use arena0_tests::wasm::program_wasm;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cumulative_sum_trilateral_runs_and_verifies() {
    let wasm = program_wasm("cumulative_sum");

    let mut arena = Arena::new();
    arena
        .program(wasm.clone())
        .participants(3)
        .timeout(Duration::from_secs(60))
        .params(encode_params(3));
    let mut run = arena.run().await;

    // The contributions table renders every seat and the running total in both
    // color depths, with no escape sequences in mono.
    let view = run
        .view(
            0,
            Viewport {
                width: 80,
                color: ColorDepth::Ansi16,
            },
        )
        .await;
    let state = view.slots[&Slot::State].clone();
    assert!(state.contains("Contributions"));
    assert!(state.contains("P0"));
    assert!(state.contains("P1"));
    assert!(state.contains("P2"));
    assert!(state.contains("Total ["));
    assert!(view.slots[&Slot::Agents].contains("P0"));
    assert!(view.slots[&Slot::StatusBar].contains("target 3000"));
    let mono = run
        .view(
            0,
            Viewport {
                width: 80,
                color: ColorDepth::Mono,
            },
        )
        .await;
    for text in mono.slots.values() {
        assert!(!text.contains("\x1b["), "mono view contains SGR: {text:?}");
    }
    assert!(mono.slots[&Slot::State].contains("Contributions"));

    let outcomes = run.expect_completed_all().await;
    // All three nodes derive the identical total, and the same session id.
    assert_eq!(
        outcomes[0], outcomes[1],
        "nodes 0 and 1 disagree on the total"
    );
    assert_eq!(
        outcomes[1], outcomes[2],
        "nodes 1 and 2 disagree on the total"
    );
    assert_eq!(run.session_hash(0), run.session_hash(1));
    assert_eq!(run.session_hash(1), run.session_hash(2));

    run.verify_all().expect("all three traces must verify");
}
