//! Host-guest ABI contract: version, module name, and import names.

use std::io;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::bounded;
use crate::{LocalStateBytes, SharedStateBytes};

use crate::Capability;

/// Current ABI version. A sandbox rejects modules declaring a different one.
pub const ABI_VERSION: u32 = 22;

/// Wasm import module name for all arena0 host functions.
pub const HOST_MODULE: &str = "arena0";

/// Maximum bytes in one semantic call payload.
pub const MAX_CALL_PAYLOAD_BYTES: usize = crate::profile::MAX_INPUT_BYTES as usize;
/// Maximum bytes in the committed session context carried into a dispatch.
pub const MAX_SESSION_CONTEXT_BYTES: usize = 1024 * 1024;
/// Maximum UTF-8 bytes returned as the reason for a rejected input dispatch.
pub const MAX_REJECTION_REASON_BYTES: usize = 1024;
/// Maximum bytes in the JSON context of one derived open callout.
pub const MAX_CALLOUT_CONTEXT_BYTES: usize = 64 * 1024;
/// Host bytes a synchronous `sign` call adds around the guest payload: the
/// `GuestSignData` header, the two `Vec<u8>` length prefixes of the returned
/// `(signed_bytes, signature)` pair, and a 64-byte Ed25519 signature. A guest
/// allocates `payload.len() + SIGN_RESULT_OVERHEAD_BYTES` for the result.
pub const SIGN_RESULT_OVERHEAD_BYTES: usize = 256;

/// Bounded, complete JSON bytes at an agent-facing request or projection boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonBytes(Vec<u8>);

impl JsonBytes {
    /// Construct validated JSON bytes under the request/projection bound.
    pub fn try_new(bytes: impl Into<Vec<u8>>) -> Result<Self, JsonBytesError> {
        let bytes = bytes.into();
        if bytes.len() > MAX_CALL_PAYLOAD_BYTES {
            return Err(JsonBytesError::TooLarge {
                actual: bytes.len(),
                max: MAX_CALL_PAYLOAD_BYTES,
            });
        }
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|error| JsonBytesError::Invalid(error.to_string()))?;
        Ok(Self(bytes))
    }

    /// Borrow the validated JSON bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Return the number of validated JSON bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Return whether the validated JSON value has no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Consume this value and return the validated bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl BorshSerialize for JsonBytes {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        bounded::write_bytes::<MAX_CALL_PAYLOAD_BYTES>(&self.0, writer)
    }
}

impl BorshDeserialize for JsonBytes {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let bytes = bounded::read_bytes::<MAX_CALL_PAYLOAD_BYTES>(reader)?;
        Self::try_new(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

impl serde::Serialize for JsonBytes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for JsonBytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = <Vec<u8> as serde::Deserialize>::deserialize(deserializer)?;
        Self::try_new(bytes).map_err(serde::de::Error::custom)
    }
}

/// Failure to construct bounded JSON projection bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JsonBytesError {
    /// The projection exceeded the byte limit.
    #[error("JSON projection is {actual} bytes; maximum is {max}")]
    TooLarge { actual: usize, max: usize },
    /// The projection was not one complete JSON value.
    #[error("invalid JSON projection: {0}")]
    Invalid(String),
}

/// Bounded opaque stock-Borsh bytes for one terminal outcome DTO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeBytes(Vec<u8>);

impl OutcomeBytes {
    /// Construct outcome bytes under the projection bound.
    pub fn try_new(bytes: impl Into<Vec<u8>>) -> Result<Self, OutcomeBytesError> {
        let bytes = bytes.into();
        if bytes.len() > MAX_CALL_PAYLOAD_BYTES {
            return Err(OutcomeBytesError::TooLarge {
                actual: bytes.len(),
                max: MAX_CALL_PAYLOAD_BYTES,
            });
        }
        Ok(Self(bytes))
    }

