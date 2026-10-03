//! Capability-gated host function registration (messaging, timers, sign, and blobs).

use crate::call::DispatchKind;
use arena0_crypto::SignScheme;
use arena0_program::Capability;
use arena0_program::abi::{self, imports};
use arena0_protocol::execution::{BlobChange, MAX_DIRECT_QUEUE, MAX_OUTGOING_MESSAGES};
use arena0_protocol::{
    BlobError, BlobHash, CvSource, MAX_BLOB_BYTES, MAX_DIRECT_CONTROL_BYTES,
    MAX_DIRECT_RANGE_BYTES, PeerId, RangeAttachment,
};
use arena0_protocol::{Effect, TimerPayload};
use std::sync::Arc;
use wasmtime::{Caller, Linker};

use super::{CallerExt as _, u32_to_sign_scheme};
use crate::engine::HostState;
use crate::{BlobView, SandboxError};

/// Register host functions for each declared capability into the linker.
pub(crate) fn register_capability_imports(
    linker: &mut Linker<HostState>,
    capabilities: &[Capability],
) -> Result<(), SandboxError> {
    let mut sign_schemes = Vec::new();
    for capability in capabilities {
        match capability {
            Capability::Messaging => register_messaging(linker)?,
            Capability::Timers => register_timers(linker)?,
            Capability::Sign { schemes } => sign_schemes.extend(schemes.iter().copied()),
            Capability::Blobs => register_blobs(linker)?,
        }
    }
    if !sign_schemes.is_empty() {
        sign_schemes = normalize_sign_schemes(sign_schemes);
        register_sign(linker, sign_schemes)?;
    }

    Ok(())
}

fn normalize_sign_schemes(mut sign_schemes: Vec<SignScheme>) -> Vec<SignScheme> {
    sign_schemes.sort_by_key(|scheme| scheme.canonical_order());
    sign_schemes.dedup();
    sign_schemes
}

fn map_err(e: wasmtime::Error) -> SandboxError {
    SandboxError::instantiation_failed(e.to_string())
}

fn register_messaging(linker: &mut Linker<HostState>) -> Result<(), SandboxError> {
    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::SEND_DIRECT,
            |mut caller: Caller<'_, HostState>,
             to_ptr: u32,
             msg_ptr: u32,
             msg_len: u32,
             range_ptr: u32,
             range_len: u32|
             -> Result<u32, wasmtime::Error> {
                caller.begin_import(imports::SEND_DIRECT)?;
                require_local(&caller, imports::SEND_DIRECT)?;
                let to = PeerId(
                    caller
                        .read_guest_bytes(to_ptr, 32, imports::SEND_DIRECT)?
                        .try_into()
                        .expect("peer width"),
                );
                let state = caller.data();
                let session = state
                    .scope
                    .session
                    .as_ref()
                    .expect("dispatch session installed");
                if Some(to) == state.scope.peer_id || !session.peers().contains(&to) {
                    return Err(wasmtime::Error::msg(
                        "send_direct: recipient must be another participant",
                    ));
                }
                if msg_len as usize > MAX_DIRECT_CONTROL_BYTES {
                    return Err(wasmtime::Error::msg(
                        "send_direct: control message exceeds limit",
                    ));
                }
                let range = if range_len == 0 {
                    None
                } else {
                    let bytes =
                        caller.read_guest_bytes(range_ptr, range_len, imports::SEND_DIRECT)?;
                    let range: RangeAttachment =
                        borsh::from_slice(&bytes).map_err(wasmtime::Error::new)?;
                    let length = caller
                        .data()
                        .scope
                        .blob_view()?
                        .granted(range.hash)
                        .map_err(wasmtime::Error::msg)?
                        .ok_or_else(|| wasmtime::Error::msg("send_direct: blob not granted"))?;
                    if range.start >= range.end
                        || range.end > length
                        || range.end - range.start > MAX_DIRECT_RANGE_BYTES
                    {
                        return Err(wasmtime::Error::msg("send_direct: invalid source range"));
                    }
                    Some(range)
                };
                let queued = caller
                    .data()
                    .scope
                    .direct_queued
                    .iter()
                    .find(|(peer, _)| *peer == to)
                    .map_or(0, |(_, len)| *len);
                let here = caller
                    .data()
                    .scope
                    .effect_queue
                    .iter()
                    .filter(|e| matches!(e, Effect::SendDirect { to: peer, .. } if *peer == to))
                    .count();
                if queued + here >= MAX_DIRECT_QUEUE {
                    return Ok(1);
                }
                let msg = caller.read_guest_bytes(msg_ptr, msg_len, imports::SEND_DIRECT)?;
                caller.record_effect(Effect::SendDirect { to, msg, range })?;
                Ok(0)
            },
        )
        .map_err(map_err)?;
    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::BROADCAST,
            |mut caller: Caller<'_, HostState>,
             data_ptr: u32,
             data_len: u32|
             -> Result<u32, wasmtime::Error> {
                caller.begin_import(imports::BROADCAST)?;
                caller.reject_read_only(imports::BROADCAST)?;
                // The queue is local, so an agreed handler never observes it:
                // in an agreed dispatch the call always queues and reports
                // success. Only a local handler sees the bound.
                if caller.data().dispatch == crate::call::DispatchKind::Local {
                    let queued_before = caller.data().outgoing_len;
                    let queued_here = caller
                        .data()
                        .scope
                        .effect_queue
                        .iter()
                        .filter(|effect| matches!(effect, Effect::Broadcast { .. }))
                        .count();
                    if queued_before + queued_here >= MAX_OUTGOING_MESSAGES {
                        return Ok(1);
                    }
                }
                let data = caller.read_guest_bytes(data_ptr, data_len, "broadcast:data")?;
                caller.record_effect(Effect::Broadcast { data })?;
                Ok(0)
            },
        )
        .map_err(map_err)?;
    Ok(())
}

