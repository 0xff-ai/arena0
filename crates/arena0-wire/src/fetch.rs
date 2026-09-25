//! Convergence-fetch stream bounds.
//!
//! `arena0-protocol` owns the fetch frame and encodes it with this crate's
//! bounded field codec; the bounds live here so the stream codec can size a
//! fetch frame body without depending on protocol types.

/// Maximum number of tickets carried by one fetch response.
pub const MAX_FETCH_TICKETS: usize = 64;
/// Maximum encoded bytes in one raw ticket payload.
pub const MAX_FETCH_TICKET_BYTES: usize = 4 * 1024;
/// Maximum encoded body bytes in one fetch response, including its kind,
/// routing key, collection length, and per-ticket lengths.
pub const MAX_FETCH_RESPONSE_BYTES: usize = 32 * 1024;
