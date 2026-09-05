//! In-process channel-based transport for local ensembles.
//!
//! [`LocalTransport`] instances connect through a shared [`LocalNetwork`]
//! using tokio mpsc channels. No sockets; messages are Borsh-serialized into
//! `Vec<u8>` frames so the same codec path is exercised as in production.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use arena0_program::ProgramHash;
use arena0_protocol::{NegotiationId, PeerId, SessionHash};
use arena0_wire::StreamProtocol;
use bytes::Bytes;
use tokio::sync::{Mutex, mpsc};

use crate::TopicFact;
use crate::blobs::{
    ordered_unique_providers, validate_actual_size, validate_declared_length, validate_stored_size,
};
use crate::negotiation::canonical_bootstrap;
use crate::negotiation::validate_fact_size;
use crate::{
    AcceptedExecStream, ExecStreamMetadata, NegotiationTopic, ProgramTopicEvent, RecvHandle,
    SendHandle, StreamState, Transport, TransportError,
};

const NEGOTIATION_EVENT_QUEUE_CAP: usize = 256;

/// Return a guard even if an unrelated test panicked while holding a local
/// network lock. A poisoned local network remains useful for diagnostics.
fn lock_unpoisoned<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Inbound control-plane stream payload. Fetch and execution streams have
/// separate queues so accepting one kind can never consume or drop the other.
enum InboundPayload {
    Exec(AcceptedExecStream),
    Fetch(RecvHandle),
}

#[derive(Debug, Clone)]
struct InboundSenders {
    exec: mpsc::Sender<InboundPayload>,
    fetch: mpsc::Sender<InboundPayload>,
}

/// Shared local network that multiple [`LocalTransport`] instances
/// connect through.
///
/// Tracks topic membership, local blob stores, and bounded stream channels.
/// Cheaply cloneable via internal `Arc`.
#[derive(Debug, Clone)]
pub struct LocalNetwork {
    inner: Arc<LocalNetworkInner>,
}

#[derive(Debug)]
struct LocalNetworkInner {
    next_conn_id: AtomicU64,
    next_stream_id: AtomicU64,
    channel_capacity: usize,
    topics: StdMutex<HashMap<ProgramHash, LocalTopicBroker>>,
    blobs: StdMutex<HashMap<PeerId, LocalPeerBlobs>>,
}

#[derive(Debug, Default)]
struct LocalTopicBroker {
    next_subscription_id: AtomicUsize,
    subscribers: BTreeMap<usize, LocalTopicSubscriber>,
}

#[derive(Debug)]
struct LocalTopicSubscriber {
    peer_id: PeerId,
    events: mpsc::Sender<ProgramTopicEvent>,
    state: Arc<LocalTopicState>,
}

#[derive(Debug)]
struct LocalTopicState {
    joined: AtomicBool,
    closed: AtomicBool,
    forced_close: AtomicBool,
    lagged_reported: AtomicBool,
    closed_reported: AtomicBool,
}

impl Default for LocalTopicState {
    fn default() -> Self {
        Self {
            joined: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            forced_close: AtomicBool::new(false),
            lagged_reported: AtomicBool::new(false),
            closed_reported: AtomicBool::new(false),
        }
    }
}

impl LocalTopicState {
    fn force_close(&self) {
        self.forced_close.store(true, Ordering::Release);
        self.closed.store(true, Ordering::Release);
    }
}

impl LocalTopicSubscriber {
    fn enqueue_neighbor_up(&self, peer_id: PeerId) -> bool {
        if !self.state.joined.swap(true, Ordering::AcqRel)
            && self.events.try_send(ProgramTopicEvent::Joined).is_err()
        {
            return false;
        }
        self.events
            .try_send(ProgramTopicEvent::NeighborUp(peer_id))
            .is_ok()
    }
}

#[derive(Debug, Default)]
struct LocalPeerBlobs {
    blobs: HashMap<[u8; 32], Bytes>,
    negotiations: HashMap<NegotiationId, HashSet<[u8; 32]>>,
    sessions: HashMap<SessionHash, HashSet<[u8; 32]>>,
}

