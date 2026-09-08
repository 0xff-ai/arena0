//! End-to-end: two Hosts in one in-process daemon form a session through real
//! negotiation (one creator, one joiner), get driven through the shared Unix
//! socket with JSON answers (no hex), and produce matching, verifiable receipts
//! whose verify call returns the recovered evidence (program, ensemble, steps,
//! typed outcome).

mod common;

use arena0_api::{
    EnsembleSpec, EventData, EventFilter, EventFrame, FullVerifiedTerminal, HostRequest,
    LightVerifiedTerminal, NextEvent, ReceiptRef, Response, ResponseOk, VerifiedResult,
};
use arena0_protocol::{NegotiationTarget, SessionHash};
use arena0_sandbox::Program;
use common::{HostTarget, call, created, cumulative_sum_wasm, daemon, drive, ok, rps_wasm};
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::UnixStream;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_hosts_play_and_verify() {
    let wasm = rps_wasm();
    let d = daemon(&wasm).await;
    let requested_exec_a = arena0_protocol::ExecId([0xa1; 32]);
    let requested_exec_b = arena0_protocol::ExecId([0xb1; 32]);
    // RPS requests input from participant zero first, independently of which
    // Host creates the negotiation. Observe that participant before driving
    // either side so the test cannot wait on an idle participant.
    let (callout_host, callout_exec) = if d.peer_a < d.peer_b {
        (&d.host_a, requested_exec_a)
    } else {
        (&d.host_b, requested_exec_b)
    };

    // Keep one real Host event subscription open before creation so the
    // supervisor's session.callout projection is observed at its source.
    let stream = UnixStream::connect(&callout_host.socket)
        .await
        .expect("connect to Host event stream");
    let (read, mut event_write) = stream.into_split();
    let mut event_read = BufReader::new(read);
    arena0_api::frame::write_frame(
        &mut event_write,
        &callout_host.request(&HostRequest::EventsSubscribe {
            filter: EventFilter {
                include: Vec::new(),
                exclude: Vec::new(),
            },
        }),
    )
    .await
    .expect("write event subscription");
    let ack: Response = arena0_api::frame::read_frame(&mut event_read)
        .await
        .expect("read event subscription ack")
        .expect("event subscription ack frame");
    assert!(matches!(ack, Ok(ResponseOk::Subscribed)));

    // A publishes one exact negotiation, then B joins it by creator and
    // negotiation id. Params are omitted (the program takes none); the program
    // is named by its full content id.
    let req_a = HostRequest::ExecNew {
        exec_id: requested_exec_a,
        program: d.program_id.to_string(),
        params: Some(serde_json::json!(null)),
        ensemble: EnsembleSpec::Explicit {
            peers: vec![d.peer_b],
        },
    };
    let (exec_a, negotiation_id) = match ok(call(&d.host_a, &req_a).await) {
        ResponseOk::ExecCreated {
            exec_id,
            negotiation_id: Some(negotiation_id),
            ..
        } => (exec_id, negotiation_id),
        other => panic!("unexpected creator response: {other:?}"),
    };
    let exec_b = created(
        call(
            &d.host_b,
            &HostRequest::ExecNew {
                exec_id: requested_exec_b,
                program: d.program_id.to_string(),
                params: Some(serde_json::json!(null)),
                ensemble: EnsembleSpec::Join {
                    target: Some(NegotiationTarget::new(d.peer_a, negotiation_id)),
                },
            },
        )
        .await,
    );
    assert_eq!(exec_a, requested_exec_a);
    assert_eq!(exec_b, requested_exec_b);

    // The first callout is observed through both public projections before
    // either driver answers it. The expected metadata comes independently
    // from the loaded RPS program; the context is the program's documented
    // first-round request, not a value copied from either projection.
    let next_request = HostRequest::ExecNext {
        exec_id: callout_exec,
    };
    let next = call(callout_host, &next_request);
    let event = async {
        loop {
            let frame = arena0_api::frame::read_frame::<_, EventFrame>(&mut event_read)
                .await
                .expect("read session.callout")
                .expect("session.callout frame");
            if let EventData::SessionCallout { .. } = &frame.data {
                return frame;
            }
        }
    };
    let (next, event) = tokio::time::timeout(Duration::from_secs(120), async {
        tokio::join!(next, event)
    })
    .await
    .expect("timed out waiting for ExecNext and session.callout");
    let EventFrame {
        data: event_data,
        exec_id: event_exec_id,
        session_id: event_session_id,
        ..
    } = event;
    match (ok(next), event_data) {
        (
            ResponseOk::Next(NextEvent::Callout {
                pending_id: next_pending_id,
                callout_index: next_callout_index,
                name: next_name,
                prompt: next_prompt,
                schema: next_schema,
                context: next_context,
            }),
            EventData::SessionCallout {
                pending_id: event_pending_id,
                callout_index: event_callout_index,
                name: event_name,
                prompt: event_prompt,
                schema: event_schema,
                context: event_context,
            },
        ) => {
            let program = Program::try_from(wasm).expect("parse real RPS guest");
            let callout = program
                .definition()
                .schema
                .callouts
                .first()
                .expect("RPS program has a callout");
            let expected_context = serde_json::json!({
                "round": 1,
                "total_rounds": 3,
                "your_score": 0,
                "their_score": 0,
            });
            assert_eq!(
                next_pending_id, event_pending_id,
                "ExecNext and event share pending identity"
            );
            assert_eq!(next_callout_index, event_callout_index);
            assert_eq!(next_name, event_name);
            assert_eq!(next_prompt, event_prompt);
            assert_eq!(next_schema, event_schema);
            assert_eq!(next_context, event_context);
            assert_eq!(next_callout_index, 0);
            assert_eq!(next_name, "ChooseMove");
            assert_eq!(next_prompt, "Choose rock, paper, or scissors");
            assert_eq!(next_schema, callout.output);
            assert_eq!(next_context, expected_context);
            assert_eq!(event_exec_id, Some(callout_exec));
            assert!(
                event_session_id.is_some(),
                "session callout is session-scoped"
            );
        }
        (next, event) => panic!("unexpected first callout projections: {next:?}, {event:?}"),
    }

    // Drive both to completion with JSON answers; they must agree on the session id.
    let (sid_a, sid_b) = tokio::join!(drive(&d.host_a, exec_a), drive(&d.host_b, exec_b));
    assert_eq!(sid_a, sid_b, "both parties confirmed the same session");
    assert_eq!(event_session_id, Some(sid_a));

    // Receipts are fetchable and verify at both tiers, on both Hosts, returning
    // evidence rather than a bool.
    let mut artifacts = Vec::new();
    for (target, sid) in [(&d.host_a, sid_a), (&d.host_b, sid_b)] {
        let ResponseOk::Receipt(receipt) = ok(call(
            target,
            &HostRequest::ReceiptGet {
                receipt: arena0_api::ReceiptRef::Produced(sid),
            },
        )
        .await) else {
            panic!("expected a receipt");
        };
        artifacts.push(receipt);
        assert_verified(target, sid, false).await;
        assert_verified(target, sid, true).await;
    }
    assert_eq!(
        artifacts[0].encode().unwrap(),
        artifacts[1].encode().unwrap()
    );
}

