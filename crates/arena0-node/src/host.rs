//! Host-owned transport routing and execution supervision.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use arena0_crypto::{ExecutionKey, ExecutionSalt, NodeKeys};
use arena0_protocol::{ExecId, FetchFrame, NegotiationId, PeerId, PeerIdSource, SessionHash};
use arena0_store::{StoreError, StoreHandle};
use arena0_transport::{RecvHandle, Transport};
use thiserror::Error;
use tokio::sync::{Mutex as TokioMutex, MutexGuard as TokioMutexGuard, mpsc};
use tokio::task::JoinSet;

use crate::context::{
    ExecContext, ExecError, HostExecutionStore, InboundStreamPayload, SessionStreamClaim,
    SpawnedExec,
};
use crate::execution::spawn_execution;
use crate::machines::activation::ActivatedSession;
use crate::machines::negotiation::{
    NegotiationAttempt, NegotiationDriveError, NegotiationDriver, NegotiationEffects,
};
use crate::router::{ExecStreamRouter, FetchRegistry};

#[derive(Debug)]
struct ExecRoute {
    tx: mpsc::Sender<InboundStreamPayload>,
    rx: Option<mpsc::Receiver<InboundStreamPayload>>,
}

/// Errors at the host/actor construction boundary.
#[derive(Debug, Error)]
pub enum HostError {
    #[error("execution {0} already has an actor")]
    DuplicateExecution(ExecId),
    #[error("session {0} already has an actor")]
    DuplicateSession(SessionHash),
    /// A lifecycle capability was paired with a different execution context.
    #[error("execution store is bound to {store}, context requests {context}")]
    ExecutionStoreMismatch { store: ExecId, context: ExecId },
    /// A lifecycle capability issued by another Host cannot be used with this
    /// Host's transport and identity.
    #[error("execution store belongs to another host")]
    ExecutionStoreHostMismatch,
    /// The store rejected an execution-scoped operation while binding the
    /// actor's durable writer capability.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Host transport and actor supervisor for one durable node identity.
#[allow(missing_debug_implementations)]
pub struct Host {
    identity: Arc<NodeKeys>,
    /// Persistent peer identity exposed for local ensemble construction.
    pub peer_id: PeerId,
    transport: Arc<dyn Transport + Sync>,
    store: StoreHandle,
    routes: Arc<StdMutex<HashMap<SessionHash, ExecRoute>>>,
    fetch_registry: FetchRegistry,
    end_wakes: StdMutex<Option<mpsc::Receiver<ExecId>>>,
    owner_token: Arc<()>,
    negotiation_lock: TokioMutex<()>,
    tasks: TokioMutex<JoinSet<()>>,
}

impl Host {
    /// Start the accept router for an already-bound transport.
    pub fn start(
        identity: Arc<NodeKeys>,
        transport: Arc<dyn Transport + Sync>,
        store: StoreHandle,
    ) -> Arc<Self> {
        let peer_id = identity.peer_id();
        let routes = Arc::new(StdMutex::new(HashMap::new()));
        let exec_router: ExecStreamRouter = {
            let routes = Arc::clone(&routes);
            Arc::new(move |session| route_sender(&routes, session))
        };
        let fetch_registry: FetchRegistry = Arc::new(StdMutex::new(HashMap::new()));
        let (end_wake_tx, end_wake_rx) = mpsc::channel(64);
        let mut tasks = JoinSet::new();
        tasks.spawn(crate::router::run_exec_accept_router(
            Arc::clone(&transport),
            exec_router,
            store.clone(),
            end_wake_tx,
        ));
        tasks.spawn(crate::router::run_fetch_accept_router(
            Arc::clone(&transport),
            Arc::clone(&fetch_registry),
        ));
        Arc::new(Self {
            identity,
            peer_id,
            transport,
            store,
            routes,
            fetch_registry,
            end_wakes: StdMutex::new(Some(end_wake_rx)),
            owner_token: Arc::new(()),
            negotiation_lock: TokioMutex::new(()),
            tasks: TokioMutex::new(tasks),
        })
    }

    /// Claim dormant-session wake requests. The embedding supervisor must
    /// reconstruct each requested actor through its normal startup path.
    /// Requests are advisory: peers receive NotYet and retry until it is live.
    pub fn take_end_wakes(&self) -> Option<mpsc::Receiver<ExecId>> {
        self.end_wakes.lock().unwrap().take()
    }

    /// Share the durable node identity signer.
    pub fn identity_keys(&self) -> Arc<NodeKeys> {
        Arc::clone(&self.identity)
    }

    /// Derive one execution-scoped signer from the store's persisted salt.
    pub fn execution_key(
        &self,
        salt: &ExecutionSalt,
        exec_id: &ExecId,
        negotiation_id: &NegotiationId,
    ) -> Result<ExecutionKey, String> {
        ExecutionKey::derive(salt, &exec_id.0, &negotiation_id.0).map_err(|error| error.to_string())
    }

    /// Stop the host accept router. Actors remain owned by their returned
    /// handles, but no new transport streams are accepted after this call.
    pub async fn stop(&self) {
        let mut tasks = self.tasks.lock().await;
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }

    /// Serialize negotiation drives that share this host's accept path.
    pub async fn negotiation_guard(&self) -> TokioMutexGuard<'_, ()> {
        self.negotiation_lock.lock().await
    }

