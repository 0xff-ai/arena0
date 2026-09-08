//! Protocol-only validation of one complete authenticated artifact.

use arena0_program::ProgramHash;
use arena0_protocol::{
    AbortKind, Activation, AggregateAttestation, CHAIN_START, Committed, Ensemble,
    MAX_PARTICIPANTS, MessageId, OutcomeHash, PeerId, PublicCursor, PublicEffect, PublicEvent,
    ReceiptArtifact, ReceiptBody, ReceiptTermination, SessionHash, StepCommitment, StopCause,
    TRACE_FORMAT_VERSION, TerminalCommitment, TicketAction,
};

use crate::error::{VerifyError, sanitize_verify_message};

/// The result of protocol-only receipt verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LightVerified {
    /// The content-addressed program named by the activation.
    pub program_id: ProgramHash,
    /// The activation-derived session identity.
    pub session_id: SessionHash,
    /// The canonical participant order used by signer bitmaps.
    pub ensemble: Vec<PeerId>,
    /// Number of contiguous public entries.
    pub steps: u64,
    /// The required successful terminal certificate.
    pub terminal: LightVerifiedTerminal,
}

/// The terminal proof evidence accepted by light verification.
///
/// Light verification authenticates the opaque Borsh outcome but cannot run the
/// guest to produce its JSON projection. The full verifier converts this type
/// into its replay terminal type only after replay. A stopped result never
/// carries an outcome field, so callers cannot mistake an empty byte vector for
/// a stopped proof.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LightVerifiedTerminal {
    /// The final public entry carries one successful `SessionEnd` effect.
    Completed {
        /// Opaque stock-Borsh outcome bytes authenticated by the terminal.
        outcome_borsh: Vec<u8>,
    },
    /// The execution stopped at exactly the authenticated or shared boundary
    /// carried by the receipt. [`StopCause::Authenticated`] preserves unilateral
    /// evidence; [`StopCause::Shared`] preserves the N-of-N step commitment.
    Stopped { cause: StopCause },
}

/// Verify a bounded encoded [`ReceiptArtifact`] without loading Wasm or a
/// sandbox.
pub fn verify_light(receipt_bytes: &[u8]) -> Result<LightVerified, VerifyError> {
    let receipt = decode_receipt(receipt_bytes)?;
    verify_decoded(&receipt)
}

pub(crate) fn decode_receipt(receipt_bytes: &[u8]) -> Result<ReceiptArtifact, VerifyError> {
    if receipt_bytes.len() > arena0_protocol::MAX_RECEIPT_BYTES {
        return Err(VerifyError::ReceiptTooLarge {
            actual: receipt_bytes.len(),
            max: arena0_protocol::MAX_RECEIPT_BYTES,
        });
    }
    ReceiptArtifact::decode(receipt_bytes)
        .map_err(|error| VerifyError::ReceiptDecode(error.to_string()))
}

/// Verify a decoded receipt. The public entrypoint performs bounded decoding;
/// replay uses this helper after it has decoded the same value.
pub(crate) fn verify_decoded(receipt: &ReceiptArtifact) -> Result<LightVerified, VerifyError> {
    let body = receipt.body();
    let header = body.header();
    let activation = &header.activation;

    activation
        .validate()
        .map_err(|error| VerifyError::ReceiptInvalid(format!("activation: {error}")))?;

    if body.params() != activation.offer().data().params.as_bytes() {
        return Err(VerifyError::ParamsMismatch);
    }

    let (ensemble, participant_keys) = participant_set(activation)?;
    let session_id = activation.session_hash();
    let terminal = &header.terminal;
    verify_trace(body, session_id, &ensemble, &participant_keys, terminal)?;

    let terminal = match terminal {
        ReceiptTermination::Completed { .. } => LightVerifiedTerminal::Completed {
            outcome_borsh: body.outcome().to_vec(),
        },
        ReceiptTermination::Stopped { cause } => LightVerifiedTerminal::Stopped {
            cause: cause.clone(),
        },
    };
    Ok(LightVerified {
        program_id: activation.offer().data().program_hash,
        session_id,
        ensemble,
        steps: u64::try_from(body.trace().len()).map_err(|_| {
            VerifyError::ReceiptInvalid("trace length does not fit in u64".to_owned())
        })?,
        terminal,
    })
}

