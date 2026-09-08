//! Project-local harness setup.
//!
//! Setup writes only the project-local skill and, for Claude, the lifecycle
//! hook. Existing harness configuration files are left untouched.

use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};

/// A project-local harness supported by arena0 setup.
#[derive(Debug, clap::Subcommand)]
pub(crate) enum Target {
    /// Install the project-local arena0 skill for Codex.
    Codex(Options),
    /// Install the project-local arena0 skill and lifecycle hook for Claude Code.
    Claude(Options),
}

#[derive(Debug, clap::Args)]
pub(crate) struct Options {
    /// Print the plan without changing any files.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Apply the plan without asking for interactive consent.
    #[arg(long, conflicts_with = "dry_run")]
    pub(crate) yes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Missing,
    Identical,
    Different,
}

#[derive(Debug)]
struct FileSpec {
    relative: &'static str,
    content: String,
}

#[derive(Debug)]
struct PlannedFile {
    spec: FileSpec,
    path: PathBuf,
    state: State,
}

impl FileSpec {
    fn plan(self, root: &Path) -> anyhow::Result<PlannedFile> {
        let path = root.join(self.relative);
        let existing = read_existing(&path)?;
        let state = match existing {
            None => State::Missing,
            Some(existing) if existing == self.content.as_bytes() => State::Identical,
            Some(_) => State::Different,
        };
        Ok(PlannedFile {
            spec: self,
            path,
            state,
        })
    }
}

impl PlannedFile {
    fn create_missing(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)?;
        if let Err(error) = output.write_all(self.spec.content.as_bytes()) {
            drop(output);
            let _ = fs::remove_file(&self.path);
            return Err(error);
        }
        output.flush()
    }
}

impl Target {
    fn options(&self) -> &Options {
        match self {
            Self::Codex(options) | Self::Claude(options) => options,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Codex(_) => "codex",
            Self::Claude(_) => "claude",
        }
    }

    fn files(&self, executable: &str) -> anyhow::Result<Vec<FileSpec>> {
        let skill = installed_skill(executable)?;
        Ok(match self {
            Self::Codex(_) => vec![FileSpec {
                relative: ".agents/skills/arena0/SKILL.md",
                content: skill,
            }],
            Self::Claude(_) => vec![
                FileSpec {
                    relative: ".claude/skills/arena0/SKILL.md",
                    content: skill,
                },
                FileSpec {
                    relative: ".claude/settings.json",
                    content: claude_settings(executable),
                },
            ],
        })
    }
}

fn installed_skill(executable: &str) -> anyhow::Result<String> {
    let source = include_str!("../../../skills/arena0/SKILL.md");
    let replacement = format!(
        "For every shell command below, replace only the `arena0` executable with the quoted absolute path shown in this version check. Preserve the quoting.\n\n```sh\n{} --version\n```",
        crate::harness_hook::shell_quote(executable)
    );
    let occurrences = source.matches(EXECUTABLE_MARKER).count();
    if occurrences != 1 {
        bail!(
            "embedded arena0 skill must contain exactly one {EXECUTABLE_MARKER} marker; found {occurrences}"
        );
    }
    Ok(source.replacen(EXECUTABLE_MARKER, &replacement, 1))
}

const EXECUTABLE_MARKER: &str = "<!-- arena0:executable -->";

fn claude_settings(executable: &str) -> String {
    let command = format!(
        "{} hook claude-session-start",
        crate::harness_hook::shell_quote(executable)
    );
    let settings = serde_json::json!({
        "hooks": {
            "SessionStart": [
                {
                    "hooks": [
                        {
                            "type": "command",
                            "command": command,
                        }
                    ]
                }
            ]
        }
    });
    serde_json::to_string_pretty(&settings).expect("encoding Claude settings cannot fail") + "\n"
}

