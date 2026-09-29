//! End-to-end checks that `arena0 ui` opens the daemon's HTTP surface.
#![cfg(unix)]

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use arena0_client::api::{Request, ResponseOk};
use arena0_client::proto::DaemonClient;
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

fn arena0d_binary() -> PathBuf {
    let target = Path::new(env!("CARGO_BIN_EXE_arena0"))
        .parent()
        .expect("arena0 test binary has a directory");
    let target = if target.file_name().is_some_and(|name| name == "deps") {
        target.parent().expect("deps has a parent")
    } else {
        target
    };
    let daemon = target.join("arena0d");
    assert!(
        daemon.is_file(),
        "arena0d is required at {}; run `cargo build -p arena0d` first",
        daemon.display()
    );
    daemon
}

fn arena0(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_arena0"));
    command
        .env("ARENA0_HOME", home)
        .env_remove("ARENA0_SOCKET")
        .env_remove("ARENA0_CONTEXT")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("RUST_LOG");
    command
}

#[tokio::test]
async fn ui_prints_the_daemon_url() {
    let _ = arena0d_binary();
    let home = tempfile::tempdir().expect("temporary arena0 home");
    let mut child = arena0(home.path())
        .args(["--json", "ui", "--no-open"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn arena0 ui");
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().expect("piped stdout"))
        .read_line(&mut line)
        .expect("read the URL line");
    let printed: Value = serde_json::from_str(&line).expect("URL JSON");
    let url = printed["url"].as_str().expect("url field");
    let client = DaemonClient::new(home.path().join("arena0.sock"));
    let ResponseOk::DaemonInfo(info) = client.call(&Request::DaemonInfo).await.unwrap() else {
        panic!("expected daemon.info");
    };
    assert_eq!(url, format!("{}/", info.http_url));

    let address = info.http_url.strip_prefix("http://").expect("HTTP URL");
    let mut http = TcpStream::connect(address).await.expect("connect HTTP");
    let body = r#"{"method":"daemon.info"}"#;
    let request = format!(
        "POST /rpc HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    http.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    http.read_to_string(&mut response).await.unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").expect("HTTP response");
    assert!(headers.starts_with("HTTP/1.1 200 "), "{headers}");
    let response: Value = serde_json::from_str(body).expect("RPC JSON");
    assert!(response.get("Ok").is_some(), "{response}");

    let pid = libc::pid_t::try_from(child.id()).expect("pid");
    // SIGINT lets the CLI stop the daemon it owns before the temporary home is removed.
    // SAFETY: the unreaped child owns this pid.
    unsafe { libc::kill(pid, libc::SIGINT) };
    assert!(child.wait().expect("wait for UI").success());
}

#[test]
fn ui_attach_fails_without_a_daemon() {
    let home = tempfile::tempdir().expect("temporary arena0 home");
    let output = arena0(home.path())
        .args(["ui", "--attach", "--no-open"])
        .output()
        .expect("run arena0 ui --attach");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Daemon not reachable"), "stderr: {stderr}");
}
