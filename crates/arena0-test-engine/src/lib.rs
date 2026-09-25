//! Process-wide Wasmtime test engine backed by a stable compilation cache.
//!
//! Compiling a guest costs ~0.5 s and every [`WasmtimeEngine::new`] starts
//! with an empty in-memory module cache, so a test run that builds one engine
//! per load pays the compile once per participant. Tests share one persistent
//! engine instead: each distinct guest is compiled once per process
//! (in-memory cache) and reused across runs through Wasmtime's on-disk
//! compilation cache. This crate is test support only (`publish = false`) and
//! depends on `arena0-sandbox` without features, so production builds never
//! carry test helpers.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use arena0_sandbox::WasmtimeEngine;

static SHARED_TEST_ENGINE: OnceLock<Arc<WasmtimeEngine>> = OnceLock::new();

/// One persistent engine per test process.
///
/// The engine is built once (first caller wins) with
/// [`WasmtimeEngine::new_persistent`] on a cache directory resolved from
/// the process environment and created if missing. Tests that
/// specifically exercise engine configuration or cache behaviour keep their
/// own engines; every other test loads programs through this one so each
/// guest module is compiled once.
#[must_use]
pub fn shared_test_engine() -> Arc<WasmtimeEngine> {
    SHARED_TEST_ENGINE
        .get_or_init(|| {
            let dir = resolve_test_cache_dir(
                std::env::var_os("ARENA0_WASMTIME_TEST_CACHE").map(PathBuf::from),
                std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from),
                &workspace_root(),
            );
            std::fs::create_dir_all(&dir).expect("create wasmtime test cache directory");
            tracing::debug!(
                target: "arena0::performance",
                operation = "test_engine_cache",
                cache_dir = %dir.display(),
            );
            Arc::new(WasmtimeEngine::new_persistent(&dir).expect("persistent wasmtime test engine"))
        })
        .clone()
}

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

/// The workspace root, derived at compile time from this crate's manifest
/// location. This assumes the crate stays at `<workspace>/crates/<name>`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("test-engine crate lives at <workspace>/crates/<name>")
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