    /// Borrow the stock-Borsh outcome bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Consume this value and return the bounded outcome bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

/// Failure to construct bounded terminal outcome bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OutcomeBytesError {
    /// The outcome exceeded the byte limit.
    #[error("Borsh outcome is {actual} bytes; maximum is {max}")]
    TooLarge { actual: usize, max: usize },
}

/// Export names in the resident-instance guest ABI.
pub mod exports {
    /// The guest's work (linear) memory.
    pub const WORK_MEMORY: &str = "memory";
    /// The `i32` global holding the guest's ABI version.
    pub const ABI_VERSION: &str = "arena0_abi_version";
    /// Guest allocation entry point.
    pub const ALLOC: &str = "arena0_alloc";
    /// Guest deallocation entry point.
    pub const DEALLOC: &str = "arena0_dealloc";
    /// Prepare the guest allocator before a resident baseline is captured.
    pub const PREPARE: &str = "arena0_prepare";
    /// Initialize a program's state before a session starts.
    pub const INITIALIZE: &str = "arena0_initialize";
    /// Dispatch one session event against the resident state memories.
    pub const DISPATCH: &str = "arena0_dispatch";
    /// Canonical replicated-state memory export.
    pub const SHARED_MEMORY: &str = "arena0_shared";
    /// Canonical participant-local-state memory export.
    pub const LOCAL_MEMORY: &str = "arena0_local";
    /// Produce the agent-facing terminal outcome.
    pub const OUTCOME: &str = "arena0_outcome";
    /// Select the sole participant eligible to author the next program message.
    pub const WRITER: &str = "arena0_writer";
    /// Answer one agent-facing query.
    pub const QUERY: &str = "arena0_query";
    /// Produce one viewport projection.
    pub const VIEW: &str = "arena0_view";
    /// Return the embedded program definition.
    pub const METADATA: &str = "arena0_metadata";
}

/// The state memory selected by a bounded host state-I/O import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum StateMemoryKind {
    /// The replicated state memory.
    Shared = 0,
    /// The participant-local state memory.
    Local = 1,
}

impl TryFrom<u32> for StateMemoryKind {
    type Error = io::Error;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Shared),
            1 => Ok(Self::Local),
            value => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown state memory kind {value}"),
            )),
        }
    }
}

/// Whether a mutating guest call accepted its event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum CallStatus {
    /// The event produced a replacement state.
    Accepted,
    /// The event was rejected without a state transition.
    Rejected,
}

impl CallStatus {
    /// Stable version-1 Borsh tag.
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Self::Accepted => 0x00,
            Self::Rejected => 0x01,
        }
    }

    /// Decode a stable version-1 Borsh tag.
    pub fn from_tag(tag: u8) -> io::Result<Self> {
        match tag {
            0x00 => Ok(Self::Accepted),
            0x01 => Ok(Self::Rejected),
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown call status tag {tag}"),
            )),
        }
    }
}

impl serde::Serialize for CallStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(self.tag())
    }
}

impl<'de> serde::Deserialize<'de> for CallStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::from_tag(<u8 as serde::Deserialize>::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

/// Failure to construct a call-specific ABI value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AbiEnvelopeError {
    /// A variable field exceeded its ABI bound.
    #[error("{field} is {actual} bytes; maximum is {max}")]
    FieldTooLarge {
        /// Name of the field that exceeded the bound.
        field: &'static str,
        /// Actual field length.
        actual: usize,
        /// Maximum accepted field length.
        max: usize,
    },
}

/// A fresh initialization input. It carries only agent parameter JSON bytes.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct InitInput {
    /// Stock-Serde JSON encoding of the program's parameter DTO.
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_CALL_PAYLOAD_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_CALL_PAYLOAD_BYTES>"
    )]
    pub params: Vec<u8>,
}

impl InitInput {
    /// Construct an initialization input after checking its field bound.
    pub fn try_new(params: Vec<u8>) -> Result<Self, AbiEnvelopeError> {
        ensure_field("params", &params, MAX_CALL_PAYLOAD_BYTES)?;
        Ok(Self { params })
    }
}

