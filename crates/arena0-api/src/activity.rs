//! Operational MCP tool activity.
//!
//! Activity is deliberately separate from the semantic [`crate::EventFrame`]
//! stream. It records only tool lifecycle and bounded result classes so a
//! monitor can follow liveness without receiving tool arguments, answers,
//! outcomes, or other private payloads.

use arena0_protocol::ExecId;
use serde::{Deserialize, Serialize};

use crate::ApiErrorCode;

/// One daemon-scoped operational activity frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityFrame {
    pub boot_id: String,
    pub seq: u64,
    pub ts: u64,
    #[serde(flatten)]
    pub data: ActivityData,
}

/// One lifecycle or stream-control activity record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", deny_unknown_fields)]
pub enum ActivityData {
    #[serde(rename = "started")]
    Started {
        call_id: String,
        tool: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        host: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        exec_id: Option<ExecId>,
    },
    #[serde(rename = "finished")]
    Finished {
        call_id: String,
        elapsed_ms: u64,
        result: ActivityResult,
    },
    #[serde(rename = "lagged")]
    Lagged { skipped: u64 },
}

/// Result class for one completed MCP tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", deny_unknown_fields)]
pub enum ActivityResult {
    #[serde(rename = "ok")]
    Ok,
    #[serde(rename = "tool_error")]
    ToolError {
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<ApiErrorCode>,
    },
    #[serde(rename = "interrupted")]
    Interrupted,
}
