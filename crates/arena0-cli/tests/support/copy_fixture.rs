use std::path::Path;
use std::process::Command;

/// Keep destination write descriptors out of the multithreaded test process:
/// another test's fork must not inherit one and prevent execution with ETXTBSY.
/// The copy process is reaped before the caller can execute the destination.
pub(super) fn copy_executable(source: &Path, destination: &Path) {
    let status = Command::new("/bin/cp")
        .arg("--")
        .arg(source)
        .arg(destination)
        .status()
        .expect("start executable fixture copy");
    assert!(status.success(), "executable fixture copy: {status}");
}
