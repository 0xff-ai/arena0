//! The web UI in `ui/dist`, which `just build-ui` produces. Release builds embed
//! it and fail to compile without it. Debug builds read it from disk on every
//! request, so a fresh `pnpm build` shows up without recompiling the daemon.

use std::path::{Component, Path};

use rust_embed::EmbeddedFile;

#[derive(rust_embed::RustEmbed)]
#[folder = "../../ui/dist/"]
#[cfg_attr(debug_assertions, allow_missing = true)]
struct Dist;

/// One UI file with its content type; `path` has no leading slash.
///
/// `path` comes from the request URL. Only plain relative names are looked up:
/// in debug builds a `..` segment would otherwise reach files outside
/// `ui/dist` through symlinks.
pub(crate) fn find(path: &str) -> Option<EmbeddedFile> {
    if !Path::new(path)
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        return None;
    }
    Dist::get(path)
}
