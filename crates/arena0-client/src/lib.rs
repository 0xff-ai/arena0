//! Shared client surface for the arena0 daemon: the socket protocol, id prefix
//! resolution, and program-text sanitizer used by local frontends.

/// The local daemon request/response DTOs.
pub use arena0_api as api;
/// Program artifact and metadata DTOs used by local frontends.
pub use arena0_program as program;
/// Protocol DTOs used by local frontends.
pub use arena0_protocol as protocol;

pub mod answer;
pub mod proto;
pub mod resolve;
pub mod sanitize;