fn participant_set(
    activation: &Activation,
) -> Result<(Vec<PeerId>, Vec<arena0_crypto::BlsPublicKey>), VerifyError> {
    if activation.tickets().len() > MAX_PARTICIPANTS {
        return Err(VerifyError::ReceiptInvalid(
            "activation has too many tickets".to_owned(),
        ));
    }
    let mut participants = Vec::with_capacity(activation.tickets().len());
    for ticket in activation.tickets() {
        match ticket.data.action {
            TicketAction::Active { execution_bls, .. } => {
                participants.push((ticket.data.signer, execution_bls));
            }
            TicketAction::Withdrawn => {
                return Err(VerifyError::ReceiptInvalid(
                    "activation contains a withdrawn ticket".to_owned(),
                ));
            }
        }
    }
    participants.sort_by_key(|(peer, _)| *peer);
    let peers = participants
        .iter()
        .map(|(peer, _)| *peer)
        .collect::<Vec<_>>();
    let ensemble = Ensemble::<Committed>::from_peers(peers.clone())
        .map_err(|error| VerifyError::ReceiptInvalid(format!("ensemble: {error}")))?;
    let keys = participants.into_iter().map(|(_, key)| key).collect();
    Ok((ensemble.peers().to_vec(), keys))
}

fn verify_trace(
    body: &ReceiptBody,
    session_id: SessionHash,
    ensemble: &[PeerId],
    participant_keys: &[arena0_crypto::BlsPublicKey],
    termination: &ReceiptTermination,
) -> Result<(), VerifyError> {
    let trace = body.trace();
    let allow_empty = matches!(
        termination,
        ReceiptTermination::Stopped {
            cause: arena0_protocol::StopCause::Authenticated(_)
        }
    );
    if trace.is_empty() && !allow_empty {
        return Err(VerifyError::EmptyTrace);
    }

    let expected_initial = body.header().activation.offer().data().initial_state;
    let mut previous_state = expected_initial;
    let mut previous_link = CHAIN_START;
    let trace_len = u64::try_from(trace.len())
        .map_err(|_| VerifyError::ReceiptInvalid("trace length does not fit in u64".to_owned()))?;
    for (index, entry) in trace.iter().enumerate() {
        let step = u64::try_from(index)
            .map_err(|_| VerifyError::ReceiptInvalid("trace index overflow".to_owned()))?;
        if entry.trace_version != TRACE_FORMAT_VERSION {
            return Err(VerifyError::PublicEntryInvalid {
                step,
                message: format!(
                    "trace version {} is not {TRACE_FORMAT_VERSION}",
                    entry.trace_version
                ),
            });
        }
        if entry.step != step {
            return Err(VerifyError::ChainBroken {
                step,
                message: format!("entry step is {}, expected {step}", entry.step),
            });
        }
        if entry.pre_state != previous_state {
            return Err(VerifyError::ChainBroken {
                step,
                message: "entry pre-state does not equal the preceding post-state".to_owned(),
            });
        }

        verify_event(entry, step, session_id, ensemble)?;
        verify_terminal_position(entry, step, trace_len, termination)?;
        let commitment = StepCommitment::for_entry(session_id, entry, previous_link);
        verify_agreement(&entry.agreement, participant_keys, &commitment)?;

        previous_state = entry.post_state;
        previous_link = commitment.link_hash();
    }

    let final_entry = trace.last();
    let cursor = PublicCursor::new(trace_len, previous_state, previous_link);
    match termination {
        ReceiptTermination::Completed { terminal } => {
            let final_entry = final_entry.ok_or(VerifyError::EmptyTrace)?;
            if terminal.final_step != final_entry.step {
                return Err(VerifyError::TerminalMismatch {
                    field: "final_step",
                });
            }
            if terminal.final_state != final_entry.post_state {
                return Err(VerifyError::TerminalMismatch {
                    field: "final_state",
                });
            }
            let outcome = final_entry
                .completed_outcome()
                .ok_or(VerifyError::OutcomeMissing)?;
            if outcome != body.outcome() {
                return Err(VerifyError::TerminalMismatch { field: "outcome" });
            }
            if terminal.outcome_hash != OutcomeHash::of(body.outcome()) {
                return Err(VerifyError::OutcomeHashMismatch);
            }
            verify_terminal_agreement(terminal, session_id, participant_keys)
        }
        ReceiptTermination::Stopped { cause } => {
            if !body.outcome().is_empty() {
                return Err(VerifyError::TerminalMismatch { field: "outcome" });
            }
            match cause {
                arena0_protocol::StopCause::Authenticated(occurrence) => {
                    occurrence
                        .validate_for_session(session_id)
                        .map_err(|error| VerifyError::ReceiptInvalid(error.to_string()))?;
                    if !ensemble.contains(&occurrence.sender())
                        || !occurrence
                            .verify_signature()
                            .map_err(|error| VerifyError::ReceiptInvalid(error.to_string()))?
                        || *occurrence.coordinate() != cursor
                    {
                        return Err(VerifyError::ReceiptInvalid(
                            "authenticated stop cause does not match the public cursor".to_owned(),
                        ));
                    }
                    if let Some(final_entry) = final_entry
                        && final_entry.effects.iter().any(|effect| {
                            matches!(
                                effect,
                                PublicEffect::SessionEnd { .. }
                                    | PublicEffect::SessionAbort { .. }
                                    | PublicEffect::Fail { .. }
                            )
                        })
                    {
                        return Err(VerifyError::TerminalNotLast {
                            step: final_entry.step,
                        });
                    }
                }
                arena0_protocol::StopCause::Shared {
                    kind,
                    commitment,
                    reason,
                } => {
                    let final_entry = final_entry.ok_or(VerifyError::EmptyTrace)?;
                    if final_entry.step != commitment.step
                        || final_entry.entry_hash() != commitment.entry_hash
                        || commitment.session_id != session_id
                        || commitment.post_state != cursor.state_hash()
                        || commitment.link_hash() != cursor.chain_hash()
                    {
                        return Err(VerifyError::TerminalMismatch { field: "stop" });
                    }
                    let valid = match (kind, final_entry.effects.as_slice()) {
                        (AbortKind::Abort, [PublicEffect::SessionAbort { reason: actual }])
                        | (AbortKind::Fail, [PublicEffect::Fail { reason: actual }]) => {
                            actual == reason
                        }
                        _ => false,
                    };
                    if !valid {
                        return Err(VerifyError::OutcomeMissing);
                    }
                }
            }
            Ok(())
        }
    }
}