/// Initialization output containing both freshly initialized state values.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct InitializedState {
    /// Initial replicated state bytes.
    pub shared: SharedStateBytes,
    /// Initial participant-local state bytes.
    pub local: LocalStateBytes,
}

/// One resident-program dispatch input.
///
/// State is deliberately absent: the host stores the committed values in the
/// exported state memories and restores those memories around failure
/// boundaries. The session and event fields are serialized protocol values,
/// rather than host-owned DTOs, so this ABI remains independent of the
/// concrete program's message type.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct DispatchInput {
    /// Local participant identity in the protocol's canonical 32-byte form.
    pub peer_id: [u8; 32],
    /// Borsh encoding of the committed session ensemble.
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_SESSION_CONTEXT_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_SESSION_CONTEXT_BYTES>"
    )]
    pub session: Vec<u8>,
    /// Borsh encoding of one flat protocol `Event<Vec<u8>>`.
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_CALL_PAYLOAD_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_CALL_PAYLOAD_BYTES>"
    )]
    pub event: Vec<u8>,
}

impl DispatchInput {
    /// Construct a dispatch input after checking its variable-field bounds.
    pub fn try_new(
        peer_id: [u8; 32],
        session: Vec<u8>,
        event: Vec<u8>,
    ) -> Result<Self, AbiEnvelopeError> {
        ensure_field("session context", &session, MAX_SESSION_CONTEXT_BYTES)?;
        ensure_field("event", &event, MAX_CALL_PAYLOAD_BYTES)?;
        Ok(Self {
            peer_id,
            session,
            event,
        })
    }
}

/// The one open callout derived from program state after an accepted dispatch.
///
/// The program returns at most one request per dispatch; the host stores it
/// with the resulting state image and never treats it as a lock. `context` is
/// the stock-Serde JSON projection of the typed callout request.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct CalloutRequest {
    /// Program-local callout variant index.
    pub callout_index: u32,
    /// Agent-facing JSON context for that callout.
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_CALLOUT_CONTEXT_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_CALLOUT_CONTEXT_BYTES>"
    )]
    pub context: Vec<u8>,
}

/// The only value returned in the guest result envelope for a mutating
/// dispatch. Effects remain in the host's per-call effect queue and are never
/// duplicated in guest-owned state bytes.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize)]
pub struct DispatchOutput {
    /// Whether the event was accepted or deterministically rejected.
    pub status: CallStatus,
    /// Bounded program reason for an input rejection. Accepted dispatches and
    /// deterministic peer-message rejections do not carry a reason.
    #[borsh(
        serialize_with = "bounded::write_option_string::<MAX_REJECTION_REASON_BYTES>",
        deserialize_with = "bounded::read_option_string::<MAX_REJECTION_REASON_BYTES>"
    )]
    pub reason: Option<String>,
    /// The single open callout derived from the accepted post-state, if any.
    /// A rejected dispatch has no callout.
    pub callout: Option<CalloutRequest>,
}

#[derive(BorshDeserialize)]
struct DispatchOutputRaw {
    status: CallStatus,
    #[borsh(
        serialize_with = "bounded::write_option_string::<MAX_REJECTION_REASON_BYTES>",
        deserialize_with = "bounded::read_option_string::<MAX_REJECTION_REASON_BYTES>"
    )]
    reason: Option<String>,
    callout: Option<CalloutRequest>,
}

impl BorshDeserialize for DispatchOutput {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let raw = DispatchOutputRaw::deserialize_reader(reader)?;
        if raw.status == CallStatus::Accepted && raw.reason.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "accepted dispatch cannot carry a rejection reason",
            ));
        }
        if raw.status == CallStatus::Rejected && raw.callout.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "rejected dispatch cannot carry an open callout",
            ));
        }
        Ok(Self {
            status: raw.status,
            reason: raw.reason,
            callout: raw.callout,
        })
    }
}

