use std::process::{Command, Stdio};

use anyhow::Context as _;
use tempfile::TempDir;

#[test]
fn duplicate_hosts_are_rejected_before_fresh_home_provisioning() -> anyhow::Result<()> {
    let home = TempDir::new().context("create isolated arena0 home")?;
    let output = Command::new(env!("CARGO_BIN_EXE_arena0d"))
        .env("ARENA0_HOME", home.path())
        .env_remove("ARENA0_SOCKET")
        .args(["--host", "same", "--host", "same"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("run arena0d with duplicate Hosts")?;

    assert!(
        !output.status.success(),
        "duplicate Hosts unexpectedly started"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ensemble hosts must use distinct names; duplicate same"),
        "unexpected duplicate Host error: {stderr}"
    );
    assert!(
        !home.path().join("hosts").exists(),
        "duplicate validation provisioned Host state under {}",
        home.path().display()
    );
    Ok(())
}