fn verify_event(
    entry: &arena0_protocol::TraceEntry,
    step: u64,
    session_id: SessionHash,
    ensemble: &[PeerId],
) -> Result<(), VerifyError> {
    match (&entry.event, step) {
        (
            PublicEvent::SessionStarted {
                ensemble: event_ensemble,
            },
            0,
        ) => {
            if event_ensemble.peers() != ensemble {
                return Err(VerifyError::PublicEntryInvalid {
                    step,
                    message: "SessionStarted ensemble does not match activation".to_owned(),
                });
            }
            if entry.witness.is_some() {
                return Err(VerifyError::PublicEntryInvalid {
                    step,
                    message: "SessionStarted carries a witness".to_owned(),
                });
            }
        }
        (PublicEvent::SessionStarted { .. }, _) => {
            return Err(VerifyError::PublicEntryInvalid {
                step,
                message: "SessionStarted is only valid at public position zero".to_owned(),
            });
        }
        (PublicEvent::MessageReceived { .. }, 0) => {
            return Err(VerifyError::PublicEntryInvalid {
                step,
                message: "first public entry must be SessionStarted".to_owned(),
            });
        }
        (
            PublicEvent::MessageReceived {
                message_id,
                from,
                position,
                pre_state,
                msg,
            },
            _,
        ) => {
            let Some(witness) = entry.witness else {
                return Err(VerifyError::PublicEntryInvalid {
                    step,
                    message: "message has no witness commitment".to_owned(),
                });
            };
            if !ensemble.contains(from) {
                return Err(VerifyError::PublicEntryInvalid {
                    step,
                    message: "message sender is not a committed participant".to_owned(),
                });
            }
            if *position != step {
                return Err(VerifyError::PublicEntryInvalid {
                    step,
                    message: format!("message position {position} does not equal {step}"),
                });
            }
            if *pre_state != entry.pre_state {
                return Err(VerifyError::PublicEntryInvalid {
                    step,
                    message: "message pre-state does not equal entry pre-state".to_owned(),
                });
            }
            let expected =
                MessageId::derive(session_id, *from, *position, *pre_state, msg, witness);
            if *message_id != expected {
                return Err(VerifyError::PublicEntryInvalid {
                    step,
                    message: "message id does not match its authenticated envelope".to_owned(),
                });
            }
        }
    }
    Ok(())
}

