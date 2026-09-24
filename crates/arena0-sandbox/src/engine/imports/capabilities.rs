//! Capability-gated host function registration (messaging, input, timers, and
//! sign).

use arena0_crypto::SignScheme;
use arena0_program::Capability;
use arena0_program::abi::{self, imports};
use arena0_protocol::{Effect, Lifecycle, TimerPayload};
use wasmtime::{Caller, Linker};

use super::{CallerExt as _, u32_to_sign_scheme};
use crate::SandboxError;
use crate::engine::HostState;

/// Register host functions for each declared capability into the linker.
pub(crate) fn register_capability_imports(
    linker: &mut Linker<HostState>,
    capabilities: &[Capability],
) -> Result<(), SandboxError> {
    let mut sign_schemes = Vec::new();
    for capability in capabilities {
        match capability {
            Capability::Messaging => register_messaging(linker)?,
            Capability::Input => register_input(linker)?,
            Capability::Timers => register_timers(linker)?,
            Capability::Sign { schemes } => sign_schemes.extend(schemes.iter().copied()),
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
            imports::BROADCAST,
            |mut caller: Caller<'_, HostState>, data_ptr: u32, data_len: u32| {
                caller.begin_import("broadcast")?;
                caller.reject_if_lifecycle_disallowed(
                    "broadcast",
                    &[
                        Lifecycle::PreSession,
                        Lifecycle::Active,
                        Lifecycle::Completed,
                        Lifecycle::Failed,
                    ],
                )?;
                caller.reject_read_only("broadcast")?;
                let data = caller.read_guest_bytes(data_ptr, data_len, "broadcast:data")?;
                caller.record_effect(Effect::Broadcast { data })
            },
        )
        .map_err(map_err)?;
    Ok(())
}

