//! The arena0 host test harness: in-process multi-party sessions, adversarial
//! fixtures, and assertion helpers, shared by every integration test in this
//! crate. Production crates never depend on this crate.

pub mod arena;
pub mod assert;
pub mod fixtures;
pub mod synthetic;
pub mod tracing;
pub mod wasm;
