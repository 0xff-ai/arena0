//! Host-guest ABI contract: version, module name, and import names.

use std::io;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::profile::MAX_CALL_ENVELOPE_BYTES;
use crate::{LocalStateBytes, SharedStateBytes};

use crate::Capability;

/// Current ABI version. A sandbox rejects modules declaring a different one.
pub const ABI_VERSION: u32 = 20;

/// Wasm import module name for all arena0 host functions.
pub const HOST_MODULE: &str = "arena0";

/// Maximum bytes in one semantic call payload.
pub const MAX_CALL_PAYLOAD_BYTES: usize = crate::profile::MAX_INPUT_BYTES as usize;
/// Maximum bytes in the session context carried into a fresh call.
pub const MAX_SESSION_CONTEXT_BYTES: usize = 1024 * 1024;

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
        write_bounded_vec(writer, &self.0, MAX_CALL_PAYLOAD_BYTES, "agent-facing JSON")
    }
}

impl BorshDeserialize for JsonBytes {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let bytes = read_bounded_vec(reader, MAX_CALL_PAYLOAD_BYTES, "agent-facing JSON")?;
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

/// Export names in the fresh-instance guest ABI.
pub mod exports {
    /// Guest allocation entry point.
    pub const ALLOC: &str = "arena0_alloc";
    /// Guest deallocation entry point.
    pub const DEALLOC: &str = "arena0_dealloc";
    /// Initialize a fresh program state.
    pub const INITIALIZE: &str = "arena0_initialize";
    /// Apply one shared/public event.
    pub const SHARED: &str = "arena0_shared";
    /// Apply one local/private event.
    pub const LOCAL: &str = "arena0_local";
    /// Produce the agent-facing terminal outcome.
    pub const OUTCOME: &str = "arena0_outcome";
    /// Select the sole participant eligible to author the next public message.
    pub const WRITER: &str = "arena0_writer";
    /// Answer one agent-facing query.
    pub const QUERY: &str = "arena0_query";
    /// Produce one viewport projection.
    pub const VIEW: &str = "arena0_view";
    /// Return the embedded program definition.
    pub const METADATA: &str = "arena0_metadata";
}

/// Whether a mutating guest call accepted its event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

impl BorshSerialize for CallStatus {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        self.tag().serialize(writer)
    }
}

impl BorshDeserialize for CallStatus {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        Self::from_tag(u8::deserialize_reader(reader)?)
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
    /// The complete encoded value exceeded the ABI envelope bound.
    #[error("ABI envelope is {actual} bytes; maximum is {max}")]
    EnvelopeTooLarge {
        /// Actual encoded envelope length.
        actual: usize,
        /// Maximum accepted envelope length.
        max: usize,
    },
}

fn field_error(field: &'static str, actual: usize, max: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        AbiEnvelopeError::FieldTooLarge { field, actual, max },
    )
}

fn write_bounded_vec<W: io::Write>(
    writer: &mut W,
    bytes: &[u8],
    max: usize,
    field: &'static str,
) -> io::Result<()> {
    if bytes.len() > max {
        return Err(field_error(field, bytes.len(), max));
    }
    let length = u32::try_from(bytes.len())
        .map_err(|_| field_error(field, bytes.len(), u32::MAX as usize))?;
    length.serialize(writer)?;
    writer.write_all(bytes)
}

fn read_bounded_vec<R: io::Read>(
    reader: &mut R,
    max: usize,
    field: &'static str,
) -> io::Result<Vec<u8>> {
    let length = usize::try_from(u32::deserialize_reader(reader)?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{field} length overflows usize"),
        )
    })?;
    if length > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            AbiEnvelopeError::FieldTooLarge {
                field,
                actual: length,
                max,
            },
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Writer that rejects an envelope before it can exceed the complete ABI cap.
struct EnvelopeWriter<'a, W> {
    writer: &'a mut W,
    written: usize,
}

impl<'a, W> EnvelopeWriter<'a, W> {
    fn new(writer: &'a mut W) -> Self {
        Self { writer, written: 0 }
    }
}

impl<W: io::Write> io::Write for EnvelopeWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self.written.checked_add(bytes.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "ABI envelope length overflow")
        })?;
        if next > MAX_CALL_ENVELOPE_BYTES as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                AbiEnvelopeError::EnvelopeTooLarge {
                    actual: next,
                    max: MAX_CALL_ENVELOPE_BYTES as usize,
                },
            ));
        }
        let count = self.writer.write(bytes)?;
        self.written = self.written.saturating_add(count);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// Reader that prevents a malformed envelope from consuming more than the