impl LocalNetwork {
    /// Create a network with the default channel capacity (256).
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(256)
    }

    /// Create a network with a custom per-connection channel capacity.
    ///
    /// A capacity of zero is not a valid bounded Tokio channel, so it is
    /// rejected immediately rather than silently changing the requested
    /// backpressure contract.
    #[must_use]
    pub fn with_capacity(channel_capacity: usize) -> Self {
        assert!(
            channel_capacity > 0,
            "local transport channel capacity must be non-zero"
        );
        Self {
            inner: Arc::new(LocalNetworkInner {
                next_conn_id: AtomicU64::new(1),
                next_stream_id: AtomicU64::new(1),
                channel_capacity,
                topics: StdMutex::new(HashMap::new()),
                blobs: StdMutex::new(HashMap::new()),
            }),
        }
    }

    fn next_conn_id(&self) -> u64 {
        self.inner.next_conn_id.fetch_add(1, Ordering::Relaxed)
    }

    fn next_stream_id(&self) -> u64 {
        self.inner.next_stream_id.fetch_add(1, Ordering::Relaxed)
    }

    fn ensure_blob_store(&self, peer_id: PeerId) {
        let mut blobs = lock_unpoisoned(&self.inner.blobs);
        blobs.entry(peer_id).or_default();
    }

    fn subscribe_topic(
        &self,
        program_id: ProgramHash,
        peer_id: PeerId,
        bootstrap: Vec<PeerId>,
    ) -> LocalNegotiationTopic {
        let (events, receiver) = mpsc::channel(NEGOTIATION_EVENT_QUEUE_CAP);
        let state = Arc::new(LocalTopicState::default());
        let bootstrap = canonical_bootstrap(bootstrap);

        let mut topics = lock_unpoisoned(&self.inner.topics);
        let broker = topics.entry(program_id).or_default();
        let subscription_id = broker.next_subscription_id.fetch_add(1, Ordering::Relaxed);

        // Registration and the initial event snapshot happen under one lock.
        // A publisher therefore cannot observe a half-created subscription.
        let registered = bootstrap
            .into_iter()
            .filter(|peer| {
                *peer != peer_id
                    && broker
                        .subscribers
                        .values()
                        .any(|subscriber| subscriber.peer_id == *peer)
            })
            .collect::<BTreeSet<_>>();
        broker.subscribers.insert(
            subscription_id,
            LocalTopicSubscriber {
                peer_id,
                events,
                state: Arc::clone(&state),
            },
        );
        let mut remove_new = false;
        for peer in &registered {
            let subscriber = broker
                .subscribers
                .get(&subscription_id)
                .expect("new subscription remains registered during join");
            if !subscriber.enqueue_neighbor_up(*peer) {
                remove_new = true;
                break;
            }
        }
        if remove_new && let Some(subscriber) = broker.subscribers.remove(&subscription_id) {
            subscriber.state.force_close();
        }
        let mut notify = Vec::new();
        if !remove_new {
            for (&id, subscriber) in &broker.subscribers {
                if id != subscription_id && registered.contains(&subscriber.peer_id) {
                    notify.push(id);
                }
            }
        }
        let mut remove = Vec::new();
        for id in notify {
            let subscriber = broker
                .subscribers
                .get(&id)
                .expect("neighbor subscription remains registered");
            if !subscriber.enqueue_neighbor_up(peer_id) {
                remove.push(id);
            }
        }
        for id in remove {
            if let Some(subscriber) = broker.subscribers.remove(&id) {
                subscriber.state.force_close();
            }
        }
        if broker.subscribers.is_empty() {
            topics.remove(&program_id);
        }
        drop(topics);

        LocalNegotiationTopic {
            network: self.clone(),
            program_id,
            peer_id,
            subscription_id,
            events: receiver,
            state,
        }
    }

    fn publish_topic(
        &self,
        program_id: ProgramHash,
        publisher: PeerId,
        subscription_id: usize,
        state: &Arc<LocalTopicState>,
        bytes: Bytes,
    ) -> Result<(), TransportError> {
        validate_fact_size(&bytes)?;

        let mut topics = lock_unpoisoned(&self.inner.topics);
        let Some(broker) = topics.get_mut(&program_id) else {
            return Err(TransportError::TopicClosed);
        };
        let Some(current) = broker.subscribers.get(&subscription_id) else {
            return Err(TransportError::TopicClosed);
        };
        if current.peer_id != publisher || !Arc::ptr_eq(&current.state, state) {
            return Err(TransportError::TopicClosed);
        }

        let event = ProgramTopicEvent::Fact(TopicFact {
            delivered_from: publisher,
            bytes,
        });
        let mut remove = Vec::new();
        for (&id, subscriber) in &broker.subscribers {
            if id == subscription_id {
                continue;
            }
            match subscriber.events.try_send(event.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_) | mpsc::error::TrySendError::Closed(_)) => {
                    remove.push(id)
                }
            }
        }
        for id in remove {
            if let Some(subscriber) = broker.subscribers.remove(&id) {
                subscriber.state.force_close();
            }
        }
        if broker.subscribers.is_empty() {
            topics.remove(&program_id);
        }
        Ok(())
    }

    fn join_topic(
        &self,
        program_id: ProgramHash,
        subscription_id: usize,
        state: &Arc<LocalTopicState>,
        peers: Vec<PeerId>,
    ) -> Result<(), TransportError> {
        let peers = canonical_bootstrap(peers);
        let mut topics = lock_unpoisoned(&self.inner.topics);
        let Some(broker) = topics.get_mut(&program_id) else {
            return Err(TransportError::TopicClosed);
        };
        let Some(current) = broker.subscribers.get(&subscription_id) else {
            return Err(TransportError::TopicClosed);
        };
        if !Arc::ptr_eq(&current.state, state) || current.state.closed.load(Ordering::Acquire) {
            return Err(TransportError::TopicClosed);
        }

        let registered = peers
            .into_iter()
            .filter(|peer| {
                *peer != current.peer_id
                    && broker
                        .subscribers
                        .values()
                        .any(|subscriber| subscriber.peer_id == *peer)
            })
            .collect::<BTreeSet<_>>();
        let mut remove_current = false;
        for peer in registered {
            let subscriber = broker
                .subscribers
                .get(&subscription_id)
                .expect("subscription remains registered during join");
            if !subscriber.enqueue_neighbor_up(peer) {
                remove_current = true;
                break;
            }
        }
        if remove_current {
            if let Some(subscriber) = broker.subscribers.remove(&subscription_id) {
                subscriber.state.force_close();
            }
            if broker.subscribers.is_empty() {
                topics.remove(&program_id);
            }
        }
        Ok(())
    }

    fn close_topic(
        &self,
        program_id: ProgramHash,
        subscription_id: usize,
        state: &Arc<LocalTopicState>,
        send_closed: bool,
    ) {
        let mut topics = lock_unpoisoned(&self.inner.topics);
        let Some(broker) = topics.get_mut(&program_id) else {
            return;
        };
        let Some(current) = broker.subscribers.get(&subscription_id) else {
            return;
        };
        if !Arc::ptr_eq(&current.state, state) {
            return;
        }
        let removed_peer = current.peer_id;

        let mut forced = false;
        if send_closed && !state.forced_close.load(Ordering::Acquire) {
            match current.events.try_send(ProgramTopicEvent::Closed) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_) | mpsc::error::TrySendError::Closed(_)) => {
                    forced = true
                }
            }
        }
        broker.subscribers.remove(&subscription_id);
        if forced {
            state.force_close();
        } else {
            state.closed.store(true, Ordering::Release);
        }
        let mut remove = Vec::new();
        for (&id, subscriber) in &broker.subscribers {
            match subscriber
                .events
                .try_send(ProgramTopicEvent::NeighborDown(removed_peer))
            {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_) | mpsc::error::TrySendError::Closed(_)) => {
                    remove.push(id)
                }
            }
        }
        for id in remove {
            if let Some(subscriber) = broker.subscribers.remove(&id) {
                subscriber.state.force_close();
            }
        }
        if broker.subscribers.is_empty() {
            topics.remove(&program_id);
        }
    }
}

impl Default for LocalNetwork {
    fn default() -> Self {
        Self::new()
    }
}

/// In-process transport for local execution. Each instance represents a single peer
/// on a shared [`LocalNetwork`].
#[derive(Debug)]
pub struct LocalTransport {
    peer_id: PeerId,
    network: LocalNetwork,
    _exec_inbound_tx: mpsc::Sender<InboundPayload>,
    _fetch_inbound_tx: mpsc::Sender<InboundPayload>,
    exec_inbound_rx: Arc<Mutex<mpsc::Receiver<InboundPayload>>>,
    fetch_inbound_rx: Arc<Mutex<mpsc::Receiver<InboundPayload>>>,
    closed: Arc<StreamState>,
    /// Peer registry uses `std::sync::Mutex` so transport construction needs no
    /// async runtime.
    peers: Arc<StdMutex<HashMap<PeerId, InboundSenders>>>,
    streams: Arc<StdMutex<HashMap<PeerId, Vec<Weak<StreamState>>>>>,
}

