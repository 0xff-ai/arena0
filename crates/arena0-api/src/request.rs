//! Requests on the daemon's local Unix socket surface.
//! The serde tag is the wire "path", so a frame reads path-like
//! (`{"method":"exec.next","params":{...}}`) while staying compile-checked.
//! Identity and catalog management methods are CLI-only.
//!
//! Values that a program schema describes cross this boundary as JSON, not opaque
//! bytes: `params`, callout answers, query in/out, and the terminal outcome. The
//! daemon validates JSON against the program's public JSON Schema at the crossing
//! and forwards it unchanged; the guest converts it to concrete DTOs.

use arena0_program::ProgramHash;
use arena0_protocol::BlobHash;
use arena0_protocol::{
    CalloutId, ColorDepth, ExecId, NegotiationTarget, ReceiptArtifact, SessionHash,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

use crate::events::EventFilter;

/// Bytes read for an import: a daemon-local file (socket callers only), or
/// an upload named by the BLAKE3 hash of its bytes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum FileSource {
    Path(PathBuf),
    Upload(BlobHash),
}
/// One request to the daemon's shared Unix endpoint. Host operations always
/// name their target explicitly; daemon operations do not select a Host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", content = "params", deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum Request {
    /// Dispatch an existing Host operation in one exact local namespace.
    #[serde(rename = "host.call")]
    Host { host: String, request: HostRequest },
    #[serde(rename = "daemon.info")]
    DaemonInfo,
    #[serde(rename = "daemon.stop")]
    DaemonStop,
    #[serde(rename = "hosts.list")]
    HostsList,
    /// Open or create a Host through the daemon's supervised provisioning path.
    #[serde(rename = "hosts.open")]
    HostsOpen {
        id: Option<String>,
        user_agent: String,
    },
    #[serde(rename = "activity.subscribe")]
    ActivitySubscribe,
}

