//! `arena0d`: the local Host ensemble supervisor.

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::time::Duration;

use arena0_home::HostName;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "arena0d",
    about = "Serve one or more local arena0 Hosts",
    version,
    after_help = "Each Host has its own state directory and identity; the daemon owns one shared Unix socket.\nThe default starts two Hosts (`host-01` and `host-02`) on one virtual local network.\nAgents use the local CLI and bind their participant with `arena0 hello`.\nPress Ctrl-C to stop the process, or run `arena0 stop` for a\ngraceful coordinated shutdown of the local ensemble."
)]
struct Args {
    /// Host names to supervise. Repeat for each participant; defaults to two
    /// distinct Hosts so the local bilateral path works immediately.
    #[arg(long = "host", value_name = "NAME", conflicts_with = "no_hosts")]
    hosts: Vec<HostName>,
    /// Start with no Hosts; clients can open Hosts on demand.
    #[arg(long, conflicts_with = "hosts")]
    no_hosts: bool,
    /// Do not mint identities or import the built-in programs on first boot.
    #[arg(long)]
    no_bootstrap: bool,
    /// Loopback address for the MCP Streamable HTTP endpoint.
    #[arg(
        long,
        hide = true,
        env = "ARENA0_MCP_LISTEN",
        default_value = "127.0.0.1:7330"
    )]
    mcp_listen: SocketAddr,
    /// Lifetime of per-Host MCP JWTs, in seconds.
    #[arg(
        long = "mcp-access-token-lifetime-secs",
        hide = true,
        env = "ARENA0_MCP_ACCESS_TOKEN_LIFETIME_SECS",
        default_value_t = 86_400,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    mcp_access_token_lifetime_secs: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "arena0_daemon=info,arena0::startup=info,warn".into()),
        )
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    let names = if args.no_hosts {
        Vec::new()
    } else if args.hosts.is_empty() {
        (0..2).map(HostName::for_local_index).collect()
    } else {
        args.hosts
    };
    let bearer_token = std::env::var("ARENA0_MCP_TOKEN").ok();
    let mcp = arena0_daemon::McpConfig::with_access_token_lifetime(
        args.mcp_listen,
        bearer_token,
        Duration::from_secs(args.mcp_access_token_lifetime_secs),
    )?;
    arena0_daemon::run(names, !args.no_bootstrap, mcp).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    #[test]
    fn help_does_not_advertise_the_internal_adapter() {
        let help = Args::command().render_long_help().to_string();
        assert!(help.contains("arena0 hello"));
        assert!(!help.to_ascii_lowercase().contains("mcp"));
    }

    #[test]
    fn no_hosts_cannot_be_combined_with_named_hosts() {
        assert!(Args::try_parse_from(["arena0d", "--no-hosts", "--host", "host-01"]).is_err());
    }

    #[test]
    fn token_lifetime_accepts_positive_seconds_and_rejects_zero() {
        let args = Args::try_parse_from(["arena0d", "--mcp-access-token-lifetime-secs", "7200"])
            .expect("parse token lifetime");
        assert_eq!(args.mcp_access_token_lifetime_secs, 7200);
        assert!(
            Args::try_parse_from(["arena0d", "--mcp-access-token-lifetime-secs", "0"]).is_err()
        );
    }
}
