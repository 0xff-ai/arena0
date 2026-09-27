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

/// Maximum length of one object in the local blob store. `create` and
/// `resolve` reject larger objects.
pub const MAX_BLOB_BYTES: u64 = 16 * 1024 * 1024;
/// Maximum program bytes in one direct message (`Effect::SendDirect::msg`).
pub const MAX_DIRECT_CONTROL_BYTES: usize = 2 * 1024;
/// Maximum length of the object range one direct message carries.
pub const MAX_DIRECT_RANGE_BYTES: u64 = 56 * 1024;
/// Maximum Bao slice bytes in one direct frame: the worst case for a
/// `MAX_DIRECT_RANGE_BYTES` range of a `MAX_BLOB_BYTES` object under Bao 0.13
/// combined encoding (8-byte header, whole 1 KiB leaves, 64-byte parents).
/// `ExecFrame::Direct` asserts that this, `MAX_DIRECT_CONTROL_BYTES`, and the
/// frame's fixed fields fit `MAX_EXEC_FRAME_BYTES`.
pub const MAX_DIRECT_SLICE_BYTES: usize = 63_304;
