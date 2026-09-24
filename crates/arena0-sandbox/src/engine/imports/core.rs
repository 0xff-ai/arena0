//! Host imports that are present for every generated guest module.

use arena0_program::StateMemoryKind;
use arena0_program::abi::{self, imports};
use arena0_protocol::{Effect, LogLevel};
use wasmtime::{Caller, Linker};

use super::CallerExt as _;
use crate::SandboxError;
use crate::engine::HostState;

/// Register imports required by every generated module. Read-only calls still
/// link these names because Wasm imports are module-scoped; the call-kind guard
/// rejects their use before any allocation or effect occurs.
pub(crate) fn register_always_available(
    linker: &mut Linker<HostState>,
) -> Result<(), SandboxError> {
    let map_err = |error: wasmtime::Error| SandboxError::instantiation_failed(error.to_string());

    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::STATE_LEN,
            |mut caller: Caller<'_, HostState>, kind: u32| {
                caller.begin_import(imports::STATE_LEN)?;
                caller.reject_state_io(imports::STATE_LEN)?;
                let memory = caller.state_memory(kind)?;
                let data = memory.data(&caller);
                if data.len() < arena0_program::CANONICAL_STATE_PREFIX_BYTES {
                    return Err(wasmtime::Error::msg(
                        "state_len: state memory is smaller than its length prefix",
                    ));
                }
                let payload_len = u32::from_le_bytes(data[..4].try_into().unwrap());
                let max_payload = state_payload_max(&caller, kind)?;
                if payload_len as usize > max_payload {
                    return Err(wasmtime::Error::msg(format!(
                        "state_len: payload length {} exceeds maximum {max_payload}",
                        payload_len
                    )));
                }
                let end = arena0_program::CANONICAL_STATE_PREFIX_BYTES
                    .checked_add(payload_len as usize)
                    .ok_or_else(|| wasmtime::Error::msg("state_len: payload length overflow"))?;
                if end > data.len() {
                    return Err(wasmtime::Error::msg(
                        "state_len: state length exceeds memory capacity",
                    ));
                }
                Ok(payload_len)
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
                let requested_end = {
                    let state_data = state.data(&caller);
                    if state_data.len() < arena0_program::CANONICAL_STATE_PREFIX_BYTES {
                        return Err(wasmtime::Error::msg(
                            "state_read: state memory is smaller than its length prefix",
                        ));
                    }
                    let payload_len = u32::from_le_bytes(state_data[..4].try_into().unwrap());
                    let max_payload = state_payload_max(&caller, kind)?;
                    if payload_len as usize > max_payload {
                        return Err(wasmtime::Error::msg(format!(
                            "state_read: payload length {} exceeds maximum {max_payload}",
                            payload_len
                        )));
                    }
                    let payload_end = arena0_program::CANONICAL_STATE_PREFIX_BYTES
                        .checked_add(payload_len as usize)
                        .ok_or_else(|| {
                            wasmtime::Error::msg("state_read: payload length overflow")
                        })?;
                    let requested_end = arena0_program::CANONICAL_STATE_PREFIX_BYTES
                        .checked_add(len as usize)
                        .ok_or_else(|| wasmtime::Error::msg("state_read: length overflow"))?;
                    if payload_end > state_data.len() || requested_end > payload_end {
                        return Err(wasmtime::Error::msg(
                            "state_read: requested range exceeds state payload",
                        ));
                    }
                    requested_end
                };
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
                    .ledger
                    .copy_bytes(len as usize, max_host_bytes)
                    .map_err(wasmtime::Error::new)?;
                let payload = {
                    let state_data = state.data(&caller);
                    state_data[arena0_program::CANONICAL_STATE_PREFIX_BYTES..requested_end].to_vec()
                };
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
                let payload_start = arena0_program::CANONICAL_STATE_PREFIX_BYTES;
                let payload_end = payload_start
                    .checked_add(len as usize)
                    .ok_or_else(|| wasmtime::Error::msg("state_write: payload overflow"))?;
                let max_payload = state_payload_max(&caller, kind)?;
                if len as usize > max_payload {
                    return Err(wasmtime::Error::msg(format!(
                        "state_write: payload length {len} exceeds maximum {max_payload}"
                    )));
                }
                let (state_bytes, old_len) = {
                    let state_data = state.data(&caller);
                    if state_data.len() < payload_start {
                        return Err(wasmtime::Error::msg(
                            "state_write: state memory is smaller than its length prefix",
                        ));
                    }
                    let old_len =
                        u32::from_le_bytes(state_data[..payload_start].try_into().unwrap())
                            as usize;
                    if old_len > max_payload {
                        return Err(wasmtime::Error::msg(format!(
                            "state_write: existing payload length {old_len} exceeds maximum {max_payload}"
                        )));
                    }
                    let old_end = payload_start
                        .checked_add(old_len)
                        .ok_or_else(|| wasmtime::Error::msg("state_write: old length overflow"))?;
                    if old_end > state_data.len() {
                        return Err(wasmtime::Error::msg(
                            "state_write: existing payload exceeds state memory",
                        ));
                    }
                    (state_data.len(), old_len)
                };
                if payload_end > state_bytes {
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
                    .ledger
                    .copy_bytes(host_bytes, max_host_bytes)
                    .map_err(wasmtime::Error::new)?;
                let payload = work.data(&caller)[start..end].to_vec();
                let state_data = state.data_mut(&mut caller);
                if stale_clear_bytes != 0 {
                    let stale_start = payload_start + len as usize;
                    let stale_end = stale_start + stale_clear_bytes;
                    state_data[stale_start..stale_end].fill(0);
                }
                state_data[..payload_start].copy_from_slice(&len.to_le_bytes());
                state_data[payload_start..payload_end].copy_from_slice(&payload);
                Ok(())
            },
        )
        .map_err(map_err)?;

    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::LOG,
            |mut caller: Caller<'_, HostState>, level: u32, ptr: u32, len: u32| {
                caller.begin_import("log")?;
                caller.reject_read_only("log")?;
                let message = caller.read_guest_bytes(ptr, len, "log")?;
                let profile = caller.data().profile.clone();
                caller
                    .data_mut()
                    .ledger
                    .log(
                        message.len(),
                        profile.limits.max_log_bytes,
                        profile.limits.max_log_entries,
                    )
                    .map_err(wasmtime::Error::new)?;
                let message = String::from_utf8_lossy(&message).into_owned();
                let log_level = match level {
                    0 => LogLevel::Debug,
                    1 => LogLevel::Info,
                    2 => LogLevel::Warn,
                    3 => LogLevel::Error,
                    _ => return Err(wasmtime::Error::msg("invalid log level ABI tag")),
                };
                caller
                    .data_mut()
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
                caller.begin_import("random")?;
                caller.reject_read_only("random")?;
                let profile = caller.data().profile.clone();
                if u64::from(len) > profile.randomness.max_draw_bytes {
                    return Err(wasmtime::Error::msg(format!(
                        "random: draw is {len} bytes; maximum is {}",
                        profile.randomness.max_draw_bytes
                    )));
                }
                caller
                    .data_mut()
                    .ledger
                    .copy_bytes(len as usize, profile.limits.max_host_bytes)
                    .map_err(wasmtime::Error::new)?;
                caller
                    .data_mut()
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
                caller
                    .data_mut()
                    .entropy
                    .fill(&mut bytes)
                    .map_err(|error| wasmtime::Error::msg(error.to_string()))?;
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
                caller.begin_import("fail")?;
                caller.reject_read_only("fail")?;
                let reason = caller.read_guest_bytes(reason_ptr, reason_len, "fail")?;
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
                caller.begin_import("end_session")?;
                caller.reject_read_only("end_session")?;
                caller.reject_if_lifecycle_disallowed(
                    "end_session",
                    &[arena0_protocol::Lifecycle::Active],
                )?;
                let outcome = caller.read_guest_bytes(outcome_ptr, outcome_len, "end_session")?;
                caller.record_effect(Effect::SessionEnd { outcome })
            },
        )
        .map_err(map_err)?;

    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::ABORT_SESSION,
            |mut caller: Caller<'_, HostState>, reason_ptr: u32, reason_len: u32| {
                caller.begin_import("abort_session")?;
                caller.reject_read_only("abort_session")?;
                caller.reject_if_lifecycle_disallowed(
                    "abort_session",
                    &[arena0_protocol::Lifecycle::Active],
                )?;
                let reason = caller.read_guest_bytes(reason_ptr, reason_len, "abort_session")?;
                caller.record_effect(Effect::SessionAbort {
                    reason: String::from_utf8_lossy(&reason).into_owned(),
                })
            },
        )
        .map_err(map_err)?;

    Ok(())
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
