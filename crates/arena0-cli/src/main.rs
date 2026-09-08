//! The public `arena0` client.
//!
//! This binary is deliberately a local client: it sends typed requests to one
//! daemon socket with an explicit Host target and renders the response. Persistent and command-scoped Host
//! supervision both execute `arena0d`; offline proof verification lives in
//! `arena0-verify`.

mod agent;
mod context;
mod coordinated;
mod harness_hook;
mod line_input;
mod local_daemon;
mod monitor;
mod process;
mod progress;
mod run;
mod serve;
mod setup;
mod terminal;
mod tui;
mod ui;
mod verify;
mod watch;
mod workspace;

use arena0_client::api::Request;
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, anyhow, bail};
use arena0_client::answer;
use arena0_client::api::{
    ApiErrorCode, AwaitState, EnsembleSpec, ExecStatus, HostRequest, IdRef, NextEvent, PendingId,
    ResponseOk, VerifiedResult,
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
    pub(crate) host: HostName,
    pub(crate) mode: Mode,
    pub(crate) palette: Palette,
}

impl Ctx {
    pub(crate) fn client(&self) -> &DaemonClient {
        &self.client
    }

    pub(crate) async fn call(&self, request: &HostRequest) -> anyhow::Result<ResponseOk> {
        self.client.call_host(&self.host, request).await
    }

    pub(crate) async fn call_raw(
        &self,
        request: &HostRequest,
    ) -> anyhow::Result<arena0_client::api::Response> {
        self.client.call_host_raw(&self.host, request).await
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
        self.client
            .exec_view(&self.host, exec_id, self.viewport())
            .await
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "arena0",
    about = "Run local Hosts, inspect executions, and verify receipts",
    after_help = "Examples:\n  arena0\n  arena0 launch rock-paper-scissors --hosts host-01,host-02\n  arena0 monitor\n  arena0 run rock-paper-scissors --human host-01 --builtin host-02=sample\n  arena0 serve\n  arena0 verify receipt.json\n\nOn a terminal, bare `arena0` opens the local program workspace. `arena0 launch` opens the launcher; adding a program starts a headless emulation; `arena0 monitor` attaches to its daemon. `arena0 serve` keeps Hosts running independently for CLI and API clients.",
    version
)]
struct Cli {
    /// Daemon socket (default: ARENA0_SOCKET or ARENA0_HOME/arena0.sock).
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Select a Host on the shared daemon socket, overriding the harness context.
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
    /// Print the agent-facing arena0 skill without contacting a Host.
    Skill,
    /// Open the Host bound to the current harness context.
    ///
    /// The context is `ARENA0_CONTEXT=harness:session[:agent]`, falling back to
    /// `codex:` followed by the value of `CODEX_THREAD_ID`. Repeating hello
    /// reuses the same Host namespace.
    Hello {
        /// Caller-reported harness name and version.
        #[arg(long, default_value_t = default_user_agent())]
        user_agent: String,
    },
    /// Start the persistent local Host service.
    Serve(serve::ServeArgs),
    /// Install the project-local skill and harness context settings.
    Setup {
        #[command(subcommand)]
        target: setup::Target,
    },
    /// Run an internal harness lifecycle hook.
    #[command(hide = true)]
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },
    /// Launch a headless emulation, or open the launcher when PROGRAM is omitted.
    Launch {
        /// Program name, id, or Wasm path.
        program: Option<String>,
        /// Participating Hosts (default: host-01,host-02). Unbound Hosts use external clients.
        #[arg(long, value_delimiter = ',', value_name = "NAME")]
        hosts: Vec<HostName>,
        /// Bind a deterministic strategy as HOST=STRATEGY.
        #[arg(long, value_name = "HOST=STRATEGY")]
        builtin: Vec<String>,
        /// Bind an executable JSONL agent as HOST=EXECUTABLE.
        #[arg(long, value_name = "HOST=EXECUTABLE")]
        agent: Vec<String>,
        /// Program params as KEY=VALUE.
        #[arg(long, value_name = "KEY=VALUE")]
        param: Vec<String>,
        /// Fully replay every Host receipt before succeeding.
        #[arg(long)]
        replay: bool,
    },
    /// Observe the local daemon and optionally answer individual callouts.
    Monitor(monitor::MonitorArgs),
    /// Show daemon status and its Hosts; --host narrows the report.
    Status,
    /// Stop the local daemon and its Hosts.
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
        /// Verify every Host through these named local Hosts.
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
        /// Fully replay every Host receipt before succeeding.
        #[arg(long)]
        replay: bool,
        /// Use inline terminal output instead of the focused TUI.
        #[arg(long)]
        no_tui: bool,
    },
}

