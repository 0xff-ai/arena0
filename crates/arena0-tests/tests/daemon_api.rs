//! The runtime-API surface over the unix socket: non-blocking launch + `exec.await`,
//! the `events.subscribe` stream, receipt content addressing / import / list, and
//! program-handle resolution over the wire.

mod common;

use std::time::Duration;

use arena0_api::{
    AwaitState, ColorDepth, EnsembleSpec, EventData, EventFilter, EventFrame, ExecLifecycle,
    HostRequest, ReceiptArtifact, Request, Response, ResponseOk,
};
use arena0_protocol::{ExecId, NegotiationTarget, Slot};
use common::{HostTarget, call, call_daemon, created, daemon, drive, ok, rps_wasm};
use tokio::io::BufReader;
use tokio::net::UnixStream;

async fn next_from_either(
    target_a: &HostTarget,
    exec_a: ExecId,
    target_b: &HostTarget,
    exec_b: ExecId,
) -> (bool, Response) {
    let request_a = HostRequest::ExecNext { exec_id: exec_a };
    let request_b = HostRequest::ExecNext { exec_id: exec_b };
    let mut next_a = Box::pin(call(target_a, &request_a));
    let mut next_b = Box::pin(call(target_b, &request_b));
    tokio::select! {
        response = &mut next_a => (true, response),
        response = &mut next_b => (false, response),
    }
}

/// `exec.new` returns immediately in `Negotiating`; `exec.await` blocks for `Active`
/// then `Terminal`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_new_returns_immediately_and_await_blocks() {
    let wasm = rps_wasm();
    let d = daemon(&wasm).await;

    let resp_a = call(
        &d.host_a,
        &HostRequest::ExecNew {
            exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
            program: d.program_id.to_string(),
            params: Some(serde_json::json!(null)),
            ensemble: EnsembleSpec::Explicit {
                peers: vec![d.peer_b],
            },
        },
    )
    .await;
    // The response is immediate and reports the Negotiating state.
    let (exec_a, state_a, negotiation_id) = match ok(resp_a) {
        ResponseOk::ExecCreated {
            exec_id,
            exec_state,
            negotiation_id: Some(negotiation_id),
            ..
        } => (exec_id, exec_state, negotiation_id),
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(
        state_a,
        ExecLifecycle::Negotiating,
        "launch is non-blocking"
    );

    let resp_b = call(
        &d.host_b,
        &HostRequest::ExecNew {
            exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
            program: d.program_id.to_string(),
            params: Some(serde_json::json!(null)),
            ensemble: EnsembleSpec::Join {
                target: Some(NegotiationTarget::new(d.peer_a, negotiation_id)),
            },
        },
    )
    .await;
    let exec_b = created(resp_b);

    // Await Active: resolves once the session confirms and starts (no driving needed).
    match ok(call(
        &d.host_a,
        &HostRequest::ExecAwait {
            exec_id: exec_a,
            until: AwaitState::Active,
        },
    )
    .await)
    {
        ResponseOk::Awaited { exec_state, .. } => {
            assert_eq!(exec_state, ExecLifecycle::Active, "await Active reached");
        }
        other => panic!("unexpected await response: {other:?}"),
    }

    // Drive both to completion, then await Terminal.
    let (_sa, _sb) = tokio::join!(drive(&d.host_a, exec_a), drive(&d.host_b, exec_b));
    match ok(call(
        &d.host_a,
        &HostRequest::ExecAwait {
            exec_id: exec_a,
            until: AwaitState::Terminal,
        },
    )
    .await)
    {
        ResponseOk::Awaited { exec_state, .. } => {
            assert_eq!(
                exec_state,
                ExecLifecycle::Completed,
                "await Terminal reached"
            );
        }
        other => panic!("unexpected await response: {other:?}"),
    }

    match ok(call(&d.host_a, &HostRequest::ExecStatus { exec_id: exec_a }).await) {
        ResponseOk::Status(status) => {
            assert!(
                status.session_id().is_some(),
                "terminal status keeps the session"
            );
            assert!(
                status
                    .session()
                    .is_some_and(|session| session.receipt_available),
                "terminal status reports the durable locally produced artifact"
            );
        }
        other => panic!("unexpected terminal status response: {other:?}"),
    }
}

