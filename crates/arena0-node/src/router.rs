//! Transport accept routing.
//!
//! Stream metadata selects an execution actor before any frame is delivered.
//! The router does not inspect, acknowledge, or otherwise interpret execution
//! facts; the actor owns that responsibility after it receives a stream.

use std::collections::HashMap;
use std::sync::Arc;

use arena0_protocol::{FetchFrame, SessionHash};
use arena0_transport::{AcceptedExecStream, RecvHandle, Transport};
use arena0_wire::StreamProtocol;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::context::InboundStreamPayload;
use crate::machines::negotiation::FETCH_TIMEOUT;

/// The session-keyed convergence-fetch registry.
pub(crate) type FetchRegistry =
    Arc<std::sync::Mutex<HashMap<SessionHash, mpsc::Sender<(RecvHandle, FetchFrame)>>>>;

/// Resolve an authenticated execution stream to the actor that owns its
/// session. Returning `None` drops the stream before a frame is consumed.
pub(crate) type ExecStreamRouter =
    Arc<dyn Fn(SessionHash) -> Option<mpsc::Sender<InboundStreamPayload>> + Send + Sync>;

/// Accept and route execution streams until transport shutdown.
pub(crate) async fn run_exec_accept_router(
    transport: Arc<dyn Transport + Sync>,
    exec_router: ExecStreamRouter,
) {
    loop {
        if accept_exec(Arc::clone(&transport), Arc::clone(&exec_router))
            .await
            .is_err()
        {
            return;
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

async fn accept_exec(
    transport: Arc<dyn Transport + Sync>,
    exec_router: ExecStreamRouter,
) -> Result<(), ()> {
    let accepted = transport.accept_exec().await.map_err(|_| ())?;
    route_exec(accepted, exec_router).await;
    Ok(())
}

async fn route_exec(accepted: AcceptedExecStream, exec_router: ExecStreamRouter) {
    let (metadata, recv) = accepted.into_parts();
    let Some(sender) = exec_router(metadata.session_hash()) else {
        return;
    };
    // Keep the authenticated remote peer alongside the non-cloneable receiver.
    // If the actor disappears, dropping this payload closes the transport side.
    let _ = sender.send((metadata.remote_peer(), recv)).await;
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
    async fn execution_accept_is_independent_of_fetch_frame_wait() {
        let network = LocalNetwork::new();
        let mut transports = LocalTransport::create_network(&network, vec![peer(1), peer(2)]);
        let sender = Arc::new(transports.remove(0));
        let receiver = Arc::new(transports.remove(0));
        let session = SessionHash([7; 32]);
        let (exec_tx, mut exec_rx) = mpsc::channel(1);
        let exec_router: ExecStreamRouter =
            Arc::new(move |candidate| (candidate == session).then(|| exec_tx.clone()));
        let fetch_registry: FetchRegistry = Arc::new(std::sync::Mutex::new(HashMap::new()));

        let exec_task = tokio::spawn(run_exec_accept_router(
            Arc::clone(&receiver) as Arc<dyn Transport + Sync>,
            exec_router,
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
