//! Bounds that guest codecs and Host validation share.

/// Maximum opaque bytes in one effect payload.
pub const MAX_EFFECT_PAYLOAD_BYTES: usize = 64 * 1024;
/// Maximum opaque bytes in one timer payload.
pub const MAX_TIMER_PAYLOAD_BYTES: usize = 64 * 1024;
/// Maximum UTF-8 bytes in one terminal reason.
pub const MAX_TERMINAL_REASON_BYTES: usize = 4 * 1024;
/// Maximum opaque bytes in one successful terminal outcome.
pub const MAX_TERMINAL_OUTCOME_BYTES: usize = 64 * 1024;
/// At most 64 participants in one activation, and therefore at most 64 tickets
/// in one offer and one convergence-fetch response.
pub const MAX_PARTICIPANTS: usize = 64;
