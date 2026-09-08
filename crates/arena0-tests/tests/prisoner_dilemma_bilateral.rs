//! M3: two in-process executions run prisoner's dilemma bilaterally
//! over `LocalTransport`; their shared canonical receipt replay-verifies.

use arena0_tests::arena::Arena;
use arena0_tests::wasm::program_wasm;
use arena0_verify::{VerifiedTerminal, verify_full};

/// Borsh-encoded `Choice` variant index (`Cooperate = 0`, `Defect = 1`).
const DEFECT_JSON: &[u8] = b"\"Defect\"";

#[tokio::test]
async fn prisoner_dilemma_bilateral_runs_and_verifies() {
    let wasm = program_wasm("prisoner_dilemma");

    let mut arena = Arena::new();
    arena.program(wasm.clone()).participants(2);
    let mut run = arena.run().await;

    // Five rounds of mutual defection → cumulative payoff 5/5 → Draw.
    for _ in 0..5 {
        run.expect_input(0)
            .respond_bytes(DEFECT_JSON.to_vec())
            .await;
        run.expect_input(1)
            .respond_bytes(DEFECT_JSON.to_vec())
            .await;
    }

    let outcomes = run.expect_completed_all().await;
    run.wait_all_receipts().await;
    assert_eq!(
        outcomes[0], outcomes[1],
        "both participants must derive the identical outcome receipt"
    );

    assert_eq!(run.session_hash(0), run.session_hash(1));
    // ponytail: assert canonical bytes/IDs before the single replay below.
    let (_canonical_receipt, canonical_bytes) = run.assert_canonical_receipt_equality();
    let verified = verify_full(&wasm, &canonical_bytes)
        .expect("canonical prisoner's dilemma receipt verifies");
    let VerifiedTerminal::Completed {
        outcome_borsh,
        outcome_json,
    } = verified.terminal
    else {
        panic!("canonical receipt must be completed");
    };
    assert_eq!(&outcome_borsh, &outcomes[0]);
    let outcome: serde_json::Value = serde_json::from_slice(outcome_json.as_bytes())
        .expect("guest projects valid prisoner's dilemma JSON");
    assert_eq!(outcome, serde_json::json!({"Draw": {"scores": [5, 5]}}));
}
