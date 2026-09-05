//! M3: two in-process executions play a full turn-based chess game (Scholar's
//! mate) bilaterally over `LocalTransport`; the replay verifier accepts both
//! traces. Exercises the callout-await-inside-`on_message` continuation path and
//! the send-before-End ordering through the real runtime.

use arena0_tests::arena::Arena;
use arena0_tests::wasm::program_wasm;

#[tokio::test]
async fn chess_bilateral_scholars_mate_runs_and_verifies() {
    let wasm = program_wasm("chess");

    let mut arena = Arena::new();
    arena.program(wasm.clone()).participants(2);
    let mut run = arena.run().await;

    // Scholar's mate: white (participant 0) checkmates on move 4. Half-moves
    // alternate white/black, so participant = index % 2.
    // Answers cross as JSON, matching what an agent would submit.
    let moves = ["e2e4", "e7e5", "f1c4", "b8c6", "d1h5", "g8f6", "h5f7"];
    for (i, mv) in moves.iter().enumerate() {
        run.expect_input(i % 2)
            .respond_bytes(serde_json::to_vec(&mv.to_string()).expect("encode UCI move as JSON"))
            .await;
    }

    let outcomes = run.expect_completed_all().await;
    assert_eq!(
        outcomes[0], outcomes[1],
        "both participants must derive the identical outcome receipt"
    );

    assert_eq!(run.session_hash(0), run.session_hash(1));
    let verified = run.verify_all(&wasm).expect("both traces must verify");
    for (verified, expected_outcome) in verified.into_iter().zip(&outcomes) {
        let arena0_verify::VerifiedTerminal::Completed {
            outcome_borsh,
            outcome_json,
        } = verified.terminal
        else {
            panic!("checkmate must complete the session");
        };
        assert_eq!(&outcome_borsh, expected_outcome);
        let outcome: serde_json::Value = serde_json::from_slice(outcome_json.as_bytes()).unwrap();
        assert_eq!(
            outcome,
            serde_json::json!({"Win": {"winner": 0, "reason": "Checkmate"}})
        );
    }
}
