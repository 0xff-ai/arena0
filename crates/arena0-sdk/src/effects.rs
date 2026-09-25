//! Host effect dispatch via wasm imports.
//!
//! Module-level functions forward runtime effects to the host through the
//! `arena0` wasm import module. On non-wasm targets the calls are no-ops,
//! allowing unit testing without a host.

use arena0_crypto::SignScheme;
use arena0_protocol::{LogLevel, TimerSpec};

/// State-memory selector used by the always-available state imports.
#[doc(hidden)]
pub const STATE_KIND_SHARED: u32 = arena0_program::StateMemoryKind::Shared as u32;
/// State-memory selector used by the always-available state imports.
#[doc(hidden)]
pub const STATE_KIND_LOCAL: u32 = arena0_program::StateMemoryKind::Local as u32;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "arena0")]
unsafe extern "C" {
    fn fail(reason_ptr: u32, reason_len: u32);
    fn log(level: u32, msg_ptr: u32, msg_len: u32);
    fn random(buf_ptr: u32, buf_len: u32);
    fn broadcast(data_ptr: u32, data_len: u32) -> u32;
    fn set_timer(delay_ms: u64, type_ptr: u32, type_len: u32, data_ptr: u32, data_len: u32);
    fn sign(scheme: u32, data_ptr: u32, data_len: u32, out_ptr: u32, out_cap: u32) -> u32;
    fn end_session(result_ptr: u32, result_len: u32);
    fn abort_session(reason_ptr: u32, reason_len: u32);
    fn state_len(kind: u32) -> u32;
    fn state_read(kind: u32, ptr: u32, len: u32);
    fn state_write(kind: u32, ptr: u32, len: u32);
}

/// Return the encoded byte length of one host-owned state value.
#[doc(hidden)]
pub fn host_state_len(kind: u32) -> usize {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        state_len(kind) as usize
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = kind;
        0
    }
}

/// Read one host-owned state value into a guest buffer.
#[doc(hidden)]
pub fn host_state_read(kind: u32, buffer: &mut [u8]) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        state_read(kind, buffer.as_mut_ptr() as u32, buffer.len() as u32);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = kind;
        buffer.fill(0);
    }
}

/// Replace one host-owned state value from a guest buffer.
#[doc(hidden)]
pub fn host_state_write(kind: u32, buffer: &[u8]) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        state_write(kind, buffer.as_ptr() as u32, buffer.len() as u32);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (kind, buffer);
    }
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
///
/// Returns [`BroadcastError::QueueFull`] when the durable outgoing queue is
/// full; the host then queues nothing.
pub(crate) fn host_broadcast(msg_bytes: &[u8]) -> Result<(), crate::context::BroadcastError> {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        if broadcast(msg_bytes.as_ptr() as u32, msg_bytes.len() as u32) != 0 {
            return Err(crate::context::BroadcastError::QueueFull);
        }
        Ok(())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        crate::testing::push_effect(arena0_protocol::Effect::Broadcast {
            data: msg_bytes.to_vec(),
        });
        Ok(())
    }
}

pub(crate) fn host_set_timer_spec(spec: &TimerSpec) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        let arena0_protocol::TimerPayload { type_name, data } = &spec.payload;
        set_timer(
            spec.delay_ms,
            type_name.as_ptr() as u32,
            type_name.len() as u32,
            data.as_ptr() as u32,
            data.len() as u32,
        );
    }
    #[cfg(not(target_arch = "wasm32"))]
    crate::testing::push_effect(arena0_protocol::Effect::SetTimer {
        delay_ms: spec.delay_ms,
        timer: spec.payload.clone(),
    });
}

pub(crate) fn host_guest_sign(scheme: SignScheme, payload: &[u8]) -> (Vec<u8>, Vec<u8>) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        let capacity = payload
            .len()
            .saturating_add(arena0_program::SIGN_RESULT_OVERHEAD_BYTES);
        let out = crate::io_alloc::io_alloc(capacity);
        assert!(!out.is_null(), "guest sign buffer allocation failed");
        // The host writes a Borsh `(signed_bytes, signature)` pair and returns
        // its length; a missing signer traps before any bytes are written.
        let written = sign(
            sign_scheme_tag(scheme),
            payload.as_ptr() as u32,
            payload.len() as u32,
            out as u32,
            capacity as u32,
        ) as usize;
        debug_assert!(written <= capacity);
        let bytes = core::slice::from_raw_parts(out, written).to_vec();
        crate::io_alloc::io_dealloc(out, capacity);
        borsh::from_slice(&bytes).expect("host sign result decode failed")
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        fake_sign(scheme, payload)
    }
}

/// Deterministic stand-in for the host signer in native builds. The harness
/// cannot reach a real key, so it returns the payload unchanged with a
/// deterministic 64-byte signature over it.
#[cfg(not(target_arch = "wasm32"))]
fn fake_sign(scheme: SignScheme, payload: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let _ = scheme;
    let first = blake3::hash(payload);
    let mut second_input = Vec::with_capacity(1 + payload.len());
    second_input.push(0);
    second_input.extend_from_slice(payload);
    let second = blake3::hash(&second_input);
    let mut signature = Vec::with_capacity(64);
    signature.extend_from_slice(first.as_bytes());
    signature.extend_from_slice(second.as_bytes());
    (payload.to_vec(), signature)
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
