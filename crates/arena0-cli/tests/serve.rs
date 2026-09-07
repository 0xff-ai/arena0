#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;

fn install_script(path: &Path, body: &str) {
    fs::write(path, body).expect("write fake arena0d");
    let mut permissions = fs::metadata(path)
        .expect("fake arena0d metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("make fake arena0d executable");
}

#[test]
fn serve_execs_the_sibling_daemon_with_exact_arguments_and_status() {
    let directory = tempfile::tempdir().expect("temporary install directory");
    let arena0 = directory.path().join("arena0");
    let arena0d = directory.path().join("arena0d");
    fs::copy(env!("CARGO_BIN_EXE_arena0"), &arena0).expect("copy arena0 binary");
    install_script(&arena0d, "#!/bin/sh\nprintf '%s\\n' \"$@\"\nexit 23\n");

    let output = Command::new(arena0)
        .args([
            "serve",
            "--hosts",
            "alpha,beta",
            "--mcp-listen",
            "127.0.0.1:7440",
            "--mcp-access-token-lifetime-secs",
            "7200",
        ])
        .output()
        .expect("run arena0 serve");

    assert_eq!(output.status.code(), Some(23));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "--host\nalpha\n--host\nbeta\n--mcp-listen\n127.0.0.1:7440\n--mcp-access-token-lifetime-secs\n7200\n"
    );
}

#[test]
fn serve_uses_platform_path_search_without_a_sibling() {
    let install = tempfile::tempdir().expect("temporary client install");
    let path_directory = tempfile::tempdir().expect("temporary PATH directory");
    let arena0 = install.path().join("arena0");
    fs::copy(env!("CARGO_BIN_EXE_arena0"), &arena0).expect("copy arena0 binary");
    install_script(
        &path_directory.path().join("arena0d"),
        "#!/bin/sh\nprintf 'path daemon\\n'\n",
    );

    let output = Command::new(arena0)
        .arg("serve")
        .env("PATH", path_directory.path())
        .output()
        .expect("run arena0 serve through PATH");

    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "path daemon\n");
}

#[test]
fn serve_rejects_client_only_global_options() {
    let output = Command::new(env!("CARGO_BIN_EXE_arena0"))
        .args(["--json", "serve"])
        .output()
        .expect("run invalid serve invocation");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("--socket, --host, and --json do not apply"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn serve_rejects_zero_token_lifetime_before_launching_the_daemon() {
    let output = Command::new(env!("CARGO_BIN_EXE_arena0"))
        .args(["serve", "--mcp-access-token-lifetime-secs", "0"])
        .output()
        .expect("run invalid token lifetime invocation");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("--mcp-access-token-lifetime-secs"),
        "{stderr}"
    );
    assert!(stderr.contains("invalid value"), "{stderr}");
}