/// Read-only query input. It contains no peer identity or participant-local
/// state, and the index is echoed by [`QueryOutput`] to bind schema validation.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct QueryInput {
    /// Explicit replicated state bytes.
    pub shared: SharedStateBytes,
    /// Opaque Borsh committed `Ensemble` session context.
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_SESSION_CONTEXT_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_SESSION_CONTEXT_BYTES>"
    )]
    pub session: Vec<u8>,
    /// Host-checked index into the advertised query schema list.
    pub query_index: u32,
    /// Stock-Serde JSON query DTO bytes.
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_CALL_PAYLOAD_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_CALL_PAYLOAD_BYTES>"
    )]
    pub query: Vec<u8>,
}

impl QueryInput {
    /// Construct a query input after checking its variable fields.
    pub fn try_new(
        shared: SharedStateBytes,
        session: Vec<u8>,
        query_index: u32,
        query: Vec<u8>,
    ) -> Result<Self, AbiEnvelopeError> {
        ensure_field("session context", &session, MAX_SESSION_CONTEXT_BYTES)?;
        ensure_field("query", &query, MAX_CALL_PAYLOAD_BYTES)?;
        Ok(Self {
            shared,
            session,
            query_index,
            query,
        })
    }
}

/// Read-only query result. It contains no state or status.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct QueryOutput {
    /// The exact query index supplied by the host.
    pub query_index: u32,
    /// Stock-Serde JSON response DTO bytes.
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_CALL_PAYLOAD_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_CALL_PAYLOAD_BYTES>"
    )]
    pub json: Vec<u8>,
}

/// Read-only viewport input. The viewport is the current stock-Serde JSON
/// encoding of the protocol's `Viewport` DTO, carried as opaque bytes here.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct ViewInput {
    /// Explicit replicated state bytes.
    pub shared: SharedStateBytes,
    /// Opaque Borsh committed `Ensemble` session context.
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_SESSION_CONTEXT_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_SESSION_CONTEXT_BYTES>"
    )]
    pub session: Vec<u8>,
    /// Stock-Serde JSON viewport DTO bytes.
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_CALL_PAYLOAD_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_CALL_PAYLOAD_BYTES>"
    )]
    pub viewport: Vec<u8>,
}

impl ViewInput {
    /// Construct a viewport input after checking its variable fields.
    pub fn try_new(
        shared: SharedStateBytes,
        session: Vec<u8>,
        viewport: Vec<u8>,
    ) -> Result<Self, AbiEnvelopeError> {
        ensure_field("session context", &session, MAX_SESSION_CONTEXT_BYTES)?;
        ensure_field("viewport", &viewport, MAX_CALL_PAYLOAD_BYTES)?;
        Ok(Self {
            shared,
            session,
            viewport,
        })
    }
}

/// Read-only viewport result. It contains no state or status.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct ViewOutput {
    /// Stock-Serde JSON view DTO bytes.
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_CALL_PAYLOAD_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_CALL_PAYLOAD_BYTES>"
    )]
    pub json: Vec<u8>,
}

/// Read-only terminal outcome input. It contains no peer identity or local
/// state and no call-specific payload.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct OutcomeInput {
    /// Explicit replicated state bytes.
    pub shared: SharedStateBytes,
    /// Opaque Borsh committed `Ensemble` session context.
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_SESSION_CONTEXT_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_SESSION_CONTEXT_BYTES>"
    )]
    pub session: Vec<u8>,
}

/// Read-only input for selecting the next program-message writer.
///
/// Writer selection is deliberately a function of replicated state alone. The
/// host invokes it before applying a candidate message so transport arrival
/// order cannot choose between sibling state transitions.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct WriterInput {
    /// Explicit replicated state bytes.
    pub shared: SharedStateBytes,
}

/// Sole participant eligible to author the next program message.
///
/// `None` means that no program message is admissible from the current shared
/// state. The host validates a returned index against the committed ensemble.
#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct WriterOutput {
    /// Stable participant index in the committed ensemble.
    pub participant: Option<u8>,
}

