//! Allocator entry points for the resident guest ABI.
//!
//! ABI buffers use the guest's normal allocator with one fixed layout. The
//! host still owns the pointer/length lifetime: it allocates an input envelope,
//! calls one export, then deallocates the envelope. Generated handlers use the
//! same allocator for ordinary `Vec`, Borsh, and Serde temporaries.

use core::alloc::Layout;

const ABI_ALLOC_ALIGN: usize = 8;

fn abi_layout(len: usize) -> Option<Layout> {
    Layout::from_size_align(len, ABI_ALLOC_ALIGN).ok()
}

/// Allocate a host ABI buffer from the guest's normal allocator.
///
/// A null pointer signals an invalid length or allocation failure. Returning a
/// fallible result through the C ABI lets the host report the failure without
/// trapping before it can restore its resident instance.
pub fn io_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return core::ptr::null_mut();
    }
    let Some(layout) = abi_layout(len) else {
        return core::ptr::null_mut();
    };
    // SAFETY: `layout` is valid and the returned allocation is released by
    // `io_dealloc` with the same fixed alignment and caller-supplied length.
    unsafe { std::alloc::alloc(layout) }
}

/// Release a host ABI buffer allocated by [`io_alloc`].
///
/// # Safety
///
/// `ptr` must have been returned by [`io_alloc`] for the same non-zero `len`,
/// and it must not have been released already.
pub unsafe fn io_dealloc(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let Some(layout) = abi_layout(len) else {
        return;
    };
    // SAFETY: upheld by this function's caller contract.
    unsafe { std::alloc::dealloc(ptr, layout) };
}

/// Allocate and release the resident temporary-work reserve.
///
/// A runtime calls this once before it captures the resident baseline. The
/// normal wasm allocator grows the work memory while satisfying the reserve;
/// after this succeeds, the runtime freezes the observed work-memory size and
/// rejects any later growth. The reserve is deliberately separate from the
/// ABI input allocation so generated temporary `Vec` values are covered by
/// the same measured baseline.
#[must_use]
pub fn prepare_allocator() -> bool {
    let Some(layout) = abi_layout(crate::RESIDENT_ALLOCATOR_RESERVE_BYTES) else {
        return false;
    };
    // SAFETY: `layout` is valid and the allocation contains at least one byte.
    // Touch both ends so an optimizing allocator/compiler cannot treat the
    // reserve as dead before the runtime observes the grown linear memory.
    let ptr = unsafe { std::alloc::alloc(layout) };
    if ptr.is_null() {
        return false;
    }
    unsafe {
        core::ptr::write_volatile(ptr, 0);
        core::ptr::write_volatile(ptr.add(layout.size() - 1), 0);
    }
    unsafe { std::alloc::dealloc(ptr, layout) };
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_length_abi_allocation_is_a_null_noop() {
        let ptr = io_alloc(0);
        assert!(ptr.is_null());
        // SAFETY: null is explicitly accepted and no allocation is released.
        unsafe { io_dealloc(ptr, 0) };
    }
}
