//! Three independent replicas agree on one bounded task assignment and verify
//! every producer receipt's portable proof.

use arena0_tests::arena::Arena;
use arena0_tests::wasm::program_wasm;
use arena0_verify::LightVerifiedTerminal;

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
    assert!(outcomes.iter().all(|outcome| outcome == &outcomes[0]));
    assert_eq!(run.session_hash(0), run.session_hash(1));
    assert_eq!(run.session_hash(1), run.session_hash(2));
    let verified = run
        .verify_all()
        .expect("all three contract-net receipts verify");

    for (participant, (verified, expected_outcome)) in
        verified.into_iter().zip(&outcomes).enumerate()
    {
        let LightVerifiedTerminal::Completed { outcome_borsh } = verified.terminal else {
            panic!("participant {participant}: expected completed receipt");
        };
        assert_eq!(&outcome_borsh, expected_outcome);
    }
}