fn current_executable() -> anyhow::Result<String> {
    let path = std::env::current_exe().context("locate the arena0 executable")?;
    if !path.is_absolute() {
        bail!(
            "arena0 executable path must be absolute; current_exe returned {}",
            path.display()
        );
    }
    let executable = path.to_str().context(
        "arena0 executable path is not valid UTF-8; setup cannot write a portable shell command",
    )?;
    if executable.is_empty() || executable.chars().any(char::is_control) {
        bail!(
            "arena0 executable path contains control characters; setup cannot write a safe shell command"
        );
    }
    Ok(executable.to_owned())
}

/// Run one project-local setup operation from the current working directory.
pub(crate) fn run(target: &Target) -> anyhow::Result<()> {
    let executable = current_executable()?;
    run_in(
        &std::env::current_dir().context("resolve project directory")?,
        target,
        &executable,
    )
}

fn run_in(root: &Path, target: &Target, executable: &str) -> anyhow::Result<()> {
    let name = target.name();
    let options = target.options();
    let plan = make_plan(root, target, executable)?;
    print_plan(&plan, name);

    if options.dry_run {
        return Ok(());
    }

    let conflicts = plan
        .iter()
        .filter(|file| file.state == State::Different)
        .count();
    if conflicts != 0 {
        bail!(
            "setup incomplete: preserved {conflicts} existing file(s) conflict with the arena0 setup"
        );
    }

    let changes = plan
        .iter()
        .filter(|file| file.state == State::Missing)
        .count();
    if changes == 0 {
        println!("No setup changes required; existing files are ready.");
        return Ok(());
    }

    if !options.yes {
        if !io::stdin().is_terminal() {
            bail!(
                "setup needs consent to apply {changes} project change(s); rerun with --yes in automation"
            );
        }
        eprint!("Apply {changes} project setup change(s)? [y/N] ");
        io::stderr().flush().context("flush setup prompt")?;
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .context("read setup consent")?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            eprintln!("Setup cancelled; no files changed.");
            return Ok(());
        }
    }

    apply(&plan)
}

fn make_plan(root: &Path, target: &Target, executable: &str) -> anyhow::Result<Vec<PlannedFile>> {
    target
        .files(executable)?
        .into_iter()
        .map(|spec| spec.plan(root))
        .collect()
}

fn read_existing(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", path.display()));
        }
    };
    let file_type = metadata.file_type();
    if !file_type.is_file() && !file_type.is_symlink() {
        bail!("{} exists but is not a regular file", path.display());
    }
    fs::read(path)
        .map(Some)
        .with_context(|| format!("read {}", path.display()))
}

fn print_plan(plan: &[PlannedFile], target: &str) {
    println!("arena0 setup {target} plan (project-local)");
    for file in plan {
        let state = match file.state {
            State::Missing => "create",
            State::Identical => "unchanged",
            State::Different => "conflict; leave unchanged",
        };
        println!("\n--- {} [{state}]", file.spec.relative);
        print!("{}", file.spec.content);
        if !file.spec.content.ends_with('\n') {
            println!();
        }
    }
}

