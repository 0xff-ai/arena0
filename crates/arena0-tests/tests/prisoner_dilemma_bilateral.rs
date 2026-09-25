//! M3: two in-process executions run prisoner's dilemma bilaterally
//! over `LocalTransport`; portable verification accepts both traces.

use arena0_protocol::{ColorDepth, Slot, Viewport};
use arena0_tests::arena::Arena;
use arena0_tests::wasm::program_wasm;

/// Borsh-encoded `Choice` variant index (`Cooperate = 0`, `Defect = 1`).
const DEFECT_JSON: &[u8] = b"\"Defect\"";

#[tokio::test]
async fn prisoner_dilemma_bilateral_runs_and_verifies() {
    let wasm = program_wasm("prisoner_dilemma");

    let mut arena = Arena::new();
    arena.program(wasm.clone()).participants(2);
    let mut run = arena.run().await;

    // The opening callout names the round, the match length, and the history.
    let callout = run.callout(0).await;
    assert_eq!(callout.callout_index, 0);
    assert_eq!(callout.context["round"], 1);
    assert_eq!(callout.context["total_rounds"], 5);
    assert_eq!(callout.context["history"], "");

    // A choice outside the enum is rejected with the program's reason and the
    // callout stays open, so the same round is answered below.
    let reason = run
        .expect_input(0)
        .respond_rejected(br#""Cheat""#.to_vec())
        .await;
    assert!(
        reason.contains("unknown variant"),
        "unexpected rejection reason: {reason}"
    );

    // Round one of mutual defection.
    run.expect_input(0)
        .respond_bytes(DEFECT_JSON.to_vec())
        .await;
    run.expect_input(1)
        .respond_bytes(DEFECT_JSON.to_vec())
        .await;

    // The second round's callout opening proves the first round was applied
    // and recorded: mutual defection pays (1,1) and the game continues.
    let callout = run.callout(0).await;
    assert_eq!(callout.context["round"], 2);
    assert_eq!(callout.context["total_rounds"], 5);
    assert!(
        callout.context["history"]
            .as_str()
            .expect("history renders as text")
            .contains("(1,1)"),
        "history should record the round-one payoff: {}",
        callout.context["history"]
    );

    // Mid-game views render the matrix, the history, and the scores, with no
    // escape sequences in mono.
    let view = run
        .view(
            0,
            Viewport {
                width: 80,
                color: ColorDepth::Ansi16,
            },
        )
        .await;
    let state = view.slots[&Slot::State].clone();
    assert!(state.contains("Payoff matrix"));
    assert!(state.contains("3/3"));
    assert!(state.contains("0/5"));
    assert!(state.contains("History"));
    assert!(view.slots[&Slot::Agents].contains("P0"));
    assert!(view.slots[&Slot::Agents].contains("P1"));
    assert!(view.slots[&Slot::StatusBar].contains("round 2 of 5"));
    let mono = run
        .view(
            0,
            Viewport {
                width: 80,
                color: ColorDepth::Mono,
            },
        )
        .await;
    for text in mono.slots.values() {
        assert!(!text.contains("\x1b["), "mono view contains SGR: {text:?}");
    }
    assert!(mono.slots[&Slot::State].contains("Payoff matrix"));

    // Rounds two through five of mutual defection → payoff 5/5 → Draw.
    for _ in 0..4 {
        run.expect_input(0)
            .respond_bytes(DEFECT_JSON.to_vec())
            .await;
        run.expect_input(1)
            .respond_bytes(DEFECT_JSON.to_vec())
            .await;
    }

    let outcomes = run.expect_completed_all().await;
    assert_eq!(
        outcomes[0], outcomes[1],
        "both participants must derive the identical outcome receipt"
    );

    assert_eq!(run.session_hash(0), run.session_hash(1));
    run.verify_all().expect("both traces must verify");
}