fn verify_terminal_position(
    entry: &arena0_protocol::TraceEntry,
    step: u64,
    trace_len: u64,
    termination: &ReceiptTermination,
) -> Result<(), VerifyError> {
    let next_step = step
        .checked_add(1)
        .ok_or_else(|| VerifyError::ReceiptInvalid("trace step overflow".to_owned()))?;
    let terminal_count = entry
        .effects
        .iter()
        .filter(|effect| {
            matches!(
                effect,
                PublicEffect::SessionEnd { .. }
                    | PublicEffect::SessionAbort { .. }
                    | PublicEffect::Fail { .. }
            )
        })
        .count();
    if terminal_count > 0 && next_step != trace_len {
        return Err(VerifyError::TerminalNotLast { step });
    }
    if next_step == trace_len {
        match termination {
            ReceiptTermination::Completed { .. } => {
                if terminal_count != 1
                    || !matches!(entry.effects.as_slice(), [PublicEffect::SessionEnd { .. }])
                {
                    return Err(VerifyError::OutcomeMissing);
                }
            }
            ReceiptTermination::Stopped { cause } => match cause {
                arena0_protocol::StopCause::Authenticated(_) => {
                    if terminal_count != 0 {
                        return Err(VerifyError::TerminalNotLast { step });
                    }
                }
                arena0_protocol::StopCause::Shared { kind, .. } => {
                    let valid = matches!(
                        (kind, entry.effects.as_slice()),
                        (AbortKind::Abort, [PublicEffect::SessionAbort { .. }])
                            | (AbortKind::Fail, [PublicEffect::Fail { .. }])
                    );
                    if !valid {
                        return Err(VerifyError::OutcomeMissing);
                    }
                }
            },
        }
    } else if terminal_count != 0 {
        return Err(VerifyError::TerminalNotLast { step });
    }
    Ok(())
}

fn verify_agreement(
    agreement: &AggregateAttestation,
    participant_keys: &[arena0_crypto::BlsPublicKey],
    commitment: &StepCommitment,
) -> Result<(), VerifyError> {
    let step = commitment.step;
    if !agreement.signers.is_full(participant_keys.len()) {
        return Err(VerifyError::MissingParticipantAgreement { step });
    }
    agreement
        .verify_signatures(step, &commitment.signing_bytes(), participant_keys)
        .map_err(|error| VerifyError::Agreement {
            step,
            message: sanitize_verify_message(error.to_string()),
        })
}

