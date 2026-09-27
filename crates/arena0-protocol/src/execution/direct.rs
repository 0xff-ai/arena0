//! Durable per-peer direct traffic, outside the agreed trace and receipts.

use super::MAX_DIRECT_QUEUE;
use crate::{MAX_DIRECT_CONTROL_BYTES, RangeAttachment};
use arena0_program::bounded;
use borsh::{BorshDeserialize, BorshSerialize};

/// One queued, unacknowledged direct message.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct DirectEntry {
    pub seq: u64,
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_DIRECT_CONTROL_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_DIRECT_CONTROL_BYTES>"
    )]
    pub msg: Vec<u8>,
    pub range: Option<RangeAttachment>,
}

/// Direct-message bookkeeping for one peer, in both directions.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct DirectLane {
    /// Sequence number of the last message queued for this peer; 0 before the first.
    pub(crate) last_queued: u64,
    /// Unacknowledged messages in sequence order. Decode bounds allocation before
    /// reading entries; recovery checks their relation to the sequence history.
    #[borsh(
        serialize_with = "bounded::write_vec::<MAX_DIRECT_QUEUE, _>",
        deserialize_with = "bounded::read_vec::<MAX_DIRECT_QUEUE, _>"
    )]
    pub(crate) queue: Vec<DirectEntry>,
    /// Last message from this peer applied or discarded; 0 before the first.
    pub(crate) last_applied: u64,
}

/// How a received direct frame relates to the peer's lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectArrival {
    /// Already applied or discarded; acknowledge again.
    Duplicate,
    /// The next sequence number; dispatch it.
    Next,
    /// Out of order; reject.
    Gap,
}
