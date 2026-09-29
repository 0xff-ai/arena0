//! `arena0 ui`: serve the browser workspace and bridge it to the daemon.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context as _;
use arena0_client::proto::DaemonClient;
use arena0_home::HostName;
use arena0_web::protocol::{
    ErrorCode, ErrorRow, ExecRef, LaunchArgs, LaunchReply, SeatDriver, StrategyRow,
};
use arena0_web::{Launcher, UiConfig, UiServer};
use tokio::sync::oneshot;

use crate::Ctx;
use crate::coordinated::{self, CoordinatedRunArgs, DriverBinding, DriverSpec};
use crate::local_daemon::{self, LocalDaemon};
use crate::progress::RunProgress;

/// Longest run error shown to the browser.
const MAX_ERROR_CHARS: usize = 300;

#[derive(Debug, Default, clap::Args)]
pub(crate) struct UiArgs {
    /// Borrow a running daemon; never start one.
    #[arg(long)]
    pub(crate) attach: bool,
    /// Loopback port for the page (0 picks a free port).
    #[arg(long, default_value_t = 0)]
    port: u16,
    /// Print the URL without opening a browser.
    #[arg(long)]
    no_open: bool,
    /// Allow a development server origin (e.g. http://127.0.0.1:5173).
    #[arg(long, hide = true)]
    dev_origin: Option<String>,
}

/// Serve the page until Ctrl-C. Without `--attach` the command starts the
/// daemon when none runs, and stops it again only if it started it.
pub(crate) async fn run(ctx: &Ctx, args: UiArgs) -> anyhow::Result<()> {
    let socket = ctx.client().socket().to_path_buf();
    let daemon = if args.attach {
        None
    } else {
        Some(LocalDaemon::connect_or_start(local_daemon::host_names(2)).await?)
    };
    let started = UiServer::start(
        UiConfig {
            socket: socket.clone(),
            port: args.port,
            dev_origin: args.dev_origin,
            attached: args.attach,
        },
        Arc::new(CliLauncher { socket }),
    )
    .await;
    let server = match started {
        Ok(server) => server,
        Err(error) => {
            if let Some(daemon) = daemon {
                let _ = daemon.shutdown().await;
            }
            return Err(error);
        }
    };

    let url = server.url();
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
    let stopped = server.shutdown().await;
    let daemon_stopped = match daemon {
        Some(daemon) => daemon.shutdown().await,
        None => Ok(()),
    };
    interrupted.context("listen for Ctrl-C")?;
    stopped?;
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

/// Starts sessions for the browser through the coordinated run.
pub(crate) struct CliLauncher {
    /// The daemon socket the gateway bridges.
    socket: PathBuf,
}

fn error(code: ErrorCode, message: impl Into<String>) -> ErrorRow {
    ErrorRow {
        code,
        message: message.into(),
    }
}

impl Launcher for CliLauncher {
    fn strategies(&self) -> Vec<StrategyRow> {
        [
            (
                "first-allowed",
                "Answers with the first value the callout's schema allows",
            ),
            (
                "sample",
                "Plays the bundled example policy for the program, else the first allowed value",
            ),
        ]
        .into_iter()
        .map(|(name, description)| StrategyRow {
            name: name.to_owned(),
            description: description.to_owned(),
        })
        .collect()
    }

    fn launch(
        &self,
        args: LaunchArgs,
    ) -> Pin<Box<dyn Future<Output = Result<LaunchReply, ErrorRow>> + Send>> {
        let socket = self.socket.clone();
        Box::pin(launch(socket, args))
    }
}

async fn launch(socket: PathBuf, args: LaunchArgs) -> Result<LaunchReply, ErrorRow> {
    // The coordinated run always talks to the environment's daemon.
    let home_socket = DaemonClient::from_env()
        .map_err(|_| error(ErrorCode::Gateway, "cannot resolve the arena0 home"))?;
    if home_socket.socket() != socket {
        return Err(error(
            ErrorCode::Gateway,
            "launching needs the daemon at the home socket; restart without --socket",
        ));
    }
    if coordinated::wasm_reference(&args.program).is_some() {
        return Err(error(
            ErrorCode::BadRequest,
            "the program must be a catalog name or id",
        ));
    }
    let bindings = args
        .seats
        .into_iter()
        .map(|seat| {
            let host = seat
                .host
                .parse::<HostName>()
                .map_err(|_| error(ErrorCode::BadRequest, "invalid Host name"))?;
            let driver = match seat.driver {
                SeatDriver::You | SeatDriver::External => DriverSpec::External,
                SeatDriver::Builtin { strategy } => DriverSpec::Builtin(strategy),
                SeatDriver::Executable { path } => DriverSpec::Executable(PathBuf::from(path)),
            };
            Ok(DriverBinding::new(host, driver))
        })
        .collect::<Result<Vec<_>, ErrorRow>>()?;

    let (created, seats) = oneshot::channel();
    let mut run = tokio::spawn(coordinated::run(
        CoordinatedRunArgs {
            program: args.program,
            params: args.params,
            bindings,
            created: Some(created),
        },
        RunProgress::hidden(),
    ));
    tokio::select! {
        seats = seats => match seats {
            Ok(seats) => {
                // The run keeps driving its seats in the background until the
                // session ends; the replica shows how it ended. The failure's
                // chain can quote a seat's answer, so it is not logged.
                tokio::spawn(async move {
                    match run.await {
                        Ok(Ok(_)) => {}
                        Ok(Err(_)) => tracing::warn!("launched session ended with an error"),
                        Err(_) => tracing::warn!("launched session task failed"),
                    }
                });
                Ok(LaunchReply {
                    execs: seats
                        .into_iter()
                        .map(|(host, exec_id)| ExecRef {
                            host: host.to_string(),
                            exec_id: exec_id.to_string(),
                        })
                        .collect(),
                })
            }
            Err(_) => Err(run_failure(run.await)),
        },
        finished = &mut run => Err(run_failure(finished)),
    }
}

/// The error of a run that ended before every seat had an execution.
fn run_failure(
    finished: Result<anyhow::Result<coordinated::AggregateResult>, tokio::task::JoinError>,
) -> ErrorRow {
    let message = match finished {
        Ok(Err(failure)) => format!("{failure:#}"),
        Ok(Ok(_)) => "the run ended before creating its executions".to_owned(),
        Err(_) => "the run task failed".to_owned(),
    };
    error(
        ErrorCode::Gateway,
        message.chars().take(MAX_ERROR_CHARS).collect::<String>(),
    )
}
