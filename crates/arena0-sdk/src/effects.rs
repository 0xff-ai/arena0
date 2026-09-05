//! Host effect dispatch via wasm imports.
//!
//! Module-level functions forward runtime effects to the host through the
//! `arena0` wasm import module. On non-wasm targets the calls are no-ops,
//! allowing unit testing without a host.

use arena0_crypto::SignScheme;
use arena0_protocol::{LogLevel, TimerSpec};

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "arena0")]
unsafe extern "C" {
    fn fail(reason_ptr: u32, reason_len: u32);
    fn log(level: u32, msg_ptr: u32, msg_len: u32);
    fn random(buf_ptr: u32, buf_len: u32);
    fn broadcast(data_ptr: u32, data_len: u32);
    fn request_input(variant_index: u32, context_ptr: u32, context_len: u32);
    fn request_input_pending(
        variant_index: u32,
        context_ptr: u32,
        context_len: u32,
        label_ptr: u32,
        label_len: u32,
        expected_ptr: u32,
        expected_len: u32,
    );
    fn set_timer(delay_ms: i64);
    fn set_typed_timer(delay_ms: i64, type_ptr: u32, type_len: u32, data_ptr: u32, data_len: u32);
    fn sign(scheme: u32, data_ptr: u32, data_len: u32);
    fn sign_pending(
        scheme: u32,
        data_ptr: u32,
        data_len: u32,
        label_ptr: u32,
        label_len: u32,
        expected_ptr: u32,
        expected_len: u32,
    );
    fn end_session(result_ptr: u32, result_len: u32);
    fn abort_session(reason_ptr: u32, reason_len: u32);
    fn retry_input(reason_ptr: u32, reason_len: u32);
    fn set_continuation_tag(tag: u32);
}

pub fn host_log(level: LogLevel, msg: &str) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        log(log_level_tag(level), msg.as_ptr() as u32, msg.len() as u32);
    }
    #[cfg(not(target_arch = "wasm32"))]
    crate::testing::push_log(format!("{level:?}"), msg.to_string());
}

#[inline]
#[cfg(target_arch = "wasm32")]
const fn log_level_tag(level: LogLevel) -> u32 {
    match level {
        LogLevel::Debug => 0,
        LogLevel::Info => 1,
        LogLevel::Warn => 2,
        LogLevel::Error => 3,
    }
}

#[inline]
#[cfg(target_arch = "wasm32")]
const fn sign_scheme_tag(scheme: SignScheme) -> u32 {
    match scheme {
        SignScheme::Ed25519 => 0,
        SignScheme::Bls => 1,
    }
}

pub(crate) fn host_random(buf: &mut [u8]) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        random(buf.as_mut_ptr() as u32, buf.len() as u32);
    }
    #[cfg(not(target_arch = "wasm32"))]
    buf.fill(0); // deterministic for tests
}

/// Broadcast a message to every participant.
pub(crate) fn host_broadcast(msg_bytes: &[u8]) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        broadcast(msg_bytes.as_ptr() as u32, msg_bytes.len() as u32);
    }
    #[cfg(not(target_arch = "wasm32"))]
    crate::testing::push_effect(arena0_protocol::Effect::Broadcast {
        data: msg_bytes.to_vec(),
    });
}

pub(crate) fn host_callout_raw(
    callout_index: u32,
    context: &[u8],
    pending_label: Option<&str>,
    expected_type: Option<&str>,
    continuation_tag: Option<u32>,
) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        if let Some(tag) = continuation_tag {
            set_continuation_tag(tag);
        }
        match (pending_label, expected_type) {
            (None, None) => {
                request_input(callout_index, context.as_ptr() as u32, context.len() as u32)
            }
            _ => request_input_pending(
                callout_index,
                context.as_ptr() as u32,
                context.len() as u32,
                pending_label.map_or(0, |s| s.as_ptr() as u32),
                pending_label.map_or(0, str::len) as u32,
                expected_type.map_or(0, |s| s.as_ptr() as u32),
                expected_type.map_or(0, str::len) as u32,
            ),
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    crate::testing::push_effect(arena0_protocol::Effect::Callout {
        callout_index,
        context: context.to_vec(),
        pending_label: pending_label.map(str::to_string),
        expected_type: expected_type.map(str::to_string),
        continuation_tag,
    });
}

pub(crate) fn host_set_timer_spec(spec: &TimerSpec) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        match &spec.payload {
            Some(arena0_protocol::TimerPayload { type_name, data }) => set_typed_timer(
                spec.delay_ms as i64,
                type_name.as_ptr() as u32,
                type_name.len() as u32,
                data.as_ptr() as u32,
                data.len() as u32,
            ),
            None => set_timer(spec.delay_ms as i64),
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    crate::testing::push_effect(arena0_protocol::Effect::SetTimer {
        delay_ms: spec.delay_ms,
        timer: spec.payload.clone(),
    });
}

pub(crate) fn host_sign(
    scheme: SignScheme,
    data: &[u8],
    pending_label: Option<&str>,
    expected_type: Option<&str>,
    continuation_tag: Option<u32>,
) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        if let Some(tag) = continuation_tag {
            set_continuation_tag(tag);
        }
        match (pending_label, expected_type) {
            (None, None) => sign(
                sign_scheme_tag(scheme),
                data.as_ptr() as u32,
                data.len() as u32,
            ),
            _ => sign_pending(
                sign_scheme_tag(scheme),
                data.as_ptr() as u32,
                data.len() as u32,
                pending_label.map_or(0, |s| s.as_ptr() as u32),
                pending_label.map_or(0, str::len) as u32,
                expected_type.map_or(0, |s| s.as_ptr() as u32),
                expected_type.map_or(0, str::len) as u32,
            ),
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    crate::testing::push_effect(arena0_protocol::Effect::Sign {
        scheme,
        data: data.to_vec(),
        pending_label: pending_label.map(str::to_string),
        expected_type: expected_type.map(str::to_string),
        continuation_tag,
    });
}

pub(crate) fn host_end_session(outcome: &[u8]) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        end_session(outcome.as_ptr() as u32, outcome.len() as u32);
    }
    #[cfg(not(target_arch = "wasm32"))]
    crate::testing::push_effect(arena0_protocol::Effect::SessionEnd {
        outcome: outcome.to_vec(),
    });
}

pub(crate) fn host_abort_session(reason: &str) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        abort_session(reason.as_ptr() as u32, reason.len() as u32);
    }
    #[cfg(not(target_arch = "wasm32"))]
    crate::testing::push_effect(arena0_protocol::Effect::SessionAbort {
        reason: reason.to_string(),
    });
}

pub fn host_fail(reason: &str) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        fail(reason.as_ptr() as u32, reason.len() as u32);
    }
    #[cfg(not(target_arch = "wasm32"))]
    crate::testing::push_effect(arena0_protocol::Effect::Fail {
        reason: reason.to_string(),
    });
}

pub fn host_retry_input(reason: &str) {
    #[cfg(target_arch = "wasm32")]
    // SAFETY: reason is a valid UTF-8 str; ptr and len are valid for the
    // duration of the host call (synchronous, single-threaded Wasm).
    unsafe {
        retry_input(reason.as_ptr() as u32, reason.len() as u32);
    }
    #[cfg(not(target_arch = "wasm32"))]
    crate::testing::push_effect(arena0_protocol::Effect::RetryInput {
        reason: reason.to_string(),
    });
}
