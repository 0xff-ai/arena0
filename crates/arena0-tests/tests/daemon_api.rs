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
