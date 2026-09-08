//! Run two Codex Participants in an isolated arena0 home.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::IsTerminal as _;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use arena0_client::proto::DaemonClient;
use arena0_home::Home;
use serde_json::Value;
use tokio::process::{Child, Command};

use crate::context::AgentContext;
use crate::local_daemon::LocalDaemon;
use crate::workspace::{self, Exit};

#[derive(Debug)]
enum PaneManager {
    Herdr,
    Tmux(OsString),
}

#[derive(Debug)]
enum Pane {
    Herdr(String),
    Tmux(String),
}

/// Select a program and run one Codex Participant in each of two panes.
pub(crate) async fn run() -> anyhow::Result<()> {
    if !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal()
        || !std::io::stderr().is_terminal()
    {
        bail!("`arena0 launch --agents` requires an interactive terminal");
    }
    require_codex_login().await?;
    let panes = PaneManager::detect()?;

    let home = Home::from_env().context("resolve isolated agent-launch home")?;
    let workspace_root = home.root().join("workspace");
    // ponytail: the temporary home's 0700 ancestor already makes this private.
    fs::create_dir_all(&workspace_root)
        .with_context(|| format!("create {}", workspace_root.display()))?;

    let session = format!("{:032x}", rand::random::<u128>());
    let left_context = format!("arena0-launch:{session}:left");
    let right_context = format!("arena0-launch:{session}:right");
    let left_host = AgentContext::parse(&left_context)?.host().clone();
    let right_host = AgentContext::parse(&right_context)?.host().clone();

    eprintln!("preparing an isolated two-agent workspace");
    let daemon = LocalDaemon::connect_or_start(vec![left_host.clone(), right_host]).await?;
    let selected = async {
        let client = DaemonClient::from_env()?;
        let (programs, executions) = workspace::load(&client, &left_host).await?;
        workspace::choose_agents(programs, executions).await
    }
    .await;
    let program = match selected {
        Ok(Exit::Agents(program)) => program,
        Ok(Exit::Quit) => return crate::finish_with_daemon(Ok(()), daemon).await,
        Ok(Exit::Launch(_)) => unreachable!("agent workspace returned an emulation launch"),
        Err(error) => return crate::finish_with_daemon(Err(error), daemon).await,
    };

    let result = async {
        crate::setup::install_codex_in(&workspace_root)?;
        run_sessions(
            panes,
            &workspace_root,
            &home,
            &program,
            [&left_context, &right_context],
        )
        .await
    }
    .await;
    crate::finish_with_daemon(result, daemon).await
}

async fn require_codex_login() -> anyhow::Result<()> {
    let error = match output("codex", ["login", "status"], "check Codex login").await {
        Ok(_) => return Ok(()),
        Err(error) => error,
    };
    if error
        .root_cause()
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        bail!("`arena0 launch --agents` requires Codex on PATH");
    }
    bail!("Codex is not authenticated; run `codex login` first: {error:#}")
}

impl PaneManager {
    fn detect() -> anyhow::Result<Self> {
        if std::env::var_os("HERDR_ENV").as_deref() == Some(OsStr::new("1"))
            && std::env::var_os("HERDR_PANE_ID").is_some()
        {
            return Ok(Self::Herdr);
        }
        if std::env::var_os("TMUX").is_some()
            && let Some(pane) = std::env::var_os("TMUX_PANE")
        {
            return Ok(Self::Tmux(pane));
        }
        bail!("run `arena0 launch --agents` inside an active Herdr or tmux pane")
    }

