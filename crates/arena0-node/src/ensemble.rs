//! A set of [`Host`](crate::Host)s connected through one local network.

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};

use arena0_crypto::NodeKeys;
use arena0_protocol::{PeerId, PeerIdSource};
use arena0_store::StoreHandle;
use arena0_transport::Transport;
use arena0_transport::TransportError;
use arena0_transport::local::{LocalNetwork, LocalTransport};
use thiserror::Error;

use crate::Host;

/// Errors returned while constructing or changing a local [`Ensemble`].
#[derive(Debug, Error)]
pub enum EnsembleError {
    #[error("duplicate host identity {peer_id}")]
    DuplicateHost { peer_id: PeerId },
    #[error("invalid host identity: {0}")]
    InvalidIdentity(String),
    #[error("the ensemble is stopped")]
    Stopped,
    #[error("host {peer_id} is not attached to the ensemble")]
    HostNotFound { peer_id: PeerId },
    #[error("failed to attach host transport: {0}")]
    Transport(TransportError),
}

impl PartialEq for EnsembleError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::DuplicateHost { peer_id: left }, Self::DuplicateHost { peer_id: right })
            | (Self::HostNotFound { peer_id: left }, Self::HostNotFound { peer_id: right }) => {
                left == right
            }
            (Self::InvalidIdentity(left), Self::InvalidIdentity(right)) => left == right,
            (Self::Stopped, Self::Stopped) => true,
            (Self::Transport(left), Self::Transport(right)) => {
                left.to_string() == right.to_string()
            }
            _ => false,
        }
    }
}

impl Eq for EnsembleError {}

#[derive(Clone)]
struct RuntimeHost {
    host: Arc<Host>,
    transport: Arc<LocalTransport>,
}

struct EnsembleState {
    stopped: bool,
    hosts: BTreeMap<PeerId, RuntimeHost>,
}

/// Independent participant hosts connected by an in-process virtual network.
///
/// The ensemble owns topology and shutdown only. Each [`Host`] independently
/// owns its protocol accept path, negotiation driver, execution state, and
/// signing identity.
#[allow(missing_debug_implementations)]
pub struct Ensemble {
    network: LocalNetwork,
    state: StdMutex<EnsembleState>,
}

impl Ensemble {
    /// Start one Host per `(identity, store)` pair on a shared local network.
    ///
    /// The store handles are retained by each Host. Callers must retain the
    /// corresponding [`arena0_store::Store`] owners for as long as the
    /// ensemble is running; dropping an owner closes its durable authority.
    pub fn start(hosts: Vec<(Arc<NodeKeys>, StoreHandle)>) -> Result<Self, EnsembleError> {
        let mut specs = hosts;
        specs.sort_by_key(|(identity, _)| identity.peer_id());
        for pair in specs.windows(2) {
            if pair[0].0.peer_id() == pair[1].0.peer_id() {
                return Err(EnsembleError::DuplicateHost {
                    peer_id: pair[0].0.peer_id(),
                });
            }
        }

        let ensemble = Self {
            network: LocalNetwork::new(),
            state: StdMutex::new(EnsembleState {
                stopped: false,
                hosts: BTreeMap::new(),
            }),
        };
        for (identity, store) in specs {
            ensemble.add_host(identity, store)?;
        }
        Ok(ensemble)
    }

    /// Attach and start one Host on this ensemble's shared local network.
    pub fn add_host(
        &self,
        identity: Arc<NodeKeys>,
        store: StoreHandle,
    ) -> Result<Arc<Host>, EnsembleError> {
        let peer_id = identity.peer_id();
        let mut state = self.state.lock().unwrap();
        if state.stopped {
            return Err(EnsembleError::Stopped);
        }
        if state.hosts.contains_key(&peer_id) {
            return Err(EnsembleError::DuplicateHost { peer_id });
        }

        let transport = self.network.attach(peer_id).map_err(|error| match error {
            TransportError::DuplicatePeer { peer_id } => EnsembleError::DuplicateHost { peer_id },
            error => EnsembleError::Transport(error),
        })?;
        let transport = Arc::new(transport);
        let host_transport: Arc<dyn Transport + Sync> = transport.clone();
        let host = Host::start(identity, host_transport, store);
        debug_assert_eq!(host.peer_id, peer_id);
        state.hosts.insert(
            peer_id,
            RuntimeHost {
                host: Arc::clone(&host),
                transport,
            },
        );
        Ok(host)
    }

    /// Return one participant host by its persistent identity.
    #[must_use]
    pub fn host(&self, peer_id: &PeerId) -> Option<Arc<Host>> {
        self.state
            .lock()
            .unwrap()
            .hosts
            .get(peer_id)
            .map(|runtime| Arc::clone(&runtime.host))
    }

    /// Return one host's local transport endpoint.
    #[must_use]
    pub fn transport(&self, peer_id: &PeerId) -> Option<Arc<LocalTransport>> {
        self.state
            .lock()
            .unwrap()
            .hosts
            .get(peer_id)
            .map(|runtime| Arc::clone(&runtime.transport))
    }

    /// Return the canonical sorted host identities.
    #[must_use]
    pub fn peer_ids(&self) -> Vec<PeerId> {
        self.state.lock().unwrap().hosts.keys().copied().collect()
    }

    /// Stop and detach one Host, allowing its identity to be attached again.
    pub async fn remove_host(&self, peer_id: impl Borrow<PeerId>) -> Result<(), EnsembleError> {
        let peer_id = *peer_id.borrow();
        let runtime = self
            .state
            .lock()
            .unwrap()
            .hosts
            .remove(&peer_id)
            .ok_or(EnsembleError::HostNotFound { peer_id })?;
        runtime.host.stop().await;
        runtime.transport.close().await;
        Ok(())
    }

