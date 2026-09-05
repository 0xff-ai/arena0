//! Session identity, lifecycle, and participant ensemble.

use std::marker::PhantomData;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::PeerId;
use crate::id::id_type;

id_type!(
    /// The BLAKE3 hash of one canonical [`ActivationData`](crate::ActivationData):
    /// the session identity, fixed at activation.
    pub struct Hash,
    Default
);

id_type!(
    /// A random 32-byte value for execution-handshake uniqueness.
    ///
    /// This nonce makes one session credential unique.
    pub struct Nonce,
    Default
);

/// Sandbox/session lifecycle.
///
/// The runtime advances this through `PreSession -> Active -> Completed|Failed`.
/// The sandbox uses the current value to gate which host functions a program may
/// call (e.g. messaging is only valid during `Active`). Distinct from the
/// program-domain phase (`#[arena0::phases]`).
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
    borsh::BorshSchema,
)]
pub enum Lifecycle {
    /// Initialization and state setup before a session is active.
    #[default]
    PreSession,
    /// Main execution: full read/write, traced events, peer messaging.
    Active,
    /// Terminal: session completed successfully.
    Completed,
    /// Terminal: session ended due to an error or explicit abort.
    Failed,
}

impl Lifecycle {
    /// Returns `true` for `Completed` and `Failed`. No further events are
    /// dispatched once a session reaches a terminal lifecycle value.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

/// A participant's deterministic identity inside one session.
///
/// Participant 0 is assigned to the peer with the lexicographically lower
/// [`PeerId`]; participants are assigned by sorted position in the ensemble. This
/// is session identity, not transport identity and not UI display metadata.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    BorshSerialize,
    BorshDeserialize,
    borsh::BorshSchema,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
pub struct Participant(u8);

impl Participant {
    /// Construct from a numeric participant index.
    #[must_use]
    pub const fn new(index: u8) -> Self {
        Self(index)
    }

    /// Numeric index for array indexing.
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// Numeric index as `u8` for serialization and UI metadata.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        self.0
    }

    /// The other participant in a bilateral session.
    #[must_use]
    pub fn other(self) -> Self {
        assert!(
            self.0 < 2,
            "other() is only valid for bilateral participants"
        );
        Self(1 - self.0)
    }

    /// Derive the local participant from two PeerIds (lower = participant 0).
    #[must_use]
    pub fn of(local: &PeerId, remote: &PeerId) -> Self {
        if local.0 < remote.0 {
            Self::new(0)
        } else {
            Self::new(1)
        }
    }
}

impl From<u8> for Participant {
    fn from(value: u8) -> Self {
        Self::new(value)
    }
}

impl TryFrom<usize> for Participant {
    type Error = std::num::TryFromIntError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        u8::try_from(value).map(Self::new)
    }
}

/// The concurrency shape a program phase declares, fixing the canonical order
/// in which broadcast messages enter the public trace.
///
impl Default for Participant {
    fn default() -> Self {
        Self::new(0)
    }
}

impl std::fmt::Display for Participant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "participant:{}", self.0)
    }
}

/// An ensemble being assembled before the participant set is fixed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Open;

/// A fixed ensemble whose participant set and ordering are committed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Committed;

/// Invalid session ensemble participant set.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EnsembleError {
    /// Sessions need at least two participants.
    #[error("session ensemble needs at least two participants, got {len}")]
    TooFewParticipants { len: usize },
    /// Participant indices are compact `u8` values.
    #[error("session ensemble length must fit in u8, got {len}")]
    TooManyParticipants { len: usize },
}

/// A session ensemble with typestated construction.
///
/// `Ensemble<Open>` is a mutable participant-set collection. Calling
/// [`commit`](Ensemble<Open>::commit) sorts and deduplicates peers, validates the
/// participant-index bounds, and returns `Ensemble<Committed>`. The committed
/// form is the only form that can start a session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Ensemble<State = Committed> {
    peers: Vec<PeerId>,
    _state: PhantomData<State>,
}

