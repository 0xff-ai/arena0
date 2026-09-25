//! End-to-end proof for one SDK-generated guest and the resident sandbox ABI.

use std::collections::BTreeSet;
use std::path::PathBuf;

use arena0_program::{CallStatus, JsonBytes};
use arena0_protocol::{Committed, Ensemble, Event, PeerId, StateHash};
use arena0_sandbox::{DispatchCall, InitializeCall, Program, ProgramInstance, WasmtimeEngine};
use arena0_test_engine::shared_test_engine;
use wasmparser::{ExternalKind, Parser, Payload};

fn cumulative_sum_wasm() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../programs/target/wasm32-unknown-unknown/release/cumulative_sum.wasm");
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "cannot read required SDK guest {}: {error}; run `just build-programs`",
            path.display()
        )
    })
}

fn exported_guest_shape(wasm: &[u8]) -> (BTreeSet<String>, bool) {
    let mut memories = BTreeSet::new();
    let mut dispatch = false;
    for payload in Parser::new(0).parse_all(wasm) {
        let Payload::ExportSection(exports) = payload.expect("valid Wasm export section") else {
            continue;
        };
        for export in exports {
            let export = export.expect("valid Wasm export");
            if export.kind == ExternalKind::Memory {
                memories.insert(export.name.to_owned());
            }
            if export.name == "arena0_dispatch" {
                assert_eq!(export.kind, ExternalKind::Func);
                dispatch = true;
            }
        }
    }
    (memories, dispatch)
}

/// Borsh-encoded `cumulative_sum::Message::Contribute { value }`.
fn contribution(value: u64) -> Vec<u8> {
    let mut message = vec![0];
    message.extend_from_slice(&value.to_le_bytes());
    message
}

/// One peer's contribution message as an agreed `MessageReceived` event.
fn contribution_event(from: PeerId, value: u64) -> Event<Vec<u8>> {
    Event::MessageReceived {
        from,
        msg: contribution(value),
    }
}

fn resident_from_cumulative_guest(
    engine: &WasmtimeEngine,
) -> (ProgramInstance, Ensemble<Committed>) {
    resident_from_cumulative_guest_with_target_size(engine, 2)
}

