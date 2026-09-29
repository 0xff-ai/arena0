mod common;

use std::collections::BTreeSet;

use arena0_api::{
    ApiErrorCode, EnsembleSpec, EventFrame, FileSource, HostRequest, Request, ResponseOk, Uploaded,
};
use arena0_daemon::{Daemon, HttpConfig};
use arena0_home::Home;
use arena0_protocol::{BlobHash, ExecId};
use common::http::{EventStream, http, rpc};
use common::{call, call_daemon, daemon, import, ok, rps_wasm};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_answers_like_the_socket() {
    let d = daemon(&rps_wasm()).await;
    assert_eq!(
        rpc(d.http, &Request::HostsList).await,
        call_daemon(&d.socket, &Request::HostsList).await
    );
    assert_eq!(
        http(d.http, "POST", "/rpc", Some("text/plain"), b"{}")
            .await
            .status,
        415
    );
    assert_eq!(
        http(d.http, "POST", "/rpc", Some("application/json"), b"{")
            .await
            .status,
        400
    );
    let reply = rpc(d.http, &Request::ActivitySubscribe).await.unwrap_err();
    assert_eq!(reply.code, ApiErrorCode::BadRequest);
    assert_eq!(
        reply.message,
        "subscriptions need their own socket connection or GET /events"
    );
    d._daemon.stop().await;
}

#[tokio::test]
async fn rpc_rejects_paths() {
    let wasm = rps_wasm();
    let d = daemon(&wasm).await;
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), &wasm).unwrap();
    for request in [
        HostRequest::ProgramImport {
            source: FileSource::Path(file.path().into()),
        },
        HostRequest::BlobImport {
            source: FileSource::Path(file.path().into()),
        },
        HostRequest::BlobExport {
            hash: BlobHash([0; 32]),
            path: file.path().into(),
        },
    ] {
        let error = rpc(d.http, &d.host_a.request(&request)).await.unwrap_err();
        assert_eq!(error.code, ApiErrorCode::BadRequest);
        assert_eq!(error.message, "paths are accepted only on the local socket");
    }
    assert_eq!(import(&d.host_a, &wasm).await, d.program_id);
    let error = rpc(d.http, &Request::DaemonStop).await.unwrap_err();
    assert_eq!(error.code, ApiErrorCode::BadRequest);
    assert_eq!(
        error.message,
        "daemon.stop is accepted only on the local socket"
    );
    assert!(matches!(
        rpc(d.http, &Request::DaemonInfo).await,
        Ok(ResponseOk::DaemonInfo(_))
    ));
    d._daemon.stop().await;
}

#[tokio::test]
async fn uploaded_program_imports() {
    let wasm = rps_wasm();
    let d = daemon(&wasm).await;
    for host in [&d.host_a, &d.host_b] {
        ok(call(
            host,
            &HostRequest::ProgramRemove {
                program: d.program_id.to_string(),
            },
        )
        .await);
    }
    let reply = http(d.http, "POST", "/uploads", Some("application/wasm"), &wasm).await;
    assert_eq!(reply.status, 201);
    let uploaded: Uploaded = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(uploaded.length, wasm.len() as u64);
    for host in [&d.host_a, &d.host_b] {
        let imported = rpc(
            d.http,
            &host.request(&HostRequest::ProgramImport {
                source: FileSource::Upload(uploaded.upload),
            }),
        )
        .await;
        let ResponseOk::Program(program) = ok(imported) else {
            panic!("expected program");
        };
        assert_eq!(program.summary.program_hash, d.program_id);
        let found = rpc(
            d.http,
            &host.request(&HostRequest::ProgramGet {
                program: d.program_id.to_string(),
            }),
        )
        .await;
        assert!(matches!(found, Ok(ResponseOk::Program(_))));
    }
    d._daemon.stop().await;
}

