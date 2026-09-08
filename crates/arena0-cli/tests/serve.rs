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
        .args(["serve", "--hosts", "alpha,beta"])
        .output()
        .expect("run arena0 serve");

    assert_eq!(output.status.code(), Some(23));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "--host\nalpha\n--host\nbeta\n"
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
fn mcp_options_are_rejected_before_launching_the_daemon() {
    for (command, option, value) in [
        ("serve", "--mcp-listen", "127.0.0.1:7440"),
        ("serve", "--mcp-access-token-lifetime-secs", "7200"),
        ("launch", "--mcp-listen", "127.0.0.1:7440"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_arena0"))
            .args([command, option, value])
            .output()
            .expect("run unsupported MCP option");

        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(option), "{stderr}");
        assert!(stderr.contains("unexpected argument"), "{stderr}");
    }
}