    async fn start_right(self, cwd: &Path, command: &str) -> anyhow::Result<Pane> {
        match self {
            Self::Herdr => {
                let bytes = output(
                    "herdr",
                    [
                        OsStr::new("pane"),
                        OsStr::new("split"),
                        OsStr::new("--current"),
                        OsStr::new("--direction"),
                        OsStr::new("right"),
                        OsStr::new("--ratio"),
                        OsStr::new("0.5"),
                        OsStr::new("--cwd"),
                        cwd.as_os_str(),
                        OsStr::new("--no-focus"),
                    ],
                    "run Herdr pane command",
                )
                .await?;
                let response: Value =
                    serde_json::from_slice(&bytes).context("parse Herdr pane split response")?;
                let id = response
                    .pointer("/result/pane/pane_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("Herdr pane split response omitted its pane id"))?
                    .to_owned();
                let pane = Pane::Herdr(id);
                if let Err(error) = output(
                    "herdr",
                    [
                        OsStr::new("pane"),
                        OsStr::new("run"),
                        OsStr::new(pane.id()),
                        OsStr::new(command),
                    ],
                    "run Herdr pane command",
                )
                .await
                {
                    let _ = pane.close().await;
                    return Err(error);
                }
                Ok(pane)
            }
            Self::Tmux(current) => {
                let output = output(
                    "tmux",
                    [
                        OsStr::new("split-window"),
                        OsStr::new("-h"),
                        OsStr::new("-d"),
                        OsStr::new("-P"),
                        OsStr::new("-F"),
                        OsStr::new("#{pane_id}"),
                        OsStr::new("-t"),
                        current.as_os_str(),
                        OsStr::new("-c"),
                        cwd.as_os_str(),
                        OsStr::new(command),
                    ],
                    "run tmux pane command",
                )
                .await?;
                let id = String::from_utf8(output)
                    .context("tmux pane id is not UTF-8")?
                    .trim()
                    .to_owned();
                if id.is_empty() {
                    bail!("tmux split-window returned an empty pane id");
                }
                Ok(Pane::Tmux(id))
            }
        }
    }
}

impl Pane {
    fn id(&self) -> &str {
        match self {
            Self::Herdr(id) | Self::Tmux(id) => id,
        }
    }

    async fn close(&self) -> anyhow::Result<()> {
        match self {
            Self::Herdr(id) => {
                output(
                    "herdr",
                    [OsStr::new("pane"), OsStr::new("close"), OsStr::new(id)],
                    "run Herdr pane command",
                )
                .await?;
            }
            Self::Tmux(id) => {
                output(
                    "tmux",
                    [OsStr::new("kill-pane"), OsStr::new("-t"), OsStr::new(id)],
                    "run tmux pane command",
                )
                .await?;
            }
        }
        Ok(())
    }
}

async fn output<I, S>(program: &str, args: I, description: &str) -> anyhow::Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| description.to_owned())?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    bail!(
        "{description} failed with {}{}",
        output.status,
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    )
}

async fn run_sessions(
    panes: PaneManager,
    workspace: &Path,
    home: &Home,
    program: &str,
    contexts: [&str; 2],
) -> anyhow::Result<()> {
    let right_marker = workspace.join(".arena0-right.done");
    let right_command = shell_session(
        workspace,
        home,
        contexts[1],
        &prompt(program, Role::Join),
        &right_marker,
    )?;
    let right = panes.start_right(workspace, &right_command).await?;
    eprintln!(
        "started the second Codex Participant in pane {}; starting the first here",
        right.id()
    );

    let result = async {
        let mut left = codex_session(
            workspace,
            home.root(),
            home.cache_dir(),
            contexts[0],
            prompt(program, Role::Create),
        )
        .spawn()
        .context("start the first Codex Participant")?;
        let left_status = wait_child(&mut left).await?;
        eprintln!(
            "first Codex session exited; waiting for pane {}",
            right.id()
        );
        let right_status = wait_marker(&right_marker).await?;
        if !left_status.success() || right_status != 0 {
            bail!(
                "agent sessions exited unsuccessfully (current pane: {left_status}; pane {}: {right_status})",
                right.id()
            );
        }
        eprintln!("both Codex Participants finished");
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = right.close().await;
    }
    result
}

fn codex_session(
    workspace: &Path,
    home: &Path,
    cache: &Path,
    context: &str,
    prompt: String,
) -> Command {
    let mut command = Command::new("codex");
    command
        .args([
            "--ask-for-approval",
            "never",
            "--sandbox",
            "workspace-write",
            "-c",
            "sandbox_workspace_write.network_access=true",
            "--cd",
        ])
        .arg(workspace)
        .args(["exec", "--skip-git-repo-check", "--color", "always"])
        .arg(prompt)
        .current_dir(workspace)
        .env_remove("ARENA0_SOCKET")
        .env_remove("ARENA0_HOST")
        .env_remove("CODEX_THREAD_ID")
        .env("ARENA0_HOME", home)
        .env("ARENA0_CACHE_DIR", cache)
        .env("ARENA0_CONTEXT", context)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    command
}

