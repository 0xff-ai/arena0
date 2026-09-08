//! `arena0d`: the local Host ensemble supervisor.

use std::io::IsTerminal;
use std::net::SocketAddr;

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
    let mcp = arena0_daemon::McpConfig::new(SocketAddr::from(([127, 0, 0, 1], 0)), bearer_token)?;
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
    fn internal_mcp_options_are_rejected() {
        for (option, value) in [
            ("--mcp-listen", "127.0.0.1:7330"),
            ("--mcp-access-token-lifetime-secs", "7200"),
        ] {
            let error = Args::try_parse_from(["arena0d", option, value]).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
        }
    }
}
