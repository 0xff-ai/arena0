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
    after_help = "Each Host has its own state directory, identity, and Unix socket.\nThe default starts two Hosts (`host-01` and `host-02`) on one virtual local network.\nAgents connect once to the Streamable HTTP endpoint at /mcp and name a Host in\neach tool call. Set ARENA0_MCP_TOKEN to require one bearer token for the endpoint.\nPress Ctrl-C to stop the process, or run `arena0 stop --host <host>` for a\ngraceful coordinated shutdown of the local ensemble."
)]
struct Args {
    /// Host names to supervise. Repeat for each participant; defaults to two
    /// distinct Hosts so the local bilateral path works immediately.
    #[arg(long = "host", value_name = "NAME")]
    hosts: Vec<HostName>,
    /// Do not mint identities or import the built-in programs on first boot.
    #[arg(long)]
    no_bootstrap: bool,
    /// Loopback address for the MCP Streamable HTTP endpoint.
    #[arg(long, env = "ARENA0_MCP_LISTEN", default_value = "127.0.0.1:7330")]
    mcp_listen: SocketAddr,
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

    let names = if args.hosts.is_empty() {
        (0..2).map(HostName::for_local_index).collect()
    } else {
        args.hosts
    };
    let bearer_token = std::env::var("ARENA0_MCP_TOKEN").ok();
    let mcp = arena0_daemon::McpConfig::new(args.mcp_listen, bearer_token)?;
    arena0_daemon::run(names, !args.no_bootstrap, mcp).await
}
