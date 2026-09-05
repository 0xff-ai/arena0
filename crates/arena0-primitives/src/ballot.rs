//! Deterministic voting over one exact proposal.
//!
//! `Ballot` is program-level policy. It does not replace arena0's N-of-N
//! agreement over shared-state steps. A program embeds a ballot in shared
//! state when its own rules need an explicit vote.

use arena0::prelude::*;

/// Stable identity of the proposal a ballot accepts or rejects.
#[arena0::data]
#[derive(Copy)]
pub struct ProposalId {
    /// Program-owned proposal format version.
    pub version: u32,
    /// BLAKE3 digest of the versioned proposal bytes.
    pub hash: [u8; 32],
}

/// One eligible participant's vote.
#[arena0::data]
#[derive(Copy)]
pub enum Vote {
    Accept,
    Reject,
}

/// Deterministic projection of the current ballot.
#[arena0::data]
#[derive(Copy)]
pub enum Tally {
    /// No proposal has configured this ballot yet.
    NotStarted,
    /// Neither result is mathematically final yet.
    Pending {
        accepts: u16,
        rejects: u16,
        remaining: u16,
        threshold: u16,
    },
    /// The acceptance threshold has been met.
    Accepted {
        accepts: u16,
        rejects: u16,
        threshold: u16,
    },
    /// Too few uncast votes remain to meet the threshold.
    Rejected {
        accepts: u16,
        rejects: u16,
        threshold: u16,
    },
}

/// Errors returned by ballot construction and transitions.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("a ballot requires at least one eligible participant")]
    NoEligibleParticipants,
    #[error("participant {0} appears more than once in the eligible set")]
    DuplicateParticipant(usize),
    #[error("ballot threshold {threshold} must be between 1 and {eligible}")]
    InvalidThreshold { threshold: u16, eligible: usize },
    #[error("proposal version must be greater than zero")]
    InvalidProposalVersion,
    #[error("ballot has not started")]
    NotStarted,
    #[error("participant {0} is not eligible to vote")]
    IneligibleParticipant(usize),
    #[error("participant {0} has already voted")]
    DuplicateVote(usize),
    #[error("ballot is already closed")]
    Closed,
}

/// Exact-set integer-threshold ballot for one versioned proposal.
///
/// Eligible participants are stored in canonical participant-index order.
/// Each participant owns one aligned vote slot. [`tally`](Self::tally) is the
/// sole result calculation; consumers should not keep another count.
#[arena0::primitive]
#[derive(Default, PartialEq, Eq)]
pub struct Ballot {
    proposal: Option<ProposalId>,
    eligible: Vec<Participant>,
    votes: Vec<Option<Vote>>,
    threshold: u16,
}

impl Ballot {
    /// Configure a ballot for exactly `proposal` and `eligible`.
    ///
    /// The participant list is canonicalized by index. Duplicates and integer
    /// thresholds outside `1..=eligible.len()` are rejected.
    pub fn new(
        proposal: ProposalId,
        mut eligible: Vec<Participant>,
        threshold: u16,
    ) -> Result<Self, Error> {
        if proposal.version == 0 {
            return Err(Error::InvalidProposalVersion);
        }
        if eligible.is_empty() {
            return Err(Error::NoEligibleParticipants);
        }

        eligible.sort_unstable_by_key(|participant| participant.as_u8());
        if let Some(duplicate) = eligible
            .windows(2)
            .find(|pair| pair[0] == pair[1])
            .map(|pair| pair[0])
        {
            return Err(Error::DuplicateParticipant(duplicate.index()));
        }
        if threshold == 0 || usize::from(threshold) > eligible.len() {
            return Err(Error::InvalidThreshold {
                threshold,
                eligible: eligible.len(),
            });
        }

        let votes = vec![None; eligible.len()];
        Ok(Self {
            proposal: Some(proposal),
            eligible,
            votes,
            threshold,
        })
    }

    /// Proposal identity fixed when this ballot was created.
    #[must_use]
    pub fn proposal(&self) -> Option<ProposalId> {
        self.proposal
    }

    /// Canonically ordered exact eligible set.
    #[must_use]
    pub fn eligible(&self) -> &[Participant] {
        &self.eligible
    }

    /// Integer acceptance threshold.
    #[must_use]
    pub fn threshold(&self) -> u16 {
        self.threshold
    }

    /// Return an eligible participant's vote, if cast.
    #[must_use]
    pub fn vote_of(&self, participant: Participant) -> Option<Vote> {
        let position = self
            .eligible
            .binary_search_by_key(&participant.as_u8(), |candidate| candidate.as_u8())
            .ok()?;
        self.votes.get(position).copied().flatten()
    }

    /// First eligible participant who still owes a vote, if the ballot remains open.
    #[must_use]
    pub fn next_voter(&self) -> Option<Participant> {
        matches!(self.tally(), Tally::Pending { .. })
            .then(|| {
                self.votes
                    .iter()
                    .position(Option::is_none)
                    .and_then(|position| self.eligible.get(position).copied())
            })
            .flatten()
    }

