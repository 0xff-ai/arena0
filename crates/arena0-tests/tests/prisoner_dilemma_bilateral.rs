//! M3: two in-process executions run prisoner's dilemma bilaterally
//! over `LocalTransport`; the replay verifier accepts both traces.

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
    assert_eq!(
        outcomes[0], outcomes[1],
        "both participants must derive the identical outcome receipt"
    );

    assert_eq!(run.session_hash(0), run.session_hash(1));
    run.verify_all(&wasm).expect("both traces must verify");
}
