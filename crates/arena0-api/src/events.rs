//! The typed daemon event stream.
//!
//! `EventFrame` is the wire envelope. Its `EventData` payload is serialized with
//! an adjacent `kind`/`data` tag while correlation identifiers remain in the
//! envelope. The event vocabulary is specified in `docs/api/events/`.

use arena0_crypto::AgentPubKey;
use arena0_program::{JsonSchemaDocument, ProgramHash};
use arena0_protocol::{
    ExecId, ExecLifecycle, NegotiationId, PeerId, PendingId, SessionHash, StateHash,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{ApiError, ApiErrorCode};

/// Why an execution record was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecOrigin {
    Request,
    Recovery,
}

/// Host failure class projected on `exec.terminated`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionFailureKind {
    Negotiation,
    HostStopped,
    ProgramAborted,
    Runtime,
    InvalidGuestOutput,
}

/// Negotiation machine stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NegotiationStage {
    Gossiping,
    Prepared,
}

/// The terminal payload nested in `exec.session.ended`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum SessionTerminal {
    #[serde(rename = "completed")]
    Completed {
        #[serde(skip_serializing_if = "Option::is_none")]
        outcome: Option<Value>,
    },
    #[serde(rename = "aborted")]
    Aborted { step: u64, reason: String },
}

/// Event-specific payload. The serde tag is flattened into [`EventFrame`],
/// producing top-level `kind` and nested `data` fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", deny_unknown_fields)]
pub enum EventData {
    #[serde(rename = "host.started")]
    HostStarted {
        version: String,
        peer_id: PeerId,
        transport_key: AgentPubKey,
        socket: String,
        abi_version: u32,
    },
    #[serde(rename = "host.stopped")]
    HostStopped {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        uptime_secs: u64,
    },
    #[serde(rename = "negotiation.offer_seen")]
    OfferSeen {
        program_id: ProgramHash,
        negotiation_id: NegotiationId,
        creator: PeerId,
        offer_seq: u64,
    },
    #[serde(rename = "exec.created")]
    Created {
        program_id: ProgramHash,
        #[serde(skip_serializing_if = "Option::is_none")]
        negotiation_id: Option<NegotiationId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        queue_position: Option<usize>,
        origin: ExecOrigin,
    },
    #[serde(rename = "exec.terminated")]
    Terminated {
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        failed_class: Option<ExecutionFailureKind>,
    },
    #[serde(rename = "exec.negotiation.started")]
    NegotiationStarted { target_size: u16 },
    #[serde(rename = "exec.negotiation.offer_accepted")]
    NegotiationOfferAccepted { creator: PeerId, offer_seq: u64 },
    #[serde(rename = "exec.negotiation.ticket_accepted")]
    NegotiationTicketAccepted {
        participant: PeerId,
        ticket_hash: arena0_protocol::TicketHash,
        ticket_count: u16,
        target_size: u16,
    },
    #[serde(rename = "exec.negotiation.peers")]
    NegotiationPeers {
        lifecycle: ExecLifecycle,
        peers: Vec<PeerId>,
    },
    #[serde(rename = "exec.negotiation.prepared")]
    NegotiationPrepared { participants: u16 },
    #[serde(rename = "exec.negotiation.resumed")]
    NegotiationResumed { participants: u16 },
    #[serde(rename = "exec.negotiation.committed")]
    NegotiationCommitted { participants: u16 },
    #[serde(rename = "exec.negotiation.retried")]
    NegotiationRetried {
        attempt: u64,
        stage: NegotiationStage,
        ticket_count: u16,
        sig_count: u16,
        target_size: u16,
    },
    #[serde(rename = "exec.negotiation.rejoined")]
    NegotiationRejoined {},
    #[serde(rename = "exec.negotiation.timed_out")]
    NegotiationTimedOut {
        stage: NegotiationStage,
        ticket_count: u16,
        sig_count: u16,
        target_size: u16,
    },
    #[serde(rename = "exec.session.started")]
    SessionStarted { ensemble: Vec<PeerId> },
    #[serde(rename = "exec.session.callout")]
    SessionCallout {
        pending_id: PendingId,
        callout_index: u32,
        name: String,
        prompt: String,
        schema: JsonSchemaDocument,
        context: Value,
    },
    #[serde(rename = "exec.session.callout_answered")]
    SessionCalloutAnswered { pending_id: PendingId },
    #[serde(rename = "exec.session.step")]
    SessionStep {
        step: u64,
        pre_state: StateHash,
        post_state: StateHash,
        fuel_used: u64,
        signers: u16,
        participants: u16,
    },
    #[serde(rename = "exec.session.ended")]
    SessionEnded { terminal: SessionTerminal },
    #[serde(rename = "stream.lagged")]
    Lagged { skipped: u64 },
}