/// complete ABI cap. Field readers still check their own bound before they
/// allocate.
struct EnvelopeReader<'a, R> {
    reader: &'a mut R,
    remaining: usize,
}

impl<'a, R> EnvelopeReader<'a, R> {
    fn new(reader: &'a mut R) -> Self {
        Self {
            reader,
            remaining: MAX_CALL_ENVELOPE_BYTES as usize,
        }
    }
}

impl<R: io::Read> io::Read for EnvelopeReader<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                AbiEnvelopeError::EnvelopeTooLarge {
                    actual: MAX_CALL_ENVELOPE_BYTES as usize + 1,
                    max: MAX_CALL_ENVELOPE_BYTES as usize,
                },
            ));
        }
        let read_len = bytes.len().min(self.remaining);
        let count = self.reader.read(&mut bytes[..read_len])?;
        self.remaining = self.remaining.saturating_sub(count);
        Ok(count)
    }
}

/// A fresh initialization input. It carries only agent parameter JSON bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitInput {
    /// Stock-Serde JSON encoding of the program's parameter DTO.
    pub params: Vec<u8>,
}

impl InitInput {
    /// Construct an initialization input after checking its field bound.
    pub fn try_new(params: Vec<u8>) -> Result<Self, AbiEnvelopeError> {
        ensure_field("params", &params, MAX_CALL_PAYLOAD_BYTES)?;
        Ok(Self { params })
    }
}

impl BorshSerialize for InitInput {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        let mut writer = EnvelopeWriter::new(writer);
        write_bounded_vec(&mut writer, &self.params, MAX_CALL_PAYLOAD_BYTES, "params")
    }
}

impl BorshDeserialize for InitInput {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let mut reader = EnvelopeReader::new(reader);
        Ok(Self {
            params: read_bounded_vec(&mut reader, MAX_CALL_PAYLOAD_BYTES, "params")?,
        })
    }
}

/// Initialization output containing both freshly initialized state values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitializedState {
    /// Initial replicated state bytes.
    pub shared: SharedStateBytes,
    /// Initial participant-local state bytes.
    pub local: LocalStateBytes,
}

impl BorshSerialize for InitializedState {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        let mut writer = EnvelopeWriter::new(writer);
        self.shared.serialize(&mut writer)?;
        self.local.serialize(&mut writer)
    }
}

impl BorshDeserialize for InitializedState {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let mut reader = EnvelopeReader::new(reader);
        Ok(Self {
            shared: SharedStateBytes::deserialize_reader(&mut reader)?,
            local: LocalStateBytes::deserialize_reader(&mut reader)?,
        })
    }
}

/// Shared/public event input. It deliberately contains no peer or local state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedInput {
    /// Explicit replicated state bytes.
    pub shared: SharedStateBytes,
    /// Opaque Borsh `Option<Ensemble<Committed>>` session context. It is `None`
    /// only for the session-start event, whose ensemble is in `event`.
    pub session: Vec<u8>,
    /// Stock-Borsh public protocol event bytes.
    pub event: Vec<u8>,
}

impl SharedInput {
    /// Construct a shared input after checking its variable fields.
    pub fn try_new(
        shared: SharedStateBytes,
        session: Vec<u8>,
        event: Vec<u8>,
    ) -> Result<Self, AbiEnvelopeError> {
        ensure_field("session context", &session, MAX_SESSION_CONTEXT_BYTES)?;
        ensure_field("public event", &event, MAX_CALL_PAYLOAD_BYTES)?;
        Ok(Self {
            shared,
            session,
            event,
        })
    }
}

impl BorshSerialize for SharedInput {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        let mut writer = EnvelopeWriter::new(writer);
        self.shared.serialize(&mut writer)?;
        write_bounded_vec(
            &mut writer,
            &self.session,
            MAX_SESSION_CONTEXT_BYTES,
            "session context",
        )?;
        write_bounded_vec(
            &mut writer,
            &self.event,
            MAX_CALL_PAYLOAD_BYTES,
            "public event",
        )
    }
}

