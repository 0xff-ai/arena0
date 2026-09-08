#![cfg(unix)]

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

fn copy_binary(directory: &Path) -> PathBuf {
    let destination = directory.join("arena0 runner ' $() `touch setup-injected` '");
    fs::copy(env!("CARGO_BIN_EXE_arena0"), &destination).expect("copy arena0 binary");
    let mut permissions = fs::metadata(&destination)
        .expect("copied binary metadata")
        .permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(&destination, permissions).expect("make copied binary executable");
    destination
}

fn setup(binary: &Path, project: &Path, target: &str, option: &str) -> Output {
    Command::new(binary)
        .args(["setup", target, option])
        .current_dir(project)
        .env("PATH", "")
        .env_remove("ARENA0_HOME")
        .env_remove("ARENA0_CACHE_DIR")
        .env_remove("ARENA0_SOCKET")
        .env_remove("ARENA0_CONTEXT")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("RUST_LOG")
        .output()
        .expect("run arena0 setup")
}

fn version_block(skill: &str) -> &str {
    let local_cli = skill
        .split_once("## Local CLI")
        .expect("generated skill Local CLI section")
        .1;
    let code = local_cli
        .split_once("```sh\n")
        .expect("generated executable shell block")
        .1;
    code.split_once("\n```")
        .expect("end of generated executable shell block")
        .0
}

fn run_version_block(skill: &str, working_directory: &Path) -> Output {
    Command::new("/bin/sh")
        .args(["-eu", "-c", version_block(skill)])
        .current_dir(working_directory)
        .env("PATH", "")
        .output()
        .expect("run generated skill command")
}

