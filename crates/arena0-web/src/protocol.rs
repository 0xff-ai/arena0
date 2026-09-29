//! Frames on the browser's WebSocket.
//!
//! The browser sends [`ClientFrame`]s and receives [`ServerFrame`]s, one JSON
//! text message each. After the upgrade the gateway sends `Hello`, then one
//! `Rows { reset: true }` batch per collection, then `Ready`, then row changes
//! as they happen. A `reset` batch replaces every row of its collection; the
//! gateway sends one whenever it cannot describe a change as upserts and
//! deletes (a reload after `stream.lagged`, or a client that fell behind).
//!
//! Enums accept unknown fields (ts-rs cannot express `deny_unknown_fields` on
//! enums); structs reject them.
//!
//! Calls are the only way the browser acts. [`Op`] is the complete list of
//! operations the gateway performs for it; nothing else reaches the daemon.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::model::{
    ActivityRow, BlobRow, CalloutRow, ExecutionRow, HostRow, OfferRow, ProgramRow, ReceiptRow,
    StepRow,
};

/// Browser → gateway.
#[derive(Debug, Clone, PartialEq, Deserialize, TS)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Run one operation. The gateway answers with exactly one `Reply`
    /// carrying the same `id`, and never retries an operation.
    Call { id: u32, op: Op },
}

/// Gateway → browser.
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerFrame {
    Hello(Hello),
    Rows {
        reset: bool,
        batch: RowBatch,
    },
    /// Every collection has received its first `reset` batch.
    Ready,
    Reply {
        id: u32,
        result: CallResult,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, TS)]