impl BorshDeserialize for SharedInput {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let mut reader = EnvelopeReader::new(reader);
        Ok(Self {
            shared: SharedStateBytes::deserialize_reader(&mut reader)?,
            session: read_bounded_vec(&mut reader, MAX_SESSION_CONTEXT_BYTES, "session context")?,
            event: read_bounded_vec(&mut reader, MAX_CALL_PAYLOAD_BYTES, "public event")?,
        })
    }
}

/// Result of a shared/public event. Local state is intentionally absent: the
/// host retains the caller's local state outside this guest call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedOutput {
    /// Whether the event was accepted or deterministically rejected.
    pub status: CallStatus,
    /// Replacement replicated state bytes.
    pub shared: SharedStateBytes,
}

impl BorshSerialize for SharedOutput {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        let mut writer = EnvelopeWriter::new(writer);
        self.status.serialize(&mut writer)?;
        self.shared.serialize(&mut writer)
    }
}

impl BorshDeserialize for SharedOutput {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let mut reader = EnvelopeReader::new(reader);
        Ok(Self {
            status: CallStatus::deserialize_reader(&mut reader)?,
            shared: SharedStateBytes::deserialize_reader(&mut reader)?,
        })
    }
}

/// Local/private event input. This is the only ABI input carrying peer identity
/// and participant-local state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalInput {
    /// Local node identity in the protocol's canonical 32-byte form.
    pub peer_id: [u8; 32],
    /// Explicit replicated state bytes, retained unchanged by the local call.
    pub shared: SharedStateBytes,
    /// Explicit participant-local state bytes.
    pub local: LocalStateBytes,
    /// Opaque Borsh committed `Ensemble` session context.
    pub session: Vec<u8>,
    /// Stock-Borsh private protocol event bytes.
    pub event: Vec<u8>,
}

impl LocalInput {
    /// Construct a local input after checking its variable fields.
    pub fn try_new(
        peer_id: [u8; 32],
        shared: SharedStateBytes,
        local: LocalStateBytes,
        session: Vec<u8>,
        event: Vec<u8>,
    ) -> Result<Self, AbiEnvelopeError> {
        ensure_field("session context", &session, MAX_SESSION_CONTEXT_BYTES)?;
        ensure_field("private event", &event, MAX_CALL_PAYLOAD_BYTES)?;
        Ok(Self {
            peer_id,
            shared,
            local,
            session,
            event,
        })
    }
}

impl BorshSerialize for LocalInput {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        let mut writer = EnvelopeWriter::new(writer);
        self.peer_id.serialize(&mut writer)?;
        self.shared.serialize(&mut writer)?;
        self.local.serialize(&mut writer)?;
        write_bounded_vec(
            &mut writer,
            &self.session,
            MAX_SESSION_CONTEXT_BYTES,
            "session context",
        )?;
        write_bounded_vec(
            &mut writer,
            &self.event,
            MAX_CALL_PAYLOAD_BYTES,
            "private event",
        )
    }
}

impl BorshDeserialize for LocalInput {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let mut reader = EnvelopeReader::new(reader);
        Ok(Self {
            peer_id: <[u8; 32]>::deserialize_reader(&mut reader)?,
            shared: SharedStateBytes::deserialize_reader(&mut reader)?,
            local: LocalStateBytes::deserialize_reader(&mut reader)?,
            session: read_bounded_vec(&mut reader, MAX_SESSION_CONTEXT_BYTES, "session context")?,
            event: read_bounded_vec(&mut reader, MAX_CALL_PAYLOAD_BYTES, "private event")?,
        })
    }
}

/// Result of a local/private event. Shared state is intentionally absent: the
/// host retains it outside the guest call and rejects any attempted replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalOutput {
    /// Whether the event was accepted.
    pub status: CallStatus,
    /// Replacement participant-local state bytes.
    pub local: LocalStateBytes,
}

impl BorshSerialize for LocalOutput {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        let mut writer = EnvelopeWriter::new(writer);
        self.status.serialize(&mut writer)?;
        self.local.serialize(&mut writer)
    }
}

impl BorshDeserialize for LocalOutput {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let mut reader = EnvelopeReader::new(reader);
        Ok(Self {
            status: CallStatus::deserialize_reader(&mut reader)?,
            local: LocalStateBytes::deserialize_reader(&mut reader)?,
        })
    }
}

