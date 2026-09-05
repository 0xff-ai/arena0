//! End-to-end: two in-process daemons over a shared `LocalNetwork`, each on its
//! own unix socket, form a session through real negotiation (one creator, one
//! joiner), get driven through rock-paper-scissors over the socket with JSON
//! answers (no hex), and produce matching, verifiable receipts whose verify call
//! returns the recovered evidence (program, ensemble, steps, typed outcome).

mod common;

use std::path::Path;

use arena0_api::{
    EnsembleSpec, FullVerifiedTerminal, LightVerifiedTerminal, ReceiptRef, Request, ResponseOk,
    VerifiedResult,
};
use arena0_protocol::SessionHash;
use common::{call, created, cumulative_sum_wasm, drive, ok, rps_wasm, two_daemons};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_daemons_play_and_verify() {
    let wasm = rps_wasm();
    let d = two_daemons(&wasm).await;

    // A publishes one exact negotiation, then B joins it by creator and
    // negotiation id. Params are omitted (the program takes none); the program
    // is named by its full content id.
    let req_a = Request::ExecNew {
        exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
        program: d.program_id.to_string(),
        params: Some(serde_json::json!(null)),
        ensemble: EnsembleSpec::Explicit {
            peers: vec![d.peer_b],
        },
    };
    let (exec_a, negotiation_id) = match ok(call(&d.sock_a, &req_a).await) {
        ResponseOk::ExecCreated {
            exec_id,
            negotiation_id,
            ..
        } => (exec_id, negotiation_id),
        other => panic!("unexpected creator response: {other:?}"),
    };
    let exec_b = created(
        call(
            &d.sock_b,
            &Request::ExecNew {
                exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
                program: d.program_id.to_string(),
                params: Some(serde_json::json!(null)),
                ensemble: EnsembleSpec::Join {
                    creator: d.peer_a,
                    negotiation_id,
                },
            },
        )
        .await,
    );

    // Drive both to completion with JSON answers; they must agree on the session id.
    let (sid_a, sid_b) = tokio::join!(drive(&d.sock_a, exec_a), drive(&d.sock_b, exec_b));
    assert_eq!(sid_a, sid_b, "both parties confirmed the same session");

    // Receipts are fetchable and verify at both tiers, on both daemons, returning
    // evidence rather than a bool.
    let mut artifacts = Vec::new();
    for (sock, sid) in [(&d.sock_a, sid_a), (&d.sock_b, sid_b)] {
        let ResponseOk::Receipt(receipt) = ok(call(
            sock,
            &Request::ReceiptGet {
                receipt: arena0_api::ReceiptRef::Produced(sid),
            },
        )
        .await) else {
            panic!("expected a receipt");
        };
        artifacts.push(receipt);
        assert_verified(sock, sid, false).await;
        assert_verified(sock, sid, true).await;
    }
    assert_eq!(
        artifacts[0].encode().unwrap(),
        artifacts[1].encode().unwrap()
    );
}

async fn assert_verified(socket: &Path, session_id: SessionHash, full: bool) {
    let resp = ok(call(
        socket,
        &Request::ReceiptVerify {
            receipt: ReceiptRef::Produced(session_id),
            full,
        },
    )
    .await);
    match resp {
        ResponseOk::Verified {
            receipt_id: _,
            program_id: _,
            session_id: verified_sid,
            ensemble,
            steps,
            result,
        } => {
            assert_eq!(verified_sid, session_id, "verify recovers the session id");
            assert_eq!(ensemble.len(), 2, "two participants");
            assert!(steps > 0, "at least one step");
            match (full, result) {
                (
                    false,
                    VerifiedResult::Light {
                        terminal: LightVerifiedTerminal::Completed { .. },
                    },
                ) => {}
                (
                    true,
                    VerifiedResult::Full {
                        terminal: FullVerifiedTerminal::Completed { outcome_json, .. },
                    },
                ) => {
                    assert!(
                        outcome_json.get("Win").is_some() || outcome_json.get("Draw").is_some(),
                        "typed rps outcome, got {outcome_json}"
                    );
                }
                (_, result) => panic!("expected completed evidence for requested tier: {result:?}"),
            }
        }
        other => panic!("unexpected verify response: {other:?}"),
    }
}

/// A joiner that gives no local params must adopt the creator's proposed terms
/// (the offer carries the exact params), rather
/// than fail schema validation with a mismatched-shape error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn joiner_without_params_adopts_creator_terms() {
    let wasm = cumulative_sum_wasm();
    let d = two_daemons(&wasm).await;

    // Creator proposes exact terms; the joiner sends NO params at all.
    let req_a = Request::ExecNew {
        exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
        program: d.program_id.to_string(),
        params: Some(serde_json::json!({ "target_size": 2, "bias": 0 })),
        ensemble: EnsembleSpec::Explicit {
            peers: vec![d.peer_b],
        },
    };
    let (exec_a, negotiation_id) = match ok(call(&d.sock_a, &req_a).await) {
        ResponseOk::ExecCreated {
            exec_id,
            negotiation_id,
            ..
        } => (exec_id, negotiation_id),
        other => panic!("unexpected creator response: {other:?}"),
    };
    let exec_b = created(
        call(
            &d.sock_b,
            &Request::ExecNew {
                exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
                program: d.program_id.to_string(),
                params: None,
                ensemble: EnsembleSpec::Join {
                    creator: d.peer_a,
                    negotiation_id,
                },
            },
        )
        .await,
    );

    // cumulative-sum runs itself to completion; both sides must land on the
    // same session with the adopted terms.
    let (sid_a, sid_b) = tokio::join!(drive(&d.sock_a, exec_a), drive(&d.sock_b, exec_b));
    assert_eq!(sid_a, sid_b, "both parties confirmed the same session");

    for (sock, sid) in [(&d.sock_a, sid_a), (&d.sock_b, sid_b)] {
        let resp = ok(call(
            sock,
            &Request::ReceiptVerify {
                receipt: ReceiptRef::Produced(sid),
                full: false,
            },
        )
        .await);
        match resp {
            ResponseOk::Verified { steps, result, .. } => {
                assert!(steps > 0, "at least one step");
                match result {
                    VerifiedResult::Light {
                        terminal: LightVerifiedTerminal::Completed { .. },
                    } => {}
                    result => panic!("expected light completed evidence: {result:?}"),
                }
            }
            other => panic!("unexpected verify response: {other:?}"),
        }
    }
}
