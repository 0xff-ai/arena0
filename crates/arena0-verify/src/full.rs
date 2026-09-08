//! Full verification by replaying each public call in fresh guest instances.

use std::time::Instant;

use arena0_program::{CallStatus, JsonBytes, PROGRAM_MAX_LEN, ProgramHash};
use arena0_protocol::{
    Committed, Effect, Ensemble, PublicEffect, PublicEvent, ReceiptArtifact, SharedStateBytes,
    StateHash, StopCause, TraceEntry,
};
use arena0_sandbox::{
    AdmittedProgram, InitializeCall, OutcomeCall, Program, SharedCall, SharedEvent, WasmtimeEngine,
    WriterCall,
};

use crate::VerifyError;
use crate::light::{LightVerifiedTerminal, decode_receipt, verify_decoded};

const PERFORMANCE_TARGET: &str = "arena0::performance";

/// The terminal proof evidence returned after full replay.
///
/// A completed result always includes both the opaque authenticated Borsh
/// outcome and the validated guest-produced JSON projection. A stopped result
/// carries only its exact authenticated or shared stop cause.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifiedTerminal {
    /// The final public entry carries one successful `SessionEnd` effect.
    Completed {
        /// Opaque stock-Borsh outcome bytes authenticated by the terminal.
        outcome_borsh: Vec<u8>,
        /// Validated guest-produced agent-facing JSON projection.
        outcome_json: JsonBytes,
    },
    /// The execution stopped at the exact authenticated or shared boundary
    /// carried by the receipt.
    Stopped { cause: StopCause },
}

/// The result of full receipt verification and replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedOutcome {
    /// The session whose receipt was verified.
    pub session_id: arena0_protocol::SessionHash,
    /// Number of contiguous public steps.
    pub steps: u64,
    /// The terminal proof evidence accepted by both verification tiers.
    pub terminal: VerifiedTerminal,
}

/// Verify a bounded encoded receipt and replay it against the exact Wasm bytes.
pub fn verify_full(
    program_binary: &[u8],
    receipt_bytes: &[u8],
) -> Result<VerifiedOutcome, VerifyError> {
    let started =
        tracing::enabled!(target: PERFORMANCE_TARGET, tracing::Level::DEBUG).then(Instant::now);
    let result = verify_full_inner(program_binary, receipt_bytes);
    if let Some(started) = started {
        record_full_replay(started, receipt_bytes.len(), &result);
    }
    result
}

fn record_full_replay(
    started: Instant,
    encoded_size: usize,
    result: &Result<VerifiedOutcome, VerifyError>,
) {
    let (session_id, count, success, result_class) = match result {
        Ok(outcome) => (Some(outcome.session_id), outcome.steps, true, "verified"),
        Err(_) => (None, 0, false, "error"),
    };
    let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    tracing::debug!(
        target: PERFORMANCE_TARGET,
        operation = "receipt_verify_full",
        ?session_id,
        version = arena0_protocol::TRACE_FORMAT_VERSION,
        input_kind = "encoded_receipt",
        encoded_size,
        count,
        success,
        result_class,
        elapsed_us,
    );
}

