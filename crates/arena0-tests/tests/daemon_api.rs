//! The runtime-API surface over the unix socket: non-blocking launch + `exec.await`,
//! the `events.subscribe` stream, receipt content addressing / import / list, and
//! program-handle resolution over the wire.

mod common;

use std::path::Path;
use std::time::Duration;

use arena0_api::{
    AwaitState, ColorDepth, EnsembleSpec, EventData, EventFilter, EventFrame, ExecLifecycle,
    ReceiptArtifact, Request, Response, ResponseOk,
};
use arena0_protocol::{ExecId, Slot};
use common::{call, created, drive, ok, rps_wasm, two_daemons};
use tokio::io::BufReader;
use tokio::net::UnixStream;

async fn next_from_either(
    socket_a: &Path,
    exec_a: ExecId,
    socket_b: &Path,
    exec_b: ExecId,
) -> (bool, Response) {
    let request_a = Request::ExecNext { exec_id: exec_a };
    let request_b = Request::ExecNext { exec_id: exec_b };
    let mut next_a = Box::pin(call(socket_a, &request_a));
    let mut next_b = Box::pin(call(socket_b, &request_b));
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
    let d = two_daemons(&wasm).await;

    let resp_a = call(
        &d.sock_a,
        &Request::ExecNew {
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
            negotiation_id,
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
    .await;
    let exec_b = created(resp_b);

    // Await Active: resolves once the session confirms and starts (no driving needed).
    match ok(call(
        &d.sock_a,
        &Request::ExecAwait {
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
    let (_sa, _sb) = tokio::join!(drive(&d.sock_a, exec_a), drive(&d.sock_b, exec_b));
    match ok(call(
        &d.sock_a,
        &Request::ExecAwait {
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

    match ok(call(&d.sock_a, &Request::ExecStatus { exec_id: exec_a }).await) {
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
    let d = two_daemons(&wasm).await;
    let (exec_a, negotiation_id) = match ok(call(
        &d.sock_a,
        &Request::ExecNew {
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
            negotiation_id,
            ..
        } => (exec_id, negotiation_id),
        other => panic!("unexpected creator response: {other:?}"),
    };
    let exec_b = created(
        call(
            &d.sock_b,
            &Request::ExecNew {
                exec_id: ExecId([line!() as u8; 32]),
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

    let pending_id = match ok(call(&d.sock_a, &Request::ExecNext { exec_id: exec_a }).await) {
        ResponseOk::Next(arena0_api::NextEvent::Callout { pending_id, .. }) => pending_id,
        ResponseOk::Next(arena0_api::NextEvent::Failed { reason }) => {
            panic!("execution failed before callout: {reason}")
        }
        other => panic!("unexpected event before callout: {other:?}"),
    };

    let request = Request::ExecSubmit {
        exec_id: exec_a,
        pending_id,
        answer: Some(serde_json::json!("Rock")),
    };
    let (left, right) = tokio::join!(call(&d.sock_a, &request), call(&d.sock_a, &request));
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

    let (_session_a, _session_b) = tokio::join!(drive(&d.sock_a, exec_a), drive(&d.sock_b, exec_b));
    match ok(call(&d.sock_a, &Request::ExecStatus { exec_id: exec_a }).await) {
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
    let d = two_daemons(&rps_wasm()).await;
    let (exec_a, negotiation_id) = match ok(call(
        &d.sock_a,
        &Request::ExecNew {
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
            negotiation_id,
            ..
        } => (exec_id, negotiation_id),
        other => panic!("unexpected creator response: {other:?}"),
    };
    let exec_b = created(
        call(
            &d.sock_b,
            &Request::ExecNew {
                exec_id: ExecId([line!() as u8; 32]),
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

    let (human_socket, human_exec, final_pending) = loop {
        let (is_a, response) = next_from_either(&d.sock_a, exec_a, &d.sock_b, exec_b).await;
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
        let other_socket = if is_a { &d.sock_a } else { &d.sock_b };
        let other_exec = if is_a { exec_a } else { exec_b };
        ok(call(
            other_socket,
            &Request::ExecSubmit {
                exec_id: other_exec,
                pending_id,
                answer: Some(serde_json::json!("Rock")),
            },
        )
        .await);

        if round == 3 {
            let human_socket = if is_a { &d.sock_b } else { &d.sock_a };
            let human_exec = if is_a { exec_b } else { exec_a };
            let final_pending = match ok(call(
                human_socket,
                &Request::ExecNext {
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
                human_socket,
                &Request::ExecSubmit {
                    exec_id: human_exec,
                    pending_id: final_pending,
                    answer: Some(serde_json::json!("Rock")),
                },
            )
            .await);
            break (human_socket, human_exec, final_pending);
        }
    };

    let await_a = Request::ExecAwait {
        exec_id: exec_a,
        until: AwaitState::Terminal,
    };
    let await_b = Request::ExecAwait {
        exec_id: exec_b,
        until: AwaitState::Terminal,
    };
    let (terminal_a, terminal_b) =
        tokio::join!(call(&d.sock_a, &await_a), call(&d.sock_b, &await_b));
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
        human_socket,
        &Request::ExecSubmit {
            exec_id: human_exec,
            pending_id: final_pending,
            answer: Some(serde_json::json!("Rock")),
        },
    )
    .await
    .expect_err("completed callout must be rejected");
    assert_eq!(stale.code, arena0_api::ApiErrorCode::CalloutNotPending);

    let missing = call(
        &d.sock_a,
        &Request::ExecSubmit {
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
async fn hosts_list_is_complete_from_one_host_socket() {
    let d = two_daemons(&rps_wasm()).await;
    match ok(call(&d.sock_a, &Request::HostsList).await) {
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
    let d = two_daemons(&wasm).await;
    let exec_id = created(
        call(
            &d.sock_a,
            &Request::ExecNew {
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
        call(&d.sock_a, &Request::ExecWithdraw { exec_id }).await,
        Ok(ResponseOk::Ack)
    ));
    assert!(matches!(
        call(&d.sock_a, &Request::ExecWithdraw { exec_id }).await,
        Ok(ResponseOk::Ack)
    ));

    match ok(call(&d.sock_a, &Request::ExecStatus { exec_id }).await) {
        ResponseOk::Status(status) => assert_eq!(status.lifecycle(), ExecLifecycle::Failed),
        other => panic!("unexpected status after withdrawal: {other:?}"),
    }
    match ok(call(&d.sock_a, &Request::ExecNext { exec_id }).await) {
        ResponseOk::Next(arena0_api::NextEvent::Failed { reason }) => {
            assert_eq!(reason, "negotiation withdrawn locally")
        }
        other => panic!("unexpected next event after withdrawal: {other:?}"),
    }
}

/// `exec.view` is served only while an execution is Active.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_view_distinguishes_negotiating_active_terminal_and_missing_executions() {
    let wasm = rps_wasm();
    let d = two_daemons(&wasm).await;

    let (exec_a, negotiation_id) = match ok(call(
        &d.sock_a,
        &Request::ExecNew {
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
            negotiation_id,
            ..
        } => (exec_id, negotiation_id),
        other => panic!("unexpected creator response: {other:?}"),
    };
    let ResponseOk::Status(status) =
        ok(call(&d.sock_a, &Request::ExecStatus { exec_id: exec_a }).await)
    else {
        panic!("expected negotiating execution status");
    };
    assert_eq!(status.lifecycle(), ExecLifecycle::Negotiating);
    let negotiating = call(
        &d.sock_a,
        &Request::ExecView {
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

    ok(call(
        &d.sock_a,
        &Request::ExecAwait {
            exec_id: exec_a,
            until: AwaitState::Active,
        },
    )
    .await);

    let ResponseOk::Status(status) =
        ok(call(&d.sock_a, &Request::ExecStatus { exec_id: exec_a }).await)
    else {
        panic!("expected active execution status");
    };
    assert_eq!(status.lifecycle(), ExecLifecycle::Active);
    match ok(call(
        &d.sock_a,
        &Request::ExecView {
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

    let (_sa, _sb) = tokio::join!(drive(&d.sock_a, exec_a), drive(&d.sock_b, exec_b));

    for (socket, exec_id) in [(&d.sock_a, exec_a), (&d.sock_b, exec_b)] {
        let ResponseOk::Status(status) = ok(call(socket, &Request::ExecStatus { exec_id }).await)
        else {
            panic!("expected completed execution status");
        };
        assert_eq!(status.lifecycle(), ExecLifecycle::Completed);
    }
    let terminal = call(
        &d.sock_a,
        &Request::ExecView {
            exec: exec_a,
            width: 80,
            color: ColorDepth::Ansi16,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(terminal.code, arena0_api::ApiErrorCode::Execution);

    let missing = call(
        &d.sock_a,
        &Request::ExecView {
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
    let d = two_daemons(&wasm).await;

    // Subscribe on daemon A (node-wide) before launching, so the Negotiating frame is
    // captured. Collect in the background until a Terminal frame arrives.
    let collector = tokio::spawn(collect_frames_unix(
        d.sock_a.clone(),
        EventFilter {
            include: vec![],
            exclude: vec![],
        },
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (exec_a, negotiation_id) = match ok(call(
        &d.sock_a,
        &Request::ExecNew {
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
    let (_sa, _sb) = tokio::join!(drive(&d.sock_a, exec_a), drive(&d.sock_b, exec_b));

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
async fn collect_frames_unix(socket: std::path::PathBuf, filter: EventFilter) -> Vec<EventFrame> {
    let mut stream = UnixStream::connect(&socket).await.expect("connect");
    let (read, mut write) = stream.split();
    let mut read = BufReader::new(read);
    arena0_api::frame::write_frame(&mut write, &Request::EventsSubscribe { filter })
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
    let d = two_daemons(&wasm).await;
    let (exec_a, negotiation_id) = match ok(call(
        &d.sock_a,
        &Request::ExecNew {
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
    let (sid, _sb) = tokio::join!(drive(&d.sock_a, exec_a), drive(&d.sock_b, exec_b));

    // Fetch A's receipt and confirm the content address is stable.
    let receipt: ReceiptArtifact = match ok(call(
        &d.sock_a,
        &Request::ReceiptGet {
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
    let imported = import_one(&d.sock_b, &receipt).await;
    assert_eq!(
        imported.provenance,
        arena0_api::ReceiptProvenance::Both,
        "identical local publication retains both provenance facts"
    );
    assert_eq!(imported.receipt_id, rid, "import keeps the content address");
    assert_eq!(imported.kind, receipt.kind());

    // Re-import is idempotent: still exactly one imported entry for that id.
    let _ = import_one(&d.sock_b, &receipt).await;
    let list = match ok(call(&d.sock_b, &Request::ReceiptList).await) {
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
        &d.sock_b,
        &Request::ReceiptGet {
            receipt: arena0_api::ReceiptRef::Stored(receipt.receipt_id()),
        },
    )
    .await);
    let ResponseOk::Receipt(fetched) = fetched else {
        panic!("expected artifact by ID");
    };
    assert_eq!(fetched.encode().unwrap(), receipt.encode().unwrap());
}

async fn import_one(socket: &Path, receipt: &ReceiptArtifact) -> arena0_api::ReceiptListEntry {
    match ok(call(
        socket,
        &Request::ReceiptImport {
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
    let d = two_daemons(&wasm).await;
    let sock = d.sock_a.as_path();
    let program_id = d.program_id;

    // By exact name.
    match ok(call(
        sock,
        &Request::ProgramGet {
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
        sock,
        &Request::ProgramGet {
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
        sock,
        &Request::ProgramGet {
            program: "does-not-exist".into(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, arena0_api::ApiErrorCode::NotFound);
}
