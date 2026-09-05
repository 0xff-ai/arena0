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

/// Maximum number of inbound execution streams buffered before an execution
/// is spawned. This bounds routing memory for unknown session identities.
const UNCLAIMED_INBOX_CAP: usize = 64;

#[derive(Debug)]
struct ExecInbox {
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
    inboxes: Arc<StdMutex<HashMap<SessionHash, ExecInbox>>>,
    fetch_registry: FetchRegistry,
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
        let inboxes = Arc::new(StdMutex::new(HashMap::new()));
        let exec_router: ExecStreamRouter = {
            let inboxes = Arc::clone(&inboxes);
            Arc::new(move |session| inbox_sender(&inboxes, session))
        };
        let fetch_registry: FetchRegistry = Arc::new(StdMutex::new(HashMap::new()));
        let mut tasks = JoinSet::new();
        tasks.spawn(crate::router::run_exec_accept_router(
            Arc::clone(&transport),
            exec_router,
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
            inboxes,
            fetch_registry,
            owner_token: Arc::new(()),
            negotiation_lock: TokioMutex::new(()),
            tasks: TokioMutex::new(tasks),
        })
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
    /// public cursor, or interrupts an in-flight terminal proof when an abort
    /// occurrence is no longer legal. The supplied writer is consumed so the
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
        crate::execution::fail_execution(execution_store.store_mut(), &self.identity, reason)
            .await
            .map(|_| ())
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
        let mut inbound = self.try_claim_inbox(session_hash)?;
        let inboxes = Arc::clone(&self.inboxes);
        let session_claim = SessionStreamClaim::new(move || {
            inboxes.lock().unwrap().remove(&session_hash);
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
            while let Some(payload) = inbound.recv().await {
                if stream_tx.send(payload).await.is_err() {
                    break;
                }
            }
        });
        Ok(spawned
            .with_session_claim(session_claim)
            .with_forwarder(forwarder))
    }

    fn try_claim_inbox(
        &self,
        key: SessionHash,
    ) -> Result<mpsc::Receiver<InboundStreamPayload>, HostError> {
        let mut inboxes = self.inboxes.lock().unwrap();
        let inbox = inboxes.entry(key).or_insert_with(|| {
            let (tx, rx) = mpsc::channel(64);
            ExecInbox { tx, rx: Some(rx) }
        });
        inbox.rx.take().ok_or(HostError::DuplicateSession(key))
    }
}

fn inbox_sender(
    inboxes: &StdMutex<HashMap<SessionHash, ExecInbox>>,
    key: SessionHash,
) -> Option<mpsc::Sender<InboundStreamPayload>> {
    let mut inboxes = inboxes.lock().unwrap();
    if !inboxes.contains_key(&key) {
        let unclaimed = inboxes.values().filter(|inbox| inbox.rx.is_some()).count();
        if unclaimed >= UNCLAIMED_INBOX_CAP {
            tracing::warn!(
                ?key,
                cap = UNCLAIMED_INBOX_CAP,
                "unclaimed execution inbox cap reached"
            );
            return None;
        }
    }
    Some(
        inboxes
            .entry(key)
            .or_insert_with(|| {
                let (tx, rx) = mpsc::channel(64);
                ExecInbox { tx, rx: Some(rx) }
            })
            .tx
            .clone(),
    )
}
