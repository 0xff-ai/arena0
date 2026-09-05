//! We provide the transport abstraction that connects arena0 runtime nodes.
//! Programs never see the network; this crate is runtime-only infrastructure.
//!
//! The P1 transport is an in-process virtual network for local ensembles.
//! [`LocalTransport`] supplies topic membership, blob convergence, and peer
//! streams through bounded channels. Wire values and canonical frame encoding
//! live in [`arena0_wire`].

/// Bounded content-addressed storage and transfer.
mod blobs;
/// Transport-domain error type.
pub mod error;
/// In-process channel-based transport for local ensembles.
pub mod local;
/// Transport-neutral program negotiation effects.
pub mod negotiation;

pub use error::TransportError;
pub use local::{LocalNetwork, LocalTransport};
pub use negotiation::{
    MAX_PROGRAM_BOOTSTRAP_PEERS, NegotiationTopic, ProgramTopicEvent, TopicFact,
};

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arena0_program::ProgramHash;
use arena0_protocol::{
    ExecFrame as DomainExecFrame, FetchFrame as DomainFetchFrame, NegotiationId, PeerId,
    SessionHash,
};
use arena0_wire::{
    Codec, ExecFrame as WireExecFrame, FetchFrame as WireFetchFrame, StreamProtocol,
};
use bytes::Bytes;
use tokio::sync::{Mutex, mpsc, oneshot, watch};

/// Routing metadata established by the transport when an execution stream is
/// accepted. The session key and remote peer are not decoded from an
/// execution frame and cannot be supplied by a caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExecStreamMetadata {
    session_hash: SessionHash,
    remote_peer: PeerId,
}

impl ExecStreamMetadata {
    fn new(session_hash: SessionHash, remote_peer: PeerId) -> Self {
        Self {
            session_hash,
            remote_peer,
        }
    }

    /// The session selected by the authenticated stream opener.
    #[must_use]
    pub const fn session_hash(&self) -> SessionHash {
        self.session_hash
    }

    /// The peer authenticated by the transport route.
    #[must_use]
    pub const fn remote_peer(&self) -> PeerId {
        self.remote_peer
    }
}

/// An accepted execution stream and the route metadata that selected it.
///
/// The stream handle is owned by this value and is not cloneable. Callers can
/// split it once with [`Self::into_parts`] after routing by session.
#[derive(Debug)]
#[must_use]
pub struct AcceptedExecStream {
    metadata: ExecStreamMetadata,
    recv: RecvHandle,
}

impl AcceptedExecStream {
    fn new(metadata: ExecStreamMetadata, recv: RecvHandle) -> Self {
        Self { metadata, recv }
    }

    /// Borrow the transport-authenticated stream metadata.
    #[must_use]
    pub const fn metadata(&self) -> &ExecStreamMetadata {
        &self.metadata
    }

    /// Consume the accepted stream and return metadata plus its sole receive
    /// handle.
    pub fn into_parts(self) -> (ExecStreamMetadata, RecvHandle) {
        (self.metadata, self.recv)
    }
}

/// A receiver-side decision that does not grant durable responsibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecDeliveryRejection {
    /// The receiver declined the frame without a conflicting durable record.
    Rejected,
    /// The frame conflicts with an existing durable record.
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecDeliveryFailure {
    Rejected,
    Conflict,
    ReceiverDropped,
}

/// One packet's sender-side responsibility result.
#[derive(Debug)]
struct ExecDeliveryReceipt {
    sender: Option<oneshot::Sender<Result<(), ExecDeliveryFailure>>>,
}

impl ExecDeliveryReceipt {
    fn new(sender: oneshot::Sender<Result<(), ExecDeliveryFailure>>) -> Self {
        Self {
            sender: Some(sender),
        }
    }

    fn acknowledge(mut self) -> Result<(), TransportError> {
        let Some(sender) = self.sender.take() else {
            return Err(TransportError::ConnectionClosed);
        };
        sender
            .send(Ok(()))
            .map_err(|_| TransportError::ConnectionClosed)
    }

