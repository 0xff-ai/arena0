//! The public `arena0` client.
//!
//! This binary is deliberately a local client: it sends typed requests to one
//! Host socket and renders the response. Persistent and command-scoped Host
//! supervision both execute `arena0d`; offline proof verification lives in
//! `arena0-verify`.

mod agent;
mod coordinated;
mod line_input;
mod local_daemon;
mod process;
mod progress;
mod run;
mod serve;
mod terminal;
mod tui;
mod ui;
mod verify;
mod watch;
mod workspace;

use std::fmt::Write as _;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, anyhow, bail};
use arena0_client::answer;
use arena0_client::api::{
    ApiErrorCode, AwaitState, EnsembleSpec, ExecStatus, IdRef, NextEvent, PendingId, ReceiptKey,
    ReceiptRef, Request, ResponseOk, VerifiedResult,
};
use arena0_client::proto::DaemonClient;
use arena0_client::protocol::{ExecId, PeerId, ReceiptTermination, SessionHash, View, Viewport};
use arena0_home::HostName;
use clap::{CommandFactory, Parser, Subcommand};
use serde_json::{Value, json};

use ui::{Mode, Palette};

/// The process-local presentation and socket choice shared by subcommands.
#[derive(Debug)]
pub(crate) struct Ctx {
    pub(crate) client: DaemonClient,
    pub(crate) mode: Mode,
    pub(crate) palette: Palette,
}

impl Ctx {
    pub(crate) fn client(&self) -> &DaemonClient {
        &self.client
    }

    #[must_use]
    pub(crate) fn viewport(&self) -> Viewport {
        Viewport {
            width: ui::terminal_width(),
            color: self.palette.color_depth(),
        }
    }