/// An operation on the Host explicitly selected by [`Request::Host`].
/// Event subscriptions acknowledge the request, then stream Host event frames.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", content = "params", deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum HostRequest {
    #[serde(rename = "negotiation.offers")]
    NegotiationOffers,
    #[serde(rename = "host.info")]
    Info,
    // Identity / custody (CLI only; never sent by the MCP server). Seeds never
    // cross the socket: `id.show` returns only public material.
    #[serde(rename = "id.show")]
    IdShow,

    // Program catalog. `program` is a human handle, unique short-hash prefix, or
    // full 64-hex id, resolved to a `ProgramHash` daemon-side.
    #[serde(rename = "program.list")]
    ProgramList,
    #[serde(rename = "program.get")]
    ProgramGet { program: String },
    #[serde(rename = "program.import")]
    ProgramImport { source: FileSource },
    // Blobs. Paths are on the daemon's filesystem. Import links the file in
    // place: the daemon hashes it once and reads it again only to send or
    // export ranges; the file must stay unchanged while executions use it.
    #[serde(rename = "blob.import")]
    BlobImport { source: FileSource },
    #[serde(rename = "blob.export")]
    BlobExport { hash: BlobHash, path: PathBuf },
    #[serde(rename = "blob.list")]
    BlobList,
    #[serde(rename = "program.remove")]
    ProgramRemove { program: String },

    // Execution. `exec.new` takes a caller-owned request id but no signing key:
    // it runs as the daemon's own identity and returns immediately (negotiation
    // runs in the background).
    // `exec.await` blocks for a lifecycle boundary; `exec.next` blocks
    // server-side until a callout or terminal event.
    #[serde(rename = "exec.new")]
    ExecNew {
        /// Client-owned request identity. Knowing the id before transport lets
        /// the caller clean up a creation whose response is delayed or lost.
        exec_id: ExecId,
        program: String,
        /// Program params as JSON, validated against the program's `params`
        /// schema.
        params: Option<Value>,
        ensemble: EnsembleSpec,
        /// Stored blobs this participant grants the execution to read, by
        /// hash. Each must already be imported.
        #[serde(default)]
        blobs: Vec<BlobHash>,
    },
    #[serde(rename = "exec.list")]
    ExecList,
    #[serde(rename = "exec.status")]
    ExecStatus { exec_id: ExecId },
    /// Read a bounded, host-local diagnostic projection of one execution.
    ///
    /// This is intentionally separate from `exec.status`: the projection may
    /// include durable activation facts and local event-record summaries,
    /// while the status shape remains stable for ordinary callers.
    #[serde(rename = "exec.inspect")]
    ExecInspect {
        exec_id: ExecId,
        /// First event position to include. `None` selects the latest bounded
        /// window, which is appropriate for live inspection UIs.
        events_from: Option<u64>,
        /// Non-zero maximum number of event summaries to return. The daemon
        /// enforces its fixed upper bound before reading the store.
        events_limit: u16,
    },
    #[serde(rename = "exec.await")]
    ExecAwait { exec_id: ExecId, until: AwaitState },
    #[serde(rename = "exec.next")]
    ExecNext { exec_id: ExecId },
    #[serde(rename = "exec.submit")]
    ExecSubmit {
        exec_id: ExecId,
        pending_id: CalloutId,
        /// The answer as JSON, validated against the pending callout's `output`
        /// schema.
        answer: Option<Value>,
    },
    #[serde(rename = "exec.query")]
    ExecQuery {
        exec_id: ExecId,
        /// The query request as JSON, validated against the program's query
        /// request schema.
        query: Option<Value>,
    },
    #[serde(rename = "exec.view")]
    ExecView {
        exec: ExecId,
        width: u16,
        color: ColorDepth,
        /// Render the shared state after this agreed step. `None` renders the
        /// latest state.
        #[serde(default)]
        at_step: Option<u64>,
    },
    #[serde(rename = "exec.trace")]
    ExecTrace { exec_id: ExecId, from: u64, to: u64 },
    /// One page of an execution's local event records from position `from`,
    /// without projecting its status (design §3.3). `limit` is non-zero and
    /// at most the daemon's inspection bound, as for `exec.inspect`.
    #[serde(rename = "exec.records")]
    ExecRecords {
        exec_id: ExecId,
        from: u64,
        limit: u16,
    },
    /// Resolve a full id or a case-insensitive hex prefix of `kind` on this
    /// Host by an indexed lookup. References are trimmed and lowercased;
    /// empty, non-hex or longer-than-64-character references match nothing.
    /// A resident full id wins; ambiguous prefixes return at most eight
    /// candidates in id order and the full match count. Used by CLI, MCP and UI.
    #[serde(rename = "resolve")]
    Resolve { kind: RefKind, reference: String },
    /// Cancel a caller-owned creation when its `exec.new` response is
    /// ambiguous. Unlike `exec.withdraw`, this follows an activation race and
    /// stops the execution before acknowledging cleanup.
    #[serde(rename = "exec.cancel_creation")]
    ExecCancelCreation { exec_id: ExecId },
    #[serde(rename = "exec.withdraw")]
    ExecWithdraw { exec_id: ExecId },
    #[serde(rename = "exec.terminate")]
    ExecTerminate { exec_id: ExecId, reason: String },

    // Event stream. The server acks, then streams `EventFrame`s on the same
    // connection until the client hangs up.
    #[serde(rename = "events.subscribe")]
    EventsSubscribe { filter: EventFilter },
    // Receipts / verification.
    #[serde(rename = "receipt.get")]
    ReceiptGet { receipt: ReceiptRef },
    #[serde(rename = "receipt.import")]
    ReceiptImport { receipt: Box<ReceiptArtifact> },
    #[serde(rename = "receipt.list")]
    ReceiptList,
    #[serde(rename = "receipt.verify")]
    ReceiptVerify { receipt: ReceiptRef },
}