    fn reject(mut self, rejection: ExecDeliveryRejection) -> Result<(), TransportError> {
        let Some(sender) = self.sender.take() else {
            return Err(TransportError::ConnectionClosed);
        };
        let failure = match rejection {
            ExecDeliveryRejection::Rejected => ExecDeliveryFailure::Rejected,
            ExecDeliveryRejection::Conflict => ExecDeliveryFailure::Conflict,
        };
        sender
            .send(Err(failure))
            .map_err(|_| TransportError::ConnectionClosed)
    }
}

impl Drop for ExecDeliveryReceipt {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Err(ExecDeliveryFailure::ReceiverDropped));
        }
    }
}

/// The packet carried by one stream channel.
///
/// Fetch packets do not carry a responsibility sender. Exec packets always
/// carry one, and the sender waits for it before reporting success.
#[derive(Debug)]
pub(crate) struct StreamPacket {
    pub(crate) bytes: Vec<u8>,
    pub(crate) responsibility: Option<oneshot::Sender<Result<(), ExecDeliveryFailure>>>,
}

/// State shared by the two handles of one local stream.
#[derive(Debug)]
pub(crate) struct StreamState {
    closed: AtomicBool,
    closed_tx: watch::Sender<bool>,
}

impl StreamState {
    pub(crate) fn new() -> Self {
        let (closed_tx, _closed_rx) = watch::channel(false);
        Self {
            closed: AtomicBool::new(false),
            closed_tx,
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn close(&self) -> bool {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.closed_tx.send_replace(true);
            true
        } else {
            false
        }
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<bool> {
        self.closed_tx.subscribe()
    }
}

/// One inbound execution frame and its one-shot durable-responsibility
/// capability.
///
/// The value is deliberately not cloneable. Dropping it without calling
/// [`Self::acknowledge`] or [`Self::reject`] reports [`TransportError::ExecReceiverDropped`]
/// to the sender, so a receiver cannot accidentally acknowledge a frame by
/// merely reading it.
#[must_use]
pub struct ExecDelivery {
    source: PeerId,
    frame: DomainExecFrame,
    responsibility: ExecDeliveryReceipt,
}

impl fmt::Debug for ExecDelivery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecDelivery")
            .field("source", &self.source)
            .field("frame", &self.frame)
            .finish_non_exhaustive()
    }
}

impl ExecDelivery {
    /// Return the peer authenticated by the local network route.
    #[must_use]
    pub const fn source(&self) -> PeerId {
        self.source
    }

    /// Borrow the decoded execution frame.
    #[must_use]
    pub const fn frame(&self) -> &DomainExecFrame {
        &self.frame
    }

    /// Confirm durable responsibility for this frame.
    ///
    /// This operation only communicates the receiver's already-completed
    /// durable-store result. It does not perform persistence itself; callers
    /// must invoke it after their store operation succeeds.
    pub fn acknowledge(self) -> Result<(), TransportError> {
        self.responsibility.acknowledge()
    }

    /// Decline durable responsibility for this frame.
    pub fn reject(self, rejection: ExecDeliveryRejection) -> Result<(), TransportError> {
        self.responsibility.reject(rejection)
    }
}

/// Handle to the outbound (send) side of a unidirectional stream.
#[derive(Debug, Clone)]
#[must_use]
pub struct SendHandle {
    pub(crate) local: PeerId,
    pub(crate) remote: PeerId,
    pub(crate) connection_id: u64,
    pub(crate) stream_id: u64,
    pub(crate) proto: StreamProtocol,
    pub(crate) session_hash: Option<SessionHash>,
    pub(crate) tx: mpsc::Sender<StreamPacket>,
    pub(crate) state: Arc<StreamState>,
}

impl PartialEq for SendHandle {
    fn eq(&self, other: &Self) -> bool {
        self.connection_id == other.connection_id && self.stream_id == other.stream_id
    }
}

impl Eq for SendHandle {}

impl std::hash::Hash for SendHandle {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.connection_id.hash(state);
        self.stream_id.hash(state);
    }
}

impl SendHandle {
    /// The remote peer on this connection.
    pub const fn remote_peer(&self) -> &PeerId {
        &self.remote
    }

    /// The local peer on this connection.
    pub const fn local_peer(&self) -> &PeerId {
        &self.local
    }

