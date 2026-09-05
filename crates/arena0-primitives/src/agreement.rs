//! Single-proposal agreement lifecycle built on [`Ballot`](crate::ballot::Ballot).
//!
//! This is program-level agreement policy. arena0's protocol still performs
//! N-of-N validation for every shared-state transition.

use arena0::prelude::*;
use borsh::BorshSerialize;

use crate::ballot::{Ballot, ProposalId, Tally, Vote};

const PROPOSAL_HASH_DOMAIN: &[u8] = b"arena0/agreement/proposal/v1\0";

/// Borrowed projection of the proposal tracked by an [`Agreement`].
#[derive(Debug)]
pub struct ProposalRef<'a, T> {
    pub id: ProposalId,
    pub value: &'a T,
}

/// Public lifecycle projection for a single-proposal agreement.
#[arena0::data]
#[derive(Copy)]
pub enum Status {
    Idle,
    Voting,
    Accepted,
    Rejected,
}

/// Errors returned by agreement lifecycle transitions.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("agreement already has a proposal")]
    AlreadyProposed,
    #[error("agreement is not accepting votes")]
    NotVoting,
    #[error("acceptance references a different proposal")]
    ProposalMismatch,
    #[error("proposal could not be deterministically encoded")]
    ProposalEncoding,
    #[error(transparent)]
    Ballot(#[from] crate::ballot::Error),
}

/// Lifecycle owner for one proposal and its exact ballot.
///
/// [`propose`](Self::propose) fixes the proposal bytes, version, eligible set,
/// and threshold. [`vote`](Self::vote) delegates all vote-slot
/// and result calculation to [`Ballot`]; `Agreement` stores no second tally.
#[arena0::primitive]
#[derive(PartialEq, Eq)]
pub struct Agreement<T> {
    value: Option<T>,
    ballot: Ballot,
}

impl<T> Default for Agreement<T> {
    fn default() -> Self {
        Self {
            value: None,
            ballot: Ballot::default(),
        }
    }
}

impl<T> Agreement<T> {
    /// Begin the only proposal lifecycle this agreement can own.
    pub fn propose(
        &mut self,
        version: u32,
        value: T,
        eligible: Vec<Participant>,
        threshold: u16,
    ) -> Result<ProposalId, Error>
    where
        T: BorshSerialize,
    {
        if self.ballot.proposal().is_some() {
            return Err(Error::AlreadyProposed);
        }
        let id = proposal_id(version, &value)?;
        self.ballot = Ballot::new(id, eligible, threshold)?;
        self.value = Some(value);
        Ok(id)
    }

    /// Cast one participant's vote for the exact proposal identity.
    pub fn vote(
        &mut self,
        participant: Participant,
        proposal_id: ProposalId,
        vote: Vote,
    ) -> Result<Tally, Error> {
        if self.status() != Status::Voting {
            return Err(Error::NotVoting);
        }
        if self.ballot.proposal() != Some(proposal_id) {
            return Err(Error::ProposalMismatch);
        }
        self.ballot.cast(participant, vote).map_err(Into::into)
    }

    /// Current proposal lifecycle.
    #[must_use]
    pub fn status(&self) -> Status {
        match self.ballot.tally() {
            Tally::NotStarted => Status::Idle,
            Tally::Pending { .. } => Status::Voting,
            Tally::Accepted { .. } => Status::Accepted,
            Tally::Rejected { .. } => Status::Rejected,
        }
    }

    /// Proposal under consideration or already decided.
    #[must_use]
    pub fn proposal(&self) -> Option<ProposalRef<'_, T>> {
        Some(ProposalRef {
            id: self.ballot.proposal()?,
            value: self.value.as_ref()?,
        })
    }

    /// The ballot that owns the agreement's votes and tally.
    #[must_use]
    pub fn ballot(&self) -> Option<&Ballot> {
        self.ballot.proposal().map(|_| &self.ballot)
    }

    /// Accepted proposal value, if the ballot reached its threshold.
    #[must_use]
    pub fn accepted(&self) -> Option<ProposalRef<'_, T>> {
        (self.status() == Status::Accepted)
            .then(|| self.proposal())
            .flatten()
    }
}

