//! A bounded set of [`Host`](crate::Host)s connected through one local network.

use std::collections::BTreeMap;
use std::sync::Arc;

use arena0_crypto::NodeKeys;
use arena0_protocol::{MAX_PARTICIPANTS, PeerId, PeerIdSource};
use arena0_store::StoreHandle;
use arena0_transport::Transport;
use arena0_transport::local::{LocalNetwork, LocalTransport};
use thiserror::Error;

use crate::Host;

/// Errors returned while constructing a local [`Ensemble`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EnsembleError {
    #[error("an ensemble requires 2..={MAX_PARTICIPANTS} hosts, got {count}")]
    ParticipantCount { count: usize },
    #[error("duplicate host identity {peer_id}")]
    DuplicateHost { peer_id: PeerId },
    #[error("invalid host identity: {0}")]
    InvalidIdentity(String),
}

/// Independent participant hosts connected by an in-process virtual network.
///
/// The ensemble owns topology and shutdown only. Each [`Host`] independently
/// owns its protocol accept path, negotiation driver, execution state, and
/// signing identity.
#[allow(missing_debug_implementations)]
pub struct Ensemble {
    hosts: BTreeMap<PeerId, Arc<Host>>,
    transports: BTreeMap<PeerId, Arc<LocalTransport>>,
}

impl Ensemble {
    /// Start one Host per `(identity, store)` pair on a shared local network.
    ///
    /// The store handles are retained by each Host. Callers must retain the
    /// corresponding [`arena0_store::Store`] owners for as long as the
    /// ensemble is running; dropping an owner closes its durable authority.
    pub fn start(hosts: Vec<(Arc<NodeKeys>, StoreHandle)>) -> Result<Self, EnsembleError> {
        if !(2..=MAX_PARTICIPANTS).contains(&hosts.len()) {
            return Err(EnsembleError::ParticipantCount { count: hosts.len() });
        }

        let mut specs = hosts
            .into_iter()
            .map(|(identity, store)| (identity.peer_id(), (identity, store)))
            .collect::<Vec<_>>();
        specs.sort_by_key(|(peer_id, _)| *peer_id);
        for pair in specs.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(EnsembleError::DuplicateHost { peer_id: pair[0].0 });
            }
        }

        let network = LocalNetwork::new();
        let peer_ids = specs
            .iter()
            .map(|(peer_id, _)| *peer_id)
            .collect::<Vec<_>>();
        let local_transports = LocalTransport::create_network(&network, peer_ids.clone());

        let mut transports = BTreeMap::new();
        let mut host_map = BTreeMap::new();
        for ((peer_id, (identity, store)), transport) in specs.into_iter().zip(local_transports) {
            let transport = Arc::new(transport);
            let host_transport: Arc<dyn Transport + Sync> = transport.clone();
            let host = Host::start(identity, host_transport, store);
            debug_assert_eq!(host.peer_id, peer_id);
            host_map.insert(peer_id, host);
            transports.insert(peer_id, transport);
        }

        Ok(Self {
            hosts: host_map,
            transports,
        })
    }

    /// Return one participant host by its persistent identity.
    #[must_use]
    pub fn host(&self, peer_id: &PeerId) -> Option<Arc<Host>> {
        self.hosts.get(peer_id).cloned()
    }

    /// Return one host's local transport endpoint.
    #[must_use]
    pub fn transport(&self, peer_id: &PeerId) -> Option<Arc<LocalTransport>> {
        self.transports.get(peer_id).cloned()
    }

    /// Return the canonical sorted host identities.
    #[must_use]
    pub fn peer_ids(&self) -> Vec<PeerId> {
        self.hosts.keys().copied().collect()
    }

    /// Stop every host accept path, close every transport, and wait for both.
    pub async fn stop(&self) {
        for host in self.hosts.values() {
            host.stop().await;
        }
        for transport in self.transports.values() {
            transport.close().await;
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

    #[test]
    fn rejects_invalid_host_counts() {
        let (one, _stores, _directories) = hosts([1]);
        assert!(matches!(
            Ensemble::start(one),
            Err(EnsembleError::ParticipantCount { count: 1 })
        ));
        let (too_many, _stores, _directories) = hosts(0..MAX_PARTICIPANTS as u8 + 1);
        assert!(matches!(
            Ensemble::start(too_many),
            Err(EnsembleError::ParticipantCount { count }) if count == MAX_PARTICIPANTS + 1
        ));
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
}
