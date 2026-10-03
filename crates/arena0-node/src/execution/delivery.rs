//! State-driven delivery. The actor authenticates and classifies frames from
//! its in-memory state; only a committed apply or a stale/duplicate decision
//! acknowledges a delivery. Restart resends the retained protocol evidence.

use std::collections::HashSet;
use std::time::Duration;

use arena0_protocol::{
    DirectArrival, DirectEntry, EndPhase, ExecFrame, ExecutionState, ExecutionStatus,
    ParticipantStepSignature, PeerId, PeerIdSource, ProtocolError,
};
use arena0_store::Change;
use arena0_transport::{ExecDelivery, ExecDeliveryRejection, SendHandle, TransportError};
use tokio::time::Instant;

use super::{ExecutionActor, PROGRESS_INTERVAL};
use crate::context::ExecError;

const SEND_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(super) struct SendLane {
    handle: Option<SendHandle>,
    acked: HashSet<[u8; 32]>,
    /// Refusals may suppress retries only while a frame is non-final. Keeping
    /// them separate prevents a later stop from counting a refusal as an ack.
    rejected: HashSet<[u8; 32]>,
    busy: bool,
    suppressed: bool,
    retry_at: Option<Instant>,
    /// Direct backoff never delays an eligible protocol frame.
    direct_retry_at: Option<Instant>,
}

pub(super) struct SendResult {
    peer: PeerId,
    sent: Sent,
    handle: Option<SendHandle>,
    result: Result<(), TransportError>,
}

pub(super) enum Sent {
    Protocol { digest: [u8; 32] },
    Direct { seq: u64 },
}

// Preparation failures retain the peer so settlement can release its send lane.
pub(super) type SendTaskResult = Result<SendResult, (PeerId, ExecError)>;

#[derive(Debug, PartialEq, Eq)]
enum Pick {
    Protocol(usize),
    Direct,
}

impl ExecutionActor {
    pub(super) async fn inbound(&mut self, delivery: ExecDelivery) -> Result<(), ExecError> {
        let source = delivery.source();
        let frame = delivery.frame().clone();
        let result = self.accept_frame(source, frame).await;
        match result {
            Ok(None) => {
                let _ = delivery.acknowledge();
            }
            Ok(Some(rejection)) => {
                let _ = delivery.reject(rejection);
            }
            Err(error @ ExecError::Diverged(_)) => {
                // A divergent dispatch is applied as a durable local failure.
                // If persistence fails, dropping the delivery keeps the sender
                // responsible for retrying it.
                if !self.fail_terminal(error.clone()).await {
                    return Err(error);
                }
                let _ = delivery.acknowledge();
            }
            Err(error) => return Err(error),
        }
        self.progress().await
    }