impl EventData {
    /// The closed dotted wire tag for this payload.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::HostStarted { .. } => "host.started",
            Self::HostStopped { .. } => "host.stopped",
            Self::OfferSeen { .. } => "negotiation.offer_seen",
            Self::Created { .. } => "exec.created",
            Self::Terminated { .. } => "exec.terminated",
            Self::NegotiationStarted { .. } => "exec.negotiation.started",
            Self::NegotiationOfferAccepted { .. } => "exec.negotiation.offer_accepted",
            Self::NegotiationTicketAccepted { .. } => "exec.negotiation.ticket_accepted",
            Self::NegotiationPeers { .. } => "exec.negotiation.peers",
            Self::NegotiationPrepared { .. } => "exec.negotiation.prepared",
            Self::NegotiationResumed { .. } => "exec.negotiation.resumed",
            Self::NegotiationCommitted { .. } => "exec.negotiation.committed",
            Self::NegotiationRetried { .. } => "exec.negotiation.retried",
            Self::NegotiationRejoined { .. } => "exec.negotiation.rejoined",
            Self::NegotiationTimedOut { .. } => "exec.negotiation.timed_out",
            Self::SessionStarted { .. } => "exec.session.started",
            Self::SessionCallout { .. } => "exec.session.callout",
            Self::SessionCalloutAnswered { .. } => "exec.session.callout_answered",
            Self::SessionStep { .. } => "exec.session.step",
            Self::SessionEnded { .. } => "exec.session.ended",
            Self::Lagged { .. } => "stream.lagged",
        }
    }

    #[must_use]
    fn is_exec_scoped(&self) -> bool {
        self.kind().starts_with("exec.")
    }

    #[must_use]
    const fn requires_session_id(&self) -> bool {
        matches!(
            self,
            Self::NegotiationPrepared { .. }
                | Self::NegotiationResumed { .. }
                | Self::NegotiationCommitted { .. }
                | Self::SessionStarted { .. }
                | Self::SessionCallout { .. }
                | Self::SessionCalloutAnswered { .. }
                | Self::SessionStep { .. }
                | Self::SessionEnded { .. }
        )
    }

    #[must_use]
    const fn allows_session_id(&self) -> bool {
        self.requires_session_id() || matches!(self, Self::Terminated { .. })
    }
}

/// One pushed event on a subscription stream.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventFrame {
    pub host: String,
    pub boot_id: String,
    pub seq: u64,
    pub ts: u64,
    #[serde(flatten)]
    pub data: EventData,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec_id: Option<ExecId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionHash>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEventFrame {
    host: String,
    boot_id: String,
    seq: u64,
    ts: u64,
    #[serde(flatten)]
    data: EventData,
    #[serde(default)]
    exec_id: Option<ExecId>,
    #[serde(default)]
    session_id: Option<SessionHash>,
}

impl EventFrame {
    /// Construct a frame while enforcing its event/correlation contract.
    pub fn new(
        host: impl Into<String>,
        boot_id: impl Into<String>,
        seq: u64,
        ts: u64,
        data: EventData,
        exec_id: Option<ExecId>,
        session_id: Option<SessionHash>,
    ) -> Result<Self, ApiError> {
        let frame = Self {
            host: host.into(),
            boot_id: boot_id.into(),
            seq,
            ts,
            data,
            exec_id,
            session_id,
        };
        frame.validate_correlations()?;
        Ok(frame)
    }

