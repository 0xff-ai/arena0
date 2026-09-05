//! Capability-gated imports and per-call resource accounting.

mod capabilities;
mod core;

use arena0_crypto::SignScheme;
use arena0_program::Capability;
use arena0_protocol::Lifecycle;
use wasmtime::{Caller, Extern, Memory};

use super::HostState;

pub(super) use capabilities::register_capability_imports;
pub(super) use core::register_always_available;

/// Link every declared capability during the metadata probe. The probe's
/// `CallKind::Metadata` rejects all effect imports if a guest attempts one.
pub(super) fn register_metadata_imports(
    linker: &mut wasmtime::Linker<HostState>,
) -> Result<(), crate::SandboxError> {
    let capabilities = [
        Capability::Messaging,
        Capability::Input,
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
    fn read_guest_bytes(
        &mut self,
        ptr: u32,
        len: u32,
        label: &str,
    ) -> Result<Vec<u8>, wasmtime::Error>;
    fn begin_import(&mut self, _name: &str) -> Result<(), wasmtime::Error>;
    fn reject_read_only(&self, name: &str) -> Result<(), wasmtime::Error>;
    fn reject_non_local(&self, name: &str) -> Result<(), wasmtime::Error>;
    fn reject_random_disallowed(&self, name: &str) -> Result<(), wasmtime::Error>;
    fn reject_if_lifecycle_disallowed(
        &self,
        function_name: &str,
        allowed: &[Lifecycle],
    ) -> Result<(), wasmtime::Error>;
    fn record_effect(&mut self, effect: arena0_protocol::Effect) -> Result<(), wasmtime::Error>;
}

impl CallerExt for Caller<'_, HostState> {
    fn work_memory(&mut self) -> Result<Memory, wasmtime::Error> {
        match self.get_export("memory") {
            Some(Extern::Memory(memory)) => Ok(memory),
            _ => Err(wasmtime::Error::msg("memory not found in caller")),
        }
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

    fn reject_non_local(&self, name: &str) -> Result<(), wasmtime::Error> {
        self.reject_read_only(name)?;
        if !self.data().call_kind.allows_local_effects() {
            return Err(wasmtime::Error::msg(format!(
                "{name}: import is unavailable to shared calls"
            )));
        }
        Ok(())
    }

    fn reject_random_disallowed(&self, name: &str) -> Result<(), wasmtime::Error> {
        self.reject_read_only(name)?;
        if !self.data().call_kind.allows_random() {
            return Err(wasmtime::Error::msg(format!(
                "{name}: randomness is unavailable to shared calls"
            )));
        }
        Ok(())
    }

    fn reject_if_lifecycle_disallowed(
        &self,
        function_name: &str,
        allowed: &[Lifecycle],
    ) -> Result<(), wasmtime::Error> {
        let lifecycle = self.data().lifecycle;
        if !allowed.contains(&lifecycle) {
            return Err(wasmtime::Error::msg(format!(
                "{function_name}: not allowed in {lifecycle:?} lifecycle"
            )));
        }
        Ok(())
    }

    fn record_effect(&mut self, effect: arena0_protocol::Effect) -> Result<(), wasmtime::Error> {
        if let arena0_protocol::Effect::Callout {
            callout_index,
            context,
            ..
        } = &effect
        {
            let schema = self
                .data()
                .callout_inputs
                .get(*callout_index as usize)
                .ok_or_else(|| wasmtime::Error::msg("unknown callout schema index"))?;
            let value: serde_json::Value = serde_json::from_slice(context).map_err(|error| {
                wasmtime::Error::msg(format!("callout context is not JSON: {error}"))
            })?;
            let validator = jsonschema::validator_for(schema.as_value()).map_err(|error| {
                wasmtime::Error::msg(format!("invalid callout schema: {error}"))
            })?;
            validator.validate(&value).map_err(|error| {
                wasmtime::Error::msg(format!("callout context schema validation failed: {error}"))
            })?;
        }
        if !self.data().call_kind.allows_effect(&effect) {
            return Err(wasmtime::Error::msg(format!(
                "effect {:?} is unavailable to {:?} calls",
                effect,
                self.data().call_kind
            )));
        }
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

/// Decode a u32 ABI discriminant to a [`SignScheme`].
fn u32_to_sign_scheme(value: u32) -> Result<SignScheme, wasmtime::Error> {
    match value {
        0 => Ok(SignScheme::Ed25519),
        1 => Ok(SignScheme::Bls),
        _ => Err(wasmtime::Error::msg(format!(
            "unknown sign scheme: {value}"
        ))),
    }
}