    pub(crate) async fn fetch_exec_view(
        &self,
        exec_id: ExecId,
    ) -> anyhow::Result<Option<(u64, View)>> {
        let viewport = self.viewport();
        match self
            .client()
            .call_raw(&Request::ExecView {
                exec: exec_id,
                width: viewport.width,
                color: viewport.color,
            })
            .await?
        {
            Ok(ResponseOk::ExecView { step, view }) => Ok(Some((step, view))),
            Ok(other) => bail!("unexpected response to exec.view: {other:?}"),
            Err(error) if error.code == ApiErrorCode::Execution => Ok(None),
            Err(error) => bail!("{error}"),
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "arena0",
    about = "Run local Hosts, inspect executions, and verify receipts",
    after_help = "Examples:\n  arena0\n  arena0 run rock-paper-scissors --human host-01 --builtin host-02=sample\n  arena0 serve\n  arena0 status\n  arena0 verify receipt.json\n\nOn a terminal, bare `arena0` opens the local program workspace. `arena0 run` starts command-scoped local Hosts when needed; `arena0 serve` keeps them running for API and MCP clients.",
    version
)]
struct Cli {
    /// Socket served by one Host (default: ARENA0_SOCKET or ARENA0_HOME/hosts/host-01/arena0.sock).
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Select a named Host below ARENA0_HOME/hosts/.
    #[arg(long, global = true)]
    host: Option<HostName>,
    /// Emit machine-readable JSON on stdout.
    #[arg(long, global = true)]
    json: bool,
    /// Use a private temporary Host home while retaining the global program cache.
    #[arg(long, global = true, conflicts_with = "socket")]
    tmp: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the persistent local Host service.
    Serve(serve::ServeArgs),
    /// Show the selected Host and active executions.
    Status,
    /// Ask the selected Host (and its local ensemble supervisor) to stop.
    Stop,
    /// Identity custody operations handled by the Host.
    Identity {
        #[command(subcommand)]
        command: IdentityCommand,
    },
    /// Manage the selected Host's local program catalog.
    Program {
        #[command(subcommand)]
        command: ProgramCommand,
    },
    /// Create and drive executions on the selected Host.
    Exec {
        #[command(subcommand)]
        command: ExecCommand,
    },
    /// Follow local Host events until an execution ends or Ctrl-C is pressed.
    Watch {
        /// An execution id or prefix. Omit to follow all local Host events.
        exec: Option<String>,
    },
    /// Fetch, import, list, or verify Host-resident receipts.
    Receipt {
        #[command(subcommand)]
        command: ReceiptCommand,
    },
    /// Verify a receipt file offline, or a receipt held by the selected Host.
    Verify {
        /// A receipt JSON path, receipt id, or session id.
        target: String,
        /// Request full replay from a running Host.
        #[arg(long)]
        replay: bool,
        /// Verify every producer through these named local Hosts.
        #[arg(long, value_delimiter = ',', value_name = "HOSTS")]
        hosts: Vec<HostName>,
    },
    /// Coordinate and verify one program across local Hosts.
    Run {
        /// Program name, id prefix, full content id, or Wasm path.
        program: String,
        /// Bind an interactive human driver to this Host (repeatable in the TUI).
        #[arg(long, value_name = "HOST")]
        human: Vec<HostName>,
        /// Bind a deterministic built-in strategy as HOST=STRATEGY.
        #[arg(long, value_name = "HOST=STRATEGY")]
        builtin: Vec<String>,
        /// Bind an executable JSONL agent as HOST=EXECUTABLE.
        #[arg(long, value_name = "HOST=EXECUTABLE")]
        agent: Vec<String>,
        /// Program params as KEY=VALUE (repeatable).
        #[arg(long, value_name = "KEY=VALUE")]
        param: Vec<String>,
        /// Fully replay every producer receipt before succeeding.
        #[arg(long)]
        replay: bool,
        /// Use inline terminal output instead of the focused TUI.
        #[arg(long)]
        no_tui: bool,
    },
}

#[derive(Debug, Subcommand)]
enum IdentityCommand {
    /// Mint a new identity, optionally with a label.
    New {
        #[arg(value_name = "LABEL")]
        label: Option<String>,
    },
    /// List identities in this Host's keystore.
    List,
    /// Show one identity by peer id or label.
    Show { id: String },
    /// Remove one identity by peer id or label.
    Remove { id: String },
}

#[derive(Debug, Subcommand)]
enum ProgramCommand {
    /// List locally registered programs.
    List,
    /// Show a program summary and public JSON Schema.
    Show { program: String },
    /// Import a Wasm program into the local catalog.
    Import { file: PathBuf },
    /// Remove a local program by name or id.
    Remove { program: String },
}

#[derive(Debug, Subcommand)]
enum ExecCommand {
    /// Create an execution; negotiation continues in the Host.
    Create {
        program: String,
        #[arg(long, value_delimiter = ',')]
        with: Vec<String>,
        #[arg(long, value_names = ["CREATOR", "NEGOTIATION_ID"], num_args = 2)]
        join: Option<Vec<String>>,
        #[arg(long, value_name = "KEY=VALUE")]
        param: Vec<String>,
    },
    /// List executions known by the Host.
    List,
    /// Show one execution's current state.
    Status { exec_id: String },
    /// Wait until an execution is active or terminal.
    Await {
        exec_id: String,
        #[arg(long, default_value = "active")]
        until: String,
    },
    /// Prompt for callouts and drive an existing execution to its terminal.
    Drive { exec_id: String },
    /// Return the next callout or terminal event.
    Next { exec_id: String },
    /// Submit a JSON answer to a pending callout.
    Submit {
        exec_id: String,
        #[arg(long)]
        pending_id: PendingId,
        #[arg(long)]
        answer: Option<String>,
    },
    /// Execute a read-only JSON query.
    Query { exec_id: String, input: String },
    /// Render the program-authored view.
    View { exec_id: String },
    /// Read the durable trace.
    Trace {
        exec_id: String,
        #[arg(long, default_value_t = 0)]
        from: u64,
        #[arg(long, default_value_t = u64::MAX)]
        to: u64,
    },
    /// Withdraw a revocable negotiation ticket.
    Withdraw { exec_id: String },
    /// Terminate an execution with an operator reason.
    Terminate {
        exec_id: String,
        #[arg(long, default_value = "operator terminated")]
        reason: String,
    },
}

#[derive(Debug, Subcommand)]
enum ReceiptCommand {
    /// Fetch one receipt; use --out to write the portable JSON artifact.
    Get {
        session: String,
        #[arg(long)]
        producer: Option<String>,
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
    },
    /// Import a portable receipt into the Host's store.
    Import { file: PathBuf },
    /// List receipts held by this Host.
    List,
    /// Verify a Host-resident receipt.
    Verify {
        session: String,
        #[arg(long)]
        producer: Option<String>,
        #[arg(long)]
        replay: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let _temporary_home = match prepare_temporary_home(cli.tmp) {
        Ok(home) => home,
        Err(error) => {
            eprintln!("error: {error:#}");
            return ExitCode::FAILURE;
        }
    };
    init_tracing();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: initialize async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = runtime.block_on(run(cli)) {
        let message = if arena0_client::proto::is_connect_error(&error) {
            format!(
                "Host not reachable; run `arena0 serve` for a persistent local service; {error}"
            )
        } else {
            format!("{error:#}")
        };
        eprintln!("error: {message}");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn prepare_temporary_home(enabled: bool) -> anyhow::Result<Option<tempfile::TempDir>> {
    if !enabled {
        return Ok(None);
    }
    let stable_home = arena0_home::Home::from_env().context("resolve global arena0 home")?;
    let temporary_dir = stable_home.temporary_dir();
    std::fs::create_dir_all(&temporary_dir)
        .with_context(|| format!("create temporary home parent {}", temporary_dir.display()))?;
    let directory = tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(&temporary_dir)
        .context("create temporary arena0 home")?;
    // SAFETY: `main` calls this before creating the Tokio runtime, initializing
    // tracing, or starting any application-owned thread. No other thread can
    // concurrently read or mutate the process environment.
    unsafe {
        std::env::set_var("ARENA0_HOME", directory.path());
        std::env::set_var("ARENA0_CACHE_DIR", stable_home.cache_dir());
        std::env::remove_var("ARENA0_SOCKET");
    }
    Ok(Some(directory))
}

fn init_tracing() {
    if std::env::var_os("RUST_LOG").is_none() {
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .compact()
        .try_init();
}

fn should_use_tui(
    has_human: bool,
    no_tui: bool,
    mode: Mode,
    streams_are_terminal: bool,
    tracing_enabled: bool,
) -> bool {
    has_human && !no_tui && !mode.is_json() && streams_are_terminal && !tracing_enabled
}

async fn interactive_workspace() -> anyhow::Result<()> {
    let initial_hosts = local_daemon::host_names(2);
    eprintln!("preparing {} local Hosts", initial_hosts.len());
    let daemon = local_daemon::LocalDaemon::connect_or_start(initial_hosts).await?;
    let client = DaemonClient::for_host(
        daemon
            .hosts()
            .first()
            .expect("local daemon always owns at least one Host"),
    );
    let client = match client {
        Ok(client) => client,
        Err(error) => return finish_with_daemon(Err(error.into()), daemon).await,
    };
    let loaded = workspace::load(&client).await;
    let (programs, executions) = match loaded {
        Ok(loaded) => loaded,
        Err(error) => return finish_with_daemon(Err(error), daemon).await,
    };
    let selection = workspace::choose(
        programs,
        executions,
        daemon.hosts().len(),
        daemon.is_spawned(),
    )
    .await;
    let selection = match selection {
        Ok(selection) => selection,
        Err(error) => return finish_with_daemon(Err(error), daemon).await,
    };
    let workspace::Exit::Launch(launch) = selection else {
        return finish_with_daemon(Ok(()), daemon).await;
    };

    let daemon = if launch.participants == daemon.hosts().len() {
        daemon
    } else {
        if !daemon.is_spawned() {
            return finish_with_daemon(
                Err(anyhow!(
                    "the persistent local service owns {} Hosts and cannot be resized by this command",
                    daemon.hosts().len()
                )),
                daemon,
            )
            .await;
        }
        daemon.shutdown().await?;
        let hosts = local_daemon::host_names(launch.participants);
        eprintln!("preparing {} local Hosts", hosts.len());
        local_daemon::LocalDaemon::connect_or_start(hosts).await?
    };

    let human_control = launch.human_control;
    let bindings = daemon
        .hosts()
        .iter()
        .cloned()
        .map(|host| {
            let driver = match &human_control {
                workspace::HumanControl::AllHosts => coordinated::DriverSpec::Human,
                workspace::HumanControl::OneHost { host: human } if human == &host => {
                    coordinated::DriverSpec::Human
                }
                workspace::HumanControl::OneHost { .. } => {
                    coordinated::DriverSpec::Builtin("sample".to_owned())
                }
            };
            coordinated::DriverBinding::new(host, driver)
        })
        .collect();
    let result = run_with_connected_bindings(
        Mode::Human,
        launch.program,
        launch.params,
        bindings,
        launch.replay,
        false,
    )
    .await;
    finish_with_daemon(result, daemon).await
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let Cli {
        socket,
        host,
        json,
        tmp,
        command,
    } = cli;
    let mode = if json { Mode::Json } else { Mode::Human };
    if command.is_none()
        && matches!(mode, Mode::Human)
        && socket.is_none()
        && host.is_none()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
    {
        return interactive_workspace().await;
    }
    if command.is_none() {
        let mut command = Cli::command();
        command.print_help().context("render help")?;
        println!();
        return Ok(());
    }

    let command = match command.expect("checked above") {
        Command::Serve(args) => {
            if tmp {
                bail!(
                    "--tmp does not apply to `arena0 serve`; temporary homes require a command-owned daemon"
                );
            }
            if socket.is_some() || host.is_some() || json {
                bail!("--socket, --host, and --json do not apply to `arena0 serve`");
            }
            return serve::serve(args);
        }
        Command::Run {
            program,
            human,
            builtin,
            agent,
            param,
            replay,
            no_tui,
        } => {
            if socket.is_some() || host.is_some() {
                bail!("--socket and --host do not apply to coordinated `arena0 run`");
            }
            return coordinated_run(
                mode,
                CoordinatedCliArgs {
                    program,
                    humans: human,
                    builtins: builtin,
                    agents: agent,
                    params: param,
                    replay,
                    no_tui,
                },
            )
            .await;
        }
        command => command,
    };

    if let Command::Verify {
        ref target,
        replay,
        ref hosts,
    } = command
        && !hosts.is_empty()
    {
        if socket.is_some() || host.is_some() {
            bail!("--socket and --host do not apply when `arena0 verify` uses --hosts");
        }
        if verify::is_path_target(Path::new(target), target) {
            bail!("--hosts verifies Host-resident session receipts, not a receipt file");
        }
        return verify::verify_hosts(mode, Palette::for_mode(mode), target, hosts, replay).await;
    }

    let host = host.unwrap_or_default();
    if let Command::Verify {
        ref target, replay, ..
    } = command
        && verify::is_path_target(Path::new(target), target)
    {
        let palette = Palette::for_mode(mode);
        if !replay {
            if let Some(socket) = socket {
                let ctx = Ctx {
                    client: DaemonClient::new(socket),
                    mode,
                    palette,
                };
                return verify::verify(&ctx, target.clone(), false).await;
            }
            return verify::verify_offline(mode, palette, Path::new(target)).await;
        }

        // An explicit full-replay target is read before Home/socket resolution so
        // missing or malformed paths retain their actionable file diagnostics.
        verify::read_receipt(Path::new(target))?;
        let client = match socket {
            Some(socket) => DaemonClient::new(socket),
            None => match DaemonClient::for_host(&host) {
                Ok(client) => client,
                Err(_) => return verify::full_replay_requires_daemon(),
            },
        };
        let ctx = Ctx {
            client,
            mode,
            palette,
        };
        return verify::verify(&ctx, target.clone(), true).await;
    }

    let client = match socket {
        Some(socket) => DaemonClient::new(socket),
        None => DaemonClient::for_host(&host)?,
    };
    let ctx = Ctx {
        client,
        mode,
        palette: Palette::for_mode(mode),
    };

    match command {
        Command::Serve(_) => unreachable!("serve returned before client construction"),
        Command::Status => status(&ctx).await,
        Command::Stop => stop(&ctx).await,
        Command::Identity { command } => identity(&ctx, command).await,
        Command::Program { command } => program(&ctx, command).await,
        Command::Exec { command } => execution(&ctx, command).await,
        Command::Watch { exec } => watch::watch(&ctx, exec).await,
        Command::Receipt { command } => receipt(&ctx, command).await,
        Command::Verify { target, replay, .. } => verify::verify(&ctx, target, replay).await,
        Command::Run { .. } => unreachable!("coordinated run returned before client construction"),
    }
}

struct CoordinatedCliArgs {
    program: String,
    humans: Vec<HostName>,
    builtins: Vec<String>,
    agents: Vec<String>,
    params: Vec<String>,
    replay: bool,
    no_tui: bool,
}

async fn coordinated_run(mode: Mode, args: CoordinatedCliArgs) -> anyhow::Result<()> {
    if mode.is_json() && !args.humans.is_empty() {
        bail!("--human cannot be used with --json; bind every Host to --builtin or --agent");
    }

    let mut bindings =
        Vec::with_capacity(args.builtins.len() + args.agents.len() + args.humans.len());
    for host in args.humans {
        bindings.push(coordinated::DriverBinding::new(
            host,
            coordinated::DriverSpec::Human,
        ));
    }
    for binding in args.builtins {
        bindings.push(parse_builtin_binding(&binding)?);
    }
    for binding in args.agents {
        bindings.push(parse_agent_binding(&binding)?);
    }
    let params = answer::assemble_params(&args.params).map_err(anyhow::Error::msg)?;
    run_with_bindings(
        mode,
        args.program,
        params,
        bindings,
        args.replay,
        args.no_tui,
    )
    .await
}

fn parse_agent_binding(value: &str) -> anyhow::Result<coordinated::DriverBinding> {
    let (host, executable) = value
        .split_once('=')
        .ok_or_else(|| anyhow!("--agent must be HOST=EXECUTABLE, got '{value}'"))?;
    if executable.is_empty() {
        bail!("--agent has an empty executable path: '{value}'");
    }
    let host = host
        .parse::<HostName>()
        .with_context(|| format!("invalid Host name in --agent '{value}'"))?;
    Ok(coordinated::DriverBinding::new(
        host,
        coordinated::DriverSpec::Executable(PathBuf::from(executable)),
    ))
}

fn parse_builtin_binding(value: &str) -> anyhow::Result<coordinated::DriverBinding> {
    let (host, strategy) = value
        .split_once('=')
        .ok_or_else(|| anyhow!("--builtin must be HOST=STRATEGY, got '{value}'"))?;
    if strategy.is_empty() {
        bail!("--builtin has an empty strategy: '{value}'");
    }
    let host = host
        .parse::<HostName>()
        .with_context(|| format!("invalid Host name in --builtin '{value}'"))?;
    Ok(coordinated::DriverBinding::new(
        host,
        coordinated::DriverSpec::Builtin(strategy.to_owned()),
    ))
}

async fn run_with_bindings(
    mode: Mode,
    program: String,
    params: Option<Value>,
    bindings: Vec<coordinated::DriverBinding>,
    replay: bool,
    no_tui: bool,
) -> anyhow::Result<()> {
    if bindings.len() < 2 {
        bail!("a coordinated run requires at least two distinct Hosts");
    }
    let hosts = bindings
        .iter()
        .map(|binding| binding.host.clone())
        .collect::<Vec<_>>();
    if matches!(mode, Mode::Human) {
        eprintln!("preparing {} local Hosts", hosts.len());
    }
    let daemon = local_daemon::LocalDaemon::connect_or_start(hosts).await?;
    let result = run_with_connected_bindings(mode, program, params, bindings, replay, no_tui).await;
    finish_with_daemon(result, daemon).await
}

async fn run_with_connected_bindings(
    mode: Mode,
    program: String,
    params: Option<Value>,
    bindings: Vec<coordinated::DriverBinding>,
    replay: bool,
    no_tui: bool,
) -> anyhow::Result<()> {
    let has_human = bindings
        .iter()
        .any(|binding| matches!(binding.driver, coordinated::DriverSpec::Human));
    let streams_are_terminal = std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal();
    let tracing_enabled = std::env::var_os("RUST_LOG").is_some();
    let use_tui = should_use_tui(
        has_human,
        no_tui,
        mode,
        streams_are_terminal,
        tracing_enabled,
    );
    let progress = progress::RunProgress::new(
        progress::ProgressMode::for_run(
            matches!(mode, Mode::Human),
            use_tui,
            streams_are_terminal && !tracing_enabled,
        ),
        Palette::for_stderr(mode),
    );
    let result = coordinated::run(
        coordinated::CoordinatedRunArgs {
            program: program.clone(),
            params,
            bindings,
            replay,
            use_tui,
        },
        progress,
    )
    .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            if mode.is_json() {
                // Detailed failures can contain local paths or bounded agent
                // stderr. Keep the machine result redacted; stderr retains the
                // actionable diagnostic from `main`.
                ui::print_json(&json!({"exec": "failed"}));
            }
            return Err(error);
        }
    };
    render_coordinated_result(mode, Palette::for_mode(mode), &program, &result);
    if !result.terminal.is_completed() {
        bail!(
            "coordinated execution {}; producer receipts verified",
            result.terminal.tag()
        );
    }
    Ok(())
}

async fn finish_with_daemon(
    result: anyhow::Result<()>,
    daemon: local_daemon::LocalDaemon,
) -> anyhow::Result<()> {
    let shutdown = daemon.shutdown().await;
    match (result, shutdown) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.context("stop command-scoped local Hosts")),
        (Err(run_error), Err(shutdown_error)) => Err(anyhow!(
            "{run_error:#}; stopping command-scoped local Hosts also failed: {shutdown_error:#}"
        )),
    }
}

fn render_coordinated_result(
    mode: Mode,
    palette: Palette,
    program: &str,
    result: &coordinated::AggregateResult,
) {
    if mode.is_json() {
        let receipts = result
            .receipts
            .iter()
            .map(|receipt| {
                json!({
                    "producer": receipt.producer.to_string(),
                    "result": "valid",
                })
            })
            .collect::<Vec<_>>();
        let mut document = json!({
            "exec": result.terminal.tag(),
            "program": program,
            "program_id": result.program_id.to_string(),
            "session_id": result.session_id.to_string(),
            "participants": result.participants.len(),
            "steps": result.steps,
            "verified": {
                "tier": result.verification.tier.as_str(),
                "receipts": receipts,
                "all_verified": result.verification.all_verified,
                "shared_evidence_agrees": result.verification.shared_evidence_agrees,
            },
        });
        if let coordinated::AggregateTerminal::Completed { outcome } = &result.terminal {
            document["outcome"] = json!(outcome);
        }
        ui::print_json(&document);
    } else {
        let terminal = match &result.terminal {
            coordinated::AggregateTerminal::Completed { .. } => {
                palette.green(result.terminal.tag())
            }
            coordinated::AggregateTerminal::Stopped => palette.yellow(result.terminal.tag()),
            coordinated::AggregateTerminal::Failed => palette.red(result.terminal.tag()),
        };
        println!("{terminal} {program}");
        println!("  session      {}", result.session_id);
        println!("  participants {}", result.participants.len());
        println!("  steps        {}", result.steps);
        if let coordinated::AggregateTerminal::Completed {
            outcome: Some(outcome),
        } = &result.terminal
        {
            println!("  outcome      {}", answer::describe_outcome(outcome));
        }
        println!(
            "  verified     {}/{} receipts ({})",
            result.receipts.len(),
            result.participants.len(),
            result.verification.tier.as_str()
        );
    }
}

async fn status(ctx: &Ctx) -> anyhow::Result<()> {
    let info = match ctx.client().call(&Request::DaemonInfo).await {
        Ok(ResponseOk::DaemonInfo(info)) => info,
        Err(error) if arena0_client::proto::is_connect_error(&error) => {
            if ctx.mode.is_json() {
                ui::print_json(&json!({
                    "reachable": false,
                    "socket": ctx.client.socket().display().to_string(),
                }));
            } else {
                println!(
                    "Host not reachable at {}; run `arena0 serve` for a persistent local service",
                    ctx.client.socket().display()
                );
            }
            return Ok(());
        }
        Err(error) => return Err(error),
        Ok(other) => bail!("unexpected response to daemon.info: {other:?}"),
    };
    let executions = match ctx.client().call(&Request::ExecList).await? {
        ResponseOk::ExecList(list) => list,
        other => bail!("unexpected response to exec.list: {other:?}"),
    };
    let active = executions
        .iter()
        .filter(|status| !status.lifecycle().is_terminal())
        .count();

    if ctx.mode.is_json() {
        ui::print_json(&json!({
            "reachable": true,
            "name": info.name,
            "peer_id": info.peer_id.to_string(),
            "socket": info.socket,
            "abi_version": info.abi_version,
            "programs": info.programs,
            "executions_active": active,
            "uptime_secs": info.uptime_secs,
        }));
    } else {
        println!(
            "Host {}  peer={}  programs={}  active executions={}  uptime={}s",
            info.name,
            info.peer_id.fmt_short(),
            info.programs,
            active,
            info.uptime_secs
        );
        println!("  socket {}", info.socket);
    }
    Ok(())
}

async fn stop(ctx: &Ctx) -> anyhow::Result<()> {
    match ctx.client().call(&Request::DaemonStop).await {
        Ok(ResponseOk::Ack) => {
            if ctx.mode.is_json() {
                ui::print_json(&json!({ "ok": true }));
            } else {
                println!("stopped Host at {}", ctx.client.socket().display());
            }
            Ok(())
        }
        Ok(other) => bail!("unexpected response to daemon.stop: {other:?}"),
        Err(error) => Err(error),
    }
}

async fn identity(ctx: &Ctx, command: IdentityCommand) -> anyhow::Result<()> {
    let request = match command {
        IdentityCommand::New { label } => Request::IdNew { label },
        IdentityCommand::List => Request::IdList,
        IdentityCommand::Show { id } => Request::IdShow {
            id: identity_reference(&id),
        },
        IdentityCommand::Remove { id } => Request::IdRemove {
            id: identity_reference(&id),
        },
    };
    match ctx.client().call(&request).await? {
        ResponseOk::Id(info) => {
            if ctx.mode.is_json() {
                ui::print_json(&id_json(&info));
            } else {
                println!(
                    "{}  {}{}",
                    info.peer_id,
                    info.label.as_deref().unwrap_or("(no label)"),
                    if info.active { "  active" } else { "" }
                );
            }
        }
        ResponseOk::IdList(list) => {
            if ctx.mode.is_json() {
                ui::print_json(&json!({
                    "identities": list.iter().map(id_json).collect::<Vec<_>>()
                }));
            } else {
                let rows = list
                    .iter()
                    .map(|info| {
                        vec![
                            info.peer_id.fmt_short().to_string(),
                            info.label.clone().unwrap_or_else(|| "(no label)".into()),
                            if info.active {
                                "active".into()
                            } else {
                                String::new()
                            },
                        ]
                    })
                    .collect::<Vec<_>>();
                print!(
                    "{}",
                    ui::render_table(
                        &["PEER", "LABEL", "STATE"],
                        &rows,
                        ctx.palette,
                        ctx.viewport().width,
                    )
                );
            }
        }
        ResponseOk::Ack => print_ack(ctx),
        other => bail!("unexpected identity response: {other:?}"),
    }
    Ok(())
}

fn identity_reference(value: &str) -> IdRef {
    value
        .parse::<PeerId>()
        .map_or_else(|_| IdRef::Label(value.to_owned()), IdRef::Peer)
}

fn id_json(info: &arena0_client::api::IdInfo) -> Value {
    json!({
        "peer_id": info.peer_id.to_string(),
        "label": info.label,
        "transport_key": info.transport_key.to_string(),
        "active": info.active,
    })
}

async fn program(ctx: &Ctx, command: ProgramCommand) -> anyhow::Result<()> {
    let response = match command {
        ProgramCommand::List => ctx.client().call(&Request::ProgramList).await?,
        ProgramCommand::Show { program } => {
            ctx.client().call(&Request::ProgramGet { program }).await?
        }
        ProgramCommand::Import { file } => {
            let wasm = std::fs::read(&file).with_context(|| format!("read {}", file.display()))?;
            ctx.client().call(&Request::ProgramImport { wasm }).await?
        }
        ProgramCommand::Remove { program } => {
            ctx.client()
                .call(&Request::ProgramRemove { program })
                .await?
        }
    };
    match response {
        ResponseOk::ProgramList(list) => {
            if ctx.mode.is_json() {
                ui::print_json(&json!({
                    "programs": list.iter().map(program_json).collect::<Vec<_>>()
                }));
            } else {
                let rows = list
                    .iter()
                    .map(|summary| {
                        vec![
                            summary.name.clone(),
                            summary.program_hash.fmt_short().to_string(),
                            summary.participants.to_string(),
                            summary.version.clone(),
                        ]
                    })
                    .collect::<Vec<_>>();
                print!(
                    "{}",
                    ui::render_table(
                        &["PROGRAM", "ID", "PARTICIPANTS", "VERSION"],
                        &rows,
                        ctx.palette,
                        ctx.viewport().width,
                    )
                );
            }
        }
        ResponseOk::Program(detail) => {
            if ctx.mode.is_json() {
                ui::print_json(&json!({
                    "summary": program_json(&detail.summary),
                    "schema": detail.schema,
                }));
            } else {
                println!("{} ({})", detail.summary.display_name, detail.summary.name);
                println!("  id           {}", detail.summary.program_hash);
                println!("  participants {}", detail.summary.participants);
                println!("  version      {}", detail.summary.version);
                println!("  description  {}", detail.summary.description);
                println!(
                    "  schema       {}",
                    serde_json::to_string_pretty(&detail.schema)?
                );
            }
        }
        ResponseOk::Ack => print_ack(ctx),
        other => bail!("unexpected program response: {other:?}"),
    }
    Ok(())
}

fn program_json(summary: &arena0_client::api::ProgramSummary) -> Value {
    json!({
        "program_id": summary.program_hash.to_string(),
        "name": summary.name,
        "display_name": summary.display_name,
        "version": summary.version,
        "description": summary.description,
        "participants": summary.participants,
    })
}

async fn execution(ctx: &Ctx, command: ExecCommand) -> anyhow::Result<()> {
    match command {
        ExecCommand::Create {
            program,
            with,
            join,
            param,
        } => {
            let ensemble = ensemble_spec(&with, join.as_deref())?;
            let params = answer::assemble_params(&param).map_err(anyhow::Error::msg)?;
            let exec_id = ExecId(rand::random());
            let created = ctx
                .client()
                .call_raw(&Request::ExecNew {
                    exec_id,
                    program,
                    params,
                    ensemble,
                })
                .await;
            let created = match created {
                Ok(Ok(created)) => created,
                Ok(Err(error)) => return Err(error.into()),
                Err(error) => {
                    return match ctx
                        .client()
                        .call(&Request::ExecCancelCreation { exec_id })
                        .await
                    {
                        Ok(ResponseOk::Ack) => Err(error.context(format!(
                            "exec.new failed; requested execution {exec_id} was cancelled"
                        ))),
                        Ok(other) => Err(error.context(format!(
                            "exec.new failed; unexpected cleanup response for {exec_id}: {other:?}"
                        ))),
                        Err(cleanup) => Err(error.context(format!(
                            "exec.new failed; cleanup for requested execution {exec_id} was incomplete: {cleanup:#}"
                        ))),
                    };
                }
            };
            match created {
                ResponseOk::ExecCreated {
                    exec_id: returned_exec_id,
                    negotiation_id,
                    session_id,
                    exec_state,
                    queue_position,
                } => {
                    if returned_exec_id != exec_id {
                        return match ctx
                            .client()
                            .call(&Request::ExecCancelCreation { exec_id })
                            .await
                        {
                            Ok(ResponseOk::Ack) => Err(anyhow!(
                                "Host returned ExecId {returned_exec_id}, requested {exec_id}; the requested id was withdrawn"
                            )),
                            Ok(other) => Err(anyhow!(
                                "Host returned ExecId {returned_exec_id}, requested {exec_id}; cleanup returned {other:?}"
                            )),
                            Err(error) => Err(error.context(format!(
                                "Host returned ExecId {returned_exec_id}, requested {exec_id}; cleanup failed"
                            ))),
                        };
                    }
                    if ctx.mode.is_json() {
                        ui::print_json(&json!({
                            "exec_id": returned_exec_id.to_string(),
                            "negotiation_id": negotiation_id.to_string(),
                            "session_id": session_id.map(|id| id.to_string()),
                            "exec_state": exec_state,
                            "queue_position": queue_position,
                        }));
                    } else {
                        println!(
                            "created exec {}  negotiation {}  state {:?}",
                            returned_exec_id.fmt_short(),
                            negotiation_id.fmt_short(),
                            exec_state
                        );
                    }
                }
                other => bail!("unexpected response to exec.create: {other:?}"),
            }
        }
        ExecCommand::List => {
            let ResponseOk::ExecList(list) = ctx.client().call(&Request::ExecList).await? else {
                bail!("unexpected response to exec.list");
            };
            if ctx.mode.is_json() {
                ui::print_json(&json!({
                    "executions": list.iter().map(exec_json).collect::<Vec<_>>()
                }));
            } else {
                let rows = list
                    .iter()
                    .map(|status| {
                        vec![
                            status.exec_id.fmt_short().to_string(),
                            status.program_id.fmt_short().to_string(),
                            format!("{:?}", status.lifecycle()),
                            status
                                .step()
                                .map_or_else(|| "-".into(), |step| step.to_string()),
                        ]
                    })
                    .collect::<Vec<_>>();
                print!(
                    "{}",
                    ui::render_table(
                        &["EXEC", "PROGRAM", "STATE", "STEP"],
                        &rows,
                        ctx.palette,
                        ctx.viewport().width,
                    )
                );
            }
        }
        ExecCommand::Status { exec_id } => {
            let exec_id = ctx.client().resolve_exec(&exec_id).await?;
            let ResponseOk::Status(status) =
                ctx.client().call(&Request::ExecStatus { exec_id }).await?
            else {
                bail!("unexpected response to exec.status");
            };
            render_exec_status(ctx, &status);
        }
        ExecCommand::Await { exec_id, until } => {
            let exec_id = ctx.client().resolve_exec(&exec_id).await?;
            let until = parse_await_state(&until)?;
            match ctx
                .client()
                .call(&Request::ExecAwait { exec_id, until })
                .await?
            {
                ResponseOk::Awaited {
                    exec_id,
                    exec_state,
                    reason,
                } => {
                    if ctx.mode.is_json() {
                        ui::print_json(&json!({
                            "exec_id": exec_id.to_string(),
                            "exec_state": exec_state,
                            "reason": reason,
                        }));
                    } else {
                        println!("{}  {:?}", exec_id.fmt_short(), exec_state);
                        if let Some(reason) = reason {
                            println!("  reason: {reason}");
                        }
                    }
                }
                other => bail!("unexpected response to exec.await: {other:?}"),
            }
        }
        ExecCommand::Drive { exec_id } => {
            let exec_id = ctx.client().resolve_exec(&exec_id).await?;
            let completed = run::drive_loop(ctx, exec_id).await?;
            if ctx.mode.is_json() {
                ui::print_json(&json!({
                    "session_id": completed.session_id.to_string(),
                    "outcome": completed.outcome,
                }));
            } else {
                println!(
                    "completed  session={}  outcome={}",
                    completed.session_id.fmt_short(),
                    completed
                        .outcome
                        .as_ref()
                        .map(answer::describe_outcome)
                        .unwrap_or_else(|| "(none)".into())
                );
            }
        }
        ExecCommand::Next { exec_id } => {
            let exec_id = ctx.client().resolve_exec(&exec_id).await?;
            match ctx.client().call(&Request::ExecNext { exec_id }).await? {
                ResponseOk::Next(event) => render_next(ctx, &event),
                other => bail!("unexpected response to exec.next: {other:?}"),
            }
        }
        ExecCommand::Submit {
            exec_id,
            pending_id,
            answer,
        } => {
            let exec_id = ctx.client().resolve_exec(&exec_id).await?;
            let answer = answer.map(|answer| answer::scalar(&answer));
            match ctx
                .client()
                .call(&Request::ExecSubmit {
                    exec_id,
                    pending_id,
                    answer,
                })
                .await?
            {
                ResponseOk::Ack => print_ack(ctx),
                other => bail!("unexpected response to exec.submit: {other:?}"),
            }
        }
        ExecCommand::Query { exec_id, input } => {
            let exec_id = ctx.client().resolve_exec(&exec_id).await?;
            let query: Value = serde_json::from_str(&input).context("query must be valid JSON")?;
            match ctx
                .client()
                .call(&Request::ExecQuery {
                    exec_id,
                    query: Some(query),
                })
                .await?
            {
                ResponseOk::Query { result } => {
                    if ctx.mode.is_json() {
                        ui::print_json(&json!({ "result": result }));
                    } else {
                        println!("{}", serde_json::to_string_pretty(&result)?);
                    }
                }
                other => bail!("unexpected response to exec.query: {other:?}"),
            }
        }
        ExecCommand::View { exec_id } => {
            let exec_id = ctx.client().resolve_exec(&exec_id).await?;
            let viewport = ctx.viewport();
            match ctx
                .client()
                .call(&Request::ExecView {
                    exec: exec_id,
                    width: viewport.width,
                    color: viewport.color,
                })
                .await?
            {
                ResponseOk::ExecView { step, view } => {
                    if ctx.mode.is_json() {
                        ui::print_json(&json!({ "step": step, "view": view }));
                    } else {
                        print!("{}", ui::render_view_summary(&view, ctx.palette));
                    }
                }
                other => bail!("unexpected response to exec.view: {other:?}"),
            }
        }
        ExecCommand::Trace { exec_id, from, to } => {
            let exec_id = ctx.client().resolve_exec(&exec_id).await?;
            match ctx
                .client()
                .call(&Request::ExecTrace { exec_id, from, to })
                .await?
            {
                ResponseOk::Trace(entries) => {
                    if ctx.mode.is_json() {
                        ui::print_json(&json!({ "entries": entries }));
                    } else {
                        for entry in entries {
                            println!(
                                "step {}  fuel={}  {} -> {}",
                                entry.step, entry.fuel_used, entry.pre_state, entry.post_state
                            );
                        }
                    }
                }
                other => bail!("unexpected response to exec.trace: {other:?}"),
            }
        }
        ExecCommand::Withdraw { exec_id } => {
            let exec_id = ctx.client().resolve_exec(&exec_id).await?;
            match ctx
                .client()
                .call(&Request::ExecWithdraw { exec_id })
                .await?
            {
                ResponseOk::Ack => print_ack(ctx),
                other => bail!("unexpected response to exec.withdraw: {other:?}"),
            }
        }
        ExecCommand::Terminate { exec_id, reason } => {
            let exec_id = ctx.client().resolve_exec(&exec_id).await?;
            match ctx
                .client()
                .call(&Request::ExecTerminate { exec_id, reason })
                .await?
            {
                ResponseOk::Ack => print_ack(ctx),
                other => bail!("unexpected response to exec.terminate: {other:?}"),
            }
        }
    }
    Ok(())
}

fn ensemble_spec(with: &[String], join: Option<&[String]>) -> anyhow::Result<EnsembleSpec> {
    match (with.is_empty(), join) {
        (false, None) => Ok(EnsembleSpec::Explicit {
            peers: with
                .iter()
                .map(|value| {
                    value.parse::<PeerId>().map_err(|_| {
                        anyhow!(
                            "invalid peer id {value}; use the full 64-hex id from the Host identity"
                        )
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?,
        }),
        (true, Some(values)) => run::parse_join_ensemble(values),
        _ => bail!("pass exactly one of --with <peer,...> or --join <creator> <negotiation-id>"),
    }
}

fn parse_await_state(value: &str) -> anyhow::Result<AwaitState> {
    match value.to_ascii_lowercase().as_str() {
        "active" => Ok(AwaitState::Active),
        "terminal" => Ok(AwaitState::Terminal),
        _ => bail!("--until must be active or terminal"),
    }
}

fn exec_json(status: &ExecStatus) -> Value {
    serde_json::to_value(status).expect("execution status is serializable")
}

fn render_exec_status(ctx: &Ctx, status: &ExecStatus) {
    if ctx.mode.is_json() {
        ui::print_json(&exec_json(status));
        return;
    }
    println!("exec {}", status.exec_id);
    println!("  program  {}", status.program_id);
    println!("  state    {:?}", status.lifecycle());
    if let Some(session_id) = status.session_id() {
        println!("  session  {session_id}");
    }
    if let Some(session) = status.session() {
        println!("  step     {}", session.step);
        println!("  peers    {}", session.peers.len());
        println!(
            "  receipt  {}",
            if session.receipt_available {
                "available"
            } else {
                "pending"
            }
        );
        if let Some(callout) = &session.pending_callout {
            println!("  callout  #{}", callout.pending_id);
        }
    }
}

fn render_next(ctx: &Ctx, event: &NextEvent) {
    if ctx.mode.is_json() {
        ui::print_json(&serde_json::to_value(event).expect("next event serializable"));
        return;
    }
    match event {
        NextEvent::Callout {
            pending_id,
            callout_index,
            name,
            prompt,
            schema,
            context,
        } => {
            println!("callout {name} (pending #{pending_id}, index {callout_index})");
            println!("  {prompt}");
            println!(
                "  schema: {}",
                serde_json::to_string(schema).unwrap_or_default()
            );
            if !context.is_null() {
                println!("  context: {}", ui::compact_json(context));
            }
        }
        NextEvent::Completed {
            session_id,
            outcome,
        } => {
            println!("completed session {}", session_id);
            if let Some(outcome) = outcome {
                println!("  outcome: {}", answer::describe_outcome(outcome));
            }
        }
        NextEvent::Failed { reason } => println!("failed: {reason}"),
    }
}

async fn receipt(ctx: &Ctx, command: ReceiptCommand) -> anyhow::Result<()> {
    match command {
        ReceiptCommand::Get {
            session,
            producer,
            out,
        } => {
            let key = resolve_receipt_key(ctx, &session, producer.as_deref()).await?;
            let ResponseOk::Receipt(receipt) =
                ctx.client().call(&Request::ReceiptGet { key }).await?
            else {
                bail!("unexpected response to receipt.get");
            };
            if let Some(path) = out {
                std::fs::write(&path, serde_json::to_vec_pretty(&receipt)?)
                    .with_context(|| format!("write {}", path.display()))?;
                if ctx.mode.is_json() {
                    ui::print_json(&json!({
                        "wrote": path.display().to_string(),
                        "receipt_id": receipt_id_text(&receipt),
                    }));
                } else {
                    println!("wrote {}", path.display());
                }
            } else if ctx.mode.is_json() {
                ui::print_json(&serde_json::to_value(&*receipt)?);
            } else {
                render_receipt(&receipt);
            }
        }
        ReceiptCommand::Import { file } => {
            let bytes = std::fs::read(&file).with_context(|| format!("read {}", file.display()))?;
            let receipt: arena0_client::protocol::Receipt =
                serde_json::from_slice(&bytes).context("receipt must be valid JSON")?;
            match ctx
                .client()
                .call(&Request::ReceiptImport {
                    receipt: Box::new(receipt),
                })
                .await?
            {
                ResponseOk::ReceiptList(entries) => {
                    if ctx.mode.is_json() {
                        ui::print_json(&json!({ "receipts": entries }));
                    } else {
                        println!("imported receipt");
                    }
                }
                other => bail!("unexpected response to receipt.import: {other:?}"),
            }
        }
        ReceiptCommand::List => match ctx.client().call(&Request::ReceiptList).await? {
            ResponseOk::ReceiptList(entries) => {
                if ctx.mode.is_json() {
                    ui::print_json(&json!({ "receipts": entries }));
                } else {
                    let rows = entries
                        .iter()
                        .map(|entry| {
                            vec![
                                entry.receipt_id[..8.min(entry.receipt_id.len())].to_string(),
                                entry.session_id.fmt_short().to_string(),
                                entry.producer.fmt_short().to_string(),
                                entry.program_id.fmt_short().to_string(),
                                if entry.completed {
                                    "completed"
                                } else {
                                    "aborted"
                                }
                                .into(),
                            ]
                        })
                        .collect::<Vec<_>>();
                    print!(
                        "{}",
                        ui::render_table(
                            &["RECEIPT", "SESSION", "PRODUCER", "PROGRAM", "STATE"],
                            &rows,
                            ctx.palette,
                            ctx.viewport().width,
                        )
                    );
                }
            }
            other => bail!("unexpected response to receipt.list: {other:?}"),
        },
        ReceiptCommand::Verify {
            session,
            producer,
            replay,
        } => {
            let key = resolve_receipt_key(ctx, &session, producer.as_deref()).await?;
            match ctx
                .client()
                .call(&Request::ReceiptVerify {
                    receipt: ReceiptRef::Produced(key),
                    full: replay,
                })
                .await?
            {
                ResponseOk::Verified {
                    program_id,
                    session_id,
                    ensemble,
                    steps,
                    result,
                } => render_verified(ctx, program_id, session_id, ensemble, steps, result),
                other => bail!("unexpected response to receipt.verify: {other:?}"),
            }
        }
    }
    Ok(())
}

async fn resolve_receipt_key(
    ctx: &Ctx,
    reference: &str,
    producer: Option<&str>,
) -> anyhow::Result<ReceiptKey> {
    if let Some(producer) = producer {
        return Ok(ReceiptKey {
            session_id: reference
                .parse::<SessionHash>()
                .map_err(|_| anyhow!("invalid session id: {reference}"))?,
            producer: producer
                .parse::<PeerId>()
                .map_err(|_| anyhow!("invalid producer peer id: {producer}"))?,
        });
    }
    ctx.client().resolve_receipt_key(reference).await
}

fn render_receipt(receipt: &arena0_client::protocol::Receipt) {
    println!("receipt {}", receipt_id_text(receipt));
    println!("  program  {}", receipt.body().header().program_hash());
    println!("  session  {}", receipt.body().header().session_hash());
    println!("  producer {}", receipt.producer());
    println!("  steps    {}", receipt.body().trace().len());
    println!(
        "  terminal {}",
        match receipt.body().termination() {
            ReceiptTermination::Completed { .. } => "completed",
            ReceiptTermination::Stopped { .. } => "stopped",
        }
    );
}

fn receipt_id_text(receipt: &arena0_client::protocol::Receipt) -> String {
    receipt
        .receipt_id()
        .as_bytes()
        .iter()
        .fold(String::with_capacity(64), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

fn render_verified(
    ctx: &Ctx,
    program_id: arena0_client::protocol::ProgramHash,
    session_id: SessionHash,
    ensemble: Vec<PeerId>,
    steps: u64,
    result: VerifiedResult,
) {
    let document = json!({
        "tier": match &result {
            VerifiedResult::Light { .. } => "Light",
            VerifiedResult::Full { .. } => "Full",
        },
        "program_id": program_id.to_string(),
        "session_id": session_id.to_string(),
        "ensemble": ensemble.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "steps": steps,
        "result": result,
    });
    if ctx.mode.is_json() {
        ui::print_json(&document);
    } else {
        println!(
            "verified ({})",
            document["tier"].as_str().unwrap_or("unknown")
        );
        println!("  program  {}", program_id);
        println!("  session  {}", session_id);
        println!("  ensemble {} participants", ensemble.len());
        println!("  steps    {steps}");
        println!("  result   {:?}", result);
    }
}

fn print_ack(ctx: &Ctx) {
    if ctx.mode.is_json() {
        ui::print_json(&json!({ "ok": true }));
    } else {
        println!("ok");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_command_paths_parse() {
        assert!(Cli::try_parse_from(["arena0", "--tmp"]).is_ok());
        assert!(
            Cli::try_parse_from(["arena0", "--tmp", "--socket", "/tmp/arena0.sock", "status"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["arena0", "--host", "host-01", "status"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "arena0",
                "serve",
                "--hosts",
                "host-01,host-02",
                "--mcp-listen",
                "127.0.0.1:7330",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "arena0", "run", "program", "--human", "host-01", "--human", "host-02",
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["arena0", "--node", "host-01", "status"]).is_err());
        assert!(Cli::try_parse_from(["arena0", "identity", "list"]).is_ok());
        assert!(Cli::try_parse_from(["arena0", "demo"]).is_err());
        assert!(
            Cli::try_parse_from([
                "arena0",
                "exec",
                "create",
                "program",
                "--with",
                &"11".repeat(32),
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "arena0",
                "run",
                "program",
                "--human",
                "host-01",
                "--builtin",
                "host-02=first-allowed",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "arena0",
                "verify",
                &"22".repeat(32),
                "--hosts",
                "host-01,host-02",
                "--replay",
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["arena0", "verify", &"22".repeat(32), "--full"]).is_err());
        assert!(Cli::try_parse_from(["arena0", "run", "program", "--join"]).is_err());
    }

    #[test]
    fn builtin_bindings_require_a_host_and_strategy() {
        assert!(parse_builtin_binding("host-02=first-allowed").is_ok());
        assert!(parse_builtin_binding("host-02").is_err());
        assert!(parse_builtin_binding("host-02=").is_err());
    }

    #[test]
    fn agent_bindings_require_a_host_and_executable() {
        assert!(parse_agent_binding("host-02=./agent.py").is_ok());
        assert!(parse_agent_binding("host-02").is_err());
        assert!(parse_agent_binding("host-02=").is_err());
    }

    #[test]
    fn operational_tracing_and_noninteractive_modes_disable_the_tui() {
        assert!(should_use_tui(true, false, Mode::Human, true, false));
        assert!(!should_use_tui(true, false, Mode::Human, true, true));
        assert!(!should_use_tui(true, true, Mode::Human, true, false));
        assert!(!should_use_tui(true, false, Mode::Json, true, false));
        assert!(!should_use_tui(true, false, Mode::Human, false, false));
    }
}
