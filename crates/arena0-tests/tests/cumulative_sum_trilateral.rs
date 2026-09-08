//! M4: three in-process executions run the cumulative-sum program as an explicit
//! local ensemble. The path exercises N-party broadcast routing, the entropy
//! record/replay path, and the all-ensemble session-end gate; every node
//! converges on the same total and their shared canonical receipt verifies.

use std::time::Duration;

use arena0_tests::arena::Arena;
use arena0_tests::fixtures::encode_params;
use arena0_tests::wasm::program_wasm;
use arena0_verify::{VerifiedTerminal, verify_full};

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

    let outcomes = run.expect_completed_all().await;
    run.wait_all_receipts().await;
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

    // ponytail: assert canonical bytes/IDs before the single replay below.
    let (_canonical_receipt, canonical_bytes) = run.assert_canonical_receipt_equality();
    let verified =
        verify_full(&wasm, &canonical_bytes).expect("canonical cumulative-sum receipt verifies");
    let VerifiedTerminal::Completed { outcome_borsh, .. } = verified.terminal else {
        panic!("canonical receipt must be completed");
    };
    assert_eq!(&outcome_borsh, &outcomes[0]);
}
