//! Bilateral Wasm execution and receipt verification.
//!
//! The live path drives the generated guest through the public Host API, and
//! the verifier consumes the resulting authenticated receipt artifact.

use std::time::Duration;

use arena0_tests::arena::Arena;
use arena0_tests::wasm::program_wasm;
use arena0_verify::{LightVerifiedTerminal, VerifiedTerminal, verify_light};

const CHOICE_ROCK: &[u8] = br#""Rock""#;
const CHOICE_SCISSORS: &[u8] = br#""Scissors""#;

async fn completed_run(wasm: &[u8]) -> arena0_tests::arena::Run {
    let mut arena = Arena::new();
    arena
        .program(wasm.to_vec())
        .participants(2)
        .timeout(Duration::from_secs(120));
    let mut run = arena.run().await;
    // Two rounds: participant 0 plays rock and participant 1 scissors.
    for _ in 0..2 {
        run.expect_input(0)
            .respond_bytes(CHOICE_ROCK.to_vec())
            .await;
        run.expect_input(1)
            .respond_bytes(CHOICE_SCISSORS.to_vec())
            .await;
    }
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
    let verified = run.verify_all(&wasm).expect("both receipts replay-verify");

    for (i, full) in verified.into_iter().enumerate() {
        let light = verify_light(&run.receipt_bytes(i)).expect("light verification");
        assert_eq!(light.program_id, arena0_program::ProgramHash::of(&wasm));
        assert_eq!(light.session_id, run.session_hash(i));
        assert!(matches!(
            light.terminal,
            LightVerifiedTerminal::Completed { .. }
        ));
        assert!(matches!(full.terminal, VerifiedTerminal::Completed { .. }));
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
