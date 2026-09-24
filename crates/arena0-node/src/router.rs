//! Transport accept routing.
//!
//! Stream metadata selects an execution actor before any frame is delivered.
//! The actor interprets live execution frames. A terminal session without a
//! live actor acknowledges stale traffic using the store's routing projection.

use std::collections::HashMap;
use std::sync::Arc;

use arena0_protocol::{FetchFrame, SessionHash};
use arena0_store::StoreHandle;
use arena0_transport::{ExecStreamMetadata, RecvHandle, Transport};
use arena0_wire::StreamProtocol;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::context::InboundStreamPayload;
use crate::machines::negotiation::FETCH_TIMEOUT;

const EXEC_FALLBACK_IDLE_TIMEOUT: std::time::Duration = FETCH_TIMEOUT;
const MAX_EXEC_FALLBACK_ROUTES: usize = 256;

/// The session-keyed convergence-fetch registry.
pub(crate) type FetchRegistry =
    Arc<std::sync::Mutex<HashMap<SessionHash, mpsc::Sender<(RecvHandle, FetchFrame)>>>>;

/// Resolve an authenticated execution stream to the actor that owns its
/// session. An absent or closed route falls back to terminal metadata.
pub(crate) type ExecStreamRouter =
    Arc<dyn Fn(SessionHash) -> Option<mpsc::Sender<InboundStreamPayload>> + Send + Sync>;

/// Accept and route execution streams until transport shutdown.
pub(crate) async fn run_exec_accept_router(
    transport: Arc<dyn Transport + Sync>,
    exec_router: ExecStreamRouter,
    store: StoreHandle,
    end_wakes: mpsc::Sender<arena0_protocol::ExecId>,
) {
    let mut routes = JoinSet::new();
    loop {
        tokio::select! {
            accepted = transport.accept_exec() => {
                let Ok(accepted) = accepted else { return; };
                let (metadata, recv) = accepted.into_parts();
                // Live actors own their streams independently of the bounded
                // terminal fallback readers.
                let recv = if let Some(sender) = exec_router(metadata.session_hash()) {
                    match sender.send((metadata.remote_peer(), recv)).await {
                        Ok(()) => continue,
                        Err(error) => error.0.1,
                    }
                } else {
                    recv
                };
                if routes.len() < MAX_EXEC_FALLBACK_ROUTES {
                    routes.spawn(route_exec(metadata, recv, store.clone(), end_wakes.clone()));
                }
            }
            Some(_) = routes.join_next(), if !routes.is_empty() => {}
        }
    }
}

/// Accept and route convergence-fetch streams until transport shutdown.
pub(crate) async fn run_fetch_accept_router(
    transport: Arc<dyn Transport + Sync>,
    fetch_registry: FetchRegistry,
) {
    loop {
        if accept_fetch(Arc::clone(&transport), Arc::clone(&fetch_registry))
            .await
            .is_err()
        {
            return;
        }
    }
}

async fn route_exec(
    metadata: ExecStreamMetadata,
    recv: RecvHandle,
    store: StoreHandle,
    end_wakes: mpsc::Sender<arena0_protocol::ExecId>,
) {
    if !matches!(
        store.execution_end(metadata.session_hash()).await,
        Ok(Some((
            _,
            arena0_protocol::EndPhase::Ending { .. } | arena0_protocol::EndPhase::Ended { .. }
        )))
    ) {
        return;
    }
    while let Ok(Ok(delivery)) = timeout(EXEC_FALLBACK_IDLE_TIMEOUT, recv.recv_exec()).await {
        // Only narrow projections are read here. Reconstruction belongs to
        // the supervisor's actor-start path, not inbound classification.
        match store.execution_end(metadata.session_hash()).await {
            Ok(Some((execution_id, arena0_protocol::EndPhase::Ended { unconfirmed })))
                if unconfirmed.contains(&metadata.remote_peer()) =>
            {
                let _ = delivery.reject(arena0_transport::ExecDeliveryRejection::NotYet);
                let _ = end_wakes.try_send(execution_id);
                return;
            }
            Ok(Some((_, phase)))
                if phase
                    .unconfirmed()
                    .is_some_and(|peers| !peers.contains(&metadata.remote_peer())) =>
            {
                let _ = delivery.acknowledge();
            }
            _ => return,
        }
    }
}

async fn accept_fetch(
    transport: Arc<dyn Transport + Sync>,
    fetch_registry: FetchRegistry,
) -> Result<(), ()> {
    let recv = transport.accept_fetch().await.map_err(|_| ())?;
    route_fetch(recv, fetch_registry).await;
    Ok(())
}

