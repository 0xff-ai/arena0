//! M3: two in-process executions play a full turn-based chess game (Scholar's
//! mate) bilaterally over `LocalTransport`; portable verification accepts both
//! traces. Exercises callouts derived after each agreed move and the final
//! broadcast before `SessionEnd` through the real runtime.

use arena0_protocol::{ColorDepth, Slot, Viewport};
use arena0_tests::arena::Arena;
use arena0_tests::wasm::program_wasm;

const STARTING_FEN: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

fn mono_viewport() -> Viewport {
    Viewport {
        width: 80,
        color: ColorDepth::Mono,
    }
}

#[tokio::test]
async fn chess_bilateral_scholars_mate_runs_and_verifies() {
    let wasm = program_wasm("chess");

    let mut arena = Arena::new();
    arena.program(wasm.clone()).participants(2);
    let mut run = arena.run().await;

    // The opening callout carries the starting position and the legal moves.
    let callout = run.callout(0).await;
    assert_eq!(callout.callout_index, 0);
    assert_eq!(callout.context["fen"], STARTING_FEN);
    assert!(
        !callout.context["legal_moves"]
            .as_str()
            .expect("legal moves render as text")
            .is_empty()
    );

    // An illegal move is rejected with the program's reason and the callout
    // stays open, so the same turn is answered below.
    let reason = run
        .expect_input(0)
        .respond_rejected(serde_json::to_vec(&"z9z9".to_string()).expect("encode UCI move"))
        .await;
    assert!(
        reason.contains("illegal move: z9z9"),
        "unexpected rejection reason: {reason}"
    );
    // A too-short move is rejected at the format boundary.
    let reason = run
        .expect_input(0)
        .respond_rejected(serde_json::to_vec(&"e2".to_string()).expect("encode UCI move"))
        .await;
    assert!(
        reason.contains("4-5 characters"),
        "unexpected rejection reason: {reason}"
    );

    // Scholar's mate: white (participant 0) checkmates on move 4. Half-moves
    // alternate white/black, so participant = index % 2.
    // Answers cross as JSON, matching what an agent would submit.
    // Each answered move is applied before the opponent is asked: the next
    // callout opening proves the turn advanced.
    let moves = ["e2e4", "e7e5", "f1c4", "b8c6", "d1h5", "g8f6", "h5f7"];
    for (i, mv) in moves.iter().enumerate().take(2) {
        run.expect_input(i % 2)
            .respond_bytes(serde_json::to_vec(&mv.to_string()).expect("encode UCI move as JSON"))
            .await;
        run.callout((i + 1) % 2).await;
    }

    // The board after 1. e4 e5 names the last move, renders both kings, and
    // carries no escape sequences in mono.
    let view = run.view(0, mono_viewport()).await;
    for text in view.slots.values() {
        assert!(!text.contains("\x1b["), "mono view contains SGR: {text:?}");
    }
    assert!(
        view.slots[&Slot::State].contains("Last move: e5"),
        "board should name the last move: {:?}",
        view.slots[&Slot::State]
    );
    assert!(view.slots[&Slot::State].contains("♔"));
    assert!(view.slots[&Slot::State].contains("♚"));

    for (i, mv) in moves.iter().enumerate().skip(2) {
        run.expect_input(i % 2)
            .respond_bytes(serde_json::to_vec(&mv.to_string()).expect("encode UCI move as JSON"))
            .await;
        if i + 1 < moves.len() {
            run.callout((i + 1) % 2).await;
        }
    }

    let outcomes = run.expect_completed_all().await;
    assert_eq!(
        outcomes[0], outcomes[1],
        "both participants must derive the identical outcome receipt"
    );

    assert_eq!(run.session_hash(0), run.session_hash(1));
    let verified = run.verify_all().expect("both traces must verify");
    for (verified, expected_outcome) in verified.into_iter().zip(&outcomes) {
        let arena0_verify::LightVerifiedTerminal::Completed { outcome_borsh } = verified.terminal
        else {
            panic!("checkmate must complete the session");
        };
        assert_eq!(&outcome_borsh, expected_outcome);
    }
}