    pub(super) async fn accept_frame(
        &mut self,
        source: PeerId,
        frame: ExecFrame,
    ) -> Result<Option<ExecDeliveryRejection>, ExecError> {
        use ExecDeliveryRejection::{Conflict, NotYet, Rejected};
        if !authenticates(&self.state, source, &frame) {
            return Ok(Some(Rejected));
        }
        if self.state.status().is_terminal() {
            match self.state.end_conclusion_matches(&frame) {
                arena0_protocol::EndMatch::Same => self.confirm_peer(source).await?,
                arena0_protocol::EndMatch::Different => {
                    tracing::error!(exec_id = %self.context.exec_id, peer = %source, "peer terminal conclusion differs");
                    self.send_lanes.entry(source).or_default().suppressed = true;
                    return Ok(Some(Conflict));
                }
                arena0_protocol::EndMatch::NotTerminalEvidence => {}
            }
            return Ok(None);
        }
        let step = self.state.agreed_step();
        match frame {
            ExecFrame::Message { commitment, data } => {
                if commitment.step < step {
                    return Ok(None);
                }
                if commitment.step > step {
                    return Ok(Some(NotYet));
                }
                if commitment.session_id != self.state.binding().session_id()
                    || commitment.pre_state != self.state.agreed_state()
                    || commitment.link != self.state.agreed_link()
                {
                    return Ok(Some(Rejected));
                }
                if let Some(proposal) = self.state.pending_shared() {
                    // Derive once per frame; the commitment is a pure
                    // function of the staged entry and the agreed link.
                    let staged = self.state.proposal_commitment();
                    return Ok(
                        if staged.as_ref() == Some(&commitment)
                            && matches!(&proposal.entry().event,
                                arena0_protocol::StepEvent::Message { from, data: staged }
                                    if *from == source && *staged == data)
                        {
                            None
                        } else {
                            Some(Conflict)
                        },
                    );
                }
                self.apply_message(source, commitment, data).await?;
            }
            ExecFrame::StepSignature {
                commitment,
                signature,
            } => {
                if commitment.step < step {
                    return Ok(None);
                }
                if commitment.step > step {
                    return Ok(Some(NotYet));
                }
                let Some(proposal) = self.state.pending_shared() else {
                    return Ok(Some(NotYet));
                };
                if self.state.proposal_commitment().as_ref() != Some(&commitment) {
                    return Ok(Some(Rejected));
                }
                let signature = ParticipantStepSignature::new(source, commitment.step, signature);
                if proposal.signatures().contains(&signature) {
                    return Ok(None);
                }
                let mut next = self.state.clone();
                let Ok(certified) = next.add_step_signature(signature) else {
                    return Ok(Some(Rejected));
                };
                self.persist_step_signature(next, certified).await?;
            }
            ExecFrame::StepCertificate { certificate } => {
                if certificate.commitment().step < step {
                    return Ok(None);
                }
                if self.state.pending_shared().is_none() {
                    return Ok(Some(NotYet));
                };
                if self.state.proposal_commitment().as_ref() != Some(certificate.commitment()) {
                    return Err(ExecError::DeliveryInvariant(
                        "verified certificate contradicts the staged proposal",
                    ));
                }
                let mut next = self.state.clone();
                let certified = next.certify_step(certificate).map_err(|_| {
                    ExecError::DeliveryInvariant("verified certificate could not be committed")
                })?;
                self.persist_step_signature(next, Some(certified)).await?;
            }
            ExecFrame::Direct {
                seq,
                msg,
                attachment,
            } => {
                if !matches!(self.state.end_phase(), EndPhase::Open) {
                    return Ok(None);
                }
                match self.state.direct_arrival(source, seq) {
                    DirectArrival::Duplicate => return Ok(None),
                    DirectArrival::Gap => return Ok(Some(Rejected)),
                    DirectArrival::Next => {}
                }
                if self.state.pending_shared().is_some()
                    || !matches!(self.state.status(), ExecutionStatus::Active)
                {
                    return Ok(Some(NotYet));
                }
                return match self.dispatch_direct(source, seq, msg, attachment).await? {
                    super::guest::DispatchOutcome::Committed
                    | super::guest::DispatchOutcome::Rejected { .. } => Ok(None),
                    super::guest::DispatchOutcome::Frozen => Ok(Some(NotYet)),
                };
            }
            ExecFrame::Abort { occurrence } => {
                if occurrence.coordinate().next_step() < step {
                    return Ok(None);
                }
                if occurrence.coordinate().next_step() > step {
                    return Ok(Some(NotYet));
                }
                let mut next = self.state.clone();
                match next.stop(occurrence) {
                    Ok(()) => {}
                    Err(ProtocolError::SharedProposalSigned) => return Ok(Some(NotYet)),
                    Err(ProtocolError::InvalidAbortCoordinate) => {
                        tracing::warn!(exec_id = %self.context.exec_id, peer = %source, "authenticated abort contradicts the agreed cursor");
                        return Ok(Some(Conflict));
                    }
                    Err(_) => return Ok(Some(Rejected)),
                }
                self.persist(next, Change::State).await?;
            }
        }
        Ok(None)
    }