#[derive(Debug, Subcommand)]
enum HookCommand {
    /// Bind the current Claude Code session for later Bash commands.
    #[command(name = "claude-session-start", hide = true)]
    ClaudeSessionStart,
}

#[derive(serde::Serialize)]
struct SkillOutput<'a> {
    name: &'static str,
    markdown: &'a str,
}

fn default_user_agent() -> String {
    format!("arena0-cli/{}", env!("CARGO_PKG_VERSION"))
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
        replay: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(exit) = dispatch_hook(&cli) {
        return exit;
    }
    if let Some(exit) = dispatch_skill(&cli) {
        return exit;
    }
    if let Some(exit) = dispatch_setup(&cli) {
        return exit;
    }
    if cli.tmp && matches!(cli.command, Some(Command::Monitor(_))) {
        eprintln!("error: --tmp does not apply to `arena0 monitor`; attach to an existing home");
        return ExitCode::FAILURE;
    }
    if cli.tmp && matches!(cli.command, Some(Command::Hello { .. })) {
        eprintln!("error: --tmp does not apply to `arena0 hello`; context Hosts are persistent");
        return ExitCode::FAILURE;
    }
    let _temporary_home = match prepare_temporary_home(cli.tmp) {
        Ok(home) => home,
        Err(error) => {
            eprintln!("error: {error:#}");
            return ExitCode::FAILURE;
        }
    };
    // The monitor renders diagnostics in its own panes; stderr tracing would
    // overwrite the alternate screen when it shares the terminal.
    if !matches!(cli.command, Some(Command::Monitor(_))) {
        init_tracing();
    }
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
                "Daemon not reachable; run `arena0` to open the workspace or `arena0 serve` for a persistent service; {error}"
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

/// Dispatch harness hooks before resolving Home, installing tracing, or
/// creating a Tokio runtime. Hooks are deliberately limited to local IO.
fn dispatch_hook(cli: &Cli) -> Option<ExitCode> {
    if !matches!(
        cli.command.as_ref(),
        Some(Command::Hook {
            command: HookCommand::ClaudeSessionStart,
        })
    ) {
        return None;
    }

    let result = (|| -> anyhow::Result<()> {
        if cli.socket.is_some() || cli.host.is_some() || cli.tmp || cli.json {
            bail!("--socket, --host, --tmp, and --json do not apply to arena0 hook");
        }
        harness_hook::claude_session_start()
    })();
    Some(early_exit(result))
}

/// Dispatch the offline skill before resolving Home, installing tracing, or
/// creating a Tokio runtime. The skill is a packaged static artifact and must
/// remain usable in a clean environment with no Host service.
fn dispatch_skill(cli: &Cli) -> Option<ExitCode> {
    if !matches!(cli.command.as_ref(), Some(Command::Skill)) {
        return None;
    }

    let result = (|| -> anyhow::Result<()> {
        if cli.socket.is_some() || cli.host.is_some() || cli.tmp {
            bail!("--socket, --host, and --tmp do not apply to `arena0 skill`");
        }
        let markdown = include_str!("../../../skills/arena0/SKILL.md");
        let bytes = if cli.json {
            let mut bytes = serde_json::to_vec(&SkillOutput {
                name: "arena0",
                markdown,
            })
            .context("encode arena0 skill JSON")?;
            bytes.push(b'\n');
            bytes
        } else {
            markdown.as_bytes().to_vec()
        };
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&bytes)
            .context("write arena0 skill to stdout")?;
        stdout.flush().context("flush arena0 skill stdout")?;
        Ok(())
    })();

    Some(early_exit(result))
}