fn require_local(caller: &Caller<'_, HostState>, name: &str) -> Result<(), wasmtime::Error> {
    caller.reject_read_only(name)?;
    if caller.data().dispatch != DispatchKind::Local {
        return Err(wasmtime::Error::msg(format!(
            "{name}: only available in local handlers"
        )));
    }
    Ok(())
}

fn blob_status(error: BlobError) -> u32 {
    u32::from(borsh::to_vec(&error).expect("blob error encoding")[0]) + 1
}

fn read_blob_hash(
    caller: &mut Caller<'_, HostState>,
    ptr: u32,
    name: &str,
) -> Result<BlobHash, wasmtime::Error> {
    Ok(BlobHash(
        caller
            .read_guest_bytes(ptr, 32, name)?
            .try_into()
            .expect("hash width"),
    ))
}

/// The receive state of `hash` as this dispatch sees it: the view's partial
/// overlaid with this dispatch's staged changes.
struct Receive {
    /// Declared length, from the partial or the first staged append.
    length: Option<u64>,
    /// Durable written bytes plus staged appended bytes.
    written: u64,
    /// Whether the partial is committed, or a `Commit` is staged.
    committed: bool,
    /// Whether the view has a partial row (so `hash_partial` applies).
    durable: bool,
}

/// The attachment stays readable for the entire dispatch; staging owns a copy.
fn blob_append(
    caller: &mut Caller<'_, HostState>,
    hash_ptr: u32,
    length: u64,
    attachment: u32,
) -> Result<u32, wasmtime::Error> {
    let hash = read_blob_hash(caller, hash_ptr, imports::BLOB_APPEND)?;
    if length > MAX_BLOB_BYTES {
        return Ok(blob_status(BlobError::Quota));
    }
    let Some(bytes) = caller
        .data()
        .scope
        .attachment
        .as_ref()
        .filter(|b| attachment == 0 && !b.is_empty())
    else {
        return Ok(blob_status(BlobError::BadAttachment));
    };
    let len = bytes.len();
    let receive = caller.data().scope.receive_state(hash)?;
    if receive.committed
        || receive.length.is_some_and(|known| known != length)
        || receive.written + len as u64 > length
    {
        return Ok(blob_status(BlobError::BadRange));
    }
    let max = caller.data().profile.limits.max_host_bytes;
    caller
        .data_mut()
        .scope
        .ledger
        .copy_bytes(len, max)
        .map_err(wasmtime::Error::new)?;
    let bytes = caller
        .data()
        .scope
        .attachment
        .as_ref()
        .expect("attachment checked")
        .clone();
    caller
        .data_mut()
        .scope
        .staged_blobs
        .push(BlobChange::Append {
            hash,
            length,
            offset: receive.written,
            bytes,
        });
    Ok(0)
}

