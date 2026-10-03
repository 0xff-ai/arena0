//! The runtime-API surface over the unix socket: non-blocking launch + `exec.await`,
//! the `events.subscribe` stream, receipt content addressing / import / list, and
//! program-handle resolution over the wire.

mod common;

use std::time::Duration;

use arena0_api::{
    AwaitState, ColorDepth, EnsembleSpec, EventData, EventFilter, EventFrame, ExecLifecycle,
    HostRequest, NextEvent, ReceiptArtifact, Request, Response, ResponseOk,
};
use arena0_protocol::{
    Block, CalloutId, Cell, ExecId, Fact, NegotiationTarget, RosterEntry, Slot, Tone, View,
};
use arena0_tests::fixtures::{LIVE_EXECUTION_TIMEOUT, view_program_wasm};
use common::{
    DaemonHarness, HostTarget, call, call_daemon, chess_wasm, created, cumulative_sum_wasm, daemon,
    drive, ok, prisoner_dilemma_wasm, rps_wasm, timer_dispatch_wasm,
};
use tokio::io::BufReader;
use tokio::net::{UnixStream, unix::OwnedReadHalf, unix::OwnedWriteHalf};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_carries_callout_details_and_terminal_facts() {
    let d = daemon(&rps_wasm()).await;
    let (a, b) = launch_pair(&d, Some(serde_json::Value::Null), None).await;
    let (from_a, _) = next_from_either(&d.host_a, a, &d.host_b, b).await;
    let (host, exec_id) = if from_a {
        (&d.host_a, a)
    } else {
        (&d.host_b, b)
    };
    {
        let ResponseOk::Next(NextEvent::Callout {
            pending_id,
            callout_index,
            name,
            prompt,
            schema,
            context,
        }) = ok(call(host, &HostRequest::ExecNext { exec_id }).await)
        else {
            panic!("expected callout");
        };
        let expected = arena0_api::PendingCalloutStatus {
            pending_id,
            callout_index,
            name,
            prompt,
            schema,
            context,
        };
        let ResponseOk::Status(status) = ok(call(host, &HostRequest::ExecStatus { exec_id }).await)
        else {
            panic!("expected status");
        };
        assert_eq!(status.pending_callout(), Some(&expected));
        let ResponseOk::ExecList(entries) = ok(call(host, &HostRequest::ExecList).await) else {
            panic!("expected list");
        };
        let entry = entries
            .iter()
            .find(|entry| entry.exec_id == exec_id)
            .unwrap();
        let summary = entry
            .pending_callout
            .as_ref()
            .expect("pending callout summary");
        assert_eq!(summary.pending_id, expected.pending_id);
        assert_eq!(summary.callout_index, expected.callout_index);
        assert_eq!(summary.name, expected.name);
        let ResponseOk::Inspection(inspection) = ok(call(
            host,
            &HostRequest::ExecInspect {
                exec_id,
                events_from: None,
                events_limit: 16,
            },
        )
        .await) else {
            panic!("expected inspection");
        };
        assert!(entry.activation.is_some());
        assert_eq!(entry.activation, inspection.activation);
    }
    tokio::join!(drive(&d.host_a, a), drive(&d.host_b, b));
    for (host, exec_id) in [(&d.host_a, a), (&d.host_b, b)] {
        let ResponseOk::Next(NextEvent::Completed { outcome, .. }) =
            ok(call(host, &HostRequest::ExecNext { exec_id }).await)
        else {
            panic!("expected completion");
        };
        let ResponseOk::Status(status) = ok(call(host, &HostRequest::ExecStatus { exec_id }).await)
        else {
            panic!("expected status");
        };
        let arena0_api::ExecStatusState::Completed {
            outcome: actual, ..
        } = status.state
        else {
            panic!("expected completed status");
        };
        assert_eq!(actual, outcome);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aborted_status_carries_reason() {
    let d = daemon(&rps_wasm()).await;
    let (a, _) = launch_pair(&d, Some(serde_json::Value::Null), None).await;
    await_active(&d.host_a, a).await;
    let reason = "API termination reason";
    ok(call(
        &d.host_a,
        &HostRequest::ExecTerminate {
            exec_id: a,
            reason: reason.into(),
        },
    )
    .await);
    ok(call(
        &d.host_a,
        &HostRequest::ExecAwait {
            exec_id: a,
            until: AwaitState::Terminal,
        },
    )
    .await);
    let ResponseOk::Status(status) =
        ok(call(&d.host_a, &HostRequest::ExecStatus { exec_id: a }).await)
    else {
        panic!("expected status");
    };
    match status.state {
        arena0_api::ExecStatusState::Aborted { reason: actual, .. }
        | arena0_api::ExecStatusState::Failed {
            reason: Some(actual),
            ..
        } => assert_eq!(actual, reason),
        other => panic!("expected abort with reason, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trace_decodes_messages() {
    let d = daemon(&rps_wasm()).await;
    let (a, b) = complete_session(&d, Some(serde_json::Value::Null), None).await;
    for (host, exec_id) in [(&d.host_a, a), (&d.host_b, b)] {
        let ResponseOk::Trace(steps) = ok(call(
            host,
            &HostRequest::ExecTrace {
                exec_id,
                from: 0,
                to: u64::MAX,
            },
        )
        .await) else {
            panic!("expected trace");
        };
        assert_eq!(steps[0].entry.step, 0);
        assert_eq!(steps[0].message, None);
        let mut messages = 0;
        for step in steps {
            if matches!(step.entry.event, arena0_protocol::StepEvent::Message { .. }) {
                messages += 1;
                assert!(matches!(
                    step.message,
                    Some(arena0_api::DecodedMessage::Json(_))
                ));
            }
        }
        assert!(messages > 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn end_progress_events_follow_the_handshake() {
    let d = daemon(&rps_wasm()).await;
    let (mut events_a, _write_a) = subscribe_events(&d.host_a).await;
    let (mut events_b, _write_b) = subscribe_events(&d.host_b).await;
    let (a, b) = complete_session(&d, Some(serde_json::Value::Null), None).await;
    for (host, exec_id, events) in [(&d.host_a, a, &mut events_a), (&d.host_b, b, &mut events_b)] {
        let end = tokio::time::timeout(LIVE_EXECUTION_TIMEOUT, async {
            loop {
                let frame = read_event(events).await;
                if let EventData::SessionEndProgress { phase, unconfirmed } = frame.data
                    && phase == arena0_api::ExecEndPhase::Ended
                {
                    break arena0_api::ExecEndStatus { phase, unconfirmed };
                }
            }
        })
        .await
        .expect("end handshake progress");
        let ResponseOk::Status(status) = ok(call(host, &HostRequest::ExecStatus { exec_id }).await)
        else {
            panic!("expected status");
        };
        assert_eq!(end, status.end);
    }
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offers_list_open_offers_from_other_hosts() {
    let d = daemon(&rps_wasm()).await;
    let (mut events, _write) = subscribe_events(&d.host_b).await;
    let (_, negotiation_id) = create_on_host_a(&d).await;
    tokio::time::timeout(LIVE_EXECUTION_TIMEOUT, async {
        loop {
            if let EventData::OfferSeen {
                program_id,
                negotiation_id: seen,
                creator,
                ..
            } = read_event(&mut events).await.data
                && seen == negotiation_id
            {
                assert_eq!(program_id, d.program_id);
                assert_eq!(creator, d.peer_a);
                break;
            }
        }
    })
    .await
    .expect("Host b discovers a's offer without a Join");
    let ResponseOk::Offers(offers) = ok(call(&d.host_b, &HostRequest::NegotiationOffers).await)
    else {
        panic!("expected open offers");
    };
    let offer = offers
        .iter()
        .find(|offer| offer.negotiation_id == negotiation_id)
        .expect("announced offer is listed");
    assert_eq!(offer.program_id, d.program_id);
    assert_eq!(offer.creator, d.peer_a);
    assert_eq!(offer.target_size, 2);
    assert_eq!(offer.params, serde_json::Value::Null);
    assert!(offer.first_seen_ms < offer.deadline_unix_ms);
    let ResponseOk::Offers(own) = ok(call(&d.host_a, &HostRequest::NegotiationOffers).await) else {
        panic!("expected open offers");
    };
    assert!(
        !own.iter()
            .any(|offer| offer.negotiation_id == negotiation_id)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn joined_offer_closes_as_complete() {
    let d = daemon(&rps_wasm()).await;
    let (mut events, _write) = subscribe_events(&d.host_b).await;
    let (_, negotiation_id) = create_on_host_a(&d).await;
    tokio::time::timeout(LIVE_EXECUTION_TIMEOUT, async {
        loop {
            if matches!(read_event(&mut events).await.data, EventData::OfferSeen { negotiation_id: seen, .. } if seen == negotiation_id) { break; }
        }
    }).await.expect("offer discovered");
    let ResponseOk::Offers(offers) = ok(call(&d.host_b, &HostRequest::NegotiationOffers).await)
    else {
        panic!("expected open offers");
    };
    let offer = offers
        .iter()
        .find(|offer| offer.negotiation_id == negotiation_id)
        .expect("listed target");
    created(
        call(
            &d.host_b,
            &HostRequest::ExecNew {
                exec_id: ExecId([0xb1; 32]),
                program: offer.program_id.to_string(),
                params: Some(offer.params.clone()),
                ensemble: EnsembleSpec::Join {
                    target: Some(NegotiationTarget::new(offer.creator, offer.negotiation_id)),
                },
                blobs: vec![],
            },
        )
        .await,
    );
    tokio::time::timeout(LIVE_EXECUTION_TIMEOUT, async {
        loop {
            if let EventData::OfferClosed {
                program_id,
                negotiation_id: closed,
                creator,
                reason,
            } = read_event(&mut events).await.data
                && closed == negotiation_id
            {
                assert_eq!(program_id, d.program_id);
                assert_eq!(creator, d.peer_a);
                assert_eq!(reason, arena0_api::OfferClosedReason::Complete);
                assert_eq!(serde_json::to_value(reason).unwrap(), "complete");
                break;
            }
        }
    })
    .await
    .expect("joined offer closes");
    let ResponseOk::Offers(offers) = ok(call(&d.host_b, &HostRequest::NegotiationOffers).await)
    else {
        panic!("expected open offers");
    };
    assert!(
        !offers
            .iter()
            .any(|offer| offer.negotiation_id == negotiation_id)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn imported_program_is_watched() {
    let d = daemon(&rps_wasm()).await;
    let wasm = cumulative_sum_wasm();
    let program_id = common::import(&d.host_a, &wasm).await;
    let ResponseOk::ProgramList(programs) = ok(call(&d.host_b, &HostRequest::ProgramList).await)
    else {
        panic!("expected programs");
    };
    // Builds may seed this example at startup. Ensure this test exercises a
    // newly imported catalog membership rather than an existing watcher.
    if programs
        .iter()
        .any(|program| program.program_hash == program_id)
    {
        ok(call(
            &d.host_b,
            &HostRequest::ProgramRemove {
                program: program_id.to_string(),
            },
        )
        .await);
    }
    let (mut events, _write) = subscribe_events(&d.host_b).await;
    assert_eq!(common::import(&d.host_b, &wasm).await, program_id);
    let params = serde_json::json!({"target_size": 2});
    let ResponseOk::ExecCreated {
        negotiation_id: Some(negotiation_id),
        ..
    } = ok(call(
        &d.host_a,
        &HostRequest::ExecNew {
            exec_id: ExecId([0xa2; 32]),
            program: program_id.to_string(),
            params: Some(params.clone()),
            ensemble: EnsembleSpec::Create {
                participant_count: 2,
            },
            blobs: vec![],
        },
    )
    .await)
    else {
        panic!("expected creator");
    };
    tokio::time::timeout(LIVE_EXECUTION_TIMEOUT, async {
        loop {
            if matches!(read_event(&mut events).await.data, EventData::OfferSeen { negotiation_id: seen, .. } if seen == negotiation_id) { break; }
        }
    }).await.expect("import starts offer discovery");
    let ResponseOk::Offers(offers) = ok(call(&d.host_b, &HostRequest::NegotiationOffers).await)
    else {
        panic!("expected open offers");
    };
    let offer = offers
        .iter()
        .find(|offer| offer.negotiation_id == negotiation_id)
        .expect("imported program offer");
    assert_eq!(offer.program_id, program_id);
    assert_eq!(offer.creator, d.peer_a);
    assert_eq!(offer.params, params);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removed_program_is_unwatched() {
    let d = daemon(&rps_wasm()).await;
    let (mut events, _write) = subscribe_events(&d.host_b).await;
    let (_, negotiation_id) = create_on_host_a(&d).await;
    tokio::time::timeout(LIVE_EXECUTION_TIMEOUT, async {
        loop {
            if matches!(read_event(&mut events).await.data, EventData::OfferSeen { negotiation_id: seen, .. } if seen == negotiation_id) { break; }
        }
    }).await.expect("offer discovered");
    assert_eq!(
        ok(call(
            &d.host_b,
            &HostRequest::ProgramRemove {
                program: d.program_id.to_string()
            }
        )
        .await),
        ResponseOk::Ack
    );
    tokio::time::timeout(LIVE_EXECUTION_TIMEOUT, async {
        loop {
            if let EventData::OfferClosed {
                program_id,
                negotiation_id: closed,
                creator,
                reason,
            } = read_event(&mut events).await.data
                && closed == negotiation_id
            {
                assert_eq!(program_id, d.program_id);
                assert_eq!(creator, d.peer_a);
                assert_eq!(reason, arena0_api::OfferClosedReason::Unwatched);
                break;
            }
        }
    })
    .await
    .expect("removed program's offer closes");
    let ResponseOk::Offers(offers) = ok(call(&d.host_b, &HostRequest::NegotiationOffers).await)
    else {
        panic!("expected open offers");
    };
    assert!(!offers.iter().any(|offer| offer.program_id == d.program_id));
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
            ensemble: EnsembleSpec::Create {
                participant_count: 2,
            },
            blobs: vec![],
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
            blobs: vec![],
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
            // Rock-paper-scissors opens a callout in its first dispatch, so
            // the activated session may already be waiting on its participant.
            assert!(
                matches!(exec_state, ExecLifecycle::Active | ExecLifecycle::Waiting),
                "await Active reached: {exec_state:?}"
            );
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
            ensemble: EnsembleSpec::Create {
                participant_count: 2,
            },
            blobs: vec![],
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
                blobs: vec![],
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
    assert!(matches!(
        conflict.code,
        arena0_api::ApiErrorCode::CalloutNotPending
    ));

    let (_session_a, _session_b) = tokio::join!(drive(&d.host_a, exec_a), drive(&d.host_b, exec_b));
    assert_eq!(
        call(target, &request).await.unwrap_err().code,
        arena0_api::ApiErrorCode::CalloutNotPending
    );
    match ok(call(&d.host_a, &HostRequest::ExecStatus { exec_id: exec_a }).await) {
        ResponseOk::Status(status) => assert_eq!(status.lifecycle(), ExecLifecycle::Completed),
        other => panic!("unexpected final status: {other:?}"),
    }
}

/// A program-level input rejection is a typed API error. It leaves the same
/// pending callout available and does not publish an answered event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_input_is_typed_and_emits_no_answered_event() {
    let d = daemon(&chess_wasm()).await;
    let (mut events_a, _events_a_write) = subscribe_events(&d.host_a).await;
    let (mut events_b, _events_b_write) = subscribe_events(&d.host_b).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (exec_a, negotiation_id) = match ok(call(
        &d.host_a,
        &HostRequest::ExecNew {
            exec_id: ExecId([line!() as u8; 32]),
            program: d.program_id.to_string(),
            params: Some(serde_json::json!(null)),
            ensemble: EnsembleSpec::Create {
                participant_count: 2,
            },
            blobs: vec![],
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
                blobs: vec![],
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

    let events = if is_a { &mut events_a } else { &mut events_b };
    loop {
        let frame = read_event(events).await;
        if matches!(
            frame.data,
            EventData::SessionCallout {
                pending_id: event_pending_id,
                ..
            } if event_pending_id == pending_id
        ) {
            break;
        }
    }

    let rejected = call(
        target,
        &HostRequest::ExecSubmit {
            exec_id,
            pending_id,
            answer: Some(serde_json::json!("z9z9")),
        },
    )
    .await
    .expect_err("invalid chess input must be rejected");
    assert_eq!(rejected.code, arena0_api::ApiErrorCode::InputRejected);
    assert!(rejected.message.contains("illegal move"));

    let next_pending_id = match ok(call(target, &HostRequest::ExecNext { exec_id }).await) {
        ResponseOk::Next(arena0_api::NextEvent::Callout { pending_id, .. }) => pending_id,
        other => panic!("unexpected event after rejection: {other:?}"),
    };
    assert_eq!(next_pending_id, pending_id);

    let answered = tokio::time::timeout(
        Duration::from_millis(250),
        read_until_answered(events, pending_id),
    )
    .await;
    assert!(
        answered.is_err(),
        "rejection must not emit callout_answered"
    );

    ok(call(
        target,
        &HostRequest::ExecSubmit {
            exec_id,
            pending_id,
            answer: Some(serde_json::json!("e2e4")),
        },
    )
    .await);
    assert!(
        tokio::time::timeout(
            Duration::from_secs(5),
            read_until_answered(events, pending_id),
        )
        .await
        .expect("accepted answer event timed out")
    );
}

/// The other RPS Host answers first in the final round, then the human answers
/// the final callout. Once the terminal supervisor has cleaned up, retrying
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
            ensemble: EnsembleSpec::Create {
                participant_count: 2,
            },
            blobs: vec![],
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
                blobs: vec![],
            },
        )
        .await,
    );

    let mut answered = std::collections::HashSet::new();
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
        if answered.contains(&pending_id) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            continue;
        }
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

        answered.insert(pending_id);

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
    // Let the terminal message reach the supervisor before retrying the old
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
            let mut ids = hosts
                .iter()
                .map(|host| host.host.id.as_str())
                .collect::<Vec<_>>();
            ids.sort_unstable();
            assert_eq!(ids, ["a", "b"]);
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
                ensemble: EnsembleSpec::Create {
                    participant_count: 2,
                },
                blobs: vec![],
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
            ensemble: EnsembleSpec::Create {
                participant_count: 2,
            },
            blobs: vec![],
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
            at_step: None,
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
                blobs: vec![],
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
            at_step: None,
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
                at_step: None,
            },
        )
        .await)
        {
            ResponseOk::ExecView { step, view } => {
                assert_eq!(
                    step,
                    Some(latest_agreed_step(socket, exec).await),
                    "a latest view names the last agreed trace step"
                );
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
            at_step: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(missing.code, arena0_api::ApiErrorCode::NotFound);
}

/// Create a two-participant execution on Host A; it stays negotiating until a
/// joiner arrives.
async fn create_on_host_a(d: &DaemonHarness) -> (ExecId, arena0_protocol::NegotiationId) {
    match ok(call(
        &d.host_a,
        &HostRequest::ExecNew {
            exec_id: ExecId([0xA1; 32]),
            program: d.program_id.to_string(),
            params: Some(serde_json::json!(null)),
            ensemble: EnsembleSpec::Create {
                participant_count: 2,
            },
            blobs: vec![],
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
    }
}

async fn join_on_host_b(
    d: &DaemonHarness,
    negotiation_id: arena0_protocol::NegotiationId,
) -> ExecId {
    created(
        call(
            &d.host_b,
            &HostRequest::ExecNew {
                exec_id: ExecId([0xB1; 32]),
                program: d.program_id.to_string(),
                params: Some(serde_json::json!(null)),
                ensemble: EnsembleSpec::Join {
                    target: Some(NegotiationTarget::new(d.peer_a, negotiation_id)),
                },
                blobs: vec![],
            },
        )
        .await,
    )
}

/// The next callout either participant has open and has not answered yet.
async fn next_callout(
    d: &DaemonHarness,
    exec_a: ExecId,
    exec_b: ExecId,
    answered: Option<CalloutId>,
) -> (&HostTarget, ExecId, CalloutId) {
    loop {
        let (is_a, next) = next_from_either(&d.host_a, exec_a, &d.host_b, exec_b).await;
        match ok(next) {
            ResponseOk::Next(NextEvent::Callout { pending_id, .. }) => {
                if Some(pending_id) != answered {
                    return if is_a {
                        (&d.host_a, exec_a, pending_id)
                    } else {
                        (&d.host_b, exec_b, pending_id)
                    };
                }
                // The answered callout is still projected until its step lands.
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            other => panic!("expected a callout, got {other:?}"),
        }
    }
}

async fn view_at(target: &HostTarget, exec: ExecId, at_step: Option<u64>) -> Response {
    call(
        target,
        &HostRequest::ExecView {
            exec,
            width: 80,
            color: ColorDepth::Ansi16,
            at_step,
        },
    )
    .await
}

fn view_reply(response: Response) -> (Option<u64>, View) {
    match ok(response) {
        ResponseOk::ExecView { step, view } => (step, view),
        other => panic!("unexpected view response: {other:?}"),
    }
}

/// The step number of the last entry in the execution's agreed trace.
async fn latest_agreed_step(target: &HostTarget, exec_id: ExecId) -> u64 {
    let request = HostRequest::ExecTrace {
        exec_id,
        from: 0,
        to: u64::MAX,
    };
    match ok(call(target, &request).await) {
        ResponseOk::Trace(steps) => {
            steps
                .last()
                .expect("a started session has step 0")
                .entry
                .step
        }
        other => panic!("unexpected trace response: {other:?}"),
    }
}

/// A view at a past step is the replay of the agreed steps up to it, so it
/// must be exactly the view the live Host rendered while that step was the
/// latest one. Chess makes every step a different board, so a Host that
/// ignored `at_step` and rendered its latest state would fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn past_view_equals_the_live_view_at_that_step() {
    let d = daemon(&chess_wasm()).await;
    let (exec_a, negotiation_id) = create_on_host_a(&d).await;
    let exec_b = join_on_host_b(&d, negotiation_id).await;

    // The session start plus fool's mate: five agreed steps. A callout opens
    // only after the latest agreed step landed and nothing advances until it
    // is answered, so each live view is a stable observation of one step.
    let mut recorded: Vec<(u64, View)> = Vec::new();
    let mut answered = None;
    for chess_move in ["f2f3", "e7e5", "g2g4", "d8h4"] {
        let (target, exec_id, pending_id) = next_callout(&d, exec_a, exec_b, answered).await;
        let (step, view) = view_reply(view_at(target, exec_id, None).await);
        recorded.push((step.expect("a callout follows an agreed step"), view));
        ok(call(
            target,
            &HostRequest::ExecSubmit {
                exec_id,
                pending_id,
                answer: Some(serde_json::json!(chess_move)),
            },
        )
        .await);
        answered = Some(pending_id);
    }
    for (target, exec_id) in [(&d.host_a, exec_a), (&d.host_b, exec_b)] {
        ok(call(
            target,
            &HostRequest::ExecAwait {
                exec_id,
                until: AwaitState::Terminal,
            },
        )
        .await);
    }
    let (step, terminal) = view_reply(view_at(&d.host_a, exec_a, None).await);
    recorded.push((
        step.expect("a completed session has agreed steps"),
        terminal,
    ));

    let steps: Vec<u64> = recorded.iter().map(|(step, _)| *step).collect();
    assert_eq!(steps, [0, 1, 2, 3, 4], "one recorded view per agreed step");
    for (index, (_, view)) in recorded.iter().enumerate() {
        assert!(
            recorded[index + 1..].iter().all(|(_, other)| other != view),
            "every step renders a different board"
        );
    }

    for (target, exec_id) in [(&d.host_a, exec_a), (&d.host_b, exec_b)] {
        for (step, live) in &recorded {
            let (replayed_step, replayed) = view_reply(view_at(target, exec_id, Some(*step)).await);
            assert_eq!(replayed_step, Some(*step));
            assert_eq!(
                &replayed, live,
                "Host {} replayed step {step} differently from the live view",
                target.name
            );
        }
    }
}

/// Only agreed steps can be replayed, and only once a session has started.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn past_view_rejects_future_steps() {
    let d = daemon(&rps_wasm()).await;
    let (exec_a, negotiation_id) = create_on_host_a(&d).await;

    // Still negotiating: a past view fails exactly as a latest view does.
    let latest = view_at(&d.host_a, exec_a, None).await.unwrap_err();
    assert_eq!(latest.code, arena0_api::ApiErrorCode::Execution);
    assert_eq!(
        view_at(&d.host_a, exec_a, Some(0)).await.unwrap_err(),
        latest
    );

    let exec_b = join_on_host_b(&d, negotiation_id).await;

    // A live session: rock-paper-scissors opens callouts at the session start
    // and nothing advances until one is answered.
    let (target, exec_id, _) = next_callout(&d, exec_a, exec_b, None).await;
    assert_future_step_rejected(target, exec_id).await;

    let (_sa, _sb) = tokio::join!(drive(&d.host_a, exec_a), drive(&d.host_b, exec_b));
    for (target, exec_id) in [(&d.host_a, exec_a), (&d.host_b, exec_b)] {
        assert_future_step_rejected(target, exec_id).await;
    }
}

async fn assert_future_step_rejected(target: &HostTarget, exec_id: ExecId) {
    let latest = latest_agreed_step(target, exec_id).await;
    let (step, _) = view_reply(view_at(target, exec_id, Some(latest)).await);
    assert_eq!(step, Some(latest), "the latest agreed step can be replayed");
    for beyond in [latest + 1, u64::MAX] {
        let error = view_at(target, exec_id, Some(beyond)).await.unwrap_err();
        assert_eq!(error.code, arena0_api::ApiErrorCode::BadRequest);
        assert_eq!(
            error.message,
            format!("step {beyond} is beyond the latest agreed step {latest}")
        );
    }
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
            ensemble: EnsembleSpec::Create {
                participant_count: 2,
            },
            blobs: vec![],
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
                blobs: vec![],
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

async fn subscribe_events(target: &HostTarget) -> (BufReader<OwnedReadHalf>, OwnedWriteHalf) {
    let stream = UnixStream::connect(&target.socket).await.expect("connect");
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let request = target.request(&HostRequest::EventsSubscribe {
        filter: EventFilter {
            include: vec![],
            exclude: vec![],
        },
    });
    arena0_api::frame::write_frame(&mut write, &request)
        .await
        .expect("write subscribe");
    let _ack: Response = arena0_api::frame::read_frame(&mut read)
        .await
        .expect("read ack")
        .expect("ack frame");
    (read, write)
}

async fn read_event(reader: &mut BufReader<OwnedReadHalf>) -> EventFrame {
    arena0_api::frame::read_frame(reader)
        .await
        .expect("read event")
        .expect("event frame")
}

async fn read_until_answered(
    reader: &mut BufReader<OwnedReadHalf>,
    pending_id: arena0_protocol::CalloutId,
) -> bool {
    loop {
        if matches!(
            read_event(reader).await.data,
            EventData::SessionCalloutAnswered { pending_id: event_pending_id }
                if event_pending_id == pending_id
        ) {
            return true;
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
            ensemble: EnsembleSpec::Create {
                participant_count: 2,
            },
            blobs: vec![],
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
                blobs: vec![],
            },
        )
        .await,
    );
    let (sid, _sb) = tokio::join!(drive(&d.host_a, exec_a), drive(&d.host_b, exec_b));

    // Fetch A's receipt and keep its content address for import checks.
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
                ensemble: EnsembleSpec::Create {
                    participant_count: 2,
                },
                blobs: vec![],
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

/// Blobs are linked by path, granted by hash at `exec.new`, and exported to a
/// new file; unknown hashes and unusable paths are rejected before anything
/// is published.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blobs_link_grant_and_export_by_hash() {
    let wasm = rps_wasm();
    let d = daemon(&wasm).await;
    let files = tempfile::tempdir().expect("blob files");
    let bytes: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let first = files.path().join("first.bin");
    std::fs::write(&first, &bytes).unwrap();

    let ResponseOk::BlobImported { hash, length } = ok(call(
        &d.host_a,
        &HostRequest::BlobImport {
            source: arena0_api::FileSource::Path(first.clone()),
        },
    )
    .await) else {
        panic!("expected BlobImported");
    };
    assert_eq!(length, bytes.len() as u64);
    assert_eq!(
        hash.0,
        arena0_crypto::hash(arena0_crypto::HashAlgorithm::Blake3, &bytes)
    );

    // Relinking the same content from another path keeps the hash and makes
    // the new path the one the Host reads.
    let second = files.path().join("second.bin");
    std::fs::write(&second, &bytes).unwrap();
    assert_eq!(
        ok(call(
            &d.host_a,
            &HostRequest::BlobImport {
                source: arena0_api::FileSource::Path(second)
            }
        )
        .await),
        ResponseOk::BlobImported { hash, length }
    );
    std::fs::remove_file(&first).unwrap();

    let missing = call(
        &d.host_a,
        &HostRequest::BlobImport {
            source: arena0_api::FileSource::Path(files.path().join("absent.bin")),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(missing.code, arena0_api::ApiErrorCode::BadRequest);

    let unknown = arena0_protocol::BlobHash([7; 32]);
    let refused = call(
        &d.host_a,
        &HostRequest::ExecNew {
            exec_id: ExecId([line!() as u8; 32]),
            program: d.program_id.to_string(),
            params: Some(serde_json::json!(null)),
            ensemble: EnsembleSpec::Create {
                participant_count: 2,
            },
            blobs: vec![hash, unknown],
        },
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code, arena0_api::ApiErrorCode::NotFound);

    let granted = ExecId([line!() as u8; 32]);
    assert!(matches!(
        ok(call(
            &d.host_a,
            &HostRequest::ExecNew {
                exec_id: granted,
                program: d.program_id.to_string(),
                params: Some(serde_json::json!(null)),
                ensemble: EnsembleSpec::Create {
                    participant_count: 2,
                },
                blobs: vec![hash],
            },
        )
        .await),
        ResponseOk::ExecCreated { .. }
    ));
    assert_eq!(
        ok(call(
            &d.host_a,
            &HostRequest::ExecCancelCreation { exec_id: granted }
        )
        .await),
        ResponseOk::Ack
    );

    let exported = files.path().join("exported.bin");
    assert_eq!(
        ok(call(
            &d.host_a,
            &HostRequest::BlobExport {
                hash,
                path: exported.clone(),
            },
        )
        .await),
        ResponseOk::BlobExported { length }
    );
    assert_eq!(std::fs::read(&exported).unwrap(), bytes);
    let again = call(
        &d.host_a,
        &HostRequest::BlobExport {
            hash,
            path: exported,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(again.code, arena0_api::ApiErrorCode::BadRequest);
    let absent = call(
        &d.host_a,
        &HostRequest::BlobExport {
            hash: unknown,
            path: files.path().join("never.bin"),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(absent.code, arena0_api::ApiErrorCode::NotFound);
    assert!(!files.path().join("never.bin").exists());
}

/// Host A creates and Host B joins. The joiner passes `joiner_params`, so
/// `None` exercises adopting the creator's terms.
async fn launch_pair(
    d: &common::DaemonHarness,
    creator_params: Option<serde_json::Value>,
    joiner_params: Option<serde_json::Value>,
) -> (ExecId, ExecId) {
    let (exec_a, negotiation_id) = match ok(call(
        &d.host_a,
        &HostRequest::ExecNew {
            exec_id: ExecId([0xa1; 32]),
            program: d.program_id.to_string(),
            params: creator_params,
            ensemble: EnsembleSpec::Create {
                participant_count: 2,
            },
            blobs: vec![],
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
                exec_id: ExecId([0xb1; 32]),
                program: d.program_id.to_string(),
                params: joiner_params,
                ensemble: EnsembleSpec::Join {
                    target: Some(NegotiationTarget::new(d.peer_a, negotiation_id)),
                },
                blobs: vec![],
            },
        )
        .await,
    );
    (exec_a, exec_b)
}

/// Launch a pair and drive both Hosts to completion.
async fn complete_session(
    d: &common::DaemonHarness,
    creator_params: Option<serde_json::Value>,
    joiner_params: Option<serde_json::Value>,
) -> (ExecId, ExecId) {
    let (exec_a, exec_b) = launch_pair(d, creator_params, joiner_params).await;
    let (session_a, session_b) = tokio::join!(drive(&d.host_a, exec_a), drive(&d.host_b, exec_b));
    assert_eq!(session_a, session_b, "both Hosts completed one session");
    (exec_a, exec_b)
}

/// Every participant's Host projects the offer params, including the joiner
/// that submitted none and adopted the creator's terms.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inspect_projects_offer_params() {
    let d = daemon(&cumulative_sum_wasm()).await;
    let params = serde_json::json!({ "target_size": 2 });
    let (exec_a, exec_b) = complete_session(&d, Some(params.clone()), None).await;

    for (host, exec_id) in [(&d.host_a, exec_a), (&d.host_b, exec_b)] {
        let ResponseOk::Inspection(inspection) = ok(call(
            host,
            &HostRequest::ExecInspect {
                exec_id,
                events_from: None,
                events_limit: 16,
            },
        )
        .await) else {
            panic!("expected Inspection from {}", host.name);
        };
        let activation = inspection
            .activation
            .unwrap_or_else(|| panic!("{} holds the activation", host.name));
        assert_eq!(activation.params, params, "offer params on {}", host.name);
    }
}

/// Agreed steps carry the local time this Host stored them, and the status
/// carries the request's creation time and the latest durable transition.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trace_and_status_carry_local_times() {
    let d = daemon(&rps_wasm()).await;
    let no_params = Some(serde_json::json!(null));
    let (exec_a, exec_b) = complete_session(&d, no_params.clone(), no_params).await;
    let after_run = arena0_node::unix_time_ms();

    for (host, exec_id) in [(&d.host_a, exec_a), (&d.host_b, exec_b)] {
        let ResponseOk::Status(status) = ok(call(host, &HostRequest::ExecStatus { exec_id }).await)
        else {
            panic!("expected Status from {}", host.name);
        };
        let ResponseOk::Trace(steps) = ok(call(
            host,
            &HostRequest::ExecTrace {
                exec_id,
                from: 0,
                to: u64::MAX,
            },
        )
        .await) else {
            panic!("expected Trace from {}", host.name);
        };
        assert!(!steps.is_empty(), "a completed session agreed on steps");
        assert!(
            steps
                .iter()
                .enumerate()
                .all(|(index, step)| step.entry.step == index as u64),
            "{} returns the trace in step order",
            host.name
        );
        assert!(
            steps
                .windows(2)
                .all(|pair| pair[0].certified_at_ms <= pair[1].certified_at_ms),
            "certification times never decrease by step on {}",
            host.name
        );
        let first = steps.first().expect("non-empty").certified_at_ms;
        let last = steps.last().expect("non-empty").certified_at_ms;
        assert!(
            status.created_at_ms <= first,
            "{}: request created at {} after its first step at {first}",
            host.name,
            status.created_at_ms
        );
        assert!(
            last <= after_run,
            "{}: last step at {last} is after the run ended at {after_run}",
            host.name
        );
        assert!(
            status.created_at_ms <= status.updated_at_ms,
            "{}: created {} after updated {}",
            host.name,
            status.created_at_ms,
            status.updated_at_ms
        );
        assert!(
            last <= status.updated_at_ms,
            "{}: the latest transition ({}) predates the last step ({last})",
            host.name,
            status.updated_at_ms
        );
    }
}

/// `blob.list` reports every linked blob ordered by hash, and an empty Host
/// reports none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob_list_reports_imported_blobs() {
    let d = daemon(&rps_wasm()).await;
    assert_eq!(
        ok(call(&d.host_a, &HostRequest::BlobList).await),
        ResponseOk::BlobList(vec![]),
        "a Host with no blobs lists none"
    );

    let files = tempfile::tempdir().expect("blob files");
    let mut expected = Vec::new();
    for (name, bytes) in [
        ("short.bin", vec![1u8; 10]),
        ("long.bin", (0..5_000u32).map(|i| (i % 253) as u8).collect()),
    ] {
        let path = files.path().join(name);
        std::fs::write(&path, &bytes).unwrap();
        expected.push((
            path,
            arena0_api::BlobEntry {
                hash: arena0_protocol::BlobHash(arena0_crypto::hash(
                    arena0_crypto::HashAlgorithm::Blake3,
                    &bytes,
                )),
                length: bytes.len() as u64,
                linked: true,
            },
        ));
    }
    // Import the greater hash first, so listing in insertion order would
    // differ from the required hash order.
    expected.sort_by_key(|(_, entry)| std::cmp::Reverse(entry.hash.0));
    for (path, entry) in &expected {
        assert_eq!(
            ok(call(
                &d.host_a,
                &HostRequest::BlobImport {
                    source: arena0_api::FileSource::Path(path.clone())
                }
            )
            .await),
            ResponseOk::BlobImported {
                hash: entry.hash,
                length: entry.length
            }
        );
    }
    expected.sort_by_key(|(_, entry)| entry.hash.0);

    assert_eq!(
        ok(call(&d.host_a, &HostRequest::BlobList).await),
        ResponseOk::BlobList(expected.into_iter().map(|(_, entry)| entry).collect())
    );
    assert_eq!(
        ok(call(&d.host_b, &HostRequest::BlobList).await),
        ResponseOk::BlobList(vec![]),
        "blobs are per Host"
    );
}

async fn await_active(target: &HostTarget, exec_id: ExecId) {
    ok(call(
        target,
        &HostRequest::ExecAwait {
            exec_id,
            until: AwaitState::Active,
        },
    )
    .await);
}

async fn session_status(target: &HostTarget, exec_id: ExecId) -> arena0_api::SessionStatus {
    let ResponseOk::Status(status) = ok(call(target, &HostRequest::ExecStatus { exec_id }).await)
    else {
        panic!("expected Status from {}", target.name);
    };
    status
        .session()
        .cloned()
        .unwrap_or_else(|| panic!("{} reports no session: {:?}", target.name, status.state))
}

/// Wait until the agreed step at index `step` is durable on this Host. The
/// step's trace entry and the execution's agreed-step cursor commit together,
/// so a status read after this returns reflects the step.
async fn read_until_step(reader: &mut BufReader<OwnedReadHalf>, step: u64) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if matches!(
                read_event(reader).await.data,
                EventData::SessionStep { step: agreed, .. } if agreed == step
            ) {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("agreed step {step} never arrived"));
}

/// `exec.status` names whose message the program accepts next and the
/// program's phase, identically on every Host, and follows the session as it
/// advances. A program whose view names no turn reports `null`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_reports_the_turn_and_phase() {
    let d = daemon(&chess_wasm()).await;
    let (mut events_a, _events_a_write) = subscribe_events(&d.host_a).await;
    let (mut events_b, _events_b_write) = subscribe_events(&d.host_b).await;
    let no_params = Some(serde_json::json!(null));
    let (exec_a, exec_b) = launch_pair(&d, no_params.clone(), no_params.clone()).await;
    await_active(&d.host_a, exec_a).await;
    await_active(&d.host_b, exec_b).await;

    // The first mover is whoever the program asks for a move: the Host that
    // holds the pending callout, an observation independent of `turn`.
    let (mover_is_a, next) = next_from_either(&d.host_a, exec_a, &d.host_b, exec_b).await;
    let (mover_target, mover_exec, mover, other_target, other_exec, other) = if mover_is_a {
        (&d.host_a, exec_a, d.peer_a, &d.host_b, exec_b, d.peer_b)
    } else {
        (&d.host_b, exec_b, d.peer_b, &d.host_a, exec_a, d.peer_a)
    };
    let pending_id = match ok(next) {
        ResponseOk::Next(arena0_api::NextEvent::Callout { pending_id, .. }) => pending_id,
        other => panic!("expected the first mover's callout: {other:?}"),
    };

    let declared: Vec<String> = match ok(call(
        &d.host_a,
        &HostRequest::ProgramGet {
            program: d.program_id.to_string(),
        },
    )
    .await)
    {
        ResponseOk::Program(detail) => detail
            .schema
            .phases
            .into_iter()
            .map(|phase| phase.name)
            .collect(),
        other => panic!("expected the program detail: {other:?}"),
    };

    let before = session_status(mover_target, mover_exec).await;
    for (target, exec_id) in [(mover_target, mover_exec), (other_target, other_exec)] {
        let session = session_status(target, exec_id).await;
        assert_eq!(
            session.step, before.step,
            "{} at the same step",
            target.name
        );
        assert_eq!(
            session.turn,
            Some(mover),
            "{} names the peer that moves first",
            target.name
        );
        let phase = session.phase.unwrap_or_else(|| {
            panic!(
                "{} reports no phase for a program that declares them",
                target.name
            )
        });
        assert!(
            declared.contains(&phase),
            "{} reports {phase:?}, not one of {declared:?}",
            target.name
        );
        // Chess leaves its default `setup` phase when the session starts.
        assert_eq!(phase, "playing", "{}", target.name);
    }

    ok(call(
        mover_target,
        &HostRequest::ExecSubmit {
            exec_id: mover_exec,
            pending_id,
            answer: Some(serde_json::json!("e2e4")),
        },
    )
    .await);
    let (mover_events, other_events) = if mover_is_a {
        (&mut events_a, &mut events_b)
    } else {
        (&mut events_b, &mut events_a)
    };
    read_until_step(mover_events, before.step).await;
    read_until_step(other_events, before.step).await;

    for (target, exec_id) in [(mover_target, mover_exec), (other_target, other_exec)] {
        let session = session_status(target, exec_id).await;
        assert_eq!(
            session.step,
            before.step + 1,
            "{} after one move",
            target.name
        );
        assert_eq!(
            session.turn,
            Some(other),
            "{} hands the move to the other peer",
            target.name
        );
        assert_eq!(session.phase.as_deref(), Some("playing"), "{}", target.name);
    }

    // timer-dispatch declares a phase but its view names no turn.
    let d = daemon(&timer_dispatch_wasm()).await;
    let (exec_a, exec_b) = launch_pair(&d, no_params.clone(), no_params).await;
    await_active(&d.host_a, exec_a).await;
    await_active(&d.host_b, exec_b).await;
    for (target, exec_id) in [(&d.host_a, exec_a), (&d.host_b, exec_b)] {
        let session = session_status(target, exec_id).await;
        assert_eq!(session.turn, None, "{}", target.name);
        assert_eq!(session.phase.as_deref(), Some("waiting"), "{}", target.name);
        let json = serde_json::to_value(&session).expect("session status JSON");
        assert!(
            json["turn"].is_null(),
            "{}: turn is an explicit null on the wire: {json}",
            target.name
        );
    }
}

/// `program.get` lists the phases a program declares, in declaration order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn program_schema_lists_declared_phases() {
    let d = daemon(&prisoner_dilemma_wasm()).await;
    let ResponseOk::Program(detail) = ok(call(
        &d.host_a,
        &HostRequest::ProgramGet {
            program: d.program_id.to_string(),
        },
    )
    .await) else {
        panic!("expected the program detail");
    };
    let phase = |name: &str, description: &str, is_default: bool| arena0_program::PhaseSchema {
        name: name.to_owned(),
        description: description.to_owned(),
        is_default,
        is_terminal: false,
    };
    assert_eq!(
        detail.schema.phases,
        vec![
            phase("setup", "Waiting for opponent", true),
            phase("playing", "Round in progress", false),
        ]
    );
}

/// The cell of `board` on `square` (for example "e5"), located through the
/// board's own labels so the check does not assume an orientation.
fn board_cell<'a>(
    cells: &'a [Cell],
    row_labels: &[String],
    col_labels: &[String],
    square: &str,
) -> &'a Cell {
    let (file, rank) = square.split_at(1);
    let row = row_labels.iter().position(|label| label == rank).unwrap();
    let col = col_labels.iter().position(|label| label == file).unwrap();
    &cells[row * col_labels.len() + col]
}

/// Chess renders a board and the side to move as typed blocks next to its
/// text slots, and the blocks agree with the text.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn views_carry_structured_blocks() {
    let d = daemon(&chess_wasm()).await;
    let (exec_a, negotiation_id) = create_on_host_a(&d).await;
    let exec_b = join_on_host_b(&d, negotiation_id).await;

    let mut answered = None;
    for chess_move in ["e2e4", "e7e5"] {
        let (target, exec_id, pending_id) = next_callout(&d, exec_a, exec_b, answered).await;
        ok(call(
            target,
            &HostRequest::ExecSubmit {
                exec_id,
                pending_id,
                answer: Some(serde_json::json!(chess_move)),
            },
        )
        .await);
        answered = Some(pending_id);
    }
    // White is to move again. A callout opens only after the latest agreed
    // step landed, so this view observes the position after both moves.
    let (target, exec_id, _) = next_callout(&d, exec_a, exec_b, answered).await;
    let (_, view) = view_reply(view_at(target, exec_id, None).await);

    let board = view
        .blocks
        .iter()
        .find(|block| matches!(block, Block::Board { .. }))
        .expect("a board block");
    let Block::Board {
        rows,
        cols,
        cells,
        row_labels,
        col_labels,
        ..
    } = board
    else {
        unreachable!("matched a board above");
    };
    assert_eq!((*rows, *cols, cells.len()), (8, 8, 64));
    let cell = |square| board_cell(cells, row_labels, col_labels, square);

    let pieces: Vec<&Cell> = cells.iter().filter(|cell| !cell.text.is_empty()).collect();
    assert_eq!(pieces.len(), 32, "no piece was captured");
    for owner in [0, 1] {
        assert_eq!(
            pieces
                .iter()
                .filter(|cell| cell.participant == Some(owner))
                .count(),
            16,
            "participant {owner} owns sixteen pieces"
        );
    }
    assert!(
        cells
            .iter()
            .filter(|cell| cell.text.is_empty())
            .all(|cell| cell.participant.is_none()),
        "an empty square has no owner"
    );

    // Black's e7-e5 was the last move: its origin and destination are
    // highlighted, and white's earlier e4 pawn is not.
    let destination = cell("e5");
    assert_eq!(destination.text, "\u{265f}");
    assert_eq!(destination.participant, Some(1));
    assert_eq!(destination.tone, Tone::Highlight);
    assert_eq!(cell("e7").tone, Tone::Highlight);
    let earlier = cell("e4");
    assert_eq!(earlier.participant, Some(0));
    assert_eq!(earlier.tone, Tone::Normal);
    assert_eq!(
        cells
            .iter()
            .filter(|cell| cell.tone == Tone::Highlight)
            .count(),
        2,
        "only the last move's two squares are highlighted"
    );

    let facts = view
        .blocks
        .iter()
        .find_map(|block| match block {
            Block::Facts { items, .. } => Some(items),
            _ => None,
        })
        .expect("a facts block");
    let fact = |label: &str| {
        &facts
            .iter()
            .find(|fact| fact.label == label)
            .unwrap_or_else(|| panic!("no fact named {label}"))
            .value
    };
    let to_move = fact("To move");
    let move_number = fact("Move");
    let status_bar = &view.slots[&Slot::StatusBar];
    assert_eq!(
        *status_bar,
        format!("{}'s turn, move {}", to_move.text, move_number.text),
        "the facts say what the status bar says"
    );
    assert_eq!(to_move.text, "white");
    assert_eq!(to_move.participant, Some(0));
}

/// Start a session of the daemon's program on two Hosts and wait until its
/// first agreed step is durable, so both a live view and a past view exist.
async fn start_view_session(d: &DaemonHarness) -> ExecId {
    let (mut events, _keep_open) = subscribe_events(&d.host_a).await;
    let (exec_a, negotiation_id) = create_on_host_a(d).await;
    join_on_host_b(d, negotiation_id).await;
    tokio::time::timeout(LIVE_EXECUTION_TIMEOUT, async {
        loop {
            let frame = read_event(&mut events).await;
            if frame.exec_id == Some(exec_a)
                && matches!(frame.data, EventData::SessionStep { step: 0, .. })
            {
                return;
            }
        }
    })
    .await
    .expect("the session's first step is agreed");
    exec_a
}

fn text_only(count: usize) -> Vec<Cell> {
    vec![Cell::text(""); count]
}

/// The Host decodes a program's view as untrusted output. A turn, phase and
/// blocks within the documented limits reach the client unchanged; each way to
/// break a limit fails the view with an error naming it, on the live path and
/// on the replayed past-step path, and never truncates or drops a field.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn view_validation_rejects_views_beyond_their_limits() {
    let blocks = |blocks: Vec<Block>| {
        blocks
            .into_iter()
            .fold(View::new().header("fixture"), View::block)
    };
    let table = |rows: usize, columns: usize| Block::Table {
        title: None,
        columns: vec!["c".into(); columns],
        rows: vec![text_only(columns); rows],
    };
    let board = |rows: u8, cols: u8, cells: usize| Block::Board {
        title: None,
        rows,
        cols,
        cells: text_only(cells),
        row_labels: vec![],
        col_labels: vec![],
    };
    let roster = |entries: usize, participant: u8| Block::Roster {
        title: None,
        entries: vec![
            RosterEntry {
                participant,
                status: Cell::text("ready"),
                detail: None,
            };
            entries
        ],
    };
    let progress = || Block::Progress {
        label: "p".into(),
        value: 1,
        max: 2,
    };
    let facts = |text: &str| Block::Facts {
        title: None,
        items: vec![Fact {
            label: "f".into(),
            value: Cell::text(text),
        }],
    };
    let cases: Vec<(&str, View, &str)> = vec![
        (
            "turn",
            View {
                turn: Some(2),
                ..View::new()
            },
            "turn 2 is outside the ensemble of 2",
        ),
        (
            "phase length",
            View {
                phase: Some("x".repeat(65)),
                ..View::new()
            },
            "phase is 65 bytes",
        ),
        (
            "board cells",
            blocks(vec![board(2, 2, 3)]),
            "needs 4 cells but has 3",
        ),
        (
            "board size",
            blocks(vec![board(33, 1, 33)]),
            "board is 33x1",
        ),
        (
            "board labels",
            blocks(vec![Block::Board {
                title: None,
                rows: 2,
                cols: 1,
                cells: text_only(2),
                row_labels: vec!["1".into()],
                col_labels: vec![],
            }]),
            "1 row labels",
        ),
        ("table rows", blocks(vec![table(65, 1)]), "65 rows"),
        ("table columns", blocks(vec![table(1, 17)]), "17 columns"),
        ("roster entries", blocks(vec![roster(65, 0)]), "65 entries"),
        (
            "block count",
            blocks((0..17).map(|_| progress()).collect()),
            "17 blocks",
        ),
        (
            "text length",
            blocks(vec![facts(&"x".repeat(257))]),
            "257 bytes",
        ),
        (
            "participant",
            blocks(vec![roster(1, 2)]),
            "participant 2 is outside the ensemble of 2",
        ),
        (
            "cell participant",
            blocks(vec![Block::Facts {
                title: None,
                items: vec![Fact {
                    label: "f".into(),
                    value: Cell::text("x").participant(2),
                }],
            }]),
            "participant 2 is outside the ensemble of 2",
        ),
    ];

    // Each limit at its edge is accepted as written. The fixture holds one
    // view of under 32 KiB, so the two 1024-cell edges get a session each.
    let at_limits: Vec<(&str, View)> = vec![
        ("table at limits", blocks(vec![table(64, 16)])),
        ("board at limits", blocks(vec![board(32, 32, 1024)])),
        (
            "counts and text at limits",
            blocks(
                std::iter::repeat_n(facts(&"x".repeat(256)), 15)
                    .chain([roster(64, 1)])
                    .collect(),
            ),
        ),
        (
            "turn and phase at limits",
            View {
                turn: Some(1),
                phase: Some("x".repeat(64)),
                ..View::new()
            },
        ),
    ];
    let rejected = cases
        .into_iter()
        .map(|(name, view, fragment)| (name, view, Some(fragment)));
    let accepted = at_limits.into_iter().map(|(name, view)| (name, view, None));
    for (name, view, rejection) in accepted.chain(rejected) {
        let json = serde_json::to_string(&view).expect("view JSON");
        let d = daemon(&view_program_wasm(&json)).await;
        let exec = start_view_session(&d).await;
        for at_step in [None, Some(0)] {
            let reply = view_at(&d.host_a, exec, at_step).await;
            match rejection {
                None => assert_eq!(
                    view_reply(reply).1,
                    view,
                    "{name} at {at_step:?}: a view within the limits passes through"
                ),
                Some(fragment) => {
                    let error = reply.unwrap_err();
                    assert_eq!(error.code, arena0_api::ApiErrorCode::Execution, "{name}");
                    assert!(
                        error.message.contains(fragment),
                        "{name} at {at_step:?}: `{}` does not name `{fragment}`",
                        error.message
                    );
                }
            }
        }
    }
}
