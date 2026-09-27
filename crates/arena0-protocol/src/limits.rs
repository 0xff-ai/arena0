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

/// Maximum length of one object in the local blob store. Import and `append`
/// reject larger objects.
pub const MAX_BLOB_BYTES: u64 = 16 * 1024 * 1024;
/// Maximum program bytes in one direct message (`Effect::SendDirect::msg`).
pub const MAX_DIRECT_CONTROL_BYTES: usize = 2 * 1024;
/// Maximum length of the blob range one direct message carries, and of the
/// bytes one `subtree_cv` call hashes. A power of two of BLAKE3 chunks, so
/// ranges aligned to it are whole BLAKE3 subtrees. `ExecFrame::Direct` asserts
/// that this, `MAX_DIRECT_CONTROL_BYTES`, and the frame's fixed fields fit
/// `MAX_EXEC_FRAME_BYTES`.
pub const MAX_DIRECT_RANGE_BYTES: u64 = 32 * 1024;