impl OutcomeInput {
    /// Construct an outcome input after checking its session field.
    pub fn try_new(shared: SharedStateBytes, session: Vec<u8>) -> Result<Self, AbiEnvelopeError> {
        ensure_field("session context", &session, MAX_SESSION_CONTEXT_BYTES)?;
        Ok(Self { shared, session })
    }
}

/// Read-only terminal outcome result carrying both stock projections of the
/// same concrete DTO.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct OutcomeOutput {
    /// Stock-Borsh bytes used by the terminal protocol effect.
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_CALL_PAYLOAD_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_CALL_PAYLOAD_BYTES>"
    )]
    pub borsh: Vec<u8>,
    /// Stock-Serde JSON bytes exposed to agents.
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_CALL_PAYLOAD_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_CALL_PAYLOAD_BYTES>"
    )]
    pub json: Vec<u8>,
}

fn ensure_field(field: &'static str, bytes: &[u8], max: usize) -> Result<(), AbiEnvelopeError> {
    if bytes.len() > max {
        return Err(AbiEnvelopeError::FieldTooLarge {
            field,
            actual: bytes.len(),
            max,
        });
    }
    Ok(())
}

/// Level tags of the `log` import. These values are part of the ABI.
pub mod log_level {
    /// Debug.
    pub const DEBUG: u32 = 0;
    /// Info.
    pub const INFO: u32 = 1;
    /// Warn.
    pub const WARN: u32 = 2;
    /// Error.
    pub const ERROR: u32 = 3;
}

/// Scheme tags of the `sign` import. These values are part of the ABI.
pub mod sign_scheme {
    use arena0_crypto::SignScheme;

    /// Ed25519.
    pub const ED25519: u32 = 0;
    /// BLS.
    pub const BLS: u32 = 1;

    /// The ABI tag of `scheme`.
    #[must_use]
    pub const fn tag(scheme: SignScheme) -> u32 {
        match scheme {
            SignScheme::Ed25519 => ED25519,
            SignScheme::Bls => BLS,
        }
    }

    /// The scheme an ABI tag names, if any.
    #[must_use]
    pub const fn from_tag(tag: u32) -> Option<SignScheme> {
        match tag {
            ED25519 => Some(SignScheme::Ed25519),
            BLS => Some(SignScheme::Bls),
            _ => None,
        }
    }
}

/// Names of host-function imports. These values are part of the ABI.
pub mod imports {
    /// Return the current payload length of one canonical state memory.
    pub const STATE_LEN: &str = "state_len";
    /// Copy a state payload into the guest's work memory.
    pub const STATE_READ: &str = "state_read";
    /// Replace a canonical state payload from bytes in the guest's work memory.
    pub const STATE_WRITE: &str = "state_write";
    /// Terminate execution with an error message.
    pub const FAIL: &str = "fail";
    /// Emit a structured log line to the host.
    pub const LOG: &str = "log";
    /// Request deterministic random bytes from the host.
    pub const RANDOM: &str = "random";
    /// Broadcast a binary message to every participant.
    pub const BROADCAST: &str = "broadcast";
    /// Start a one-shot timer with a typed payload.
    pub const SET_TIMER: &str = "set_timer";
    /// Sign one guest payload with the node's key.
    pub const SIGN: &str = "sign";
    /// End the current session successfully.
    pub const END_SESSION: &str = "end_session";
    /// Abort the current session.
    pub const ABORT_SESSION: &str = "abort_session";
}

impl Capability {
    /// Import names unlocked by this declared capability.
    #[must_use]
    pub fn imports(&self) -> &'static [&'static str] {
        match self {
            Self::Messaging => &[imports::BROADCAST],
            Self::Timers => &[imports::SET_TIMER],
            Self::Sign { .. } => &[imports::SIGN],
        }
    }
}

