//! End-to-end: two Hosts in one in-process daemon form a session through real
//! negotiation (one creator, one joiner), get driven through the shared Unix
//! socket with JSON answers (no hex), and produce matching, verifiable receipts
//! whose verify call returns the recovered evidence (program, ensemble, steps,
//! typed outcome).

mod common;

use arena0_api::{
    AwaitState, ColorDepth, EnsembleSpec, EventData, EventFilter, EventFrame, ExecLifecycle,
    FullVerifiedTerminal, HostRequest, LightVerifiedTerminal, NextEvent, ReceiptArtifact,
    ReceiptRef, Response, ResponseOk, VerifiedResult,
};
use arena0_protocol::{ExecId, NegotiationTarget, PendingId, SessionHash};
use arena0_sandbox::Program;
use common::{
    HostTarget, call, created, cumulative_sum_wasm, daemon, drive, drive_script, ok, rps_wasm,
};
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::UnixStream;

#[derive(Clone, Copy)]
struct ExecutionPair {
    exec_a: ExecId,
    exec_b: ExecId,
    callout_is_a: bool,
}
impl ExecutionPair {
    fn callout<'a>(&self, d: &'a common::DaemonHarness) -> (&'a HostTarget, ExecId) {
        if self.callout_is_a {
            (&d.host_a, self.exec_a)
        } else {
            (&d.host_b, self.exec_b)
        }
    }
    fn targets<'a>(&self, d: &'a common::DaemonHarness) -> [(&'a HostTarget, ExecId); 2] {
        [(&d.host_a, self.exec_a), (&d.host_b, self.exec_b)]
    }
}
type EventSubscription = (
    BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
);
type FirstCallout = (
    ExecutionPair,
    EventSubscription,
    PendingId,
    Option<SessionHash>,
    Vec<EventFrame>,
);
struct DrivenExecution {
    pair: ExecutionPair,
    first_pending_id: PendingId,
    sessions: [SessionHash; 2],
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_hosts_play_and_verify() {
    let wasm = rps_wasm();
    let d = daemon(&wasm).await;
    let (pair, events) = create_and_activate(&d).await;
    let first = observe_first_callout(&d, &wasm, pair, events).await;
    let run = race_and_drive(&d, first).await;
    assert_terminal(&d, &run).await;
    assert_receipts(&d, &run).await;
}
async fn subscribe_events(target: &HostTarget) -> EventSubscription {
    let (read, mut write) = UnixStream::connect(&target.socket)
        .await
        .expect("connect to Host event stream")
        .into_split();
    let mut read = BufReader::new(read);
    arena0_api::frame::write_frame(
        &mut write,
        &target.request(&HostRequest::EventsSubscribe {
            filter: EventFilter {
                include: Vec::new(),
                exclude: Vec::new(),
            },
        }),
    )
    .await
    .expect("write event subscription");
    let ack: Response = arena0_api::frame::read_frame(&mut read)
        .await
        .expect("read event subscription ack")
        .expect("event subscription ack frame");
    assert!(matches!(ack, Ok(ResponseOk::Subscribed)));
    (read, write)
}
async fn await_lifecycle(
    target: &HostTarget,
    exec_id: ExecId,
    until: AwaitState,
    expected: ExecLifecycle,
    phase: &str,
) {
    match ok(call(target, &HostRequest::ExecAwait { exec_id, until }).await) {
        ResponseOk::Awaited { exec_state, .. } => {
            assert_eq!(exec_state, expected, "{phase} lifecycle")
        }
        other => panic!("unexpected {phase} await response: {other:?}"),
    }
}
async fn execution_view(target: &HostTarget, exec_id: ExecId) -> (u64, arena0_protocol::View) {
    let request = HostRequest::ExecView {
        exec: exec_id,
        width: 80,
        color: ColorDepth::Ansi16,
    };
    match ok(call(target, &request).await) {
        ResponseOk::ExecView { step, view } => (step, view),
        other => panic!("unexpected execution view response: {other:?}"),
    }
}
async fn create_and_activate(d: &common::DaemonHarness) -> (ExecutionPair, EventSubscription) {
    let requested_exec_a = ExecId([0xa1; 32]);
    let requested_exec_b = ExecId([0xb1; 32]);
    let callout_is_a = d.peer_a < d.peer_b;
    let callout_host = if callout_is_a { &d.host_a } else { &d.host_b };
    // Subscribe before creation so the real Unix stream observes the source
    // events, then create, join, and sample both lifecycle boundaries.
    let events = subscribe_events(callout_host).await;
    let (exec_a, negotiation_id) = match ok(call(
        &d.host_a,
        &HostRequest::ExecNew {
            exec_id: requested_exec_a,
            program: d.program_id.to_string(),
            params: Some(serde_json::json!(null)),
            ensemble: EnsembleSpec::Create {
                participant_count: 2,
            },
        },
    )
    .await)
    {
        ResponseOk::ExecCreated {
            exec_id,
            negotiation_id: Some(negotiation_id),
            exec_state,
            ..
        } => {
            assert_eq!(exec_id, requested_exec_a);
            assert_eq!(
                exec_state,
                ExecLifecycle::Negotiating,
                "launch is non-blocking"
            );
            (exec_id, negotiation_id)
        }
        other => panic!("unexpected creator response: {other:?}"),
    };
    let ResponseOk::Status(status) =
        ok(call(&d.host_a, &HostRequest::ExecStatus { exec_id: exec_a }).await)
    else {
        panic!("expected negotiating execution status");
    };
    assert_eq!(status.lifecycle(), ExecLifecycle::Negotiating);
    let negotiating_view = call(
        &d.host_a,
        &HostRequest::ExecView {
            exec: exec_a,
            width: 80,
            color: ColorDepth::Ansi16,
        },
    )
    .await
    .expect_err("view is unavailable before activation");
    assert_eq!(
        negotiating_view.code,
        arena0_api::ApiErrorCode::Execution,
        "negotiating view has a typed execution error"
    );
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
    assert_eq!(exec_b, requested_exec_b);
    await_lifecycle(
        &d.host_a,
        exec_a,
        AwaitState::Active,
        ExecLifecycle::Active,
        "await Active",
    )
    .await;
    let ResponseOk::Status(status) =
        ok(call(&d.host_a, &HostRequest::ExecStatus { exec_id: exec_a }).await)
    else {
        panic!("expected active execution status");
    };
    assert_eq!(status.lifecycle(), ExecLifecycle::Active);
    let (_, view) = execution_view(&d.host_a, exec_a).await;
    assert!(!view.slots.is_empty(), "active view has at least one slot");
    assert!(
        view.slots
            .get(&arena0_protocol::Slot::Header)
            .is_some_and(|header| !header.is_empty()),
        "active view has a header"
    );
    (
        ExecutionPair {
            exec_a,
            exec_b,
            callout_is_a,
        },
        events,
    )
}
async fn observe_first_callout(
    d: &common::DaemonHarness,
    wasm: &[u8],
    pair: ExecutionPair,
    events: EventSubscription,
) -> FirstCallout {
    let (callout_host, callout_exec) = pair.callout(d);
    let (mut event_read, event_write) = events;
    let next_request = HostRequest::ExecNext {
        exec_id: callout_exec,
    };
    let next = call(callout_host, &next_request);
    let mut event_prefix = Vec::new();
    let event = async {
        loop {
            let frame = arena0_api::frame::read_frame::<_, EventFrame>(&mut event_read)
                .await
                .expect("read session.callout")
                .expect("session.callout frame");
            if frame.exec_id == Some(callout_exec)
                && matches!(&frame.data, EventData::SessionCallout { .. })
            {
                return frame;
            }
            event_prefix.push(frame);
        }
    };
    let (next, event) = tokio::time::timeout(Duration::from_secs(120), async {
        tokio::join!(next, event)
    })
    .await
    .expect("timed out waiting for ExecNext and session.callout");
    let mut event_frames = event_prefix;
    event_frames.push(event.clone());
    let EventFrame {
        data: event_data,
        exec_id: event_exec_id,
        session_id: event_session_id,
        ..
    } = event;
    let (pending_id, event_session_id) = match (ok(next), event_data) {
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
            assert_eq!(next_pending_id, event_pending_id);
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
            (next_pending_id, event_session_id)
        }
        (next, event) => panic!("unexpected first callout projections: {next:?}, {event:?}"),
    };
    (
        pair,
        (event_read, event_write),
        pending_id,
        event_session_id,
        event_frames,
    )
}
async fn race_and_drive(d: &common::DaemonHarness, first: FirstCallout) -> DrivenExecution {
    let (pair, events, pending_id, event_session_id, mut event_frames) = first;
    let (callout_host, callout_exec) = pair.callout(d);
    let (event_read, event_write) = events;
    let event_collector = tokio::spawn(collect_event_frames(event_read, callout_exec));
    let race_request = HostRequest::ExecSubmit {
        exec_id: callout_exec,
        pending_id,
        answer: Some(serde_json::json!("Rock")),
    };
    let (left, right) = tokio::join!(
        call(callout_host, &race_request),
        call(callout_host, &race_request)
    );
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
    assert_eq!(
        conflict.code,
        arena0_api::ApiErrorCode::CalloutNotPending,
        "the losing answer has a typed conflict"
    );
    let p0_answers = [serde_json::json!("Rock")];
    let p1_answers = [serde_json::json!("Scissors"), serde_json::json!("Scissors")];
    let (answers_a, answers_b) = if pair.callout_is_a {
        (&p0_answers[..], &p1_answers[..])
    } else {
        (&p1_answers[..], &p0_answers[..])
    };
    let (sid_a, sid_b, collected) = tokio::join!(
        drive_script(&d.host_a, pair.exec_a, answers_a),
        drive_script(&d.host_b, pair.exec_b, answers_b),
        event_collector
    );
    event_frames.extend(collected.expect("event collector joined"));
    assert_eq!(sid_a, sid_b, "both parties confirmed the same session");
    assert_eq!(event_session_id, Some(sid_a));
    assert_event_sequence(&event_frames, callout_exec);
    drop(event_write);
    DrivenExecution {
        pair,
        first_pending_id: pending_id,
        sessions: [sid_a, sid_b],
    }
}
fn assert_event_sequence(frames: &[EventFrame], exec_id: ExecId) {
    let position = |predicate: &dyn Fn(&EventData) -> bool| {
        frames
            .iter()
            .position(|frame| frame.exec_id == Some(exec_id) && predicate(&frame.data))
    };
    let [Some(negotiation), Some(callout), Some(step), Some(terminal)] = [
        position(&|data| matches!(data, EventData::NegotiationStarted { .. })),
        position(&|data| matches!(data, EventData::SessionCallout { .. })),
        position(&|data| matches!(data, EventData::SessionStep { .. })),
        position(&|data| {
            matches!(
                data,
                EventData::SessionEnded { .. } | EventData::Terminated { .. }
            )
        }),
    ] else {
        panic!("missing correlated negotiation, callout, step, or terminal event");
    };
    assert!(
        negotiation < callout && negotiation < step && callout < terminal && step < terminal,
        "execution events retain negotiation, callout, step, terminal order"
    );
    assert!(
        frames.windows(2).all(|pair| pair[0].seq < pair[1].seq),
        "correlated event sequence numbers are strictly increasing"
    );
    let post_state = frames
        .iter()
        .find_map(|frame| match &frame.data {
            EventData::SessionStep { post_state, .. } if frame.exec_id == Some(exec_id) => {
                Some(*post_state)
            }
            _ => None,
        })
        .expect("a correlated session step frame");
    assert_ne!(post_state.0, [0u8; 32], "step carries a real post_state");
}
async fn assert_terminal(d: &common::DaemonHarness, run: &DrivenExecution) {
    for (target, exec_id) in run.pair.targets(d) {
        await_lifecycle(
            target,
            exec_id,
            AwaitState::Terminal,
            ExecLifecycle::Completed,
            "await Terminal",
        )
        .await;
    }
    // Let terminal cleanup settle before testing stale handles.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (callout_host, callout_exec) = run.pair.callout(d);
    let stale = call(
        callout_host,
        &HostRequest::ExecSubmit {
            exec_id: callout_exec,
            pending_id: run.first_pending_id,
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
            pending_id: run.first_pending_id,
            answer: Some(serde_json::json!("Rock")),
        },
    )
    .await
    .expect_err("unknown execution must remain NotFound");
    assert_eq!(missing.code, arena0_api::ApiErrorCode::NotFound);
    for (target, exec_id) in run.pair.targets(d) {
        match ok(call(target, &HostRequest::ExecStatus { exec_id }).await) {
            ResponseOk::Status(status) => {
                assert_eq!(status.lifecycle(), ExecLifecycle::Completed);
                assert_eq!(status.session_id(), Some(run.sessions[0]));
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
    let mut terminal_views = Vec::new();
    for (target, exec_id) in run.pair.targets(d) {
        let (step, view) = execution_view(target, exec_id).await;
        assert!(step > 0, "terminal view has a committed step");
        assert!(
            view.slots
                .get(&arena0_protocol::Slot::Header)
                .is_some_and(|header| !header.is_empty()),
            "terminal view has a header"
        );
        terminal_views.push((step, view.slots));
    }
    assert_eq!(
        terminal_views[0], terminal_views[1],
        "both Hosts render the same terminal shared-state view"
    );
    let missing_view = call(
        &d.host_a,
        &HostRequest::ExecView {
            exec: ExecId([0xFA; 32]),
            width: 80,
            color: ColorDepth::Ansi16,
        },
    )
    .await
    .expect_err("unknown execution view must be rejected");
    assert_eq!(missing_view.code, arena0_api::ApiErrorCode::NotFound);
}
async fn assert_receipts(d: &common::DaemonHarness, run: &DrivenExecution) {
    let mut artifacts = Vec::new();
    for ((target, _), sid) in run.pair.targets(d).into_iter().zip(run.sessions) {
        let ResponseOk::Receipt(receipt) = ok(call(
            target,
            &HostRequest::ReceiptGet {
                receipt: ReceiptRef::Produced(sid),
            },
        )
        .await) else {
            panic!("expected a receipt");
        };
        artifacts.push(*receipt);
    }
    assert_eq!(
        artifacts[0].body().trace(),
        artifacts[1].body().trace(),
        "both Hosts publish the same public trace"
    );
    let canonical_id = artifacts[0].receipt_id();
    assert_eq!(
        artifacts[1].receipt_id(),
        canonical_id,
        "both Hosts publish the same content identity"
    );
    let canonical_bytes = artifacts[0].encode().unwrap();
    assert_eq!(
        artifacts[1].encode().unwrap(),
        canonical_bytes,
        "both Hosts publish identical canonical bytes"
    );
    for ((target, _), sid) in run.pair.targets(d).into_iter().zip(run.sessions) {
        assert_verified(target, sid, false).await;
        assert_verified(target, sid, true).await;
    }
    let imported = import_one(&d.host_b, &artifacts[0]).await;
    let canonical_hex = hex::encode(canonical_id.as_bytes());
    assert_eq!(
        imported.provenance,
        arena0_api::ReceiptProvenance::Both,
        "identical local publication retains both provenance facts"
    );
    assert_eq!(
        imported.receipt_id, canonical_hex,
        "import keeps the content address"
    );
    assert_eq!(imported.kind, artifacts[0].kind());
    let _ = import_one(&d.host_b, &artifacts[0]).await;
    let list = match ok(call(&d.host_b, &HostRequest::ReceiptList).await) {
        ResponseOk::ReceiptList(v) => v,
        other => panic!("unexpected receipt list response: {other:?}"),
    };
    let imported_entries: Vec<_> = list
        .iter()
        .filter(|entry| entry.provenance == arena0_api::ReceiptProvenance::Both)
        .collect();
    assert_eq!(list.len(), 1, "canonical imports deduplicate across Hosts");
    assert_eq!(imported_entries.len(), 1, "re-import is idempotent");
    assert_eq!(imported_entries[0].receipt_id, canonical_hex);
    assert_eq!(imported_entries[0].session_id, run.sessions[0]);
    assert_eq!(imported_entries[0].kind, artifacts[0].kind());
    let fetched = ok(call(
        &d.host_b,
        &HostRequest::ReceiptGet {
            receipt: ReceiptRef::Stored(canonical_id),
        },
    )
    .await);
    let ResponseOk::Receipt(fetched) = fetched else {
        panic!("expected artifact by ID");
    };
    assert_eq!(
        fetched.encode().unwrap(),
        canonical_bytes,
        "stored receipt retrieval preserves exact bytes"
    );
    // Removing the certified terminal entry must invalidate a nonempty prefix.
    let mut trace = artifacts[0].body().trace().to_vec();
    assert!(
        trace.len() > 1,
        "fixture must have a nonempty public prefix"
    );
    trace.pop();
    let body = arena0_protocol::ReceiptBody::new(
        artifacts[0].body().header().clone(),
        artifacts[0].body().outcome().to_vec(),
        artifacts[0].body().params().to_vec(),
        trace,
    )
    .expect("shape-only body assembly");
    assert!(
        arena0_protocol::ReceiptArtifact::new(body).is_err(),
        "missing certified terminal trace entry is rejected"
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

async fn collect_event_frames<R>(
    mut read: BufReader<R>,
    exec_id: arena0_protocol::ExecId,
) -> Vec<EventFrame>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let mut frames = Vec::new();
    loop {
        let frame = tokio::time::timeout_at(
            deadline,
            arena0_api::frame::read_frame::<_, EventFrame>(&mut read),
        )
        .await
        .expect("timed out waiting for terminal event")
        .expect("read execution event")
        .expect("execution event frame");
        let is_current_execution = frame.exec_id == Some(exec_id);
        let is_terminal = is_current_execution
            && matches!(
                &frame.data,
                EventData::SessionEnded { .. } | EventData::Terminated { .. }
            );
        if is_current_execution {
            frames.push(frame);
        }
        if is_terminal {
            return frames;
        }
    }
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
        ResponseOk::ReceiptList(mut entries) => entries.pop().expect("one receipt entry"),
        other => panic!("unexpected import response: {other:?}"),
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
        ensemble: EnsembleSpec::Create {
            participant_count: 2,
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