fn shell_session(
    workspace: &Path,
    home: &Home,
    context: &str,
    prompt: &str,
    marker: &Path,
) -> anyhow::Result<String> {
    let quote_path = |path: &Path| -> anyhow::Result<String> {
        Ok(crate::harness_hook::shell_quote(
            path.to_str().context("agent launch path is not UTF-8")?,
        ))
    };
    let marker = quote_path(marker)?;
    let script = format!(
        "status=1; unset ARENA0_SOCKET ARENA0_HOST CODEX_THREAD_ID; export ARENA0_HOME={home} ARENA0_CACHE_DIR={cache} ARENA0_CONTEXT={context}; cd {workspace} && codex --ask-for-approval never --sandbox workspace-write -c sandbox_workspace_write.network_access=true --cd {workspace} exec --skip-git-repo-check --color always {prompt}; status=$?; tmp={marker}.tmp.$$; printf '%s\\n' \"$status\" > \"$tmp\" && mv \"$tmp\" {marker} || exit 125; exit \"$status\"",
        home = quote_path(home.root())?,
        cache = quote_path(home.cache_dir())?,
        context = crate::harness_hook::shell_quote(context),
        workspace = quote_path(workspace)?,
        prompt = crate::harness_hook::shell_quote(prompt),
    );
    // Pane managers evaluate this command through the user's login shell, which
    // may be fish or another non-POSIX shell. Keep status capture and marker
    // publication under `sh` so both participants follow the same lifecycle.
    Ok(format!(
        "exec sh -c {}",
        crate::harness_hook::shell_quote(&script)
    ))
}

#[derive(Clone, Copy)]
enum Role {
    Create,
    Join,
}

fn prompt(program: &str, role: Role) -> String {
    let start = match role {
        Role::Create => format!(
            "Create the interaction exactly once with `arena0 --json exec create {program} --participants 2`, adding any `--param KEY=VALUE` arguments required by the inspected schema."
        ),
        Role::Join => format!(
            "Join the matching open offer exactly once with `arena0 --json exec create {program} --join`."
        ),
    };
    format!(
        "You are one of exactly two autonomous Participants in an arena0 interaction. Follow the installed arena0 skill. Keep the supplied harness context unchanged. Run `arena0 --json hello --user-agent codex`, inspect program `{program}`, then continue without user input or confirmation. {start} Retain its exec_id and, before every subsequent answer or turn, run `arena0 exec view <EXEC_ID>` with that id and print the authored view. Narrate each state and action concisely with an emoji. Drive the interaction through completion, verify the session receipt, and report the observed result. Use open program-topic discovery; do not use files, invent a peer id, or create a replacement execution while waiting."
    )
}

async fn wait_child(child: &mut Child) -> anyhow::Result<std::process::ExitStatus> {
    tokio::select! {
        status = child.wait() => status.context("wait for the first Codex Participant"),
        signal = tokio::signal::ctrl_c() => {
            signal.context("listen for agent-launch cancellation")?;
            let _ = child.start_kill();
            let _ = child.wait().await;
            bail!("agent launch cancelled")
        }
    }
}

async fn wait_marker(marker: &Path) -> anyhow::Result<i32> {
    loop {
        match fs::read_to_string(marker) {
            Ok(value) => {
                return value
                    .trim()
                    .parse::<i32>()
                    .with_context(|| format!("parse {}", marker.display()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("read {}", marker.display())),
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            signal = tokio::signal::ctrl_c() => {
                signal.context("listen for agent-launch cancellation")?;
                bail!("agent launch cancelled")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_use_open_roles_without_approval_prompts() {
        let creator = prompt("01ab", Role::Create);
        let joiner = prompt("01ab", Role::Join);
        assert!(creator.contains("exec create 01ab --participants 2"));
        assert!(joiner.contains("exec create 01ab --join"));
        assert!(creator.contains("before every subsequent answer or turn"));
        assert!(creator.contains("arena0 exec view <EXEC_ID>"));
        assert!(creator.contains("Narrate each state and action concisely with an emoji"));

        let left = codex_session(
            Path::new("/tmp/work"),
            Path::new("/tmp/home"),
            Path::new("/tmp/cache"),
            "arena0-launch:session:left",
            creator,
        );
        let left_args = left
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();
        assert!(
            left_args
                .windows(2)
                .any(|args| args == ["exec", "--skip-git-repo-check"])
        );
        assert!(left_args.contains(&"sandbox_workspace_write.network_access=true".into()));

        let command = shell_session(
            Path::new("/tmp/work"),
            &Home::from_root("/tmp/home".into()).unwrap(),
            "arena0-launch:session:right",
            &joiner,
            Path::new("/tmp/done"),
        )
        .unwrap();
        assert!(command.starts_with("exec sh -c "));
        assert!(command.contains("--ask-for-approval never"));
        assert!(command.contains("sandbox_workspace_write.network_access=true"));
        assert!(command.contains("exec --skip-git-repo-check --color always"));
        assert!(command.contains("ARENA0_CONTEXT="));
        assert!(command.contains("arena0-launch:session:right"));
    }
}