    /// Validate the frame's correlation fields.
    pub fn validate_correlations(&self) -> Result<(), ApiError> {
        let expects_exec = self.data.is_exec_scoped();
        if self.exec_id.is_some() != expects_exec {
            return Err(ApiError::new(
                ApiErrorCode::BadRequest,
                format!(
                    "event {} requires exec_id={} correlation",
                    self.data.kind(),
                    expects_exec
                ),
            ));
        }
        if self.data.requires_session_id() && self.session_id.is_none() {
            return Err(ApiError::new(
                ApiErrorCode::BadRequest,
                format!("event {} requires session_id correlation", self.data.kind()),
            ));
        }
        if !self.data.allows_session_id() && self.session_id.is_some() {
            return Err(ApiError::new(
                ApiErrorCode::BadRequest,
                format!(
                    "event {} does not allow session_id correlation",
                    self.data.kind()
                ),
            ));
        }
        Ok(())
    }

    /// The closed dotted tag.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        self.data.kind()
    }
}

impl<'de> Deserialize<'de> for EventFrame {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawEventFrame::deserialize(deserializer)?;
        Self::new(
            raw.host,
            raw.boot_id,
            raw.seq,
            raw.ts,
            raw.data,
            raw.exec_id,
            raw.session_id,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// What a subscriber wants to watch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EventFilter {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEventFilter {
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
}

impl<'de> Deserialize<'de> for EventFilter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawEventFilter::deserialize(deserializer)?;
        Self::try_new(raw.include, raw.exclude).map_err(serde::de::Error::custom)
    }
}

impl EventFilter {
    /// Build and validate an event filter.
    pub fn try_new(include: Vec<String>, exclude: Vec<String>) -> Result<Self, ApiError> {
        let filter = Self { include, exclude };
        filter.validate()?;
        Ok(filter)
    }

    /// Validate all patterns, returning the offending pattern in the error.
    pub fn validate(&self) -> Result<(), ApiError> {
        self.include
            .iter()
            .chain(self.exclude.iter())
            .try_for_each(|pattern| validate_pattern(pattern))
    }

    /// Whether a frame passes this filter. Delivery controls bypass filters.
    #[must_use]
    pub fn matches(&self, frame: &EventFrame) -> bool {
        if matches!(frame.kind(), "host.started" | "stream.lagged") {
            return true;
        }
        let included = self.include.is_empty()
            || self
                .include
                .iter()
                .any(|pattern| pattern_matches(pattern, frame.kind()));
        included
            && !self
                .exclude
                .iter()
                .any(|pattern| pattern_matches(pattern, frame.kind()))
    }
}

const CATALOG: &[&str] = &[
    "host.started",
    "host.stopped",
    "negotiation.offer_seen",
    "exec.created",
    "exec.terminated",
    "exec.negotiation.started",
    "exec.negotiation.offer_accepted",
    "exec.negotiation.ticket_accepted",
    "exec.negotiation.peers",
    "exec.negotiation.prepared",
    "exec.negotiation.resumed",
    "exec.negotiation.committed",
    "exec.negotiation.retried",
    "exec.negotiation.rejoined",
    "exec.negotiation.timed_out",
    "exec.session.started",
    "exec.session.callout",
    "exec.session.callout_answered",
    "exec.session.step",
    "exec.session.ended",
    "stream.lagged",
];

fn invalid_pattern(pattern: &str, reason: &str) -> ApiError {
    ApiError::new(
        ApiErrorCode::BadRequest,
        format!("invalid event filter pattern {pattern:?}: {reason}"),
    )
}

fn valid_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn validate_pattern(pattern: &str) -> Result<(), ApiError> {
    if pattern.is_empty() {
        return Err(invalid_pattern(pattern, "pattern is empty"));
    }
    let segments: Vec<&str> = pattern.split('.').collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        return Err(invalid_pattern(pattern, "empty segments are not allowed"));
    }
    if segments.len() > 3 {
        return Err(invalid_pattern(
            pattern,
            "patterns may have at most three segments",
        ));
    }
    if segments.len() < 2 {
        return Err(invalid_pattern(
            pattern,
            "bare domains and bare * are not patterns",
        ));
    }

    let wildcard = segments.last() == Some(&"*");
    let prefix = if wildcard {
        &segments[..segments.len() - 1]
    } else {
        &segments[..]
    };
    if prefix.iter().any(|segment| !valid_segment(segment)) {
        return Err(invalid_pattern(
            pattern,
            "segments contain invalid characters",
        ));
    }
    if wildcard {
        if !CATALOG.iter().any(|kind| pattern_matches(pattern, kind)) {
            return Err(invalid_pattern(pattern, "unknown domain or subdomain"));
        }
    } else {
        if !CATALOG.contains(&pattern) {
            return Err(invalid_pattern(pattern, "unknown event tag"));
        }
    }
    Ok(())
}