impl Ensemble<Open> {
    /// Start building an ensemble with the local peer already included.
    #[must_use]
    pub fn new(local: PeerId) -> Self {
        Self {
            peers: vec![local],
            _state: PhantomData,
        }
    }

    /// Add a peer to the open ensemble. Sorting and deduplication happen at
    /// commit time, so caller input order does not affect the committed set.
    pub fn add(&mut self, peer: PeerId) {
        self.peers.push(peer);
    }

    /// Commit the participant set and freeze its ordering.
    pub fn commit(self) -> Result<Ensemble<Committed>, EnsembleError> {
        Ensemble::<Committed>::from_peers(self.peers)
    }
}

impl Ensemble<Committed> {
    /// Build a committed ensemble from a participant set.
    ///
    /// Peers are sorted and deduplicated so every participant computes the same
    /// participant ordering from the same set.
    pub fn from_peers(mut peers: Vec<PeerId>) -> Result<Self, EnsembleError> {
        peers.sort();
        peers.dedup();
        let len = peers.len();
        if len < 2 {
            return Err(EnsembleError::TooFewParticipants { len });
        }
        if len > crate::MAX_PARTICIPANTS {
            return Err(EnsembleError::TooManyParticipants { len });
        }
        Ok(Self {
            peers,
            _state: PhantomData,
        })
    }

    /// The ensemble in canonical participant order.
    #[must_use]
    pub fn peers(&self) -> &[PeerId] {
        &self.peers
    }

    /// Number of participants in this session.
    #[must_use]
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Whether the ensemble has no participants.
    ///
    /// Always false for values constructed through [`from_peers`](Self::from_peers).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Whether `peer` is a participant of the committed ensemble.
    #[must_use]
    pub fn contains(&self, peer: &PeerId) -> bool {
        self.peers.contains(peer)
    }

    /// The participant index assigned to `peer`, or `None` if absent.
    #[must_use]
    pub fn participant_of(&self, peer: &PeerId) -> Option<Participant> {
        self.peers.iter().position(|p| p == peer).map(|index| {
            Participant::try_from(index).expect("committed ensemble length fits in u8")
        })
    }

    /// The peer at participant index `participant`, or `None` if out of range.
    #[must_use]
    pub fn peer_at(&self, participant: Participant) -> Option<PeerId> {
        self.peers.get(participant.index()).copied()
    }

    /// Every participant except `local`, in sorted order.
    pub fn others<'a>(&'a self, local: &'a PeerId) -> impl Iterator<Item = PeerId> + 'a {
        self.peers.iter().copied().filter(move |p| p != local)
    }
}

impl BorshSerialize for Ensemble<Committed> {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        if self.peers.len() > crate::MAX_PARTICIPANTS {
            return Err(borsh::io::Error::new(
                borsh::io::ErrorKind::InvalidInput,
                "ensemble participant count exceeds bound",
            ));
        }
        BorshSerialize::serialize(&self.peers, writer)
    }
}

impl BorshDeserialize for Ensemble<Committed> {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let len = u32::deserialize_reader(reader)? as usize;
        if len > crate::MAX_PARTICIPANTS {
            return Err(borsh::io::Error::new(
                borsh::io::ErrorKind::InvalidData,
                "ensemble participant count exceeds bound",
            ));
        }
        let mut peers = Vec::with_capacity(len);
        for _ in 0..len {
            peers.push(PeerId::deserialize_reader(reader)?);
        }
        Self::from_peers(peers).map_err(|err| {
            borsh::io::Error::new(
                borsh::io::ErrorKind::InvalidData,
                format!("invalid session ensemble: {err}"),
            )
        })
    }
}

impl Serialize for Ensemble<Committed> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&self.peers, serializer)
    }
}