    /// Stop every host accept path, close every transport, and wait for both.
    pub async fn stop(&self) {
        let runtimes = {
            let mut state = self.state.lock().unwrap();
            state.stopped = true;
            state.hosts.values().cloned().collect::<Vec<_>>()
        };
        for runtime in runtimes {
            runtime.host.stop().await;
            runtime.transport.close().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_crypto::{NodeKeys, SecretKey};
    use arena0_store::{Store, StoreConfig};
    use tempfile::TempDir;

    type HostSpec = (Arc<NodeKeys>, StoreHandle);

    fn identity(byte: u8) -> Arc<NodeKeys> {
        Arc::new(NodeKeys::from_secret(SecretKey::from_bytes([byte; 32])))
    }

    fn hosts(bytes: impl IntoIterator<Item = u8>) -> (Vec<HostSpec>, Vec<Store>, Vec<TempDir>) {
        let mut specs = Vec::new();
        let mut stores = Vec::new();
        let mut directories = Vec::new();
        for byte in bytes {
            let identity = identity(byte);
            let directory = tempfile::tempdir().expect("store directory");
            let store = Store::open(StoreConfig::new(
                directory.path().join("arena0.sqlite"),
                identity.peer_id(),
            ))
            .expect("store");
            specs.push((Arc::clone(&identity), store.handle().clone()));
            stores.push(store);
            directories.push(directory);
        }
        (specs, stores, directories)
    }

    #[tokio::test]
    async fn allows_empty_and_single_host_topologies() {
        let empty = Ensemble::start(vec![]).expect("empty topology");
        assert!(empty.peer_ids().is_empty());
        empty.stop().await;

        let (one, _stores, _directories) = hosts([1]);
        let ensemble = Ensemble::start(one).expect("single-host topology");
        assert_eq!(ensemble.peer_ids().len(), 1);
        ensemble.stop().await;
    }

    #[test]
    fn rejects_duplicate_hosts() {
        let peer_id = identity(1).peer_id();
        let (hosts, _stores, _directories) = hosts([1, 1]);
        assert!(matches!(
            Ensemble::start(hosts),
            Err(EnsembleError::DuplicateHost { peer_id: duplicate }) if duplicate == peer_id
        ));
    }

    #[tokio::test]
    async fn starts_hosts_in_canonical_order() {
        let (hosts, _stores, _directories) = hosts([3, 1, 2]);
        let ensemble = Ensemble::start(hosts).unwrap();
        let peer_ids = ensemble.peer_ids();
        assert!(peer_ids.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(peer_ids.iter().all(
            |peer_id| ensemble.host(peer_id).is_some() && ensemble.transport(peer_id).is_some()
        ));
        ensemble.stop().await;
    }

    #[tokio::test]
    async fn adds_and_removes_hosts_on_one_network() {
        let (initial, mut stores, mut directories) = hosts([1]);
        let ensemble = Ensemble::start(initial).unwrap();
        let first_peer = identity(1).peer_id();

        let second_identity = identity(2);
        let second_directory = tempfile::tempdir().expect("store directory");
        let second_store = Store::open(StoreConfig::new(
            second_directory.path().join("arena0.sqlite"),
            second_identity.peer_id(),
        ))
        .expect("store");
        let second_handle = second_store.handle().clone();
        let second = ensemble
            .add_host(Arc::clone(&second_identity), second_handle)
            .expect("attach second host");
        assert!(
            ensemble
                .host(&second.peer_id)
                .is_some_and(|host| Arc::ptr_eq(&host, &second))
        );
        assert!(ensemble.transport(&second.peer_id).is_some());

        assert!(matches!(
            ensemble.add_host(Arc::clone(&second_identity), second_store.handle().clone()),
            Err(EnsembleError::DuplicateHost { peer_id }) if peer_id == second.peer_id
        ));

        ensemble.remove_host(second.peer_id).await.unwrap();
        assert!(ensemble.host(&second.peer_id).is_none());
        assert!(ensemble.transport(&second.peer_id).is_none());
        let reopened = ensemble
            .add_host(Arc::clone(&second_identity), second_store.handle().clone())
            .expect("reopen removed host");
        assert_eq!(reopened.peer_id, second.peer_id);
        let mut expected = vec![first_peer, second.peer_id];
        expected.sort();
        assert_eq!(ensemble.peer_ids(), expected);

        // Keep owners alive through the asynchronous host shutdowns.
        stores.push(second_store);
        directories.push(second_directory);
        ensemble.stop().await;
    }

    #[tokio::test]
    async fn rollback_and_stop_control_future_attachments() {
        let ensemble = Ensemble::start(vec![]).unwrap();
        let identity = identity(9);
        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(StoreConfig::new(
            directory.path().join("arena0.sqlite"),
            identity.peer_id(),
        ))
        .expect("store");
        let peer_id = identity.peer_id();
        ensemble
            .add_host(Arc::clone(&identity), store.handle().clone())
            .expect("attach host");
        ensemble.remove_host(peer_id).await.unwrap();
        ensemble
            .add_host(Arc::clone(&identity), store.handle().clone())
            .expect("attach after rollback");

        ensemble.stop().await;
        assert!(matches!(
            ensemble.add_host(identity, store.handle().clone()),
            Err(EnsembleError::Stopped)
        ));
    }
}
