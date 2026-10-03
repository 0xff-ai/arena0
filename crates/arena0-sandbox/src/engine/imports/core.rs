//! Host imports that are present for every generated guest module.

use arena0_program::abi::{self, imports};
use arena0_program::{
    CANONICAL_STATE_PREFIX_BYTES as STATE_PREFIX, StateFrameError, StateMemoryKind,
    state_frame_len, write_state_frame,
};
use arena0_protocol::{Effect, LogLevel};
use wasmtime::{Caller, Linker};

use super::CallerExt as _;
use crate::SandboxError;
use crate::engine::HostState;

/// Register imports required by every generated module. Pure imports are also
/// available to read-only calls; effect and state imports enforce their own
/// call-kind boundaries before allocating or changing state.
pub(crate) fn register_always_available(
    linker: &mut Linker<HostState>,
) -> Result<(), SandboxError> {
    let map_err = |error: wasmtime::Error| SandboxError::instantiation_failed(error.to_string());

    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::HASH,
            |mut caller: Caller<'_, HostState>, data_ptr: u32, data_len: u32, out_ptr: u32| {
                caller.begin_import(imports::HASH)?;
                let data = caller.read_guest_bytes(data_ptr, data_len, imports::HASH)?;
                let fuel = u64::from(data_len) * caller.data().profile.fuel.hash_per_byte;
                caller.charge_fuel(fuel, imports::HASH)?;
                let max = caller.data().profile.limits.max_host_bytes;
                caller
                    .data_mut()
                    .scope
                    .ledger
                    .copy_bytes(32, max)
                    .map_err(wasmtime::Error::new)?;
                let digest = arena0_crypto::hash(arena0_crypto::HashAlgorithm::Blake3, &data);
                caller
                    .work_memory()?
                    .write(&mut caller, out_ptr as usize, &digest)
                    .map_err(|error| wasmtime::Error::msg(error.to_string()))
            },
        )
        .map_err(map_err)?;

    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::MERGE_CV,
            |mut caller: Caller<'_, HostState>,
             left_ptr: u32,
             right_ptr: u32,
             root: u32,
             out_ptr: u32| {
                caller.begin_import(imports::MERGE_CV)?;
                let left = caller.read_guest_bytes(left_ptr, 32, imports::MERGE_CV)?;
                let right = caller.read_guest_bytes(right_ptr, 32, imports::MERGE_CV)?;
                // One BLAKE3 compression over the 64-byte parent block.
                let fuel = 64 * caller.data().profile.fuel.hash_per_byte;
                caller.charge_fuel(fuel, imports::MERGE_CV)?;
                let max = caller.data().profile.limits.max_host_bytes;
                caller
                    .data_mut()
                    .scope
                    .ledger
                    .copy_bytes(32, max)
                    .map_err(wasmtime::Error::new)?;
                let parent = arena0_crypto::blake3_tree::merge_cv(
                    &left.try_into().expect("cv width"),
                    &right.try_into().expect("cv width"),
                    root != 0,
                );
                caller
                    .work_memory()?
                    .write(&mut caller, out_ptr as usize, &parent)
                    .map_err(|error| wasmtime::Error::msg(error.to_string()))
            },
        )
        .map_err(map_err)?;

    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::PERMUTATION,
            |mut caller: Caller<'_, HostState>, seed_ptr: u32, n: u32, out_ptr: u32| {
                caller.begin_import(imports::PERMUTATION)?;
                if u64::from(n) > caller.data().profile.limits.max_permutation_len {
                    return Err(wasmtime::Error::msg("permutation: length exceeds maximum"));
                }
                let seed = caller.read_guest_bytes(seed_ptr, 32, imports::PERMUTATION)?;
                let max = caller.data().profile.limits.max_host_bytes;
                caller
                    .data_mut()
                    .scope
                    .ledger
                    .copy_bytes(4 * n as usize, max)
                    .map_err(wasmtime::Error::new)?;
                let fuel = u64::from(n) * caller.data().profile.fuel.permutation_per_item;
                caller.charge_fuel(fuel, imports::PERMUTATION)?;
                let order = arena0_crypto::permutation(seed.try_into().expect("32-byte seed"), n)
                    .ok_or_else(|| {
                    wasmtime::Error::msg("permutation: random draw exhausted")
                })?;
                let memory = caller.work_memory()?;
                for (index, item) in order.into_iter().enumerate() {
                    memory
                        .write(
                            &mut caller,
                            out_ptr as usize + 4 * index,
                            &item.to_le_bytes(),
                        )
                        .map_err(|error| wasmtime::Error::msg(error.to_string()))?;
                }
                Ok(())
            },
        )
        .map_err(map_err)?;

    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::VERIFY,
            |mut caller: Caller<'_, HostState>,
             signed_ptr: u32,
             signed_len: u32,
             sig_ptr: u32,
             sig_len: u32,
             signer_ptr: u32,
             out_ptr: u32,
             out_cap: u32| {
                caller.begin_import(imports::VERIFY)?;
                let verifier = caller.data().scope.verifier.clone().ok_or_else(|| {
                    wasmtime::Error::msg("verify is only available in dispatches")
                })?;
                let fuel = caller.data().profile.fuel.verify;
                caller.charge_fuel(fuel, imports::VERIFY)?;
                let signed = caller.read_guest_bytes(signed_ptr, signed_len, imports::VERIFY)?;
                let signature = caller.read_guest_bytes(sig_ptr, sig_len, imports::VERIFY)?;
                let signer = caller.read_guest_bytes(signer_ptr, 32, imports::VERIFY)?;
                let signer =
                    arena0_protocol::PeerId(signer.try_into().expect("32-byte peer identity"));
                let encoded = borsh::to_vec(&verifier.verify(&signed, &signature, &signer))
                    .expect("verification result is serializable");
                if encoded.len() > out_cap as usize {
                    return Err(wasmtime::Error::msg(
                        "verify: result exceeds output capacity",
                    ));
                }
                let max = caller.data().profile.limits.max_host_bytes;
                caller
                    .data_mut()
                    .scope
                    .ledger
                    .copy_bytes(encoded.len(), max)
                    .map_err(wasmtime::Error::new)?;
                caller
                    .work_memory()?
                    .write(&mut caller, out_ptr as usize, &encoded)
                    .map_err(|error| wasmtime::Error::msg(error.to_string()))?;
                Ok(encoded.len() as u32)
            },
        )
        .map_err(map_err)?;

    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::STATE_LEN,
            |mut caller: Caller<'_, HostState>, kind: u32| {
                caller.begin_import(imports::STATE_LEN)?;
                caller.reject_state_io(imports::STATE_LEN)?;
                let memory = caller.state_memory(kind)?;
                let max_payload = state_payload_max(&caller, kind)?;
                let payload_len = state_frame_len(memory.data(&caller), max_payload)
                    .map_err(|error| frame_error(imports::STATE_LEN, error))?;
                // The length was read from the frame's `u32` prefix.
                Ok(payload_len as u32)
            },
        )
        .map_err(map_err)?;

    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::STATE_READ,
            |mut caller: Caller<'_, HostState>, kind: u32, dst: u32, len: u32| {
                caller.begin_import(imports::STATE_READ)?;
                caller.reject_state_io(imports::STATE_READ)?;
                let state = caller.state_memory(kind)?;
                let max_payload = state_payload_max(&caller, kind)?;
                let payload_len = state_frame_len(state.data(&caller), max_payload)
                    .map_err(|error| frame_error(imports::STATE_READ, error))?;
                if len as usize > payload_len {
                    return Err(wasmtime::Error::msg(
                        "state_read: requested range exceeds state payload",
                    ));
                }
                let requested_end = STATE_PREFIX + len as usize;
                let work = caller.work_memory()?;
                let start = dst as usize;
                let end = start
                    .checked_add(len as usize)
                    .ok_or_else(|| wasmtime::Error::msg("state_read: destination overflow"))?;
                if end > work.data_size(&caller) {
                    return Err(wasmtime::Error::msg(
                        "state_read: destination exceeds work memory",
                    ));
                }
                let max_host_bytes = caller.data().profile.limits.max_host_bytes;
                caller
                    .data_mut()
                    .scope
                    .ledger
                    .copy_bytes(len as usize, max_host_bytes)
                    .map_err(wasmtime::Error::new)?;
                let payload = state.data(&caller)[STATE_PREFIX..requested_end].to_vec();
                work.write(&mut caller, start, &payload)
                    .map_err(|error| wasmtime::Error::msg(error.to_string()))
            },
        )
        .map_err(map_err)?;

    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::STATE_WRITE,
            |mut caller: Caller<'_, HostState>, kind: u32, src: u32, len: u32| {
                caller.begin_import(imports::STATE_WRITE)?;
                caller.reject_state_io(imports::STATE_WRITE)?;
                let work = caller.work_memory()?;
                let start = src as usize;
                let end = start
                    .checked_add(len as usize)
                    .ok_or_else(|| wasmtime::Error::msg("state_write: source overflow"))?;
                if end > work.data_size(&caller) {
                    return Err(wasmtime::Error::msg(
                        "state_write: source exceeds work memory",
                    ));
                }
                let state = caller.state_memory(kind)?;
                let max_payload = state_payload_max(&caller, kind)?;
                if len as usize > max_payload {
                    return Err(wasmtime::Error::msg(format!(
                        "state_write: payload length {len} exceeds maximum {max_payload}"
                    )));
                }
                let old_len = state_frame_len(state.data(&caller), max_payload)
                    .map_err(|error| frame_error(imports::STATE_WRITE, error))?;
                if STATE_PREFIX + len as usize > state.data_size(&caller) {
                    return Err(wasmtime::Error::msg(
                        "state_write: payload exceeds state memory",
                    ));
                }
                let stale_clear_bytes = old_len.saturating_sub(len as usize);
                let host_bytes = stale_clear_bytes
                    .checked_add(len as usize)
                    .ok_or_else(|| wasmtime::Error::msg("state_write: host byte count overflow"))?;
                let max_host_bytes = caller.data().profile.limits.max_host_bytes;
                caller
                    .data_mut()
                    .scope
                    .ledger
                    .copy_bytes(host_bytes, max_host_bytes)
                    .map_err(wasmtime::Error::new)?;
                let payload = work.data(&caller)[start..end].to_vec();
                write_state_frame(state.data_mut(&mut caller), old_len, &payload)
                    .map_err(|error| frame_error(imports::STATE_WRITE, error))
            },
        )
        .map_err(map_err)?;

    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::LOG,
            |mut caller: Caller<'_, HostState>, level: u32, ptr: u32, len: u32| {
                caller.begin_import(imports::LOG)?;
                caller.reject_read_only(imports::LOG)?;
                let message = caller.read_guest_bytes(ptr, len, imports::LOG)?;
                let profile = caller.data().profile.clone();
                caller
                    .data_mut()
                    .scope
                    .ledger
                    .log(
                        message.len(),
                        profile.limits.max_log_bytes,
                        profile.limits.max_log_entries,
                    )
                    .map_err(wasmtime::Error::new)?;
                let message = String::from_utf8_lossy(&message).into_owned();
                let log_level = LogLevel::from_abi_tag(level)
                    .ok_or_else(|| wasmtime::Error::msg("invalid log level ABI tag"))?;
                caller
                    .data_mut()
                    .scope
                    .logs
                    .push((format!("{log_level:?}"), message));
                Ok(())
            },
        )
        .map_err(map_err)?;

    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::RANDOM,
            |mut caller: Caller<'_, HostState>, ptr: u32, len: u32| {
                caller.begin_import(imports::RANDOM)?;
                caller.reject_read_only(imports::RANDOM)?;
                let profile = caller.data().profile.clone();
                if u64::from(len) > profile.randomness.max_draw_bytes {
                    return Err(wasmtime::Error::msg(format!(
                        "random: draw is {len} bytes; maximum is {}",
                        profile.randomness.max_draw_bytes
                    )));
                }
                caller
                    .data_mut()
                    .scope
                    .ledger
                    .copy_bytes(len as usize, profile.limits.max_host_bytes)
                    .map_err(wasmtime::Error::new)?;
                caller
                    .data_mut()
                    .scope
                    .ledger
                    .random(profile.limits.max_random_draws)
                    .map_err(wasmtime::Error::new)?;
                let work_mem = caller.work_memory()?;
                let data_len = work_mem.data(&caller).len();
                let start = ptr as usize;
                let end = start
                    .checked_add(len as usize)
                    .ok_or_else(|| wasmtime::Error::msg("random: ptr+len overflow"))?;
                if end > data_len {
                    return Err(wasmtime::Error::msg("random: write out of bounds"));
                }
                let mut bytes = vec![0u8; len as usize];
                caller.data_mut().scope.entropy.fill(&mut bytes);
                let data = work_mem.data_mut(&mut caller);
                data[start..end].copy_from_slice(&bytes);
                Ok(())
            },
        )
        .map_err(map_err)?;

    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::FAIL,
            |mut caller: Caller<'_, HostState>, reason_ptr: u32, reason_len: u32| {
                caller.begin_import(imports::FAIL)?;
                caller.reject_read_only(imports::FAIL)?;
                let reason = caller.read_guest_bytes(reason_ptr, reason_len, imports::FAIL)?;
                caller.record_effect(Effect::Fail {
                    reason: String::from_utf8_lossy(&reason).into_owned(),
                })
            },
        )
        .map_err(map_err)?;

    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::END_SESSION,
            |mut caller: Caller<'_, HostState>, outcome_ptr: u32, outcome_len: u32| {
                caller.begin_import(imports::END_SESSION)?;
                caller.reject_read_only(imports::END_SESSION)?;
                let outcome =
                    caller.read_guest_bytes(outcome_ptr, outcome_len, imports::END_SESSION)?;
                caller.record_effect(Effect::SessionEnd { outcome })
            },
        )
        .map_err(map_err)?;

    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::ABORT_SESSION,
            |mut caller: Caller<'_, HostState>, reason_ptr: u32, reason_len: u32| {
                caller.begin_import(imports::ABORT_SESSION)?;
                caller.reject_read_only(imports::ABORT_SESSION)?;
                let reason =
                    caller.read_guest_bytes(reason_ptr, reason_len, imports::ABORT_SESSION)?;
                caller.record_effect(Effect::SessionAbort {
                    reason: String::from_utf8_lossy(&reason).into_owned(),
                })
            },
        )
        .map_err(map_err)?;

    Ok(())
}

