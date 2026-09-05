//! Bounded diagnostics for child processes owned by the CLI.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::anyhow;
use tokio::io::AsyncReadExt as _;
use tokio::process::ChildStderr;
use tokio::task::JoinHandle;

const READ_CHUNK_BYTES: usize = 1024;

/// Continuously drains child stderr while retaining only its bounded tail.
#[derive(Debug)]
pub(crate) struct StderrCapture {
    tail: Arc<Mutex<StderrTail>>,
    task: Option<JoinHandle<std::io::Result<()>>>,
}

impl StderrCapture {
    pub(crate) fn start(stderr: ChildStderr, limit: usize) -> Self {
        let tail = Arc::new(Mutex::new(StderrTail::new(limit)));
        let task_tail = Arc::clone(&tail);
        let task = tokio::spawn(async move { drain_stderr(stderr, task_tail).await });
        Self {
            tail,
            task: Some(task),
        }
    }

    #[must_use]
    pub(crate) fn tail(&self) -> String {
        self.tail
            .lock()
            .map(|tail| tail.as_string())
            .unwrap_or_else(|_| "<stderr unavailable>".to_owned())
    }

    pub(crate) async fn join(
        &mut self,
        timeout: Duration,
        owner: &'static str,
    ) -> anyhow::Result<()> {
        let Some(mut task) = self.task.take() else {
            return Ok(());
        };
        match tokio::time::timeout(timeout, &mut task).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(anyhow!("drain {owner} stderr: {error}")),
            Ok(Err(error)) => Err(anyhow!("join {owner} stderr task: {error}")),
            Err(_) => {
                task.abort();
                match task.await {
                    Err(error) if error.is_cancelled() => Ok(()),
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(anyhow!("drain {owner} stderr: {error}")),
                    Err(error) => Err(anyhow!("join {owner} stderr task: {error}")),
                }
            }
        }
    }
}

impl Drop for StderrCapture {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Debug)]
struct StderrTail {
    bytes: VecDeque<u8>,
    limit: usize,
    truncated: bool,
}

impl StderrTail {
    fn new(limit: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(limit),
            limit,
            truncated: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if self.bytes.len() == self.limit {
                self.bytes.pop_front();
                self.truncated = true;
            }
            self.bytes.push_back(*byte);
        }
    }

    fn as_string(&self) -> String {
        if self.bytes.is_empty() {
            return String::new();
        }
        let mut value = String::new();
        if self.truncated {
            value.push('…');
        }
        let bytes = self.bytes.iter().copied().collect::<Vec<_>>();
        value.push_str(&String::from_utf8_lossy(&bytes));
        value.trim_end().to_owned()
    }
}

async fn drain_stderr(
    mut stderr: ChildStderr,
    tail: Arc<Mutex<StderrTail>>,
) -> std::io::Result<()> {
    let mut buffer = [0u8; READ_CHUNK_BYTES];
    loop {
        let read = stderr.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        if let Ok(mut tail) = tail.lock() {
            tail.push(&buffer[..read]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_tail_keeps_only_the_bounded_suffix() {
        let mut tail = StderrTail::new(4);
        tail.push(b"abcdef");
        assert_eq!(tail.as_string(), "…cdef");
    }
}
