//! Local confirmation of terminal evidence. This state never enters a receipt
//! or signed commitment; each participant owns its own confirmation set.

use super::{ExecutionState, ProtocolError, StopCause};
use crate::{ExecFrame, PeerId};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Progress of the local end handshake, independent of receipt publication.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum EndPhase {
    /// Execution has not reached a terminal conclusion.
    Open,
    /// Terminal evidence is being confirmed by remote participants.
    Ending { unconfirmed: BTreeSet<PeerId> },
    /// The confirmation window ended; outstanding peers may wake the actor.
    Ended { unconfirmed: BTreeSet<PeerId> },
}

impl EndPhase {
    /// Peers still needing confirmation; Open has no confirmation set.
    pub fn unconfirmed(&self) -> Option<&BTreeSet<PeerId>> {
        match self {
            Self::Open => None,
            Self::Ending { unconfirmed } | Self::Ended { unconfirmed } => Some(unconfirmed),
        }
    }
}

/// Relationship of authenticated peer evidence to the local terminal result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndMatch {
    /// The evidence reaches the same conclusion.
    Same,
    /// The evidence contradicts the local terminal conclusion.
    Different,
    /// This frame is ordinary traffic or a certificate for an earlier step.
    NotTerminalEvidence,
}

impl ExecutionState {
    /// Local end handshake, persisted alongside execution state.
    pub fn end_phase(&self) -> &EndPhase {
        &self.end_phase
    }

    pub(super) fn begin_end(&mut self) {
        if !self.status().is_terminal() || !matches!(self.end_phase, EndPhase::Open) {
            return;
        }
        let unconfirmed: BTreeSet<_> = self
            .binding()
            .activation()
            .tickets()
            .iter()
            .map(|ticket| ticket.data.signer)
            .filter(|peer| *peer != self.producer())
            .collect();
        self.end_phase = EndPhase::Ending { unconfirmed };
    }

    /// Confirm a remote participant. Duplicate confirmations do not change
    /// the version and must not be persisted as a new transition.
    pub fn confirm_end(&mut self, peer: PeerId) -> Result<bool, ProtocolError> {
        if peer == self.producer()
            || !self
                .binding()
                .activation()
                .tickets()
                .iter()
                .any(|t| t.data.signer == peer)
        {
            return Err(ProtocolError::InvalidTerminalStatus);
        }
        let mut next = self.clone();
        match &mut next.end_phase {
            EndPhase::Open => return Err(ProtocolError::InvalidTerminalStatus),
            EndPhase::Ending { unconfirmed } | EndPhase::Ended { unconfirmed } => {
                if !unconfirmed.remove(&peer) {
                    return Ok(false);
                }
                if unconfirmed.is_empty() {
                    next.end_phase = EndPhase::Ended {
                        unconfirmed: BTreeSet::new(),
                    };
                }
            }
        }
        next.bump_version()?;
        *self = next;
        Ok(true)
    }

    /// Close the current confirmation window, retaining every silent peer.
    pub fn expire_end(&mut self) -> Result<(), ProtocolError> {
        let EndPhase::Ending { unconfirmed } = &self.end_phase else {
            return Err(ProtocolError::InvalidTerminalStatus);
        };
        let mut next = self.clone();
        next.end_phase = EndPhase::Ended {
            unconfirmed: unconfirmed.clone(),
        };
        next.bump_version()?;
        *self = next;
        Ok(())
    }

    /// Evidence sent during ending: a certified final step, including shared
    /// abort/failure, or the authenticated unilateral occurrence we adopted.
    pub fn terminal_evidence(&self) -> Option<ExecFrame> {
        if !self.status().is_terminal() {
            return None;
        }
        if let Some(StopCause::Authenticated(occurrence)) = self.status().terminal_cause() {
            return Some(ExecFrame::Abort {
                occurrence: occurrence.clone(),
            });
        }
        self.last_certificate
            .clone()
            .map(|certificate| ExecFrame::StepCertificate { certificate })
    }

    /// Compare evidence after the caller has authenticated it against the
    /// session binding. Earlier certificates are ordinary stale traffic.
    pub fn end_conclusion_matches(&self, frame: &ExecFrame) -> EndMatch {
        let Some(ours) = self.terminal_evidence() else {
            return EndMatch::NotTerminalEvidence;
        };
        match (&ours, frame) {
            (
                ExecFrame::StepCertificate { certificate: ours },
                ExecFrame::StepCertificate { certificate },
            ) if ours.commitment() == certificate.commitment() => EndMatch::Same,
            (ExecFrame::Abort { .. }, ExecFrame::Abort { occurrence })
                if *occurrence.coordinate() == self.step_cursor() =>
            {
                EndMatch::Same
            }
            (_, ExecFrame::StepCertificate { certificate })
                if certificate.commitment().step
                    < match &ours {
                        ExecFrame::StepCertificate { certificate } => certificate.commitment().step,
                        _ => self.agreed_step(),
                    } =>
            {
                EndMatch::NotTerminalEvidence
            }
            (_, ExecFrame::StepCertificate { .. } | ExecFrame::Abort { .. }) => EndMatch::Different,
            _ => EndMatch::NotTerminalEvidence,
        }
    }

    pub(super) fn validate_end(&self) -> Result<(), ProtocolError> {
        match &self.end_phase {
            EndPhase::Open if !self.status().is_terminal() => Ok(()),
            EndPhase::Ending { unconfirmed } | EndPhase::Ended { unconfirmed }
                if self.status().is_terminal() =>
            {
                if matches!(self.end_phase, EndPhase::Ending { .. }) && unconfirmed.is_empty() {
                    return Err(ProtocolError::InvalidTerminalStatus);
                }
                if unconfirmed.iter().any(|peer| {
                    *peer == self.producer()
                        || !self
                            .binding()
                            .activation()
                            .tickets()
                            .iter()
                            .any(|t| t.data.signer == *peer)
                }) {
                    return Err(ProtocolError::InvalidTerminalStatus);
                }
                Ok(())
            }
            _ => Err(ProtocolError::InvalidTerminalStatus),
        }
    }
}