/// Read-only query input. It contains no peer identity or participant-local
/// state, and the index is echoed by [`QueryOutput`] to bind schema validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryInput {
    /// Explicit replicated state bytes.
    pub shared: SharedStateBytes,
    /// Opaque Borsh committed `Ensemble` session context.
    pub session: Vec<u8>,
    /// Host-checked index into the advertised query schema list.
    pub query_index: u32,
    /// Stock-Serde JSON query DTO bytes.
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

impl BorshSerialize for QueryInput {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        let mut writer = EnvelopeWriter::new(writer);
        self.shared.serialize(&mut writer)?;
        write_bounded_vec(
            &mut writer,
            &self.session,
            MAX_SESSION_CONTEXT_BYTES,
            "session context",
        )?;
        self.query_index.serialize(&mut writer)?;
        write_bounded_vec(&mut writer, &self.query, MAX_CALL_PAYLOAD_BYTES, "query")
    }
}

impl BorshDeserialize for QueryInput {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let mut reader = EnvelopeReader::new(reader);
        Ok(Self {
            shared: SharedStateBytes::deserialize_reader(&mut reader)?,
            session: read_bounded_vec(&mut reader, MAX_SESSION_CONTEXT_BYTES, "session context")?,
            query_index: u32::deserialize_reader(&mut reader)?,
            query: read_bounded_vec(&mut reader, MAX_CALL_PAYLOAD_BYTES, "query")?,
        })
    }
}

/// Read-only query result. It contains no state or status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryOutput {
    /// The exact query index supplied by the host.
    pub query_index: u32,
    /// Stock-Serde JSON response DTO bytes.
    pub json: Vec<u8>,
}

impl BorshSerialize for QueryOutput {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        let mut writer = EnvelopeWriter::new(writer);
        self.query_index.serialize(&mut writer)?;
        write_bounded_vec(
            &mut writer,
            &self.json,
            MAX_CALL_PAYLOAD_BYTES,
            "query response",
        )
    }
}

impl BorshDeserialize for QueryOutput {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let mut reader = EnvelopeReader::new(reader);
        Ok(Self {
            query_index: u32::deserialize_reader(&mut reader)?,
            json: read_bounded_vec(&mut reader, MAX_CALL_PAYLOAD_BYTES, "query response")?,
        })
    }
}

/// Read-only viewport input. The viewport is the current stock-Serde JSON
/// encoding of the protocol's `Viewport` DTO, carried as opaque bytes here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewInput {
    /// Explicit replicated state bytes.
    pub shared: SharedStateBytes,
    /// Opaque Borsh committed `Ensemble` session context.
    pub session: Vec<u8>,
    /// Stock-Serde JSON viewport DTO bytes.
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

impl BorshSerialize for ViewInput {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        let mut writer = EnvelopeWriter::new(writer);
        self.shared.serialize(&mut writer)?;
        write_bounded_vec(
            &mut writer,
            &self.session,
            MAX_SESSION_CONTEXT_BYTES,
            "session context",
        )?;
        write_bounded_vec(
            &mut writer,
            &self.viewport,
            MAX_CALL_PAYLOAD_BYTES,
            "viewport",
        )
    }
}

impl BorshDeserialize for ViewInput {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let mut reader = EnvelopeReader::new(reader);
        Ok(Self {
            shared: SharedStateBytes::deserialize_reader(&mut reader)?,
            session: read_bounded_vec(&mut reader, MAX_SESSION_CONTEXT_BYTES, "session context")?,
            viewport: read_bounded_vec(&mut reader, MAX_CALL_PAYLOAD_BYTES, "viewport")?,
        })
    }
}

/// Read-only viewport result. It contains no state or status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewOutput {
    /// Stock-Serde JSON view DTO bytes.
    pub json: Vec<u8>,
}

impl BorshSerialize for ViewOutput {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        let mut writer = EnvelopeWriter::new(writer);
        write_bounded_vec(
            &mut writer,
            &self.json,
            MAX_CALL_PAYLOAD_BYTES,
            "view response",
        )
    }
}

impl BorshDeserialize for ViewOutput {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let mut reader = EnvelopeReader::new(reader);
        Ok(Self {
            json: read_bounded_vec(&mut reader, MAX_CALL_PAYLOAD_BYTES, "view response")?,
        })
    }
}

/// Read-only terminal outcome input. It contains no peer identity or local
/// state and no call-specific payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeInput {
    /// Explicit replicated state bytes.
    pub shared: SharedStateBytes,
    /// Opaque Borsh committed `Ensemble` session context.
    pub session: Vec<u8>,
}