#[test]
fn setup_binds_the_real_binary_in_both_skills_and_claude_hook() {
    let binaries = tempfile::tempdir().expect("temporary binary directory");
    let binary = copy_binary(binaries.path());
    let project = tempfile::tempdir().expect("temporary project");
    let other_directory = tempfile::tempdir().expect("different working directory");

    let dry_run = setup(&binary, project.path(), "codex", "--dry-run");
    assert!(
        dry_run.status.success(),
        "dry run failed: {:?}",
        dry_run.stderr
    );
    assert!(
        !project.path().join(".agents").exists(),
        "dry run created Codex files"
    );
    let dry_text = String::from_utf8(dry_run.stdout).expect("dry-run output is UTF-8");

    let codex = setup(&binary, project.path(), "codex", "--yes");
    assert!(
        codex.status.success(),
        "Codex setup failed: {:?}",
        codex.stderr
    );
    let codex_skill_path = project.path().join(".agents/skills/arena0/SKILL.md");
    assert!(
        !project.path().join(".codex/config.toml").exists(),
        "fresh Codex setup created an MCP registration file"
    );
    let codex_skill = fs::read_to_string(&codex_skill_path).expect("Codex skill");
    assert!(!codex_skill.contains("<!-- arena0:executable -->"));
    assert!(codex_skill.contains("For every shell command below, replace only"));
    assert!(codex_skill.contains("`arena0 --json hello"));
    assert_eq!(version_block(&dry_text), version_block(&codex_skill));
    let version = run_version_block(&codex_skill, other_directory.path());
    assert!(
        version.status.success(),
        "skill command failed: {version:?}"
    );
    let version_stdout = std::str::from_utf8(&version.stdout).expect("version output is UTF-8");
    assert!(
        version_stdout.starts_with("arena0 "),
        "unexpected version output: {:?}",
        version.stdout
    );
    assert!(
        !other_directory.path().join("setup-injected").exists(),
        "skill path was interpreted as shell code"
    );

    let claude = setup(&binary, project.path(), "claude", "--yes");
    assert!(
        claude.status.success(),
        "Claude setup failed: {:?}",
        claude.stderr
    );
    let claude_skill_path = project.path().join(".claude/skills/arena0/SKILL.md");
    let claude_skill = fs::read_to_string(&claude_skill_path).expect("Claude skill");
    assert_eq!(claude_skill, codex_skill);
    assert!(
        !project.path().join(".mcp.json").exists(),
        "fresh Claude setup created an MCP registration file"
    );

    let settings: Value = serde_json::from_str(
        &fs::read_to_string(project.path().join(".claude/settings.json")).expect("Claude settings"),
    )
    .expect("Claude settings JSON");
    let hook_command = settings["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .expect("Claude hook command");
    assert!(hook_command.ends_with(" hook claude-session-start"));

    let env_file = project.path().join("claude.env");
    let injected = other_directory.path().join("hook-injected");
    let session_id = format!("session'; touch {}; #", injected.display());
    let input = serde_json::json!({"session_id": session_id}).to_string();
    let mut hook = Command::new("/bin/sh")
        .args(["-eu", "-c", hook_command, "arena0-hook"])
        .current_dir(other_directory.path())
        .env("PATH", "")
        .env("CLAUDE_ENV_FILE", &env_file)
        .env_remove("ARENA0_HOME")
        .env_remove("ARENA0_CONTEXT")
        .env_remove("CODEX_THREAD_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run generated Claude hook");
    hook.stdin
        .take()
        .expect("Claude hook stdin")
        .write_all(input.as_bytes())
        .expect("write Claude hook input");
    let hook_output = hook.wait_with_output().expect("wait for Claude hook");
    assert!(
        hook_output.status.success(),
        "Claude hook failed: {:?}",
        hook_output.stderr
    );
    assert!(
        hook_output.stdout.is_empty(),
        "Claude hook wrote stdout: {:?}",
        hook_output.stdout
    );
    assert!(
        hook_output.stderr.is_empty(),
        "Claude hook wrote stderr: {:?}",
        hook_output.stderr
    );
    let environment = fs::read_to_string(&env_file).expect("Claude environment file");
    assert!(environment.contains("export ARENA0_CONTEXT="));
    assert!(!injected.exists(), "Claude hook allowed shell injection");

    let sourced = Command::new("/bin/sh")
        .args([
            "-eu",
            "-c",
            ". \"$1\"; printf '%s' \"$ARENA0_CONTEXT\"",
            "arena0-env",
            env_file.to_str().expect("environment path is UTF-8"),
        ])
        .env("PATH", "")
        .output()
        .expect("source Claude environment file");
    assert!(
        sourced.status.success(),
        "source failed: {:?}",
        sourced.stderr
    );
    assert_eq!(
        String::from_utf8(sourced.stdout).expect("sourced environment is UTF-8"),
        format!("claude:{session_id}")
    );
    assert!(
        !injected.exists(),
        "Claude environment allowed shell injection"
    );
    assert!(
        !other_directory.path().join("setup-injected").exists(),
        "skill command allowed shell injection"
    );

    let codex_skill_before = fs::read(&codex_skill_path).expect("Codex skill before repeat");
    let claude_skill_before = fs::read(&claude_skill_path).expect("Claude skill before repeat");
    let settings_path = project.path().join(".claude/settings.json");
    let settings_before = fs::read(&settings_path).expect("Claude settings before repeat");
    let repeat_codex = setup(&binary, project.path(), "codex", "--yes");
    let repeat_claude = setup(&binary, project.path(), "claude", "--yes");
    assert!(
        repeat_codex.status.success() && repeat_claude.status.success(),
        "repeat setup failed: {:?} {:?}",
        repeat_codex.stderr,
        repeat_claude.stderr
    );
    assert_eq!(
        fs::read(&codex_skill_path).expect("Codex skill after repeat"),
        codex_skill_before
    );
    assert_eq!(
        fs::read(&claude_skill_path).expect("Claude skill after repeat"),
        claude_skill_before
    );
    assert_eq!(
        fs::read(&settings_path).expect("Claude settings after repeat"),
        settings_before
    );
}

#[test]
fn setup_leaves_existing_codex_config_untouched() {
    let binaries = tempfile::tempdir().expect("temporary binary directory");
    let binary = copy_binary(binaries.path());
    let project = tempfile::tempdir().expect("temporary project");
    let config_path = project.path().join(".codex/config.toml");
    fs::create_dir_all(config_path.parent().expect("config parent")).expect("config directory");
    let original = "answer = 'keep'\n\n[mcp_servers.arena0]\nurl = 'http://127.0.0.1:7330/mcp'\n\n[mcp_servers.other]\ncommand = 'keep-tool'\n";
    fs::write(&config_path, original).expect("Codex config");

    let applied = setup(&binary, project.path(), "codex", "--yes");
    assert!(
        applied.status.success(),
        "Codex setup failed: {:?}",
        applied.stderr
    );
    let stdout = String::from_utf8(applied.stdout).expect("setup output is UTF-8");
    assert!(!stdout.contains("127.0.0.1:7330"));
    assert!(!stdout.contains("keep-tool"));
    assert_eq!(
        fs::read_to_string(&config_path).expect("Codex config after setup"),
        original
    );
}

#[test]
fn setup_leaves_existing_claude_mcp_config_untouched() {
    let binaries = tempfile::tempdir().expect("temporary binary directory");
    let binary = copy_binary(binaries.path());
    let project = tempfile::tempdir().expect("temporary project");
    let config_path = project.path().join(".mcp.json");
    let original = r#"{
  "other": {"keep": true},
  "mcpServers": {
    "arena0": {"type": "http", "url": "http://127.0.0.1:7330/mcp"},
    "other-tool": {"command": "keep-tool"}
  }
}
"#;
    fs::write(&config_path, original).expect("Claude MCP config");

    let applied = setup(&binary, project.path(), "claude", "--yes");
    assert!(
        applied.status.success(),
        "Claude setup failed: {:?}",
        applied.stderr
    );
    let stdout = String::from_utf8(applied.stdout).expect("setup output is UTF-8");
    assert!(!stdout.contains("127.0.0.1:7330"));
    assert!(!stdout.contains("keep-tool"));
    assert_eq!(
        fs::read_to_string(&config_path).expect("Claude MCP config after setup"),
        original
    );
}

