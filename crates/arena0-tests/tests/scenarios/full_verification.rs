//! Full verification checks certified execution against the program itself.

use arena0_node::{ReplayError, verify_full};
use arena0_protocol::StepTerminal;
use arena0_sandbox::Program;
use arena0_test_engine::shared_test_engine;
use arena0_tests::arena::Arena;
use arena0_tests::wasm::program_wasm;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_receipt_replays_and_projects_its_outcome() {
    let wasm = program_wasm("vickrey_auction");
    let program = Program::try_from(wasm.clone()).expect("program");
    let loaded = shared_test_engine().load(&program).expect("load");
    let mut arena = Arena::new();
    arena
        .program(wasm)
        .participants(3)
        .params(br#"{"item":"lamp"}"#.to_vec());
    let mut run = arena.run().await;
    run.expect_input(1).respond_bytes(b"40".to_vec()).await;
    run.expect_input(2).respond_bytes(b"30".to_vec()).await;
    run.expect_agreed_completion().await;
    for i in 0..run.node_count() {
        let verified = verify_full(&loaded, &run.receipt(i)).expect("full verification");
        let outcome = verified.outcome_json.expect("completed outcome");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(outcome.as_bytes()).expect("JSON outcome"),
            serde_json::json!({"Sold":{"item":"lamp","reserve":null,"bids":[40,30],"winner":1,"price":30}})
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resigned_wrong_state_fails_at_its_step() {
    let wasm = program_wasm("vickrey_auction");
    let program = Program::try_from(wasm.clone()).expect("program");
    let loaded = shared_test_engine().load(&program).expect("load");
    let mut arena = Arena::new();
    arena
        .program(wasm)
        .participants(3)
        .params(br#"{"item":"lamp"}"#.to_vec());
    let mut run = arena.run().await;
    run.expect_input(1).respond_bytes(b"40".to_vec()).await;
    run.expect_input(2).respond_bytes(b"30".to_vec()).await;
    run.expect_agreed_completion().await;
    let artifact = run.recertified_receipt(0, |trace, _| {
        let last = trace.last_mut().expect("last step");
        last.post_state = last.pre_state;
    });
    let last_step = artifact.body().trace().last().expect("last step").step;
    assert!(
        matches!(verify_full(&loaded, &artifact), Err(ReplayError::Diverged { step, .. }) if step == last_step)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resigned_wrong_ending_fails_at_its_step() {
    let wasm = program_wasm("vickrey_auction");
    let program = Program::try_from(wasm.clone()).expect("program");
    let loaded = shared_test_engine().load(&program).expect("load");
    let mut arena = Arena::new();
    arena
        .program(wasm)
        .participants(3)
        .params(br#"{"item":"lamp"}"#.to_vec());
    let mut run = arena.run().await;
    run.expect_input(1).respond_bytes(b"40".to_vec()).await;
    run.expect_input(2).respond_bytes(b"30".to_vec()).await;
    run.expect_agreed_completion().await;
    let artifact = run.recertified_receipt(0, |trace, outcome| {
        let last = trace.last_mut().expect("last step");
        let Some(StepTerminal::End { outcome: ending }) = &mut last.terminal else {
            panic!("completion must have a SessionEnd");
        };
        ending.push(0);
        *outcome = ending.clone();
    });
    let last_step = artifact.body().trace().last().expect("last step").step;
    assert!(
        matches!(verify_full(&loaded, &artifact), Err(ReplayError::Terminal { step, .. }) if step == last_step)
    );
}