/// Two concurrent answers to one durable callout resolve exactly once. The
/// loser receives the typed conflict and the actor remains live for the rest
/// of the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn competing_callout_submissions_return_typed_conflict_and_execution_continues() {
    let wasm = rps_wasm();
    let d = daemon(&wasm).await;
    let (exec_a, negotiation_id) = match ok(call(
        &d.host_a,
        &HostRequest::ExecNew {
            exec_id: ExecId([line!() as u8; 32]),
            program: d.program_id.to_string(),
            params: Some(serde_json::json!(null)),
            ensemble: EnsembleSpec::Explicit {
                peers: vec![d.peer_b],
            },
        },
    )
    .await)
    {
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
                exec_id: ExecId([line!() as u8; 32]),
                program: d.program_id.to_string(),
                params: Some(serde_json::json!(null)),
                ensemble: EnsembleSpec::Join {
                    target: Some(NegotiationTarget::new(d.peer_a, negotiation_id)),
                },
            },
        )
        .await,
    );

    let (is_a, next) = next_from_either(&d.host_a, exec_a, &d.host_b, exec_b).await;
    let (target, exec_id) = if is_a {
        (&d.host_a, exec_a)
    } else {
        (&d.host_b, exec_b)
    };
    let pending_id = match ok(next) {
        ResponseOk::Next(arena0_api::NextEvent::Callout { pending_id, .. }) => pending_id,
        ResponseOk::Next(arena0_api::NextEvent::Failed { reason }) => {
            panic!("execution failed before callout: {reason}")
        }
        other => panic!("unexpected event before callout: {other:?}"),
    };

    let request = HostRequest::ExecSubmit {
        exec_id,
        pending_id,
        answer: Some(serde_json::json!("Rock")),
    };
    let (left, right) = tokio::join!(call(target, &request), call(target, &request));
    let responses = [left, right];
    assert_eq!(
        responses
            .iter()
            .filter(|response| matches!(response, Ok(ResponseOk::Ack)))
            .count(),
        1,
        "one competing answer is accepted"
    );
    let conflict = responses
        .into_iter()
        .find_map(Result::err)
        .expect("one competing answer is rejected");
    assert_eq!(conflict.code, arena0_api::ApiErrorCode::CalloutNotPending);

    let (_session_a, _session_b) = tokio::join!(drive(&d.host_a, exec_a), drive(&d.host_b, exec_b));
    match ok(call(&d.host_a, &HostRequest::ExecStatus { exec_id: exec_a }).await) {
        ResponseOk::Status(status) => assert_eq!(status.lifecycle(), ExecLifecycle::Completed),
        other => panic!("unexpected final status: {other:?}"),
    }
}

