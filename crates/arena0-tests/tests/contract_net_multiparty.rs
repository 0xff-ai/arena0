//! Three independent replicas agree on one bounded task assignment and replay
//! their shared canonical receipt against the exact guest Wasm.

use arena0_tests::arena::Arena;
use arena0_tests::wasm::program_wasm;
use arena0_verify::{VerifiedTerminal, verify_full};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contract_net_runs_and_replays_canonical_receipt() {
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

    let outcomes = run.expect_completed_all().await;
    run.wait_all_receipts().await;
    assert!(outcomes.iter().all(|outcome| outcome == &outcomes[0]));
    assert_eq!(run.session_hash(0), run.session_hash(1));
    assert_eq!(run.session_hash(1), run.session_hash(2));

    // ponytail: assert canonical bytes/IDs before the single replay below.
    let (_canonical_receipt, canonical_bytes) = run.assert_canonical_receipt_equality();
    let verified = verify_full(&wasm, &canonical_bytes)
        .expect("canonical contract-net receipt replay-verifies");
    let VerifiedTerminal::Completed {
        outcome_borsh,
        outcome_json,
    } = verified.terminal
    else {
        panic!("canonical receipt must be completed");
    };
    assert_eq!(&outcome_borsh, &outcomes[0]);
    let outcome: serde_json::Value = serde_json::from_slice(outcome_json.as_bytes())
        .expect("guest projects valid contract-net JSON");
    let assignments = outcome["plan"]["assignments"]
        .as_array()
        .expect("outcome has an assignment array");
    assert_eq!(assignments.len(), 2);
    assert_eq!(assignments[0]["award"]["Assigned"]["worker"], 1);
    assert_eq!(assignments[0]["award"]["Assigned"]["cost"], 7);
    assert_eq!(assignments[1]["award"]["Assigned"]["worker"], 2);
    assert_eq!(assignments[1]["award"]["Assigned"]["cost"], 4);
}