#[tokio::test]
async fn octet_stream_upload_is_not_a_program() {
    let wasm = rps_wasm();
    let d = daemon(&wasm).await;
    let reply = http(
        d.http,
        "POST",
        "/uploads",
        Some("application/octet-stream"),
        &wasm,
    )
    .await;
    assert_eq!(reply.status, 201);
    let uploaded: Uploaded = serde_json::from_slice(&reply.body).unwrap();
    let error = rpc(
        d.http,
        &d.host_a.request(&HostRequest::ProgramImport {
            source: FileSource::Upload(uploaded.upload),
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, ApiErrorCode::BadRequest);
    assert_eq!(
        error.message,
        format!(
            "upload {} was not sent as application/wasm",
            uploaded.upload
        )
    );
    let error = rpc(
        d.http,
        &d.host_a.request(&HostRequest::ProgramImport {
            source: FileSource::Upload(BlobHash([1; 32])),
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, ApiErrorCode::NotFound);
    assert_eq!(error.message, "no such upload");
    assert_eq!(
        http(
            d.http,
            "POST",
            "/uploads",
            Some("application/wasm; charset=utf-8"),
            b""
        )
        .await
        .status,
        415
    );
    d._daemon.stop().await;
}

#[tokio::test]
async fn uploaded_blob_round_trips() {
    let d = daemon(&rps_wasm()).await;
    let bytes = b"browser blob\0\xff";
    let reply = http(
        d.http,
        "POST",
        "/uploads",
        Some("application/octet-stream"),
        bytes,
    )
    .await;
    assert_eq!(reply.status, 201);
    let uploaded: Uploaded = serde_json::from_slice(&reply.body).unwrap();
    let result = rpc(
        d.http,
        &d.host_a.request(&HostRequest::BlobImport {
            source: FileSource::Upload(uploaded.upload),
        }),
    )
    .await;
    assert_eq!(
        ok(result),
        ResponseOk::BlobImported {
            hash: uploaded.upload,
            length: bytes.len() as u64
        }
    );
    // An owned copy survives removal of the process-wide upload.
    std::fs::remove_file(
        d._home
            .path()
            .join("uploads")
            .join(format!("{}.bin", uploaded.upload)),
    )
    .unwrap();
    let ResponseOk::BlobList(entries) =
        ok(rpc(d.http, &d.host_a.request(&HostRequest::BlobList)).await)
    else {
        panic!("expected blobs");
    };
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].hash, uploaded.upload);
    assert_eq!(entries[0].length, bytes.len() as u64);
    assert!(!entries[0].linked);
    let reply = http(
        d.http,
        "GET",
        &format!("/hosts/a/blobs/{}", uploaded.upload),
        None,
        b"",
    )
    .await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body, bytes);
    assert!(
        reply
            .headers
            .contains(&("content-type".into(), "application/octet-stream".into()))
    );
    assert!(reply.headers.contains(&(
        "content-disposition".into(),
        format!("attachment; filename=\"{}\"", uploaded.upload)
    )));
    assert_eq!(
        http(
            d.http,
            "GET",
            &format!("/hosts/b/blobs/{}", uploaded.upload),
            None,
            b""
        )
        .await
        .status,
        404
    );
    assert_eq!(
        http(
            d.http,
            "GET",
            &format!("/hosts/missing/blobs/{}", uploaded.upload),
            None,
            b""
        )
        .await
        .status,
        404
    );
    d._daemon.stop().await;
}

#[tokio::test]
async fn events_stream_all_hosts() {
    let d = daemon(&rps_wasm()).await;
    let mut events = EventStream::open(d.http).await;
    let mut hosts = BTreeSet::new();
    while hosts.len() < 2 {
        let (event, data) = events.next().await;
        assert_eq!(event, "host");
        let frame: EventFrame = serde_json::from_str(&data).unwrap();
        if frame.kind() == "host.started" {
            hosts.insert(frame.host.id);
        }
    }
    assert_eq!(hosts, BTreeSet::from(["a".to_owned(), "b".to_owned()]));
    let exec_id = ExecId([31; 32]);
    ok(rpc(
        d.http,
        &d.host_a.request(&HostRequest::ExecNew {
            exec_id,
            program: d.program_id.to_string(),
            params: Some(serde_json::Value::Null),
            ensemble: EnsembleSpec::Create {
                participant_count: 2,
            },
            blobs: vec![],
        }),
    )
    .await);
    loop {
        let (event, data) = events.next().await;
        if event != "host" {
            continue;
        }
        let frame: EventFrame = serde_json::from_str(&data).unwrap();
        if frame.kind() == "exec.created" {
            assert_eq!(frame.host.id, "a");
            break;
        }
    }
    ok(rpc(
        d.http,
        &Request::HostsOpen {
            id: Some("c".into()),
            user_agent: "http-test".into(),
        },
    )
    .await);
    loop {
        let (event, data) = events.next().await;
        if event != "host" {
            continue;
        }
        let frame: EventFrame = serde_json::from_str(&data).unwrap();
        if frame.kind() == "host.started" && frame.host.id == "c" {
            break;
        }
    }
    drop(events);
    d._daemon.stop().await;
}

#[tokio::test]
async fn uploads_are_cleared_on_start() {
    let directory = tempfile::tempdir().unwrap();
    let home = Home::from_root(directory.path().into()).unwrap();
    std::fs::create_dir_all(home.uploads_dir()).unwrap();
    let stale = home.uploads_dir().join("stale.bin");
    std::fs::write(&stale, b"stale").unwrap();
    let daemon = Daemon::start(
        vec!["a".parse().unwrap()],
        HttpConfig::new("127.0.0.1:0".parse().unwrap(), None).unwrap(),
        arena0_test_engine::shared_test_engine(),
        home,
        true,
    )
    .await
    .unwrap();
    assert!(!stale.exists());
    daemon.stop().await;
}