#[test]
fn setup_rejects_old_and_custom_skill_content_without_mutation() {
    let binaries = tempfile::tempdir().expect("temporary binary directory");
    let binary = copy_binary(binaries.path());
    let old_project = tempfile::tempdir().expect("temporary old-skill project");
    let old_skill_path = old_project.path().join(".agents/skills/arena0/SKILL.md");
    fs::create_dir_all(old_skill_path.parent().expect("skill parent")).expect("skill directory");
    let old_skill = "---\nname: arena0\n---\n\n# Participate through arena0\n\nUse MCP when the interaction needs open discovery.\n";
    fs::write(&old_skill_path, old_skill).expect("old skill");

    let old_conflict = setup(&binary, old_project.path(), "codex", "--yes");
    assert!(
        !old_conflict.status.success(),
        "old skill conflict was reported as success"
    );
    assert_eq!(
        fs::read_to_string(&old_skill_path).expect("old skill after conflict"),
        old_skill
    );

    let custom_project = tempfile::tempdir().expect("temporary custom-skill project");
    let installed = setup(&binary, custom_project.path(), "codex", "--yes");
    assert!(
        installed.status.success(),
        "fresh setup failed: {installed:?}"
    );
    let custom_skill_path = custom_project.path().join(".agents/skills/arena0/SKILL.md");
    let custom_skill = format!(
        "{}\ncustom edit\n",
        fs::read_to_string(&custom_skill_path).expect("installed skill")
    );
    fs::write(&custom_skill_path, &custom_skill).expect("custom skill edit");
    let custom_conflict = setup(&binary, custom_project.path(), "codex", "--yes");
    assert!(
        !custom_conflict.status.success(),
        "custom skill conflict was reported as success"
    );
    assert_eq!(
        fs::read_to_string(&custom_skill_path).expect("custom skill after conflict"),
        custom_skill
    );
}

#[test]
fn setup_rejects_a_conflicting_symlinked_skill_without_mutating_its_target() {
    let binaries = tempfile::tempdir().expect("temporary binary directory");
    let binary = copy_binary(binaries.path());
    let project = tempfile::tempdir().expect("temporary project");
    let skill_path = project.path().join(".agents/skills/arena0/SKILL.md");
    fs::create_dir_all(skill_path.parent().expect("skill parent")).expect("skill directory");
    let target = project.path().join("custom-skill.md");
    let original = "custom skill target\n";
    fs::write(&target, original).expect("custom skill target");
    std::os::unix::fs::symlink(&target, &skill_path).expect("skill symlink");

    let result = setup(&binary, project.path(), "codex", "--yes");
    assert!(
        !result.status.success(),
        "symlinked skill conflict was reported as success"
    );
    assert_eq!(
        fs::read_to_string(&target).expect("skill target after conflict"),
        original
    );
    assert!(
        fs::symlink_metadata(&skill_path)
            .expect("skill symlink metadata")
            .file_type()
            .is_symlink(),
        "setup replaced the skill symlink"
    );
}

#[test]
fn setup_rejects_a_custom_claude_hook_without_creating_a_partial_setup() {
    let binaries = tempfile::tempdir().expect("temporary binary directory");
    let binary = copy_binary(binaries.path());
    let project = tempfile::tempdir().expect("temporary project");
    let settings_path = project.path().join(".claude/settings.json");
    fs::create_dir_all(settings_path.parent().expect("settings parent"))
        .expect("settings directory");
    let original = "{\"hooks\":{\"SessionStart\":[]}}\n";
    fs::write(&settings_path, original).expect("custom settings");

    let result = setup(&binary, project.path(), "claude", "--yes");
    assert!(
        !result.status.success(),
        "custom hook conflict was reported as success"
    );
    assert_eq!(
        fs::read_to_string(&settings_path).expect("settings after conflict"),
        original
    );
    assert!(
        !project
            .path()
            .join(".claude/skills/arena0/SKILL.md")
            .exists(),
        "setup created a partial skill while the hook conflicted"
    );
}