fn verify_terminal_agreement(
    terminal: &arena0_protocol::SessionTerminal,
    session_id: SessionHash,
    participant_keys: &[arena0_crypto::BlsPublicKey],
) -> Result<(), VerifyError> {
    if !terminal.agreement.signers.is_full(participant_keys.len()) {
        return Err(VerifyError::MissingParticipantAgreement {
            step: terminal.final_step,
        });
    }
    let commitment = TerminalCommitment::new(
        session_id,
        terminal.final_step,
        terminal.final_state,
        terminal.outcome_hash,
    );
    terminal
        .agreement
        .verify_signatures(
            terminal.final_step,
            &commitment.signing_bytes(),
            participant_keys,
        )
        .map_err(|error| VerifyError::Agreement {
            step: terminal.final_step,
            message: sanitize_verify_message(error.to_string()),
        })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use arena0_crypto::{BlsSignature, ExecutionKey, ExecutionSalt, NodeKeys, SecretKey};
    use arena0_program::ExecutionProfile;
    use arena0_protocol::{
        Activation, AggregateAttestation, Offer, OfferData, PreparedActivation, SessionHeader,
        SessionTerminal, SignerSet, StateHash, Ticket, TicketAction, TicketData, TraceEntry,
    };

    fn fixture() -> Vec<u8> {
        fixture_with_binding(ProgramHash([0x44; 32]), StateHash([0x11; 32]), false)
            .expect("valid fixture")
    }

    #[cfg(feature = "replay")]
    pub(crate) fn fixture_for_replay(
        program_hash: ProgramHash,
        initial_state: StateHash,
    ) -> Vec<u8> {
        fixture_with_binding(program_hash, initial_state, true).expect("valid replay fixture")
    }

    fn fixture_with_binding(
        program_hash: ProgramHash,
        initial_state: StateHash,
        two_steps: bool,
    ) -> Result<Vec<u8>, arena0_protocol::ProtocolError> {
        let negotiation = arena0_protocol::NegotiationId([9; 32]);
        let keys = [
            NodeKeys::from_secret(SecretKey::from_bytes([1; 32])),
            NodeKeys::from_secret(SecretKey::from_bytes([2; 32])),
        ];
        let mut participants = keys
            .iter()
            .enumerate()
            .map(|(index, identity)| {
                let peer = PeerId(identity.ed25519_public_key().0);
                let execution = ExecutionKey::derive(
                    &ExecutionSalt::try_from_bytes([index as u8 + 10; 32])
                        .expect("non-zero test salt"),
                    &[index as u8 + 20; 32],
                    &negotiation.0,
                )
                .expect("fixture execution key");
                (peer, identity, execution)
            })
            .collect::<Vec<_>>();
        participants.sort_by_key(|(peer, _, _)| *peer);
        let creator = participants[0].0;
        let params = br#"{"seed":1}"#.to_vec();
        let profile = ExecutionProfile::current().hash();
        let data = OfferData::new(
            negotiation,
            0,
            creator,
            program_hash,
            profile,
            arena0_program::JsonBytes::try_new(params.clone()).expect("fixture JSON params"),
            2,
            initial_state,
            1,
        )
        .expect("fixture offer");
        let offer_hash = arena0_protocol::OfferHash::of(&data);
        let mut tickets = Vec::new();
        for (peer, identity, execution) in &participants {
            let action = TicketAction::Active {
                execution_bls: execution.public_key(),
                key_binding: execution.key_binding(&offer_hash.0, &peer.0),
                issued_at_unix_ms: 0,
                valid_for_ms: 1,
            };
            let ticket_data =
                TicketData::new(negotiation, 0, *peer, 0, action).expect("fixture ticket data");
            let signature = identity.sign(&ticket_data.signing_bytes());
            tickets.push(Ticket {
                data: ticket_data,
                signature,
            });
        }
        let ticket_hashes = tickets
            .iter()
            .map(|ticket| arena0_protocol::TicketHash::of(&ticket.data))
            .collect::<Vec<_>>();
        let offer = Offer::new(data, ticket_hashes).expect("fixture offer");
        let prepared = PreparedActivation::new(offer, tickets).expect("fixture preparation");
        let activation_signatures = participants
            .iter()
            .map(|(_, _, execution)| execution.sign(&prepared.activation_data().signing_bytes()))
            .collect::<Vec<_>>();
        let activation = Activation::new(
            prepared,
            BlsSignature::aggregate(&activation_signatures).expect("aggregate"),
        )
        .expect("fixture activation");
        let session = activation.session_hash();
        let ensemble = Ensemble::<Committed>::from_peers(
            participants.iter().map(|(peer, _, _)| *peer).collect(),
        )
        .expect("fixture ensemble");
        let outcome = vec![7, 8, 9];
        let first_post_state = StateHash::of(&[1]);
        let final_state = if two_steps {
            StateHash::of(&[2])
        } else {
            first_post_state
        };
        let mut entries = Vec::new();
        let mut first = TraceEntry {
            trace_version: TRACE_FORMAT_VERSION,
            step: 0,
            event: PublicEvent::SessionStarted {
                ensemble: ensemble.clone(),
            },
            effects: if two_steps {
                Vec::new()
            } else {
                vec![PublicEffect::SessionEnd {
                    outcome: outcome.clone(),
                }]
            },
            pre_state: initial_state,
            post_state: first_post_state,
            fuel_used: 3,
            witness: None,
            agreement: AggregateAttestation::empty(),
        };
        let first_commitment = StepCommitment::for_entry(session, &first, CHAIN_START);
        let step_signatures = participants
            .iter()
            .map(|(_, _, execution)| execution.sign(&first_commitment.signing_bytes()))
            .collect::<Vec<_>>();
        first.agreement = AggregateAttestation::from_signatures(
            SignerSet::full(participants.len()).expect("full signer set"),
            &step_signatures,
        )
        .expect("step aggregate");
        entries.push(first);
        if two_steps {
            let msg = vec![42];
            let witness = arena0_protocol::WitnessCommitment([0x55; 32]);
            let sender = participants[1].0;
            let mut second = TraceEntry {
                trace_version: TRACE_FORMAT_VERSION,
                step: 1,
                event: PublicEvent::MessageReceived {
                    message_id: MessageId::derive(
                        session,
                        sender,
                        1,
                        first_post_state,
                        &msg,
                        witness,
                    ),
                    from: sender,
                    position: 1,
                    pre_state: first_post_state,
                    msg,
                },
                effects: vec![PublicEffect::SessionEnd {
                    outcome: outcome.clone(),
                }],
                pre_state: first_post_state,
                post_state: final_state,
                fuel_used: 4,
                witness: Some(witness),
                agreement: AggregateAttestation::empty(),
            };
            let second_commitment =
                StepCommitment::for_entry(session, &second, first_commitment.link_hash());
            let step_signatures = participants
                .iter()
                .map(|(_, _, execution)| execution.sign(&second_commitment.signing_bytes()))
                .collect::<Vec<_>>();
            second.agreement = AggregateAttestation::from_signatures(
                SignerSet::full(participants.len()).expect("full signer set"),
                &step_signatures,
            )
            .expect("step aggregate");
            entries.push(second);
        }
        let terminal_commitment = TerminalCommitment::new(
            session,
            if two_steps { 1 } else { 0 },
            final_state,
            OutcomeHash::of(&outcome),
        );
        let terminal_signatures = participants
            .iter()
            .map(|(_, _, execution)| execution.sign(&terminal_commitment.signing_bytes()))
            .collect::<Vec<_>>();
        let terminal = SessionTerminal {
            final_step: if two_steps { 1 } else { 0 },
            final_state,
            outcome_hash: OutcomeHash::of(&outcome),
            agreement: AggregateAttestation::from_signatures(
                SignerSet::full(participants.len()).expect("full signer set"),
                &terminal_signatures,
            )
            .expect("terminal aggregate"),
        };
        let body = ReceiptBody::new(
            SessionHeader::new(activation, ReceiptTermination::Completed { terminal }),
            outcome,
            params,
            entries,
        )
        .expect("fixture body");

        arena0_protocol::ReceiptArtifact::new(body)?.encode()
    }

    #[test]
    fn malformed_receipts_are_rejected_without_panicking() {
        let mut samples = vec![Vec::new(), vec![0xff], vec![1], vec![1, 99]];
        samples.push(vec![0u8; arena0_protocol::MAX_RECEIPT_BYTES + 1]);
        for bytes in samples {
            assert!(verify_light(&bytes).is_err());
        }
    }

    #[test]
    fn valid_canonical_receipt_is_accepted() {
        let bytes = fixture();
        let verified = verify_light(&bytes).expect("fixture verifies");
        assert_eq!(verified.steps, 1);
        assert!(matches!(
            verified.terminal,
            LightVerifiedTerminal::Completed { outcome_borsh }
                if outcome_borsh == vec![7, 8, 9]
        ));
    }

    #[test]
    fn tampered_evidence_fails_closed_without_panicking() {
        let original = fixture();
        let cases = [0, 1, original.len() / 2, original.len() - 1];
        for offset in cases {
            let mut bytes = original.clone();
            bytes[offset] ^= 1;
            assert!(verify_light(&bytes).is_err());
        }
    }

    #[test]
    fn truncated_prefixes_and_unknown_version_fail_closed() {
        let original = fixture();
        for end in 0..original.len() {
            assert!(
                verify_light(&original[..end]).is_err(),
                "invalid prefix at {end}"
            );
        }
        let mut unknown = original;
        unknown[0] = 0xff;
        assert!(verify_light(&unknown).is_err());
    }
}