/// Hash only after completeness and fuel checks, leaving mismatches unstaged.
fn blob_commit(caller: &mut Caller<'_, HostState>, hash_ptr: u32) -> Result<u32, wasmtime::Error> {
    let hash = read_blob_hash(caller, hash_ptr, imports::BLOB_COMMIT)?;
    let receive = caller.data().scope.receive_state(hash)?;
    let Some(length) = receive.length else {
        return Ok(blob_status(BlobError::NotFound));
    };
    if receive.committed {
        return Ok(blob_status(BlobError::BadRange));
    }
    if receive.written != length {
        return Ok(blob_status(BlobError::Incomplete));
    }
    caller.charge_fuel(
        length * caller.data().profile.fuel.hash_per_byte,
        imports::BLOB_COMMIT,
    )?;
    let tail = caller.data().scope.staged_tail(hash);
    let digest = if receive.durable {
        caller
            .data()
            .scope
            .blob_view()?
            .hash_partial(hash, &tail)
            .map_err(wasmtime::Error::msg)?
    } else {
        BlobHash(arena0_crypto::hash(
            arena0_crypto::HashAlgorithm::Blake3,
            &tail,
        ))
    };
    if digest != hash {
        return Ok(blob_status(BlobError::Mismatch));
    }
    caller
        .data_mut()
        .scope
        .staged_blobs
        .push(BlobChange::Commit { hash });
    Ok(0)
}

/// Validate subtree geometry and charge before reading the granted bytes.
fn subtree_cv(
    caller: &mut Caller<'_, HostState>,
    source_ptr: u32,
    source_len: u32,
    offset: u64,
    out_ptr: u32,
) -> Result<u32, wasmtime::Error> {
    if source_len > 64 {
        return Err(wasmtime::Error::msg("subtree_cv: source exceeds limit"));
    }
    let source: CvSource =
        borsh::from_slice(&caller.read_guest_bytes(source_ptr, source_len, imports::SUBTREE_CV)?)
            .map_err(wasmtime::Error::new)?;
    let view = caller.data().scope.blob_view()?;
    let len = match source {
        CvSource::Blob { hash, start, end } => {
            if start >= end || end - start > MAX_DIRECT_RANGE_BYTES {
                return Ok(blob_status(BlobError::BadRange));
            }
            let Some(length) = view.granted(hash).map_err(wasmtime::Error::msg)? else {
                return Ok(blob_status(BlobError::NotFound));
            };
            if end > length {
                return Ok(blob_status(BlobError::BadRange));
            }
            end - start
        }
        CvSource::Attachment(token) => {
            let Some(bytes) = caller
                .data()
                .scope
                .attachment
                .as_ref()
                .filter(|b| token.0 == 0 && !b.is_empty())
            else {
                return Ok(blob_status(BlobError::BadAttachment));
            };
            let len = bytes.len() as u64;
            if len > MAX_DIRECT_RANGE_BYTES {
                return Ok(blob_status(BlobError::BadRange));
            }
            len
        }
    };
    if !arena0_crypto::blake3_tree::is_subtree(offset, len) {
        return Ok(blob_status(BlobError::BadRange));
    }
    caller.charge_fuel(
        len * caller.data().profile.fuel.hash_per_byte,
        imports::SUBTREE_CV,
    )?;
    let max = caller.data().profile.limits.max_host_bytes;
    caller
        .data_mut()
        .scope
        .ledger
        .copy_bytes(32, max)
        .map_err(wasmtime::Error::new)?;
    let blob_bytes;
    let bytes = match source {
        CvSource::Blob { hash, start, end } => {
            let Some(bytes) = view
                .read_granted(hash, start..end)
                .map_err(wasmtime::Error::msg)?
            else {
                return Ok(blob_status(BlobError::NotFound));
            };
            blob_bytes = bytes;
            blob_bytes.as_slice()
        }
        CvSource::Attachment(_) => caller
            .data()
            .scope
            .attachment
            .as_deref()
            .expect("attachment checked"),
    };
    let cv = arena0_crypto::blake3_tree::subtree_cv(bytes, offset);
    caller
        .work_memory()?
        .write(&mut *caller, out_ptr as usize, &cv)
        .map_err(wasmtime::Error::new)?;
    Ok(0)
}