    /// Start at most one send per peer. Opening a stream and waiting for its
    /// acknowledgement share a deadline. JoinSet owns cancellation when the
    /// actor exits, and each task owns its stream until settlement.
    pub(super) fn deliver_frames(&mut self) -> Result<(), ExecError> {
        let producer = self.context.identity.peer_id();
        let ending = self.state.status().is_terminal();
        let frames = if ending {
            self.state.terminal_evidence().into_iter().collect()
        } else {
            self.state.current_frames(producer)
        };
        let frames = frames
            .into_iter()
            .map(|frame| Ok((frame_digest(&frame)?, frame)))
            .collect::<Result<Vec<_>, ExecError>>()?;
        for ticket in self.context.activation.tickets() {
            let peer = ticket.data.signer;
            if peer == producer {
                continue;
            }
            if ending
                && !self
                    .state
                    .end_phase()
                    .unconfirmed()
                    .is_some_and(|peers| peers.contains(&peer))
            {
                continue;
            }
            let lane = self.send_lanes.entry(peer).or_default();
            lane.acked
                .retain(|digest| frames.iter().any(|(current, _)| current == digest));
            lane.rejected
                .retain(|digest| frames.iter().any(|(current, _)| current == digest));
            if lane.suppressed || lane.busy {
                continue;
            }
            let direct_head = self.state.direct_queue(peer).first();
            let Some(pick) = lane.next(&frames, direct_head, ending, Instant::now()) else {
                continue;
            };
            let (sent, frame, entry) = match pick {
                Pick::Protocol(index) => (
                    Sent::Protocol {
                        digest: frames[index].0,
                    },
                    Some(frames[index].1.clone()),
                    None,
                ),
                Pick::Direct => {
                    let entry = direct_head.expect("direct pick requires a head").clone();
                    (Sent::Direct { seq: entry.seq }, None, Some(entry))
                }
            };
            lane.busy = true;
            let handle = lane.handle.take();
            let transport = self.context.transport.clone();
            let session = self.context.activation.session_hash();
            let store = self.context.blob_store.clone();
            self.send_tasks.spawn(async move {
                let frame = match frame {
                    Some(frame) => frame,
                    None => {
                        let entry = entry.expect("direct send owns its queue entry");
                        let seq = entry.seq;
                        let prepared = async {
                            // A source the Host can no longer read (file gone
                            // or shorter than the range) still sends the
                            // frame, without its attachment: the receiver's
                            // program sees none and settles the transfer.
                            let attachment = match entry.range {
                                Some(range) => tokio::task::spawn_blocking(move || {
                                    store.read_blob_range_blocking(
                                        range.hash,
                                        range.start..range.end,
                                    )
                                })
                                .await
                                .map_err(|_| {
                                    ExecError::DeliveryInvariant("blob read task failed")
                                })??,
                                None => None,
                            };
                            Ok::<_, ExecError>(ExecFrame::Direct {
                                seq,
                                msg: entry.msg,
                                attachment,
                            })
                        }
                        .await;
                        prepared.map_err(|error| (peer, error))?
                    }
                };
                // The task owns the attachment bytes only until send settlement.
                let operation = async move {
                    let handle = match handle {
                        Some(handle) => handle,
                        None => transport.open_exec(&peer, session).await?,
                    };
                    let result = handle.send_exec(&frame).await;
                    Ok::<_, TransportError>((handle, result))
                };
                let (handle, result) = match tokio::time::timeout(SEND_DEADLINE, operation).await {
                    Ok(Ok((handle, result))) => (Some(handle), result),
                    Ok(Err(error)) => (None, Err(error)),
                    Err(_) => (None, Err(TransportError::Timeout(5000))),
                };
                Ok(SendResult {
                    peer,
                    sent,
                    handle,
                    result,
                })
            });
        }
        Ok(())
    }

    async fn confirm_peer(&mut self, peer: PeerId) -> Result<(), ExecError> {
        let mut next = self.state.clone();
        if next.confirm_end(peer)? {
            self.persist(next, Change::State).await?;
        }
        Ok(())
    }

    pub(super) async fn settle_send(&mut self, result: SendTaskResult) -> Result<(), ExecError> {
        let result = match result {
            Ok(result) => result,
            Err((peer, error)) => {
                self.send_lanes.entry(peer).or_default().busy = false;
                return Err(error);
            }
        };
        let SendResult {
            peer,
            sent,
            handle,
            result,
        } = result;
        let digest = match sent {
            Sent::Protocol { digest } => digest,
            Sent::Direct { seq } => {
                let lane = self.send_lanes.entry(peer).or_default();
                lane.busy = false;
                lane.direct_retry_at = None;
                match result {
                    Ok(()) => lane.handle = handle,
                    Err(TransportError::ExecRejected | TransportError::ExecConflict) => {
                        tracing::warn!(exec_id = %self.context.exec_id, %peer, seq,
                            "peer rejected direct frame; dropping entry");
                        lane.handle = handle;
                    }
                    Err(TransportError::ExecNotYet) => {
                        lane.handle = handle;
                        lane.direct_retry_at = Some(Instant::now() + PROGRESS_INTERVAL);
                        return Ok(());
                    }
                    Err(_) => {
                        lane.direct_retry_at = Some(Instant::now() + PROGRESS_INTERVAL);
                        return Ok(());
                    }
                }
                let mut next = self.state.clone();
                if next.ack_direct(peer, seq)? {
                    self.persist(next, Change::State).await?;
                }
                return Ok(());
            }
        };
        let final_frame = self
            .state
            .terminal_evidence()
            .as_ref()
            .is_some_and(|frame| frame_digest(frame).is_ok_and(|current| current == digest));
        let lane = self.send_lanes.entry(peer).or_default();
        lane.busy = false;
        lane.retry_at = None;
        match result {
            Ok(()) => {
                lane.acked.insert(digest);
                lane.handle = handle;
                if final_frame {
                    self.confirm_peer(peer).await?;
                }
            }
            Err(TransportError::ExecRejected) => {
                if final_frame {
                    tracing::error!(exec_id = %self.context.exec_id, %peer, "peer rejected terminal evidence");
                    lane.suppressed = true;
                    return Ok(());
                }
                tracing::warn!(exec_id = %self.context.exec_id, %peer, "execution frame rejected");
                lane.rejected.insert(digest);
                lane.handle = handle;
            }
            Err(TransportError::ExecConflict) => {
                // A receipt is not evidence of divergence. Only verified
                // frames on the receive path can change our conclusion.
                tracing::error!(exec_id = %self.context.exec_id, %peer, "peer reported conflicting execution evidence");
                lane.suppressed = true;
            }
            Err(TransportError::ExecNotYet) => {
                lane.handle = handle;
                lane.retry_at = Some(Instant::now() + PROGRESS_INTERVAL);
            }
            Err(_) => {
                lane.retry_at = Some(Instant::now() + PROGRESS_INTERVAL);
            }
        }
        Ok(())
    }
}

