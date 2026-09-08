//! The arena0 daemon's local protocol: the [`Request`]/[`Response`] types both the
//! daemon and its thin clients (the CLI and the MCP server) share, so the two
//! cannot drift. Over the Unix domain socket, frames are length-prefixed JSON
//! (see [`frame`], behind the `io` feature); every method is request/response except
//! `events.subscribe`, which acks then streams [`EventFrame`]s.
//!
//! Values a program schema describes cross as JSON, not opaque bytes, checked
//! daemon-side against the program's public JSON Schema.
//!
//! The agent never holds a key and never signs: there is no `sign`/`submit_signature`
//! method, and [`NextEvent`] has no signing variant. The daemon answers signing
//! internally with the custodied identity.

#[cfg(feature = "io")]
pub mod frame;

mod activity;
mod events;
mod request;
mod response;

pub use activity::{ActivityData, ActivityFrame, ActivityResult};
pub use arena0_protocol::{
    ColorDepth, ExecLifecycle, NegotiationTarget, PendingId, ReceiptArtifact, TerminalResult, View,
};
pub use events::{
    EventData, EventFilter, EventFrame, ExecOrigin, ExecutionFailureKind, NegotiationStage,
    SessionTerminal,
};
pub use request::{
    AwaitState, EnsembleSpec, HostRequest, IdRef, ProgramRefError, ReceiptRef, Request,
};
pub use response::{
    ActivationInspection, ActivationInspectionState, ActivationParticipant, ApiError, ApiErrorCode,
    DaemonInfo, ExecStatus, ExecStatusState, ExecutionInspection, FullVerifiedTerminal, HostInfo,
    HostStatus, IdInfo, LightVerifiedTerminal, NextEvent, PendingCalloutStatus,
    PrivateCommitSummary, PrivateEffectKind, PrivateEffectSummary, PrivateEventKind, ProgramDetail,
    ProgramSummary, ReceiptListEntry, ReceiptProvenance, Response, ResponseOk, SessionProgress,
    SessionStatus, VerifiedResult,
};

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_program::{ParticipantCount, ProgramHash};
    use arena0_protocol::{ExecId, NegotiationId, PeerId, SessionHash};

    #[test]
    fn host_routing_is_explicit_at_the_wire_boundary() {
        let request = Request::Host {
            host: "alice".into(),
            request: HostRequest::ExecNext {
                exec_id: ExecId([7; 32]),
            },
        };
        let mut json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["method"], "host.call");
        assert_eq!(json["params"]["host"], "alice");
        assert_eq!(
            serde_json::from_value::<Request>(json.clone()).unwrap(),
            request
        );
        json["params"].as_object_mut().unwrap().remove("host");
        assert!(serde_json::from_value::<Request>(json).is_err());
        assert!(
            serde_json::from_value::<Request>(serde_json::json!({"method":"exec.list"})).is_err()
        );
        for request in [
            Request::DaemonInfo,
            Request::DaemonStop,
            Request::HostsList,
            Request::ActivitySubscribe,
        ] {
            assert_eq!(
                serde_json::from_value::<Request>(serde_json::to_value(&request).unwrap()).unwrap(),
                request
            );
        }
    }

    /// Round-trip every request shape through JSON, asserting the `method` tag is
    /// the wire path.
    #[test]
    fn request_round_trips_with_path_tags() {
        let cases = [
            (HostRequest::IdList, "id.list"),
            (
                HostRequest::ExecNext {
                    exec_id: ExecId([7u8; 32]),
                },
                "exec.next",
            ),
            (
                HostRequest::ExecNew {
                    exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
                    program: "rock-paper-scissors".into(),
                    params: Some(serde_json::json!({"rounds": 3})),
                    ensemble: EnsembleSpec::Explicit {
                        peers: vec![PeerId([2u8; 32])],
                    },
                },
                "exec.new",
            ),
            (
                HostRequest::ExecInspect {
                    exec_id: ExecId([9u8; 32]),
                    private_from: Some(4),
                    private_limit: 32,
                },
                "exec.inspect",
            ),
            (
                HostRequest::ExecSubmit {
                    exec_id: ExecId([1u8; 32]),
                    pending_id: PendingId::new(3),
                    answer: Some(serde_json::json!("Rock")),
                },
                "exec.submit",
            ),
            (
                HostRequest::ExecCancelCreation {
                    exec_id: ExecId([6u8; 32]),
                },
                "exec.cancel_creation",
            ),
            (
                HostRequest::ExecView {
                    exec: ExecId([8u8; 32]),
                    width: 80,
                    color: ColorDepth::Ansi16,
                },
                "exec.view",
            ),
            (HostRequest::Info, "host.info"),
            (HostRequest::ReceiptList, "receipt.list"),
            (
                HostRequest::ReceiptGet {
                    receipt: ReceiptRef::Produced(SessionHash([9u8; 32])),
                },
                "receipt.get",
            ),
        ];
        for (req, path) in cases {
            let json = serde_json::to_value(&req).unwrap();
            assert_eq!(json["method"], path, "wire path for {req:?}");
            let back: HostRequest = serde_json::from_value(json).unwrap();
            assert_eq!(req, back);
        }
    }

    #[test]
    fn request_boundary_values_keep_string_json() {
        let exec_id = ExecId([8u8; 32]);
        let exec_request = HostRequest::ExecView {
            exec: exec_id,
            width: 80,
            color: ColorDepth::Ansi16,
        };
        let exec_json = serde_json::to_value(&exec_request).unwrap();
        assert_eq!(exec_json["params"]["exec"], exec_id.to_string());
        assert_eq!(
            serde_json::from_value::<HostRequest>(exec_json).unwrap(),
            exec_request
        );

        let join_request = HostRequest::ExecNew {
            exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
            program: String::new(),
            params: None,
            ensemble: EnsembleSpec::Join {
                target: Some(NegotiationTarget::new(
                    PeerId([3u8; 32]),
                    NegotiationId([2u8; 32]),
                )),
            },
        };
        let join_json = serde_json::to_value(&join_request).unwrap();
        assert_eq!(
            join_json["params"]["ensemble"]["Join"]["target"]["creator"],
            PeerId([3u8; 32]).to_string()
        );
        assert_eq!(
            join_json["params"]["ensemble"]["Join"]["target"]["negotiation_id"],
            NegotiationId([2u8; 32]).to_string()
        );
        assert_eq!(
            serde_json::from_value::<HostRequest>(join_json).unwrap(),
            join_request
        );

        let pending_id = PendingId::new(u64::MAX);
        let submit = HostRequest::ExecSubmit {
            exec_id,
            pending_id,
            answer: Some(serde_json::json!("Rock")),
        };
        let submit_json = serde_json::to_value(&submit).expect("submit request JSON");
        assert_eq!(submit_json["params"]["pending_id"], pending_id.to_string());
        assert_eq!(
            serde_json::from_value::<HostRequest>(submit_json.clone()).unwrap(),
            submit
        );
        let mut numeric_submit = submit_json;
        numeric_submit["params"]["pending_id"] = serde_json::json!(u64::MAX);
        assert!(
            serde_json::from_value::<HostRequest>(numeric_submit).is_err(),
            "numeric pending ids must not cross the API boundary"
        );
    }

    #[test]
    fn request_rejects_unknown_raw_value_fields() {
        let exec_id = ExecId([1u8; 32]);
        let cases = [
            (
                HostRequest::ExecNew {
                    exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
                    program: "rock-paper-scissors".into(),
                    params: Some(serde_json::json!({})),
                    ensemble: EnsembleSpec::Explicit {
                        peers: vec![PeerId([2; 32])],
                    },
                },
                "params_raw",
            ),
            (
                HostRequest::ExecSubmit {
                    exec_id,
                    pending_id: PendingId::new(3),
                    answer: Some(serde_json::json!({})),
                },
                "answer_raw",
            ),
            (
                HostRequest::ExecQuery {
                    exec_id,
                    query: Some(serde_json::json!({})),
                },
                "query_raw",
            ),
        ];

        for (request, raw_field) in cases {
            let mut json = serde_json::to_value(request).expect("request JSON");
            json["params"][raw_field] = serde_json::json!("00");
            assert!(
                serde_json::from_value::<HostRequest>(json).is_err(),
                "unknown raw field must not be ignored"
            );
        }
    }

    #[test]
    fn program_summary_exposes_participants() {
        let summary = ProgramSummary {
            program_hash: ProgramHash([1; 32]),
            name: "cumulative-sum".into(),
            display_name: "Cumulative sum".into(),
            version: "1.0.0".into(),
            description: "N-party".into(),
            participants: ParticipantCount::Range { min: 2, max: 64 },
        };
        let encoded = serde_json::to_value(summary).unwrap();
        assert_eq!(
            encoded["participants"],
            serde_json::json!({"kind": "range", "min": 2, "max": 64})
        );
    }

    #[test]
    fn receipt_provenance_is_total_and_wire_stable() {
        for (provenance, encoded) in [
            (ReceiptProvenance::Produced, "produced"),
            (ReceiptProvenance::Imported, "imported"),
            (ReceiptProvenance::Both, "both"),
        ] {
            let value = serde_json::to_value(provenance).expect("provenance JSON");
            assert_eq!(value, serde_json::json!(encoded));
            assert_eq!(
                serde_json::from_value::<ReceiptProvenance>(value).expect("provenance decode"),
                provenance
            );
        }
    }

    #[test]
    fn exec_status_serializes_only_state_valid_fields() {
        let status = ExecStatus {
            exec_id: ExecId([5; 32]),
            negotiation_id: None,
            program_id: ProgramHash([7; 32]),
            state: ExecStatusState::Active {
                session: SessionStatus {
                    session_id: SessionHash([6; 32]),
                    step: 3,
                    peers: vec![PeerId([8; 32])],
                    participants: 2,
                    pending_callout: Some(PendingCalloutStatus {
                        pending_id: PendingId::new(11),
                        callout_index: 1,
                        expected_type: Some("Move".into()),
                    }),
                    receipt_available: false,
                },
            },
        };

        let json = serde_json::to_value(&status).unwrap();
        assert!(json.get("host").is_none());
        assert!(json.get("step").is_none());
        assert_eq!(json["state"]["exec_state"], "Active");
        assert_eq!(json["state"]["session"]["step"], 3);
        let mut with_unknown_field = json.clone();
        with_unknown_field["queue_position"] = serde_json::json!(2);
        assert!(serde_json::from_value::<ExecStatus>(with_unknown_field).is_err());
        let mut with_wrong_state_field = json.clone();
        with_wrong_state_field["state"]["queue_position"] = serde_json::json!(2);
        assert!(serde_json::from_value::<ExecStatus>(with_wrong_state_field).is_err());
        assert_eq!(serde_json::from_value::<ExecStatus>(json).unwrap(), status);
    }

    #[test]
    fn response_pending_ids_are_decimal_strings() {
        let pending_id = PendingId::new(u64::MAX);
        let response = ResponseOk::Next(NextEvent::Callout {
            pending_id,
            callout_index: 0,
            name: "Ask".into(),
            prompt: "?".into(),
            schema: arena0_program::JsonSchemaDocument::for_type::<String>(),
            context: serde_json::Value::Null,
        });
        let json = serde_json::to_value(&response).expect("next response JSON");
        assert_eq!(
            json["Next"]["Callout"]["pending_id"],
            pending_id.to_string()
        );
        assert_eq!(
            serde_json::from_value::<ResponseOk>(json).unwrap(),
            response
        );
    }

    #[test]
    fn public_terminal_json_contains_only_agent_values() {
        let terminal = TerminalResult::Completed {
            outcome: Some(serde_json::json!({"winner": "Rock"})),
        };
        let json = serde_json::to_value(terminal).unwrap();
        assert_eq!(json["Completed"]["outcome"]["winner"], "Rock");
        assert!(json["Completed"].get("outcome_raw").is_none());

        let verified = LightVerifiedTerminal::Completed {
            outcome_borsh: vec![0],
        };
        let json = serde_json::to_value(verified).unwrap();
        assert_eq!(json["Completed"]["outcome_borsh"], serde_json::json!([0]));
        assert!(json["Completed"].get("outcome_json").is_none());
        assert!(
            serde_json::from_value::<LightVerifiedTerminal>(serde_json::json!({
                "Completed": {"outcome_borsh": [0], "outcome_json": null}
            }))
            .is_err()
        );

        let verified = FullVerifiedTerminal::Completed {
            outcome_borsh: vec![0],
            outcome_json: serde_json::json!({"winner": "Rock"}),
        };
        let json = serde_json::to_value(verified).unwrap();
        assert_eq!(json["Completed"]["outcome_borsh"], serde_json::json!([0]));
        assert_eq!(json["Completed"]["outcome_json"]["winner"], "Rock");
        assert!(
            serde_json::from_value::<FullVerifiedTerminal>(serde_json::json!({
                "Completed": {"outcome_borsh": [0]}
            }))
            .is_err()
        );

        let verified = VerifiedResult::Light {
            terminal: LightVerifiedTerminal::Completed {
                outcome_borsh: vec![0],
            },
        };
        let json = serde_json::to_value(verified).unwrap();
        assert!(
            json["Light"]["terminal"]["Completed"]
                .get("outcome_json")
                .is_none()
        );

        let response = Ok(ResponseOk::Verified {
            receipt_id: arena0_protocol::ReceiptId::from_bytes([9; 32]),
            program_id: ProgramHash([1; 32]),
            session_id: SessionHash([2; 32]),
            ensemble: vec![PeerId([3; 32])],
            steps: 1,
            result: VerifiedResult::Full {
                terminal: FullVerifiedTerminal::Completed {
                    outcome_borsh: vec![0],
                    outcome_json: serde_json::json!({"ok": true}),
                },
            },
        });
        let encoded = serde_json::to_value(&response).unwrap();
        assert!(
            encoded["Ok"]["Verified"]["result"]["Full"]["terminal"]["Completed"]
                .get("outcome_json")
                .is_some()
        );
        assert_eq!(
            serde_json::from_value::<Response>(encoded).unwrap(),
            response
        );
    }
}
