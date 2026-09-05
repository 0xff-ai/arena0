//! Transport-neutral negotiation effects.
//!
//! Runtime code publishes bounded opaque facts to a program topic and observes
//! topic membership through this boundary. The local implementation is the P1
//! topic; a future remote implementation can use the same contract.

use arena0_protocol::{MAX_NEGOTIATION_FACT_BYTES, PeerId};
use async_trait::async_trait;
use bytes::Bytes;

use crate::TransportError;

/// Maximum number of distinct bootstrap peers accepted for one topic join.
pub const MAX_PROGRAM_BOOTSTRAP_PEERS: usize = 8;

/// One opaque fact delivered by a program topic.
///
/// The bytes carry the signed Arena0 fact. `delivered_from` is only the topic
/// delivery peer and is never an authentication identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicFact {
    /// The peer that delivered this fact to the local topic subscription.
    pub delivered_from: PeerId,
    /// Opaque, bounded application bytes.
    pub bytes: Bytes,
}

/// Events emitted by one program topic subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramTopicEvent {
    /// The topic has a direct neighbor and is joined.
    Joined,
    /// A direct topic neighbor became available.
    NeighborUp(PeerId),
    /// A direct topic neighbor became unavailable.
    NeighborDown(PeerId),
    /// An opaque fact arrived from a topic neighbor.
    Fact(TopicFact),
    /// The bounded topic queue overflowed and the generation was closed.
    Lagged,
    /// The topic generation has ended.
    Closed,
}

/// Canonicalize the bootstrap peers before they reach a topic implementation.
///
/// Sorting and deduplicating makes the input independent of caller order;
/// the cap keeps one join bounded.
pub(crate) fn canonical_bootstrap<I>(input: I) -> Vec<PeerId>
where
    I: IntoIterator<Item = PeerId>,
{
    let mut peers = input.into_iter().collect::<Vec<_>>();
    peers.sort_unstable();
    peers.dedup();
    peers.truncate(MAX_PROGRAM_BOOTSTRAP_PEERS);
    peers
}

pub(crate) fn validate_fact_size(bytes: &Bytes) -> Result<(), TransportError> {
    if bytes.len() > MAX_NEGOTIATION_FACT_BYTES {
        return Err(TransportError::PayloadTooLarge {
            size: bytes.len(),
            max: MAX_NEGOTIATION_FACT_BYTES,
        });
    }
    Ok(())
}

/// One owned subscription to a program topic.
#[async_trait]
pub trait NegotiationTopic: Send {
    /// Publish one already-encoded, bounded negotiation fact.
    async fn publish(&self, bytes: Bytes) -> Result<(), TransportError>;

    /// Receive the next topic-generation event.
    async fn recv(&mut self) -> Result<ProgramTopicEvent, TransportError>;

    /// Add a bounded set of newly resolved bootstrap peers once.
    async fn join_peers(&self, peers: Vec<PeerId>) -> Result<(), TransportError>;

    /// Close this handle and await its owner when this is the last handle.
    async fn close(&mut self) -> Result<(), TransportError>;
}