fn register_input(linker: &mut Linker<HostState>) -> Result<(), SandboxError> {
    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::REQUEST_INPUT,
            |mut caller: Caller<'_, HostState>,
             variant_index: u32,
             context_ptr: u32,
             context_len: u32,
             expected_ptr: u32,
             expected_len: u32| {
                caller.begin_import("request_input")?;
                caller.reject_if_lifecycle_disallowed("request_input", &[Lifecycle::Active])?;
                caller.reject_read_only("request_input")?;
                let context =
                    caller.read_guest_bytes(context_ptr, context_len, "request_input:context")?;
                let expected_type = read_optional_guest_string(
                    &mut caller,
                    expected_ptr,
                    expected_len,
                    "request_input:expected_type",
                )?;
                caller.record_effect(Effect::Callout {
                    callout_index: variant_index,
                    context,
                    expected_type,
                })
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
                caller.begin_import("set_timer")?;
                caller.reject_if_lifecycle_disallowed(
                    "set_timer",
                    &[Lifecycle::PreSession, Lifecycle::Active],
                )?;
                caller.reject_read_only("set_timer")?;
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
                caller.begin_import("sign")?;
                caller.reject_if_lifecycle_disallowed("sign", &[Lifecycle::Active])?;
                caller.reject_read_only("sign")?;
                let scheme = u32_to_sign_scheme(scheme)?;
                reject_if_sign_scheme_disallowed("sign", scheme, &allowed_schemes)?;
                let payload = caller.read_guest_bytes(data_ptr, data_len, "sign")?;
                let Some(signer) = caller.data().signer.signer() else {
                    return Err(wasmtime::Error::msg(
                        "sign is only available in local handlers",
                    ));
                };
                let call_index = caller.data_mut().signer.next_call();
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

fn read_optional_guest_string(
    caller: &mut Caller<'_, HostState>,
    ptr: u32,
    len: u32,
    label: &str,
) -> Result<Option<String>, wasmtime::Error> {
    if len == 0 {
        return Ok(None);
    }
    let bytes = caller.read_guest_bytes(ptr, len, label)?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| wasmtime::Error::msg(format!("{label}: invalid UTF-8: {error}")))
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
        stage: Lifecycle,
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
                Lifecycle::PreSession,
                None,
                Vec::new(),
            );
            hs.lifecycle = stage;
            hs
        });
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let set_timer = instance
            .get_typed_func::<(), ()>(&mut store, "call_set_timer")
            .unwrap();
        (store, set_timer)
    }

    fn instantiate_sign_test_module(
        stage: Lifecycle,
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
                Lifecycle::PreSession,
                None,
                Vec::new(),
            );
            hs.lifecycle = stage;
            if with_signer {
                hs.signer.install(Some(std::sync::Arc::new(TestSigner)));
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

    fn instantiate_input_test_module(
        stage: Lifecycle,
    ) -> (Store<HostState>, wasmtime::TypedFunc<(), ()>) {
        let engine = Engine::default();
        let module = Module::new(
            &engine,
            r#"
                (module
                  (import "arena0" "request_input"
                    (func $request_input (param i32 i32 i32 i32 i32)))
                  (memory (export "memory") 1)
                  (data (i32.const 0) "null")
                  (data (i32.const 8) "Choice")
                  (func (export "call_request_input")
                    i32.const 0
                    i32.const 0
                    i32.const 4
                    i32.const 8
                    i32.const 6
                    call $request_input))
            "#,
        )
        .unwrap();

        let mut linker = Linker::new(&engine);
        register_capability_imports(&mut linker, &[Capability::Input]).unwrap();

        let mut store = Store::new(&engine, {
            let mut hs = HostState::new(
                arena0_program::ExecutionProfile::current(),
                CallKind::Dispatch,
                Lifecycle::PreSession,
                None,
                vec![
                    arena0_program::JsonSchemaDocument::new(serde_json::json!({
                        "$schema": "https://json-schema.org/draft/2020-12/schema",
                        "type": "null"
                    }))
                    .unwrap(),
                ],
            );
            hs.lifecycle = stage;
            hs
        });
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let request_input = instance
            .get_typed_func::<(), ()>(&mut store, "call_request_input")
            .unwrap();
        (store, request_input)
    }

    #[test]
    fn set_timer_allowed_during_pre_session() {
        let (mut store, set_timer) = instantiate_timer_test_module(Lifecycle::PreSession);
        set_timer.call(&mut store, ()).unwrap();
        assert_eq!(
            store.data().effect_queue,
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
    fn set_timer_rejected_after_finish() {
        let (mut store, set_timer) = instantiate_timer_test_module(Lifecycle::Completed);
        assert!(set_timer.call(&mut store, ()).is_err());
        assert!(store.data().effect_queue.is_empty());
    }

    #[test]
    fn request_input_records_declared_output_type() {
        let (mut store, request_input) = instantiate_input_test_module(Lifecycle::Active);
        request_input.call(&mut store, ()).unwrap();
        assert_eq!(
            store.data().effect_queue,
            vec![Effect::Callout {
                callout_index: 0,
                context: b"null".to_vec(),
                expected_type: Some("Choice".into()),
            }]
        );
    }

    #[test]
    fn sign_without_a_signer_traps_in_a_local_handler() {
        let (mut store, sign, _) =
            instantiate_sign_test_module(Lifecycle::Active, 0, vec![SignScheme::Ed25519], false);
        let error = sign.call(&mut store, ()).unwrap_err();
        assert!(format!("{error:?}").contains("local handlers"), "{error:?}");
        assert!(store.data().effect_queue.is_empty());
    }

    #[test]
    fn sign_rejected_during_pre_session() {
        let (mut store, sign, _) =
            instantiate_sign_test_module(Lifecycle::PreSession, 0, vec![SignScheme::Ed25519], true);
        assert!(sign.call(&mut store, ()).is_err());
        assert!(store.data().effect_queue.is_empty());
    }

    #[test]
    fn sign_allows_declared_schemes_regardless_of_duplicate_declarations() {
        for declared in [
            vec![SignScheme::Ed25519],
            vec![SignScheme::Ed25519, SignScheme::Ed25519],
        ] {
            let (mut store, sign, memory) =
                instantiate_sign_test_module(Lifecycle::Active, 0, declared, true);
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
        let (mut store, sign, _) =
            instantiate_sign_test_module(Lifecycle::Active, 1, vec![SignScheme::Ed25519], true);
        assert!(sign.call(&mut store, ()).is_err());
        assert!(store.data().effect_queue.is_empty());
    }

    #[test]
    fn sign_rejects_unknown_scheme_discriminant() {
        let (mut store, sign, _) =
            instantiate_sign_test_module(Lifecycle::Active, 7, vec![SignScheme::Ed25519], true);
        assert!(sign.call(&mut store, ()).is_err());
        assert!(store.data().effect_queue.is_empty());
    }
}
