//! IO-only adapters for harness lifecycle hooks.

use std::fs::OpenOptions;
use std::io::{self, Read as _, Write as _};
use std::path::Path;

use anyhow::{Context as _, anyhow, bail};
use serde::Deserialize;

use crate::context::AgentContext;

const MAX_INPUT_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
struct ClaudeSessionStart {
    session_id: String,
}

/// Adapt one Claude Code SessionStart event into the environment file Claude
/// sources for later Bash commands.
pub(crate) fn claude_session_start() -> anyhow::Result<()> {
    let input = read_stdin()?;
    let event: ClaudeSessionStart =
        serde_json::from_slice(&input).context("parse Claude SessionStart input")?;
    append_context_from_session(&event.session_id, &std::env::var_os("CLAUDE_ENV_FILE"))
}

fn read_stdin() -> anyhow::Result<Vec<u8>> {
    let mut input = Vec::new();
    io::stdin()
        .lock()
        .take((MAX_INPUT_BYTES as u64) + 1)
        .read_to_end(&mut input)
        .context("read Claude SessionStart input")?;
    if input.len() > MAX_INPUT_BYTES {
        bail!("Claude SessionStart input exceeds {MAX_INPUT_BYTES} bytes");
    }
    Ok(input)
}

fn append_context_from_session(
    session_id: &str,
    env_file: &Option<std::ffi::OsString>,
) -> anyhow::Result<()> {
    if session_id.is_empty() {
        bail!("Claude SessionStart input requires a non-empty session_id");
    }

    let context = format!("claude:{session_id}");
    AgentContext::parse(&context).context("validate Claude session context")?;

    let env_file = env_file
        .as_ref()
        .ok_or_else(|| anyhow!("Claude SessionStart requires CLAUDE_ENV_FILE"))?;
    let env_file = Path::new(env_file);
    if env_file.as_os_str().is_empty() {
        bail!("CLAUDE_ENV_FILE must name an environment file");
    }

    append_export(env_file, &context)
}

fn append_export(path: &Path, context: &str) -> anyhow::Result<()> {
    let mut output = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open CLAUDE_ENV_FILE {}", path.display()))?;

    let line = format!("\nexport ARENA0_CONTEXT={}\n", shell_quote(context));
    output
        .write_all(line.as_bytes())
        .with_context(|| format!("append ARENA0_CONTEXT to {}", path.display()))?;
    output
        .flush()
        .with_context(|| format!("flush CLAUDE_ENV_FILE {}", path.display()))?;
    Ok(())
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_export_preserves_existing_bytes_and_adds_separator_when_needed() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("claude.env");
        std::fs::write(&path, b"existing=1").expect("existing environment");

        append_export(&path, "claude:session").expect("append environment");

        assert_eq!(
            std::fs::read(&path).expect("read environment"),
            b"existing=1\nexport ARENA0_CONTEXT='claude:session'\n"
        );
    }
}