fn pattern_matches(pattern: &str, kind: &str) -> bool {
    pattern.strip_suffix(".*").map_or_else(
        || kind == pattern,
        |prefix| {
            kind.strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('.'))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> ExecId {
        ExecId([byte; 32])
    }

    fn session(byte: u8) -> SessionHash {
        SessionHash([byte; 32])
    }

    fn frame(
        data: EventData,
        exec_id: Option<ExecId>,
        session_id: Option<SessionHash>,
    ) -> EventFrame {
        EventFrame::new("host-01", "boot-00", 1, 2, data, exec_id, session_id).unwrap()
    }

    fn all_frames() -> Vec<EventFrame> {
        let peer = PeerId([2; 32]);
        let program = ProgramHash([3; 32]);
        let negotiation = NegotiationId([4; 32]);
        let state = StateHash([5; 32]);
        let schema = JsonSchemaDocument::for_type::<String>();
        let exec = id(1);
        let sid = session(6);
        vec![
            frame(
                EventData::HostStarted {
                    version: "0.1.0".into(),
                    peer_id: peer,
                    transport_key: AgentPubKey([7; 32]),
                    socket: "/tmp/a".into(),
                    abi_version: 19,
                },
                None,
                None,
            ),
            frame(
                EventData::HostStopped {
                    reason: None,
                    uptime_secs: 4,
                },
                None,
                None,
            ),
            frame(
                EventData::OfferSeen {
                    program_id: program,
                    negotiation_id: negotiation,
                    creator: peer,
                    offer_seq: 1,
                },
                None,
                None,
            ),
            frame(
                EventData::Created {
                    program_id: program,
                    negotiation_id: Some(negotiation),
                    queue_position: None,
                    origin: ExecOrigin::Request,
                },
                Some(exec),
                None,
            ),
            frame(
                EventData::Terminated {
                    reason: "failed".into(),
                    failed_class: None,
                },
                Some(exec),
                None,
            ),
            frame(
                EventData::NegotiationStarted { target_size: 2 },
                Some(exec),
                None,
            ),
            frame(
                EventData::NegotiationOfferAccepted {
                    creator: peer,
                    offer_seq: 1,
                },
                Some(exec),
                None,
            ),
            frame(
                EventData::NegotiationTicketAccepted {
                    participant: peer,
                    ticket_hash: arena0_protocol::TicketHash([8; 32]),
                    ticket_count: 1,
                    target_size: 2,
                },
                Some(exec),
                None,
            ),
            frame(
                EventData::NegotiationPeers {
                    lifecycle: ExecLifecycle::Negotiating,
                    peers: vec![peer],
                },
                Some(exec),
                None,
            ),
            frame(
                EventData::NegotiationPrepared { participants: 2 },
                Some(exec),
                Some(sid),
            ),
            frame(
                EventData::NegotiationResumed { participants: 2 },
                Some(exec),
                Some(sid),
            ),
            frame(
                EventData::NegotiationCommitted { participants: 2 },
                Some(exec),
                Some(sid),
            ),
            frame(
                EventData::NegotiationRetried {
                    attempt: 1,
                    stage: NegotiationStage::Prepared,
                    ticket_count: 1,
                    sig_count: 1,
                    target_size: 2,
                },
                Some(exec),
                None,
            ),
            frame(EventData::NegotiationRejoined {}, Some(exec), None),
            frame(
                EventData::NegotiationTimedOut {
                    stage: NegotiationStage::Gossiping,
                    ticket_count: 1,
                    sig_count: 0,
                    target_size: 2,
                },
                Some(exec),
                None,
            ),
            frame(
                EventData::SessionStarted {
                    ensemble: vec![peer],
                },
                Some(exec),
                Some(sid),
            ),
            frame(
                EventData::SessionCallout {
                    pending_id: PendingId::new(1),
                    callout_index: 0,
                    name: "Ask".into(),
                    prompt: "?".into(),
                    schema,
                    context: Value::Null,
                },
                Some(exec),
                Some(sid),
            ),
            frame(
                EventData::SessionCalloutAnswered {
                    pending_id: PendingId::new(1),
                },
                Some(exec),
                Some(sid),
            ),
            frame(
                EventData::SessionStep {
                    step: 1,
                    pre_state: state,
                    post_state: state,
                    fuel_used: 1,
                    signers: 1,
                    participants: 1,
                },
                Some(exec),
                Some(sid),
            ),
            frame(
                EventData::SessionEnded {
                    terminal: SessionTerminal::Completed { outcome: None },
                },
                Some(exec),
                Some(sid),
            ),
            frame(EventData::Lagged { skipped: 1 }, None, None),
        ]
    }

    #[test]
    fn all_21_variants_round_trip_with_adjacent_data() {
        let frames = all_frames();
        assert_eq!(frames.len(), 21);
        for frame in frames {
            let json = serde_json::to_value(&frame).unwrap();
            assert!(json.get("kind").and_then(Value::as_str).is_some());
            assert!(json.get("data").is_some());
            assert_eq!(json.get("host").unwrap(), "host-01");
            assert_eq!(json.get("boot_id").unwrap(), "boot-00");
            let decoded: EventFrame = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(decoded, frame);
            assert!(!json["data"].as_object().unwrap().contains_key("exec_id"));
            assert!(!json["data"].as_object().unwrap().contains_key("session_id"));
        }
    }

    #[test]
    fn event_pending_ids_are_decimal_strings() {
        let pending_id = PendingId::new(u64::MAX);
        let frame = frame(
            EventData::SessionCalloutAnswered { pending_id },
            Some(id(1)),
            Some(session(2)),
        );
        let json = serde_json::to_value(&frame).expect("event JSON");
        assert_eq!(json["data"]["pending_id"], pending_id.to_string());
        assert_eq!(serde_json::from_value::<EventFrame>(json).unwrap(), frame);
    }

    #[test]
    fn optional_fields_are_omitted() {
        let frame = frame(
            EventData::Created {
                program_id: ProgramHash([1; 32]),
                negotiation_id: None,
                queue_position: None,
                origin: ExecOrigin::Request,
            },
            Some(id(1)),
            None,
        );
        let json = serde_json::to_value(frame).unwrap();
        assert!(json["data"].get("negotiation_id").is_none());
        assert!(json["data"].get("queue_position").is_none());
        assert!(json.get("session_id").is_none());
    }

    #[test]
    fn correlation_is_enforced_by_constructor_and_deserializer() {
        let data = EventData::NegotiationStarted { target_size: 2 };
        assert!(EventFrame::new("n", "b", 1, 2, data.clone(), None, None).is_err());
        assert!(
            EventFrame::new("n", "b", 1, 2, data.clone(), Some(id(1)), Some(session(2))).is_err()
        );
        let terminated = EventData::Terminated {
            reason: "runtime".into(),
            failed_class: Some(ExecutionFailureKind::Runtime),
        };
        assert!(EventFrame::new("n", "b", 1, 2, terminated, Some(id(1)), Some(session(2))).is_ok());
        let frame = frame(data, Some(id(1)), None);
        let mut json = serde_json::to_value(frame).unwrap();
        json["exec_id"] = Value::Null;
        assert!(serde_json::from_value::<EventFrame>(json).is_err());
    }

    #[test]
    fn event_payloads_reject_unknown_fields() {
        let frame = frame(
            EventData::SessionStep {
                step: 1,
                pre_state: StateHash([1; 32]),
                post_state: StateHash([2; 32]),
                fuel_used: 3,
                signers: 2,
                participants: 2,
            },
            Some(id(1)),
            Some(session(2)),
        );
        let mut agreed = serde_json::to_value(&frame).unwrap();
        agreed["data"]["agreed"] = Value::Bool(true);
        assert!(serde_json::from_value::<EventFrame>(agreed).is_err());

        let mut nested_correlation = serde_json::to_value(frame).unwrap();
        nested_correlation["data"]["exec_id"] = Value::String("01".repeat(32));
        assert!(serde_json::from_value::<EventFrame>(nested_correlation).is_err());
    }

    #[test]
    fn host_event_wire_has_no_node_compatibility_shape() {
        let started_frame = frame(
            EventData::HostStarted {
                version: "0.1.0".into(),
                peer_id: PeerId([1; 32]),
                transport_key: AgentPubKey([2; 32]),
                socket: "/tmp/arena0.sock".into(),
                abi_version: 1,
            },
            None,
            None,
        );
        let mut legacy_frame = serde_json::to_value(&started_frame).unwrap();
        legacy_frame["node"] = Value::String("host-01".into());
        assert!(serde_json::from_value::<EventFrame>(legacy_frame).is_err());
        assert!(EventFilter::try_new(vec!["node.started".into()], vec![]).is_err());

        let failure = EventData::Terminated {
            reason: "host stopped".into(),
            failed_class: Some(ExecutionFailureKind::HostStopped),
        };
        let failure_frame = frame(failure, Some(id(1)), None);
        let failure_json = serde_json::to_value(failure_frame).unwrap();
        assert_eq!(failure_json["data"]["failed_class"], "host_stopped");
    }

    #[test]
    fn subtree_filters_and_controls_work() {
        let started = frame(
            EventData::SessionStarted { ensemble: vec![] },
            Some(id(1)),
            Some(session(2)),
        );
        let step = frame(
            EventData::SessionStep {
                step: 1,
                pre_state: StateHash([1; 32]),
                post_state: StateHash([2; 32]),
                fuel_used: 1,
                signers: 1,
                participants: 1,
            },
            Some(id(1)),
            Some(session(2)),
        );
        let negotiation = frame(
            EventData::NegotiationStarted { target_size: 2 },
            Some(id(1)),
            None,
        );
        let root = frame(
            EventData::Created {
                program_id: ProgramHash([1; 32]),
                negotiation_id: None,
                queue_position: None,
                origin: ExecOrigin::Request,
            },
            Some(id(1)),
            None,
        );
        let host = frame(
            EventData::HostStopped {
                reason: None,
                uptime_secs: 1,
            },
            None,
            None,
        );
        let lag = frame(EventData::Lagged { skipped: 1 }, None, None);
        let filter =
            EventFilter::try_new(vec!["exec.*".into()], vec!["exec.session.*".into()]).unwrap();
        assert!(filter.matches(&root));
        assert!(filter.matches(&negotiation));
        assert!(!filter.matches(&started));
        assert!(!filter.matches(&step));
        assert!(!filter.matches(&host));
        assert!(filter.matches(&lag));
    }

    #[test]
    fn filter_validation_rejects_every_malformed_shape() {
        let bad = [
            "*",
            "exec",
            "exec.negotiation",
            "exec.neg*",
            "exec.*.ended",
            "exec.session.step*",
            "",
            ".exec.created",
            "exec.created.",
            "exec..created",
            "EXEC.*",
            "exec/session/*",
            "exec.foo.*",
            "exec.negotiation.started.extra",
            "unknown.event",
        ];
        for pattern in bad {
            let error = EventFilter::try_new(vec![pattern.into()], vec![]).unwrap_err();
            assert_eq!(error.code, ApiErrorCode::BadRequest);
            assert!(
                error.message.contains(pattern),
                "{pattern}: {}",
                error.message
            );
        }
    }

    #[test]
    fn filter_deserialization_validates_patterns() {
        let value = serde_json::json!({"include": ["exec.*"], "exclude": ["exec.session.*"]});
        let filter: EventFilter = serde_json::from_value(value).unwrap();
        assert_eq!(filter.include, ["exec.*"]);
        let bad = serde_json::json!({"include": ["exec.*.ended"], "exclude": []});
        assert!(serde_json::from_value::<EventFilter>(bad).is_err());
    }
}