impl Request {
    /// The daemon-level JSON-RPC wire method used to attribute request work.
    #[must_use]
    pub fn method(&self) -> &'static str {
        match self {
            Self::Host { .. } => "host.call",
            Self::DaemonInfo => "daemon.info",
            Self::DaemonStop => "daemon.stop",
            Self::HostsList => "hosts.list",
            Self::HostsOpen { .. } => "hosts.open",
            Self::ActivitySubscribe => "activity.subscribe",
        }
    }
}

impl HostRequest {
    /// The JSON-RPC wire method, also used to attribute work to request spans.
    #[must_use]
    pub fn method(&self) -> &'static str {
        match self {
            Self::NegotiationOffers => "negotiation.offers",
            Self::Info => "host.info",
            Self::IdShow => "id.show",
            Self::ProgramList => "program.list",
            Self::ProgramGet { .. } => "program.get",
            Self::ProgramImport { .. } => "program.import",
            Self::BlobImport { .. } => "blob.import",
            Self::BlobExport { .. } => "blob.export",
            Self::BlobList => "blob.list",
            Self::ProgramRemove { .. } => "program.remove",
            Self::ExecNew { .. } => "exec.new",
            Self::ExecList => "exec.list",
            Self::ExecStatus { .. } => "exec.status",
            Self::ExecInspect { .. } => "exec.inspect",
            Self::ExecAwait { .. } => "exec.await",
            Self::ExecNext { .. } => "exec.next",
            Self::ExecSubmit { .. } => "exec.submit",
            Self::ExecQuery { .. } => "exec.query",
            Self::ExecView { .. } => "exec.view",
            Self::ExecTrace { .. } => "exec.trace",
            Self::ExecRecords { .. } => "exec.records",
            Self::Resolve { .. } => "resolve",
            Self::ExecCancelCreation { .. } => "exec.cancel_creation",
            Self::ExecWithdraw { .. } => "exec.withdraw",
            Self::ExecTerminate { .. } => "exec.terminate",
            Self::EventsSubscribe { .. } => "events.subscribe",
            Self::ReceiptGet { .. } => "receipt.get",
            Self::ReceiptImport { .. } => "receipt.import",
            Self::ReceiptList => "receipt.list",
            Self::ReceiptVerify { .. } => "receipt.verify",
        }
    }
}

/// The state `exec.await` blocks for: session established, or terminal.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum AwaitState {
    /// The session has confirmed and started (execution can run).
    Active,
    /// The execution reached any terminal state (completed, failed, aborted).
    Terminal,
}

/// How `exec.new` starts or joins a negotiation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum EnsembleSpec {
    /// Create an offer and collect exactly this many participants, including
    /// the local Host.
    Create { participant_count: u16 },
    /// Join the first valid offer on the program topic, or one exact offer
    /// when `target` is supplied.
    Join { target: Option<NegotiationTarget> },
}

/// What a `resolve` reference names.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum RefKind {
    /// An execution id (`exec_requests`).
    Exec,
    /// A session id: committed activations, execution aggregates and receipts.
    Session,
    /// A receipt content id.
    Receipt,
}

/// Select exact stored evidence, this Host's session publication, or an inline artifact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum ReceiptRef {
    Produced(SessionHash),
    Stored(arena0_protocol::ReceiptId),
    Inline(Box<ReceiptArtifact>),
}

/// Which program a `ProgramHash` inbound-resolution failure could have meant.
///
/// Not a wire type; the daemon renders these into an [`ApiError`](crate::ApiError)
/// message. Kept here so the resolution vocabulary lives with the request types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramRefError {
    /// No registered program matched the reference.
    NotFound { reference: String },
    /// More than one registered program matched; the caller must disambiguate.
    Ambiguous {
        reference: String,
        candidates: Vec<ProgramHash>,
    },
}

impl std::fmt::Display for ProgramRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { reference } => {
                write!(f, "no program matches '{reference}'")
            }
            Self::Ambiguous {
                reference,
                candidates,
            } => {
                let list = candidates
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "'{reference}' is ambiguous; candidates: {list}; use an exact program ID"
                )
            }
        }
    }
}