async fn assert_verified(target: &HostTarget, session_id: SessionHash, full: bool) {
    let resp = ok(call(
        target,
        &HostRequest::ReceiptVerify {
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
    let d = daemon(&wasm).await;

    // Creator proposes exact terms; the joiner sends NO params at all.
    let req_a = HostRequest::ExecNew {
        exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
        program: d.program_id.to_string(),
        params: Some(serde_json::json!({ "target_size": 2, "bias": 0 })),
        ensemble: EnsembleSpec::Explicit {
            peers: vec![d.peer_b],
        },
    };
    let (exec_a, negotiation_id) = match ok(call(&d.host_a, &req_a).await) {
        ResponseOk::ExecCreated {
            exec_id,
            negotiation_id: Some(negotiation_id),
            ..
        } => (exec_id, negotiation_id),
        other => panic!("unexpected creator response: {other:?}"),
    };
    let exec_b = created(
        call(
            &d.host_b,
            &HostRequest::ExecNew {
                exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
                program: d.program_id.to_string(),
                params: None,
                ensemble: EnsembleSpec::Join {
                    target: Some(NegotiationTarget::new(d.peer_a, negotiation_id)),
                },
            },
        )
        .await,
    );

    // cumulative-sum runs itself to completion; both sides must land on the
    // same session with the adopted terms.
    let (sid_a, sid_b) = tokio::join!(drive(&d.host_a, exec_a), drive(&d.host_b, exec_b));
    assert_eq!(sid_a, sid_b, "both parties confirmed the same session");

    for (target, sid) in [(&d.host_a, sid_a), (&d.host_b, sid_b)] {
        let resp = ok(call(
            target,
            &HostRequest::ReceiptVerify {
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
