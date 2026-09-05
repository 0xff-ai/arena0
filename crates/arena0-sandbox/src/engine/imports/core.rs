//! Host imports that are present for every generated guest module.

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
                caller.reject_random_disallowed("random")?;
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

    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::RETRY_INPUT,
            |mut caller: Caller<'_, HostState>, reason_ptr: u32, reason_len: u32| {
                caller.begin_import("retry_input")?;
                caller.reject_non_local("retry_input")?;
                let reason = caller.read_guest_bytes(reason_ptr, reason_len, "retry_input")?;
                caller.record_effect(Effect::RetryInput {
                    reason: String::from_utf8_lossy(&reason).into_owned(),
                })
            },
        )
        .map_err(map_err)?;

    linker
        .func_wrap(
            abi::HOST_MODULE,
            imports::SET_CONTINUATION_TAG,
            |mut caller: Caller<'_, HostState>, tag: u32| {
                caller.begin_import("set_continuation_tag")?;
                caller.reject_non_local("set_continuation_tag")?;
                caller.data_mut().next_continuation_tag = Some(tag);
                Ok(())
            },
        )
        .map_err(map_err)?;

    Ok(())
}
