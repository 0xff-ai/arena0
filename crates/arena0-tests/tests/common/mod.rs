//! Shared local-only harness for daemon API integration tests.
//!
//! Each Host owns a Unix socket, identity, registry, and receipt store. The
//! public [`Daemon`] supervisor supplies their shared local Ensemble.

#![allow(dead_code, unreachable_pub)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arena0_api::{NextEvent, Request, Response, ResponseOk};
use arena0_daemon::{Daemon, HostConfig, McpConfig, Paths};
use arena0_program::ProgramHash;
use arena0_protocol::{PeerId, SessionHash};
use tokio::io::BufReader;
use tokio::net::UnixStream;

/// The required rock-paper-scissors guest artifact.
pub fn rps_wasm() -> Vec<u8> {
    arena0_tests::wasm::program_wasm("rock_paper_scissors")
}

/// The required cumulative-sum guest artifact.
pub fn cumulative_sum_wasm() -> Vec<u8> {
    arena0_tests::wasm::program_wasm("cumulative_sum")
}

/// One framed request -> one framed response on a fresh Unix connection.
pub async fn call(socket: &Path, req: &Request) -> Response {
    let stream = UnixStream::connect(socket)
        .await
        .expect("connect to daemon socket");
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    arena0_api::frame::write_frame(&mut write, req)
        .await
        .expect("write request");
    arena0_api::frame::read_frame::<_, Response>(&mut read)
        .await
        .expect("read response")
        .expect("response frame")
}

/// Send one raw JSON frame when testing request-deserialization failures.
pub async fn call_json(
    socket: &Path,
    req: &serde_json::Value,
) -> std::io::Result<Option<Response>> {
    let stream = UnixStream::connect(socket).await?;
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    arena0_api::frame::write_frame(&mut write, req).await?;
    arena0_api::frame::read_frame::<_, Response>(&mut read).await
}

pub fn ok(resp: Response) -> ResponseOk {
    resp.unwrap_or_else(|error| panic!("daemon error: {error}"))
}

/// Drive a rock-paper-scissors execution to completion, answering every callout
/// with the JSON string `"Rock"`.
pub async fn drive(socket: &Path, exec_id: arena0_protocol::ExecId) -> SessionHash {
    drive_script(socket, exec_id, &[]).await
}

/// Drive one execution to completion with a deterministic answer script. An
/// empty script answers every callout with `"Rock"`.
pub async fn drive_script(
    socket: &Path,
    exec_id: arena0_protocol::ExecId,
    script: &[serde_json::Value],
) -> SessionHash {
    let mut cursor = 0;
    loop {
        match ok(call(socket, &Request::ExecNext { exec_id }).await) {
            ResponseOk::Next(NextEvent::Callout { pending_id, .. }) => {
                let answer = if script.is_empty() {
                    serde_json::json!("Rock")
                } else {
                    script
                        .get(cursor)
                        .unwrap_or_else(|| panic!("script exhausted at callout {cursor}"))
                        .clone()
                };
                cursor += 1;
                ok(call(
                    socket,
                    &Request::ExecSubmit {
                        exec_id,
                        pending_id,
                        answer: Some(answer),
                    },
                )
                .await);
            }
            ResponseOk::Next(NextEvent::Completed { session_id, .. }) => return session_id,
            ResponseOk::Next(NextEvent::Failed { reason }) => {
                panic!("execution failed: {reason}")
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }
}

pub async fn wait_for_socket(socket: &Path) {
    for _ in 0..200 {
        if UnixStream::connect(socket).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("daemon socket never came up: {}", socket.display());
}

pub async fn import(socket: &Path, wasm: &[u8]) -> ProgramHash {
    match ok(call(
        socket,
        &Request::ProgramImport {
            wasm: wasm.to_vec(),
        },
    )
    .await)
    {
        ResponseOk::Program(detail) => detail.summary.program_hash,
        other => panic!("unexpected import response: {other:?}"),
    }
}

pub fn created(resp: Response) -> arena0_protocol::ExecId {
    match ok(resp) {
        ResponseOk::ExecCreated { exec_id, .. } => exec_id,
        other => panic!("unexpected new-exec response: {other:?}"),
    }
}

/// Two Host services wired through one public process-level `Daemon` supervisor.
/// Both registries contain the same imported program, so P1 admission exercises
/// only explicit creator/join negotiation and never remote program transfer.
pub struct TwoDaemons {
    pub _dir_a: tempfile::TempDir,
    pub _dir_b: tempfile::TempDir,
    /// The one process-level public supervisor that owns both Host services.
    pub _supervisor: Arc<Daemon>,
    pub peer_a: PeerId,
    pub peer_b: PeerId,
    pub sock_a: PathBuf,
    pub sock_b: PathBuf,
    pub program_id: ProgramHash,
}

/// Boot one public [`Daemon`] supervisor with two [`HostConfig`]s, import the
/// program into both registries, and return their sockets and identities.
pub async fn two_daemons(wasm: &[u8]) -> TwoDaemons {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let sock_a = dir_a.path().join("arena0.sock");
    let sock_b = dir_b.path().join("arena0.sock");
    let host_a = HostConfig::open(
        "a",
        Paths::new(dir_a.path().to_path_buf(), sock_a.clone()),
        true,
    )
    .unwrap_or_else(|error| panic!("open host a: {error}"));
    let host_b = HostConfig::open(
        "b",
        Paths::new(dir_b.path().to_path_buf(), sock_b.clone()),
        true,
    )
    .unwrap_or_else(|error| panic!("open host b: {error}"));
    let peer_a = host_a.peer_id();
    let peer_b = host_b.peer_id();
    let mcp = McpConfig::new(SocketAddr::from(([127, 0, 0, 1], 0)), None).unwrap();
    let engine = Arc::new(arena0_sandbox::WasmtimeEngine::new().expect("sandbox engine"));
    let supervisor = Daemon::start(vec![host_a, host_b], mcp, engine)
        .await
        .unwrap_or_else(|error| panic!("start daemon supervisor: {error}"));
    tokio::spawn(Arc::clone(&supervisor).serve());
    wait_for_socket(&sock_a).await;
    wait_for_socket(&sock_b).await;

    let program_id = import(&sock_a, wasm).await;
    assert_eq!(import(&sock_b, wasm).await, program_id);

    TwoDaemons {
        _dir_a: dir_a,
        _dir_b: dir_b,
        _supervisor: supervisor,
        peer_a,
        peer_b,
        sock_a,
        sock_b,
        program_id,
    }
}
