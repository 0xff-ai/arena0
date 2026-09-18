//! End-to-end proof for one SDK-generated guest and the resident sandbox ABI.

use std::collections::BTreeSet;
use std::path::PathBuf;

use arena0_program::{CallStatus, JsonBytes};
use arena0_protocol::{Committed, Ensemble, Event, MessageId, PeerId, StateHash};
use arena0_sandbox::{DispatchCall, InitializeCall, Program, ProgramInstance, WasmtimeEngine};
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

    let engine = WasmtimeEngine::new().expect("sandbox engine");
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
    let (before_react_shared, before_react_local) = resident.committed_payloads();
    let before_react_shared = before_react_shared.clone();
    let before_react_local = before_react_local.clone();
    let before_react_hash = StateHash::of_shared(&before_react_shared).0;

    let accepted = resident
        .dispatch(DispatchCall::new(peer0, session.clone(), Event::React))
        .expect("React dispatch");
    assert_eq!(accepted.status, CallStatus::Accepted);
    assert_ne!(accepted.shared, before_react_shared);
    assert_ne!(accepted.local, before_react_local);
    assert_ne!(accepted.shared_hash, before_react_hash);
    assert!(
        accepted
            .observations
            .effects
            .iter()
            .any(|effect| matches!(effect, arena0_protocol::Effect::Broadcast { .. })),
        "React must emit the contribution broadcast"
    );
    let (accepted_shared, accepted_local) = resident
        .commit_payloads()
        .expect("commit accepted React state");
    assert_eq!(accepted_shared, accepted.shared);
    assert_eq!(accepted_local, accepted.local);

    // The first React fills participant 0's slot, so a message from that same
    // participant is a validly encoded but deterministic wrong-writer reject.
    // Its payload is deliberately different from the committed state so the
    // result proves the resident checkpoint, not input equality, is restored.
    let mut message = vec![0];
    message.extend_from_slice(&999u64.to_le_bytes());
    let rejected = resident
        .dispatch(DispatchCall::new(
            peer0,
            session,
            Event::MessageReceived {
                message_id: MessageId([9; 32]),
                from: peer0,
                position: 1,
                pre_state: StateHash([8; 32]),
                msg: message,
            },
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
    let engine = WasmtimeEngine::new().expect("sandbox engine");
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

    for _ in 0..8 {
        let accepted = resident
            .dispatch(DispatchCall::new(peer0, session.clone(), Event::React))
            .expect("repeated React dispatch");
        assert_eq!(accepted.status, CallStatus::Accepted);
        resident
            .commit_payloads()
            .expect("commit repeated React state");
    }
    let (committed_shared, committed_local) = resident.committed_payloads();
    let committed_shared = committed_shared.clone();
    let committed_local = committed_local.clone();
    let committed_hash = StateHash::of_shared(&committed_shared).0;

    // An accepted candidate is discarded by explicit recovery before the
    // subsequent failure, proving that the durable checkpoint remains the
    // source of rollback state.
    let candidate = resident
        .dispatch(DispatchCall::new(peer0, session.clone(), Event::React))
        .expect("candidate React dispatch");
    assert_eq!(candidate.status, CallStatus::Accepted);
    resident
        .restore_committed()
        .expect("restore committed state");

    // Participant 0 has already contributed, so this valid message is a
    // deterministic wrong-writer rejection. Its payload differs from the
    // checkpoint and therefore proves restoration rather than input equality.
    let mut message = vec![0];
    message.extend_from_slice(&999u64.to_le_bytes());
    let rejected = resident
        .dispatch(DispatchCall::new(
            peer0,
            session.clone(),
            Event::MessageReceived {
                message_id: MessageId([9; 32]),
                from: peer0,
                position: 1,
                pre_state: StateHash([8; 32]),
                msg: message.clone(),
            },
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
            message_id: MessageId([10; 32]),
            from: peer0,
            position: 1,
            pre_state: StateHash([8; 32]),
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
                message_id: MessageId([11; 32]),
                from: peer0,
                position: 1,
                pre_state: StateHash([8; 32]),
                msg: message,
            },
        ))
        .expect("dispatch after trap should observe the restored checkpoint");
    assert_eq!(after_fault.status, CallStatus::Rejected);
    assert_eq!(after_fault.shared, committed_shared);
    assert_eq!(after_fault.local, committed_local);
    assert_eq!(after_fault.shared_hash, committed_hash);
}
