//! Capability-gated imports and per-call resource accounting.

mod capabilities;
mod core;

use arena0_crypto::SignScheme;
use arena0_program::abi::imports;
use arena0_program::{Capability, StateMemoryKind};
use arena0_protocol::Effect;
use wasmtime::{Caller, Extern, Memory};

use super::HostState;
use crate::call::DispatchKind;

pub(super) use capabilities::register_capability_imports;
pub(super) use core::register_always_available;

/// Link every declared capability during the metadata probe. The probe's
/// `CallKind::Metadata` rejects all effect imports if a guest attempts one.
pub(super) fn register_metadata_imports(
    linker: &mut wasmtime::Linker<HostState>,
) -> Result<(), crate::SandboxError> {
    let capabilities = [
        Capability::Messaging,
        Capability::Timers,
        Capability::Sign {
            schemes: vec![SignScheme::Ed25519, SignScheme::Bls],
        },
    ];
    capabilities::register_capability_imports(linker, &capabilities)
}

/// Arena0-specific operations on a Wasmtime host-function caller.
pub(super) trait CallerExt {
    fn work_memory(&mut self) -> Result<Memory, wasmtime::Error>;
    fn state_memory(&mut self, kind: u32) -> Result<Memory, wasmtime::Error>;
    fn reject_state_io(&self, name: &str) -> Result<(), wasmtime::Error>;
    fn read_guest_bytes(
        &mut self,
        ptr: u32,
        len: u32,
        label: &str,
    ) -> Result<Vec<u8>, wasmtime::Error>;
    fn begin_import(&mut self, _name: &str) -> Result<(), wasmtime::Error>;
    fn reject_read_only(&self, name: &str) -> Result<(), wasmtime::Error>;
    fn record_effect(&mut self, effect: Effect) -> Result<(), wasmtime::Error>;
}

impl CallerExt for Caller<'_, HostState> {
    fn work_memory(&mut self) -> Result<Memory, wasmtime::Error> {
        match self.get_export(arena0_program::abi::exports::WORK_MEMORY) {
            Some(Extern::Memory(memory)) => Ok(memory),
            _ => Err(wasmtime::Error::msg("memory not found in caller")),
        }
    }

    fn state_memory(&mut self, kind: u32) -> Result<Memory, wasmtime::Error> {
        let name = match StateMemoryKind::try_from(kind)
            .map_err(|error| wasmtime::Error::msg(error.to_string()))?
        {
            StateMemoryKind::Shared => arena0_program::abi::exports::SHARED_MEMORY,
            StateMemoryKind::Local => arena0_program::abi::exports::LOCAL_MEMORY,
        };
        match self.get_export(name) {
            Some(Extern::Memory(memory)) => Ok(memory),
            _ => Err(wasmtime::Error::msg(format!("{name} memory not found"))),
        }
    }

    fn reject_state_io(&self, name: &str) -> Result<(), wasmtime::Error> {
        if !self.data().call_kind.allows_state_io() {
            return Err(wasmtime::Error::msg(format!(
                "{name}: state memory imports are unavailable to this call"
            )));
        }
        Ok(())
    }

    fn read_guest_bytes(
        &mut self,
        ptr: u32,
        len: u32,
        label: &str,
    ) -> Result<Vec<u8>, wasmtime::Error> {
        let max = self.data().profile.limits.max_host_bytes;
        self.data_mut()
            .ledger
            .copy_bytes(len as usize, max)
            .map_err(wasmtime::Error::new)?;
        let work_mem = self.work_memory()?;
        let data = work_mem.data(&*self);
        let start = ptr as usize;
        let end = start
            .checked_add(len as usize)
            .ok_or_else(|| wasmtime::Error::msg(format!("{label}: ptr+len overflow")))?;
        if end > data.len() {
            return Err(wasmtime::Error::msg(format!("{label}: read out of bounds")));
        }
        Ok(data[start..end].to_vec())
    }

    fn begin_import(&mut self, _name: &str) -> Result<(), wasmtime::Error> {
        let max_calls = self.data().profile.limits.max_host_calls;
        self.data_mut()
            .ledger
            .host_call(max_calls)
            .map_err(wasmtime::Error::new)
    }

    fn reject_read_only(&self, name: &str) -> Result<(), wasmtime::Error> {
        if !self.data().call_kind.allows_effects() {
            return Err(wasmtime::Error::msg(format!(
                "{name}: effect imports are unavailable to this call"
            )));
        }
        Ok(())
    }

    fn record_effect(&mut self, effect: Effect) -> Result<(), wasmtime::Error> {
        if !self.data().call_kind.allows_effects() {
            return Err(wasmtime::Error::msg(format!(
                "effect {:?} is unavailable to {:?} calls",
                effect,
                self.data().call_kind
            )));
        }
        let queue = &self.data().effect_queue;
        if effect.is_lifecycle() {
            if self.data().dispatch != DispatchKind::Agreed {
                return Err(wasmtime::Error::msg(format!(
                    "{}: a lifecycle effect is only available to agreed events",
                    effect_name(&effect)
                )));
            }
            if queue.iter().any(Effect::is_lifecycle) {
                return Err(wasmtime::Error::msg(
                    "at most one lifecycle effect is allowed per dispatch",
                ));
            }
            if queue
                .iter()
                .any(|queued| matches!(queued, Effect::SetTimer { .. }))
            {
                return Err(wasmtime::Error::msg(
                    "a lifecycle effect cannot be combined with SetTimer",
                ));
            }
        } else if matches!(effect, Effect::SetTimer { .. })
            && queue.iter().any(Effect::is_lifecycle)
        {
            return Err(wasmtime::Error::msg(
                "SetTimer cannot be combined with a lifecycle effect",
            ));
        }
        // Enforce every protocol effect limit at emission, including the exact
        // canonical `Vec<Effect>` aggregate, so the dispatch path never has to
        // reject an effect the guest already emitted.
        arena0_protocol::execution::check_effect_budget(
            self.data()
                .effect_queue
                .iter()
                .chain(std::iter::once(&effect)),
        )
        .map_err(|error| wasmtime::Error::msg(format!("effect rejected: {error}")))?;
        let bytes = borsh::to_vec(&effect)
            .map_err(|error| wasmtime::Error::msg(format!("effect encoding failed: {error}")))?;
        let profile = self.data().profile.clone();
        self.data_mut()
            .ledger
            .effect(
                bytes.len(),
                profile.limits.max_effect_bytes,
                profile.limits.max_effects_per_dispatch,
            )
            .map_err(wasmtime::Error::new)?;
        self.data_mut().effect_queue.push(effect);
        Ok(())
    }
}

fn effect_name(effect: &Effect) -> &'static str {
    match effect {
        Effect::SessionEnd { .. } => imports::END_SESSION,
        Effect::SessionAbort { .. } => imports::ABORT_SESSION,
        Effect::Fail { .. } => imports::FAIL,
        Effect::SetTimer { .. } => imports::SET_TIMER,
        Effect::Broadcast { .. } => imports::BROADCAST,
    }
}

/// Decode a u32 ABI discriminant to a [`SignScheme`].
fn u32_to_sign_scheme(value: u32) -> Result<SignScheme, wasmtime::Error> {
    arena0_program::abi::sign_scheme::from_tag(value)
        .ok_or_else(|| wasmtime::Error::msg(format!("unknown sign scheme: {value}")))
}
