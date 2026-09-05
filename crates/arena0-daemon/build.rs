//! Embed the shipped program wasm into the daemon so a boot seeds its program
//! registry from the binary alone, with no source tree and no network.
//!
//! The wasm is read from `programs/target/wasm32-unknown-unknown/release` (what
//! `just build-programs` produces). A missing blob fails the build: a binary
//! without its shipped programs is broken by design, not worth a warning.

use std::path::PathBuf;
use std::{env, fs};

/// Wasm file stems for each shipped program.
const PROGRAMS: &[&str] = &[
    "rock_paper_scissors",
    "prisoner_dilemma",
    "chess",
    "cumulative_sum",
    "sequential_count",
    "vickrey_auction",
    "contract_net",
];

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo = manifest.join("..").join("..");
    let programs_dir = repo.join("programs/target/wasm32-unknown-unknown/release");

    let mut code = String::from("pub(crate) static PROGRAMS: &[&[u8]] = &[\n");
    let mut missing = Vec::new();
    for stem in PROGRAMS {
        let wasm = programs_dir.join(format!("{stem}.wasm"));
        println!("cargo::rerun-if-changed={}", wasm.display());
        if wasm.exists() {
            code.push_str(&format!(
                "    include_bytes!({:?}),\n",
                wasm.display().to_string()
            ));
        } else {
            missing.push(*stem);
        }
    }
    code.push_str("];\n");
    if !missing.is_empty() {
        panic!(
            "missing embedded programs under {}: {}. Build the programs first \
             (`just build-programs`).",
            programs_dir.display(),
            missing.join(", ")
        );
    }

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(out.join("embedded_programs.rs"), code).unwrap();
}