fn register_blobs(linker: &mut Linker<HostState>) -> Result<(), SandboxError> {
    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::BLOB_APPEND,
            |mut caller: Caller<'_, HostState>,
             hash_ptr: u32,
             length: u64,
             attachment: u32|
             -> Result<u32, wasmtime::Error> {
                caller.begin_import(imports::BLOB_APPEND)?;
                require_local(&caller, imports::BLOB_APPEND)?;
                blob_append(&mut caller, hash_ptr, length, attachment)
            },
        )
        .map_err(map_err)?;
    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::BLOB_COMMIT,
            |mut caller: Caller<'_, HostState>, hash_ptr: u32| -> Result<u32, wasmtime::Error> {
                caller.begin_import(imports::BLOB_COMMIT)?;
                require_local(&caller, imports::BLOB_COMMIT)?;
                blob_commit(&mut caller, hash_ptr)
            },
        )
        .map_err(map_err)?;
    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::SUBTREE_CV,
            |mut caller: Caller<'_, HostState>,
             source_ptr: u32,
             source_len: u32,
             offset: u64,
             out_ptr: u32|
             -> Result<u32, wasmtime::Error> {
                caller.begin_import(imports::SUBTREE_CV)?;
                require_local(&caller, imports::SUBTREE_CV)?;
                subtree_cv(&mut caller, source_ptr, source_len, offset, out_ptr)
            },
        )
        .map_err(map_err)?;
    Ok(())
}

fn register_timers(linker: &mut Linker<HostState>) -> Result<(), SandboxError> {
    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::SET_TIMER,
            |mut caller: Caller<'_, HostState>,
             delay_ms: u64,
             type_ptr: u32,
             type_len: u32,
             data_ptr: u32,
             data_len: u32| {
                caller.begin_import(imports::SET_TIMER)?;
                caller.reject_read_only(imports::SET_TIMER)?;
                let type_bytes = caller.read_guest_bytes(type_ptr, type_len, "set_timer:type")?;
                let type_name = String::from_utf8(type_bytes).map_err(|e| {
                    wasmtime::Error::msg(format!("set_timer: invalid type name: {e}"))
                })?;
                let data = caller.read_guest_bytes(data_ptr, data_len, "set_timer:data")?;
                caller.record_effect(Effect::SetTimer {
                    delay_ms,
                    timer: TimerPayload { type_name, data },
                })
            },
        )
        .map_err(map_err)?;
    Ok(())
}

fn register_sign(
    linker: &mut Linker<HostState>,
    allowed_schemes: Vec<SignScheme>,
) -> Result<(), SandboxError> {
    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::SIGN,
            move |mut caller: Caller<'_, HostState>,
                  scheme: u32,
                  data_ptr: u32,
                  data_len: u32,
                  out_ptr: u32,
                  out_cap: u32|
                  -> Result<u32, wasmtime::Error> {
                caller.begin_import(imports::SIGN)?;
                caller.reject_read_only(imports::SIGN)?;
                let scheme = u32_to_sign_scheme(scheme)?;
                reject_if_sign_scheme_disallowed(imports::SIGN, scheme, &allowed_schemes)?;
                let payload = caller.read_guest_bytes(data_ptr, data_len, imports::SIGN)?;
                let Some(signer) = caller.data().scope.signer.signer() else {
                    return Err(wasmtime::Error::msg(
                        "sign is only available in local handlers",
                    ));
                };
                let call_index = caller.data_mut().scope.signer.next_call();
                let (signed_bytes, signature) = signer
                    .sign(call_index, scheme, &payload)
                    .map_err(|error| wasmtime::Error::msg(format!("sign: {error}")))?;
                let encoded = borsh::to_vec(&(signed_bytes, signature))
                    .map_err(|error| wasmtime::Error::msg(format!("sign: encode: {error}")))?;
                if encoded.len() > out_cap as usize {
                    return Err(wasmtime::Error::msg(format!(
                        "sign: result is {} bytes; guest capacity is {out_cap}",
                        encoded.len()
                    )));
                }
                let max_host_bytes = caller.data().profile.limits.max_host_bytes;
                caller
                    .data_mut()
                    .scope
                    .ledger
                    .copy_bytes(encoded.len(), max_host_bytes)
                    .map_err(wasmtime::Error::new)?;
                let work = caller.work_memory()?;
                work.write(&mut caller, out_ptr as usize, &encoded)
                    .map_err(|error| wasmtime::Error::msg(format!("sign: {error}")))?;
                u32::try_from(encoded.len())
                    .map_err(|_| wasmtime::Error::msg("sign: result length exceeds u32"))
            },
        )
        .map_err(map_err)?;
    Ok(())
}