/// The other RPS Host answers first in the final round, then the human answers
/// the final callout. Once the terminal supervisor has cleaned up, replaying
/// that old pending id is a typed conflict while an unknown execution remains
/// NotFound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_callout_after_terminal_is_typed_conflict_and_missing_exec_is_not_found() {
    let d = daemon(&rps_wasm()).await;
    let (exec_a, negotiation_id) = match ok(call(
        &d.host_a,
        &HostRequest::ExecNew {
            exec_id: ExecId([line!() as u8; 32]),
            program: d.program_id.to_string(),
            params: Some(serde_json::json!(null)),
            ensemble: EnsembleSpec::Explicit {
                peers: vec![d.peer_b],
            },
        },
    )
    .await)
    {
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
                exec_id: ExecId([line!() as u8; 32]),
                program: d.program_id.to_string(),
                params: Some(serde_json::json!(null)),
                ensemble: EnsembleSpec::Join {
                    target: Some(NegotiationTarget::new(d.peer_a, negotiation_id)),
                },
            },
        )
        .await,
    );

    let (human_target, human_exec, final_pending) = loop {
        let (is_a, response) = next_from_either(&d.host_a, exec_a, &d.host_b, exec_b).await;
        let next = ok(response);
        let ResponseOk::Next(arena0_api::NextEvent::Callout {
            pending_id,
            context,
            ..
        }) = next
        else {
            panic!("expected RPS callout, got {next:?}");
        };
        let round = context
            .get("round")
            .and_then(serde_json::Value::as_u64)
            .expect("RPS callout round");
        let other_target = if is_a { &d.host_a } else { &d.host_b };
        let other_exec = if is_a { exec_a } else { exec_b };
        ok(call(
            other_target,
            &HostRequest::ExecSubmit {
                exec_id: other_exec,
                pending_id,
                answer: Some(serde_json::json!("Rock")),
            },
        )
        .await);

        if round == 3 {
            let human_target = if is_a { &d.host_b } else { &d.host_a };
            let human_exec = if is_a { exec_b } else { exec_a };
            let final_pending = match ok(call(
                human_target,
                &HostRequest::ExecNext {
                    exec_id: human_exec,
                },
            )
            .await)
            {
                ResponseOk::Next(arena0_api::NextEvent::Callout {
                    pending_id,
                    context,
                    ..
                }) => {
                    assert_eq!(
                        context.get("round").and_then(serde_json::Value::as_u64),
                        Some(3),
                        "human answer is the final RPS round"
                    );
                    pending_id
                }
                other => panic!("unexpected final human event: {other:?}"),
            };
            ok(call(
                human_target,
                &HostRequest::ExecSubmit {
                    exec_id: human_exec,
                    pending_id: final_pending,
                    answer: Some(serde_json::json!("Rock")),
                },
            )
            .await);
            break (human_target, human_exec, final_pending);
        }
    };

    let await_a = HostRequest::ExecAwait {
        exec_id: exec_a,
        until: AwaitState::Terminal,
    };
    let await_b = HostRequest::ExecAwait {
        exec_id: exec_b,
        until: AwaitState::Terminal,
    };
    let (terminal_a, terminal_b) =
        tokio::join!(call(&d.host_a, &await_a), call(&d.host_b, &await_b));
    for terminal in [terminal_a, terminal_b] {
        match ok(terminal) {
            ResponseOk::Awaited { exec_state, .. } => {
                assert_eq!(exec_state, ExecLifecycle::Completed)
            }
            other => panic!("unexpected terminal await response: {other:?}"),
        }
    }
    // Let the terminal message reach the supervisor before replaying the old
    // answer; this is the stale-handle path that previously returned NotFound.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let stale = call(
        human_target,
        &HostRequest::ExecSubmit {
            exec_id: human_exec,
            pending_id: final_pending,
            answer: Some(serde_json::json!("Rock")),
        },
    )
    .await
    .expect_err("completed callout must be rejected");
    assert_eq!(stale.code, arena0_api::ApiErrorCode::CalloutNotPending);

    let missing = call(
        &d.host_a,
        &HostRequest::ExecSubmit {
            exec_id: ExecId([0xFA; 32]),
            pending_id: final_pending,
            answer: Some(serde_json::json!("Rock")),
        },
    )
    .await
    .expect_err("unknown execution must remain NotFound");
    assert_eq!(missing.code, arena0_api::ApiErrorCode::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosts_list_is_complete_from_shared_daemon_socket() {
    let d = daemon(&rps_wasm()).await;
    match ok(call_daemon(&d.socket, &Request::HostsList).await) {
        ResponseOk::Hosts(hosts) => {
            assert_eq!(hosts.len(), 2);
            assert_eq!(
                hosts
                    .iter()
                    .map(|host| host.host.id.as_str())
                    .collect::<Vec<_>>(),
                ["a", "b"]
            );
            assert_ne!(hosts[0].host.peer_id, hosts[1].host.peer_id);
        }
        other => panic!("unexpected HostsList response: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn negotiating_ticket_can_be_withdrawn() {
    let wasm = rps_wasm();
    let d = daemon(&wasm).await;
    let exec_id = created(
        call(
            &d.host_a,
            &HostRequest::ExecNew {
                exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
                program: d.program_id.to_string(),
                params: Some(serde_json::json!(null)),
                ensemble: EnsembleSpec::Explicit {
                    peers: vec![d.peer_b],
                },
            },
        )
        .await,
    );

    assert!(matches!(
        call(&d.host_a, &HostRequest::ExecWithdraw { exec_id }).await,
        Ok(ResponseOk::Ack)
    ));
    assert!(matches!(
        call(&d.host_a, &HostRequest::ExecWithdraw { exec_id }).await,
        Ok(ResponseOk::Ack)
    ));

    match ok(call(&d.host_a, &HostRequest::ExecStatus { exec_id }).await) {
        ResponseOk::Status(status) => assert_eq!(status.lifecycle(), ExecLifecycle::Failed),
        other => panic!("unexpected status after withdrawal: {other:?}"),
    }
    match ok(call(&d.host_a, &HostRequest::ExecNext { exec_id }).await) {
        ResponseOk::Next(arena0_api::NextEvent::Failed { reason }) => {
            assert_eq!(reason, "negotiation withdrawn locally")
        }
        other => panic!("unexpected next event after withdrawal: {other:?}"),
    }
}

/// `exec.view` renders active and terminal shared state, but not negotiation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_view_distinguishes_negotiating_active_terminal_and_missing_executions() {
    let wasm = rps_wasm();
    let d = daemon(&wasm).await;

    let (exec_a, negotiation_id) = match ok(call(
        &d.host_a,
        &HostRequest::ExecNew {
            exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
            program: d.program_id.to_string(),
            params: Some(serde_json::json!(null)),
            ensemble: EnsembleSpec::Explicit {
                peers: vec![d.peer_b],
            },
        },
    )
    .await)
    {
        ResponseOk::ExecCreated {
            exec_id,
            negotiation_id: Some(negotiation_id),
            ..
        } => (exec_id, negotiation_id),
        other => panic!("unexpected creator response: {other:?}"),
    };
    let ResponseOk::Status(status) =
        ok(call(&d.host_a, &HostRequest::ExecStatus { exec_id: exec_a }).await)
    else {
        panic!("expected negotiating execution status");
    };
    assert_eq!(status.lifecycle(), ExecLifecycle::Negotiating);
    let negotiating = call(
        &d.host_a,
        &HostRequest::ExecView {
            exec: exec_a,
            width: 80,
            color: ColorDepth::Ansi16,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(negotiating.code, arena0_api::ApiErrorCode::Execution);

    let exec_b = created(
        call(
            &d.host_b,
            &HostRequest::ExecNew {
                exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
                program: d.program_id.to_string(),
                params: Some(serde_json::json!(null)),
                ensemble: EnsembleSpec::Join {
                    target: Some(NegotiationTarget::new(d.peer_a, negotiation_id)),
                },
            },
        )
        .await,
    );

    ok(call(
        &d.host_a,
        &HostRequest::ExecAwait {
            exec_id: exec_a,
            until: AwaitState::Active,
        },
    )
    .await);

    let ResponseOk::Status(status) =
        ok(call(&d.host_a, &HostRequest::ExecStatus { exec_id: exec_a }).await)
    else {
        panic!("expected active execution status");
    };
    assert_eq!(status.lifecycle(), ExecLifecycle::Active);
    match ok(call(
        &d.host_a,
        &HostRequest::ExecView {
            exec: exec_a,
            width: 80,
            color: ColorDepth::Ansi16,
        },
    )
    .await)
    {
        ResponseOk::ExecView { view, .. } => {
            assert!(!view.slots.is_empty(), "view has at least one slot");
            assert!(
                view.slots.get(&Slot::Header).is_some_and(|s| !s.is_empty()),
                "view has a header"
            );
        }
        other => panic!("unexpected view response: {other:?}"),
    };

    let (_sa, _sb) = tokio::join!(drive(&d.host_a, exec_a), drive(&d.host_b, exec_b));

    for (socket, exec_id) in [(&d.host_a, exec_a), (&d.host_b, exec_b)] {
        let ResponseOk::Status(status) =
            ok(call(socket, &HostRequest::ExecStatus { exec_id }).await)
        else {
            panic!("expected completed execution status");
        };
        assert_eq!(status.lifecycle(), ExecLifecycle::Completed);
    }
    let mut terminal_views = Vec::new();
    for (socket, exec) in [(&d.host_a, exec_a), (&d.host_b, exec_b)] {
        match ok(call(
            socket,
            &HostRequest::ExecView {
                exec,
                width: 80,
                color: ColorDepth::Ansi16,
            },
        )
        .await)
        {
            ResponseOk::ExecView { step, view } => {
                assert!(step > 0);
                assert!(
                    view.slots
                        .get(&Slot::Header)
                        .is_some_and(|header| !header.is_empty())
                );
                terminal_views.push((step, view.slots));
            }
            other => panic!("unexpected terminal view: {other:?}"),
        }
    }
    assert_eq!(
        terminal_views[0], terminal_views[1],
        "same terminal shared-state view"
    );

    let missing = call(
        &d.host_a,
        &HostRequest::ExecView {
            exec: ExecId([0xFA; 32]),
            width: 80,
            color: ColorDepth::Ansi16,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(missing.code, arena0_api::ApiErrorCode::NotFound);
}

/// `events.subscribe` delivers Negotiation, Step, and Terminal frames for a driven
/// execution.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_subscribe_streams_negotiation_step_terminal() {
    let wasm = rps_wasm();
    let d = daemon(&wasm).await;

    // Subscribe on Host A before launching, so its Negotiating frame is captured.
    // Collect in the background until a Terminal frame arrives.
    let collector = tokio::spawn(collect_frames_unix(
        d.host_a.clone(),
        EventFilter {
            include: vec![],
            exclude: vec![],
        },
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (exec_a, negotiation_id) = match ok(call(
        &d.host_a,
        &HostRequest::ExecNew {
            exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
            program: d.program_id.to_string(),
            params: Some(serde_json::json!(null)),
            ensemble: EnsembleSpec::Explicit {
                peers: vec![d.peer_b],
            },
        },
    )
    .await)
    {
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
                params: Some(serde_json::json!(null)),
                ensemble: EnsembleSpec::Join {
                    target: Some(NegotiationTarget::new(d.peer_a, negotiation_id)),
                },
            },
        )
        .await,
    );
    let (_sa, _sb) = tokio::join!(drive(&d.host_a, exec_a), drive(&d.host_b, exec_b));

    let frames = collector.await.expect("collector joined");
    assert!(
        frames
            .iter()
            .any(|f| matches!(f.data, EventData::NegotiationStarted { .. })),
        "saw a negotiation frame"
    );
    assert!(
        frames
            .iter()
            .any(|f| matches!(f.data, EventData::SessionStep { .. })),
        "saw a step frame"
    );
    assert!(
        frames.iter().any(|f| matches!(
            f.data,
            EventData::SessionEnded { .. } | EventData::Terminated { .. }
        )),
        "saw a terminal frame"
    );
    // Step frames carry the enriched fields.
    let step = frames
        .iter()
        .find_map(|f| match &f.data {
            EventData::SessionStep {
                step, post_state, ..
            } => Some((*step, *post_state)),
            _ => None,
        })
        .expect("a step frame");
    assert_ne!(step.1.0, [0u8; 32], "Step carries a real post_state");
}

/// Subscribe on `socket` and collect frames until a Terminal arrives (or a timeout).
async fn collect_frames_unix(target: HostTarget, filter: EventFilter) -> Vec<EventFrame> {
    let mut stream = UnixStream::connect(&target.socket).await.expect("connect");
    let (read, mut write) = stream.split();
    let mut read = BufReader::new(read);
    let request = target.request(&HostRequest::EventsSubscribe { filter });
    arena0_api::frame::write_frame(&mut write, &request)
        .await
        .expect("write subscribe");
    // The ack.
    let _ack: Response = arena0_api::frame::read_frame(&mut read)
        .await
        .expect("read ack")
        .expect("ack frame");

    let mut frames = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let read_fut = arena0_api::frame::read_frame::<_, EventFrame>(&mut read);
        match tokio::time::timeout_at(deadline, read_fut).await {
            Ok(Ok(Some(frame))) => {
                let is_terminal = matches!(
                    frame.data,
                    EventData::SessionEnded { .. } | EventData::Terminated { .. }
                );
                frames.push(frame);
                if is_terminal {
                    return frames;
                }
            }
            _ => return frames,
        }
    }
}

/// ReceiptArtifact content addressing is stable; import is idempotent and lists a foreign
/// receipt with imported provenance.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn receipt_id_import_idempotence_and_list() {
    let wasm = rps_wasm();
    let d = daemon(&wasm).await;
    let (exec_a, negotiation_id) = match ok(call(
        &d.host_a,
        &HostRequest::ExecNew {
            exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
            program: d.program_id.to_string(),
            params: Some(serde_json::json!(null)),
            ensemble: EnsembleSpec::Explicit {
                peers: vec![d.peer_b],
            },
        },
    )
    .await)
    {
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
                params: Some(serde_json::json!(null)),
                ensemble: EnsembleSpec::Join {
                    target: Some(NegotiationTarget::new(d.peer_a, negotiation_id)),
                },
            },
        )
        .await,
    );
    let (sid, _sb) = tokio::join!(drive(&d.host_a, exec_a), drive(&d.host_b, exec_b));

    // Fetch A's receipt and confirm the content address is stable.
    let receipt: ReceiptArtifact = match ok(call(
        &d.host_a,
        &HostRequest::ReceiptGet {
            receipt: arena0_api::ReceiptRef::Produced(sid),
        },
    )
    .await)
    {
        ResponseOk::Receipt(r) => *r,
        other => panic!("unexpected: {other:?}"),
    };
    let rid = hex::encode(receipt.receipt_id().as_bytes());
    assert_eq!(
        rid,
        hex::encode(receipt.receipt_id().as_bytes()),
        "receipt_id is stable"
    );

    // The other Host already produced the same canonical artifact.
    let imported = import_one(&d.host_b, &receipt).await;
    assert_eq!(
        imported.provenance,
        arena0_api::ReceiptProvenance::Both,
        "identical local publication retains both provenance facts"
    );
    assert_eq!(imported.receipt_id, rid, "import keeps the content address");
    assert_eq!(imported.kind, receipt.kind());

    // Re-import is idempotent: still exactly one imported entry for that id.
    let _ = import_one(&d.host_b, &receipt).await;
    let list = match ok(call(&d.host_b, &HostRequest::ReceiptList).await) {
        ResponseOk::ReceiptList(v) => v,
        other => panic!("unexpected: {other:?}"),
    };
    let imported: Vec<_> = list
        .iter()
        .filter(|e| e.provenance == arena0_api::ReceiptProvenance::Both)
        .collect();
    assert_eq!(list.len(), 1, "canonical imports deduplicate across Hosts");
    assert_eq!(imported.len(), 1, "re-import is idempotent");
    assert_eq!(imported[0].receipt_id, rid);
    assert_eq!(imported[0].session_id, sid);
    let fetched = ok(call(
        &d.host_b,
        &HostRequest::ReceiptGet {
            receipt: arena0_api::ReceiptRef::Stored(receipt.receipt_id()),
        },
    )
    .await);
    let ResponseOk::Receipt(fetched) = fetched else {
        panic!("expected artifact by ID");
    };
    assert_eq!(fetched.encode().unwrap(), receipt.encode().unwrap());
}

async fn import_one(
    target: &HostTarget,
    receipt: &ReceiptArtifact,
) -> arena0_api::ReceiptListEntry {
    match ok(call(
        target,
        &HostRequest::ReceiptImport {
            receipt: Box::new(receipt.clone()),
        },
    )
    .await)
    {
        ResponseOk::ReceiptList(mut v) => v.pop().expect("one entry"),
        other => panic!("unexpected import response: {other:?}"),
    }
}

/// Program-handle resolution over the wire: name and full id resolve; a bogus name
/// is NotFound.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn program_handle_resolution_over_socket() {
    let wasm = rps_wasm();
    let d = daemon(&wasm).await;
    let target = &d.host_a;
    let program_id = d.program_id;

    // By exact name.
    match ok(call(
        target,
        &HostRequest::ProgramGet {
            program: "rock-paper-scissors".into(),
        },
    )
    .await)
    {
        ResponseOk::Program(detail) => assert_eq!(detail.summary.program_hash, program_id),
        other => panic!("unexpected: {other:?}"),
    }

    // By full content id.
    match ok(call(
        target,
        &HostRequest::ProgramGet {
            program: program_id.to_string(),
        },
    )
    .await)
    {
        ResponseOk::Program(detail) => assert_eq!(detail.summary.program_hash, program_id),
        other => panic!("unexpected: {other:?}"),
    }

    // A bogus handle is a precise NotFound error.
    let err = call(
        target,
        &HostRequest::ProgramGet {
            program: "does-not-exist".into(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, arena0_api::ApiErrorCode::NotFound);
}

/// The one Unix endpoint routes Host operations by the explicit outer Host
/// name. Invalid Host names return a typed error; missing required envelope
/// fields are rejected during deserialization and close the socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_endpoint_routes_multiple_hosts_and_rejects_invalid_host_calls() {
    let d = daemon(&rps_wasm()).await;

    let info_a = match ok(call(&d.host_a, &HostRequest::Info).await) {
        ResponseOk::HostStatus(status) => status.host,
        other => panic!("unexpected Host A info response: {other:?}"),
    };
    let info_b = match ok(call(&d.host_b, &HostRequest::Info).await) {
        ResponseOk::HostStatus(status) => status.host,
        other => panic!("unexpected Host B info response: {other:?}"),
    };
    assert_eq!(info_a.id, "a");
    assert_eq!(info_b.id, "b");
    assert_ne!(info_a.peer_id, info_b.peer_id);

    let exec_id = created(
        call(
            &d.host_a,
            &HostRequest::ExecNew {
                exec_id: ExecId([line!() as u8; 32]),
                program: d.program_id.to_string(),
                params: Some(serde_json::json!(null)),
                ensemble: EnsembleSpec::Explicit {
                    peers: vec![d.peer_b],
                },
            },
        )
        .await,
    );
    let wrong_host = call(&d.host_b, &HostRequest::ExecStatus { exec_id })
        .await
        .unwrap_err();
    assert_eq!(wrong_host.code, arena0_api::ApiErrorCode::NotFound);
    match ok(call(&d.host_a, &HostRequest::ExecStatus { exec_id }).await) {
        ResponseOk::Status(status) => assert_eq!(status.lifecycle(), ExecLifecycle::Negotiating),
        other => panic!("unexpected Host A status response: {other:?}"),
    }

    let malformed = common::call_json(
        &d.socket,
        &serde_json::json!({
            "method": "host.call",
            "params": {
                "host": "a/b",
                "request": {"method": "host.info"}
            }
        }),
    )
    .await;
    match malformed {
        Ok(Some(Err(error))) => assert_eq!(error.code, arena0_api::ApiErrorCode::BadRequest),
        other => panic!("malformed Host name must return BadRequest: {other:?}"),
    }

    let missing_host = common::call_json(
        &d.socket,
        &serde_json::json!({
            "method": "host.call",
            "params": {"request": {"method": "host.info"}}
        }),
    )
    .await;
    assert!(
        matches!(missing_host, Ok(None) | Err(_)),
        "missing Host must close the malformed request: {missing_host:?}"
    );
}
