//! Bounded Wasm memory access for one guest invocation.

use wasmtime::{Instance, Store};

use super::HostState;
use crate::SandboxError;

/// Unpack the canonical ABI `(ptr, len)` pair from one i64.
///
/// The high 32 bits are the pointer and the low 32 bits are the byte length.
#[allow(clippy::cast_sign_loss)]
fn unpack_i64(packed: i64) -> Result<(u32, u32), SandboxError> {
    if packed < 0 {
        return Err(SandboxError::dispatch_failed(
            "guest returned a negative pointer/length pair",
        ));
    }
    let ptr = (packed >> 32) as u32;
    let len = (packed & 0xFFFF_FFFF) as u32;
    Ok((ptr, len))
}

/// A handle bundling the store and instance, kept private to the current call.
pub(super) struct Guest<'a> {
    store: &'a mut Store<HostState>,
    instance: &'a Instance,
}

impl<'a> Guest<'a> {
    pub(super) fn new(store: &'a mut Store<HostState>, instance: &'a Instance) -> Self {
        Self { store, instance }
    }

    /// Read bytes from the canonical exported linear memory.
    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn read_mem(&mut self, ptr: u32, len: u32) -> Result<Vec<u8>, SandboxError> {
        let memory = self.memory()?;
        let data = memory.data(&*self.store);
        let start = ptr as usize;
        let end = start
            .checked_add(len as usize)
            .ok_or_else(|| SandboxError::dispatch_failed("pointer arithmetic overflow"))?;
        if end > data.len() {
            return Err(SandboxError::dispatch_failed(format!(
                "read out of bounds: {end} > {}",
                data.len()
            )));
        }
        Ok(data[start..end].to_vec())
    }

    /// Read bytes and charge the copy against the current call's host budget.
    pub(super) fn read_mem_charged(
        &mut self,
        ptr: u32,
        len: u32,
        max_host_bytes: u64,
    ) -> Result<Vec<u8>, SandboxError> {
        self.store
            .data_mut()
            .ledger
            .copy_bytes(len as usize, max_host_bytes)?;
        self.read_mem(ptr, len)
    }

    /// Write bytes into linear memory after checking pointer arithmetic and the
    /// configured memory bound.
    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn write_mem(&mut self, offset: u32, bytes: &[u8]) -> Result<(), SandboxError> {
        let max_memory = self.store.data().profile.limits.max_memory_bytes as usize;
        if bytes.len() > max_memory {
            return Err(SandboxError::MemoryLimitExceeded(bytes.len() as u64));
        }
        let memory = self.memory()?;
        let needed = (offset as usize)
            .checked_add(bytes.len())
            .ok_or_else(|| SandboxError::dispatch_failed("write offset+length overflow"))?;
        if needed > max_memory {
            return Err(SandboxError::MemoryLimitExceeded(needed as u64));
        }
        let current = memory.data_size(&*self.store);
        if needed > current {
            let extra_pages = ((needed - current) as u64).div_ceil(65_536);
            memory
                .grow(&mut *self.store, extra_pages)
                .map_err(|_| SandboxError::MemoryLimitExceeded(needed as u64))?;
        }

        let data = memory.data_mut(&mut *self.store);
        let end = (offset as usize)
            .checked_add(bytes.len())
            .ok_or_else(|| SandboxError::dispatch_failed("write offset+length overflow"))?;
        data[offset as usize..end].copy_from_slice(bytes);
        Ok(())
    }

    /// Write host-provided bytes and charge the copy to the invocation's
    /// cumulative host-byte budget before touching guest memory.
    pub(super) fn write_mem_charged(
        &mut self,
        offset: u32,
        bytes: &[u8],
        max_host_bytes: u64,
    ) -> Result<(), SandboxError> {
        self.store
            .data_mut()
            .ledger
            .copy_bytes(bytes.len(), max_host_bytes)?;
        self.write_mem(offset, bytes)
    }

    /// Call one canonical export that accepts `(ptr, len)` and returns a packed
    /// `(ptr, len)` result.
    #[allow(clippy::cast_possible_wrap)]
    pub(super) fn call_pair_return(
        &mut self,
        name: &str,
        ptr: u32,
        len: u32,
    ) -> Result<(u32, u32), SandboxError> {
        let func = self
            .instance
            .get_typed_func::<(i32, i32), i64>(&mut *self.store, name)
            .map_err(|e| SandboxError::dispatch_failed(e.to_string()))?;
        let packed = func
            .call(&mut *self.store, (ptr as i32, len as i32))
            .map_err(trap_to_error)?;
        unpack_i64(packed)
    }

    /// Call the metadata export with no arguments.
    pub(super) fn call_metadata(&mut self) -> Result<(u32, u32), SandboxError> {
        let func = self
            .instance
            .get_typed_func::<(), i64>(&mut *self.store, "arena0_metadata")
            .map_err(|e| SandboxError::dispatch_failed(e.to_string()))?;
        unpack_i64(func.call(&mut *self.store, ()).map_err(trap_to_error)?)
    }

    /// Allocate a guest buffer through the explicit allocator export.
    #[allow(clippy::cast_possible_wrap)]
    pub(super) fn alloc(&mut self, len: u32) -> Result<u32, SandboxError> {
        let func = self
            .instance
            .get_typed_func::<i32, i32>(&mut *self.store, "arena0_alloc")
            .map_err(|e| SandboxError::dispatch_failed(e.to_string()))?;
        let ptr = func
            .call(&mut *self.store, len as i32)
            .map_err(trap_to_error)?;
        if ptr <= 0 && len > 0 {
            return Err(SandboxError::dispatch_failed(
                "guest allocator returned null for a non-empty buffer",
            ));
        }
        Ok(ptr as u32)
    }

    /// Deallocate a guest buffer. A trap is fatal to this fresh call.
    #[allow(clippy::cast_possible_wrap)]
    pub(super) fn dealloc(&mut self, ptr: u32, len: u32) -> Result<(), SandboxError> {
        let func = self
            .instance
            .get_typed_func::<(i32, i32), ()>(&mut *self.store, "arena0_dealloc")
            .map_err(|e| SandboxError::dispatch_failed(e.to_string()))?;
        func.call(&mut *self.store, (ptr as i32, len as i32))
            .map_err(trap_to_error)
    }

    fn memory(&mut self) -> Result<wasmtime::Memory, SandboxError> {
        self.instance
            .get_memory(&mut *self.store, "memory")
            .ok_or_else(|| SandboxError::dispatch_failed("no 'memory' export"))
    }
}

fn trap_to_error(error: wasmtime::Error) -> SandboxError {
    match error.downcast::<SandboxError>() {
        Ok(error) => error,
        Err(error) => SandboxError::dispatch_failed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::unpack_i64;

    #[test]
    fn packed_result_rejects_negative_pointer_pairs() {
        assert!(unpack_i64(-1).is_err());
        assert!(unpack_i64(i64::MIN).is_err());
    }

    #[test]
    fn packed_result_decodes_bounded_unsigned_halves() {
        let packed = (0x1234_i64 << 32) | 0x5678;
        assert_eq!(unpack_i64(packed).unwrap(), (0x1234, 0x5678));
    }
}
