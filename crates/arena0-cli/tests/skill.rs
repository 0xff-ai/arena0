use std::fs;
use std::process::Command;

fn canonical_skill() -> String {
    fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../skills/arena0/SKILL.md"
    ))
    .expect("read canonical arena0 skill")
}

#[test]
fn skill_is_available_without_creating_home_state() {
    let home = tempfile::tempdir().expect("temporary arena0 home");
    let output = Command::new(env!("CARGO_BIN_EXE_arena0"))
        .args(["skill"])
        .env("ARENA0_HOME", home.path())
        .env("ARENA0_CACHE_DIR", home.path().join("cache"))
        .env("RUST_LOG", "arena0=trace")
        .output()
        .expect("run arena0 skill");

    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert_eq!(output.stdout, canonical_skill().as_bytes());
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {:?}",
        output.stderr
    );
    assert!(
        home.path()
            .read_dir()
            .expect("read temporary home")
            .next()
            .is_none()
    );
}

#[test]
fn skill_json_wraps_the_canonical_markdown() {
    let home = tempfile::tempdir().expect("temporary arena0 home");
    let output = Command::new(env!("CARGO_BIN_EXE_arena0"))
        .args(["--json", "skill"])
        .env("ARENA0_HOME", home.path())
        .output()
        .expect("run arena0 skill JSON");

    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("skill JSON");
    assert_eq!(value["name"], "arena0");
    assert_eq!(value["markdown"], canonical_skill());
    assert!(
        home.path()
            .read_dir()
            .expect("read temporary home")
            .next()
            .is_none()
    );
}

#[test]
fn skill_rejects_host_resolution_options() {
    for args in [
        vec!["--host", "host-01", "skill"],
        vec!["--socket", "/tmp/arena0.sock", "skill"],
        vec!["--tmp", "skill"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_arena0"))
            .args(args)
            .output()
            .expect("run invalid arena0 skill invocation");
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("do not apply to `arena0 skill`"),
            "unexpected stderr: {:?}",
            output.stderr
        );
    }
}

#[test]
fn agent_discovery_advertises_only_the_cli_path() {
    for args in [
        vec!["--help"],
        vec!["launch", "--help"],
        vec!["serve", "--help"],
        vec!["setup", "codex", "--help"],
        vec!["setup", "claude", "--help"],
        vec!["skill"],
        vec!["--json", "skill"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_arena0"))
            .args(&args)
            .output()
            .expect("read agent discovery surface");
        assert!(output.status.success(), "{args:?}: {:?}", output.stderr);
        let stdout = String::from_utf8(output.stdout).expect("UTF-8 discovery output");
        assert!(!stdout.is_empty());
        assert!(
            !stdout.to_ascii_lowercase().contains("mcp"),
            "{args:?}: {stdout}"
        );
        assert!(output.stderr.is_empty(), "{args:?}: {:?}", output.stderr);
    }
}
