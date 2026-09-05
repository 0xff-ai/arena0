//! Dedicated bump allocator for host I/O buffers.
//! Decoupled from the guest's general-purpose allocator so the ABI
//! is agnostic to allocator choice (dlmalloc, wee_alloc, etc.).

/// The host may pass one complete bounded call envelope through the guest I/O
/// allocator. Keep this capacity tied to the ABI envelope limit.
const IO_ARENA_SIZE: usize = crate::MAX_CALL_ENVELOPE_BYTES as usize;

#[repr(C, align(8))]
struct IoArena {
    buf: [u8; IO_ARENA_SIZE],
}

static mut IO_ARENA: IoArena = IoArena {
    buf: [0u8; IO_ARENA_SIZE],
};

// SAFETY: Wasm is single-threaded; no concurrent access.
static mut IO_BUMP_OFFSET: usize = 0;

/// Allocate `len` bytes from the I/O arena.
///
/// Returns a pointer into the static buffer. The host writes event data
/// here before calling a semantic guest export, then calls `arena0_dealloc`
/// to release it.
///
/// # Panics
/// Panics if `len` exceeds remaining arena capacity.
pub fn io_alloc(len: usize) -> *mut u8 {
    // SAFETY: single-threaded wasm; we use raw pointer reads/writes to avoid
    // creating references to mutable statics (forbidden in edition 2024).
    unsafe {
        let offset = core::ptr::read(core::ptr::addr_of!(IO_BUMP_OFFSET));
        let aligned = offset
            .checked_add(7)
            .map(|value| value & !7)
            .expect("I/O arena alignment overflow");
        let new_offset = aligned
            .checked_add(len)
            .expect("I/O arena allocation length overflow");
        assert!(
            new_offset <= IO_ARENA_SIZE,
            "I/O arena overflow: requested {len} bytes at offset {aligned}, arena size {IO_ARENA_SIZE}"
        );
        core::ptr::write(core::ptr::addr_of_mut!(IO_BUMP_OFFSET), new_offset);
        core::ptr::addr_of_mut!((*core::ptr::addr_of_mut!(IO_ARENA)).buf)
            .cast::<u8>()
            .add(aligned)
    }
}

/// Release bytes back to the I/O arena. Resets the bump pointer
/// to the start of this allocation. Since the host does exactly
/// one alloc + one dealloc per dispatch, this effectively resets
/// the arena each time.
pub fn io_dealloc(ptr: *mut u8, _len: usize) {
    // SAFETY: single-threaded wasm; raw pointer arithmetic avoids creating
    // references to mutable statics (forbidden in edition 2024).
    unsafe {
        let base = core::ptr::addr_of!((*core::ptr::addr_of!(IO_ARENA)).buf) as usize;
        let ptr_addr = ptr as usize;
        assert!(
            ptr_addr >= base && ptr_addr <= base + IO_ARENA_SIZE,
            "io_dealloc: pointer outside I/O arena"
        );
        let offset = ptr_addr - base;
        core::ptr::write(core::ptr::addr_of_mut!(IO_BUMP_OFFSET), offset);
    }
}
