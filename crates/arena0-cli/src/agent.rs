//! Direct executable-agent driver for the private CLI coordination layer.
//!
//! The process boundary is intentionally small: arena0 sends one JSON callout
//! object per line and the executable sends one JSON value per line.  The
//! driver owns the child, its pipes, and its stderr-reader task.  There is no
//! shell, handshake, provider registry, or detached background task.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::process::StderrCapture;

/// Maximum UTF-8/JSON line size in either direction, including no framing
/// bytes.  Callouts larger than this are a driver boundary error before they
/// reach the child; answers larger than this are rejected while reading.
pub(crate) const MAX_LINE_BYTES: usize = 64 * 1024;

/// Default response bound for one callout.
pub(crate) const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// A short non-blocking observation window used only to detect output already
/// queued after an answer.  The response itself is always governed by the
/// configured timeout.
const EXTRA_OUTPUT_GRACE: Duration = Duration::from_millis(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

const STDERR_TAIL_BYTES: usize = 8 * 1024;
const READ_CHUNK_BYTES: usize = 1024;

/// An executable child that answers one callout at a time.
///
/// Construct this in an async runtime with [`ExecutableAgent::spawn`].  Call
/// [`ExecutableAgent::answer`] for each Host-owned pending callout, then call
/// [`ExecutableAgent::terminate`] after the Host reaches terminal.  Dropping
/// the value starts best-effort child termination and aborts the owned stderr
/// task; asynchronous callers should prefer `terminate` because only it can
/// await reaping.
#[derive(Debug)]
pub(crate) struct ExecutableAgent {
    path: PathBuf,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    stdout_pending: VecDeque<u8>,
    stderr: StderrCapture,
    response_timeout: Duration,
    #[cfg(unix)]
    process_group: i32,
    closed: bool,
}

impl ExecutableAgent {
    /// Spawn `path` directly with piped standard streams.
    ///
    /// `path` is passed to `execve`/the platform process API as-is.  No shell
    /// parses it and no protocol value is interpolated into it.
    pub(crate) fn spawn(
        path: impl Into<PathBuf>,
        response_timeout: Duration,
    ) -> anyhow::Result<Self> {
        if response_timeout.is_zero() {
            bail!("executable-agent response timeout must be greater than zero");
        }
        let path = path.into();
        if path.as_os_str().is_empty() {
            bail!("executable-agent path must not be empty");
        }

        let mut command = Command::new(&path);
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .with_context(|| format!("spawn executable agent {}", path.display()))?;
        #[cfg(unix)]
        let process_group = i32::try_from(
            child
                .id()
                .ok_or_else(|| anyhow!("executable agent has no process id after spawn"))?,
        )
        .context("executable-agent process id exceeds platform range")?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("executable agent stdin was not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("executable agent stdout was not piped"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("executable agent stderr was not piped"))?;

        let stderr = StderrCapture::start(stderr, STDERR_TAIL_BYTES);

        Ok(Self {
            path,
            child,
            stdin: Some(stdin),
            stdout,
            stdout_pending: VecDeque::new(),
            stderr,
            response_timeout,
            #[cfg(unix)]
            process_group,
            closed: false,
        })
    }

    /// Send one callout and return its one JSON answer value.
    ///
    /// The Host remains the authority for schema validation.  This adapter
    /// only frames and parses JSON; it does not interpret the answer schema or
    /// manufacture a program DTO.  A malformed answer, extra output, timeout,
    /// broken pipe, or unexpected child exit permanently closes the driver and
    /// returns an actionable error after killing and reaping the child.
    pub(crate) async fn answer(
        &mut self,
        name: &str,
        prompt: &str,
        context: &Value,
        answer_schema: &Value,
    ) -> anyhow::Result<Value> {
        if self.closed {
            bail!("executable agent {} is closed", self.path.display());
        }
        // An extra line can arrive just after the previous answer was read.
        // Probe before writing the next callout as well as after each answer so
        // that a delayed extra line cannot become the next callout's answer.
        if let Err(error) = self.reject_extra_output().await {
            return Err(self.fail(error).await);
        }
        let line = CalloutLine {
            name,
            prompt,
            context,
            answer_schema,
        };
        let mut encoded = serde_json::to_vec(&line).context("encode executable-agent callout")?;
        if encoded.len() + 1 > MAX_LINE_BYTES {
            return Err(self
                .fail(anyhow!(
                    "executable-agent callout exceeds {MAX_LINE_BYTES} bytes"
                ))
                .await);
        }
        encoded.push(b'\n');

        let result = tokio::time::timeout(self.response_timeout, async {
            let stdin = self
                .stdin
                .as_mut()
                .ok_or_else(|| anyhow!("executable agent stdin is closed"))?;
            stdin
                .write_all(&encoded)
                .await
                .context("write executable-agent callout")?;
            stdin
                .flush()
                .await
                .context("flush executable-agent callout")?;

            let answer = self.read_answer_line().await?;
            let value = serde_json::from_slice::<Value>(&answer)
                .map_err(|_| anyhow!("executable agent returned malformed JSON"))?;
            self.reject_extra_output().await?;
            Ok::<Value, anyhow::Error>(value)
        })
        .await;

        let value = match result {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => return Err(self.fail(error).await),
            Err(_) => {
                return Err(self
                    .fail(anyhow!(
                        "executable agent timed out after {} ms",
                        self.response_timeout.as_millis()
                    ))
                    .await);
            }
        };

        match self.child.try_wait() {
            Ok(None) => Ok(value),
            Ok(Some(_status)) => Err(self
                .fail(anyhow!(
                    "executable agent exited before the run reached terminal"
                ))
                .await),
            Err(error) => Err(self
                .fail(anyhow!("check executable-agent status: {error}"))
                .await),
        }
    }

    /// Close stdin and let a well-behaved executable exit, then reap it.
    pub(crate) async fn finish(&mut self) -> anyhow::Result<()> {
        if self.closed {
            return Ok(());
        }
        if let Err(error) = self.reject_extra_output().await {
            return Err(self.fail(error).await);
        }
        self.stdin.take();
        let status = match tokio::time::timeout(SHUTDOWN_TIMEOUT, self.child.wait()).await {
            Ok(status) => status.context("reap executable agent")?,
            Err(_) => {
                self.kill_owned_processes()
                    .context("kill executable agent after shutdown timeout")?;
                self.child.wait().await.context("reap executable agent")?
            }
        };
        // The direct process may exit successfully while a background child
        // remains in the group. Completion owns and closes that remainder too.
        self.kill_owned_processes()
            .context("stop executable-agent descendants after completion")?;
        self.join_stderr().await?;
        self.closed = true;
        if status.success() {
            Ok(())
        } else {
            bail!("executable agent exited with status {status}")
        }
    }

    /// Kill, reap, and close the executable plus its owned stderr task.
    pub(crate) async fn terminate(&mut self) -> anyhow::Result<()> {
        if self.closed {
            return Ok(());
        }
        // Dropping stdin tells a well-behaved agent that no more callouts will
        // arrive.  We still kill below because terminal cleanup must be bounded.
        self.stdin.take();

        match self.child.try_wait() {
            Ok(Some(_)) => self.kill_owned_processes()?,
            Ok(None) => {
                if let Err(error) = self.kill_owned_processes() {
                    // A concurrent natural exit is safe; otherwise preserve the
                    // cleanup failure for the caller.
                    if self.child.try_wait()?.is_none() {
                        return Err(anyhow!("kill executable agent: {error}"));
                    }
                }
                self.child.wait().await.context("reap executable agent")?;
            }
            Err(error) => return Err(anyhow!("check executable-agent status: {error}")),
        }
        self.join_stderr().await?;
        self.closed = true;
        Ok(())
    }

    #[cfg(unix)]
    fn kill_owned_processes(&mut self) -> std::io::Result<()> {
        // The agent is its process-group leader. A negative pid targets the
        // complete owned group so interpreter and shell descendants stop too.
        let result = unsafe { libc::kill(-self.process_group, libc::SIGKILL) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }

    #[cfg(not(unix))]
    fn kill_owned_processes(&mut self) -> std::io::Result<()> {
        self.child.start_kill()
    }

    /// Return the retained stderr tail for failure diagnostics only.
    #[must_use]
    pub(crate) fn stderr_tail(&self) -> String {
        self.stderr.tail()
    }

    /// Whether this child has been closed by terminal cleanup or a driver
    /// failure.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn is_closed(&self) -> bool {
        self.closed
    }

    async fn fail(&mut self, error: anyhow::Error) -> anyhow::Error {
        let cleanup = self.terminate().await.err();
        let stderr = self.stderr_tail();
        let mut message = error.to_string();
        // A child can close stdin between the stdout probe and the write.
        // Keep its final status in that diagnostic too, after cleanup has
        // reaped it, rather than reporting only the failed pipe operation.
        match self.child.try_wait() {
            Ok(Some(status)) => message.push_str(&format!("; final process status {status}")),
            Ok(None) => {}
            Err(error) => message.push_str(&format!("; read final process status: {error}")),
        }
        if let Some(cleanup) = cleanup {
            message.push_str(&format!("; cleanup failed: {cleanup}"));
        }
        if !stderr.is_empty() {
            message.push_str("; agent stderr tail: ");
            message.push_str(&stderr);
        }
        anyhow!(message)
    }

    async fn read_answer_line(&mut self) -> anyhow::Result<Vec<u8>> {
        loop {
            if let Some(newline) = self.stdout_pending.iter().position(|byte| *byte == b'\n') {
                let mut line = Vec::with_capacity(newline);
                for _ in 0..=newline {
                    // The position came from the queue, so this cannot be
                    // absent unless another owner mutates the private queue.
                    line.push(
                        self.stdout_pending
                            .pop_front()
                            .ok_or_else(|| anyhow!("executable-agent output buffer changed"))?,
                    );
                }
                if line.len() > MAX_LINE_BYTES {
                    bail!("executable-agent answer exceeds {MAX_LINE_BYTES} bytes");
                }
                line.pop(); // JSONL newline; serde accepts any trailing JSON whitespace.
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(line);
            }
            if self.stdout_pending.len() >= MAX_LINE_BYTES {
                bail!("executable-agent answer exceeds {MAX_LINE_BYTES} bytes");
            }

            let mut chunk = [0u8; READ_CHUNK_BYTES];
            let read = self
                .stdout
                .read(&mut chunk)
                .await
                .context("read executable-agent answer")?;
            if read == 0 {
                let status = self
                    .child
                    .wait()
                    .await
                    .context("reap executable agent after stdout closed")?;
                bail!("executable agent exited with status {status} before answering");
            }
            self.stdout_pending.extend(&chunk[..read]);
        }
    }

    async fn reject_extra_output(&mut self) -> anyhow::Result<()> {
        if !self.stdout_pending.is_empty() {
            bail!("executable agent produced extra stdout before the next callout");
        }

        let mut probe = [0u8; READ_CHUNK_BYTES];
        match tokio::time::timeout(EXTRA_OUTPUT_GRACE, self.stdout.read(&mut probe)).await {
            Ok(Ok(0)) => {
                let status = self
                    .child
                    .wait()
                    .await
                    .context("reap executable agent after stdout closed")?;
                bail!("executable agent exited with status {status}")
            }
            Ok(Ok(read)) => {
                self.stdout_pending.extend(&probe[..read]);
                bail!("executable agent produced extra stdout for one callout")
            }
            Ok(Err(error)) => Err(anyhow!("read executable-agent extra output: {error}")),
            Err(_) => Ok(()),
        }
    }

    async fn join_stderr(&mut self) -> anyhow::Result<()> {
        self.stderr.join(SHUTDOWN_TIMEOUT, "executable-agent").await
    }
}

impl Drop for ExecutableAgent {
    fn drop(&mut self) {
        // Drop cannot await.  Start the kill and abort only the task this value
        // owns; callers that need a reaped child must call terminate().
        if !self.closed {
            let _ = self.kill_owned_processes();
            self.closed = true;
        }
    }
}

#[derive(Debug, Serialize)]
struct CalloutLine<'a> {
    name: &'a str,
    prompt: &'a str,
    context: &'a Value,
    answer_schema: &'a Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(unix)]
    fn script(body: &str) -> (tempfile::TempDir, PathBuf) {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("temporary agent directory");
        let path = directory.path().join("agent");
        let mut file = std::fs::File::create(&path).expect("create agent script");
        file.write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
            .expect("write agent script");
        file.sync_all().expect("sync agent script");
        drop(file);
        let mut permissions = std::fs::metadata(&path)
            .expect("agent metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("make agent executable");
        (directory, path)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn direct_cat_round_trips_one_json_answer() {
        let mut agent =
            ExecutableAgent::spawn("/bin/cat", Duration::from_secs(1)).expect("spawn cat");
        let answer = agent
            .answer(
                "choose",
                "Choose one",
                &json!({"round": 1}),
                &json!({"enum": ["a"]}),
            )
            .await
            .expect("cat answer");
        assert_eq!(
            answer,
            json!({
                "name": "choose",
                "prompt": "Choose one",
                "context": {"round": 1},
                "answer_schema": {"enum": ["a"]}
            })
        );
        agent.terminate().await.expect("reap cat");
        assert!(agent.is_closed());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oversized_callout_is_rejected_before_write() {
        let mut agent =
            ExecutableAgent::spawn("/bin/cat", Duration::from_secs(1)).expect("spawn cat");
        let context = Value::String("x".repeat(MAX_LINE_BYTES));
        let error = agent
            .answer("choose", "Choose", &context, &json!({"type": "string"}))
            .await
            .expect_err("oversized callout must fail");
        assert!(error.to_string().contains("callout exceeds 65536 bytes"));
        assert!(agent.is_closed());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_without_an_answer_fails_cleanly() {
        let mut agent =
            ExecutableAgent::spawn("/bin/false", Duration::from_secs(1)).expect("spawn false");
        let error = agent
            .answer(
                "choose",
                "Choose one",
                &Value::Null,
                &json!({"enum": ["a"]}),
            )
            .await
            .expect_err("false cannot answer");
        assert!(error.to_string().contains("status"), "{error:#}");
        assert!(agent.is_closed());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn malformed_json_kills_and_reaps_the_agent() {
        let (_directory, path) = script("read line\nprintf 'not-json\\n'");
        let mut agent = ExecutableAgent::spawn(path, Duration::from_secs(1)).expect("spawn agent");
        let error = agent
            .answer("choose", "Choose", &Value::Null, &json!({"type": "string"}))
            .await
            .expect_err("malformed JSON must fail");
        assert!(error.to_string().contains("malformed JSON"));
        assert!(agent.is_closed());
        assert!(agent.child.try_wait().expect("child status").is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn extra_answer_line_is_rejected() {
        let (_directory, path) = script("read line\nprintf '1\\n2\\n'");
        let mut agent = ExecutableAgent::spawn(path, Duration::from_secs(1)).expect("spawn agent");
        let error = agent
            .answer("choose", "Choose", &Value::Null, &json!({"type": "number"}))
            .await
            .expect_err("extra output must fail");
        assert!(error.to_string().contains("extra stdout"));
        assert!(agent.is_closed());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oversized_answer_is_rejected_before_unbounded_growth() {
        let (_directory, path) =
            script("read line\nhead -c 70000 /dev/zero | tr '\\000' x\nprintf '\\n'");
        let mut agent = ExecutableAgent::spawn(path, Duration::from_secs(1)).expect("spawn agent");
        let error = agent
            .answer("choose", "Choose", &Value::Null, &json!({"type": "string"}))
            .await
            .expect_err("oversized answer must fail");
        assert!(error.to_string().contains("exceeds 65536 bytes"));
        assert!(agent.is_closed());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invalid_utf8_answer_is_rejected() {
        let (_directory, path) = script("read line\nprintf '\\377\\n'");
        let mut agent = ExecutableAgent::spawn(path, Duration::from_secs(1)).expect("spawn agent");
        let error = agent
            .answer("choose", "Choose", &Value::Null, &json!({"type": "string"}))
            .await
            .expect_err("invalid UTF-8 must fail");
        assert!(error.to_string().contains("malformed JSON"));
        assert!(agent.is_closed());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn response_timeout_stops_agent_descendants() {
        let directory = tempfile::tempdir().expect("temporary marker directory");
        let marker = directory.path().join("descendant-survived");
        let body = format!(
            "read line\n(sleep 0.2; printf survived > '{}') &\nsleep 10",
            marker.display()
        );
        let (_script_directory, path) = script(&body);
        let mut agent =
            ExecutableAgent::spawn(path, Duration::from_millis(20)).expect("spawn agent");

        let error = agent
            .answer("choose", "Choose", &Value::Null, &json!({"type": "number"}))
            .await
            .expect_err("silent process tree must time out");
        assert!(error.to_string().contains("timed out"));
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(!marker.exists(), "agent descendant survived group cleanup");
        assert!(agent.is_closed());
        assert!(agent.child.try_wait().expect("child status").is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_completion_stops_agent_descendants() {
        let directory = tempfile::tempdir().expect("temporary marker directory");
        let marker = directory.path().join("descendant-survived");
        let body = format!(
            "(sleep 0.2; printf survived > '{}') &\nwhile read line; do printf '0\\n'; done",
            marker.display()
        );
        let (_script_directory, path) = script(&body);
        let mut agent = ExecutableAgent::spawn(path, Duration::from_secs(1)).expect("spawn agent");

        assert_eq!(
            agent
                .answer("choose", "Choose", &Value::Null, &json!({"type": "number"}))
                .await
                .expect("agent answer"),
            json!(0)
        );
        agent.finish().await.expect("finish process group");
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(!marker.exists(), "agent descendant survived completion");
        assert!(agent.is_closed());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stderr_flood_is_drained_and_capped() {
        let (_directory, path) = script(
            "while read line; do\n  head -c 131072 /dev/zero >&2\n  printf '\"Cooperate\"\\n'\ndone",
        );
        let mut agent = ExecutableAgent::spawn(path, Duration::from_secs(2)).expect("spawn agent");
        let answer = agent
            .answer(
                "choose",
                "Choose",
                &Value::Null,
                &json!({"enum": ["Cooperate", "Defect"]}),
            )
            .await
            .expect("stderr must not block the answer");
        assert_eq!(answer, json!("Cooperate"));
        agent.finish().await.expect("finish agent");
        assert!(agent.stderr_tail().len() <= STDERR_TAIL_BYTES + '…'.len_utf8());
    }
}
