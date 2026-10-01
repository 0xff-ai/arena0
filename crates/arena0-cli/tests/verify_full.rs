//! Real-binary proof of full file verification and unchanged offline light output.
//! Failure cases: --full accidentally verifies offline; full text/JSON loses the
//! outcome; light rendering changes; daemon routing returns only light evidence.
#![cfg(unix)]

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use arena0_client::api::{HostRequest, ResponseOk};
use arena0_client::proto::DaemonClient;
use arena0_home::{Home, HostName};
use tokio::process::Command;

#[tokio::test]
async fn full_file_verification_renders_the_program_outcome_and_light_stays_offline() {
    let binary = Path::new(env!("CARGO_BIN_EXE_arena0"));
    let daemon_binary = binary.parent().expect("binary directory").join("arena0d");
    assert!(
        daemon_binary.is_file(),
        "build this checkout with cargo build -p arena0d first"
    );
    let directory = tempfile::tempdir().expect("private Home");
    let home = Home::from_root(directory.path().to_path_buf()).expect("Home");
    let agent =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/agents/first_allowed.py");
    let receipt = directory.path().join("receipt.json");
    let daemon_log = directory.path().join("daemon.log");
    // serve resolves the sibling daemon built from this checkout. Its HTTP
    // listener binds port 0, and every test owns a distinct Home and Unix socket.
    let command = || {
        let mut command = Command::new(binary);
        command
            .env("ARENA0_HOME", directory.path())
            .env("NO_COLOR", "1")
            .env_remove("ARENA0_SOCKET")
            .env_remove("ARENA0_CONTEXT")
            .env_remove("CODEX_THREAD_ID")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    };
    let mut daemon = command()
        .arg("serve")
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&daemon_log).expect("daemon log"))
        .spawn()
        .expect("start arena0 serve");
    let run = |args: Vec<String>| {
        let mut process = command();
        process
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        async move {
            let output = process.output().await.expect("run arena0");
            assert!(
                output.status.success(),
                "arena0 failed: {}\nstdout: {}\nstderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).expect("UTF-8 output")
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(90), async {
        let client = DaemonClient::new(home.socket());
        let host = HostName::try_from("host-01").expect("Host name");
        loop {
            assert!(
                daemon.try_wait().expect("daemon status").is_none(),
                "daemon exited: {}",
                std::fs::read_to_string(&daemon_log).unwrap()
            );
            if matches!(
                client.call_host(&host, &HostRequest::Info).await,
                Ok(ResponseOk::HostStatus(_))
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
        let output = run(vec![
            "--json".into(),
            "run".into(),
            "rock-paper-scissors".into(),
            "--agent".into(),
            format!("host-01={}", agent.display()),
            "--agent".into(),
            format!("host-02={}", agent.display()),
        ])
        .await;
        let completed: serde_json::Value = serde_json::from_str(&output).expect("run JSON");
        let session = completed["session_id"].as_str().expect("session id");
        let outcome = &completed["outcome"];
        assert!(!outcome.is_null(), "{completed}");
        run(vec![
            "receipt".into(),
            "get".into(),
            session.into(),
            "--host".into(),
            "host-01".into(),
            "--out".into(),
            receipt.display().to_string(),
        ])
        .await;

        let text = run(vec![
            "verify".into(),
            "--full".into(),
            receipt.display().to_string(),
            "--host".into(),
            "host-01".into(),
        ])
        .await;
        assert_eq!(text.lines().next(), Some("verified (full)"), "{text}");
        let rendered = text
            .lines()
            .find_map(|line| line.strip_prefix("  outcome     "))
            .expect("full outcome line");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(rendered).expect("JSON outcome"),
            *outcome
        );

        let json = run(vec![
            "--json".into(),
            "verify".into(),
            "--full".into(),
            receipt.display().to_string(),
            "--host".into(),
            "host-01".into(),
        ])
        .await;
        let full: serde_json::Value = serde_json::from_str(&json).expect("full JSON");
        assert_eq!(full["full"], true);
        assert_eq!(full["outcome_json"], *outcome);

        daemon.kill().await.expect("stop owned daemon");
        daemon.wait().await.expect("reap owned daemon");
        let light = run(vec!["verify".into(), receipt.display().to_string()]).await;
        assert_eq!(light.lines().next(), Some("verified"));
        assert!(
            light
                .lines()
                .any(|line| line
                    == "  outcome     (JSON projection unavailable without the program)"),
            "{light}"
        );
        let light_json = run(vec![
            "--json".into(),
            "verify".into(),
            receipt.display().to_string(),
        ])
        .await;
        let light: serde_json::Value = serde_json::from_str(&light_json).expect("light JSON");
        assert!(light.get("full").is_none());
        assert!(light.get("outcome_json").is_none());

        let down = command()
            .args([
                "verify",
                "--full",
                receipt.to_str().unwrap(),
                "--host",
                "host-01",
            ])
            .output()
            .await
            .expect("verify with daemon down");
        assert!(!down.status.success());
        assert!(
            String::from_utf8_lossy(&down.stderr).contains("full verification needs the daemon at"),
            "{}",
            String::from_utf8_lossy(&down.stderr)
        );
    })
    .await;
    assert!(
        result.is_ok(),
        "verification journey exceeded 90s\ndaemon: {}",
        std::fs::read_to_string(daemon_log).expect("daemon diagnostics")
    );
}