fn resident_from_cumulative_guest_with_target_size(
    engine: &WasmtimeEngine,
    target_size: u32,
) -> (ProgramInstance, Ensemble<Committed>) {
    let program = Program::try_from(cumulative_sum_wasm()).expect("parse cumulative-sum guest");
    let loaded = engine.load(&program).expect("load cumulative-sum guest");
    let params = format!(r#"{{"target_size":{target_size}}}"#);
    let initialized = loaded
        .initialize(InitializeCall::new(
            JsonBytes::try_new(params.into_bytes()).expect("valid params JSON"),
        ))
        .expect("initialize cumulative-sum guest");
    let session = Ensemble::from_peers(
        (0..target_size)
            .map(|index| PeerId([index as u8; 32]))
            .collect(),
    )
    .expect("valid committed session");
    let resident = loaded
        .resident(initialized.shared, initialized.local)
        .expect("create resident cumulative-sum guest");
    (resident, session)
}

#[test]
fn sdk_guest_dispatch_has_exact_memories_and_rolls_back_rejected_state() {
    let wasm = cumulative_sum_wasm();
    let (memories, has_dispatch) = exported_guest_shape(&wasm);
    assert_eq!(
        memories,
        ["arena0_local", "arena0_shared", "memory"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
    assert!(has_dispatch, "SDK guest must export arena0_dispatch");

    let engine = shared_test_engine();
    let (mut resident, session) = resident_from_cumulative_guest(&engine);
    let peer0 = PeerId([0; 32]);

    let started = resident
        .dispatch(DispatchCall::new(
            peer0,
            session.clone(),
            Event::SessionStarted {
                ensemble: session.clone(),
            },
        ))
        .expect("SessionStarted dispatch");
    assert_eq!(started.status, CallStatus::Accepted);
    resident
        .commit_payloads()
        .expect("commit SessionStarted state");
    let (before_shared, before_local) = resident.committed_payloads();
    let before_shared = before_shared.clone();
    let before_local = before_local.clone();
    let before_hash = StateHash::of_shared(&before_shared).0;

    // The author applies its own queued contribution through the same
    // `on_message` dispatch every receiver runs.
    let accepted = resident
        .dispatch(DispatchCall::new(
            peer0,
            session.clone(),
            contribution_event(peer0, 111),
        ))
        .expect("own contribution dispatch");
    assert_eq!(accepted.status, CallStatus::Accepted);
    assert_ne!(accepted.shared, before_shared);
    assert_eq!(accepted.local, before_local);
    assert_ne!(accepted.shared_hash, before_hash);
    let (accepted_shared, accepted_local) = resident
        .commit_payloads()
        .expect("commit accepted contribution state");
    assert_eq!(accepted_shared, accepted.shared);
    assert_eq!(accepted_local, accepted.local);

    // Participant 0's slot is filled, so a message from that same participant
    // is a validly encoded but deterministic wrong-writer reject. Its payload
    // is deliberately different from the committed state so the result proves
    // the resident checkpoint, not input equality, is restored.
    let rejected = resident
        .dispatch(DispatchCall::new(
            peer0,
            session,
            contribution_event(peer0, 999),
        ))
        .expect("wrong-writer dispatch should return rejected status");
    assert_eq!(rejected.status, CallStatus::Rejected);
    assert_eq!(rejected.shared, accepted_shared);
    assert_eq!(rejected.local, accepted_local);
    assert_eq!(rejected.shared_hash, accepted.shared_hash);
    assert_eq!(
        resident.committed_payloads(),
        (&accepted_shared, &accepted_local)
    );
}

#[test]
fn sdk_guest_repeated_allocations_preserve_commit_restore_and_rollback() {
    // The largest legal cumulative-sum roster makes every generated dispatch
    // deserialize and reserialize a non-trivial Vec-backed shared state. Run
    // the same accepted event repeatedly so the resident allocator is reused,
    // then exercise both rejection and a guest decode trap against the latest
    // committed checkpoint.
    let engine = shared_test_engine();
    let (mut resident, session) = resident_from_cumulative_guest_with_target_size(&engine, 64);
    let peer0 = PeerId([0; 32]);

    resident
        .dispatch(DispatchCall::new(
            peer0,
            session.clone(),
            Event::SessionStarted {
                ensemble: session.clone(),
            },
        ))
        .expect("SessionStarted dispatch");
    resident
        .commit_payloads()
        .expect("commit SessionStarted state");

    for index in 0..8u8 {
        let peer = PeerId([index; 32]);
        let accepted = resident
            .dispatch(DispatchCall::new(
                peer,
                session.clone(),
                contribution_event(peer, u64::from(index) + 1),
            ))
            .expect("repeated contribution dispatch");
        assert_eq!(accepted.status, CallStatus::Accepted);
        resident
            .commit_payloads()
            .expect("commit repeated contribution state");
    }
    let (committed_shared, committed_local) = resident.committed_payloads();
    let committed_shared = committed_shared.clone();
    let committed_local = committed_local.clone();
    let committed_hash = StateHash::of_shared(&committed_shared).0;

    // An accepted candidate is discarded by explicit recovery before the
    // subsequent failure, proving that the durable checkpoint remains the
    // source of rollback state.
    let candidate_peer = PeerId([8; 32]);
    let candidate = resident
        .dispatch(DispatchCall::new(
            candidate_peer,
            session.clone(),
            contribution_event(candidate_peer, 8),
        ))
        .expect("candidate contribution dispatch");
    assert_eq!(candidate.status, CallStatus::Accepted);
    resident
        .restore_committed()
        .expect("restore committed state");

    // Participant 0 has already contributed, so this valid message is a
    // deterministic wrong-writer rejection. Its payload differs from the
    // checkpoint and therefore proves restoration rather than input equality.
    let message = contribution(999);
    let rejected = resident
        .dispatch(DispatchCall::new(
            peer0,
            session.clone(),
            contribution_event(peer0, 999),
        ))
        .expect("wrong-writer dispatch should reject");
    assert_eq!(rejected.status, CallStatus::Rejected);
    assert_eq!(rejected.shared, committed_shared);
    assert_eq!(rejected.local, committed_local);
    assert_eq!(rejected.shared_hash, committed_hash);

    // Malformed generated-event bytes trap before the handler can commit; the
    // resident must still expose exactly the same checkpoint afterward.
    let fault = resident.dispatch(DispatchCall::new(
        peer0,
        session.clone(),
        Event::MessageReceived {
            from: peer0,
            msg: vec![255],
        },
    ));
    assert!(fault.is_err(), "malformed guest message must trap");
    assert_eq!(
        resident.committed_payloads(),
        (&committed_shared, &committed_local)
    );
    let after_fault = resident
        .dispatch(DispatchCall::new(
            peer0,
            session,
            Event::MessageReceived {
                from: peer0,
                msg: message,
            },
        ))
        .expect("dispatch after trap should observe the restored checkpoint");
    assert_eq!(after_fault.status, CallStatus::Rejected);
    assert_eq!(after_fault.shared, committed_shared);
    assert_eq!(after_fault.local, committed_local);
    assert_eq!(after_fault.shared_hash, committed_hash);
}

fn local_context_forge_wasm() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../programs/target/wasm32-unknown-unknown/release/local_context_forge.wasm");
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "cannot read required SDK guest {}: {error}; run `just build-programs`",
            path.display()
        )
    })
}