fn frame_error(import: &str, error: StateFrameError) -> wasmtime::Error {
    wasmtime::Error::msg(format!("{import}: {error}"))
}

fn state_payload_max(caller: &Caller<'_, HostState>, kind: u32) -> Result<usize, wasmtime::Error> {
    let max = match StateMemoryKind::try_from(kind)
        .map_err(|error| wasmtime::Error::msg(error.to_string()))?
    {
        StateMemoryKind::Shared => caller.data().profile.limits.max_shared_state_bytes,
        StateMemoryKind::Local => caller.data().profile.limits.max_local_state_bytes,
    };
    usize::try_from(max)
        .map_err(|_| wasmtime::Error::msg("state payload maximum does not fit in usize"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_writes_blake3_and_charges_fuel() {
        let wat = r#"(module
            (import "arena0" "hash" (func $hash (param i32 i32 i32)))
            (memory (export "memory") 1)
            (data (i32.const 64) "abc")
            (func (export "run")
                i32.const 64 i32.const 3 i32.const 0 call $hash))"#;
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine = wasmtime::Engine::new(&config).unwrap();
        let module = wasmtime::Module::new(&engine, wat).unwrap();
        let mut linker = Linker::new(&engine);
        register_always_available(&mut linker).unwrap();
        let mut store = wasmtime::Store::new(
            &engine,
            HostState::new(
                arena0_program::ExecutionProfile::current(),
                crate::engine::CallKind::Query,
                crate::call::DispatchKind::Agreed,
                Vec::new(),
            ),
        );
        store.set_fuel(10_000_000).unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let memory = instance.get_memory(&mut store, "memory").unwrap();

        let run = instance
            .get_typed_func::<(), ()>(&mut store, "run")
            .unwrap();
        store.data_mut().profile.fuel.hash_per_byte = 0;
        run.call(&mut store, ()).unwrap();
        let baseline = 10_000_000 - store.get_fuel().unwrap();
        store.set_fuel(10_000_000).unwrap();
        store.data_mut().profile.fuel.hash_per_byte = arena0_program::profile::HASH_FUEL_PER_BYTE;
        run.call(&mut store, ()).unwrap();
        assert_eq!(
            10_000_000 - store.get_fuel().unwrap(),
            baseline + 3 * arena0_program::profile::HASH_FUEL_PER_BYTE
        );
        assert_eq!(
            &memory.data(&store)[..32],
            &arena0_crypto::hash(arena0_crypto::HashAlgorithm::Blake3, b"abc")
        );
        assert_eq!(store.data().scope.ledger.host_bytes, 2 * (3 + 32));
        assert!(store.data().scope.effect_queue.is_empty());
    }

    #[test]
    fn permutation_matches_arena0_crypto_and_rejects_over_the_limit() {
        let wat = r#"(module
            (import "arena0" "permutation" (func $permutation (param i32 i32 i32)))
            (memory (export "memory") 1)
            (func (export "run") (param $n i32)
                i32.const 0 local.get $n i32.const 64 call $permutation))"#;
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine = wasmtime::Engine::new(&config).unwrap();
        let module = wasmtime::Module::new(&engine, wat).unwrap();
        let mut linker = Linker::new(&engine);
        register_always_available(&mut linker).unwrap();
        let mut store = wasmtime::Store::new(
            &engine,
            HostState::new(
                arena0_program::ExecutionProfile::current(),
                crate::engine::CallKind::Query,
                crate::call::DispatchKind::Agreed,
                Vec::new(),
            ),
        );
        store.set_fuel(10_000_000).unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let memory = instance.get_memory(&mut store, "memory").unwrap();

        memory.write(&mut store, 0, &[7; 32]).unwrap();
        let run = instance
            .get_typed_func::<u32, ()>(&mut store, "run")
            .unwrap();
        for n in [0, 1, 64, arena0_program::profile::MAX_PERMUTATION_LEN] {
            run.call(&mut store, n).unwrap();
            let actual = memory.data(&store)[64..64 + 4 * n as usize]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| u32::from_le_bytes(*bytes))
                .collect::<Vec<_>>();
            assert_eq!(actual, arena0_crypto::permutation([7; 32], n).unwrap());
        }
        let copied = store.data().scope.ledger.host_bytes;
        let before = memory.data(&store).to_vec();
        let error = run
            .call(&mut store, arena0_program::profile::MAX_PERMUTATION_LEN + 1)
            .unwrap_err();
        assert!(format!("{error:?}").contains("length exceeds maximum"));
        assert_eq!(store.data().scope.ledger.host_bytes, copied);
        assert_eq!(memory.data(&store), before);
        assert!(store.data().scope.effect_queue.is_empty());
    }

    #[test]
    fn verify_without_a_verifier_traps() {
        let wat = r#"(module
            (import "arena0" "verify" (func $verify (param i32 i32 i32 i32 i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "run") (result i32)
                i32.const 0 i32.const 0 i32.const 0 i32.const 0
                i32.const 0 i32.const 64 i32.const 128 call $verify))"#;
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine = wasmtime::Engine::new(&config).unwrap();
        let module = wasmtime::Module::new(&engine, wat).unwrap();
        let mut linker = Linker::new(&engine);
        register_always_available(&mut linker).unwrap();
        let mut store = wasmtime::Store::new(
            &engine,
            HostState::new(
                arena0_program::ExecutionProfile::current(),
                crate::engine::CallKind::Query,
                crate::call::DispatchKind::Agreed,
                Vec::new(),
            ),
        );
        store.set_fuel(10_000_000).unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let memory = instance.get_memory(&mut store, "memory").unwrap();

        let run = instance
            .get_typed_func::<(), u32>(&mut store, "run")
            .unwrap();
        let error = run.call(&mut store, ()).unwrap_err();
        assert!(format!("{error:?}").contains("verify is only available in dispatches"));
        assert_eq!(store.data().scope.ledger.host_bytes, 0);
        assert_eq!(&memory.data(&store)[64..192], &[0; 128]);
    }

    #[test]
    fn verify_writes_the_verifier_result() {
        struct Verifier(Result<Vec<u8>, arena0_protocol::VerifyError>);
        impl crate::GuestVerifier for Verifier {
            fn verify(
                &self,
                signed: &[u8],
                signature: &[u8],
                signer: &arena0_protocol::PeerId,
            ) -> Result<Vec<u8>, arena0_protocol::VerifyError> {
                assert_eq!(signed, b"signed");
                assert_eq!(signature, b"signature");
                assert_eq!(signer.0, [7; 32]);
                self.0.clone()
            }
        }
        let wat = r#"(module
            (import "arena0" "verify" (func $verify (param i32 i32 i32 i32 i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "signed")
            (data (i32.const 16) "signature")
            (func (export "run") (param $cap i32) (result i32)
                i32.const 0 i32.const 6 i32.const 16 i32.const 9
                i32.const 32 i32.const 64 local.get $cap call $verify))"#;
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine = wasmtime::Engine::new(&config).unwrap();
        let module = wasmtime::Module::new(&engine, wat).unwrap();
        let mut linker = Linker::new(&engine);
        register_always_available(&mut linker).unwrap();
        let mut store = wasmtime::Store::new(
            &engine,
            HostState::new(
                arena0_program::ExecutionProfile::current(),
                crate::engine::CallKind::Query,
                crate::call::DispatchKind::Agreed,
                Vec::new(),
            ),
        );
        store.set_fuel(10_000_000).unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let memory = instance.get_memory(&mut store, "memory").unwrap();

        memory.write(&mut store, 32, &[7; 32]).unwrap();
        store.data_mut().call_kind = crate::engine::CallKind::Dispatch;
        let run = instance
            .get_typed_func::<u32, u32>(&mut store, "run")
            .unwrap();
        for result in [
            Ok(b"payload".to_vec()),
            Err(arena0_protocol::VerifyError::BadSignature),
        ] {
            store.data_mut().scope.verifier = Some(std::sync::Arc::new(Verifier(result.clone())));
            let len = run.call(&mut store, 128).unwrap();
            let decoded: Result<Vec<u8>, arena0_protocol::VerifyError> =
                borsh::from_slice(&memory.data(&store)[64..64 + len as usize]).unwrap();
            assert_eq!(decoded, result);
            let error = run.call(&mut store, 0).unwrap_err();
            assert!(format!("{error:?}").contains("result exceeds output capacity"));
        }
        assert!(store.data().scope.effect_queue.is_empty());
    }
}