/// Read-only input for selecting the next public-message writer.
///
/// Writer selection is deliberately a function of replicated state alone. The
/// host invokes it before applying a candidate message so transport arrival
/// order cannot choose between sibling state transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterInput {
    /// Explicit replicated state bytes.
    pub shared: SharedStateBytes,
}

impl BorshSerialize for WriterInput {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        self.shared.serialize(writer)
    }
}

impl BorshDeserialize for WriterInput {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        Ok(Self {
            shared: SharedStateBytes::deserialize_reader(reader)?,
        })
    }
}

/// Sole participant eligible to author the next public message.
///
/// `None` means that no public message is admissible from the current shared
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

impl BorshSerialize for OutcomeInput {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        let mut writer = EnvelopeWriter::new(writer);
        self.shared.serialize(&mut writer)?;
        write_bounded_vec(
            &mut writer,
            &self.session,
            MAX_SESSION_CONTEXT_BYTES,
            "session context",
        )
    }
}

impl BorshDeserialize for OutcomeInput {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let mut reader = EnvelopeReader::new(reader);
        Ok(Self {
            shared: SharedStateBytes::deserialize_reader(&mut reader)?,
            session: read_bounded_vec(&mut reader, MAX_SESSION_CONTEXT_BYTES, "session context")?,
        })
    }
}

/// Read-only terminal outcome result carrying both stock projections of the
/// same concrete DTO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeOutput {
    /// Stock-Borsh bytes used by the terminal protocol effect.
    pub borsh: Vec<u8>,
    /// Stock-Serde JSON bytes exposed to agents.
    pub json: Vec<u8>,
}

impl BorshSerialize for OutcomeOutput {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        let mut writer = EnvelopeWriter::new(writer);
        write_bounded_vec(
            &mut writer,
            &self.borsh,
            MAX_CALL_PAYLOAD_BYTES,
            "Borsh outcome",
        )?;
        write_bounded_vec(
            &mut writer,
            &self.json,
            MAX_CALL_PAYLOAD_BYTES,
            "JSON outcome",
        )
    }
}

impl BorshDeserialize for OutcomeOutput {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let mut reader = EnvelopeReader::new(reader);
        Ok(Self {
            borsh: read_bounded_vec(&mut reader, MAX_CALL_PAYLOAD_BYTES, "Borsh outcome")?,
            json: read_bounded_vec(&mut reader, MAX_CALL_PAYLOAD_BYTES, "JSON outcome")?,
        })
    }
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

/// Names of host-function imports. These values are part of the ABI.
pub mod imports {
    /// Terminate execution with an error message.
    pub const FAIL: &str = "fail";
    /// Emit a structured log line to the host.
    pub const LOG: &str = "log";
    /// Request deterministic random bytes from the host.
    pub const RANDOM: &str = "random";
    /// Broadcast a binary message to every participant.
    pub const BROADCAST: &str = "broadcast";
    /// Request input from the controlling agent.
    pub const REQUEST_INPUT: &str = "request_input";
    /// Request input with pending/expected-type trace metadata.
    pub const REQUEST_INPUT_PENDING: &str = "request_input_pending";
    /// Attach a generated continuation tag to the next pending effect.
    pub const SET_CONTINUATION_TAG: &str = "set_continuation_tag";
    /// Start an untyped one-shot timer.
    pub const SET_TIMER: &str = "set_timer";
    /// Start a typed one-shot timer.
    pub const SET_TYPED_TIMER: &str = "set_typed_timer";
    /// Sign data with the node's key.
    pub const SIGN: &str = "sign";
    /// Sign data with local pending/expected-type trace metadata.
    pub const SIGN_PENDING: &str = "sign_pending";
    /// End the current session successfully.
    pub const END_SESSION: &str = "end_session";
    /// Abort the current session.
    pub const ABORT_SESSION: &str = "abort_session";
    /// Signal retryable callout input error.
    pub const RETRY_INPUT: &str = "retry_input";
}

impl Capability {
    /// Import names unlocked by this declared capability.
    #[must_use]
    pub fn imports(&self) -> &'static [&'static str] {
        match self {
            Self::Messaging => &[imports::BROADCAST],
            Self::Input => &[imports::REQUEST_INPUT, imports::REQUEST_INPUT_PENDING],
            Self::Timers => &[imports::SET_TIMER, imports::SET_TYPED_TIMER],
            Self::Sign { .. } => &[imports::SIGN, imports::SIGN_PENDING],
        }
    }
}

