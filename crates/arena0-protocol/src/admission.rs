//! Durable local authority for starting or joining one execution.

use std::io;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::{EnsembleError, NegotiationId, PeerId};

const ADMISSION_VERSION: u8 = 1;

/// A creator and negotiation selected by a join request.
///
/// Open joins start without this value. The daemon persists the value it first
/// accepts before the local ticket is signed, so a retry cannot move the
/// execution to another offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NegotiationTarget {
    /// The Host that authored the offer.
    pub creator: PeerId,
    /// The negotiation published by the creator.
    pub negotiation_id: NegotiationId,
}

impl NegotiationTarget {
    /// Bind one target.
    #[must_use]
    pub const fn new(creator: PeerId, negotiation_id: NegotiationId) -> Self {
        Self {
            creator,
            negotiation_id,
        }
    }
}

/// The exact caller authority persisted before negotiation or secret creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionAdmission {
    /// Create a negotiation and collect exactly this many participants.
    Create {
        /// Identity of the negotiation created for this request.
        negotiation_id: NegotiationId,
        /// Total participant count, including the local creator.
        participant_count: u16,
    },
    /// Join a negotiation. `target == None` is an open join and is filled with
    /// the first valid offer the Host accepts from the program topic.
    Join {
        /// Exact creator and negotiation, when selected.
        target: Option<NegotiationTarget>,
    },
}

impl ExecutionAdmission {
    /// Construct a creator admission for an open offer.
    pub fn create(
        negotiation_id: NegotiationId,
        participant_count: u16,
    ) -> Result<Self, EnsembleError> {
        if participant_count < 2 {
            return Err(EnsembleError::TooFewParticipants {
                len: participant_count as usize,
            });
        }
        if participant_count > crate::MAX_PARTICIPANTS as u16 {
            return Err(EnsembleError::TooManyParticipants {
                len: participant_count as usize,
            });
        }
        Ok(Self::Create {
            negotiation_id,
            participant_count,
        })
    }

    /// Bind a join request to one creator and negotiation.
    #[must_use]
    pub const fn join(creator: PeerId, negotiation_id: NegotiationId) -> Self {
        Self::Join {
            target: Some(NegotiationTarget::new(creator, negotiation_id)),
        }
    }

    /// Construct an open join. The selected target is persisted before any
    /// local ticket is issued.
    #[must_use]
    pub const fn join_open() -> Self {
        Self::Join { target: None }
    }

    /// Return the only negotiation authorized by this request.
    #[must_use]
    pub const fn negotiation_id(&self) -> Option<NegotiationId> {
        match self {
            Self::Create { negotiation_id, .. } => Some(*negotiation_id),
            Self::Join {
                target: Some(target),
            } => Some(target.negotiation_id),
            Self::Join { target: None } => None,
        }
    }

    /// Return the selected exact target, if this request has one.
    #[must_use]
    pub const fn target(&self) -> Option<NegotiationTarget> {
        match self {
            Self::Join { target, .. } => *target,
            _ => None,
        }
    }

    /// Return the creator when this request has an exact creator authority.
    #[must_use]
    pub const fn creator(&self) -> Option<PeerId> {
        match self {
            Self::Join {
                target: Some(target),
                ..
            } => Some(target.creator),
            _ => None,
        }
    }

    /// Return the requested total participant count for a creator admission.
    #[must_use]
    pub fn participant_count(&self) -> Option<u16> {
        match self {
            Self::Create {
                participant_count, ..
            } => Some(*participant_count),
            Self::Join { .. } => None,
        }
    }

    /// Return the required creator for a join request.
    #[must_use]
    pub const fn join_creator(&self) -> Option<PeerId> {
        match self {
            Self::Join {
                target: Some(target),
                ..
            } => Some(target.creator),
            Self::Create { .. } | Self::Join { target: None, .. } => None,
        }
    }
}

impl BorshSerialize for ExecutionAdmission {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        BorshSerialize::serialize(&ADMISSION_VERSION, writer)?;
        match self {
            Self::Create {
                negotiation_id,
                participant_count,
            } => {
                BorshSerialize::serialize(&2u8, writer)?;
                BorshSerialize::serialize(negotiation_id, writer)?;
                BorshSerialize::serialize(participant_count, writer)
            }
            Self::Join {
                target: Some(target),
            } => {
                BorshSerialize::serialize(&1u8, writer)?;
                BorshSerialize::serialize(&target.creator, writer)?;
                BorshSerialize::serialize(&target.negotiation_id, writer)
            }
            Self::Join { target: None } => {
                BorshSerialize::serialize(&3u8, writer)?;
                Ok(())
            }
        }
    }
}

impl BorshDeserialize for ExecutionAdmission {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        let version = u8::deserialize_reader(reader)?;
        if version != ADMISSION_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown execution admission version {version}"),
            ));
        }
        match u8::deserialize_reader(reader)? {
            1 => {
                let creator = PeerId::deserialize_reader(reader)?;
                let negotiation_id = NegotiationId::deserialize_reader(reader)?;
                Ok(Self::Join {
                    target: Some(NegotiationTarget::new(creator, negotiation_id)),
                })
            }
            2 => Self::create(
                NegotiationId::deserialize_reader(reader)?,
                u16::deserialize_reader(reader)?,
            )
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            3 => Ok(Self::Join { target: None }),
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown execution admission tag {tag}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_explicit_tag_is_rejected() {
        let error = borsh::from_slice::<ExecutionAdmission>(&[ADMISSION_VERSION, 0])
            .expect_err("retired explicit admission tag must stay invalid");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error
                .to_string()
                .contains("unknown execution admission tag 0")
        );
    }

    #[test]
    fn tags_and_version_are_explicit() {
        let join = ExecutionAdmission::join(PeerId([1; 32]), NegotiationId([2; 32]));
        let mut encoded = borsh::to_vec(&join).expect("encode");
        assert_eq!(&encoded[..2], &[ADMISSION_VERSION, 1]);
        encoded[0] = 9;
        assert!(borsh::from_slice::<ExecutionAdmission>(&encoded).is_err());
    }

    #[test]
    fn open_join_round_trips_and_binds_only_as_a_target() {
        let admission = ExecutionAdmission::join_open();
        let encoded = borsh::to_vec(&admission).expect("encode");
        assert_eq!(&encoded[..2], &[ADMISSION_VERSION, 3]);
        assert_eq!(
            borsh::from_slice::<ExecutionAdmission>(&encoded).unwrap(),
            admission
        );
        assert_eq!(admission.target(), None);
    }

    #[test]
    fn create_decode_rejects_invalid_participant_counts() {
        for participant_count in [0, 1, u16::MAX] {
            let encoded = borsh::to_vec(&ExecutionAdmission::Create {
                negotiation_id: NegotiationId([4; 32]),
                participant_count,
            })
            .unwrap();
            assert!(borsh::from_slice::<ExecutionAdmission>(&encoded).is_err());
        }
    }

    #[test]
    fn create_round_trips_with_count() {
        let admission = ExecutionAdmission::create(NegotiationId([4; 32]), 3).unwrap();
        let encoded = borsh::to_vec(&admission).expect("encode");
        assert_eq!(
            borsh::from_slice::<ExecutionAdmission>(&encoded).unwrap(),
            admission
        );
        assert_eq!(admission.participant_count(), Some(3));
    }
}
