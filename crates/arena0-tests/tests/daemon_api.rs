//! Runtime API smoke coverage over the shared daemon Unix socket: host
//! discovery, negotiation withdrawal, program-handle resolution, and endpoint
//! routing.

mod common;

use arena0_api::{EnsembleSpec, ExecLifecycle, HostRequest, IdRef, Request, ResponseOk};
use arena0_protocol::ExecId;
use common::{call, call_daemon, created, daemon, ok, rps_wasm};

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

/// The one Unix endpoint routes Host operations by the explicit outer Host
/// name. It also exposes the stable program and identity projections and
/// classifies malformed or unknown references at the Unix boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_endpoint_routes_hosts_programs_and_rejects_invalid_calls() {
    let d = daemon(&rps_wasm()).await;

    let listed_hosts = match ok(call_daemon(&d.socket, &Request::HostsList).await) {
        ResponseOk::Hosts(hosts) => hosts,
        other => panic!("unexpected HostsList response: {other:?}"),
    };
    assert_eq!(listed_hosts.len(), 2);
    let mut listed_ids = listed_hosts
        .iter()
        .map(|host| host.host.id.as_str())
        .collect::<Vec<_>>();
    listed_ids.sort_unstable();
    assert_eq!(listed_ids, ["a", "b"]);
    assert_ne!(
        listed_hosts[0].host.peer_id, listed_hosts[1].host.peer_id,
        "HostsList retains distinct identities"
    );

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
    let listed_peers = listed_hosts
        .iter()
        .map(|host| host.host.peer_id)
        .collect::<Vec<_>>();
    assert!(listed_peers.contains(&info_a.peer_id));
    assert!(listed_peers.contains(&info_b.peer_id));

    // Program handles resolve by the stable name and full content id; an
    // unknown handle remains a typed NotFound error.
    for program in ["rock-paper-scissors".to_owned(), d.program_id.to_string()] {
        match ok(call(&d.host_a, &HostRequest::ProgramGet { program }).await) {
            ResponseOk::Program(detail) => {
                assert_eq!(detail.summary.program_hash, d.program_id)
            }
            other => panic!("unexpected program response: {other:?}"),
        }
    }
    let missing_program = call(
        &d.host_a,
        &HostRequest::ProgramGet {
            program: "does-not-exist".into(),
        },
    )
    .await
    .expect_err("unknown program handle must be rejected");
    assert_eq!(missing_program.code, arena0_api::ApiErrorCode::NotFound);

    // Identity mutations are routed through the same endpoint. Removing the
    // active identity is rejected before the keystore changes.
    let identities_before = match ok(call(&d.host_a, &HostRequest::IdList).await) {
        ResponseOk::IdList(identities) => identities,
        other => panic!("unexpected identity list response: {other:?}"),
    };
    let remove_active = call(
        &d.host_a,
        &HostRequest::IdRemove {
            id: IdRef::Peer(info_a.peer_id),
        },
    )
    .await
    .expect_err("active identity removal must be rejected");
    assert_eq!(remove_active.code, arena0_api::ApiErrorCode::BadRequest);
    assert!(remove_active.message.contains("active Host identity"));
    let identities_after = match ok(call(&d.host_a, &HostRequest::IdList).await) {
        ResponseOk::IdList(identities) => identities,
        other => panic!("unexpected identity list response: {other:?}"),
    };
    assert_eq!(identities_after, identities_before);

    let missing_identity = call(
        &d.host_a,
        &HostRequest::IdShow {
            id: IdRef::Label("missing".into()),
        },
    )
    .await
    .expect_err("missing identity should be typed");
    assert_eq!(missing_identity.code, arena0_api::ApiErrorCode::NotFound);

    let invalid_label = call(
        &d.host_a,
        &HostRequest::IdNew {
            label: Some("bad\u{1b}label".into()),
        },
    )
    .await
    .expect_err("invalid identity label should be typed");
    assert_eq!(invalid_label.code, arena0_api::ApiErrorCode::BadRequest);

    // A malformed import must fail before catalog registration. Compare the
    // complete summary projection, not just its row count.
    let programs_before = match ok(call(&d.host_a, &HostRequest::ProgramList).await) {
        ResponseOk::ProgramList(programs) => programs,
        other => panic!("unexpected program list response: {other:?}"),
    };
    let malformed_program = call(
        &d.host_a,
        &HostRequest::ProgramImport {
            wasm: b"Cargo.toml".to_vec(),
        },
    )
    .await
    .expect_err("invalid Wasm should be typed");
    assert_eq!(malformed_program.code, arena0_api::ApiErrorCode::BadRequest);
    let programs_after = match ok(call(&d.host_a, &HostRequest::ProgramList).await) {
        ResponseOk::ProgramList(programs) => programs,
        other => panic!("unexpected program list response: {other:?}"),
    };
    assert_eq!(programs_after, programs_before);

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

    let unknown_host = call_daemon(
        &d.socket,
        &Request::Host {
            host: "missing".into(),
            request: HostRequest::Info,
        },
    )
    .await
    .expect_err("unknown Host must return a typed NotFound error");
    assert_eq!(unknown_host.code, arena0_api::ApiErrorCode::NotFound);

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
