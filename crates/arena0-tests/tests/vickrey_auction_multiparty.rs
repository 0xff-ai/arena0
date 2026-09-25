//! Three independent replicas complete a sealed-bid auction and verify every
//! locally produced artifact's portable proof.

use arena0_protocol::{ColorDepth, ReceiptTermination, Slot, Viewport};
use arena0_tests::arena::Arena;
use arena0_tests::wasm::program_wasm;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vickrey_auction_runs_and_every_producer_verifies() {
    let wasm = program_wasm("vickrey_auction");

    let params = serde_json::to_vec(&serde_json::json!({
        "item": "launch lot",
        "reserve": 50,
    }))
    .expect("auction params encode as JSON");
    let mut arena = Arena::new();
    arena.program(wasm.clone()).participants(3).params(params);
    let mut run = arena.run().await;

    // The first bidder's callout carries the auction item and the reserve.
    let callout = run.callout(1).await;
    assert_eq!(callout.context["item"], "launch lot");
    assert_eq!(callout.context["reserve"], 50);

    // A non-integer bid is rejected with the program's reason and the callout
    // stays open, so the valid bid is answered below.
    let reason = run
        .expect_input(1)
        .respond_rejected(br#""high""#.to_vec())
        .await;
    assert!(
        reason.contains("invalid type"),
        "unexpected rejection reason: {reason}"
    );

    run.expect_input(1)
        .respond_bytes(serde_json::to_vec(&120_u64).expect("bid encodes as JSON"))
        .await;
    // The second bidder is asked only after the first bid is committed, so
    // its node renders the collected commitments mid-game.
    let callout = run.callout(2).await;
    assert_eq!(callout.context["item"], "launch lot");
    let view = run
        .view(
            2,
            Viewport {
                width: 120,
                color: ColorDepth::Mono,
            },
        )
        .await;
    assert_eq!(view.slots.len(), 4);
    assert!(
        view.slots[&Slot::Header].contains("Sealed-Bid Vickrey Auction | launch lot"),
        "unexpected header: {:?}",
        view.slots[&Slot::Header]
    );
    assert!(view.slots[&Slot::Agents].contains("P0 seller/coordinator"));
    assert!(
        view.slots[&Slot::State].contains("Bids: 2 of 3 commitments collected"),
        "unexpected state: {:?}",
        view.slots[&Slot::State]
    );
    assert!(view.slots[&Slot::State].contains("Waiting for sealed bids."));
    assert!(view.slots[&Slot::StatusBar].contains("reserve: 50"));
    assert!(
        view.slots.values().all(|text| !text.contains("\x1b[")),
        "mono view contains SGR"
    );
    run.expect_input(2)
        .respond_bytes(serde_json::to_vec(&120_u64).expect("bid encodes as JSON"))
        .await;

    let outcomes = run.expect_completed_all().await;
    assert!(outcomes.iter().all(|outcome| outcome == &outcomes[0]));
    assert_eq!(run.session_hash(0), run.session_hash(1));
    assert_eq!(run.session_hash(1), run.session_hash(2));

    let verified = run.verify_all().expect("all three auction receipts verify");

    for i in 1..run.node_count() {
        assert_eq!(
            run.receipt_bytes(0),
            run.receipt_bytes(i),
            "canonical auction receipt including joint randomness"
        );
        assert_eq!(run.receipt(0).receipt_id(), run.receipt(i).receipt_id());
    }

    for (participant, (verified, expected_outcome)) in
        verified.into_iter().zip(&outcomes).enumerate()
    {
        assert_eq!(
            verified.terminal,
            ReceiptTermination::Completed,
            "participant {participant}: expected completed receipt"
        );
        assert_eq!(verified.outcome_borsh.as_ref(), Some(expected_outcome));
    }
}
