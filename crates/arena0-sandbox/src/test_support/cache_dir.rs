//! The on-disk Wasmtime test cache directory.
//!
//! Compiled into both `arena0-sandbox`'s unit tests and `arena0-test-engine`
//! (through `#[path]`): the sandbox's own unit tests cannot depend on the test
//! engine without a cycle, and one source file keeps both resolving the same
//! directory.

use std::path::{Path, PathBuf};

/// Resolve the on-disk compilation cache directory to one absolute path.
///
/// Rules: the explicit override wins; otherwise `$CARGO_TARGET_DIR`, else
/// `<workspace root>/target` — each suffixed with `wasmtime-cache`. A
/// relative override or `CARGO_TARGET_DIR` is resolved against the workspace
/// root, never against the process working directory (Cargo runs each
/// package's tests from that package's root), so every package's test process
/// resolves the same directory. `Path::join` with an absolute argument yields
/// that argument, so absolute inputs pass through unchanged.
#[must_use]
pub(crate) fn resolve_test_cache_dir(
    override_dir: Option<PathBuf>,
    cargo_target_dir: Option<PathBuf>,
    workspace_root: &Path,
) -> PathBuf {
    if let Some(dir) = override_dir {
        return workspace_root.join(dir);
    }
    let target = cargo_target_dir.map_or_else(
        || workspace_root.join("target"),
        |dir| workspace_root.join(dir),
    );
    target.join("wasmtime-cache")
}

/// The workspace root, derived at compile time from the including crate's
/// manifest location. This assumes the crate stays at
/// `<workspace>/crates/<name>`.
pub(crate) fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate lives at <workspace>/crates/<name>")
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolver_anchors_everything_at_the_workspace_root() {
        let root = Path::new("/ws");
        // Absolute override passes through unchanged.
        assert_eq!(
            resolve_test_cache_dir(Some(PathBuf::from("/cache")), None, root),
            PathBuf::from("/cache")
        );
        // Relative override is anchored at the workspace root, not the cwd.
        assert_eq!(
            resolve_test_cache_dir(Some(PathBuf::from("build/cache")), None, root),
            root.join("build/cache")
        );
        // Absolute CARGO_TARGET_DIR keeps its base.
        assert_eq!(
            resolve_test_cache_dir(None, Some(PathBuf::from("/t")), root),
            PathBuf::from("/t/wasmtime-cache")
        );
        // Relative CARGO_TARGET_DIR is anchored at the workspace root.
        assert_eq!(
            resolve_test_cache_dir(None, Some(PathBuf::from("out")), root),
            root.join("out/wasmtime-cache")
        );
        // Unset values fall back to the workspace target dir.
        assert_eq!(
            resolve_test_cache_dir(None, None, root),
            root.join("target/wasmtime-cache")
        );
        for dir in [
            resolve_test_cache_dir(Some(PathBuf::from("rel")), None, root),
            resolve_test_cache_dir(None, Some(PathBuf::from("rel")), root),
            resolve_test_cache_dir(None, None, root),
        ] {
            assert!(
                dir.is_absolute(),
                "relative input escaped: {}",
                dir.display()
            );
        }
    }
}