/// Authenticate a frame against the session binding before any comparison
/// with local state. Live actors and the retired-session router share this
/// one policy. Adopted aborts retain their original signer, independently of
/// the forwarding peer.
pub(crate) fn authenticates(state: &ExecutionState, source: PeerId, frame: &ExecFrame) -> bool {
    let binding = state.binding();
    if !binding.is_participant(source) {
        return false;
    }
    match frame {
        ExecFrame::StepCertificate { certificate } => certificate.verify(binding).is_ok(),
        ExecFrame::Abort { occurrence } => {
            occurrence.session_id() == binding.session_id()
                && binding.is_participant(occurrence.sender())
                && occurrence.verify_signature().unwrap_or(false)
        }
        ExecFrame::StepSignature { commitment, .. } => {
            commitment.session_id == binding.session_id()
        }
        ExecFrame::Message { .. } | ExecFrame::Direct { .. } => true,
    }
}

fn frame_digest(frame: &ExecFrame) -> Result<[u8; 32], ExecError> {
    let bytes = borsh::to_vec(frame).map_err(|error| ExecError::InvalidState(error.to_string()))?;
    Ok(*blake3::hash(&bytes).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames() -> Vec<([u8; 32], ExecFrame)> {
        vec![(
            [1; 32],
            ExecFrame::Message {
                commitment: arena0_protocol::StepCommitment {
                    domain: arena0_protocol::STEP_COMMIT_DOMAIN,
                    session_id: arena0_protocol::SessionHash([2; 32]),
                    step: 1,
                    entry_hash: [3; 32],
                    pre_state: arena0_protocol::StateHash([4; 32]),
                    post_state: arena0_protocol::StateHash([5; 32]),
                    link: [6; 32],
                },
                data: vec![],
            },
        )]
    }

    fn head() -> DirectEntry {
        DirectEntry {
            seq: 1,
            msg: vec![7],
            range: None,
        }
    }

    #[test]
    fn protocol_frames_preempt_direct_frames() {
        assert_eq!(
            SendLane::default().next(&frames(), Some(&head()), false, Instant::now()),
            Some(Pick::Protocol(0))
        );
    }

    #[test]
    fn direct_retry_delays_only_direct_frames() {
        let now = Instant::now();
        let mut lane = SendLane {
            direct_retry_at: Some(now + PROGRESS_INTERVAL),
            ..Default::default()
        };
        assert_eq!(
            lane.next(&frames(), Some(&head()), false, now),
            Some(Pick::Protocol(0))
        );
        assert_eq!(lane.next(&[], Some(&head()), false, now), None);
        assert_eq!(
            lane.next(&[], Some(&head()), false, now + PROGRESS_INTERVAL),
            Some(Pick::Direct)
        );
        lane.direct_retry_at = None;
        lane.retry_at = Some(now + PROGRESS_INTERVAL);
        assert_eq!(
            lane.next(&frames(), Some(&head()), false, now),
            Some(Pick::Direct)
        );
    }

    #[test]
    fn no_direct_frames_while_ending() {
        assert_eq!(
            SendLane::default().next(&[], Some(&head()), true, Instant::now()),
            None
        );
    }

    #[test]
    fn direct_head_goes_when_no_protocol_frame_is_eligible() {
        let mut lane = SendLane::default();
        lane.acked.insert([1; 32]);
        assert_eq!(
            lane.next(&frames(), Some(&head()), false, Instant::now()),
            Some(Pick::Direct)
        );
        lane.acked.clear();
        lane.rejected.insert([1; 32]);
        assert_eq!(
            lane.next(&frames(), Some(&head()), false, Instant::now()),
            Some(Pick::Direct)
        );
        assert_eq!(lane.next(&frames(), None, false, Instant::now()), None);
    }
}

impl SendLane {
    /// Protocol evidence preempts direct traffic; each traffic class keeps its own retry clock, and ending suppresses direct delivery.
    fn next(
        &self,
        frames: &[([u8; 32], ExecFrame)],
        direct_head: Option<&DirectEntry>,
        ending: bool,
        now: Instant,
    ) -> Option<Pick> {
        let _ = (frames, direct_head, ending, now);
        todo!("STUB(transport)")
    }
}
