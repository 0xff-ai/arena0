//! Protocol-only validation of one complete authenticated artifact.

use arena0_program::ProgramHash;
use arena0_protocol::{PeerId, ReceiptArtifact, ReceiptTermination, SessionHash, StopCause};

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
    /// The authenticated terminal evidence represented by the artifact.
    pub terminal: LightVerifiedTerminal,
}

/// The terminal evidence accepted by light verification.
///
/// Light verification authenticates the opaque Borsh outcome but cannot run the
/// guest to produce its JSON projection. A stopped result never carries an
/// outcome field, so callers cannot mistake an empty byte vector for a stopped
/// proof.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LightVerifiedTerminal {
    /// The final certified entry carries a `SessionEnd` terminal.
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
    verify_light_artifact(&receipt)
}

/// Verify an already-authenticated [`ReceiptArtifact`] without re-encoding it.
///
/// A `ReceiptArtifact` is authenticated and bounded at construction, so this
/// skips the artifact decode step and projects its evidence directly.
pub fn verify_light_artifact(receipt: &ReceiptArtifact) -> Result<LightVerified, VerifyError> {
    project_verified(receipt)
}

pub(crate) fn decode_receipt(receipt_bytes: &[u8]) -> Result<ReceiptArtifact, VerifyError> {
    if receipt_bytes.len() > arena0_protocol::MAX_RECEIPT_BYTES {
        return Err(VerifyError::ReceiptTooLarge {
            actual: receipt_bytes.len(),
            max: arena0_protocol::MAX_RECEIPT_BYTES,
        });
    }
    ReceiptArtifact::decode(receipt_bytes)
        .map_err(|error| VerifyError::ReceiptDecode(sanitize_verify_message(error.to_string())))
}

/// Project evidence from an artifact authenticated by [`ReceiptArtifact::decode`].
fn project_verified(receipt: &ReceiptArtifact) -> Result<LightVerified, VerifyError> {
    let body = receipt.body();
    let activation = &body.header().activation;
    let mut ensemble = activation
        .tickets()
        .iter()
        .map(|ticket| ticket.data.signer)
        .collect::<Vec<_>>();
    ensemble.sort_unstable();
    let session_id = activation.session_hash();
    let terminal = match &body.header().terminal {
        ReceiptTermination::Completed => LightVerifiedTerminal::Completed {
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use arena0_crypto::{BlsSignature, ExecutionKey, ExecutionSalt, NodeKeys, SecretKey};
    use arena0_program::ExecutionProfile;
    use arena0_protocol::{
        Activation, AggregateAttestation, CHAIN_START, Committed, Ensemble, Offer, OfferData,
        PreparedActivation, ReceiptBody, SessionHeader, SignerSet, StateHash, StepCommitment,
        StepEvent, StepTerminal, TRACE_FORMAT_VERSION, Ticket, TicketAction, TicketData,
        TraceEntry,
    };

    fn fixture() -> Vec<u8> {
        fixture_with_binding(ProgramHash([0x44; 32]), StateHash([0x11; 32]), false)
            .expect("valid fixture")
    }

    fn fixture_with_binding(
        program_hash: ProgramHash,
        initial_state: StateHash,
        two_steps: bool,
    ) -> Result<Vec<u8>, arena0_protocol::ProtocolError> {
        fixture_with_message_sender(program_hash, initial_state, two_steps, None)
    }

    fn fixture_with_message_sender(
        program_hash: ProgramHash,
        initial_state: StateHash,
        two_steps: bool,
        message_sender: Option<PeerId>,
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
            event: StepEvent::SessionStarted {
                ensemble: ensemble.clone(),
            },
            pre_state: initial_state,
            post_state: first_post_state,
            terminal: (!two_steps).then(|| StepTerminal::End {
                outcome: outcome.clone(),
            }),
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
            let sender = message_sender.unwrap_or(participants[1].0);
            let mut second = TraceEntry {
                trace_version: TRACE_FORMAT_VERSION,
                step: 1,
                event: StepEvent::Message {
                    from: sender,
                    data: msg,
                },
                pre_state: first_post_state,
                post_state: final_state,
                terminal: Some(StepTerminal::End {
                    outcome: outcome.clone(),
                }),
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
        let body = ReceiptBody::new(
            SessionHeader::new(activation, ReceiptTermination::Completed),
            outcome,
            params,
            entries,
        )
        .expect("fixture body");

        arena0_protocol::ReceiptArtifact::new(body)?.encode()
    }

    #[test]
    fn invalid_receipt_encodings_fail_closed() {
        let mut samples = vec![Vec::new(), vec![0xff], vec![1], vec![1, 99]];
        samples.push(vec![0u8; arena0_protocol::MAX_RECEIPT_BYTES + 1]);
        for bytes in samples {
            assert!(verify_light(&bytes).is_err());
        }

        let original = fixture();
        let cases = [0, 1, original.len() / 2, original.len() - 1];
        for offset in cases {
            let mut bytes = original.clone();
            bytes[offset] ^= 1;
            assert!(verify_light(&bytes).is_err());
        }

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

    #[test]
    fn obsolete_receipt_and_body_versions_are_rejected() {
        let original = fixture();
        for (offset, version) in [(0, 2), (1, 2), (0, 3), (1, 3)] {
            let mut bytes = original.clone();
            bytes[offset] = version;
            assert!(
                verify_light(&bytes).is_err(),
                "obsolete version at {offset}"
            );
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
    fn artifact_verification_matches_encoded_verification() {
        let bytes = fixture();
        let receipt = arena0_protocol::ReceiptArtifact::decode(&bytes).expect("fixture decodes");
        assert_eq!(
            verify_light(&bytes).expect("encoded fixture verifies"),
            verify_light_artifact(&receipt).expect("artifact verifies"),
        );
    }

    #[test]
    fn valid_multi_step_message_receipt_is_accepted() {
        let bytes = fixture_with_binding(ProgramHash([0x44; 32]), StateHash([0x11; 32]), true)
            .expect("valid multi-step fixture");
        let verified = verify_light(&bytes).expect("multi-step fixture verifies");
        assert_eq!(verified.steps, 2);
        assert!(matches!(
            verified.terminal,
            LightVerifiedTerminal::Completed { outcome_borsh }
                if outcome_borsh == vec![7, 8, 9]
        ));
    }

    #[test]
    fn message_receipt_rejects_sender_outside_ensemble() {
        let error = fixture_with_message_sender(
            ProgramHash([0x44; 32]),
            StateHash([0x11; 32]),
            true,
            Some(PeerId([0x77; 32])),
        )
        .expect_err("outsider message must not become authenticated evidence");
        assert!(matches!(
            error,
            arena0_protocol::ProtocolError::UnknownParticipant { .. }
        ));
    }
}
