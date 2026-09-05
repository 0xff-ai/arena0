//! `cargo-arena0`: build an arena0 program and embed its metadata section.
//!
//! `cargo arena0 build` runs `cargo build` for `wasm32-unknown-unknown`, then
//! appends each produced wasm's borsh `ProgramDefinition` as a custom section so
//! `arena0 program import` reads metadata without compiling.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "cargo-arena0", bin_name = "cargo arena0", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// cargo build for wasm32-unknown-unknown + embed the metadata section.
    Build {
        /// Debug build (default is release).
        #[arg(long)]
        debug: bool,
        /// Extra args passed through to `cargo build` (after `--`).
        #[arg(last = true)]
        cargo_args: Vec<String>,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse_from(cargo_args()).cmd {
        Cmd::Build { debug, cargo_args } => build(debug, cargo_args),
    }
}

/// Cargo invokes subcommand executables with the subcommand name as the first
/// forwarded argument (`cargo-arena0 arena0 ...`). Direct invocations start
/// with the actual command (`cargo-arena0 ...`), so normalize both forms before
/// handing the arguments to Clap.
fn cargo_args() -> Vec<OsString> {
    normalize_cargo_args(std::env::args_os())
}

fn normalize_cargo_args<I>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args: Vec<_> = args.into_iter().collect();
    if args
        .get(1)
        .and_then(|arg| arg.to_str())
        .is_some_and(|arg| arg == "arena0")
    {
        args.remove(1);
    }
    args
}

fn build(debug: bool, extra: Vec<String>) -> anyhow::Result<()> {
    let mut cmd = Command::new("cargo");
    cmd.arg("build");
    if !debug {
        cmd.arg("--release");
    }
    cmd.arg("--target").arg("wasm32-unknown-unknown");
    cmd.args(extra);
    let status = cmd.status().context("failed to run cargo build")?;
    if !status.success() {
        bail!("cargo build exited with {status}");
    }

    let wasm_dir =
        target_dir()?
            .join("wasm32-unknown-unknown")
            .join(if debug { "debug" } else { "release" });
    let entries =
        std::fs::read_dir(&wasm_dir).with_context(|| format!("read {}", wasm_dir.display()))?;

    let engine = arena0_sandbox::WasmtimeEngine::new()?;
    let mut count = 0;
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("wasm") {
            continue;
        }
        let wasm = std::fs::read(&path)?;
        let program = engine.build_program(&wasm)?;
        if program.bytes() != wasm {
            write_atomic(&path, program.bytes())?;
        }
        println!("{}  {}", program.hash(), path.display());
        count += 1;
    }
    if count == 0 {
        bail!("no .wasm produced under {}", wasm_dir.display());
    }
    Ok(())
}

/// The workspace's `target` directory (respects `CARGO_TARGET_DIR`).
fn target_dir() -> anyhow::Result<PathBuf> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .context("failed to run cargo metadata")?;
    if !out.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let value: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let dir = value["target_directory"]
        .as_str()
        .context("cargo metadata: missing target_directory")?;
    Ok(PathBuf::from(dir))
}

/// Write via a temp file + rename so a read-only target (cargo emits `0555` wasm)
/// still gets replaced.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("wasm.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}
