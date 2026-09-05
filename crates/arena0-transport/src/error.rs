//! Error type for the transport domain.

use std::sync::Arc;

use thiserror::Error;

/// Every failure produced by a transport operation.
#[derive(Error, Debug, Clone)]
#[non_exhaustive]
pub enum TransportError {
    /// Failed to establish a connection to a peer.
    #[error("connection failed: {0}")]
    ConnectionFailed(Arc<dyn std::error::Error + Send + Sync>),
    /// The connection was closed (locally or by the remote).
    #[error("connection closed")]
    ConnectionClosed,
    /// The receiver declined durable responsibility for an execution frame.
    #[error("execution frame was rejected by the receiver")]
    ExecRejected,
    /// The receiver found conflicting durable evidence for an execution frame.
    #[error("execution frame conflicts with receiver evidence")]
    ExecConflict,
    /// The receiver read the frame but dropped it before deciding durable
    /// responsibility.
    #[error("receiver dropped execution frame before accepting it")]
    ExecReceiverDropped,
    /// Sending a message over an established connection failed.
    #[error("send failed: {0}")]
    SendFailed(Arc<dyn std::error::Error + Send + Sync>),
    /// Receiving a message from an established connection failed.
    #[error("receive failed: {0}")]
    ReceiveFailed(Arc<dyn std::error::Error + Send + Sync>),
    /// No connection exists for the given peer.
    #[error("peer not found: {0}")]
    PeerNotFound(String),
    /// The operation exceeded its deadline.
    #[error("timeout after {0}ms")]
    Timeout(u64),
    /// The remote peer speaks an incompatible protocol version.
    #[error("protocol mismatch: {0}")]
    ProtocolMismatch(String),
    /// A decoded frame does not satisfy the protocol-domain contract.
    #[error("invalid protocol frame: {0}")]
    InvalidFrame(String),
    /// The message exceeds the maximum allowed size.
    #[error("payload too large: {size} bytes (max {max})")]
    PayloadTooLarge { size: usize, max: usize },
    /// The program-topic handle or its owner is closed.
    #[error("program gossip topic is closed")]
    TopicClosed,
    /// The declared blob size exceeds the caller's bound.
    #[error("declared blob length {declared} exceeds limit {max}")]
    BlobInvalidLength { declared: u64, max: u64 },
    /// The stored or imported blob exceeds the caller's bound.
    #[error("blob size {size} exceeds limit {max}")]
    BlobTooLarge { size: u64, max: u64 },
    /// The requested blob is not complete in the local store.
    #[error("blob is not found: {hash:?}")]
    BlobNotFound { hash: [u8; 32] },
    /// No provider was supplied for a blob that is not already local.
    #[error("no provider was supplied for the blob")]
    MissingBlobProvider,
    /// The bytes do not hash to the requested content address.
    #[error("blob hash mismatch: expected {expected:?}, got {actual:?}")]
    BlobHashMismatch {
        expected: [u8; 32],
        actual: [u8; 32],
    },
    /// The bytes do not have the requested exact length.
    #[error("blob length mismatch: expected {expected}, got {actual}")]
    BlobLengthMismatch { expected: u64, actual: u64 },
}

impl TransportError {
    /// Wrap a connection error, preserving the source.
    #[track_caller]
    pub fn connection_failed(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::ConnectionFailed(Arc::new(err))
    }

    /// Wrap a send error, preserving the source.
    #[track_caller]
    pub fn send_failed(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::SendFailed(Arc::new(err))
    }

    /// Wrap a receive error, preserving the source.
    #[track_caller]
    pub fn receive_failed(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::ReceiveFailed(Arc::new(err))
    }
}

impl From<arena0_wire::WireError> for TransportError {
    fn from(error: arena0_wire::WireError) -> Self {
        match error {
            arena0_wire::WireError::PayloadTooLarge { size, max }
            | arena0_wire::WireError::ValueTooLarge {
                size,
                max,
                field: _,
            } => Self::PayloadTooLarge { size, max },
            arena0_wire::WireError::CollectionTooLarge {
                size,
                max,
                field: _,
            } => Self::PayloadTooLarge { size, max },
            other => Self::ProtocolMismatch(other.to_string()),
        }
    }
}

impl From<arena0_protocol::ExecFrameError> for TransportError {
    fn from(error: arena0_protocol::ExecFrameError) -> Self {
        match error {
            arena0_protocol::ExecFrameError::Wire(error) => Self::from(error),
            arena0_protocol::ExecFrameError::Commitment(error) => Self::InvalidFrame(error),
            arena0_protocol::ExecFrameError::Abort(error) => Self::InvalidFrame(error),
        }
    }
}

impl From<arena0_protocol::FetchFrameError> for TransportError {
    fn from(error: arena0_protocol::FetchFrameError) -> Self {
        match error {
            arena0_protocol::FetchFrameError::Wire(error) => Self::from(error),
            other => Self::InvalidFrame(other.to_string()),
        }
    }
}