    /// Run one negotiation drive to the durable activation boundary.
    ///
    /// The host-issued lifecycle capability is consumed for the drive and
    /// returned with the committed session. The caller must pass that same
    /// value to [`Self::spawn`]; it cannot be dropped and reacquired between
    /// activation and actor construction without releasing ownership.
    pub async fn negotiate(
        &self,
        execution: &ExecutionKey,
        mut execution_store: HostExecutionStore,
        attempt: NegotiationAttempt,
        effects: NegotiationEffects<'_>,
    ) -> Result<(ActivatedSession, HostExecutionStore), NegotiationDriveError> {
        if !execution_store.belongs_to(&self.owner_token) {
            return Err(NegotiationDriveError::ExecutionStoreHostMismatch);
        }
        if execution_store.execution_id() != attempt.exec_id {
            return Err(NegotiationDriveError::ExecutionStoreMismatch {
                store: execution_store.execution_id(),
                requested: attempt.exec_id,
            });
        }
        let committed = NegotiationDriver::new(
            Arc::clone(&self.transport),
            Arc::clone(&self.fetch_registry),
            &self.identity,
            execution,
            execution_store.store_mut(),
            attempt,
            effects,
        )?
        .run()
        .await?;
        Ok((committed, execution_store))
    }

    /// Claim the one live lifecycle writer for an execution on this Host.
    pub fn claim_execution(&self, execution_id: ExecId) -> Result<HostExecutionStore, HostError> {
        self.store
            .claim_execution(execution_id)
            .map(|store| HostExecutionStore::from_store(Arc::clone(&self.owner_token), store))
            .map_err(|error| match error {
                StoreError::ExecutionAlreadyClaimed(exec_id) => {
                    HostError::DuplicateExecution(exec_id)
                }
                error => HostError::Store(error),
            })
    }

    /// Durably fail a recovered execution that cannot be reconstructed.
    ///
    /// The execution aggregate remains the authority after activation commit.
    /// This method therefore signs a `Fail` occurrence against the current
    /// public cursor, or preserves certified terminal evidence for publication. The supplied writer is consumed so the
    /// recovery pass cannot leave a failed execution claimed as live.
    pub async fn fail_recovered_execution(
        &self,
        mut execution_store: HostExecutionStore,
        reason: String,
    ) -> Result<(), ExecError> {
        if !execution_store.belongs_to(&self.owner_token) {
            return Err(ExecError::InvalidState(
                "execution store belongs to another host".into(),
            ));
        }
        crate::execution::fail_execution(execution_store.store_mut(), &self.identity, reason).await
    }

    /// Register a convergence-fetch handler for one session.
    pub fn register_fetch_handler(
        &self,
        session_hash: SessionHash,
    ) -> mpsc::Receiver<(RecvHandle, FetchFrame)> {
        let (tx, rx) = mpsc::channel(64);
        self.fetch_registry.lock().unwrap().insert(session_hash, tx);
        rx
    }

    /// Remove a convergence-fetch handler.
    pub fn unregister_fetch_handler(&self, session_hash: SessionHash) {
        self.fetch_registry.lock().unwrap().remove(&session_hash);
    }

    /// Construct exactly one actor for an activated execution.
    ///
    /// `execution_store` must be the host-issued capability returned by
    /// [`Self::negotiate`] (or [`Self::claim_execution`] for a recovered
    /// execution). It is moved into the actor and cannot be reused by the
    /// caller. The `Arc` receiver is retained by the returned
    /// [`SpawnedExec`], keeping this Host's accept routers alive until the
    /// execution handle is shut down or dropped.
    pub fn spawn(
        self: &Arc<Self>,
        context: ExecContext,
        execution_store: HostExecutionStore,
    ) -> Result<SpawnedExec, HostError> {
        if !execution_store.belongs_to(&self.owner_token) {
            return Err(HostError::ExecutionStoreHostMismatch);
        }
        if execution_store.execution_id() != context.exec_id {
            return Err(HostError::ExecutionStoreMismatch {
                store: execution_store.execution_id(),
                context: context.exec_id,
            });
        }
        let execution_store = execution_store.into_store();
        let session_hash = context.activation.session_hash();
        let mut inbound = self.try_claim_route(session_hash)?;
        let routes = Arc::clone(&self.routes);
        let session_claim = SessionStreamClaim::new(move || {
            routes.lock().unwrap().remove(&session_hash);
        });
        let spawned = spawn_execution(
            context.bind(
                Arc::clone(&self.identity),
                execution_store,
                Arc::clone(&self.transport),
            ),
            Arc::clone(self),
        );
        let stream_tx = spawned.stream_tx.clone();
        let forwarder = tokio::spawn(async move {
            loop {
                let payload = tokio::select! {
                    _ = stream_tx.closed() => break,
                    payload = inbound.recv() => match payload {
                        Some(payload) => payload,
                        None => break,
                    },
                };
                if stream_tx.send(payload).await.is_err() {
                    break;
                }
            }
        });
        Ok(spawned
            .with_session_claim(session_claim)
            .with_forwarder(forwarder))
    }

    fn try_claim_route(
        &self,
        key: SessionHash,
    ) -> Result<mpsc::Receiver<InboundStreamPayload>, HostError> {
        let mut routes = self.routes.lock().unwrap();
        let route = routes.entry(key).or_insert_with(|| {
            let (tx, rx) = mpsc::channel(64);
            ExecRoute { tx, rx: Some(rx) }
        });
        route.rx.take().ok_or(HostError::DuplicateSession(key))
    }
}

fn route_sender(
    routes: &StdMutex<HashMap<SessionHash, ExecRoute>>,
    key: SessionHash,
) -> Option<mpsc::Sender<InboundStreamPayload>> {
    routes
        .lock()
        .unwrap()
        .get(&key)
        .map(|route| route.tx.clone())
}