impl LocalTransport {
    fn lock_peer_registry(
        peers: &StdMutex<HashMap<PeerId, InboundSenders>>,
    ) -> std::sync::MutexGuard<'_, HashMap<PeerId, InboundSenders>> {
        lock_unpoisoned(peers)
    }

    fn inbound_sender(
        &self,
        peer: &PeerId,
        proto: StreamProtocol,
    ) -> Result<mpsc::Sender<InboundPayload>, TransportError> {
        let peers = Self::lock_peer_registry(&self.peers);
        peers
            .get(peer)
            .map(|senders| match proto {
                StreamProtocol::Exec => senders.exec.clone(),
                StreamProtocol::Fetch => senders.fetch.clone(),
            })
            .ok_or_else(|| {
                TransportError::PeerNotFound(format!("peer {peer} not registered on this network"))
            })
    }

    fn register_stream(&self, peer: PeerId, state: &Arc<StreamState>) {
        let mut streams = lock_unpoisoned(&self.streams);
        streams.entry(peer).or_default().push(Arc::downgrade(state));
    }

    fn close_streams(&self, peer: PeerId) {
        let mut streams = lock_unpoisoned(&self.streams);
        let Some(states) = streams.get_mut(&peer) else {
            return;
        };
        states.retain(|state| {
            let Some(state) = state.upgrade() else {
                return false;
            };
            state.close();
            true
        });
    }

    /// Create N transports sharing the same network and peer registry.
    #[must_use]
    pub fn create_network(network: &LocalNetwork, peer_ids: Vec<PeerId>) -> Vec<Self> {
        let shared_peers: Arc<StdMutex<HashMap<PeerId, InboundSenders>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        let streams: Arc<StdMutex<HashMap<PeerId, Vec<Weak<StreamState>>>>> =
            Arc::new(StdMutex::new(HashMap::new()));

        let mut transports = Vec::with_capacity(peer_ids.len());

        for peer_id in peer_ids {
            let (exec_inbound_tx, exec_inbound_rx) = mpsc::channel(64);
            let (fetch_inbound_tx, fetch_inbound_rx) = mpsc::channel(64);
            {
                let mut peers = Self::lock_peer_registry(&shared_peers);
                peers.insert(
                    peer_id,
                    InboundSenders {
                        exec: exec_inbound_tx.clone(),
                        fetch: fetch_inbound_tx.clone(),
                    },
                );
            }
            network.ensure_blob_store(peer_id);

            transports.push(Self {
                peer_id,
                network: network.clone(),
                _exec_inbound_tx: exec_inbound_tx,
                _fetch_inbound_tx: fetch_inbound_tx,
                exec_inbound_rx: Arc::new(Mutex::new(exec_inbound_rx)),
                fetch_inbound_rx: Arc::new(Mutex::new(fetch_inbound_rx)),
                closed: Arc::new(StreamState::new()),
                peers: shared_peers.clone(),
                streams: streams.clone(),
            });
        }

        transports
    }

    /// This transport's peer identity.
    #[must_use]
    pub const fn peer_id(&self) -> &PeerId {
        &self.peer_id
    }

    async fn open_stream(
        &self,
        peer: &PeerId,
        proto: StreamProtocol,
        session_hash: Option<SessionHash>,
    ) -> Result<SendHandle, TransportError> {
        debug_assert_eq!(proto == StreamProtocol::Exec, session_hash.is_some());
        if self.closed.is_closed() {
            return Err(TransportError::ConnectionClosed);
        }
        if *peer == self.peer_id {
            return Err(TransportError::connection_failed(std::io::Error::other(
                "cannot open stream to self",
            )));
        }

        let connection_id = self.network.next_conn_id();
        let stream_id = self.network.next_stream_id();
        let cap = self.network.inner.channel_capacity;

        let (tx, rx) = mpsc::channel(cap);
        let state = Arc::new(StreamState::new());

        let send_handle = SendHandle {
            local: self.peer_id,
            remote: *peer,
            connection_id,
            stream_id,
            proto,
            session_hash,
            tx,
            state: Arc::clone(&state),
        };
        let recv_handle = RecvHandle {
            local: *peer,
            remote: self.peer_id,
            connection_id,
            stream_id,
            proto,
            session_hash,
            rx: Mutex::new(rx),
            state: Arc::clone(&state),
        };

        self.register_stream(self.peer_id, &state);
        self.register_stream(*peer, &state);

        // Register the shared state before handing the receiver to the remote
        // endpoint. A concurrent remote close must be able to close a stream
        // even while this bounded inbound queue is backpressured.
        let remote_tx = match self.inbound_sender(peer, proto) {
            Ok(sender) => sender,
            Err(error) => {
                state.close();
                return Err(error);
            }
        };
        let payload = match (proto, session_hash) {
            (StreamProtocol::Exec, Some(session_hash)) => {
                InboundPayload::Exec(AcceptedExecStream::new(
                    ExecStreamMetadata::new(session_hash, self.peer_id),
                    recv_handle,
                ))
            }
            (StreamProtocol::Fetch, None) => InboundPayload::Fetch(recv_handle),
            _ => unreachable!("stream protocol and session metadata must agree"),
        };
        if remote_tx.send(payload).await.is_err() {
            state.close();
            return Err(TransportError::connection_failed(std::io::Error::other(
                "remote peer is gone",
            )));
        }
        if state.is_closed() {
            return Err(TransportError::ConnectionClosed);
        }

        Ok(send_handle)
    }

    fn close_sync(&self) {
        if !self.closed.close() {
            return;
        }
        let mut peers = Self::lock_peer_registry(&self.peers);
        peers.remove(&self.peer_id);
        drop(peers);
        self.close_streams(self.peer_id);
    }
}

impl Drop for LocalTransport {
    fn drop(&mut self) {
        self.close_sync();
    }
}

fn validate_local_blob(
    hash: [u8; 32],
    bytes: &Bytes,
    declared_len: u64,
    max_bytes: u64,
) -> Result<(), TransportError> {
    let actual_len = bytes.len() as u64;
    validate_stored_size(actual_len, declared_len, max_bytes)?;
    let actual_hash = *blake3::hash(bytes).as_bytes();
    if actual_hash != hash {
        return Err(TransportError::BlobHashMismatch {
            expected: hash,
            actual: actual_hash,
        });
    }
    Ok(())
}

