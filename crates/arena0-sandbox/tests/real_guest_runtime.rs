//! End-to-end proof for one SDK-generated guest and the resident sandbox ABI.

use std::collections::BTreeSet;
use std::path::PathBuf;

use arena0_program::{CallStatus, JsonBytes, LocalStateBytes, SharedStateBytes};
use arena0_protocol::{Committed, Ensemble, Event, PeerId, StateHash};
use arena0_sandbox::{DispatchCall, DispatchCallResult, Program, ProgramInstance, WasmtimeEngine};

/// Borrow one accepted dispatch's images. Accepted results always carry them;
/// rejected results carry none, so rejection clones nothing.
fn accepted_images(result: &DispatchCallResult) -> (&SharedStateBytes, &LocalStateBytes) {
    assert_eq!(result.status, CallStatus::Accepted);
    (
        result.shared.as_ref().expect("accepted shared image"),
        result.local.as_ref().expect("accepted local image"),
    )
}

/// A rejected dispatch carries only its reason and no images.
fn assert_no_images(result: &DispatchCallResult) {
    assert_eq!(result.status, CallStatus::Rejected);
    assert!(result.shared.is_none(), "rejection carries no shared image");
    assert!(result.local.is_none(), "rejection carries no local image");
}
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
        .initialize(JsonBytes::try_new(params.into_bytes()).expect("valid params JSON"))
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
    resident.commit().expect("commit SessionStarted state");
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
    let (accepted_shared_image, accepted_local_image) = accepted_images(&accepted);
    assert_ne!(accepted_shared_image, &before_shared);
    assert_eq!(accepted_local_image, &before_local);
    assert_ne!(StateHash::of_shared(accepted_shared_image).0, before_hash);
    resident
        .commit()
        .expect("commit accepted contribution state");
    let (accepted_shared, accepted_local) = resident.committed_payloads();
    let accepted_shared = accepted_shared.clone();
    let accepted_local = accepted_local.clone();
    assert_eq!(&accepted_shared, accepted_shared_image);
    assert_eq!(&accepted_local, accepted_local_image);

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
    assert_no_images(&rejected);
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
    resident.commit().expect("commit SessionStarted state");

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
            .commit()
            .expect("commit repeated contribution state");
    }
    let (committed_shared, committed_local) = resident.committed_payloads();
    let committed_shared = committed_shared.clone();
    let committed_local = committed_local.clone();

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
    assert_no_images(&rejected);

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
    assert_no_images(&after_fault);
}

fn timer_dispatch_wasm() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../programs/target/wasm32-unknown-unknown/release/timer_dispatch.wasm");
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "cannot read required SDK guest {}: {error}; run `just build-programs`",
            path.display()
        )
    })
}

/// A local handler that forges a replacement context must be rejected by the
/// generated glue: the forged shared view never reaches `callout`, and neither
/// image is stored. The `timer-dispatch` fixture forges on `Timer::Forge`.
#[test]
fn sdk_guest_rejects_a_local_handler_that_forges_its_shared_view() {
    let engine = shared_test_engine();
    let program = Program::try_from(timer_dispatch_wasm()).expect("parse forge guest");
    let loaded = engine.load(&program).expect("load forge guest");
    let initialized = loaded
        .initialize(JsonBytes::try_new(b"null".to_vec()).expect("valid params JSON"))
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
    resident.commit().expect("commit SessionStarted state");
    let (before_shared, before_local) = resident.committed_payloads();
    let before_shared = before_shared.clone();
    let before_local = before_local.clone();

    let rejected = resident
        .dispatch(DispatchCall::new(
            peer0,
            session,
            Event::TimerFired {
                // The fixture's `Timer::Forge`: the SDK names the timer type
                // and Borsh-encodes the variant index.
                timer: arena0_protocol::TimerPayload {
                    type_name: "timer_dispatch::Timer".to_owned(),
                    data: vec![1],
                },
            },
        ))
        .expect("timer dispatch");
    assert_eq!(
        rejected.status,
        CallStatus::Rejected,
        "the forged shared view must be rejected"
    );
    assert_no_images(&rejected);
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
