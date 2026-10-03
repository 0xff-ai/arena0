use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;

pub(super) fn install_script(path: &Path, body: &str) {
    install_script_observed(path, body, || {});
}

/// The observer marks the boundary between writing the bytes and publishing
/// the executable permissions. It lets the fork regression hold an unrelated
/// child at exactly the point where a parent-owned writer can be inherited.
pub(super) fn install_script_observed(path: &Path, body: &str, written: impl FnOnce()) {
    // A parent-owned writable descriptor can be inherited by another test's
    // fork even with CLOEXEC, and remain open until that child execs. Linux
    // then rejects executing this fixture with ETXTBSY after our own close.
    // Only this short-lived shell opens the fixture for writing; unrelated
    // children of the test process cannot inherit its descriptors. Wait for
    // exit before publishing permissions. Body/path are positional arguments,
    // never shell source, so fixture bytes and paths remain literal.
    let status = Command::new("/bin/sh")
        .args(["-c", "printf '%s' \"$2\" > \"$1\"", "fixture-writer"])
        .arg(path)
        .arg(body)
        .status()
        .expect("start fixture writer");
    assert!(status.success(), "fixture writer: {status}");
    written();
    let mut permissions = std::fs::metadata(path)
        .expect("fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("make fixture executable");
}
