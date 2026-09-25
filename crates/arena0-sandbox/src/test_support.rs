//! Shared persistent engine for this crate's own unit tests.
//!
//! Integration tests use `arena0-test-engine`; unit tests inside `src/`
//! cannot (that would be a dependency cycle back into this crate), so the
//! ~10-line [`resolve_test_cache_dir`] below duplicates the canonical
//! resolver in `arena0-test-engine` (same rules, same workspace-root base).
//! The two copies must stay in sync; the canonical unit tests live there.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::WasmtimeEngine;

static SHARED_TEST_ENGINE: OnceLock<Arc<WasmtimeEngine>> = OnceLock::new();

pub(crate) fn shared_test_engine() -> Arc<WasmtimeEngine> {
    SHARED_TEST_ENGINE
        .get_or_init(|| {
            let dir = resolve_test_cache_dir(
                std::env::var_os("ARENA0_WASMTIME_TEST_CACHE").map(PathBuf::from),
                std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from),
                &workspace_root(),
            );
            std::fs::create_dir_all(&dir).expect("create wasmtime test cache directory");
            Arc::new(WasmtimeEngine::new_persistent(&dir).expect("persistent wasmtime test engine"))
        })
        .clone()
}

/// Copy of `arena0_test_engine::resolve_test_cache_dir`: the override wins,
/// else `$CARGO_TARGET_DIR`, else `<workspace root>/target` — each suffixed
/// with `wasmtime-cache`, with relative inputs anchored at the workspace
/// root, never at the process working directory.
fn resolve_test_cache_dir(
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
        .expect("sandbox crate lives at <workspace>/crates/<name>")
        .to_path_buf()
}