/// Dispatch project-local setup before resolving Home, installing tracing, or
/// creating a Tokio runtime. Setup reads and updates project files below cwd.
fn dispatch_setup(cli: &Cli) -> Option<ExitCode> {
    let target = match cli.command.as_ref() {
        Some(Command::Setup { target }) => target,
        _ => return None,
    };
    let result = (|| -> anyhow::Result<()> {
        if cli.socket.is_some() || cli.host.is_some() || cli.tmp || cli.json {
            bail!("--socket, --host, --tmp, and --json do not apply to arena0 setup");
        }
        setup::run(target)
    })();
    Some(early_exit(result))
}

fn early_exit(result: anyhow::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
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
    let hosts = local_daemon::host_names(2);
    eprintln!("preparing {} local Hosts", hosts.len());
    let daemon = local_daemon::LocalDaemon::connect_or_start(hosts.clone()).await?;
    choose_and_run(
        daemon,
        workspace::Setup {
            hosts,
            fixed_hosts: false,
            bindings: None,
            params: None,
            replay: true,
        },
        None,
    )
    .await
}

async fn choose_and_run(
    mut daemon: local_daemon::LocalDaemon,
    setup: workspace::Setup,
    reference: Option<&str>,
) -> anyhow::Result<()> {
    let selected = async {
        let client = DaemonClient::from_env()?;
        let (mut programs, executions) = workspace::load(&client, &setup.hosts[0]).await?;
        if let Some(reference) = reference {
            let prefix = reference.to_ascii_lowercase();
            let has_name = programs
                .iter()
                .any(|program| program.summary.name == reference);
            programs.retain(|program| {
                if has_name {
                    program.summary.name == reference
                } else {
                    program
                        .summary
                        .program_hash
                        .to_string()
                        .starts_with(&prefix)
                }
            });
        }
        workspace::choose(programs, executions, setup).await
    }
    .await;
    let selection = match selected {
        Ok(selection) => selection,
        Err(error) => return finish_with_daemon(Err(error), daemon).await,
    };
    let workspace::Exit::Launch(launch) = selection else {
        return finish_with_daemon(Ok(()), daemon).await;
    };
    if let Err(error) = daemon.ensure_hosts(launch.hosts.clone()).await {
        return finish_with_daemon(Err(error), daemon).await;
    }
    let bindings = match launch.input_control {
        workspace::InputControl::Configured(bindings) => bindings,
        control => launch
            .hosts
            .into_iter()
            .map(|host| {
                let driver = match &control {
                    workspace::InputControl::AllHosts => coordinated::DriverSpec::Human,
                    workspace::InputControl::OneHost { host: human } if human == &host => {
                        coordinated::DriverSpec::Human
                    }
                    workspace::InputControl::OneHost { .. } => {
                        coordinated::DriverSpec::Builtin("sample".to_owned())
                    }
                    workspace::InputControl::Configured(_) => {
                        unreachable!("handled configured bindings")
                    }
                };
                coordinated::DriverBinding::new(host, driver)
            })
            .collect(),
    };
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
        Command::Skill => unreachable!("offline skill returned before runtime setup"),
        Command::Hello { user_agent } => {
            if host.is_some() {
                bail!("--host does not apply to `arena0 hello`; context selects the Host");
            }
            let agent_context = context::AgentContext::from_env()?.ok_or_else(|| {
                anyhow!("arena0 hello requires ARENA0_CONTEXT or CODEX_THREAD_ID")
            })?;
            let client = match socket {
                Some(socket) => DaemonClient::new(socket),
                None => DaemonClient::from_env()?,
            };
            return hello(&client, &agent_context, user_agent, mode).await;
        }
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
        Command::Setup { .. } => unreachable!("setup returned before runtime setup"),
        Command::Launch {
            program,
            hosts,
            builtin,
            agent,
            param,
            replay,
        } => {
            if socket.is_some() || host.is_some() {
                bail!("--socket and --host do not apply to `arena0 launch`; use --hosts");
            }
            let interactive = !mode.is_json()
                && std::io::stdin().is_terminal()
                && std::io::stdout().is_terminal()
                && std::io::stderr().is_terminal();
            if program.is_none() && !interactive {
                bail!("a program is required outside a terminal; use `arena0 launch <program>`");
            }
            let fixed_hosts = !hosts.is_empty() || !builtin.is_empty() || !agent.is_empty();
            let configured = !builtin.is_empty() || !agent.is_empty();
            let bindings = launch_bindings(hosts, builtin, agent)?;
            if bindings.len() < 2 {
                bail!("an emulation requires at least two distinct Hosts");
            }
            let params = answer::assemble_params(&param).map_err(anyhow::Error::msg)?;
            let daemon = local_daemon::LocalDaemon::connect_or_start(
                bindings
                    .iter()
                    .map(|binding| binding.host.clone())
                    .collect(),
            )
            .await?;
            let setup = workspace::Setup {
                hosts: bindings
                    .iter()
                    .map(|binding| binding.host.clone())
                    .collect(),
                fixed_hosts,
                bindings: configured.then(|| bindings.clone()),
                params: params.clone(),
                replay,
            };
            let Some(mut program) = program else {
                return choose_and_run(daemon, setup, None).await;
            };
            let client = DaemonClient::from_env()?;
            if coordinated::wasm_reference(&program).is_none() {
                let resolved = client
                    .call_host_raw(
                        &bindings[0].host,
                        &HostRequest::ProgramGet {
                            program: program.clone(),
                        },
                    )
                    .await;
                let resolved = match resolved {
                    Ok(Ok(ResponseOk::Program(detail))) => {
                        Ok(detail.summary.program_hash.to_string())
                    }
                    Ok(Err(error)) if error.code == ApiErrorCode::Ambiguous && interactive => {
                        return choose_and_run(
                            daemon,
                            workspace::Setup {
                                bindings: Some(bindings),
                                fixed_hosts: true,
                                ..setup
                            },
                            Some(&program),
                        )
                        .await;
                    }
                    Ok(Err(error)) => Err(error.into()),
                    Ok(Ok(other)) => Err(anyhow!("unexpected program.get response: {other:?}")),
                    Err(error) => Err(error),
                };
                program = match resolved {
                    Ok(program) => program,
                    Err(error) => {
                        if mode.is_json() {
                            ui::print_json(&json!({"exec":"failed"}));
                        }
                        return finish_with_daemon(Err(error), daemon).await;
                    }
                };
            }
            if !mode.is_json() {
                eprintln!("preparing headless emulation; attach with `arena0 monitor`");
                if bindings
                    .iter()
                    .any(|binding| binding.driver == coordinated::DriverSpec::External)
                {
                    eprintln!("unbound Hosts wait for an external client or a monitor answer");
                }
            }
            let result =
                run_with_connected_bindings(mode, program, params, bindings, replay, true).await;
            return finish_with_daemon(result, daemon).await;
        }
        Command::Monitor(mut args) => {
            if json || tmp {
                bail!("--json and --tmp do not apply to `arena0 monitor`");
            }
            if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                bail!("arena0 monitor requires a terminal; use `arena0 watch --json` for a stream");
            }
            if let Some(host) = host {
                if !args.hosts.is_empty() {
                    bail!("use either --host or monitor --hosts to filter Hosts");
                }
                args.hosts.push(host);
            }
            let client = match socket {
                Some(socket) => DaemonClient::new(socket),
                None => DaemonClient::from_env()?,
            };
            return monitor::attach(client, args).await;
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

    // Context binding applies only to ordinary Host operations. Daemon-only
    // commands and offline receipt verification retain their existing paths;
    // an explicit --host also bypasses context parsing altogether.
    let agent_context = match &command {
        Command::Stop => None,
        Command::Verify { target, replay, .. }
            if !*replay && verify::is_path_target(Path::new(target), target) =>
        {
            None
        }
        _ if host.is_some() => None,
        _ => context::AgentContext::from_env()?,
    };
    let host_selected = host.is_some() || agent_context.is_some();
    let host = host
        .or_else(|| agent_context.as_ref().map(|context| context.host().clone()))
        .unwrap_or_default();
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
                    host,
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
            None => match DaemonClient::from_env() {
                Ok(client) => client,
                Err(_) => return verify::full_replay_requires_daemon(),
            },
        };
        let ctx = Ctx {
            client,
            host,
            mode,
            palette,
        };
        return verify::verify(&ctx, target.clone(), true).await;
    }

    let client = match socket {
        Some(socket) => DaemonClient::new(socket),
        None => DaemonClient::from_env()?,
    };
    let ctx = Ctx {
        client,
        host,
        mode,
        palette: Palette::for_mode(mode),
    };

    match command {
        Command::Skill => unreachable!("offline skill returned before runtime setup"),
        Command::Hello { .. } => unreachable!("hello returned before client construction"),
        Command::Serve(_) => unreachable!("serve returned before client construction"),
        Command::Setup { .. } => unreachable!("setup returned before client construction"),
        Command::Hook { .. } => unreachable!("hook returned before runtime setup"),
        Command::Status => status(&ctx, host_selected).await,
        Command::Stop => stop(&ctx).await,
        Command::Identity { command } => identity(&ctx, command).await,
        Command::Program { command } => program(&ctx, command).await,
        Command::Exec { command } => execution(&ctx, command).await,
        Command::Watch { exec } => watch::watch(&ctx, exec).await,
        Command::Receipt { command } => receipt(&ctx, command).await,
        Command::Verify { target, replay, .. } => verify::verify(&ctx, target, replay).await,
        Command::Run { .. } => unreachable!("coordinated run returned before client construction"),
        Command::Launch { .. } | Command::Monitor(_) => {
            unreachable!("launch/monitor returned before client construction")
        }
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

fn launch_bindings(
    mut hosts: Vec<HostName>,
    builtins: Vec<String>,
    agents: Vec<String>,
) -> anyhow::Result<Vec<coordinated::DriverBinding>> {
    let bindings = builtins
        .iter()
        .map(|value| parse_builtin_binding(value))
        .chain(agents.iter().map(|value| parse_agent_binding(value)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    if hosts.is_empty() {
        hosts = local_daemon::host_names(2);
    }
    let mut seen = std::collections::HashSet::new();
    for host in &hosts {
        if !seen.insert(host) {
            bail!("Host '{host}' was selected more than once");
        }
    }
    let mut drivers = std::collections::HashSet::new();
    for binding in &bindings {
        if !drivers.insert(&binding.host) {
            bail!("Host '{}' has more than one driver binding", binding.host);
        }
        if !hosts.contains(&binding.host) {
            bail!("driver Host '{}' is not selected by --hosts", binding.host);
        }
    }
    Ok(hosts
        .into_iter()
        .map(|host| {
            let driver = bindings
                .iter()
                .find(|binding| binding.host == host)
                .map_or(coordinated::DriverSpec::External, |binding| {
                    binding.driver.clone()
                });
            coordinated::DriverBinding::new(host, driver)
        })
        .collect())
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
            "coordinated execution {}; Host receipts verified",
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
                    "peer_id": receipt.peer_id.to_string(),
                    "receipt_id": receipt.receipt_id.to_string(),
                    "result": "valid",
                })
            })
            .collect::<Vec<_>>();
        let mut document = json!({
            "exec": result.terminal.tag(),
            "program": program,
            "program_id": result.program_id.to_string(),
            "session_id": result.session_id.to_string(),
            "receipt_id": result.receipt_id.to_string(),
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
        println!("  receipt      {}", result.receipt_id);
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

async fn hello(
    client: &DaemonClient,
    context: &context::AgentContext,
    user_agent: String,
    mode: Mode,
) -> anyhow::Result<()> {
    let info = client
        .open_host(Some(context.host().to_string()), user_agent)
        .await?;
    if mode.is_json() {
        ui::print_json(&serde_json::to_value(&info)?);
    } else {
        println!(
            "Host {}  peer={}  user-agent={}",
            info.id,
            info.peer_id,
            info.user_agent.as_deref().unwrap_or("(none)")
        );
    }
    Ok(())
}

async fn status(ctx: &Ctx, host_selected: bool) -> anyhow::Result<()> {
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
                    "Daemon not reachable at {}; run `arena0` to open the workspace or `arena0 serve` for a persistent service",
                    ctx.client.socket().display()
                );
            }
            return Ok(());
        }
        Err(error) => return Err(error),
        Ok(other) => bail!("unexpected response to daemon.info: {other:?}"),
    };
    let mut hosts = ctx.client().list_hosts().await?;
    if host_selected {
        hosts.retain(|status| status.host.id == ctx.host.as_str());
        if hosts.is_empty() {
            bail!(
                "Host '{}' is not open in this daemon; run `arena0 status` to list Hosts",
                ctx.host
            );
        }
    }
    if ctx.mode.is_json() {
        ui::print_json(&json!({
            "reachable": true,
            "daemon": {
                "version": info.version,
                "abi_version": info.abi_version,
                "uptime_secs": info.uptime_secs,
                "socket": info.socket,
            },
            "hosts": hosts,
        }));
    } else {
        let active: usize = hosts.iter().map(|host| host.execs_active).sum();
        println!(
            "Daemon running  Hosts={}  active executions={}  uptime={}s",
            hosts.len(),
            active,
            info.uptime_secs
        );
        println!("  socket {}", info.socket);
        for status in hosts {
            println!(
                "  Host {}  peer={}  programs={}  active executions={}",
                status.host.id,
                status.host.peer_id.fmt_short(),
                status.programs,
                status.execs_active
            );
        }
    }
    Ok(())
}

