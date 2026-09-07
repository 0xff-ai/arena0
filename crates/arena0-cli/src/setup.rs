//! Project-local harness setup.
//!
//! Setup is deliberately a small, file-oriented operation. It plans complete
//! file contents first, never merges an existing file, and only creates paths
//! that are still absent when the plan is applied.

use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};

const DEFAULT_MCP_ENDPOINT: &str = "http://127.0.0.1:7330/mcp";

/// A project-local harness supported by arena0 setup.
#[derive(Debug, Clone, clap::Subcommand)]
pub(crate) enum Target {
    /// Configure Codex with the project-local skill and MCP server.
    Codex(Options),
    /// Configure Claude Code with the project-local skill and MCP server.
    Claude(Options),
}

#[derive(Debug, Clone, clap::Args)]
pub(crate) struct Options {
    /// Print the complete plan without changing any files.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Apply the plan without asking for interactive consent.
    #[arg(long, conflicts_with = "dry_run")]
    pub(crate) yes: bool,
    /// Streamable HTTP MCP endpoint to write to the harness configuration.
    #[arg(long, default_value = DEFAULT_MCP_ENDPOINT, value_name = "URL")]
    pub(crate) endpoint: String,
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

impl Target {
    fn options(&self) -> Options {
        match self {
            Self::Codex(options) | Self::Claude(options) => options.clone(),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Codex(_) => "codex",
            Self::Claude(_) => "claude",
        }
    }