impl LocalPeerBlobs {
    fn collect_unreferenced(&mut self) {
        let retained = self
            .negotiations
            .values()
            .chain(self.sessions.values())
            .flatten()
            .copied()
            .collect::<HashSet<_>>();
        self.blobs.retain(|hash, _| retained.contains(hash));
    }
}

/// One subscription to a local program topic.
#[derive(Debug)]
struct LocalNegotiationTopic {
    network: LocalNetwork,
    program_id: ProgramHash,
    peer_id: PeerId,
    subscription_id: usize,
    events: mpsc::Receiver<ProgramTopicEvent>,
    state: Arc<LocalTopicState>,
}

impl LocalNegotiationTopic {
    async fn receive_event(&mut self) -> Result<ProgramTopicEvent, TransportError> {
        loop {
            if self.state.forced_close.load(Ordering::Acquire) {
                if !self.state.lagged_reported.swap(true, Ordering::AcqRel) {
                    return Ok(ProgramTopicEvent::Lagged);
                }
                if !self.state.closed_reported.swap(true, Ordering::AcqRel) {
                    return Ok(ProgramTopicEvent::Closed);
                }
                return Err(TransportError::TopicClosed);
            }

            match self.events.recv().await {
                Some(event) => return Ok(event),
                None if self.state.forced_close.load(Ordering::Acquire) => continue,
                None => return Err(TransportError::TopicClosed),
            }
        }
    }
}

#[async_trait::async_trait]
impl NegotiationTopic for LocalNegotiationTopic {
    async fn publish(&self, bytes: Bytes) -> Result<(), TransportError> {
        if self.state.closed.load(Ordering::Acquire) {
            return Err(TransportError::TopicClosed);
        }
        self.network.publish_topic(
            self.program_id,
            self.peer_id,
            self.subscription_id,
            &self.state,
            bytes,
        )
    }

    async fn recv(&mut self) -> Result<ProgramTopicEvent, TransportError> {
        self.receive_event().await
    }

    async fn join_peers(&self, peers: Vec<PeerId>) -> Result<(), TransportError> {
        if self.state.closed.load(Ordering::Acquire) {
            return Err(TransportError::TopicClosed);
        }
        self.network
            .join_topic(self.program_id, self.subscription_id, &self.state, peers)
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        if self.state.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.network
            .close_topic(self.program_id, self.subscription_id, &self.state, true);
        Ok(())
    }
}

impl Drop for LocalNegotiationTopic {
    fn drop(&mut self) {
        if !self.state.closed.swap(true, Ordering::AcqRel) {
            self.network
                .close_topic(self.program_id, self.subscription_id, &self.state, false);
        }
    }
}

#[async_trait::async_trait]
impl Transport for LocalTransport {
    async fn subscribe_program(
        &self,
        program_id: ProgramHash,
        bootstrap: Vec<PeerId>,
    ) -> Result<Box<dyn NegotiationTopic>, TransportError> {
        Ok(Box::new(self.network.subscribe_topic(
            program_id,
            self.peer_id,
            bootstrap,
        )))
    }

    async fn import_blob(
        &self,
        negotiation_id: NegotiationId,
        bytes: Bytes,
        max_bytes: u64,
    ) -> Result<[u8; 32], TransportError> {
        let actual_len = bytes.len() as u64;
        validate_actual_size(actual_len, max_bytes)?;
        let hash = *blake3::hash(&bytes).as_bytes();
        let mut blobs = lock_unpoisoned(&self.network.inner.blobs);
        let store = blobs.entry(self.peer_id).or_default();
        store.blobs.insert(hash, bytes);
        store
            .negotiations
            .entry(negotiation_id)
            .or_default()
            .insert(hash);
        Ok(hash)
    }