    /// Which protocol this stream carries.
    pub const fn proto(&self) -> StreamProtocol {
        self.proto
    }

    /// The execution session bound at stream open, or `None` for fetch
    /// streams.
    #[must_use]
    pub const fn session_hash(&self) -> Option<SessionHash> {
        self.session_hash
    }

    /// Send an [`arena0_protocol::ExecFrame`] on an `Exec` stream.
    pub async fn send_exec(&self, msg: &DomainExecFrame) -> Result<(), TransportError> {
        self.validate_exec_route(msg)?;
        let wire = WireExecFrame::try_from(msg)?;
        if self.proto != StreamProtocol::Exec {
            return Err(TransportError::ProtocolMismatch(format!(
                "sent {:?} frame on a {:?} stream",
                StreamProtocol::Exec,
                self.proto
            )));
        }
        let frame = Codec::new(StreamProtocol::Exec.max_frame_body()).encode(&wire)?;
        let (responsibility, receipt) = oneshot::channel();
        self.send_packet(StreamPacket {
            bytes: frame,
            responsibility: Some(responsibility),
        })
        .await?;
        if self.state.is_closed() {
            return Err(TransportError::ConnectionClosed);
        }
        let mut closed = self.state.subscribe();
        match tokio::select! {
            result = receipt => result,
            result = closed.changed() => {
                let _ = result;
                return Err(TransportError::ConnectionClosed);
            }
        } {
            Ok(Ok(_)) if self.state.is_closed() => Err(TransportError::ConnectionClosed),
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(ExecDeliveryFailure::Rejected)) => Err(TransportError::ExecRejected),
            Ok(Err(ExecDeliveryFailure::Conflict)) => Err(TransportError::ExecConflict),
            Ok(Err(ExecDeliveryFailure::ReceiverDropped)) => {
                Err(TransportError::ExecReceiverDropped)
            }
            Err(_) => Err(TransportError::ConnectionClosed),
        }
    }

    /// Send an [`arena0_protocol::FetchFrame`] on a `Fetch` stream (the convergence fetch).
    pub async fn send_fetch(&self, msg: &DomainFetchFrame) -> Result<(), TransportError> {
        let wire = WireFetchFrame::try_from(msg)?;
        self.send_typed(StreamProtocol::Fetch, &wire).await
    }

    async fn send_typed<T: borsh::BorshSerialize>(
        &self,
        expected: StreamProtocol,
        msg: &T,
    ) -> Result<(), TransportError> {
        if self.proto != expected {
            return Err(TransportError::ProtocolMismatch(format!(
                "sent {expected:?} frame on a {:?} stream",
                self.proto
            )));
        }
        let frame = Codec::new(expected.max_frame_body()).encode(msg)?;
        self.send_packet(StreamPacket {
            bytes: frame,
            responsibility: None,
        })
        .await
    }

    async fn send_packet(&self, packet: StreamPacket) -> Result<(), TransportError> {
        if self.state.is_closed() {
            return Err(TransportError::ConnectionClosed);
        }
        let mut closed = self.state.subscribe();
        if self.state.is_closed() {
            return Err(TransportError::ConnectionClosed);
        }
        tokio::select! {
            result = self.tx.send(packet) => {
                result.map_err(|_| TransportError::ConnectionClosed)
            }
            result = closed.changed() => {
                let _ = result;
                Err(TransportError::ConnectionClosed)
            }
        }
    }

    fn validate_exec_route(&self, frame: &DomainExecFrame) -> Result<(), TransportError> {
        let Some(session_hash) = self.session_hash else {
            return Err(TransportError::ProtocolMismatch(
                "execution handle has no session binding".into(),
            ));
        };
        validate_exec_route(frame, session_hash, self.local)
    }
}

/// Handle to the inbound (recv) side of a unidirectional stream.
#[derive(Debug)]
#[must_use]
pub struct RecvHandle {
    pub(crate) local: PeerId,
    pub(crate) remote: PeerId,
    pub(crate) connection_id: u64,
    pub(crate) stream_id: u64,
    pub(crate) proto: StreamProtocol,
    pub(crate) session_hash: Option<SessionHash>,
    pub(crate) rx: Mutex<mpsc::Receiver<StreamPacket>>,
    pub(crate) state: Arc<StreamState>,
}