fn verify_full_inner(
    program_binary: &[u8],
    receipt_bytes: &[u8],
) -> Result<VerifiedOutcome, VerifyError> {
    let receipt = decode_receipt(receipt_bytes)?;
    // ponytail: consume the verified receipt fields at their only use site.
    let crate::light::LightVerified {
        program_id,
        session_id,
        ensemble,
        steps,
        terminal,
    } = verify_decoded(&receipt)?;
    let program_len =
        u64::try_from(program_binary.len()).map_err(|_| VerifyError::ProgramTooLarge {
            actual: usize::MAX,
            max: PROGRAM_MAX_LEN,
        })?;
    if program_len > PROGRAM_MAX_LEN {
        return Err(VerifyError::ProgramTooLarge {
            actual: program_binary.len(),
            max: PROGRAM_MAX_LEN,
        });
    }
    let loaded_hash = ProgramHash::of(program_binary);
    if loaded_hash != program_id {
        return Err(VerifyError::ProgramMismatch {
            loaded: loaded_hash,
            attested: program_id,
        });
    }

    let program = Program::try_from(program_binary.to_vec())
        .map_err(|error| VerifyError::Sandbox(error.to_string()))?;
    let engine = WasmtimeEngine::new().map_err(|error| VerifyError::Sandbox(error.to_string()))?;
    let admitted = engine
        .admit(&program)
        .map_err(|error| VerifyError::Sandbox(error.to_string()))?;
    verify_profile(&admitted, &receipt)?;

    let session = Ensemble::<Committed>::from_peers(ensemble).map_err(|error| {
        VerifyError::ReplayMismatch {
            step: 0,
            message: format!("invalid receipt ensemble: {error}"),
        }
    })?;
    let params = JsonBytes::try_new(receipt.body().params().to_vec()).map_err(|error| {
        VerifyError::ReplayMismatch {
            step: 0,
            message: format!("activation params are not valid JSON: {error}"),
        }
    })?;
    let initialized = admitted
        .initialize(InitializeCall::new(params))
        .map_err(|error| VerifyError::Sandbox(error.to_string()))?;
    let trace = receipt.body().trace();
    let initial_hash = StateHash::of(initialized.shared.as_bytes());
    let recorded_initial = trace.first().map_or_else(
        || {
            receipt
                .body()
                .header()
                .activation
                .offer()
                .data()
                .initial_state
        },
        |first| first.pre_state,
    );
    if initial_hash != recorded_initial {
        return Err(VerifyError::ReplayMismatch {
            step: 0,
            message: format!(
                "initialized shared state hash {initial_hash} differs from recorded {recorded_initial}",
            ),
        });
    }

    // Re-run the read-only outcome projection against the final explicit shared
    // state only for a completed proof. A stopped proof has no outcome DTO to
    // project; its exact StopCause remains the result evidence.
    let final_shared = replay_trace(&admitted, &session, initialized.shared, trace)?;
    let terminal = match terminal {
        LightVerifiedTerminal::Stopped { cause } => VerifiedTerminal::Stopped { cause },
        LightVerifiedTerminal::Completed { outcome_borsh } => {
            let outcome = admitted
                .outcome(OutcomeCall::new(final_shared, session))
                .map_err(|error| VerifyError::Sandbox(error.to_string()))?;
            if outcome.borsh.as_bytes() != outcome_borsh.as_slice() {
                return Err(VerifyError::OutcomeMismatch);
            }
            VerifiedTerminal::Completed {
                outcome_borsh,
                outcome_json: outcome.json,
            }
        }
    };

    Ok(VerifiedOutcome {
        session_id,
        steps,
        terminal,
    })
}

fn verify_profile(
    admitted: &AdmittedProgram,
    receipt: &ReceiptArtifact,
) -> Result<(), VerifyError> {
    let attested = receipt
        .body()
        .header()
        .activation
        .offer()
        .data()
        .execution_profile;
    let replaying = admitted.profile().hash();
    if attested != replaying {
        return Err(VerifyError::FingerprintMismatch {
            attested: attested.to_string(),
            replaying: replaying.to_string(),
        });
    }
    Ok(())
}

fn replay_trace(
    admitted: &AdmittedProgram,
    session: &Ensemble<Committed>,
    mut shared: SharedStateBytes,
    trace: &[TraceEntry],
) -> Result<SharedStateBytes, VerifyError> {
    for entry in trace {
        shared = replay_entry(admitted, session, shared, entry)?;
    }
    Ok(shared)
}

