//! Three independent replicas complete a sealed-bid auction and replay their
//! shared canonical artifact against the exact guest Wasm.

use arena0_tests::arena::Arena;
use arena0_tests::wasm::program_wasm;
use arena0_verify::{VerifiedTerminal, verify_full};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vickrey_auction_runs_and_replays_canonical_receipt() {
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
    run.wait_all_receipts().await;
    assert!(outcomes.iter().all(|outcome| outcome == &outcomes[0]));
    assert_eq!(run.session_hash(0), run.session_hash(1));
    assert_eq!(run.session_hash(1), run.session_hash(2));

    // ponytail: assert canonical bytes/IDs before the single replay below.
    let (_canonical_receipt, canonical_bytes) = run.assert_canonical_receipt_equality();
    let verified =
        verify_full(&wasm, &canonical_bytes).expect("canonical auction receipt replay-verifies");
    let VerifiedTerminal::Completed {
        outcome_borsh,
        outcome_json,
    } = verified.terminal
    else {
        panic!("canonical receipt must be completed");
    };
    assert_eq!(&outcome_borsh, &outcomes[0]);
    let outcome: serde_json::Value =
        serde_json::from_slice(outcome_json.as_bytes()).expect("guest projects valid auction JSON");
    let sold = outcome
        .get("Sold")
        .expect("auction outcome has the Sold variant");
    assert_eq!(sold["bids"], serde_json::json!([120, 120]));
    assert!(matches!(sold["winner"].as_u64(), Some(1 | 2)));
    assert_eq!(sold["price"], 120);
}