impl PartialEq for RecvHandle {
    fn eq(&self, other: &Self) -> bool {
        self.connection_id == other.connection_id && self.stream_id == other.stream_id
    }
}

impl Eq for RecvHandle {}

impl std::hash::Hash for RecvHandle {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.connection_id.hash(state);
        self.stream_id.hash(state);
    }
}

impl RecvHandle {
    /// The remote peer on this connection.
    pub const fn remote_peer(&self) -> &PeerId {
        &self.remote
    }

    /// The local peer on this connection.
    pub const fn local_peer(&self) -> &PeerId {
        &self.local
    }

    /// Which protocol this stream carries.
    pub const fn proto(&self) -> StreamProtocol {
        self.proto
    }

    /// The execution session bound at stream open, or `None` for fetch
    /// streams.
    #[must_use]
    pub const fn session_hash(&self) -> Option<SessionHash> {
        self.session_hash
    }

    /// Receive the next [`arena0_protocol::ExecFrame`] on an `Exec` stream.
    pub async fn recv_exec(&self) -> Result<ExecDelivery, TransportError> {
        let packet = self.recv_packet(StreamProtocol::Exec).await?;
        let Some(responsibility) = packet.responsibility else {
            return Err(TransportError::ProtocolMismatch(
                "exec packet is missing its responsibility receipt".into(),
            ));
        };
        let frame = match Codec::new(StreamProtocol::Exec.max_frame_body())
            .decode::<WireExecFrame>(&packet.bytes)
        {
            Ok(frame) => match DomainExecFrame::try_from(frame) {
                Ok(frame) => {
                    if let Err(error) = self.validate_exec_route(&frame) {
                        let _ = responsibility.send(Err(ExecDeliveryFailure::Rejected));
                        return Err(error);
                    }
                    frame
                }
                Err(error) => {
                    let _ = responsibility.send(Err(ExecDeliveryFailure::Rejected));
                    return Err(error.into());
                }
            },
            Err(error) => {
                let _ = responsibility.send(Err(ExecDeliveryFailure::Rejected));
                return Err(error.into());
            }
        };
        Ok(ExecDelivery {
            source: self.remote,
            frame,
            responsibility: ExecDeliveryReceipt::new(responsibility),
        })
    }

    /// Receive the next [`arena0_protocol::FetchFrame`] on a `Fetch` stream (the convergence
    /// fetch).
    pub async fn recv_fetch(&self) -> Result<DomainFetchFrame, TransportError> {
        let packet = self.recv_packet(StreamProtocol::Fetch).await?;
        if let Some(responsibility) = packet.responsibility {
            let _ = responsibility.send(Err(ExecDeliveryFailure::Rejected));
            return Err(TransportError::ProtocolMismatch(
                "fetch packet unexpectedly carries an exec responsibility receipt".into(),
            ));
        }
        let frame: WireFetchFrame =
            Codec::new(StreamProtocol::Fetch.max_frame_body()).decode(&packet.bytes)?;
        Ok(DomainFetchFrame::try_from(frame)?)
    }

    async fn recv_packet(&self, expected: StreamProtocol) -> Result<StreamPacket, TransportError> {
        if self.proto != expected {
            return Err(TransportError::ProtocolMismatch(format!(
                "received {expected:?} frame on a {:?} stream",
                self.proto
            )));
        }
        if self.state.is_closed() {
            return Err(TransportError::ConnectionClosed);
        }
        let mut rx = self.rx.lock().await;
        let mut closed = self.state.subscribe();
        if self.state.is_closed() {
            return Err(TransportError::ConnectionClosed);
        }
        let packet = tokio::select! {
            packet = rx.recv() => packet.ok_or(TransportError::ConnectionClosed),
            result = closed.changed() => {
                let _ = result;
                Err(TransportError::ConnectionClosed)
            }
        }?;
        if self.state.is_closed() {
            drop(packet);
            return Err(TransportError::ConnectionClosed);
        }
        Ok(packet)
    }

    fn validate_exec_route(&self, frame: &DomainExecFrame) -> Result<(), TransportError> {
        let Some(session_hash) = self.session_hash else {
            return Err(TransportError::ProtocolMismatch(
                "execution handle has no session binding".into(),
            ));
        };
        validate_exec_route(frame, session_hash, self.remote)
    }
}

