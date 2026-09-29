//! Embeds the built web UI (`ui/dist`, or `$ARENA0_UI_DIST`) into the crate.
//!
//! The generated `assets.rs` holds one table entry per file. Without an
//! `index.html` the table is empty and `arena0_web::has_assets()` is false, so
//! a plain `cargo build` works before the UI has ever been built.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=ARENA0_UI_DIST");
    println!("cargo:rerun-if-changed=build.rs");
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let dist = std::env::var_os("ARENA0_UI_DIST")
        .map_or_else(|| manifest.join("../../ui/dist"), PathBuf::from);
    println!("cargo:rerun-if-changed={}", dist.display());

    let mut files = Vec::new();
    if dist.join("index.html").is_file() {
        collect(&dist, &dist, &mut files);
    }
    files.sort();

    let mut table = String::from("pub(crate) static ASSETS: &[(&str, &[u8], &str)] = &[\n");
    for (relative, absolute) in &files {
        let absolute = absolute.canonicalize().expect("canonical asset path");
        writeln!(
            table,
            "    ({relative:?}, include_bytes!({:?}), {:?}),",
            absolute.display().to_string(),
            content_type(relative),
        )
        .expect("write to string");
    }
    table.push_str("];\n");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("out dir")).join("assets.rs");
    std::fs::write(out, table).expect("write assets table");
}

fn collect(root: &Path, directory: &Path, files: &mut Vec<(String, PathBuf)>) {
    for entry in std::fs::read_dir(directory).expect("read dist directory") {
        let path = entry.expect("dist entry").path();
        if path.is_dir() {
            collect(root, &path, files);
        } else {
            let relative = path
                .strip_prefix(root)
                .expect("inside dist")
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            files.push((relative, path));
        }
    }
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, extension)| extension) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