fn proposal_id<T: BorshSerialize>(version: u32, value: &T) -> Result<ProposalId, Error> {
    if version == 0 {
        return Err(crate::ballot::Error::InvalidProposalVersion.into());
    }
    let encoded = borsh::to_vec(value).map_err(|_| Error::ProposalEncoding)?;
    let mut preimage = Vec::with_capacity(PROPOSAL_HASH_DOMAIN.len() + 4 + encoded.len());
    preimage.extend_from_slice(PROPOSAL_HASH_DOMAIN);
    preimage.extend_from_slice(&version.to_le_bytes());
    preimage.extend_from_slice(&encoded);
    Ok(ProposalId {
        version,
        hash: *blake3::hash(&preimage).as_bytes(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(index: u8) -> Participant {
        Participant::new(index)
    }

    #[test]
    fn lifecycle_uses_the_ballot_as_its_only_tally() {
        let mut agreement = Agreement::default();
        let id = agreement
            .propose(
                1,
                42_u32,
                vec![participant(2), participant(0), participant(1)],
                2,
            )
            .unwrap();

        assert_eq!(agreement.status(), Status::Voting);
        assert_eq!(
            agreement.vote(participant(0), id, Vote::Accept).unwrap(),
            Tally::Pending {
                accepts: 1,
                rejects: 0,
                remaining: 2,
                threshold: 2,
            }
        );
        assert_eq!(
            agreement.vote(participant(1), id, Vote::Accept).unwrap(),
            Tally::Accepted {
                accepts: 2,
                rejects: 0,
                threshold: 2,
            }
        );
        assert_eq!(agreement.status(), Status::Accepted);
        assert_eq!(
            agreement.accepted().map(|proposal| *proposal.value),
            Some(42)
        );
        assert_eq!(
            agreement.ballot().map(Ballot::tally),
            Some(Tally::Accepted {
                accepts: 2,
                rejects: 0,
                threshold: 2,
            })
        );
    }

    #[test]
    fn invalid_transitions_preserve_the_lifecycle() {
        let mut agreement = Agreement::default();
        let unknown = ProposalId {
            version: 1,
            hash: [9; 32],
        };
        assert_eq!(
            agreement.vote(participant(0), unknown, Vote::Accept),
            Err(Error::NotVoting)
        );

        let id = agreement.propose(3, 9_u8, vec![participant(0)], 1).unwrap();
        assert_eq!(
            agreement.vote(participant(0), unknown, Vote::Accept),
            Err(Error::ProposalMismatch)
        );
        assert_eq!(agreement.status(), Status::Voting);
        assert_eq!(
            agreement.propose(3, 10_u8, vec![participant(0)], 1),
            Err(Error::AlreadyProposed)
        );
        agreement.vote(participant(0), id, Vote::Accept).unwrap();
        assert_eq!(
            agreement.vote(participant(0), id, Vote::Accept),
            Err(Error::NotVoting)
        );
    }

    #[test]
    fn proposal_hash_and_encoding_are_stable() {
        let mut agreement = Agreement::default();
        let id = agreement
            .propose(3, 42_u32, vec![participant(2)], 1)
            .unwrap();

        let digest = id
            .hash
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            digest,
            "092dc286364c9a1c0f8192e5aa3150859ac195013de8721021590e89d0a55a4a"
        );
        let encoded = borsh::to_vec(&agreement).unwrap();
        // Some(42u32), Some(ProposalId { version: 3, hash }), one eligible
        // participant (2), one empty vote slot, threshold 1.
        let mut expected = vec![1, 42, 0, 0, 0, 1, 3, 0, 0, 0];
        expected.extend([
            0x09, 0x2d, 0xc2, 0x86, 0x36, 0x4c, 0x9a, 0x1c, 0x0f, 0x81, 0x92, 0xe5, 0xaa, 0x31,
            0x50, 0x85, 0x9a, 0xc1, 0x95, 0x01, 0x3d, 0xe8, 0x72, 0x10, 0x21, 0x59, 0x0e, 0x89,
            0xd0, 0xa5, 0x5a, 0x4a,
        ]);
        expected.extend([1, 0, 0, 0, 2, 1, 0, 0, 0, 0, 1, 0]);
        assert_eq!(encoded, expected);
        assert_eq!(
            borsh::from_slice::<Agreement<u32>>(&expected).unwrap(),
            agreement
        );
    }

    #[test]
    fn rejected_ballot_closes_the_agreement_without_an_accepted_value() {
        let mut agreement = Agreement::default();
        let id = agreement
            .propose(1, 42_u32, vec![participant(0), participant(1)], 2)
            .unwrap();

        assert_eq!(
            agreement.vote(participant(0), id, Vote::Reject).unwrap(),
            Tally::Rejected {
                accepts: 0,
                rejects: 1,
                threshold: 2,
            }
        );
        assert_eq!(agreement.status(), Status::Rejected);
        assert!(agreement.accepted().is_none());
        assert_eq!(
            agreement.vote(participant(1), id, Vote::Accept),
            Err(Error::NotVoting)
        );
    }
}