fn validate_exec_route(
    frame: &DomainExecFrame,
    session_hash: SessionHash,
    authenticated_sender: PeerId,
) -> Result<(), TransportError> {
    match frame {
        DomainExecFrame::Message { .. } => {}
        DomainExecFrame::StepSignature { commitment, .. } => {
            if commitment.session_id != session_hash {
                return Err(TransportError::ProtocolMismatch(
                    "step signature does not match the execution stream session".into(),
                ));
            }
        }
        DomainExecFrame::End { commitment, .. } => {
            if commitment.session_id != session_hash {
                return Err(TransportError::ProtocolMismatch(
                    "terminal signature does not match the execution stream session".into(),
                ));
            }
        }
        DomainExecFrame::Abort { occurrence } => {
            if occurrence.session_id() != session_hash {
                return Err(TransportError::ProtocolMismatch(
                    "abort does not match the execution stream session".into(),
                ));
            }
            if occurrence.sender() != authenticated_sender {
                return Err(TransportError::ProtocolMismatch(
                    "abort sender does not match the authenticated stream peer".into(),
                ));
            }
        }
    }
    Ok(())
}

impl Drop for RecvHandle {
    fn drop(&mut self) {
        self.state.close();
    }
}

/// Async transport abstraction for peer-to-peer communication.
///
/// Implementations manage connection pooling internally. Callers use the
/// typed `open_exec`/`accept_exec` and `open_fetch`/`accept_fetch` operations;
/// execution session routing is established by the control plane before any
/// one-fact frame is delivered.
#[async_trait::async_trait]
pub trait Transport: Send {
    /// Subscribe to the one topic derived from `program_id`.
    async fn subscribe_program(
        &self,
        program_id: ProgramHash,
        bootstrap: Vec<PeerId>,
    ) -> Result<Box<dyn NegotiationTopic>, TransportError>;

    /// Import bounded bytes and retain them for one negotiation.
    async fn import_blob(
        &self,
        negotiation_id: NegotiationId,
        bytes: Bytes,
        max_bytes: u64,
    ) -> Result<[u8; 32], TransportError>;

    /// Fetch one exact bounded blob from at most two providers and retain it
    /// for one negotiation.
    async fn fetch_blob(
        &self,
        negotiation_id: NegotiationId,
        hash: [u8; 32],
        declared_len: u64,
        providers: Vec<PeerId>,
        max_bytes: u64,
    ) -> Result<(), TransportError>;

    /// Read one complete local blob through a bounded buffer.
    async fn read_blob(&self, hash: [u8; 32], max_bytes: u64) -> Result<Bytes, TransportError>;

    /// Retain a complete local blob for one session.
    async fn retain_session_blob(
        &self,
        session_id: SessionHash,
        hash: [u8; 32],
    ) -> Result<(), TransportError>;

    /// Release every blob retained for a negotiation that has ended.
    async fn release_negotiation_blobs(
        &self,
        negotiation_id: NegotiationId,
    ) -> Result<(), TransportError>;

    /// Release every blob retained for a session that has ended.
    async fn release_session_blobs(&self, session_id: SessionHash) -> Result<(), TransportError>;

    /// Open a new outbound execution stream to `peer` for `session_hash`.
    ///
    /// The session key is control-plane metadata written at stream open; no
    /// execution handshake frame is needed or accepted.
    async fn open_exec(
        &self,
        peer: &PeerId,
        session_hash: SessionHash,
    ) -> Result<SendHandle, TransportError>;

    /// Accept the next inbound execution stream and its authenticated route
    /// metadata.
    async fn accept_exec(&self) -> Result<AcceptedExecStream, TransportError>;

    /// Open a new outbound convergence-fetch stream to `peer`.
    async fn open_fetch(&self, peer: &PeerId) -> Result<SendHandle, TransportError>;

    /// Accept the next inbound convergence-fetch stream.
    async fn accept_fetch(&self) -> Result<RecvHandle, TransportError>;

    /// Gracefully shut down the transport. Default is a no-op.
    async fn close(&self) {}
}
