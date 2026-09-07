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
use arena0_protocol::{
    ColorDepth, ExecId, NegotiationId, PeerId, PendingId, ReceiptArtifact, SessionHash,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::events::EventFilter;
/// One request to the daemon's shared Unix endpoint. Host operations always
/// name their target explicitly; daemon operations do not select a Host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", content = "params", deny_unknown_fields)]
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
pub enum HostRequest {
    #[serde(rename = "host.info")]
    Info,
    // Identity / custody (CLI only; never sent by the MCP server). Seeds never
    // cross the socket: `id.new` returns only public material.
    #[serde(rename = "id.new")]
    IdNew { label: Option<String> },
    #[serde(rename = "id.list")]
    IdList,
    #[serde(rename = "id.show")]
    IdShow { id: IdRef },
    #[serde(rename = "id.remove")]
    IdRemove { id: IdRef },

    // Program catalog. `program` is a human handle, unique short-hash prefix, or
    // full 64-hex id, resolved to a `ProgramHash` daemon-side.
    #[serde(rename = "program.list")]
    ProgramList,
    #[serde(rename = "program.get")]
    ProgramGet { program: String },
    #[serde(rename = "program.import")]
    ProgramImport { wasm: Vec<u8> },
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
    },
    #[serde(rename = "exec.list")]
    ExecList,
    #[serde(rename = "exec.status")]
    ExecStatus { exec_id: ExecId },
    /// Read a bounded, host-local diagnostic projection of one execution.
    ///
    /// This is intentionally separate from `exec.status`: the projection may
    /// include durable activation facts and local private-handler summaries,
    /// while the status shape remains stable for ordinary callers.
    #[serde(rename = "exec.inspect")]
    ExecInspect {
        exec_id: ExecId,
        /// First private handler sequence to include. `None` selects the latest
        /// bounded window, which is appropriate for live inspection UIs.
        private_from: Option<u64>,
        /// Non-zero maximum number of private handler summaries to return. The
        /// daemon enforces its fixed upper bound before reading the store.
        private_limit: u16,
    },
    #[serde(rename = "exec.await")]
    ExecAwait { exec_id: ExecId, until: AwaitState },
    #[serde(rename = "exec.next")]
    ExecNext { exec_id: ExecId },
    #[serde(rename = "exec.submit")]
    ExecSubmit {
        exec_id: ExecId,
        pending_id: PendingId,
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
    },
    #[serde(rename = "exec.trace")]
    ExecTrace { exec_id: ExecId, from: u64, to: u64 },
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
    ReceiptVerify { receipt: ReceiptRef, full: bool },
}

/// The state `exec.await` blocks for: session established, or terminal.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AwaitState {
    /// The session has confirmed and started (execution can run).
    Active,
    /// The execution reached any terminal state (completed, failed, aborted).
    Terminal,
}

/// How `exec.new` starts or joins a negotiation. `Explicit` creates one and
/// contacts exactly `peers`; `Join` names the creator and exact negotiation to
/// join.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EnsembleSpec {
    Explicit {
        peers: Vec<PeerId>,
    },
    /// Join one exact negotiation published by `creator`.
    Join {
        creator: PeerId,
        negotiation_id: NegotiationId,
    },
}

/// Reference an identity by `PeerId` or operator label.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum IdRef {
    Peer(PeerId),
    Label(String),
}

/// Select exact stored evidence, this Host's session publication, or an inline artifact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
