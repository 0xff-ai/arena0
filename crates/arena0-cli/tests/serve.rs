#![cfg(unix)]

use std::path::Path;
use std::process::Command;

#[path = "support/copy_fixture.rs"]
mod copy_fixture;
#[path = "support/executable_fixture.rs"]
mod executable_fixture;

use copy_fixture::copy_executable;
use executable_fixture::install_script;

#[test]
fn serve_execs_the_sibling_daemon_with_exact_arguments_and_status() {
    let directory = tempfile::tempdir().expect("temporary install directory");
    let arena0 = directory.path().join("arena0");
    let arena0d = directory.path().join("arena0d");
    copy_executable(Path::new(env!("CARGO_BIN_EXE_arena0")), &arena0);
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
    copy_executable(Path::new(env!("CARGO_BIN_EXE_arena0")), &arena0);
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
