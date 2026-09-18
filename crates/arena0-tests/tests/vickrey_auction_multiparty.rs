//! Three independent replicas complete a sealed-bid auction and verify every
//! locally produced artifact's portable proof.

use arena0_tests::arena::Arena;
use arena0_tests::wasm::program_wasm;
use arena0_verify::LightVerifiedTerminal;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vickrey_auction_runs_and_every_producer_verifies() {
    let wasm = program_wasm("vickrey_auction");

    let params = serde_json::to_vec(&serde_json::json!({
        "item": "launch lot",
        "reserve": 50,
    }))
    .expect("auction params encode as JSON");
    let mut arena = Arena::new();
    arena.program(wasm.clone()).participants(3).params(params);
    let mut run = arena.run().await;

    run.expect_input(1)
        .respond_bytes(serde_json::to_vec(&120_u64).expect("bid encodes as JSON"))
        .await;
    run.expect_input(2)
        .respond_bytes(serde_json::to_vec(&120_u64).expect("bid encodes as JSON"))
        .await;

    let outcomes = run.expect_completed_all().await;
    assert!(outcomes.iter().all(|outcome| outcome == &outcomes[0]));
    assert_eq!(run.session_hash(0), run.session_hash(1));
    assert_eq!(run.session_hash(1), run.session_hash(2));
    let verified = run.verify_all().expect("all three auction receipts verify");

    for i in 1..run.node_count() {
        assert_eq!(
            run.receipt_bytes(0),
            run.receipt_bytes(i),
            "canonical auction receipt including joint randomness"
        );
        assert_eq!(run.receipt(0).receipt_id(), run.receipt(i).receipt_id());
    }

    for (participant, (verified, expected_outcome)) in
        verified.into_iter().zip(&outcomes).enumerate()
    {
        let LightVerifiedTerminal::Completed { outcome_borsh } = verified.terminal else {
            panic!("participant {participant}: expected completed receipt");
        };
        assert_eq!(&outcome_borsh, expected_outcome);
    }
}
