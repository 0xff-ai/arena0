//! Host effect dispatch via wasm imports.
//!
//! Module-level functions forward runtime effects to the host through the
//! `arena0` wasm import module. On non-wasm targets the SDK never reaches a
//! host import: native builds compile pure program logic only.

use arena0_crypto::SignScheme;
#[cfg(not(target_arch = "wasm32"))]
use arena0_program::abi::imports;
use arena0_protocol::{LogLevel, TimerPayload};

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

/// Host imports exist only inside the Wasm guest. Native builds compile the
/// SDK for pure program tests, which never reach a host import.
#[cfg(not(target_arch = "wasm32"))]
#[cold]
fn native_host_unavailable(import: &'static str) -> ! {
    panic!("arena0 host import `{import}` is only available inside the Wasm guest")
}

/// Run `body` against the `arena0` imports inside the Wasm guest. Native
/// builds never reach a host import, so there the call consumes its
/// arguments and panics naming `IMPORT`.
macro_rules! host_import {
    ($import:ident($($arg:expr),*) { $($body:tt)* }) => {{
        #[cfg(target_arch = "wasm32")]
        // SAFETY: every import reads or writes only the guest ranges passed
        // to it, which the caller's borrowed arguments keep alive.
        unsafe { $($body)* }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = ($($arg,)*);
            native_host_unavailable(imports::$import)
        }
    }};
}

/// Return the encoded byte length of one host-owned state value.
#[doc(hidden)]
pub fn host_state_len(kind: u32) -> usize {
    host_import!(STATE_LEN(kind) {
        state_len(kind) as usize
    })
}

/// Read one host-owned state value into a guest buffer.
#[doc(hidden)]
pub fn host_state_read(kind: u32, buffer: &mut [u8]) {
    host_import!(STATE_READ(kind, buffer) {
        state_read(kind, buffer.as_mut_ptr() as u32, buffer.len() as u32);
    })
}

/// Replace one host-owned state value from a guest buffer.
#[doc(hidden)]
pub fn host_state_write(kind: u32, buffer: &[u8]) {
    host_import!(STATE_WRITE(kind, buffer) {
        state_write(kind, buffer.as_ptr() as u32, buffer.len() as u32);
    })
}

pub fn host_log(level: LogLevel, msg: &str) {
    host_import!(LOG(level, msg) {
        log(level.abi_tag(), msg.as_ptr() as u32, msg.len() as u32);
    })
}

pub(crate) fn host_random(buf: &mut [u8]) {
    host_import!(RANDOM(buf) {
        random(buf.as_mut_ptr() as u32, buf.len() as u32);
    })
}

/// Broadcast a message to every participant.
///
/// Returns [`BroadcastError::QueueFull`] when the durable outgoing queue is
/// full; the host then queues nothing.
pub(crate) fn host_broadcast(msg_bytes: &[u8]) -> Result<(), crate::context::BroadcastError> {
    host_import!(BROADCAST(msg_bytes) {
        if broadcast(msg_bytes.as_ptr() as u32, msg_bytes.len() as u32) != 0 {
            return Err(crate::context::BroadcastError::QueueFull);
        }
        Ok(())
    })
}

pub(crate) fn host_set_timer(delay_ms: u64, payload: &TimerPayload) {
    host_import!(SET_TIMER(delay_ms, payload) {
        let TimerPayload { type_name, data } = payload;
        set_timer(
            delay_ms,
            type_name.as_ptr() as u32,
            type_name.len() as u32,
            data.as_ptr() as u32,
            data.len() as u32,
        );
    })
}

pub(crate) fn host_guest_sign(scheme: SignScheme, payload: &[u8]) -> (Vec<u8>, Vec<u8>) {
    host_import!(SIGN(scheme, payload) {
        let capacity = payload
            .len()
            .saturating_add(arena0_program::SIGN_RESULT_OVERHEAD_BYTES);
        let out = crate::io_alloc::io_alloc(capacity);
        assert!(!out.is_null(), "guest sign buffer allocation failed");
        // The host writes a Borsh `(signed_bytes, signature)` pair and returns
        // its length; a missing signer traps before any bytes are written.
        let written = sign(
            arena0_program::abi::sign_scheme::tag(scheme),
            payload.as_ptr() as u32,
            payload.len() as u32,
            out as u32,
            capacity as u32,
        ) as usize;
        debug_assert!(written <= capacity);
        let bytes = core::slice::from_raw_parts(out, written).to_vec();
        crate::io_alloc::io_dealloc(out, capacity);
        borsh::from_slice(&bytes).expect("host sign result decode failed")
    })
}

pub(crate) fn host_end_session(outcome: &[u8]) {
    host_import!(END_SESSION(outcome) {
        end_session(outcome.as_ptr() as u32, outcome.len() as u32);
    })
}

pub(crate) fn host_abort_session(reason: &str) {
    host_import!(ABORT_SESSION(reason) {
        abort_session(reason.as_ptr() as u32, reason.len() as u32);
    })
}

pub fn host_fail(reason: &str) {
    host_import!(FAIL(reason) {
        fail(reason.as_ptr() as u32, reason.len() as u32);
    })
}
