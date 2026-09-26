//! Shared persistent engine for this crate's own unit tests.
//!
//! Integration tests use `arena0-test-engine`; unit tests inside `src/`
//! cannot (that would be a dependency cycle back into this crate), so they
//! build their own engine on the same cache directory, resolved by the
//! [`cache_dir`] module both crates compile.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crate::WasmtimeEngine;

mod cache_dir;

use cache_dir::{resolve_test_cache_dir, workspace_root};

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