fn replay_entry(
    admitted: &AdmittedProgram,
    session: &Ensemble<Committed>,
    shared: SharedStateBytes,
    entry: &TraceEntry,
) -> Result<SharedStateBytes, VerifyError> {
    let before = StateHash::of(shared.as_bytes());
    if before != entry.pre_state {
        return Err(VerifyError::ReplayMismatch {
            step: entry.step,
            message: format!(
                "pre-state {before} differs from recorded {}",
                entry.pre_state
            ),
        });
    }
    let call = match &entry.event {
        PublicEvent::SessionStarted { .. } => SharedCall::session_started(shared, session.clone()),
        PublicEvent::MessageReceived {
            message_id,
            from,
            position,
            pre_state,
            msg,
        } => {
            let writer = admitted
                .writer(WriterCall::new(shared.clone(), session.clone()))
                .map_err(|error| VerifyError::Sandbox(error.to_string()))?;
            let sender =
                session
                    .participant_of(from)
                    .ok_or_else(|| VerifyError::ReplayMismatch {
                        step: entry.step,
                        message: "message sender is outside the committed ensemble".to_owned(),
                    })?;
            if writer.writer != Some(sender) {
                return Err(VerifyError::ReplayMismatch {
                    step: entry.step,
                    message: format!(
                        "message sender {sender:?} is not the guest-selected writer {:?}",
                        writer.writer
                    ),
                });
            }
            SharedCall::new(
                shared,
                session.clone(),
                SharedEvent::MessageReceived {
                    message_id: *message_id,
                    from: *from,
                    position: *position,
                    pre_state: *pre_state,
                    msg: msg.clone(),
                },
            )
        }
    };
    let result = admitted
        .apply_shared(call)
        .map_err(|error| VerifyError::Sandbox(error.to_string()))?;
    if result.status != CallStatus::Accepted {
        return Err(VerifyError::ReplayMismatch {
            step: entry.step,
            message: "recorded public entry was rejected by the guest".to_owned(),
        });
    }
    if !result.observations.random_draws.is_empty() {
        return Err(VerifyError::ReplayMismatch {
            step: entry.step,
            message: "shared replay produced unrecorded random draws".to_owned(),
        });
    }
    if result.observations.fuel_used != entry.fuel_used {
        return Err(VerifyError::ReplayMismatch {
            step: entry.step,
            message: format!(
                "fuel differs: replayed {}, recorded {}",
                result.observations.fuel_used, entry.fuel_used
            ),
        });
    }
    let effects = public_effects(result.observations.effects, entry.step)?;
    if effects != entry.effects {
        return Err(VerifyError::ReplayMismatch {
            step: entry.step,
            message: format!(
                "effects differ: replayed {effects:?}, recorded {:?}",
                entry.effects
            ),
        });
    }
    let after = StateHash::of(result.shared.as_bytes());
    if after != entry.post_state {
        return Err(VerifyError::ReplayMismatch {
            step: entry.step,
            message: format!(
                "post-state {after} differs from recorded {}",
                entry.post_state
            ),
        });
    }
    Ok(result.shared)
}

