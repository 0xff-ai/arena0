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

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use arena0_sandbox::WasmtimeEngine;

#[path = "../../arena0-sandbox/src/test_support/cache_dir.rs"]
mod cache_dir;

use cache_dir::{resolve_test_cache_dir, workspace_root};

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
