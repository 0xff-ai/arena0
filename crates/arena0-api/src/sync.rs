//! `GET /sync`: one resumable stream of every Host's summary rows (design
//! §3.2). A client sends the cursor it has *applied*; per Host the daemon
//! either replays what changed after it or resets that Host and sends a full
//! snapshot, then streams live changes.

use std::collections::BTreeMap;

use arena0_crypto::AgentPubKey;
use arena0_program::ProgramHash;
use arena0_protocol::ExecId;
use serde::{Deserialize, Serialize};

use crate::{
    ActivityFrame, BlobEntry, EventFrame, ExecSummary, HostInfo, OpenOffer, ProgramDetail,
    ReceiptListEntry,
};

/// Where a client stands in one Host's change order: it has applied every
/// change up to `seq` of the Host lifetime named `boot_id` (the Host's event
/// `boot_id`). Meaningless with any other `boot_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct HostCursor {
    pub boot_id: String,
    pub seq: u64,
}

/// The cursor a client sends on (re)connect, by Host id. Hosts it omits are
/// snapshotted.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SyncCursor(pub BTreeMap<String, HostCursor>);

/// One `/sync` frame. Per Host, frames arrive in this order: `Host`, then
/// either `Reset` + snapshot `Rows` or catch-up `Rows`, then `Synced`, then
/// live `Rows`. Host event observations follow that Host's `Synced`; daemon
/// activity can interleave anywhere. `Observed` never moves a cursor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum SyncFrame {
    /// A Host this stream covers, its online state and current lifetime. Sent
    /// first for each Host, and again when it starts, stops or reopens.
    Host {
        host: HostInfo,
        /// As in `HostStatus`.
        transport_key: AgentPubKey,
        online: bool,
        boot_id: String,
    },
    /// Drop every row of `host`; a snapshot of its rows follows.
    Reset { host: String },
    /// Rows of `host` current as of its change `seq`. Rows are upserts keyed by
    /// their identity; a row may already reflect changes after `seq`. After
    /// applying them the client's cursor for `host` is `(boot_id, seq)`.
    Rows {
        host: String,
        seq: u64,
        ops: Vec<RowOp>,
    },
    /// `host`'s snapshot or catch-up is complete as of `seq`; live rows follow.
    Synced { host: String, seq: u64 },
    /// A live-only fact with no durable owner. Not replayed on resume. A named
    /// field, not a flattened newtype: an `EventFrame` has its own `kind`.
    Observed { observation: Observation },
}

/// One row upsert or removal. Identity: `Exec` by `exec_id`, `Steps` by
/// `(exec_id, step)`, `Receipt` by `receipt_id`, `Program` by
/// `summary.program_hash`, `Blob` by `hash`; all scoped to the frame's Host.
/// The two large rows are boxed to keep every op small; serde and ts-rs see
/// through the box, so the wire shape is unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "row", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum RowOp {
    Exec(Box<ExecSummary>),
    Steps(StepTimes),
    Receipt(ReceiptListEntry),
    /// The full detail: the UI's program rows carry the schema (callouts,
    /// queries, phases), which `ProgramSummary` lacks.
    Program(Box<ProgramDetail>),
    Blob(BlobEntry),
    /// A registered program was removed from the active catalog.
    ProgramRemoved {
        program_hash: ProgramHash,
    },
}

/// Agreed steps `from_step..from_step + len` of one execution as compact
/// parallel arrays (ruling 3). Steps are immutable, so a client overwrites
/// that range and keeps the rest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct StepTimes {
    pub exec_id: ExecId,
    pub from_step: u64,
    /// Local time this Host stored each step, ms since the epoch.
    pub certified_at_ms: Vec<u64>,
    /// First four bytes of each step's post-state hash, big-endian. Equal
    /// prefixes can hide a divergence with probability 2^-32; the session
    /// focus compares full hashes.
    pub state_prefix: Vec<u32>,
}

/// Live-only facts, re-sent on connect where they have current state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "observed", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum Observation {
    /// A Host's semantic event (the `/events` frame), for activity and
    /// negotiation progress. Not sent for events before the connection.
    Event(EventFrame),
    /// A daemon MCP activity frame.
    Activity(ActivityFrame),
    /// The complete current set of open offers on `host`; replaces the
    /// previous set. Sent on connect and whenever it changes.
    Offers {
        host: String,
        offers: Vec<OpenOffer>,
    },
}
