//! End-to-end check of a `--human` participant through the real binary: it is
//! answered from piped stdin at the inline prompt, never a full-screen UI.
//!
//! Ways this can fail, decided before the test was written:
//! - the participant never reads stdin, so the run stalls (caught by the deadline);
//! - stdin ends before the program stops asking, so the run errors with
//!   "stdin closed before the callout was answered";
//! - an answer the program's schema rejects is re-prompted forever instead of
//!   completing;
//! - the run completes but the Host receipts are not verified.
#![cfg(unix)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(90);

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

#[test]
fn human_participant_is_answered_from_piped_stdin_and_the_receipt_is_printed() {
    let _ = arena0d_binary();
    let home = tempfile::tempdir().expect("temporary arena0 home");
    let mut child = Command::new(env!("CARGO_BIN_EXE_arena0"))
        .args([
            "run",
            "rock-paper-scissors",
            "--human",
            "host-01",
            "--agent",
            &format!(
                "host-02={}",
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../examples/agents/first_allowed.py")
                    .display()
            ),
        ])
        .env("ARENA0_HOME", home.path())
        .env_remove("ARENA0_SOCKET")
        .env_remove("ARENA0_CONTEXT")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn arena0 run");

    // One JSON answer per line, one per round of the three-round program.
    // The executable opponent always plays the first allowed move, Rock.
    let mut stdin = child.stdin.take().expect("piped stdin");
    stdin
        .write_all(b"\"Paper\"\n\"Paper\"\n\"Paper\"\n")
        .expect("write answers");
    drop(stdin);

    let deadline = Instant::now() + DEADLINE;
    while child.try_wait().expect("poll arena0 run").is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().expect("collect output");
            panic!(
                "arena0 run did not finish within {DEADLINE:?}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
    let output = child.wait_with_output().expect("collect output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "arena0 run failed with {}\nstdout: {stdout}\nstderr: {stderr}",
        output.status
    );
    assert!(stdout.contains("completed rock-paper-scissors"), "{stdout}");
    assert!(stdout.contains("receipt      "), "{stdout}");
    assert!(stdout.contains("verified     2/2 receipts"), "{stdout}");
    assert!(stderr.contains("callout  ChooseMove"), "{stderr}");
}