/// Import names linked regardless of declared capabilities.
#[must_use]
pub fn always_available_imports() -> &'static [&'static str] {
    &[
        imports::STATE_LEN,
        imports::STATE_READ,
        imports::STATE_WRITE,
        imports::FAIL,
        imports::LOG,
        imports::RANDOM,
        imports::END_SESSION,
        imports::ABORT_SESSION,
    ]
}

/// Every known effect import name, excluding state I/O imports.
#[must_use]
pub fn all_effect_imports() -> &'static [&'static str] {
    &[
        imports::FAIL,
        imports::LOG,
        imports::RANDOM,
        imports::BROADCAST,
        imports::SET_TIMER,
        imports::SIGN,
        imports::END_SESSION,
        imports::ABORT_SESSION,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_crypto::SignScheme;
    use std::collections::HashSet;

    #[test]
    fn sign_scheme_tags_round_trip() {
        for scheme in [SignScheme::Ed25519, SignScheme::Bls] {
            assert_eq!(
                sign_scheme::from_tag(sign_scheme::tag(scheme)),
                Some(scheme)
            );
        }
        assert_eq!(sign_scheme::from_tag(2), None);
    }

    #[test]
    fn imports_for_each_capability() {
        assert_eq!(Capability::Messaging.imports(), &["broadcast"]);
        assert_eq!(Capability::Timers.imports(), &["set_timer"]);
        assert_eq!(
            Capability::Sign {
                schemes: vec![SignScheme::Ed25519]
            }
            .imports(),
            &["sign"]
        );
    }

    #[test]
    fn import_lists_have_no_duplicates() {
        for list in [always_available_imports(), all_effect_imports()] {
            let unique: HashSet<&str> = list.iter().copied().collect();
            assert_eq!(list.len(), unique.len());
        }
    }

    #[test]
    fn capability_imports_are_effect_imports() {
        let all_effects: HashSet<&str> = all_effect_imports().iter().copied().collect();
        for capability in [
            Capability::Messaging,
            Capability::Timers,
            Capability::Sign {
                schemes: vec![SignScheme::Ed25519],
            },
        ] {
            for import in capability.imports() {
                assert!(all_effects.contains(import));
            }
        }
    }

    #[test]
    fn sign_result_overhead_covers_the_preimage_header_and_signature() {
        // GuestSignData header: domain, version, session, program, execution,
        // event position, call index, scheme, and payload length prefix.
        let header = 24 + 2 + 32 + 32 + 32 + 8 + 4 + 1 + 4;
        // Returned `(signed_bytes, signature)`: two length prefixes and a
        // 64-byte Ed25519 signature.
        let envelope = 4 + 4 + 64;
        assert!(SIGN_RESULT_OVERHEAD_BYTES >= header + envelope);
    }

    #[test]
    fn json_bytes_reject_invalid_and_accept_complete_values() {
        assert!(matches!(
            JsonBytes::try_new(b"not-json".to_vec()),
            Err(JsonBytesError::Invalid(_))
        ));
        let value = JsonBytes::try_new(br#"{"answer":42}"#.to_vec()).unwrap();
        assert_eq!(value.as_bytes(), br#"{"answer":42}"#);
    }

    #[test]
    fn json_bytes_reject_oversized_values_before_parsing() {
        let bytes = vec![b' '; MAX_CALL_PAYLOAD_BYTES + 1];
        assert!(matches!(
            JsonBytes::try_new(bytes),
            Err(JsonBytesError::TooLarge { .. })
        ));
    }

    #[test]
    fn json_bytes_decode_revalidates_the_json_contract() {
        let value = JsonBytes::try_new(br#"{"answer":42}"#.to_vec()).unwrap();
        let encoded = borsh::to_vec(&value).unwrap();
        assert_eq!(borsh::from_slice::<JsonBytes>(&encoded).unwrap(), value);

        let invalid = [8u32.to_le_bytes().as_slice(), b"not-json"].concat();
        assert!(borsh::from_slice::<JsonBytes>(&invalid).is_err());
    }

    #[test]
    fn call_status_uses_stable_v1_tags() {
        assert_eq!(borsh::to_vec(&CallStatus::Accepted).unwrap(), [0]);
        assert_eq!(borsh::to_vec(&CallStatus::Rejected).unwrap(), [1]);
        assert_eq!(
            borsh::from_slice::<CallStatus>(&[0]).unwrap(),
            CallStatus::Accepted
        );
        assert_eq!(
            borsh::from_slice::<CallStatus>(&[1]).unwrap(),
            CallStatus::Rejected
        );
        assert!(borsh::from_slice::<CallStatus>(&[2]).is_err());
    }

    #[test]
    fn dispatch_output_round_trips_bounded_rejection_reason() {
        for output in [
            DispatchOutput {
                status: CallStatus::Accepted,
                reason: None,
                callout: None,
            },
            DispatchOutput {
                status: CallStatus::Accepted,
                reason: None,
                callout: Some(CalloutRequest {
                    callout_index: 1,
                    context: br#"{"prompt":"choose"}"#.to_vec(),
                }),
            },
            DispatchOutput {
                status: CallStatus::Rejected,
                reason: None,
                callout: None,
            },
            DispatchOutput {
                status: CallStatus::Rejected,
                reason: Some("invalid answer".into()),
                callout: None,
            },
        ] {
            let encoded = borsh::to_vec(&output).unwrap();
            assert_eq!(
                borsh::from_slice::<DispatchOutput>(&encoded).unwrap(),
                output
            );
        }
    }

    #[test]
    fn dispatch_output_rejects_invalid_reason_combinations_and_bounds() {
        // The derived serializer enforces field bounds but not the
        // status/reason/callout invariant; decode still rejects it below.
        let oversized = DispatchOutput {
            status: CallStatus::Rejected,
            reason: Some("x".repeat(MAX_REJECTION_REASON_BYTES + 1)),
            callout: None,
        };
        assert!(borsh::to_vec(&oversized).is_err());

        let oversized_context = DispatchOutput {
            status: CallStatus::Accepted,
            reason: None,
            callout: Some(CalloutRequest {
                callout_index: 0,
                context: vec![b' '; MAX_CALLOUT_CONTEXT_BYTES + 1],
            }),
        };
        assert!(borsh::to_vec(&oversized_context).is_err());

        let mut accepted_with_encoded_reason = vec![CallStatus::Accepted.tag(), 1];
        accepted_with_encoded_reason.extend_from_slice(&1u32.to_le_bytes());
        accepted_with_encoded_reason.push(b'x');
        assert!(borsh::from_slice::<DispatchOutput>(&accepted_with_encoded_reason).is_err());
    }

    #[test]
    fn abi_inputs_and_outputs_round_trip() {
        let shared = SharedStateBytes::try_from_slice(b"shared").unwrap();
        let local = LocalStateBytes::try_from_slice(b"local").unwrap();

        let init = InitInput::try_new(br#"{"answer":42}"#.to_vec()).unwrap();
        assert_eq!(
            borsh::from_slice::<InitInput>(&borsh::to_vec(&init).unwrap()).unwrap(),
            init
        );

        let initialized = InitializedState {
            shared: shared.clone(),
            local: local.clone(),
        };
        assert_eq!(
            borsh::from_slice::<InitializedState>(&borsh::to_vec(&initialized).unwrap()).unwrap(),
            initialized
        );

        let dispatch = DispatchInput::try_new([7; 32], vec![1, 2], vec![3, 4, 5]).unwrap();
        assert_eq!(
            borsh::from_slice::<DispatchInput>(&borsh::to_vec(&dispatch).unwrap()).unwrap(),
            dispatch
        );

        let callout = CalloutRequest {
            callout_index: 1,
            context: br#"{"prompt":"choose"}"#.to_vec(),
        };
        assert_eq!(
            borsh::from_slice::<CalloutRequest>(&borsh::to_vec(&callout).unwrap()).unwrap(),
            callout
        );

        let query =
            QueryInput::try_new(shared.clone(), vec![1], 2, br#"{"q":1}"#.to_vec()).unwrap();
        assert_eq!(
            borsh::from_slice::<QueryInput>(&borsh::to_vec(&query).unwrap()).unwrap(),
            query
        );

        let query_output = QueryOutput {
            query_index: 2,
            json: br#"{"a":1}"#.to_vec(),
        };
        assert_eq!(
            borsh::from_slice::<QueryOutput>(&borsh::to_vec(&query_output).unwrap()).unwrap(),
            query_output
        );

        let view = ViewInput::try_new(shared.clone(), vec![1], br#"{"v":1}"#.to_vec()).unwrap();
        assert_eq!(
            borsh::from_slice::<ViewInput>(&borsh::to_vec(&view).unwrap()).unwrap(),
            view
        );

        let view_output = ViewOutput {
            json: br#"{"w":2}"#.to_vec(),
        };
        assert_eq!(
            borsh::from_slice::<ViewOutput>(&borsh::to_vec(&view_output).unwrap()).unwrap(),
            view_output
        );

        let outcome = OutcomeInput::try_new(shared.clone(), vec![9]).unwrap();
        assert_eq!(
            borsh::from_slice::<OutcomeInput>(&borsh::to_vec(&outcome).unwrap()).unwrap(),
            outcome
        );

        let outcome_output = OutcomeOutput {
            borsh: vec![0],
            json: b"null".to_vec(),
        };
        assert_eq!(
            borsh::from_slice::<OutcomeOutput>(&borsh::to_vec(&outcome_output).unwrap()).unwrap(),
            outcome_output
        );

        let writer = WriterInput {
            shared: shared.clone(),
        };
        assert_eq!(
            borsh::from_slice::<WriterInput>(&borsh::to_vec(&writer).unwrap()).unwrap(),
            writer
        );

        for output in [
            WriterOutput { participant: None },
            WriterOutput {
                participant: Some(3),
            },
        ] {
            assert_eq!(
                borsh::from_slice::<WriterOutput>(&borsh::to_vec(&output).unwrap()).unwrap(),
                output
            );
        }
    }

    #[test]
    fn input_length_prefixes_are_checked_before_allocation() {
        let oversized = u32::MAX.to_le_bytes();
        let session = [0u32.to_le_bytes(), oversized].concat();
        // Omit the declared payload: a size error must precede a body read.
        for (input, error) in [
            (
                "init",
                borsh::from_slice::<InitInput>(&oversized).unwrap_err(),
            ),
            (
                "query",
                borsh::from_slice::<QueryInput>(&session).unwrap_err(),
            ),
            (
                "view",
                borsh::from_slice::<ViewInput>(&session).unwrap_err(),
            ),
            (
                "outcome",
                borsh::from_slice::<OutcomeInput>(&session).unwrap_err(),
            ),
            (
                "dispatch session",
                borsh::from_slice::<DispatchInput>(
                    &[[0; 32].as_slice(), oversized.as_slice()].concat(),
                )
                .unwrap_err(),
            ),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{input}: {error}");
        }
    }

    #[test]
    fn output_length_prefixes_are_checked_before_allocation() {
        let oversized = u32::MAX.to_le_bytes();
        let query = [0u32.to_le_bytes(), oversized].concat();
        let outcome = (MAX_CALL_PAYLOAD_BYTES as u32 + 1).to_le_bytes();
        for (output, error) in [
            (
                "query",
                borsh::from_slice::<QueryOutput>(&query).unwrap_err(),
            ),
            (
                "view",
                borsh::from_slice::<ViewOutput>(&oversized).unwrap_err(),
            ),
            (
                "outcome",
                borsh::from_slice::<OutcomeOutput>(&outcome).unwrap_err(),
            ),
        ] {
            assert_eq!(
                error.kind(),
                io::ErrorKind::InvalidData,
                "{output}: {error}"
            );
        }
    }
}
