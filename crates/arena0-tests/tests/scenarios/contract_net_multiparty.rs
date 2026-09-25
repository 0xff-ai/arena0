//! Three independent replicas agree on one bounded task assignment and verify
//! every producer receipt's portable proof.

use arena0_protocol::{ColorDepth, Slot, Viewport};
use arena0_tests::arena::Arena;
use arena0_tests::wasm::program_wasm;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contract_net_runs_and_every_producer_verifies() {
    let wasm = program_wasm("contract_net");

    let params = serde_json::to_vec(&serde_json::json!({
        "target_size": 3,
        "tasks": [
            {"name": "compile", "capability": "rust"},
            {"name": "illustrate", "capability": "design"}
        ],
    }))
    .expect("contract-net params encode as JSON");
    let mut arena = Arena::new();
    arena.program(wasm.clone()).participants(3).params(params);
    let mut run = arena.run().await;

    // The first worker's callout carries the tasks and the offer bounds.
    let callout = run.callout(1).await;
    assert_eq!(callout.context["tasks"].as_array().expect("tasks").len(), 2);
    assert_eq!(callout.context["maximum_capacity"], 2);
    assert_eq!(callout.context["maximum_cost"], 1_000_000_000u64);

    // A duplicate bid is rejected with the program's reason and the callout
    // stays open, so the valid offer is answered below.
    let reason = run
        .expect_input(1)
        .respond_rejected(
            serde_json::to_vec(&serde_json::json!({
                "capabilities": ["rust"],
                "capacity": 1,
                "bids": [{"task": 0, "cost": 7}, {"task": 0, "cost": 8}]
            }))
            .expect("invalid offer encodes as JSON"),
        )
        .await;
    assert!(
        reason.contains("duplicate bid for task 0"),
        "unexpected rejection reason: {reason}"
    );

    run.expect_input(1)
        .respond_bytes(
            serde_json::to_vec(&serde_json::json!({
                "capabilities": ["rust"],
                "capacity": 1,
                "bids": [{"task": 0, "cost": 7}]
            }))
            .expect("worker one offer encodes as JSON"),
        )
        .await;
    // The second worker is asked only after the first offer is applied, so
    // its node renders the collected offer mid-game.
    let callout = run.callout(2).await;
    assert_eq!(callout.context["tasks"].as_array().expect("tasks").len(), 2);
    let view = run
        .view(
            2,
            Viewport {
                width: 96,
                color: ColorDepth::Mono,
            },
        )
        .await;
    assert!(view.slots[&Slot::Header].contains("Contract net - 2 tasks"));
    assert!(view.slots[&Slot::Agents].contains("P0: coordinator"));
    assert!(view.slots[&Slot::Agents].contains("P1: worker, capacity 1"));
    assert!(view.slots[&Slot::Agents].contains("P2: worker, offer pending"));
    assert!(view.slots[&Slot::State].contains("Tasks"));
    assert!(
        view.slots[&Slot::StatusBar].contains("collecting offers - 1/2"),
        "unexpected status bar: {:?}",
        view.slots[&Slot::StatusBar]
    );
    for text in view.slots.values() {
        assert!(!text.contains("\x1b["), "mono view contains SGR: {text:?}");
    }

    run.expect_input(2)
        .respond_bytes(
            serde_json::to_vec(&serde_json::json!({
                "capabilities": ["rust", "design"],
                "capacity": 2,
                "bids": [
                    {"task": 0, "cost": 9},
                    {"task": 1, "cost": 4}
                ]
            }))
            .expect("worker two offer encodes as JSON"),
        )
        .await;

    run.expect_agreed_completion().await;
}
