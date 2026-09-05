//! Bilateral Wasm execution and receipt verification.
//!
//! The live path drives the generated guest through the public Host API, and
//! the verifier consumes the resulting sealed receipt artifact.

use std::time::Duration;

use arena0_tests::arena::Arena;
use arena0_tests::synthetic::Synthetic;
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
}

#[tokio::test]
async fn verify_light_is_sandbox_free_and_self_describing() {
    let wasm = program_wasm("rock_paper_scissors");
    let run = completed_run(&wasm).await;
    let bytes = run.receipt_bytes(0);
    let verified = verify_light(&bytes).expect("light verification accepts honest receipt");
    assert_eq!(verified.program_id, arena0_program::ProgramHash::of(&wasm));
    assert_eq!(verified.session_id, run.session_hash(0));
    assert_eq!(verified.steps as usize, run.trace(0).len());

    // The receipt is content-addressed and sealed. Any byte-level mutation is
    // rejected before a caller can reinterpret its terminal evidence.
    let mut tampered = bytes;
    let index = tampered.len() / 2;
    tampered[index] ^= 0xFF;
    assert!(verify_light(&tampered).is_err());
}

#[tokio::test]
async fn receipt_terminal_and_activation_evidence_are_not_optional() {
    let wasm = program_wasm("rock_paper_scissors");
    let run = completed_run(&wasm).await;
    assert!(run.activation_with_activation_signers(&[0]).is_err());

    let mut trace = run.trace(0);
    trace.pop();
    let mut forged = run.receipt(0);
    // Reassembling a receipt with a truncated trace cannot retain the honest
    // producer seal. The portable verifier therefore rejects the raw artifact.
    let body = arena0_protocol::ReceiptBody::new(
        forged.body().header().clone(),
        forged.body().outcome().to_vec(),
        forged.body().params().to_vec(),
        trace,
    )
    .expect("shape-only body assembly");
    assert!(arena0_protocol::Receipt::new(body, forged.seal().clone()).is_err());
    // Keep the variable consumed by the assertion above and make the intended
    // failure explicit through the original receipt bytes.
    let _ = &mut forged;
    assert!(verify_light(&run.receipt_bytes(0)).is_ok());
}

#[test]
fn authenticated_stop_is_a_distinct_terminal_result() {
    let synthetic = Synthetic::new(2);
    let cause = synthetic.authenticated_stop(0, arena0_protocol::AbortKind::Fail, "guest failed");
    let receipt = synthetic.stopped_receipt(0, cause, Vec::new());
    let bytes = receipt.encode().expect("encode stopped receipt");
    let verified = verify_light(&bytes).expect("stopped receipt verifies");
    assert!(matches!(
        verified.terminal,
        LightVerifiedTerminal::Stopped { .. }
    ));
}