/// Import names linked regardless of declared capabilities.
#[must_use]
pub fn always_available_imports() -> &'static [&'static str] {
    &[
        imports::FAIL,
        imports::LOG,
        imports::RANDOM,
        imports::END_SESSION,
        imports::ABORT_SESSION,
        imports::RETRY_INPUT,
        imports::SET_CONTINUATION_TAG,
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
        imports::REQUEST_INPUT,
        imports::REQUEST_INPUT_PENDING,
        imports::SET_TIMER,
        imports::SET_TYPED_TIMER,
        imports::SIGN,
        imports::SIGN_PENDING,
        imports::END_SESSION,
        imports::ABORT_SESSION,
        imports::RETRY_INPUT,
        imports::SET_CONTINUATION_TAG,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_crypto::SignScheme;
    use std::collections::HashSet;

    #[test]
    fn imports_for_each_capability() {
        assert_eq!(Capability::Messaging.imports(), &["broadcast"]);
        assert_eq!(
            Capability::Input.imports(),
            &["request_input", "request_input_pending"]
        );
        assert_eq!(
            Capability::Timers.imports(),
            &["set_timer", "set_typed_timer"]
        );
        assert_eq!(
            Capability::Sign {
                schemes: vec![SignScheme::Ed25519]
            }
            .imports(),
            &["sign", "sign_pending"]
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
            Capability::Input,
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
    fn input_length_prefixes_are_checked_before_allocation() {
        let mut cases = [
            (vec![0u8; 0], "init"),
            (vec![0u8; 4], "shared"),
            (vec![0u8; 4], "local"),
            (vec![0u8; 4], "query"),
            (vec![0u8; 4], "view"),
            (vec![0u8; 4], "outcome"),
        ];
        // Each fixture has a valid fixed prefix followed by a hostile length
        // prefix for its first variable field.
        cases[0].0 = u32::MAX.to_le_bytes().to_vec();
        cases[1].0 = [0u32.to_le_bytes().as_slice(), &u32::MAX.to_le_bytes(), &[]].concat();
        cases[2].0 = [0u8; 32]
            .into_iter()
            .chain(0u32.to_le_bytes())
            .chain(0u32.to_le_bytes())
            .chain(u32::MAX.to_le_bytes())
            .collect();
        cases[3].0 = [0u32.to_le_bytes().as_slice(), &u32::MAX.to_le_bytes()].concat();
        cases[4].0 = [0u32.to_le_bytes().as_slice(), &u32::MAX.to_le_bytes()].concat();
        cases[5].0 = [0u32.to_le_bytes().as_slice(), &u32::MAX.to_le_bytes()].concat();

        assert!(borsh::from_slice::<InitInput>(&cases[0].0).is_err());
        assert!(borsh::from_slice::<SharedInput>(&cases[1].0).is_err());
        assert!(borsh::from_slice::<LocalInput>(&cases[2].0).is_err());
        assert!(borsh::from_slice::<QueryInput>(&cases[3].0).is_err());
        assert!(borsh::from_slice::<ViewInput>(&cases[4].0).is_err());
        assert!(borsh::from_slice::<OutcomeInput>(&cases[5].0).is_err());
    }

    #[test]
    fn output_length_prefixes_are_checked_before_allocation() {
        let status = [0u8];
        let state = 0u32.to_le_bytes();
        let mut shared = status.to_vec();
        shared.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut local = status.to_vec();
        local.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut query = 0u32.to_le_bytes().to_vec();
        query.extend_from_slice(&u32::MAX.to_le_bytes());
        let view = u32::MAX.to_le_bytes().to_vec();
        let mut outcome = (MAX_CALL_PAYLOAD_BYTES as u32 + 1).to_le_bytes().to_vec();
        outcome.extend_from_slice(&state);

        assert!(borsh::from_slice::<SharedOutput>(&shared).is_err());
        assert!(borsh::from_slice::<LocalOutput>(&local).is_err());
        assert!(borsh::from_slice::<QueryOutput>(&query).is_err());
        assert!(borsh::from_slice::<ViewOutput>(&view).is_err());
        assert!(borsh::from_slice::<OutcomeOutput>(&outcome).is_err());
    }
}