    /// Cast one vote into the participant's unique slot.
    pub fn cast(&mut self, participant: Participant, vote: Vote) -> Result<Tally, Error> {
        if self.proposal.is_none() {
            return Err(Error::NotStarted);
        }
        if !matches!(self.tally(), Tally::Pending { .. }) {
            return Err(Error::Closed);
        }
        let position = self
            .eligible
            .binary_search_by_key(&participant.as_u8(), |candidate| candidate.as_u8())
            .map_err(|_| Error::IneligibleParticipant(participant.index()))?;
        let slot = self
            .votes
            .get_mut(position)
            .expect("ballot vote slots match the eligible set");
        if slot.is_some() {
            return Err(Error::DuplicateVote(participant.index()));
        }
        *slot = Some(vote);
        Ok(self.tally())
    }

    /// Derive the ballot result from its vote slots and threshold.
    #[must_use]
    pub fn tally(&self) -> Tally {
        if self.proposal.is_none() {
            return Tally::NotStarted;
        }

        let accepts = self
            .votes
            .iter()
            .filter(|vote| matches!(vote, Some(Vote::Accept)))
            .count() as u16;
        let rejects = self
            .votes
            .iter()
            .filter(|vote| matches!(vote, Some(Vote::Reject)))
            .count() as u16;
        let remaining = self.eligible.len() as u16 - accepts - rejects;

        if accepts >= self.threshold {
            Tally::Accepted {
                accepts,
                rejects,
                threshold: self.threshold,
            }
        } else if accepts + remaining < self.threshold {
            Tally::Rejected {
                accepts,
                rejects,
                threshold: self.threshold,
            }
        } else {
            Tally::Pending {
                accepts,
                rejects,
                remaining,
                threshold: self.threshold,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(index: u8) -> Participant {
        Participant::new(index)
    }

    fn proposal() -> ProposalId {
        ProposalId {
            version: 7,
            hash: [0xab; 32],
        }
    }

    #[test]
    fn canonical_slots_make_delivery_order_irrelevant() {
        let eligible = vec![participant(2), participant(0), participant(1)];
        let mut first = Ballot::new(proposal(), eligible.clone(), 2).unwrap();
        let mut second = Ballot::new(proposal(), eligible, 2).unwrap();

        first.cast(participant(0), Vote::Accept).unwrap();
        first.cast(participant(2), Vote::Reject).unwrap();
        second.cast(participant(2), Vote::Reject).unwrap();
        second.cast(participant(0), Vote::Accept).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.next_voter(), Some(participant(1)));
        assert_eq!(
            first.cast(participant(1), Vote::Accept).unwrap(),
            Tally::Accepted {
                accepts: 2,
                rejects: 1,
                threshold: 2,
            }
        );
    }

    #[test]
    fn rejection_is_final_when_threshold_becomes_unreachable() {
        let mut ballot = Ballot::new(
            proposal(),
            vec![participant(0), participant(1), participant(2)],
            3,
        )
        .unwrap();

        assert_eq!(
            ballot.cast(participant(0), Vote::Reject).unwrap(),
            Tally::Rejected {
                accepts: 0,
                rejects: 1,
                threshold: 3,
            }
        );
        assert_eq!(
            ballot.cast(participant(1), Vote::Accept),
            Err(Error::Closed)
        );
    }

    #[test]
    fn invalid_construction_and_transitions_are_rejected() {
        assert_eq!(
            Ballot::new(proposal(), Vec::new(), 1),
            Err(Error::NoEligibleParticipants)
        );
        assert_eq!(
            Ballot::new(proposal(), vec![participant(0), participant(0)], 1),
            Err(Error::DuplicateParticipant(0))
        );
        assert_eq!(
            Ballot::new(proposal(), vec![participant(0)], 0),
            Err(Error::InvalidThreshold {
                threshold: 0,
                eligible: 1,
            })
        );

        let mut ballot = Ballot::new(proposal(), vec![participant(0)], 1).unwrap();
        assert_eq!(
            ballot.cast(participant(1), Vote::Accept),
            Err(Error::IneligibleParticipant(1))
        );
        ballot.cast(participant(0), Vote::Accept).unwrap();
        assert_eq!(
            ballot.cast(participant(0), Vote::Accept),
            Err(Error::Closed)
        );

        let mut open = Ballot::new(
            proposal(),
            vec![participant(0), participant(1), participant(2)],
            3,
        )
        .unwrap();
        open.cast(participant(0), Vote::Accept).unwrap();
        assert_eq!(
            open.cast(participant(0), Vote::Reject),
            Err(Error::DuplicateVote(0))
        );
        assert_eq!(
            Ballot::new(proposal(), vec![participant(0)], 2),
            Err(Error::InvalidThreshold {
                threshold: 2,
                eligible: 1,
            })
        );
        assert_eq!(
            Ballot::new(
                ProposalId {
                    version: 0,
                    hash: [0; 32],
                },
                vec![participant(0)],
                1,
            ),
            Err(Error::InvalidProposalVersion)
        );
    }

    #[test]
    fn borsh_vector_is_stable() {
        let mut ballot = Ballot::new(proposal(), vec![participant(2), participant(0)], 2).unwrap();
        ballot.cast(participant(0), Vote::Accept).unwrap();

        let encoded = borsh::to_vec(&ballot).unwrap();
        let mut expected = vec![1, 7, 0, 0, 0];
        expected.extend([0xab; 32]);
        expected.extend([2, 0, 0, 0, 0, 2]);
        expected.extend([2, 0, 0, 0, 1, 0, 0]);
        expected.extend([2, 0]);
        assert_eq!(encoded, expected);
    }
}
