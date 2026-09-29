//! `arena0 ui`: open the web UI the daemon serves.

use anyhow::{Context as _, bail};
use arena0_client::api::{Request, ResponseOk};

use crate::Ctx;
use crate::local_daemon::{self, LocalDaemon};

#[derive(Debug, Default, clap::Args)]
pub(crate) struct UiArgs {
    /// Borrow a running daemon; never start one.
    #[arg(long)]
    pub(crate) attach: bool,
    /// Print the URL without opening a browser.
    #[arg(long)]
    no_open: bool,
}

/// Print the daemon's UI URL and open it, then wait for Ctrl-C. Without
/// `--attach` the command starts the daemon when none runs, and stops it
/// again on Ctrl-C only if it started it.
pub(crate) async fn run(ctx: &Ctx, args: UiArgs) -> anyhow::Result<()> {
    let daemon = if args.attach {
        None
    } else {
        Some(LocalDaemon::connect_or_start(local_daemon::host_names(2)).await?)
    };
    let info = ctx.client().call(&Request::DaemonInfo).await;
    let info = match info {
        Ok(ResponseOk::DaemonInfo(info)) => info,
        other => {
            if let Some(daemon) = daemon {
                daemon.shutdown().await?;
            }
            match other {
                Err(error) => return Err(error),
                Ok(other) => bail!("unexpected daemon.info response: {other:?}"),
            }
        }
    };
    let url = format!("{}/", info.http_url);
    if ctx.mode.is_json() {
        println!("{}", serde_json::json!({ "url": url }));
    } else {
        println!("arena0 ui  {url}");
    }
    if !args.no_open
        && let Err(error) = open_browser(&url)
    {
        eprintln!("could not open a browser ({error}); open the URL above yourself");
    }

    let interrupted = tokio::signal::ctrl_c().await;
    let daemon_stopped = match daemon {
        Some(daemon) => daemon.shutdown().await,
        None => Ok(()),
    };
    interrupted.context("listen for Ctrl-C")?;
    daemon_stopped
}

fn open_browser(url: &str) -> std::io::Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
}
