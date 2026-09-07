use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn invoke(input: &str, env_file: Option<&Path>, home: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_arena0"));
    command
        .args(["hook", "claude-session-start"])
        .env_remove("ARENA0_SOCKET")
        .env_remove("ARENA0_CACHE_DIR")
        .env_remove("ARENA0_CONTEXT")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("RUST_LOG")
        .env_remove("CLAUDE_ENV_FILE");
    if let Some(env_file) = env_file {
        command.env("CLAUDE_ENV_FILE", env_file);
    }
    if let Some(home) = home {
        command.env("ARENA0_HOME", home);
    } else {
        command.env_remove("ARENA0_HOME");
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run Claude SessionStart hook");
    if let Err(error) = child
        .stdin
        .take()
        .expect("hook stdin")
        .write_all(input.as_bytes())
    {
        assert!(
            input.len() > 64 * 1024 && error.kind() == std::io::ErrorKind::BrokenPipe,
            "write hook input: {error}"
        );
    }
    child
        .wait_with_output()
        .expect("wait for Claude SessionStart hook")
}

fn stderr(output: Output) -> String {
    assert!(!output.status.success(), "hook unexpectedly succeeded");
    assert!(
        output.stdout.is_empty(),
        "hook wrote stdout: {:?}",
        output.stdout
    );
    String::from_utf8(output.stderr).expect("hook stderr is UTF-8")
}

#[test]
fn hook_reports_missing_session_and_environment_file_without_creating_home_state() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let env_file = directory.path().join("claude.env");
    let home = directory.path().join("home");

    let error = stderr(invoke("{}", Some(&env_file), Some(&home)));
    assert!(error.contains("session_id"), "unexpected error: {error}");
    assert!(!env_file.exists());
    assert!(!home.exists());

    let error = stderr(invoke(r#"{"session_id":"session-1"}"#, None, None));
    assert!(
        error.contains("CLAUDE_ENV_FILE"),
        "unexpected error: {error}"
    );
}

#[test]
fn hook_rejects_empty_or_oversized_session_input() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let env_file = directory.path().join("claude.env");

    let error = stderr(invoke(r#"{"session_id":""}"#, Some(&env_file), None));
    assert!(
        error.contains("non-empty session_id"),
        "unexpected error: {error}"
    );
    assert!(!env_file.exists());

    let oversized = format!(r#"{{"session_id":"{}"}}"#, "x".repeat(70_000));
    let error = stderr(invoke(&oversized, Some(&env_file), None));
    assert!(error.contains("exceeds"), "unexpected error: {error}");
    assert!(!env_file.exists());
}

#[test]
fn hook_appends_context_and_preserves_existing_environment_bytes() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let env_file = directory.path().join("claude.env");
    let home = directory.path().join("home");
    fs::write(&env_file, b"existing=1").expect("existing environment");

    let output = invoke(
        r#"{"session_id":"session-1"}"#,
        Some(&env_file),
        Some(&home),
    );
    assert!(output.status.success(), "hook failed: {:?}", output.stderr);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(
        fs::read(&env_file).expect("read environment"),
        b"existing=1\nexport ARENA0_CONTEXT='claude:session-1'\n"
    );
    assert!(
        !home.exists(),
        "hook must not resolve or create arena0 Home"
    );
}

#[test]
fn hook_shell_quotes_session_id_without_allowing_command_injection() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let env_file = directory.path().join("claude.env");
    let marker = directory.path().join("injected");
    let session_id = format!("session'; touch {}; #", marker.display());
    let input = serde_json::json!({"session_id": session_id}).to_string();

    let output = invoke(&input, Some(&env_file), None);
    assert!(output.status.success(), "hook failed: {:?}", output.stderr);
    assert!(
        fs::read_to_string(&env_file)
            .expect("read environment")
            .contains("export ARENA0_CONTEXT=")
    );

    #[cfg(unix)]
    {
        let output = Command::new("sh")
            .args([
                "-c",
                "set -eu; . \"$1\"; printf '%s' \"$ARENA0_CONTEXT\"",
                "arena0-hook-test",
                env_file.to_str().expect("environment path is UTF-8"),
            ])
            .output()
            .expect("source generated environment");
        assert!(output.status.success(), "shell failed: {:?}", output.stderr);
        assert_eq!(
            String::from_utf8(output.stdout).expect("shell output is UTF-8"),
            format!("claude:{session_id}")
        );
        assert!(!marker.exists(), "session id escaped into shell code");
    }
}
