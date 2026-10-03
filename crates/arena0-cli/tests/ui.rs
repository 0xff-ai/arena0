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
        .env_remove("ARENA0_UI_DIR")
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
    let ui = home.path().join("ui");
    std::fs::create_dir_all(ui.join("assets")).unwrap();
    let index = "<div id=\"root\"></div><script type=\"module\" src=\"/assets/app.js\"></script>";
    std::fs::write(ui.join("index.html"), index).unwrap();
    std::fs::write(ui.join("assets/app.js"), "console.log('arena0');").unwrap();
    std::fs::write(home.path().join("secret.txt"), "PRIVATE_UI_SECRET").unwrap();
    let mut child = arena0(home.path())
        .env("ARENA0_UI_DIR", &ui)
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
    assert!(info.ui);

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

    let mut responses = Vec::new();
    for path in [
        "/",
        "/assets/app.js",
        "/sessions/abc",
        "/assets/missing.js",
        "/../secret.txt",
    ] {
        // Raw TCP preserves dot segments that an HTTP client would normalize.
        let mut http = TcpStream::connect(address).await.unwrap();
        http.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let mut response = String::new();
        http.read_to_string(&mut response).await.unwrap();
        responses.push(response);
    }

    let pid = libc::pid_t::try_from(child.id()).expect("pid");
    // SIGINT lets the CLI stop the daemon it owns before the temporary home is removed.
    // SAFETY: the unreaped child owns this pid.
    unsafe { libc::kill(pid, libc::SIGINT) };
    assert!(child.wait().expect("wait for UI").success());

    for (i, response) in responses.iter().enumerate() {
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        let headers = headers.to_ascii_lowercase();
        assert!(
            headers.contains("content-security-policy: default-src 'self';"),
            "{response}"
        );
        match i {
            0 | 2 => {
                assert!(headers.starts_with("http/1.1 200 "), "{response}");
                assert!(headers.contains("content-type: text/html"), "{response}");
                assert!(headers.contains("cache-control: no-cache"), "{response}");
                assert_eq!(body, index);
            }
            1 => {
                assert!(headers.starts_with("http/1.1 200 "), "{response}");
                assert!(
                    headers.contains("content-type: text/javascript")
                        || headers.contains("content-type: application/javascript"),
                    "{response}"
                );
                assert!(
                    headers.contains("cache-control: public, max-age=31536000, immutable"),
                    "{response}"
                );
                assert_eq!(body, "console.log('arena0');");
            }
            3 => assert!(headers.starts_with("http/1.1 404 "), "{response}"),
            4 => assert!(!body.contains("PRIVATE_UI_SECRET"), "{response}"),
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn ui_without_a_ui_dir_fails_and_stops_the_daemon_it_started() {
    let _ = arena0d_binary();
    let home = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::from(arena0(home.path()))
        .args(["ui", "--no-open"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait()).await;
    // Also clean up when a broken UI check waits for Ctrl-C instead of failing.
    if result.is_err() {
        let pid = libc::pid_t::try_from(child.id().unwrap()).unwrap();
        // SAFETY: the unreaped child owns this pid.
        unsafe { libc::kill(pid, libc::SIGINT) };
        child.wait().await.unwrap();
    }
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .await
        .unwrap();
    let status = result
        .expect("UI without assets must exit, not wait for Ctrl-C")
        .unwrap();
    assert!(!status.success(), "{stderr}");
    assert!(stderr.contains("serves no web UI"), "{stderr}");
    assert!(
        !DaemonClient::new(home.path().join("arena0.sock"))
            .daemon_up()
            .await
    );
}

#[tokio::test]
async fn daemon_without_a_ui_dir_serves_the_api_only() {
    let _ = arena0d_binary();
    let home = tempfile::tempdir().unwrap();
    let mut child = arena0(home.path())
        .arg("serve")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    // Startup's trace is the readiness barrier; no fixed sleep or reused server.
    let mut startup = BufReader::new(child.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert_ne!(
            startup.read_line(&mut line).unwrap(),
            0,
            "daemon exited before readiness"
        );
        if line.contains("initialization_complete") {
            break;
        }
    }
    let client = DaemonClient::new(home.path().join("arena0.sock"));
    let ResponseOk::DaemonInfo(info) = client.call(&Request::DaemonInfo).await.unwrap() else {
        panic!("expected daemon.info");
    };
    let address = info.http_url.strip_prefix("http://").unwrap();
    let mut http = TcpStream::connect(address).await.unwrap();
    http.write_all(
        format!("GET / HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut response = String::new();
    http.read_to_string(&mut response).await.unwrap();
    client.call(&Request::DaemonStop).await.unwrap();
    assert!(child.wait().unwrap().success());
    assert!(!info.ui);
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(headers.starts_with("HTTP/1.1 404 "), "{response}");
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("content-type: text/plain"),
        "{response}"
    );
    assert_eq!(
        body,
        "this daemon serves no web UI; start it with ARENA0_UI_DIR set to a built arena0-ui"
    );
}

#[test]
fn ui_dir_without_index_html_stops_arena0d() {
    let home = tempfile::tempdir().unwrap();
    let ui = tempfile::tempdir().unwrap();
    let output = Command::new(arena0d_binary())
        .env("ARENA0_HOME", home.path())
        .env("ARENA0_UI_DIR", ui.path())
        .env_remove("ARENA0_SOCKET")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ARENA0_UI_DIR") && stderr.contains("index.html"),
        "{stderr}"
    );
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
