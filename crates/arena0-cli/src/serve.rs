//! Launch the installed `arena0d` service.
//!
//! The client deliberately does not depend on the daemon crate. This module
//! only selects the sibling daemon executable (or its `PATH` fallback),
//! translates the small product-facing option set, and hands process
//! ownership to the daemon.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Context as _;
use arena0_home::HostName;

#[cfg(windows)]
const DAEMON_BINARY: &str = "arena0d.exe";
#[cfg(not(windows))]
const DAEMON_BINARY: &str = "arena0d";

/// Options for the thin `arena0 serve` launcher.
#[derive(Debug, clap::Args)]
pub(crate) struct ServeArgs {
    /// Host names to pass to arena0d. Repeat or comma-separate the names.
    #[arg(long = "hosts", value_delimiter = ',', value_name = "NAME")]
    pub(crate) hosts: Vec<HostName>,
    /// Loopback address for the daemon's MCP endpoint.
    #[arg(long = "mcp-listen", hide = true, value_name = "ADDR")]
    pub(crate) mcp_listen: Option<SocketAddr>,
    /// Lifetime of per-Host MCP JWTs, in seconds.
    #[arg(
        long = "mcp-access-token-lifetime-secs",
        hide = true,
        value_name = "SECONDS",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub(crate) mcp_access_token_lifetime_secs: Option<u64>,
}

/// Replace this client with the installed daemon and let it own the process.
pub(crate) fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let daemon = daemon_executable()?;
    let daemon_args = daemon_arguments(&args);

    let mut command = Command::new(&daemon);
    command
        .args(&daemon_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        Err::<(), _>(command.exec()).with_context(|| {
            format!(
                "start {}; install or repair the arena0 package if arena0d is missing",
                daemon.display()
            )
        })
    }

    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .with_context(|| format!("start {}", daemon.display()))?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("{} exited with {status}", daemon.display())
        }
    }
}

/// Locate the daemon installed beside this client, falling back to `PATH`.
pub(crate) fn daemon_executable() -> anyhow::Result<PathBuf> {
    let current_exe = std::env::current_exe().context("locate the running arena0 executable")?;
    Ok(resolve_daemon(&current_exe))
}

fn resolve_daemon(current_exe: &Path) -> PathBuf {
    current_exe
        .parent()
        .map(|directory| directory.join(DAEMON_BINARY))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(DAEMON_BINARY))
}

fn daemon_arguments(args: &ServeArgs) -> Vec<OsString> {
    let mut translated = Vec::with_capacity(
        args.hosts.len() * 2
            + usize::from(args.mcp_listen.is_some()) * 2
            + usize::from(args.mcp_access_token_lifetime_secs.is_some()) * 2,
    );
    for host in &args.hosts {
        translated.push(OsString::from("--host"));
        translated.push(OsString::from(host.as_str()));
    }
    if let Some(address) = args.mcp_listen {
        translated.push(OsString::from("--mcp-listen"));
        translated.push(OsString::from(address.to_string()));
    }
    if let Some(lifetime) = args.mcp_access_token_lifetime_secs {
        translated.push(OsString::from("--mcp-access-token-lifetime-secs"));
        translated.push(OsString::from(lifetime.to_string()));
    }
    translated
}