/// A local handler that forges a replacement context must be rejected by the
/// generated glue: the forged shared view never reaches `callout`, and neither
/// image is stored.
#[test]
fn sdk_guest_rejects_a_local_handler_that_forges_its_shared_view() {
    let engine = shared_test_engine();
    let program = Program::try_from(local_context_forge_wasm()).expect("parse forge guest");
    let loaded = engine.load(&program).expect("load forge guest");
    let initialized = loaded
        .initialize(InitializeCall::new(
            JsonBytes::try_new(b"null".to_vec()).expect("valid params JSON"),
        ))
        .expect("initialize forge guest");
    let session = Ensemble::from_peers(vec![PeerId([0; 32]), PeerId([1; 32])])
        .expect("valid committed session");
    let mut resident = loaded
        .resident(initialized.shared, initialized.local)
        .expect("create resident forge guest");
    let peer0 = PeerId([0; 32]);

    let started = resident
        .dispatch(DispatchCall::new(
            peer0,
            session.clone(),
            Event::SessionStarted {
                ensemble: session.clone(),
            },
        ))
        .expect("SessionStarted dispatch");
    assert_eq!(started.status, CallStatus::Accepted);
    // The committed marker is zero, so the fixture opens no callout.
    assert!(started.callout.is_none());
    resident
        .commit_payloads()
        .expect("commit SessionStarted state");
    let (before_shared, before_local) = resident.committed_payloads();
    let before_shared = before_shared.clone();
    let before_local = before_local.clone();

    let rejected = resident
        .dispatch(DispatchCall::new(
            peer0,
            session,
            Event::TimerFired {
                timer: arena0_protocol::TimerPayload::unit(),
            },
        ))
        .expect("timer dispatch");
    assert_eq!(
        rejected.status,
        CallStatus::Rejected,
        "the forged shared view must be rejected"
    );
    assert_eq!(rejected.shared, before_shared);
    assert_eq!(rejected.local, before_local);
    // The forged marker would open a callout; the glue must derive none.
    assert!(
        rejected.callout.is_none(),
        "no callout may be derived from the forged shared image"
    );
    assert_eq!(
        resident.committed_payloads(),
        (&before_shared, &before_local)
    );
}