    fn files(self, endpoint: &str) -> Vec<FileSpec> {
        let codex_config = format!("[mcp_servers.arena0]\nurl = {}\n", encoded_string(endpoint));
        let claude_config = format!(
            "{{\n  \"mcpServers\": {{\n    \"arena0\": {{\n      \"type\": \"http\",\n      \"url\": {}\n    }}\n  }}\n}}\n",
            encoded_string(endpoint)
        );
        match self {
            Self::Codex(_) => vec![
                FileSpec {
                    relative: ".agents/skills/arena0/SKILL.md",
                    content: include_str!("../../../skills/arena0/SKILL.md").to_owned(),
                },
                FileSpec {
                    relative: ".codex/config.toml",
                    content: codex_config,
                },
            ],
            Self::Claude(_) => vec![
                FileSpec {
                    relative: ".claude/skills/arena0/SKILL.md",
                    content: include_str!("../../../skills/arena0/SKILL.md").to_owned(),
                },
                FileSpec {
                    relative: ".mcp.json",
                    content: claude_config,
                },
            ],
        }
    }
}

/// Encode a URL for both JSON and TOML basic string literals.
/// `serde_json` escapes are accepted by TOML for the characters a URL can
/// contain, and this keeps the generated configuration exact on all hosts.
fn encoded_string(value: &str) -> String {
    serde_json::to_string(value).expect("encoding a string cannot fail")
}

/// Run one project-local setup operation from the current working directory.
pub(crate) fn run(target: Target) -> anyhow::Result<()> {
    run_in(
        &std::env::current_dir().context("resolve project directory")?,
        target,
    )
}

fn run_in(root: &Path, target: Target) -> anyhow::Result<()> {
    let name = target.name();
    let options = target.options();
    let plan = plan_with_endpoint(root, target.clone(), &options.endpoint)?;
    print_plan(&plan, name);

    if options.dry_run {
        return Ok(());
    }

    let missing = plan
        .iter()
        .filter(|file| file.state == State::Missing)
        .count();
    if missing == 0 {
        let preserved = plan
            .iter()
            .filter(|file| file.state == State::Different)
            .count();
        if preserved == 0 {
            println!("No missing project files; existing files were left unchanged.");
        } else {
            eprintln!(
                "Setup incomplete: preserved {preserved} existing file(s) that differ from the arena0 setup."
            );
        }
        return Ok(());
    }

    if !options.yes {
        if !io::stdin().is_terminal() {
            bail!(
                "setup needs consent to create {missing} project file(s); rerun with --yes in automation"
            );
        }
        eprint!("Create {missing} missing project file(s)? [y/N] ");
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

#[cfg(test)]
fn plan(root: &Path, target: Target) -> anyhow::Result<Vec<PlannedFile>> {
    plan_with_endpoint(root, target, DEFAULT_MCP_ENDPOINT)
}

fn plan_with_endpoint(
    root: &Path,
    target: Target,
    endpoint: &str,
) -> anyhow::Result<Vec<PlannedFile>> {
    target
        .files(endpoint)
        .into_iter()
        .map(|spec| {
            let path = root.join(spec.relative);
            let state = match fs::read(&path) {
                Ok(existing) if existing == spec.content.as_bytes() => State::Identical,
                Ok(_) => State::Different,
                Err(error) if error.kind() == io::ErrorKind::NotFound => State::Missing,
                Err(error) => {
                    return Err(error).with_context(|| format!("read {}", path.display()));
                }
            };
            Ok(PlannedFile { spec, path, state })
        })
        .collect()
}

fn print_plan(plan: &[PlannedFile], target: &str) {
    println!("arena0 setup {target} plan (project-local)");
    for file in plan {
        let state = match file.state {
            State::Missing => "create",
            State::Identical => "unchanged",
            State::Different => {
                eprintln!(
                    "warning: {} differs from the arena0 setup; leaving the whole file unchanged",
                    file.path.display()
                );
                "different; leave unchanged"
            }
        };
        println!("\n--- {} [{state}]", file.spec.relative);
        print!("{}", file.spec.content);
        if !file.spec.content.ends_with('\n') {
            println!();
        }
    }
}

fn apply(plan: &[PlannedFile]) -> anyhow::Result<()> {
    let mut created = 0usize;
    let mut preserved = 0usize;
    for file in plan {
        match file.state {
            State::Identical => {}
            State::Different => preserved += 1,
            State::Missing => match create_missing(file) {
                Ok(()) => {
                    created += 1;
                    println!("Created {}", file.spec.relative);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    preserved += 1;
                    eprintln!(
                        "warning: {} appeared during setup; leaving the whole file unchanged",
                        file.path.display()
                    );
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("create {}", file.path.display()));
                }
            },
        }
    }
    if preserved == 0 {
        println!("Setup complete: created {created} file(s); existing files were preserved.");
    } else {
        eprintln!(
            "Setup incomplete: created {created} file(s); preserved {preserved} existing file(s) that differ from the arena0 setup."
        );
    }
    Ok(())
}

fn create_missing(file: &PlannedFile) -> io::Result<()> {
    if let Some(parent) = file.path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&file.path)?;
    if let Err(error) = output.write_all(file.spec.content.as_bytes()) {
        // The file is ours only after a successful write. Best-effort cleanup
        // avoids leaving a partial generated artifact after an I/O failure.
        drop(output);
        let _ = fs::remove_file(&file.path);
        return Err(error);
    }
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[derive(Debug, clap::Parser)]
    struct TestCli {
        #[command(subcommand)]
        target: Target,
    }

    #[test]
    fn yes_and_dry_run_are_mutually_exclusive() {
        assert!(TestCli::try_parse_from(["arena0", "codex", "--yes", "--dry-run"]).is_err());
    }

    #[test]
    fn codex_plan_contains_complete_skill_and_config() {
        let root = tempfile::tempdir().expect("temporary project");
        let plan = plan(
            root.path(),
            Target::Codex(Options {
                dry_run: true,
                yes: false,
                endpoint: DEFAULT_MCP_ENDPOINT.to_owned(),
            }),
        )
        .expect("build setup plan");
        assert_eq!(plan.len(), 2);
        assert!(plan[0].spec.content.starts_with("---\nname: arena0"));
        assert!(plan[1].spec.content.contains("[mcp_servers.arena0]"));
        assert!(!plan[1].spec.content.contains("type = \"http\""));
        assert!(
            plan[1]
                .spec
                .content
                .contains("url = \"http://127.0.0.1:7330/mcp\"")
        );
        assert!(!plan[1].spec.content.contains("command"));
        assert!(plan.iter().all(|file| file.state == State::Missing));
    }

    #[test]
    fn endpoint_is_written_to_both_harness_configs() {
        let root = tempfile::tempdir().expect("temporary project");
        let endpoint = "http://127.0.0.1:7440/mcp";
        let codex = plan_with_endpoint(
            root.path(),
            Target::Codex(Options {
                dry_run: true,
                yes: false,
                endpoint: endpoint.to_owned(),
            }),
            endpoint,
        )
        .expect("build Codex setup plan");
        let claude = plan_with_endpoint(
            root.path(),
            Target::Claude(Options {
                dry_run: true,
                yes: false,
                endpoint: endpoint.to_owned(),
            }),
            endpoint,
        )
        .expect("build Claude setup plan");

        assert!(!codex[1].spec.content.contains("type = \"http\""));
        assert!(codex[1].spec.content.contains(endpoint));
        assert!(claude[1].spec.content.contains("\"type\": \"http\""));
        assert!(claude[1].spec.content.contains(endpoint));
    }

    #[test]
    fn identical_files_are_skipped_and_differences_are_whole_file_conflicts() {
        let root = tempfile::tempdir().expect("temporary project");
        let skill = root.path().join(".agents/skills/arena0/SKILL.md");
        fs::create_dir_all(skill.parent().expect("skill parent")).expect("skill directory");
        fs::write(&skill, include_str!("../../../skills/arena0/SKILL.md")).expect("skill");
        let config = root.path().join(".codex/config.toml");
        fs::create_dir_all(config.parent().expect("config parent")).expect("config directory");
        fs::write(&config, "user-owned\n").expect("config");

        let plan = plan(
            root.path(),
            Target::Codex(Options {
                dry_run: true,
                yes: false,
                endpoint: DEFAULT_MCP_ENDPOINT.to_owned(),
            }),
        )
        .expect("build setup plan");
        assert_eq!(plan[0].state, State::Identical);
        assert_eq!(plan[1].state, State::Different);
        apply(&plan).expect("apply setup plan");
        assert_eq!(
            fs::read_to_string(config).expect("config contents"),
            "user-owned\n"
        );
    }

    #[test]
    fn apply_creates_only_missing_files() {
        let root = tempfile::tempdir().expect("temporary project");
        let plan = plan(
            root.path(),
            Target::Claude(Options {
                dry_run: false,
                yes: true,
                endpoint: DEFAULT_MCP_ENDPOINT.to_owned(),
            }),
        )
        .expect("build setup plan");
        apply(&plan).expect("apply setup plan");
        let path = root.path().join(".mcp.json");
        assert_eq!(
            fs::read_to_string(path).expect("MCP config"),
            "{\n  \"mcpServers\": {\n    \"arena0\": {\n      \"type\": \"http\",\n      \"url\": \"http://127.0.0.1:7330/mcp\"\n    }\n  }\n}\n"
        );
        assert_eq!(
            fs::read_to_string(root.path().join(".claude/skills/arena0/SKILL.md"))
                .expect("Claude skill"),
            include_str!("../../../skills/arena0/SKILL.md")
        );
        apply(&plan).expect("repeat setup plan");
    }
}