pub struct Hello {
    /// The `arena0` version serving this page.
    pub version: String,
    pub daemon: DaemonRow,
    /// Built-in seat strategies the launcher offers.
    pub strategies: Vec<StrategyRow>,
    /// True when this gateway borrowed a daemon it did not start.
    pub attached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct DaemonRow {
    pub version: String,
    pub abi_version: u32,
    /// Daemon uptime when the `Hello` was sent.
    pub uptime_secs: u64,
    pub socket: String,
    pub mcp_endpoint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct StrategyRow {
    pub name: String,
    pub description: String,
}

/// Row changes for one collection, applied in order.
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[serde(tag = "collection", content = "ops", rename_all = "snake_case")]
pub enum RowBatch {
    Hosts(Vec<RowOp<HostRow>>),
    Executions(Vec<RowOp<ExecutionRow>>),
    Steps(Vec<RowOp<StepRow>>),
    Callouts(Vec<RowOp<CalloutRow>>),
    Programs(Vec<RowOp<ProgramRow>>),
    Receipts(Vec<RowOp<ReceiptRow>>),
    Offers(Vec<RowOp<OfferRow>>),
    Activity(Vec<RowOp<ActivityRow>>),
    Blobs(Vec<RowOp<BlobRow>>),
}

#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RowOp<T> {
    Upsert { key: String, row: T },
    Delete { key: String },
}

/// Exactly one of the two is present.
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CallResult {
    /// The operation's reply; its shape is fixed per `Op` variant (see each
    /// variant's doc).
    Ok(Value),
    Err(ErrorRow),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct ErrorRow {
    pub code: ErrorCode,
    pub message: String,
}

/// The daemon's error codes, plus `gateway` for failures in the gateway
/// itself (daemon unreachable, a reply of the wrong shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    BadRequest,
    CalloutNotPending,
    InputRejected,
    Ambiguous,
    Schema,
    Negotiation,
    Execution,
    Verification,
    Storage,
    Timeout,
    Internal,
    Gateway,
}

/// Every operation the browser may request.
#[derive(Debug, Clone, PartialEq, Deserialize, TS)]
#[serde(tag = "op", content = "args", rename_all = "snake_case")]
pub enum Op {
    /// Render the program's view. Reply: [`ViewReply`].
    View {
        host: String,
        exec_id: String,
        /// Viewport width in character cells.
        width: u16,
        /// Render the shared state after this step; `None` is the latest.
        at_step: Option<u64>,
    },
    /// A page of local event records. Reply: [`RecordsReply`].
    Records {
        host: String,
        exec_id: String,
        /// First event position; `None` selects the latest page.
        from: Option<u64>,
        limit: u16,
    },
    /// Run a program query. Reply: the guest's JSON result.
    Query {
        host: String,
        exec_id: String,
        query: Value,
    },
    /// Fetch a stored artifact. Reply: the `ReceiptArtifact` JSON.
    Receipt { host: String, receipt_id: String },
    /// Verify a stored or dropped artifact. Reply: [`VerifyReply`].
    Verify { host: String, target: VerifyTarget },
    /// Answer an open callout. Reply: `null`. Errors: `input_rejected` (the
    /// program's reason; the callout stays open), `callout_not_pending`
    /// (another client answered), `schema`.
    Answer {
        host: String,
        exec_id: String,
        pending_id: String,
        answer: Value,
    },
    /// Create an offer on one Host. Reply: [`CreatedReply`].
    Create {
        host: String,
        program: String,
        params: Option<Value>,
        participants: u16,
        blobs: Vec<String>,
    },
    /// Join the first offer on the program topic, or one exact offer. Reply:
    /// [`CreatedReply`].
    Join {
        host: String,
        program: String,
        target: Option<JoinTarget>,
        blobs: Vec<String>,
    },
    /// Start one session across local Hosts, with the launcher driving the
    /// seats that are not `you` or `external`. Reply: [`LaunchReply`].
    Launch(LaunchArgs),
    /// Reply: `null`.
    Withdraw { host: String, exec_id: String },
    /// Reply: `null`.
    Terminate { host: String, exec_id: String },
    /// Import Wasm into each listed Host, or every Host when `hosts` is empty.
    /// Reply: [`ProgramImported`].
    ProgramImport {
        hosts: Vec<String>,
        wasm_base64: String,
    },
    /// Reply: `null`.
    ProgramRemove { host: String, program: String },
    /// Store an artifact. Reply: `{ "receipt_id": string }`.
    ReceiptImport { host: String, artifact: Value },
    /// Link a daemon-local file. Reply: [`BlobImported`].
    BlobImport { host: String, path: String },
    /// Copy a blob to a daemon-local path. Reply: `{ "length": number }`.
    BlobExport {
        host: String,
        hash: String,
        path: String,
    },
    /// Open or create a Host. Reply: the new [`HostRow`].
    HostOpen {
        id: Option<String>,
        user_agent: String,
    },
    /// Stop the daemon and every Host. Reply: `null`.
    DaemonStop,
}

#[derive(Debug, Clone, PartialEq, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerifyTarget {
    Stored {
        receipt_id: String,
    },
    /// A `ReceiptArtifact` JSON document supplied by the user.
    Inline {
        artifact: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct JoinTarget {
    pub creator: String,
    pub negotiation_id: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct LaunchArgs {
    pub program: String,
    pub params: Option<Value>,
    /// One seat per participating Host. The first seat's Host creates the
    /// offer; the others join it.
    pub seats: Vec<Seat>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Seat {
    pub host: String,
    pub driver: SeatDriver,
}

#[derive(Debug, Clone, PartialEq, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SeatDriver {
    /// Answered in this UI.
    You,
    /// A built-in strategy named in `Hello.strategies`.
    Builtin { strategy: String },
    /// A JSONL agent executable on this machine.
    Executable { path: String },
    /// Answered by another client, such as an MCP agent.
    External,
}

#[derive(Debug, Clone, PartialEq, Serialize, TS)]
pub struct ViewReply {
    /// The step whose shared state was rendered.
    pub step: u64,
    /// Slot text: UTF-8 with ANSI SGR sequences only.
    pub header: Option<String>,
    pub agents: Option<String>,
    pub state: Option<String>,
    pub status_bar: Option<String>,
    /// Typed blocks the program rendered next to its text slots, validated
    /// by the daemon. Empty for programs that render text only.
    pub blocks: Vec<BlockRow>,
}

/// One typed piece of a program view; mirrors `arena0_protocol::Block`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockRow {
    Facts {
        title: Option<String>,
        items: Vec<FactRow>,
    },
    Table {
        title: Option<String>,
        columns: Vec<String>,
        rows: Vec<Vec<CellRow>>,
    },
    /// Row-major cells; `cells.len() == rows * cols`.
    Board {
        title: Option<String>,
        rows: u8,
        cols: u8,
        cells: Vec<CellRow>,
        row_labels: Vec<String>,
        col_labels: Vec<String>,
    },
    Progress {
        label: String,
        value: u64,
        max: u64,
    },
    Roster {
        title: Option<String>,
        entries: Vec<RosterEntryRow>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct FactRow {
    pub label: String,
    pub value: CellRow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct CellRow {
    pub text: String,
    pub tone: ToneRow,
    /// Participant index in the committed ensemble, for consistent colour.
    pub participant: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct RosterEntryRow {
    pub participant: u8,
    pub status: CellRow,
    pub detail: Option<String>,
}

/// A semantic emphasis; the UI chooses the colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ToneRow {
    Normal,
    Muted,
    Good,
    Warn,
    Bad,
    Highlight,
}

#[derive(Debug, Clone, PartialEq, Serialize, TS)]
pub struct RecordsReply {
    pub from: u64,
    pub total: u64,
    pub next: Option<u64>,
    pub records: Vec<RecordRow>,
}

/// A summary of one durable local event record. Payloads are never exposed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct RecordRow {
    pub position: u64,
    /// Agreed steps this event produced.
    pub steps: Vec<u64>,
    pub event: RecordEventKind,
    pub input_bytes: Option<u64>,
    pub effects: Vec<EffectRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum RecordEventKind {
    SessionStarted,
    MessageReceived,
    InputReceived,
    TimerFired,
    DirectReceived,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct EffectRow {
    pub kind: EffectKindRow,
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum EffectKindRow {
    SessionEnd,
    SessionAbort,
    Broadcast,
    SetTimer,
    Fail,
    SendDirect,
}

#[derive(Debug, Clone, PartialEq, Serialize, TS)]
pub struct VerifyReply {
    pub receipt_id: String,
    pub program: String,
    pub session_id: String,
    /// Participants in canonical order.
    pub ensemble: Vec<String>,
    pub steps: u64,
    pub termination: Termination,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Termination {
    Completed,
    /// A stop report; `cause` is the protocol's stop cause in words.
    Stopped {
        cause: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct CreatedReply {
    pub exec_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct LaunchReply {
    /// The execution each seat's Host created, in seat order.
    pub execs: Vec<ExecRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct ExecRef {
    pub host: String,
    pub exec_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct ProgramImported {
    pub hash: String,
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct BlobImported {
    pub hash: String,
    pub length: u64,
}