async fn stop(ctx: &Ctx) -> anyhow::Result<()> {
    match ctx.client().call(&Request::DaemonStop).await {
        Ok(ResponseOk::Ack) => {
            if ctx.mode.is_json() {
                ui::print_json(&json!({ "ok": true }));
            } else {
                println!("stopped daemon at {}", ctx.client.socket().display());
            }
            Ok(())
        }
        Ok(other) => bail!("unexpected response to daemon.stop: {other:?}"),
        Err(error) => Err(error),
    }
}

async fn identity(ctx: &Ctx, command: IdentityCommand) -> anyhow::Result<()> {
    let request = match command {
        IdentityCommand::New { label } => HostRequest::IdNew { label },
        IdentityCommand::List => HostRequest::IdList,
        IdentityCommand::Show { id } => HostRequest::IdShow {
            id: identity_reference(&id),
        },
        IdentityCommand::Remove { id } => HostRequest::IdRemove {
            id: identity_reference(&id),
        },
    };
    match ctx.call(&request).await? {
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
        ProgramCommand::List => ctx.call(&HostRequest::ProgramList).await?,
        ProgramCommand::Show { program } => ctx.call(&HostRequest::ProgramGet { program }).await?,
        ProgramCommand::Import { file } => {
            let wasm = std::fs::read(&file).with_context(|| format!("read {}", file.display()))?;
            ctx.call(&HostRequest::ProgramImport { wasm }).await?
        }
        ProgramCommand::Remove { program } => {
            ctx.call(&HostRequest::ProgramRemove { program }).await?
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
                .call_raw(&HostRequest::ExecNew {
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
                    return match ctx.call(&HostRequest::ExecCancelCreation { exec_id })
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
                        return match ctx.call(&HostRequest::ExecCancelCreation { exec_id })
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
                            "negotiation_id": negotiation_id.map(|id| id.to_string()),
                            "session_id": session_id.map(|id| id.to_string()),
                            "exec_state": exec_state,
                            "queue_position": queue_position,
                        }));
                    } else {
                        let negotiation = negotiation_id.map_or_else(
                            || "waiting for offer".to_owned(),
                            |id| id.fmt_short().to_string(),
                        );
                        println!(
                            "created exec {}  negotiation {}  state {:?}",
                            returned_exec_id.fmt_short(),
                            negotiation,
                            exec_state
                        );
                    }
                }
                other => bail!("unexpected response to exec.create: {other:?}"),
            }
        }
        ExecCommand::List => {
            let ResponseOk::ExecList(list) = ctx.call(&HostRequest::ExecList).await? else {
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
            let exec_id = ctx.client().resolve_exec(&ctx.host, &exec_id).await?;
            let ResponseOk::Status(status) = ctx.call(&HostRequest::ExecStatus { exec_id }).await?
            else {
                bail!("unexpected response to exec.status");
            };
            render_exec_status(ctx, &status);
        }
        ExecCommand::Await { exec_id, until } => {
            let exec_id = ctx.client().resolve_exec(&ctx.host, &exec_id).await?;
            let until = parse_await_state(&until)?;
            match ctx.call(&HostRequest::ExecAwait { exec_id, until }).await? {
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
            let exec_id = ctx.client().resolve_exec(&ctx.host, &exec_id).await?;
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
            let exec_id = ctx.client().resolve_exec(&ctx.host, &exec_id).await?;
            match ctx.call(&HostRequest::ExecNext { exec_id }).await? {
                ResponseOk::Next(event) => render_next(ctx, &event),
                other => bail!("unexpected response to exec.next: {other:?}"),
            }
        }
        ExecCommand::Submit {
            exec_id,
            pending_id,
            answer,
        } => {
            let exec_id = ctx.client().resolve_exec(&ctx.host, &exec_id).await?;
            let answer = answer.map(|answer| answer::scalar(&answer));
            match ctx
                .call(&HostRequest::ExecSubmit {
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
            let exec_id = ctx.client().resolve_exec(&ctx.host, &exec_id).await?;
            let query: Value = serde_json::from_str(&input).context("query must be valid JSON")?;
            match ctx
                .call(&HostRequest::ExecQuery {
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
            let exec_id = ctx.client().resolve_exec(&ctx.host, &exec_id).await?;
            let viewport = ctx.viewport();
            match ctx
                .call(&HostRequest::ExecView {
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
            let exec_id = ctx.client().resolve_exec(&ctx.host, &exec_id).await?;
            match ctx
                .call(&HostRequest::ExecTrace { exec_id, from, to })
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
            let exec_id = ctx.client().resolve_exec(&ctx.host, &exec_id).await?;
            match ctx.call(&HostRequest::ExecWithdraw { exec_id }).await? {
                ResponseOk::Ack => print_ack(ctx),
                other => bail!("unexpected response to exec.withdraw: {other:?}"),
            }
        }
        ExecCommand::Terminate { exec_id, reason } => {
            let exec_id = ctx.client().resolve_exec(&ctx.host, &exec_id).await?;
            match ctx
                .call(&HostRequest::ExecTerminate { exec_id, reason })
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
        ReceiptCommand::Get { session, out } => {
            let receipt = ctx
                .client()
                .resolve_receipt_ref(&ctx.host, &session)
                .await?;
            let ResponseOk::Receipt(receipt) =
                ctx.call(&HostRequest::ReceiptGet { receipt }).await?
            else {
                bail!("unexpected response to receipt.get");
            };
            if let Some(path) = out {
                std::fs::write(&path, serde_json::to_vec_pretty(&receipt)?)
                    .with_context(|| format!("write {}", path.display()))?;
                if ctx.mode.is_json() {
                    ui::print_json(&json!({
                        "wrote": path.display().to_string(),
                        "receipt_id": receipt.receipt_id(),
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
            let receipt: arena0_client::protocol::ReceiptArtifact =
                serde_json::from_slice(&bytes).context("receipt must be valid JSON")?;
            match ctx
                .call(&HostRequest::ReceiptImport {
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
        ReceiptCommand::List => match ctx.call(&HostRequest::ReceiptList).await? {
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
                                format!("{:?}", entry.kind),
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
                            &["RECEIPT", "SESSION", "KIND", "PROGRAM", "STATE"],
                            &rows,
                            ctx.palette,
                            ctx.viewport().width,
                        )
                    );
                }
            }
            other => bail!("unexpected response to receipt.list: {other:?}"),
        },
        ReceiptCommand::Verify { session, replay } => {
            let receipt = ctx
                .client()
                .resolve_receipt_ref(&ctx.host, &session)
                .await?;
            match ctx
                .call(&HostRequest::ReceiptVerify {
                    receipt,
                    full: replay,
                })
                .await?
            {
                ResponseOk::Verified {
                    receipt_id,
                    program_id,
                    session_id,
                    ensemble,
                    steps,
                    result,
                } => render_verified(
                    ctx, receipt_id, program_id, session_id, ensemble, steps, result,
                ),
                other => bail!("unexpected response to receipt.verify: {other:?}"),
            }
        }
    }
    Ok(())
}

fn render_receipt(receipt: &arena0_client::protocol::ReceiptArtifact) {
    println!("receipt {}", receipt.receipt_id());
    println!("  program  {}", receipt.body().header().program_hash());
    println!("  session  {}", receipt.body().header().session_hash());
    println!("  kind     {:?}", receipt.kind());
    println!("  steps    {}", receipt.body().trace().len());
    println!(
        "  terminal {}",
        match receipt.body().termination() {
            ReceiptTermination::Completed { .. } => "completed",
            ReceiptTermination::Stopped { .. } => "stopped",
        }
    );
}

fn render_verified(
    ctx: &Ctx,
    receipt_id: arena0_client::protocol::ReceiptId,
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
        "receipt_id": receipt_id.to_string(),
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
        println!("  receipt  {}", receipt_id);
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
    fn launch_selects_any_number_of_hosts_and_leaves_unbound_hosts_external() {
        let hosts = ["alpha", "beta", "gamma", "delta"]
            .map(|name| name.parse().unwrap())
            .to_vec();
        let bindings = launch_bindings(hosts, vec!["beta=sample".into()], vec![]).unwrap();
        assert_eq!(bindings.len(), 4);
        assert_eq!(
            bindings
                .iter()
                .map(|binding| binding.host.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta", "gamma", "delta"]
        );
        let defaults = launch_bindings(vec![], vec!["host-01=sample".into()], vec![]).unwrap();
        assert_eq!(defaults.len(), 2);
        assert_eq!(defaults[1].driver, coordinated::DriverSpec::External);
        assert_eq!(
            bindings
                .iter()
                .filter(|binding| binding.driver == coordinated::DriverSpec::External)
                .count(),
            3
        );
        assert!(
            launch_bindings(
                vec!["alpha".parse().unwrap(), "beta".parse().unwrap()],
                vec!["other=sample".into()],
                vec![]
            )
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "arena0",
                "launch",
                "program",
                "--hosts",
                "alpha,beta,gamma,delta"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["arena0", "monitor"]).is_ok());
    }

    #[test]
    fn receipt_commands_accept_content_references_without_a_producer() {
        for command in ["get", "verify"] {
            assert!(Cli::try_parse_from(["arena0", "receipt", command, "abcdef"]).is_ok());
            assert!(
                Cli::try_parse_from(["arena0", "receipt", command, "abcdef", "--producer", "peer"])
                    .is_err()
            );
        }
        assert!(Cli::try_parse_from(["arena0", "receipt", "list"]).is_ok());
    }

    #[test]
    fn current_command_paths_parse() {
        assert!(Cli::try_parse_from(["arena0", "--tmp"]).is_ok());
        assert!(
            Cli::try_parse_from(["arena0", "--tmp", "--socket", "/tmp/arena0.sock", "status"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["arena0", "--host", "host-01", "status"]).is_ok());
        assert!(Cli::try_parse_from(["arena0", "mcp"]).is_err());
        assert!(Cli::try_parse_from(["arena0", "serve", "--hosts", "host-01,host-02",]).is_ok());
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
