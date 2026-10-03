#![cfg(target_os = "linux")]

use std::io::{Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::process::Command;

#[path = "support/executable_fixture.rs"]
mod executable_fixture;

struct HeldFork {
    socket: UnixStream,
    pid: libc::pid_t,
}

impl HeldFork {
    fn new() -> Self {
        let (mut parent, child) = UnixStream::pair().expect("fork barrier sockets");
        let parent_fd = parent.as_raw_fd();
        let child_fd = child.as_raw_fd();
        // SAFETY: after fork the child uses only async-signal-safe libc calls
        // and stack storage, then _exit. It never allocates, locks, or invokes
        // Rust destructors inherited from the multithreaded test process.
        let pid = unsafe {
            let pid = libc::fork();
            if pid == 0 {
                libc::close(parent_fd);
                let ready = b'r';
                if libc::write(child_fd, (&ready as *const u8).cast(), 1) != 1 {
                    libc::_exit(1);
                }
                let mut release = 0_u8;
                let received = libc::read(child_fd, (&mut release as *mut u8).cast(), 1);
                libc::_exit(if received == 1 { 0 } else { 1 });
            }
            pid
        };
        assert!(pid > 0, "fork: {}", std::io::Error::last_os_error());
        drop(child);
        let mut ready = [0_u8; 1];
        parent
            .read_exact(&mut ready)
            .expect("child reached fork barrier");
        assert_eq!(ready, [b'r']);
        Self {
            socket: parent,
            pid,
        }
    }
}

impl Drop for HeldFork {
    fn drop(&mut self) {
        self.socket.write_all(b"x").expect("release forked child");
        let mut status = 0;
        // SAFETY: pid belongs to this guard, and status is a writable int.
        let reaped = unsafe { libc::waitpid(self.pid, &mut status, 0) };
        assert_eq!(reaped, self.pid, "reap forked child");
        assert_eq!(status, 0, "forked child exited cleanly");
    }
}

#[test]
fn executable_fixture_runs_while_an_unrelated_fork_is_held() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let control = directory.path().join("control");
    executable_fixture::install_script(&control, "#!/bin/sh\nexit 0\n");
    assert!(
        Command::new(&control)
            .status()
            .expect("control fixture")
            .success()
    );
    let script = directory.path().join("script");
    let mut peer = None;
    executable_fixture::install_script_observed(
        &script,
        "#!/bin/sh\nprintf 'fixture-ok\\n'\n",
        || {
            peer = Some(HeldFork::new());
        },
    );
    // The writer has returned and closed its own descriptors. The unrelated
    // child remains alive, so any writer it inherited is still held open.
    let output = Command::new(&script).output();
    drop(peer);
    let output = output.expect("execute fixture while unrelated fork is held");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"fixture-ok\n");
}