impl<'de> Deserialize<'de> for Ensemble<Committed> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let peers = <Vec<PeerId> as Deserialize>::deserialize(deserializer)?;
        Self::from_peers(peers).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(b: u8) -> PeerId {
        PeerId([b; 32])
    }

    #[test]
    fn other_is_inverse_for_bilateral_participants() {
        assert_eq!(Participant::new(0).other(), Participant::new(1));
        assert_eq!(Participant::new(1).other(), Participant::new(0));
    }

    #[test]
    #[should_panic(expected = "other() is only valid for bilateral participants")]
    fn other_rejects_non_bilateral_participants() {
        let _ = Participant::new(2).other();
    }

    #[test]
    fn of_deterministic() {
        let a = PeerId([1u8; 32]);
        let b = PeerId([2u8; 32]);
        assert_eq!(Participant::of(&a, &b), Participant::new(0));
        assert_eq!(Participant::of(&b, &a), Participant::new(1));
    }

    #[test]
    fn try_from_usize_accepts_u8_max() {
        assert_eq!(
            Participant::try_from(usize::from(u8::MAX)).expect("u8 max fits in usize"),
            Participant::new(u8::MAX)
        );
    }

    #[test]
    fn try_from_usize_rejects_values_above_u8_max() {
        assert!(Participant::try_from(usize::from(u8::MAX) + 1).is_err());
    }

    #[test]
    fn open_ensemble_commits_sorted_membership() {
        let mut ensemble = Ensemble::<Open>::new(peer(3));
        ensemble.add(peer(1));
        ensemble.add(peer(2));
        ensemble.add(peer(1));

        let committed = ensemble.commit().unwrap();
        assert_eq!(committed.peers(), &[peer(1), peer(2), peer(3)]);
        assert_eq!(
            committed.participant_of(&peer(2)),
            Some(Participant::try_from(1usize).expect("test participant index fits in u8"))
        );
        assert_eq!(
            committed.peer_at(
                Participant::try_from(2usize).expect("test participant index fits in u8"),
            ),
            Some(peer(3))
        );
        assert_eq!(
            committed.others(&peer(2)).collect::<Vec<_>>(),
            vec![peer(1), peer(3)]
        );
    }

    #[test]
    fn committed_ensemble_rejects_too_few_participants() {
        let mut ensemble = Ensemble::<Open>::new(peer(1));
        assert_eq!(
            ensemble.clone().commit().unwrap_err(),
            EnsembleError::TooFewParticipants { len: 1 }
        );

        ensemble.add(peer(2));
        assert!(ensemble.commit().is_ok());
    }

    #[test]
    fn committed_ensemble_rejects_more_than_protocol_maximum() {
        let peers = (0..=crate::MAX_PARTICIPANTS)
            .map(|index| {
                let mut bytes = [0u8; 32];
                bytes[..8].copy_from_slice(&u64::try_from(index).unwrap().to_le_bytes());
                PeerId(bytes)
            })
            .collect();

        assert!(matches!(
            Ensemble::<Committed>::from_peers(peers),
            Err(EnsembleError::TooManyParticipants { len }) if len == crate::MAX_PARTICIPANTS + 1
        ));
    }

    #[test]
    fn committed_ensemble_deserialization_validates_membership() {
        let encoded_single = borsh::to_vec(&vec![peer(1)]).unwrap();
        let err = borsh::from_slice::<Ensemble>(&encoded_single).unwrap_err();
        assert_eq!(err.kind(), borsh::io::ErrorKind::InvalidData);

        let committed = Ensemble::from_peers(vec![peer(2), peer(1)]).unwrap();
        let encoded = borsh::to_vec(&committed).unwrap();
        let decoded = borsh::from_slice::<Ensemble>(&encoded).unwrap();
        assert_eq!(decoded.peers(), &[peer(1), peer(2)]);
    }

    #[test]
    fn lifecycle_terminal() {
        assert!(!Lifecycle::PreSession.is_terminal());
        assert!(!Lifecycle::Active.is_terminal());
        assert!(Lifecycle::Completed.is_terminal());
        assert!(Lifecycle::Failed.is_terminal());
    }
}
