//! `arena0-daemon`: the local process supervisor. It owns one virtual runtime
//! [`arena0_node::Ensemble`] and provisions a per-Host keystore, transport,
//! SQLite store, and execution set. One daemon-owned Unix socket and one
//! Host-explicit MCP endpoint serve every Host.
//!
//! The trust boundary is custody: the daemon holds the seeds and performs all
//! signing (step attestations, BLS activation ratification, synchronous guest
//! signing calls); a client supplies only callout *answers* and never holds a
//! key. The execution actor performs guest signing without crossing the socket.
//!
//! Daemon state changes record one redacted system event and publish one
//! matching local API frame through a Host-local event feed.

mod assets;
mod catalog;
mod ensemble;
mod exec_manager;
mod mcp;
mod mcp_auth;
mod paths;
mod run;
mod schema;
mod server;
mod startup;
mod store;
mod system_event;

#[cfg(test)]
mod open_host_tests;

pub use ensemble::{Daemon, McpConfig};
pub use paths::Paths;
pub use run::run;
pub use store::Keystore;