fn apply(plan: &[PlannedFile]) -> anyhow::Result<()> {
    if let Some(conflict) = plan.iter().find(|file| file.state == State::Different) {
        bail!(
            "setup incomplete: {} differs from the arena0 setup; resolve it and rerun",
            conflict.path.display()
        );
    }

    let mut applied = 0usize;
    for file in plan.iter().filter(|file| file.state == State::Missing) {
        match file.create_missing() {
            Ok(()) => {
                applied += 1;
                println!("Created {}", file.spec.relative);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                bail!(
                    "{} appeared during setup; refusing to overwrite it",
                    file.path.display()
                );
            }
            Err(error) => {
                return Err(error).with_context(|| format!("create {}", file.path.display()));
            }
        }
    }
    println!("Setup complete: applied {applied} change(s).");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    const TEST_EXECUTABLE: &str = "/tmp/arena0";

    #[derive(Debug, clap::Parser)]
    struct TestCli {
        #[command(subcommand)]
        target: Target,
    }

    fn target_codex() -> Target {
        Target::Codex(Options {
            dry_run: true,
            yes: false,
        })
    }

    fn target_claude() -> Target {
        Target::Claude(Options {
            dry_run: true,
            yes: false,
        })
    }

    #[test]
    fn yes_and_dry_run_are_mutually_exclusive() {
        assert!(TestCli::try_parse_from(["arena0", "codex", "--yes", "--dry-run"]).is_err());
        assert!(TestCli::try_parse_from(["arena0", "codex", "--endpoint", "http://x"]).is_err());
    }

    #[test]
    fn fresh_plan_contains_only_skill_and_existing_hook() {
        let root = tempfile::tempdir().expect("temporary project");
        let codex = make_plan(root.path(), &target_codex(), TEST_EXECUTABLE).expect("Codex plan");
        assert_eq!(codex.len(), 1);
        assert_eq!(codex[0].spec.relative, ".agents/skills/arena0/SKILL.md");
        assert_eq!(codex[0].state, State::Missing);

        let claude =
            make_plan(root.path(), &target_claude(), TEST_EXECUTABLE).expect("Claude plan");
        assert_eq!(claude.len(), 2);
        assert_eq!(claude[0].spec.relative, ".claude/skills/arena0/SKILL.md");
        assert_eq!(claude[1].spec.relative, ".claude/settings.json");
        assert!(claude.iter().all(|file| file.state == State::Missing));
        assert!(!claude.iter().any(|file| file.spec.relative == ".mcp.json"));
    }

    #[test]
    fn settings_keep_absolute_quoted_hook_command() {
        let root = tempfile::tempdir().expect("temporary project");
        let claude =
            make_plan(root.path(), &target_claude(), TEST_EXECUTABLE).expect("Claude plan");
        let settings = serde_json::from_str::<serde_json::Value>(&claude[1].spec.content)
            .expect("Claude settings JSON");
        assert_eq!(
            settings["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            format!(
                "{} hook claude-session-start",
                crate::harness_hook::shell_quote(TEST_EXECUTABLE)
            )
        );
    }

    #[test]
    fn existing_files_are_idempotent_and_harness_config_is_ignored() {
        let root = tempfile::tempdir().expect("temporary project");
        let skill = root.path().join(".agents/skills/arena0/SKILL.md");
        fs::create_dir_all(skill.parent().expect("skill parent")).expect("skill directory");
        fs::write(
            &skill,
            installed_skill(TEST_EXECUTABLE).expect("installed skill"),
        )
        .expect("skill");
        let config = root.path().join(".codex/config.toml");
        fs::create_dir_all(config.parent().expect("config parent")).expect("config directory");
        fs::write(&config, "user-owned = true\n").expect("config");

        let plan = make_plan(root.path(), &target_codex(), TEST_EXECUTABLE).expect("setup plan");
        assert_eq!(plan[0].state, State::Identical);
        assert_eq!(plan.len(), 1);
        apply(&plan).expect("unchanged setup succeeds");
        assert_eq!(
            fs::read_to_string(config).expect("config contents"),
            "user-owned = true\n"
        );
    }

    #[test]
    fn create_rejects_a_concurrent_file_without_overwriting_it() {
        let root = tempfile::tempdir().expect("temporary project");
        let path = root.path().join(".agents/skills/arena0/SKILL.md");
        let plan = make_plan(root.path(), &target_codex(), TEST_EXECUTABLE).expect("setup plan");
        fs::create_dir_all(path.parent().expect("skill parent")).expect("skill directory");
        fs::write(&path, "concurrent custom skill\n").expect("concurrent skill");

        assert!(apply(&plan).is_err());
        assert_eq!(
            fs::read_to_string(path).expect("preserved concurrent skill"),
            "concurrent custom skill\n"
        );
    }
}