    async fn fetch_blob(
        &self,
        negotiation_id: NegotiationId,
        hash: [u8; 32],
        declared_len: u64,
        providers: Vec<PeerId>,
        max_bytes: u64,
    ) -> Result<(), TransportError> {
        validate_declared_length(declared_len, max_bytes)?;

        let providers = ordered_unique_providers(&providers);
        let mut blobs = lock_unpoisoned(&self.network.inner.blobs);
        let local = blobs
            .entry(self.peer_id)
            .or_default()
            .blobs
            .get(&hash)
            .cloned();
        if let Some(bytes) = local {
            validate_local_blob(hash, &bytes, declared_len, max_bytes)?;
            blobs
                .entry(self.peer_id)
                .or_default()
                .negotiations
                .entry(negotiation_id)
                .or_default()
                .insert(hash);
            return Ok(());
        }
        if providers.is_empty() {
            return Err(TransportError::MissingBlobProvider);
        }

        let mut last_error = None;
        let mut fetched = None;
        for provider in providers {
            let Some(bytes) = blobs
                .get(&provider)
                .and_then(|store| store.blobs.get(&hash))
                .cloned()
            else {
                continue;
            };
            match validate_local_blob(hash, &bytes, declared_len, max_bytes) {
                Ok(()) => {
                    fetched = Some(bytes);
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }

        let Some(bytes) = fetched else {
            return Err(last_error.unwrap_or(TransportError::BlobNotFound { hash }));
        };
        let store = blobs.entry(self.peer_id).or_default();
        store.blobs.insert(hash, bytes);
        store
            .negotiations
            .entry(negotiation_id)
            .or_default()
            .insert(hash);
        Ok(())
    }

    async fn read_blob(&self, hash: [u8; 32], max_bytes: u64) -> Result<Bytes, TransportError> {
        let blobs = lock_unpoisoned(&self.network.inner.blobs);
        let Some(bytes) = blobs
            .get(&self.peer_id)
            .and_then(|store| store.blobs.get(&hash))
            .cloned()
        else {
            return Err(TransportError::BlobNotFound { hash });
        };
        validate_local_blob(hash, &bytes, bytes.len() as u64, max_bytes)?;
        Ok(bytes)
    }

    async fn retain_session_blob(
        &self,
        session_id: SessionHash,
        hash: [u8; 32],
    ) -> Result<(), TransportError> {
        let mut blobs = lock_unpoisoned(&self.network.inner.blobs);
        let store = blobs.entry(self.peer_id).or_default();
        let bytes = store
            .blobs
            .get(&hash)
            .ok_or(TransportError::BlobNotFound { hash })?;
        validate_local_blob(hash, bytes, bytes.len() as u64, u64::MAX)?;
        store.sessions.entry(session_id).or_default().insert(hash);
        Ok(())
    }

    async fn release_negotiation_blobs(
        &self,
        negotiation_id: NegotiationId,
    ) -> Result<(), TransportError> {
        if let Some(store) = lock_unpoisoned(&self.network.inner.blobs).get_mut(&self.peer_id) {
            store.negotiations.remove(&negotiation_id);
            store.collect_unreferenced();
        }
        Ok(())
    }

    async fn release_session_blobs(&self, session_id: SessionHash) -> Result<(), TransportError> {
        if let Some(store) = lock_unpoisoned(&self.network.inner.blobs).get_mut(&self.peer_id) {
            store.sessions.remove(&session_id);
            store.collect_unreferenced();
        }
        Ok(())
    }

    async fn open_exec(
        &self,
        peer: &PeerId,
        session_hash: SessionHash,
    ) -> Result<SendHandle, TransportError> {
        self.open_stream(peer, StreamProtocol::Exec, Some(session_hash))
            .await
    }

    async fn open_fetch(&self, peer: &PeerId) -> Result<SendHandle, TransportError> {
        self.open_stream(peer, StreamProtocol::Fetch, None).await
    }

    async fn accept_exec(&self) -> Result<AcceptedExecStream, TransportError> {
        if self.closed.is_closed() {
            return Err(TransportError::ConnectionClosed);
        }
        let mut rx = self.exec_inbound_rx.lock().await;
        let mut closed = self.closed.subscribe();
        if self.closed.is_closed() {
            return Err(TransportError::ConnectionClosed);
        }
        tokio::select! {
            payload = rx.recv() => match payload {
                Some(InboundPayload::Exec(stream)) => Ok(stream),
                Some(InboundPayload::Fetch(_)) => unreachable!("fetch payload has its own queue"),
                None => Err(TransportError::ConnectionClosed),
            },
            result = closed.changed() => {
                let _ = result;
                Err(TransportError::ConnectionClosed)
            }
        }
    }

    async fn accept_fetch(&self) -> Result<RecvHandle, TransportError> {
        if self.closed.is_closed() {
            return Err(TransportError::ConnectionClosed);
        }
        let mut rx = self.fetch_inbound_rx.lock().await;
        let mut closed = self.closed.subscribe();
        if self.closed.is_closed() {
            return Err(TransportError::ConnectionClosed);
        }
        tokio::select! {
            payload = rx.recv() => match payload {
                Some(InboundPayload::Fetch(stream)) => Ok(stream),
                Some(InboundPayload::Exec(_)) => unreachable!("execution payload has its own queue"),
                None => Err(TransportError::ConnectionClosed),
            },
            result = closed.changed() => {
                let _ = result;
                Err(TransportError::ConnectionClosed)
            }
        }
    }

    async fn close(&self) {
        self.close_sync();
        self.exec_inbound_rx.lock().await.close();
        self.fetch_inbound_rx.lock().await.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_crypto::{NodeKeys, SecretKey};
    use arena0_protocol::{
        AbortKind, AbortOccurrence, ExecFrame, MessageId, PublicCursor, StateHash,
        WitnessCommitment,
    };
    use tokio::time::{Duration, timeout};

    fn peer(byte: u8) -> PeerId {
        PeerId([byte; 32])
    }

    fn transports() -> (LocalNetwork, Vec<LocalTransport>) {
        let network = LocalNetwork::new();
        let peers =
            LocalTransport::create_network(&network, vec![peer(1), peer(2), peer(3), peer(4)]);
        (network, peers)
    }

    fn message_frame(byte: u8) -> ExecFrame {
        ExecFrame::Message {
            message_id: MessageId([byte; 32]),
            seq: u64::from(byte),
            prestate: StateHash([byte.wrapping_add(1); 32]),
            data: vec![byte.wrapping_add(2)],
            witness: WitnessCommitment([byte.wrapping_add(3); 32]),
        }
    }

    fn abort_frame(session_hash: SessionHash, sender: PeerId) -> ExecFrame {
        let keys = NodeKeys::from_secret(SecretKey::from_bytes([42; 32]));
        let unsigned = AbortOccurrence::unsigned(
            session_hash,
            sender,
            AbortKind::Abort,
            1,
            "test abort",
            PublicCursor::new(0, StateHash([0; 32]), arena0_protocol::CHAIN_START),
        )
        .expect("abort occurrence");
        let signature = keys.sign(&unsigned.signing_bytes().expect("abort signing bytes"));
        ExecFrame::Abort {
            occurrence: unsigned.with_signature(signature).expect("signed abort"),
        }
    }

    async fn joined(topic: &mut Box<dyn NegotiationTopic>) {
        assert_eq!(topic.recv().await.unwrap(), ProgramTopicEvent::Joined);
    }

    #[tokio::test]
    async fn negotiation_pubsub_fans_out_exact_bytes_without_self_delivery() {
        let (_network, peers) = transports();
        let program_id = ProgramHash([7; 32]);
        let mut first = peers[0]
            .subscribe_program(program_id, vec![])
            .await
            .unwrap();
        let mut second = peers[1]
            .subscribe_program(program_id, vec![peer(1)])
            .await
            .unwrap();
        let mut third = peers[2]
            .subscribe_program(program_id, vec![peer(1)])
            .await
            .unwrap();
        joined(&mut first).await;
        assert_eq!(
            first.recv().await.unwrap(),
            ProgramTopicEvent::NeighborUp(peer(2))
        );
        assert_eq!(
            first.recv().await.unwrap(),
            ProgramTopicEvent::NeighborUp(peer(3))
        );
        joined(&mut second).await;
        assert_eq!(
            second.recv().await.unwrap(),
            ProgramTopicEvent::NeighborUp(peer(1))
        );
        joined(&mut third).await;
        assert_eq!(
            third.recv().await.unwrap(),
            ProgramTopicEvent::NeighborUp(peer(1))
        );

        let bytes = Bytes::from_static(b"signed negotiation fact");
        first.publish(bytes.clone()).await.unwrap();
        let expected = ProgramTopicEvent::Fact(TopicFact {
            delivered_from: peer(1),
            bytes: bytes.clone(),
        });
        assert_eq!(second.recv().await.unwrap(), expected);
        assert_eq!(
            third.recv().await.unwrap(),
            ProgramTopicEvent::Fact(TopicFact {
                delivered_from: peer(1),
                bytes,
            })
        );
        assert!(
            timeout(Duration::from_millis(10), first.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn bootstrap_neighbor_up_is_sorted_deduplicated_and_registered_only() {
        let (_network, peers) = transports();
        let program_id = ProgramHash([8; 32]);
        let mut first = peers[0]
            .subscribe_program(program_id, vec![])
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(10), first.recv())
                .await
                .is_err()
        );
        let mut second = peers[1]
            .subscribe_program(program_id, vec![peer(1)])
            .await
            .unwrap();
        joined(&mut second).await;
        assert_eq!(
            second.recv().await.unwrap(),
            ProgramTopicEvent::NeighborUp(peer(1))
        );
        joined(&mut first).await;
        assert_eq!(
            first.recv().await.unwrap(),
            ProgramTopicEvent::NeighborUp(peer(2))
        );

        let mut third = peers[2]
            .subscribe_program(
                program_id,
                vec![peer(4), peer(2), peer(1), peer(2), peer(4)],
            )
            .await
            .unwrap();
        assert_eq!(third.recv().await.unwrap(), ProgramTopicEvent::Joined);
        assert_eq!(
            third.recv().await.unwrap(),
            ProgramTopicEvent::NeighborUp(peer(1))
        );
        assert_eq!(
            third.recv().await.unwrap(),
            ProgramTopicEvent::NeighborUp(peer(2))
        );
        assert!(
            timeout(Duration::from_millis(10), third.recv())
                .await
                .is_err()
        );

        third
            .join_peers(vec![peer(4), peer(2), peer(1), peer(2)])
            .await
            .unwrap();
        assert_eq!(
            third.recv().await.unwrap(),
            ProgramTopicEvent::NeighborUp(peer(1))
        );
        assert_eq!(
            third.recv().await.unwrap(),
            ProgramTopicEvent::NeighborUp(peer(2))
        );
        assert!(
            timeout(Duration::from_millis(10), third.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn queue_lag_closes_only_the_slow_generation() {
        let (_network, peers) = transports();
        let program_id = ProgramHash([9; 32]);
        let mut publisher = peers[0]
            .subscribe_program(program_id, vec![])
            .await
            .unwrap();
        let mut slow = peers[1]
            .subscribe_program(program_id, vec![peer(1)])
            .await
            .unwrap();
        joined(&mut publisher).await;
        assert_eq!(
            publisher.recv().await.unwrap(),
            ProgramTopicEvent::NeighborUp(peer(2))
        );
        joined(&mut slow).await;
        assert_eq!(
            slow.recv().await.unwrap(),
            ProgramTopicEvent::NeighborUp(peer(1))
        );

        for _ in 0..=NEGOTIATION_EVENT_QUEUE_CAP {
            publisher
                .publish(Bytes::from_static(b"fact"))
                .await
                .unwrap();
        }
        assert_eq!(slow.recv().await.unwrap(), ProgramTopicEvent::Lagged);
        assert_eq!(slow.recv().await.unwrap(), ProgramTopicEvent::Closed);
        assert!(matches!(
            slow.recv().await,
            Err(TransportError::TopicClosed)
        ));

        // The publisher's generation stays live after the other subscriber lags.
        publisher
            .publish(Bytes::from_static(b"still-live"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn blobs_fail_over_and_check_length_hash_and_bounds() {
        let (network, peers) = transports();
        let first = &peers[0];
        let second = &peers[1];
        let third = &peers[2];
        let negotiation_id = NegotiationId([7; 32]);
        let bytes = Bytes::from_static(b"content-addressed negotiation terms");
        let hash = first
            .import_blob(negotiation_id, bytes.clone(), 1024)
            .await
            .unwrap();
        third
            .import_blob(negotiation_id, bytes.clone(), 1024)
            .await
            .unwrap();

        // The first provider is missing. The second named provider succeeds.
        second
            .fetch_blob(
                negotiation_id,
                hash,
                bytes.len() as u64,
                vec![peer(99), *first.peer_id(), *third.peer_id()],
                1024,
            )
            .await
            .unwrap();
        assert_eq!(second.read_blob(hash, 1024).await.unwrap(), bytes);

        let new_peer = &peers[3];
        assert!(matches!(
            new_peer
                .fetch_blob(
                    negotiation_id,
                    hash,
                    bytes.len() as u64 + 1,
                    vec![*first.peer_id()],
                    1024,
                )
                .await,
            Err(TransportError::BlobLengthMismatch { .. })
        ));
        assert!(matches!(
            new_peer
                .fetch_blob(negotiation_id, hash, 4, vec![*first.peer_id()], 4)
                .await,
            Err(TransportError::BlobTooLarge { .. })
        ));

        let bad_hash = [0xabu8; 32];
        {
            let mut stores = lock_unpoisoned(&network.inner.blobs);
            stores
                .entry(*first.peer_id())
                .or_default()
                .blobs
                .insert(bad_hash, Bytes::from_static(b"not that hash"));
        }
        assert!(matches!(
            new_peer
                .fetch_blob(negotiation_id, bad_hash, 13, vec![*first.peer_id()], 1024,)
                .await,
            Err(TransportError::BlobHashMismatch { .. })
        ));
        assert!(matches!(
            new_peer.read_blob(hash, 4).await,
            Err(TransportError::BlobNotFound { .. })
        ));
    }

    #[tokio::test]
    async fn blob_retention_ends_with_its_session() {
        let (network, peers) = transports();
        let transport = &peers[0];
        let negotiation_id = NegotiationId([9; 32]);
        let hash = transport
            .import_blob(negotiation_id, Bytes::from_static(b"session blob"), 1024)
            .await
            .unwrap();
        let first = SessionHash([1; 32]);
        let second = SessionHash([2; 32]);

        transport.retain_session_blob(first, hash).await.unwrap();
        transport.retain_session_blob(second, hash).await.unwrap();
        transport
            .release_negotiation_blobs(negotiation_id)
            .await
            .unwrap();
        transport.release_session_blobs(first).await.unwrap();

        {
            let stores = lock_unpoisoned(&network.inner.blobs);
            let store = &stores[transport.peer_id()];
            assert!(!store.sessions.contains_key(&first));
            assert_eq!(store.sessions[&second], HashSet::from([hash]));
            assert!(store.blobs.contains_key(&hash));
        }

        transport.release_session_blobs(second).await.unwrap();
        assert!(matches!(
            transport.read_blob(hash, 1024).await,
            Err(TransportError::BlobNotFound { .. })
        ));
    }

    #[tokio::test]
    async fn exec_send_waits_for_explicit_durable_acceptance() {
        let (_network, peers) = transports();
        let send = peers[0]
            .open_exec(peers[1].peer_id(), SessionHash([11; 32]))
            .await
            .unwrap();
        let recv = peers[1].accept_exec().await.unwrap().into_parts().1;
        let frame = message_frame(11);
        let task = tokio::spawn({
            let send = send.clone();
            let frame = frame.clone();
            async move { send.send_exec(&frame).await }
        });

        let delivery = recv.recv_exec().await.unwrap();
        assert_eq!(delivery.source(), *peers[0].peer_id());
        assert_eq!(delivery.frame(), &frame);
        assert!(!task.is_finished());

        delivery.acknowledge().unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn exact_retry_can_be_accepted_after_first_delivery_is_dropped() {
        let (_network, peers) = transports();
        let send = peers[0]
            .open_exec(peers[1].peer_id(), SessionHash([12; 32]))
            .await
            .unwrap();
        let recv = peers[1].accept_exec().await.unwrap().into_parts().1;
        let frame = message_frame(12);

        let first = tokio::spawn({
            let send = send.clone();
            let frame = frame.clone();
            async move { send.send_exec(&frame).await }
        });
        let first_delivery = recv.recv_exec().await.unwrap();
        drop(first_delivery);
        assert!(matches!(
            first.await.unwrap(),
            Err(TransportError::ExecReceiverDropped)
        ));

        let second = tokio::spawn({
            let send = send.clone();
            let frame = frame.clone();
            async move { send.send_exec(&frame).await }
        });
        let second_delivery = recv.recv_exec().await.unwrap();
        second_delivery.acknowledge().unwrap();
        second.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn rejection_and_drop_never_ack_exec_delivery() {
        let (_network, peers) = transports();
        let send = peers[0]
            .open_exec(peers[1].peer_id(), SessionHash([13; 32]))
            .await
            .unwrap();
        let recv = peers[1].accept_exec().await.unwrap().into_parts().1;
        let frame = message_frame(13);

        let rejected = tokio::spawn({
            let send = send.clone();
            let frame = frame.clone();
            async move { send.send_exec(&frame).await }
        });
        let delivery = recv.recv_exec().await.unwrap();
        delivery
            .reject(crate::ExecDeliveryRejection::Rejected)
            .unwrap();
        assert!(matches!(
            rejected.await.unwrap(),
            Err(TransportError::ExecRejected)
        ));

        let conflict = tokio::spawn({
            let send = send.clone();
            let frame = frame.clone();
            async move { send.send_exec(&frame).await }
        });
        let delivery = recv.recv_exec().await.unwrap();
        delivery
            .reject(crate::ExecDeliveryRejection::Conflict)
            .unwrap();
        assert!(matches!(
            conflict.await.unwrap(),
            Err(TransportError::ExecConflict)
        ));

        let dropped = tokio::spawn({
            let send = send.clone();
            async move { send.send_exec(&frame).await }
        });
        drop(recv.recv_exec().await.unwrap());
        assert!(matches!(
            dropped.await.unwrap(),
            Err(TransportError::ExecReceiverDropped)
        ));
    }

    #[tokio::test]
    async fn closing_the_receive_handle_reports_connection_closed() {
        let (_network, peers) = transports();
        let send = peers[0]
            .open_exec(peers[1].peer_id(), SessionHash([14; 32]))
            .await
            .unwrap();
        let recv = peers[1].accept_exec().await.unwrap().into_parts().1;
        let task = tokio::spawn({
            let frame = message_frame(14);
            async move { send.send_exec(&frame).await }
        });
        drop(recv);
        assert!(matches!(
            task.await.unwrap(),
            Err(TransportError::ConnectionClosed)
        ));
    }

    #[tokio::test]
    async fn wrong_stream_protocol_is_rejected_without_consuming_the_packet() {
        use arena0_protocol::{FetchActivationTickets, FetchFrame};

        let (_network, peers) = transports();
        let send = peers[0].open_fetch(peers[1].peer_id()).await.unwrap();
        let recv = peers[1].accept_fetch().await.unwrap();
        let frame = FetchFrame::FetchActivationTickets(FetchActivationTickets {
            session_hash: SessionHash([19; 32]),
        });
        send.send_fetch(&frame).await.unwrap();
        assert!(matches!(
            recv.recv_exec().await,
            Err(TransportError::ProtocolMismatch(_))
        ));
        assert_eq!(recv.recv_fetch().await.unwrap(), frame);
    }

    #[tokio::test]
    async fn closing_the_transport_reports_connection_closed() {
        let (_network, peers) = transports();
        let send = peers[0]
            .open_exec(peers[1].peer_id(), SessionHash([15; 32]))
            .await
            .unwrap();
        let _recv = peers[1].accept_exec().await.unwrap();
        let task = tokio::spawn({
            let frame = message_frame(15);
            async move { send.send_exec(&frame).await }
        });
        peers[1].close().await;
        assert!(matches!(
            task.await.unwrap(),
            Err(TransportError::ConnectionClosed)
        ));
        assert!(matches!(
            peers[1].accept_exec().await,
            Err(TransportError::ConnectionClosed)
        ));
    }

    #[tokio::test]
    async fn exec_source_is_route_authenticated_and_not_frame_supplied() {
        let (_network, peers) = transports();
        let send = peers[2]
            .open_exec(peers[1].peer_id(), SessionHash([16; 32]))
            .await
            .unwrap();
        let accepted = peers[1].accept_exec().await.unwrap();
        assert_eq!(accepted.metadata().session_hash(), SessionHash([16; 32]));
        let recv = accepted.into_parts().1;
        let task = tokio::spawn({
            let frame = message_frame(16);
            async move { send.send_exec(&frame).await }
        });
        let delivery = recv.recv_exec().await.unwrap();
        assert_eq!(delivery.source(), *peers[2].peer_id());
        assert_eq!(delivery.source(), *recv.remote_peer());
        assert_ne!(delivery.source(), *peers[0].peer_id());
        delivery.acknowledge().unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn abort_sender_is_authenticated_in_both_stream_directions() {
        let (_network, peers) = transports();
        let session_hash = SessionHash([26; 32]);
        let send = peers[0]
            .open_exec(peers[1].peer_id(), session_hash)
            .await
            .unwrap();
        let recv = peers[1].accept_exec().await.unwrap().into_parts().1;
        let frame = abort_frame(session_hash, *peers[0].peer_id());
        let task = tokio::spawn({
            let send = send.clone();
            let frame = frame.clone();
            async move { send.send_exec(&frame).await }
        });

        let delivery = recv.recv_exec().await.unwrap();
        assert_eq!(delivery.source(), *peers[0].peer_id());
        assert_eq!(delivery.frame(), &frame);
        delivery.acknowledge().unwrap();
        task.await.unwrap().unwrap();

        let forged = abort_frame(session_hash, *peers[2].peer_id());
        assert!(matches!(
            send.send_exec(&forged).await,
            Err(TransportError::ProtocolMismatch(_))
        ));
    }

    #[tokio::test]
    async fn exec_stream_queue_is_bounded_before_receivers_accept() {
        let network = LocalNetwork::with_capacity(1);
        let peers = LocalTransport::create_network(&network, vec![peer(1), peer(2)]);
        let send = peers[0]
            .open_exec(peers[1].peer_id(), SessionHash([17; 32]))
            .await
            .unwrap();
        let recv = peers[1].accept_exec().await.unwrap().into_parts().1;
        let first_frame = message_frame(17);
        let second_frame = message_frame(18);
        let first = tokio::spawn({
            let send = send.clone();
            let frame = first_frame.clone();
            async move { send.send_exec(&frame).await }
        });
        let first_delivery = recv.recv_exec().await.unwrap();
        assert!(!first.is_finished());

        // Hold the only free channel slot while starting the second send.
        // This makes its bounded-queue backpressure deterministic without a
        // scheduler-dependent timeout assertion.
        let permit = send.tx.reserve().await.unwrap();
        let second = tokio::spawn({
            let send = send.clone();
            let frame = second_frame.clone();
            async move { send.send_exec(&frame).await }
        });
        first_delivery.acknowledge().unwrap();
        first.await.unwrap().unwrap();

        assert!(!second.is_finished());
        drop(permit);
        let second_delivery = recv.recv_exec().await.unwrap();
        assert_eq!(second_delivery.frame(), &second_frame);
        second_delivery.acknowledge().unwrap();
        second.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn fetch_uses_two_one_frame_streams_correlated_by_session_hash() {
        use arena0_crypto::{BlsPublicKey, BlsSignature, Ed25519Signature};
        use arena0_protocol::{
            ActivationTickets, FetchActivationTickets, FetchFrame, NegotiationId, Ticket,
            TicketAction, TicketData,
        };
        use arena0_wire::StreamProtocol;

        let (_network, peers) = transports();
        // Stream A: requester (peers[0]) -> creator (peers[1]) carries the
        // one-frame request; the requester drops the stream after sending.
        let request_send = peers[0].open_fetch(peers[1].peer_id()).await.unwrap();
        let request = FetchFrame::FetchActivationTickets(FetchActivationTickets {
            session_hash: SessionHash([1; 32]),
        });
        request_send.send_fetch(&request).await.unwrap();
        drop(request_send);

        let request_recv = peers[1].accept_fetch().await.unwrap();
        assert_eq!(request_recv.proto(), StreamProtocol::Fetch);
        assert_eq!(request_recv.recv_fetch().await.unwrap(), request);

        // Stream B: creator (peers[1]) -> requester (peers[0]) carries the
        // one-frame response, correlated by session_hash; the creator drops
        // the stream after sending.
        let response_send = peers[1].open_fetch(peers[0].peer_id()).await.unwrap();
        let response = FetchFrame::ActivationTickets(ActivationTickets {
            session_hash: SessionHash([1; 32]),
            tickets: vec![],
        });
        response_send.send_fetch(&response).await.unwrap();
        drop(response_send);

        let response_recv = peers[0].accept_fetch().await.unwrap();
        assert_eq!(response_recv.proto(), StreamProtocol::Fetch);
        assert_eq!(response_recv.recv_fetch().await.unwrap(), response);

        // An oversize response (over MAX_FETCH_RESPONSE_BYTES plus the enum
        // tag) is rejected by the codec bound before it is sent.
        let ticket = Ticket {
            data: TicketData::new(
                NegotiationId([1; 32]),
                0,
                PeerId([1; 32]),
                0,
                TicketAction::Active {
                    execution_bls: BlsPublicKey([1; 96]),
                    key_binding: BlsSignature([0; 48]),
                    issued_at_unix_ms: 1,
                    valid_for_ms: 60_000,
                },
            )
            .expect("valid ticket data"),
            signature: Ed25519Signature([0; 64]),
        };
        let oversize = FetchFrame::ActivationTickets(ActivationTickets {
            session_hash: SessionHash([1; 32]),
            tickets: vec![ticket; 101],
        });
        let oversize_send = peers[1].open_fetch(peers[0].peer_id()).await.unwrap();
        assert!(matches!(
            oversize_send.send_fetch(&oversize).await,
            Err(TransportError::PayloadTooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn close_unregisters_one_handle_and_keeps_other_handles_live() {
        let (_network, peers) = transports();
        let program_id = ProgramHash([10; 32]);
        let mut first = peers[0]
            .subscribe_program(program_id, vec![])
            .await
            .unwrap();
        let mut second = peers[1]
            .subscribe_program(program_id, vec![peer(1)])
            .await
            .unwrap();
        joined(&mut first).await;
        assert_eq!(
            first.recv().await.unwrap(),
            ProgramTopicEvent::NeighborUp(peer(2))
        );
        joined(&mut second).await;
        assert_eq!(
            second.recv().await.unwrap(),
            ProgramTopicEvent::NeighborUp(peer(1))
        );
        first.close().await.unwrap();
        first.close().await.unwrap();
        second
            .publish(Bytes::from_static(b"after-close"))
            .await
            .unwrap();
        assert_eq!(first.recv().await.unwrap(), ProgramTopicEvent::Closed);
        assert!(matches!(
            first.recv().await,
            Err(TransportError::TopicClosed)
        ));

        let mut third = peers[2]
            .subscribe_program(program_id, vec![peer(1), peer(2)])
            .await
            .unwrap();
        assert_eq!(third.recv().await.unwrap(), ProgramTopicEvent::Joined);
        assert_eq!(
            third.recv().await.unwrap(),
            ProgramTopicEvent::NeighborUp(peer(2))
        );
        assert!(
            timeout(Duration::from_millis(10), third.recv())
                .await
                .is_err()
        );
    }
}
