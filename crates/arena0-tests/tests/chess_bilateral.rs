//! M3: two in-process executions play a short turn-based chess game (Fool's
//! mate) bilaterally over `LocalTransport`; their shared canonical receipt
//! replay-verifies. Exercises the callout-await-inside-`on_message` continuation
//! path and the send-before-End ordering through the real runtime.

use arena0_tests::arena::Arena;
use arena0_tests::wasm::program_wasm;
use arena0_verify::{VerifiedTerminal, verify_full};

#[tokio::test]
async fn chess_bilateral_fools_mate_runs_and_verifies() {
    let wasm = program_wasm("chess");

    let mut arena = Arena::new();
    arena.program(wasm.clone()).participants(2);
    let mut run = arena.run().await;

    // Fool's mate: black (participant 1) checkmates on move 2. Half-moves
    // alternate white/black, so participant = index % 2.
    // Answers cross as JSON, matching what an agent would submit.
    let moves = ["f2f3", "e7e5", "g2g4", "d8h4"];
    for (i, mv) in moves.iter().enumerate() {
        run.expect_input(i % 2)
            .respond_bytes(serde_json::to_vec(&mv.to_string()).expect("encode UCI move as JSON"))
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
    let verified = verify_full(&wasm, &canonical_bytes).expect("canonical chess receipt verifies");
    let VerifiedTerminal::Completed {
        outcome_borsh,
        outcome_json,
    } = verified.terminal
    else {
        panic!("checkmate must complete the session");
    };
    assert_eq!(&outcome_borsh, &outcomes[0]);
    let outcome: serde_json::Value = serde_json::from_slice(outcome_json.as_bytes()).unwrap();
    assert_eq!(
        outcome,
        serde_json::json!({"Win": {"winner": 1, "reason": "Checkmate"}})
    );
}
