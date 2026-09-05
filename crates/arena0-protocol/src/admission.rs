//! Durable local authority for starting or joining one execution.

use std::io;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::{Committed, Ensemble, EnsembleError, NegotiationId, PeerId};

const ADMISSION_VERSION: u8 = 1;

/// The exact caller authority persisted before negotiation or secret creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionAdmission {
    /// Create a negotiation for one exact, canonically ordered participant set.
    Explicit {
        /// Identity of the negotiation created for this request.
        negotiation_id: NegotiationId,
        /// Complete participant set, including the local creator.
        peers: Ensemble<Committed>,
    },
    /// Join one exact negotiation published by one exact creator.
    Join {
        /// Creator whose authenticated offer may satisfy this request.
        creator: PeerId,
        /// Identity of the negotiation to join.
        negotiation_id: NegotiationId,
    },
}

impl ExecutionAdmission {
    /// Validate and canonicalize an explicit participant set.
    pub fn explicit(
        negotiation_id: NegotiationId,
        peers: Vec<PeerId>,
    ) -> Result<Self, EnsembleError> {
        Ok(Self::Explicit {
            negotiation_id,
            peers: Ensemble::from_peers(peers)?,
        })
    }

    /// Bind a join request to one creator and negotiation.
    #[must_use]
    pub const fn join(creator: PeerId, negotiation_id: NegotiationId) -> Self {
        Self::Join {
            creator,
            negotiation_id,
        }
    }

    /// Return the only negotiation authorized by this request.
    #[must_use]
    pub const fn negotiation_id(&self) -> NegotiationId {
        match self {
            Self::Explicit { negotiation_id, .. } | Self::Join { negotiation_id, .. } => {
                *negotiation_id
            }
        }
    }

    /// Return the exact explicit participant set, if this request creates one.
    #[must_use]
    pub const fn explicit_peers(&self) -> Option<&Ensemble<Committed>> {
        match self {
            Self::Explicit { peers, .. } => Some(peers),
            Self::Join { .. } => None,
        }
    }

    /// Return the required creator for a join request.
    #[must_use]
    pub const fn join_creator(&self) -> Option<PeerId> {
        match self {
            Self::Join { creator, .. } => Some(*creator),
            Self::Explicit { .. } => None,
        }
    }
}

impl BorshSerialize for ExecutionAdmission {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        BorshSerialize::serialize(&ADMISSION_VERSION, writer)?;
        match self {
            Self::Explicit {
                negotiation_id,
                peers,
            } => {
                BorshSerialize::serialize(&0u8, writer)?;
                BorshSerialize::serialize(negotiation_id, writer)?;
                BorshSerialize::serialize(peers, writer)
            }
            Self::Join {
                creator,
                negotiation_id,
            } => {
                BorshSerialize::serialize(&1u8, writer)?;
                BorshSerialize::serialize(creator, writer)?;
                BorshSerialize::serialize(negotiation_id, writer)
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
            0 => Ok(Self::Explicit {
                negotiation_id: NegotiationId::deserialize_reader(reader)?,
                peers: Ensemble::<Committed>::deserialize_reader(reader)?,
            }),
            1 => Ok(Self::Join {
                creator: PeerId::deserialize_reader(reader)?,
                negotiation_id: NegotiationId::deserialize_reader(reader)?,
            }),
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
    fn explicit_is_canonical_and_decode_revalidates_the_ensemble() {
        let admission = ExecutionAdmission::explicit(
            NegotiationId([7; 32]),
            vec![PeerId([2; 32]), PeerId([1; 32])],
        )
        .expect("explicit admission");
        assert_eq!(
            admission.explicit_peers().expect("peers").peers(),
            &[PeerId([1; 32]), PeerId([2; 32])]
        );
        let encoded = borsh::to_vec(&admission).expect("encode");
        assert_eq!(
            borsh::from_slice::<ExecutionAdmission>(&encoded).expect("decode"),
            admission
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
}
