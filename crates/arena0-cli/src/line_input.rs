//! Cancellable line input for human CLI callouts.
//!
//! Tokio's standard-input adapter delegates blocking reads to a runtime worker,
//! which cannot be stopped while a terminal is waiting for a line.  This small
//! Unix adapter owns a poll thread and a socket-pair wakeup, so dropping the
//! read future wakes and joins the thread instead of detaching it.

use std::io;
use std::os::fd::{AsRawFd as _, RawFd};
use std::os::unix::net::UnixStream;
use std::thread::JoinHandle;

use tokio::sync::oneshot;

enum ReadOutcome {
    Line(String),
    Closed,
    Cancelled,
}

struct LineRead {
    cancel: UnixStream,
    result: Option<oneshot::Receiver<io::Result<ReadOutcome>>>,
    worker: Option<JoinHandle<()>>,
}

impl LineRead {
    fn start(input: RawFd) -> io::Result<Self> {
        let (cancel, wake) = UnixStream::pair()?;
        let (result_tx, result) = oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("arena0-line-input".to_owned())
            .spawn(move || {
                let outcome = poll_line(input, wake.as_raw_fd());
                let _ = result_tx.send(outcome);
            })?;
        Ok(Self {
            cancel,
            result: Some(result),
            worker: Some(worker),
        })
    }

    async fn wait(mut self) -> io::Result<ReadOutcome> {
        let result = self
            .result
            .take()
            .expect("line input result has one owner")
            .await
            .map_err(|_| io::Error::other("line input worker stopped without a result"))?;
        self.join_worker()?;
        result
    }

    fn join_worker(&mut self) -> io::Result<()> {
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| io::Error::other("line input worker panicked"))?;
        }
        Ok(())
    }

    fn cancel_and_join(&mut self) {
        if self.worker.is_none() {
            return;
        }
        let _ = std::io::Write::write_all(&mut self.cancel, &[1]);
        let _ = self.join_worker();
    }
}

impl Drop for LineRead {
    fn drop(&mut self) {
        self.cancel_and_join();
    }
}

/// Read one UTF-8 line from standard input. Dropping this future cancels and
/// joins its owned reader thread.
pub(crate) async fn read_line() -> io::Result<Option<String>> {
    match LineRead::start(libc::STDIN_FILENO)?.wait().await? {
        ReadOutcome::Line(line) => Ok(Some(line)),
        ReadOutcome::Closed => Ok(None),
        ReadOutcome::Cancelled => Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "line input cancelled",
        )),
    }
}

fn poll_line(input: RawFd, cancel: RawFd) -> io::Result<ReadOutcome> {
    let mut bytes = Vec::new();
    loop {
        let mut descriptors = [
            libc::pollfd {
                fd: input,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: cancel,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // Both descriptors are borrowed for this call and remain valid for the
        // lifetime of the worker thread. `poll` only writes their `revents` fields.
        let ready = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                -1,
            )
        };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }

        if descriptors[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            return Ok(ReadOutcome::Cancelled);
        }
        if descriptors[0].revents & libc::POLLNVAL != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "standard input descriptor is invalid",
            ));
        }
        if descriptors[0].revents & (libc::POLLIN | libc::POLLHUP) == 0 {
            if descriptors[0].revents & libc::POLLERR != 0 {
                return Err(io::Error::other("standard input poll failed"));
            }
            continue;
        }

        let mut byte = 0_u8;
        // `poll` reported this descriptor readable. Reading one byte avoids
        // consuming a later pasted answer while preserving terminal line editing.
        let read = unsafe { libc::read(input, (&raw mut byte).cast(), 1) };
        if read < 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                continue;
            }
            return Err(error);
        }
        if read == 0 {
            if bytes.is_empty() {
                return Ok(ReadOutcome::Closed);
            }
            return String::from_utf8(bytes)
                .map(ReadOutcome::Line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        }
        bytes.push(byte);
        if byte == b'\n' {
            return String::from_utf8(bytes)
                .map(ReadOutcome::Line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_wakes_and_joins_the_owned_reader() {
        let (input, _writer) = UnixStream::pair().expect("input pair");
        let mut read = LineRead::start(input.as_raw_fd()).expect("start line read");

        read.cancel_and_join();

        assert!(read.worker.is_none());
    }

    #[tokio::test]
    async fn reads_one_line_without_consuming_the_next() {
        let (input, mut writer) = UnixStream::pair().expect("input pair");
        std::io::Write::write_all(&mut writer, b"first\nsecond\n").expect("write lines");

        let first = LineRead::start(input.as_raw_fd())
            .expect("start first read")
            .wait()
            .await
            .expect("read first line");
        let second = LineRead::start(input.as_raw_fd())
            .expect("start second read")
            .wait()
            .await
            .expect("read second line");

        assert!(matches!(first, ReadOutcome::Line(line) if line == "first\n"));
        assert!(matches!(second, ReadOutcome::Line(line) if line == "second\n"));
    }
}