fn reject_if_sign_scheme_disallowed(
    function_name: &str,
    scheme: SignScheme,
    allowed: &[SignScheme],
) -> Result<(), wasmtime::Error> {
    if allowed.contains(&scheme) {
        Ok(())
    } else {
        Err(wasmtime::Error::msg(format!(
            "{function_name}: sign scheme {scheme:?} is not declared"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::CallKind;
    use wasmtime::{Engine, Linker, Module, Store};

    fn instantiate_timer_test_module(
        dispatch: crate::call::DispatchKind,
    ) -> (Store<HostState>, wasmtime::TypedFunc<(), ()>) {
        let engine = Engine::default();
        let module = Module::new(
            &engine,
            r#"
                (module
                  (import "arena0" "set_timer"
                    (func $set_timer (param i64 i32 i32 i32 i32)))
                  (memory (export "memory") 1)
                  (data (i32.const 0) "Timer")
                  (data (i32.const 8) "abc")
                  (func (export "call_set_timer")
                    i64.const 25
                    i32.const 0
                    i32.const 5
                    i32.const 8
                    i32.const 3
                    call $set_timer))
            "#,
        )
        .unwrap();

        let mut linker = Linker::new(&engine);
        register_capability_imports(&mut linker, &[Capability::Timers]).unwrap();

        let mut store = Store::new(&engine, {
            let mut hs = HostState::new(
                arena0_program::ExecutionProfile::current(),
                CallKind::Dispatch,
                crate::call::DispatchKind::Local,
                Vec::new(),
            );
            hs.dispatch = dispatch;
            hs
        });
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let set_timer = instance
            .get_typed_func::<(), ()>(&mut store, "call_set_timer")
            .unwrap();
        (store, set_timer)
    }

    fn instantiate_sign_test_module(
        dispatch: crate::call::DispatchKind,
        scheme: u32,
        declared_schemes: Vec<SignScheme>,
        with_signer: bool,
    ) -> (
        Store<HostState>,
        wasmtime::TypedFunc<(), i32>,
        wasmtime::Memory,
    ) {
        let engine = Engine::default();
        let wat = format!(
            r#"
                (module
                  (import "arena0" "sign" (func $sign (param i32 i32 i32 i32 i32) (result i32)))
                  (memory (export "memory") 1)
                  (data (i32.const 64) "payload")
                  (func (export "call_sign") (result i32)
                    i32.const {scheme}
                    i32.const 64
                    i32.const 7
                    i32.const 0
                    i32.const 512
                    call $sign))
            "#
        );
        let module = Module::new(&engine, wat).unwrap();

        let mut linker = Linker::new(&engine);
        register_capability_imports(
            &mut linker,
            &[Capability::Sign {
                schemes: declared_schemes,
            }],
        )
        .unwrap();

        let mut store = Store::new(&engine, {
            let mut hs = HostState::new(
                arena0_program::ExecutionProfile::current(),
                CallKind::Dispatch,
                crate::call::DispatchKind::Local,
                Vec::new(),
            );
            hs.dispatch = dispatch;
            if with_signer {
                hs.scope
                    .signer
                    .install(Some(std::sync::Arc::new(TestSigner)));
            }
            hs
        });
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let sign = instance
            .get_typed_func::<(), i32>(&mut store, "call_sign")
            .unwrap();
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        (store, sign, memory)
    }

    /// Deterministic stand-in for the host signer: the "signed bytes" are the
    /// call ordinal followed by the payload, and the signature repeats the
    /// ordinal so the round trip is visible without a real key.
    struct TestSigner;

    impl crate::GuestSigner for TestSigner {
        fn sign(
            &self,
            call_index: u32,
            _scheme: SignScheme,
            payload: &[u8],
        ) -> Result<(Vec<u8>, Vec<u8>), String> {
            let mut signed = call_index.to_le_bytes().to_vec();
            signed.extend_from_slice(payload);
            Ok((signed, vec![u8::try_from(call_index).unwrap_or(0); 64]))
        }
    }

    #[test]
    fn set_timer_allowed_during_a_local_dispatch() {
        let (mut store, set_timer) =
            instantiate_timer_test_module(crate::call::DispatchKind::Local);
        set_timer.call(&mut store, ()).unwrap();
        assert_eq!(
            store.data().scope.effect_queue,
            vec![Effect::SetTimer {
                delay_ms: 25,
                timer: TimerPayload {
                    type_name: "Timer".into(),
                    data: b"abc".to_vec(),
                },
            }]
        );
    }

    #[test]
    fn set_timer_rejected_during_a_read_only_call() {
        let (mut store, set_timer) =
            instantiate_timer_test_module(crate::call::DispatchKind::Agreed);
        store.data_mut().call_kind = CallKind::Query;
        assert!(set_timer.call(&mut store, ()).is_err());
        assert!(store.data().scope.effect_queue.is_empty());
    }

    #[test]
    fn sign_without_a_signer_traps_in_a_local_handler() {
        let (mut store, sign, _) = instantiate_sign_test_module(
            crate::call::DispatchKind::Local,
            0,
            vec![SignScheme::Ed25519],
            false,
        );
        let error = sign.call(&mut store, ()).unwrap_err();
        assert!(format!("{error:?}").contains("local handlers"), "{error:?}");
        assert!(store.data().scope.effect_queue.is_empty());
    }

    #[test]
    fn sign_allows_declared_schemes_regardless_of_duplicate_declarations() {
        for declared in [
            vec![SignScheme::Ed25519],
            vec![SignScheme::Ed25519, SignScheme::Ed25519],
        ] {
            let (mut store, sign, memory) =
                instantiate_sign_test_module(crate::call::DispatchKind::Local, 0, declared, true);
            let written = sign.call(&mut store, ()).unwrap();
            let mut encoded = vec![0u8; written as usize];
            memory.read(&store, 0, &mut encoded).unwrap();
            let (signed_bytes, signature): (Vec<u8>, Vec<u8>) =
                borsh::from_slice(&encoded).unwrap();
            assert_eq!(&signed_bytes[..4], 0u32.to_le_bytes());
            assert_eq!(&signed_bytes[4..], b"payload");
            assert_eq!(signature, vec![0u8; 64]);
        }
    }

    #[test]
    fn sign_rejects_undeclared_scheme() {
        let (mut store, sign, _) = instantiate_sign_test_module(
            crate::call::DispatchKind::Local,
            1,
            vec![SignScheme::Ed25519],
            true,
        );
        assert!(sign.call(&mut store, ()).is_err());
        assert!(store.data().scope.effect_queue.is_empty());
    }

    #[test]
    fn sign_rejects_unknown_scheme_discriminant() {
        let (mut store, sign, _) = instantiate_sign_test_module(
            crate::call::DispatchKind::Local,
            7,
            vec![SignScheme::Ed25519],
            true,
        );
        assert!(sign.call(&mut store, ()).is_err());
        assert!(store.data().scope.effect_queue.is_empty());
    }

    fn instantiate_effect_test_module(
        dispatch: crate::call::DispatchKind,
        body: &str,
        imports: &str,
    ) -> (Store<HostState>, wasmtime::TypedFunc<(), i32>) {
        instantiate_effect_test_module_with_pages(dispatch, body, imports, 1)
    }

    fn instantiate_effect_test_module_with_pages(
        dispatch: crate::call::DispatchKind,
        body: &str,
        imports: &str,
        pages: u32,
    ) -> (Store<HostState>, wasmtime::TypedFunc<(), i32>) {
        let engine = Engine::default();
        let wat = format!(
            r#"
                (module
                  {imports}
                  (memory (export "memory") {pages})
                  (data (i32.const 0) "x")
                  (func (export "call") (result i32) {body}))
            "#
        );
        let module = Module::new(&engine, wat).unwrap();
        let mut linker = Linker::new(&engine);
        crate::engine::imports::register_always_available(&mut linker).unwrap();
        register_capability_imports(&mut linker, &[Capability::Messaging, Capability::Timers])
            .unwrap();
        let mut store = Store::new(&engine, {
            let mut hs = HostState::new(
                arena0_program::ExecutionProfile::current(),
                CallKind::Dispatch,
                crate::call::DispatchKind::Local,
                Vec::new(),
            );
            hs.dispatch = dispatch;
            hs
        });
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let call = instance
            .get_typed_func::<(), i32>(&mut store, "call")
            .unwrap();
        (store, call)
    }

    #[test]
    fn lifecycle_effect_traps_in_a_local_dispatch() {
        let (mut store, call) = instantiate_effect_test_module(
            crate::call::DispatchKind::Local,
            "i32.const 0 i32.const 1 call $end_session i32.const 0",
            r#"(import "arena0" "end_session" (func $end_session (param i32 i32)))"#,
        );
        let error = call.call(&mut store, ()).unwrap_err();
        assert!(
            format!("{error:?}").contains("only available to agreed events"),
            "{error:?}"
        );
        assert!(store.data().scope.effect_queue.is_empty());
    }

    #[test]
    fn a_second_lifecycle_effect_traps() {
        let (mut store, call) = instantiate_effect_test_module(
            crate::call::DispatchKind::Agreed,
            "i32.const 0 i32.const 1 call $end_session i32.const 0 i32.const 1 call $abort_session i32.const 0",
            r#"(import "arena0" "end_session" (func $end_session (param i32 i32)))
               (import "arena0" "abort_session" (func $abort_session (param i32 i32)))"#,
        );
        let error = call.call(&mut store, ()).unwrap_err();
        assert!(
            format!("{error:?}").contains("at most one lifecycle effect"),
            "{error:?}"
        );
    }

    #[test]
    fn set_timer_combined_with_a_lifecycle_effect_traps() {
        let imports = r#"(import "arena0" "end_session" (func $end_session (param i32 i32)))
               (import "arena0" "set_timer" (func $set_timer (param i64 i32 i32 i32 i32)))"#;
        let (mut store, call) = instantiate_effect_test_module(
            crate::call::DispatchKind::Agreed,
            "i32.const 0 i32.const 1 call $end_session i64.const 0 i32.const 0 i32.const 1 i32.const 0 i32.const 1 call $set_timer i32.const 0",
            imports,
        );
        let error = call.call(&mut store, ()).unwrap_err();
        assert!(
            format!("{error:?}").contains("SetTimer cannot be combined"),
            "{error:?}"
        );

        let (mut store, call) = instantiate_effect_test_module(
            crate::call::DispatchKind::Agreed,
            "i64.const 0 i32.const 0 i32.const 1 i32.const 0 i32.const 1 call $set_timer i32.const 0 i32.const 1 call $end_session i32.const 0",
            imports,
        );
        let error = call.call(&mut store, ()).unwrap_err();
        assert!(
            format!("{error:?}").contains("cannot be combined with SetTimer"),
            "{error:?}"
        );
    }

    #[test]
    fn local_broadcast_returns_queue_full_after_an_earlier_broadcast() {
        let (mut store, call) = instantiate_effect_test_module(
            crate::call::DispatchKind::Local,
            "i32.const 0 i32.const 1 call $broadcast drop \
             i32.const 0 i32.const 1 call $broadcast",
            r#"(import "arena0" "broadcast" (func $broadcast (param i32 i32) (result i32)))"#,
        );
        store.data_mut().outgoing_len = arena0_protocol::execution::MAX_OUTGOING_MESSAGES - 1;
        // The first broadcast fills the queue; the second observes the bound.
        assert_eq!(call.call(&mut store, ()).unwrap(), 1);
        assert_eq!(store.data().scope.effect_queue.len(), 1);
    }

    #[test]
    fn local_broadcast_returns_queue_full_when_the_queue_is_already_full() {
        let (mut store, call) = instantiate_effect_test_module(
            crate::call::DispatchKind::Local,
            "i32.const 0 i32.const 1 call $broadcast",
            r#"(import "arena0" "broadcast" (func $broadcast (param i32 i32) (result i32)))"#,
        );
        store.data_mut().outgoing_len = arena0_protocol::execution::MAX_OUTGOING_MESSAGES;
        assert_eq!(call.call(&mut store, ()).unwrap(), 1);
        assert!(store.data().scope.effect_queue.is_empty());
    }

    #[test]
    fn broadcast_payload_bound_is_enforced_at_emission() {
        let at_limit = arena0_protocol::MAX_EFFECT_PAYLOAD_BYTES;
        let (mut store, call) = instantiate_effect_test_module_with_pages(
            crate::call::DispatchKind::Local,
            &format!("i32.const 0 i32.const {at_limit} call $broadcast"),
            r#"(import "arena0" "broadcast" (func $broadcast (param i32 i32) (result i32)))"#,
            2,
        );
        assert_eq!(call.call(&mut store, ()).unwrap(), 0);
        assert_eq!(store.data().scope.effect_queue.len(), 1);

        let (mut store, call) = instantiate_effect_test_module_with_pages(
            crate::call::DispatchKind::Local,
            &format!("i32.const 0 i32.const {} call $broadcast", at_limit + 1),
            r#"(import "arena0" "broadcast" (func $broadcast (param i32 i32) (result i32)))"#,
            2,
        );
        assert!(call.call(&mut store, ()).is_err());
        assert!(store.data().scope.effect_queue.is_empty());
    }

    #[test]
    fn effect_count_bound_is_enforced_at_emission() {
        let count = arena0_program::MAX_EFFECTS_PER_DISPATCH as usize;
        let imports =
            r#"(import "arena0" "broadcast" (func $broadcast (param i32 i32) (result i32)))"#;
        let emit = |n: usize| {
            format!(
                "{}i32.const 0",
                "i32.const 0 i32.const 1 call $broadcast drop ".repeat(n)
            )
        };
        let (mut store, call) = instantiate_effect_test_module(
            crate::call::DispatchKind::Agreed,
            &emit(count),
            imports,
        );
        assert_eq!(call.call(&mut store, ()).unwrap(), 0);
        assert_eq!(store.data().scope.effect_queue.len(), count);

        let (mut store, call) = instantiate_effect_test_module(
            crate::call::DispatchKind::Agreed,
            &emit(count + 1),
            imports,
        );
        assert!(call.call(&mut store, ()).is_err());
        assert_eq!(store.data().scope.effect_queue.len(), count);
    }

    #[test]
    fn broadcast_is_available_to_agreed_events() {
        let (mut store, call) = instantiate_effect_test_module(
            crate::call::DispatchKind::Agreed,
            "i32.const 0 i32.const 1 call $broadcast",
            r#"(import "arena0" "broadcast" (func $broadcast (param i32 i32) (result i32)))"#,
        );
        assert_eq!(call.call(&mut store, ()).unwrap(), 0);
        assert_eq!(store.data().scope.effect_queue.len(), 1);
    }
}

impl super::super::DispatchScope {
    /// Require this dispatch's installed blob capability; no view survives reset.
    fn blob_view(&self) -> Result<Arc<dyn BlobView>, wasmtime::Error> {
        let _ = ();
        todo!("STUB(sandbox)")
    }

    /// Overlay durable partial state with staged append/commit operations in dispatch order without publishing any effect.
    fn receive_state(&self, hash: BlobHash) -> Result<Receive, wasmtime::Error> {
        let _ = hash;
        todo!("STUB(sandbox)")
    }

    /// Project this dispatch's appended bytes in call order; durable bytes remain owned by the installed view.
    fn staged_tail(&self, hash: BlobHash) -> Vec<u8> {
        let _ = hash;
        todo!("STUB(sandbox)")
    }
}