async fn route_fetch(recv: RecvHandle, fetch_registry: FetchRegistry) {
    if recv.proto() != StreamProtocol::Fetch {
        return;
    }
    let Ok(Ok(frame)) = timeout(FETCH_TIMEOUT, recv.recv_fetch()).await else {
        return;
    };
    let session_hash = match &frame {
        FetchFrame::FetchActivationTickets(request) => request.session_hash,
        FetchFrame::ActivationTickets(response) => response.session_hash,
    };
    let sender = fetch_registry.lock().unwrap().get(&session_hash).cloned();
    if let Some(sender) = sender {
        let _ = sender.send((recv, frame)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_protocol::PeerId;
    use arena0_transport::local::{LocalNetwork, LocalTransport};
    use tokio::time::{Duration, timeout};

    fn peer(byte: u8) -> PeerId {
        PeerId([byte; 32])
    }

    #[tokio::test]
    async fn unknown_execution_stream_is_dropped_without_waiting_for_a_frame() {
        let network = LocalNetwork::new();
        let transports = LocalTransport::create_network(&network, vec![peer(1), peer(2)]).unwrap();
        let session = SessionHash([7; 32]);
        let send = transports[0]
            .open_exec(transports[1].peer_id(), session)
            .await
            .unwrap();
        let (metadata, recv) = transports[1].accept_exec().await.unwrap().into_parts();
        let directory = tempfile::tempdir().unwrap();
        let store = arena0_store::Store::open(arena0_store::StoreConfig::new(
            directory.path().join("router.db"),
            peer(2),
        ))
        .unwrap();
        timeout(
            Duration::from_secs(1),
            route_exec(metadata, recv, store.handle().clone(), mpsc::channel(1).0),
        )
        .await
        .expect("unknown sessions must close before receiving any frame");
        let state = arena0_protocol::StateHash([1; 32]);
        let data = vec![1];
        let frame = arena0_protocol::ExecFrame::Message {
            message_id: arena0_protocol::MessageId::derive(
                session,
                peer(1),
                0,
                state,
                state,
                &data,
            ),
            seq: 0,
            prestate: state,
            poststate: state,
            data,
        };
        assert!(matches!(
            send.send_exec(&frame).await,
            Err(arena0_transport::TransportError::ConnectionClosed)
        ));
    }

    #[tokio::test]
    async fn execution_accept_is_independent_of_fetch_frame_wait() {
        let network = LocalNetwork::new();
        let mut transports = LocalTransport::create_network(&network, vec![peer(1), peer(2)])
            .expect("attach local transports");
        let sender = Arc::new(transports.remove(0));
        let receiver = Arc::new(transports.remove(0));
        let session = SessionHash([7; 32]);
        let (exec_tx, mut exec_rx) = mpsc::channel(1);
        let exec_router: ExecStreamRouter =
            Arc::new(move |candidate| (candidate == session).then(|| exec_tx.clone()));
        let fetch_registry: FetchRegistry = Arc::new(std::sync::Mutex::new(HashMap::new()));

        let directory = tempfile::tempdir().expect("store directory");
        let store = arena0_store::Store::open(arena0_store::StoreConfig::new(
            directory.path().join("router.db"),
            peer(2),
        ))
        .expect("store");
        let exec_task = tokio::spawn(run_exec_accept_router(
            Arc::clone(&receiver) as Arc<dyn Transport + Sync>,
            exec_router,
            store.handle().clone(),
            mpsc::channel(1).0,
        ));
        let fetch_task = tokio::spawn(run_fetch_accept_router(
            Arc::clone(&receiver) as Arc<dyn Transport + Sync>,
            fetch_registry,
        ));

        // Leave the first fetch stream without a frame. Its bounded fetch
        // reader waits for FETCH_TIMEOUT, while the execution accept loop
        // must continue to receive independent execution streams.
        let _fetch = sender
            .open_fetch(receiver.peer_id())
            .await
            .expect("open fetch stream");
        let _exec = sender
            .open_exec(receiver.peer_id(), session)
            .await
            .expect("open execution stream");
        let (remote, recv) = timeout(Duration::from_secs(1), exec_rx.recv())
            .await
            .expect("execution stream should not wait for fetch timeout")
            .expect("execution route remains connected");
        assert_eq!(remote, *sender.peer_id());
        assert_eq!(recv.remote_peer(), sender.peer_id());
        assert_eq!(recv.session_hash(), Some(session));

        exec_task.abort();
        fetch_task.abort();
        let _ = exec_task.await;
        let _ = fetch_task.await;
    }
}
