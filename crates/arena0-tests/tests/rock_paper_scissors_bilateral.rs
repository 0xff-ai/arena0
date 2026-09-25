//! Bilateral Wasm execution and receipt verification.
//!
//! The live path drives the generated guest through the public Host API, and
//! the verifier consumes the resulting authenticated receipt artifact.

use std::time::Duration;

use arena0_protocol::{ColorDepth, Slot, Viewport};
use arena0_tests::arena::Arena;
use arena0_tests::wasm::program_wasm;
use arena0_verify::LightVerifiedTerminal;

const CHOICE_ROCK: &[u8] = br#""Rock""#;
const CHOICE_SCISSORS: &[u8] = br#""Scissors""#;

async fn completed_run(wasm: &[u8]) -> arena0_tests::arena::Run {
    let mut arena = Arena::new();
    arena
        .program(wasm.to_vec())
        .participants(2)
        .timeout(Duration::from_secs(120));
    let mut run = arena.run().await;

    // The opening callout names the round, the match length, and the scores.
    let callout = run.callout(0).await;
    assert_eq!(callout.callout_index, 0);
    assert_eq!(callout.context["round"], 1);
    assert_eq!(callout.context["total_rounds"], 3);
    assert_eq!(callout.context["your_score"], 0);
    assert_eq!(callout.context["their_score"], 0);

    // A choice outside the enum is rejected with the program's reason and the
    // callout stays open, so the same round is answered below.
    let reason = run
        .expect_input(0)
        .respond_rejected(br#""Lizard""#.to_vec())
        .await;
    assert!(
        reason.contains("unknown variant"),
        "unexpected rejection reason: {reason}"
    );

    // Round one: participant 0 plays rock and participant 1 scissors.
    run.expect_input(0)
        .respond_bytes(CHOICE_ROCK.to_vec())
        .await;
    run.expect_input(1)
        .respond_bytes(CHOICE_SCISSORS.to_vec())
        .await;
    // The second round's callout opening proves the first round was applied:
    // rock beats scissors, so participant 0 leads 1-0 in round 2.
    let callout = run.callout(0).await;
    assert_eq!(callout.context["round"], 2);
    assert_eq!(callout.context["your_score"], 1);
    assert_eq!(callout.context["their_score"], 0);

    // Mid-game views render the choosing state: the header names round 2,
    // the agents carry the round-one scores, and mono has no escapes.
    let viewport = Viewport {
        width: 80,
        color: ColorDepth::Ansi16,
    };
    let view = run.view(0, viewport).await;
    assert!(view.slots[&Slot::Header].contains("round 2 of 3"));
    assert!(view.slots[&Slot::State].contains("Waiting for choices"));
    assert!(view.slots[&Slot::Agents].contains("P0"));
    assert!(view.slots[&Slot::Agents].contains("1 point"));
    assert!(view.slots[&Slot::StatusBar].contains("playing"));
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

    // Round two clinches the match 2-0.
    run.expect_input(0)
        .respond_bytes(CHOICE_ROCK.to_vec())
        .await;
    run.expect_input(1)
        .respond_bytes(CHOICE_SCISSORS.to_vec())
        .await;
    run.expect_completed_all().await;
    run
}

#[tokio::test]
async fn rock_paper_scissors_bilateral_runs_and_verifies_receipts() {
    let wasm = program_wasm("rock_paper_scissors");
    let run = completed_run(&wasm).await;

    assert_eq!(run.completed_outcome(0), run.completed_outcome(1));
    assert_eq!(run.session_hash(0), run.session_hash(1));
    assert_eq!(run.trace(0), run.trace(1));
    assert_eq!(run.receipt_bytes(0), run.receipt_bytes(1));
    assert_eq!(run.receipt(0).receipt_id(), run.receipt(1).receipt_id());
    assert_eq!(
        serde_json::to_value(run.receipt(0)).unwrap(),
        serde_json::to_value(run.receipt(1)).unwrap()
    );
    let verified = run.verify_all().expect("both receipts verify");

    for (i, verified) in verified.into_iter().enumerate() {
        assert_eq!(verified.program_id, arena0_program::ProgramHash::of(&wasm));
        assert_eq!(verified.session_id, run.session_hash(i));
        assert!(matches!(
            verified.terminal,
            LightVerifiedTerminal::Completed { .. }
        ));
    }

    let mut trace = run.trace(0);
    assert!(
        trace.len() > 1,
        "fixture must have a nonempty public prefix"
    );
    trace.pop();
    let forged = run.receipt(0);
    // A truncated trace cannot satisfy the certified terminal boundary.
    let body = arena0_protocol::ReceiptBody::new(
        forged.body().header().clone(),
        forged.body().outcome().to_vec(),
        forged.body().params().to_vec(),
        trace,
    )
    .expect("shape-only body assembly");
    assert!(arena0_protocol::ReceiptArtifact::new(body).is_err());
}