fn public_effects(effects: Vec<Effect>, step: u64) -> Result<Vec<PublicEffect>, VerifyError> {
    effects
        .into_iter()
        .map(PublicEffect::try_from)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| VerifyError::ReplayMismatch {
            step,
            message: format!("shared replay emitted a private effect: {error}"),
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use arena0_crypto::{BlsSignature, ExecutionKey, ExecutionSalt, NodeKeys, SecretKey};
    use arena0_program::{
        JsonSchemaDocument, ProgramDefinition, ProgramMetadata, ProgramSchema, StateSchema,
    };
    use arena0_protocol::{
        Activation, AggregateAttestation, CHAIN_START, Ensemble, MessageId, Offer, OfferData,
        OutcomeHash, PeerId, PublicEffect, PublicEvent, ReceiptBody, SessionHeader,
        SessionTerminal, SignerSet, StepCommitment, TRACE_FORMAT_VERSION, TerminalCommitment,
        Ticket, TicketAction, TicketData, TicketHash, TraceEntry, WitnessCommitment,
    };
    use arena0_sandbox::SharedEvent;

    fn wat_data(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|byte| format!("\\{byte:02x}"))
            .collect::<Vec<_>>()
            .join("")
    }

    /// Build a tiny real guest that accepts two public calls, ends the session
    /// on the second one, and projects a unit outcome. The test drives this
    /// admitted program to produce its trace rather than hand-writing guest
    /// outputs, so full verification exercises the actual replay boundary.
    fn admitted_replay_program() -> (Vec<u8>, Arc<AdmittedProgram>) {
        let unit = JsonSchemaDocument::unit();
        let definition = ProgramDefinition {
            metadata: ProgramMetadata {
                name: "full-replay-fixture".into(),
                version: "0.1.0".into(),
                description: "full replay fixture".into(),
                author: None,
                capabilities: Vec::new(),
                display_name: "Full replay fixture".into(),
                participants: arena0_program::ParticipantCount::Exact { count: 2 },
            },
            schema: ProgramSchema {
                state: StateSchema {
                    schema: unit.clone(),
                    max_bytes: 0,
                },
                callouts: Vec::new(),
                messages: Vec::new(),
                params: unit.clone(),
                queries: Vec::new(),
                outcome: unit,
            },
        };
        let metadata = definition.encode().expect("encode metadata");
        let init = [0u8; 8];
        let shared = [0u8; 5];
        let writer = [1u8, 0];
        // OutcomeOutput is two bounded vectors: the protocol outcome bytes
        // (`[0]`) followed by the agent-facing JSON projection (`null`).
        let outcome = [1u8, 0, 0, 0, 0, 4, 0, 0, 0, b'n', b'u', b'l', b'l'];
        let raw = wat::parse_str(format!(
            r#"
            (module
              (import "arena0" "end_session" (func $end_session (param i32 i32)))
              (memory (export "memory") 2)
              (global (export "arena0_abi_version") i32 (i32.const 20))
              (data (i32.const 2040) "{end_outcome}")
              (data (i32.const 2048) "{init}")
              (data (i32.const 2064) "{shared}")
              (data (i32.const 2072) "{writer}")
              (data (i32.const 2080) "{outcome}")
              (data (i32.const 4096) "{metadata}")
              (func $pack (param $ptr i32) (param $len i32) (result i64)
                local.get $ptr
                i64.extend_i32_s
                i64.const 32
                i64.shl
                local.get $len
                i64.extend_i32_s
                i64.const 4294967295
                i64.and
                i64.or)
              (func (export "arena0_alloc") (param i32) (result i32) i32.const 1024)
              (func (export "arena0_dealloc") (param i32 i32))
              (func (export "arena0_initialize") (param i32 i32) (result i64)
                i32.const 2048 i32.const 8 call $pack)
              (func (export "arena0_shared") (param i32 i32) (result i64)
                local.get 0
                ;; SharedInput encodes an empty shared vector, then a one-byte
                ;; None/Some session option at offset 8.
                i32.load8_u offset=8
                (if
                  (then i32.const 2040 i32.const 1 call $end_session))
                i32.const 2064 i32.const 5 call $pack)
              (func (export "arena0_local") (param i32 i32) (result i64)
                i32.const 2064 i32.const 5 call $pack)
              (func (export "arena0_writer") (param i32 i32) (result i64)
                i32.const 2072 i32.const 2 call $pack)
              (func (export "arena0_outcome") (param i32 i32) (result i64)
                i32.const 2080 i32.const 13 call $pack)
              (func (export "arena0_query") (param i32 i32) (result i64)
                i32.const 2072 i32.const 2 call $pack)
              (func (export "arena0_view") (param i32 i32) (result i64)
                i32.const 2080 i32.const 12 call $pack)
              (func (export "arena0_metadata") (result i64)
                i32.const 4096 i32.const {metadata_len} call $pack))
            "#,
            end_outcome = wat_data(&[0]),
            init = wat_data(&init),
            shared = wat_data(&shared),
            writer = wat_data(&writer),
            outcome = wat_data(&outcome),
            metadata = wat_data(&metadata),
            metadata_len = metadata.len(),
        ))
        .expect("compile WAT fixture");
        let engine = WasmtimeEngine::new().expect("sandbox engine");
        let program = engine.build_program(&raw).expect("embed metadata");
        let binary = program.bytes().to_vec();
        let parsed = Program::try_from(binary.clone()).expect("parse completed program");
        let admitted = engine.admit(&parsed).expect("admit fixture");
        (binary, admitted)
    }

    fn replay_receipt(admitted: &AdmittedProgram, stopped: bool) -> Vec<u8> {
        let identities = [
            NodeKeys::from_secret(SecretKey::from_bytes([1; 32])),
            NodeKeys::from_secret(SecretKey::from_bytes([2; 32])),
        ];
        let negotiation_id = arena0_protocol::NegotiationId([0x71; 32]);
        let executions = [
            ExecutionKey::derive(
                &ExecutionSalt::try_from_bytes([11; 32]).expect("non-zero test salt"),
                &[21; 32],
                &negotiation_id.0,
            )
            .expect("execution key"),
            ExecutionKey::derive(
                &ExecutionSalt::try_from_bytes([12; 32]).expect("non-zero test salt"),
                &[22; 32],
                &negotiation_id.0,
            )
            .expect("execution key"),
        ];
        let peers = identities
            .iter()
            .map(|identity| PeerId(identity.ed25519_public_key().0))
            .collect::<Vec<_>>();
        let params = arena0_program::JsonBytes::try_new(br#"null"#.to_vec()).expect("params");
        let initialized = admitted
            .initialize(InitializeCall::new(params.clone()))
            .expect("initialize");
        let initial_state = StateHash::of(initialized.shared.as_bytes());
        let offer_data = OfferData::new(
            negotiation_id,
            0,
            peers[0],
            admitted.program().hash(),
            admitted.profile().hash(),
            params.clone(),
            2,
            initial_state,
            1,
        )
        .expect("offer data");
        let offer_hash = arena0_protocol::OfferHash::of(&offer_data);
        let tickets = identities
            .iter()
            .zip(&executions)
            .zip(&peers)
            .map(|((identity, execution), peer)| {
                let data = TicketData::new(
                    negotiation_id,
                    0,
                    *peer,
                    0,
                    TicketAction::Active {
                        execution_bls: execution.public_key(),
                        key_binding: execution
                            .key_binding(&offer_hash.0, &identity.ed25519_public_key().0),
                        issued_at_unix_ms: 0,
                        valid_for_ms: 1,
                    },
                )
                .expect("ticket data");
                Ticket {
                    signature: identity.sign(&data.signing_bytes()),
                    data,
                }
            })
            .collect::<Vec<_>>();
        let offer = Offer::new(
            offer_data,
            tickets
                .iter()
                .map(|ticket| TicketHash::of(&ticket.data))
                .collect(),
        )
        .expect("offer");
        let prepared =
            arena0_protocol::PreparedActivation::new(offer, tickets).expect("prepared activation");
        let activation_data = prepared.activation_data().signing_bytes();
        let activation = Activation::new(
            prepared,
            BlsSignature::aggregate(
                &executions
                    .iter()
                    .map(|execution| execution.sign(&activation_data))
                    .collect::<Vec<_>>(),
            )
            .expect("activation aggregate"),
        )
        .expect("activation");
        let ensemble = Ensemble::from_peers(peers.clone()).expect("ensemble");

        let started = admitted
            .apply_shared(SharedCall::session_started(
                initialized.shared.clone(),
                ensemble.clone(),
            ))
            .expect("session start replay");
        assert!(started.observations.effects.is_empty());
        let first_state = StateHash::of(started.shared.as_bytes());
        let mut first = TraceEntry {
            trace_version: TRACE_FORMAT_VERSION,
            step: 0,
            event: PublicEvent::SessionStarted {
                ensemble: ensemble.clone(),
            },
            effects: Vec::new(),
            pre_state: initial_state,
            post_state: first_state,
            fuel_used: started.observations.fuel_used,
            witness: None,
            agreement: AggregateAttestation::empty(),
        };
        let first_commitment =
            StepCommitment::for_entry(activation.session_hash(), &first, CHAIN_START);
        let first_signatures = executions
            .iter()
            .map(|execution| execution.sign(&first_commitment.signing_bytes()))
            .collect::<Vec<_>>();
        first.agreement = AggregateAttestation::from_signatures(
            SignerSet::full(executions.len()).expect("full signer set"),
            &first_signatures,
        )
        .expect("first agreement");

        if stopped {
            let coordinate =
                arena0_protocol::PublicCursor::new(1, first_state, first_commitment.link_hash());
            let unsigned = arena0_protocol::AbortOccurrence::unsigned(
                activation.session_hash(),
                peers[0],
                arena0_protocol::AbortKind::Abort,
                7,
                "stopped after the first public step",
                coordinate,
            )
            .expect("stopped occurrence");
            let occurrence = unsigned
                .clone()
                .with_signature(
                    identities[0].sign(&unsigned.signing_bytes().expect("abort signing bytes")),
                )
                .expect("signed stopped occurrence");
            let body = ReceiptBody::new(
                SessionHeader::new(
                    activation,
                    arena0_protocol::ReceiptTermination::Stopped {
                        cause: arena0_protocol::StopCause::Authenticated(occurrence),
                    },
                ),
                Vec::new(),
                params.into_bytes(),
                vec![first],
            )
            .expect("stopped receipt body");

            return arena0_protocol::ReceiptArtifact::new(body)
                .expect("authenticated stop report")
                .encode()
                .expect("stopped receipt encoding");
        }

        let message = vec![42];
        let witness = WitnessCommitment([0x55; 32]);
        // `Ensemble` freezes canonical sorted order, which need not match the
        // identity construction order above. The fixture guest selects index
        // zero, so derive the sender from the committed ensemble itself.
        let sender = ensemble.peers()[0];
        let message_id = MessageId::derive(
            activation.session_hash(),
            sender,
            1,
            first_state,
            &message,
            witness,
        );
        let second_event = SharedEvent::MessageReceived {
            message_id,
            from: sender,
            position: 1,
            pre_state: first_state,
            msg: message.clone(),
        };
        let writer = admitted
            .writer(arena0_sandbox::WriterCall::new(
                started.shared.clone(),
                ensemble.clone(),
            ))
            .expect("writer projection");
        assert_eq!(
            writer.writer.map(|participant| participant.index()),
            Some(0)
        );
        let finished = admitted
            .apply_shared(SharedCall::new(
                started.shared,
                ensemble.clone(),
                second_event,
            ))
            .expect("message replay");
        assert_eq!(
            finished.observations.effects,
            vec![arena0_protocol::Effect::SessionEnd { outcome: vec![0] }]
        );
        let final_state = StateHash::of(finished.shared.as_bytes());
        let mut second = TraceEntry {
            trace_version: TRACE_FORMAT_VERSION,
            step: 1,
            event: PublicEvent::MessageReceived {
                message_id,
                from: sender,
                position: 1,
                pre_state: first_state,
                msg: message,
            },
            effects: vec![PublicEffect::SessionEnd { outcome: vec![0] }],
            pre_state: first_state,
            post_state: final_state,
            fuel_used: finished.observations.fuel_used,
            witness: Some(witness),
            agreement: AggregateAttestation::empty(),
        };
        let second_commitment = StepCommitment::for_entry(
            activation.session_hash(),
            &second,
            first_commitment.link_hash(),
        );
        let second_signatures = executions
            .iter()
            .map(|execution| execution.sign(&second_commitment.signing_bytes()))
            .collect::<Vec<_>>();
        second.agreement = AggregateAttestation::from_signatures(
            SignerSet::full(executions.len()).expect("full signer set"),
            &second_signatures,
        )
        .expect("second agreement");
        let outcome = vec![0];
        let terminal_commitment = TerminalCommitment::new(
            activation.session_hash(),
            1,
            final_state,
            OutcomeHash::of(&outcome),
        );
        let terminal_signatures = executions
            .iter()
            .map(|execution| execution.sign(&terminal_commitment.signing_bytes()))
            .collect::<Vec<_>>();
        let terminal = SessionTerminal {
            final_step: 1,
            final_state,
            outcome_hash: OutcomeHash::of(&outcome),
            agreement: AggregateAttestation::from_signatures(
                SignerSet::full(executions.len()).expect("full signer set"),
                &terminal_signatures,
            )
            .expect("terminal agreement"),
        };
        let body = ReceiptBody::new(
            SessionHeader::new(
                activation,
                arena0_protocol::ReceiptTermination::Completed { terminal },
            ),
            outcome,
            params.into_bytes(),
            vec![first, second],
        )
        .expect("receipt body");

        arena0_protocol::ReceiptArtifact::new(body)
            .expect("authenticated artifact")
            .encode()
            .expect("receipt encoding")
    }

    #[test]
    fn malformed_receipt_and_oversized_program_fail_without_panicking() {
        let oversized_program = vec![0u8; PROGRAM_MAX_LEN as usize + 1];
        let cases: [&[u8]; 3] = [&[], &[0xff], &[1, 2, 3]];
        for receipt in cases {
            assert!(verify_full(&[], receipt).is_err());
        }
        assert!(verify_full(&oversized_program, &[]).is_err());
    }

    #[test]
    fn replay_rejects_a_program_outside_the_receipt_binding() {
        let receipt =
            crate::light::tests::fixture_for_replay(ProgramHash([0x44; 32]), StateHash::of(&[0]));

        assert!(matches!(
            verify_full(b"not the attested program", &receipt),
            Err(VerifyError::ProgramMismatch { .. })
        ));
    }

    #[test]
    fn replay_accepts_a_receipt_from_an_admitted_program() {
        let (program, admitted) = admitted_replay_program();
        let receipt = replay_receipt(&admitted, false);

        let verified = verify_full(&program, &receipt).expect("valid replay receipt");
        assert_eq!(verified.steps, 2);
        assert!(matches!(
            verified.terminal,
            VerifiedTerminal::Completed {
                outcome_borsh,
                outcome_json: ref json,
            } if outcome_borsh == vec![0] && json.as_bytes() == br#"null"#
        ));
    }

    #[test]
    fn stopped_receipt_still_requires_the_bound_program_and_replays_its_public_prefix() {
        let (program, admitted) = admitted_replay_program();
        let receipt = replay_receipt(&admitted, true);

        assert!(matches!(
            verify_full(b"not the bound program", &receipt),
            Err(VerifyError::ProgramMismatch { .. })
        ));
        let verified = verify_full(&program, &receipt).expect("valid stopped replay receipt");
        assert_eq!(verified.steps, 1);
        assert!(matches!(
            verified.terminal,
            VerifiedTerminal::Stopped {
                cause: arena0_protocol::StopCause::Authenticated(ref occurrence),
            } if occurrence.coordinate().next_step() == 1
        ));
    }

    #[test]
    fn performance_replay_records_safe_fields_without_receipt_contents() {
        use std::io::{self, Write};
        use std::sync::{Arc, Mutex};

        let (program, admitted) = admitted_replay_program();
        let receipt = replay_receipt(&admitted, false);
        let output = SharedWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(output.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            assert!(tracing::enabled!(target: PERFORMANCE_TARGET, tracing::Level::DEBUG));
            verify_full(&program, &receipt).expect("valid replay receipt");
        });

        let lines = output
            .text()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON trace line"))
            .collect::<Vec<_>>();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line["fields"]["operation"] == "receipt_verify_full")
                .count(),
            1,
            "one aggregate event per full replay"
        );
        let allowed = [
            "operation",
            "session_id",
            "version",
            "public_step",
            "input_kind",
            "encoded_size",
            "count",
            "success",
            "result_class",
            "elapsed_us",
        ];
        for line in &lines {
            assert_eq!(line["target"], "arena0::performance");
            let fields = line["fields"].as_object().expect("structured fields");
            assert!(fields.keys().all(|field| allowed.contains(&field.as_str())));
            for forbidden in [
                "params",
                "outcome",
                "context",
                "signature",
                "program",
                "private",
                "sql",
                "payload",
            ] {
                assert!(
                    !fields.contains_key(forbidden),
                    "unexpected field {forbidden}"
                );
            }
        }
        let aggregate = lines
            .iter()
            .find(|line| line["fields"]["operation"] == "receipt_verify_full")
            .expect("aggregate replay event");
        assert_eq!(aggregate["fields"]["count"], 2);
        assert_eq!(aggregate["fields"]["success"], true);
        assert!(!output.text().contains("full-replay-fixture"));
        assert!(!output.text().contains("null"));

        #[derive(Clone, Default)]
        struct SharedWriter(Arc<Mutex<Vec<u8>>>);

        impl SharedWriter {
            fn text(&self) -> String {
                String::from_utf8(self.0.lock().expect("writer lock").clone())
                    .expect("trace output is UTF-8")
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedWriter {
            type Writer = SharedWriter;

            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        impl Write for SharedWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().expect("writer lock").extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
    }
}
