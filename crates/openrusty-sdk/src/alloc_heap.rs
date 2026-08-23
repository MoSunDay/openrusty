//! Bump allocator for wasm guests (no free; memory is reclaimed when the
//! host drops the store).
//!
//! The ABI requires guests to export `orr_alloc`; see the crate-root
//! `export_allocators!` macro, which wires [`Bump`] as the global
//! allocator. Allocations are 8-aligned slices of a 1 MiB static arena;
//! exhaustion returns null/0.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Arena size: 1 MiB.
pub const HEAP_SIZE: usize = 1024 * 1024;

/// The arena lives in an `UnsafeCell` so handing out raw (writable)
/// pointers into it is sound.
struct Heap(UnsafeCell<[u8; HEAP_SIZE]>);

// SAFETY: byte ranges are handed out exactly once via the atomic water
// mark; no Rust reference into the arena is ever created.
unsafe impl Sync for Heap {}

static HEAP: Heap = Heap(UnsafeCell::new([0; HEAP_SIZE]));

/// Absolute address of the arena's first byte.
fn heap_base() -> usize {
    HEAP.0.get() as usize
}

/// Water mark: bytes of the arena handed out so far.
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Zero-sized [`GlobalAlloc`] handle over the static arena.
pub struct Bump;

unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match bump_offset(&NEXT, HEAP_SIZE, layout.size(), layout.align()) {
            Some(off) => (heap_base() + off) as *mut u8,
            None => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Bump allocator: nothing to free.
    }
}

/// Allocate `size` bytes (8-aligned) for the host-facing `orr_alloc`.
/// Returns an absolute address, or `0` on exhaustion.
pub fn alloc(size: usize) -> usize {
    match bump_offset(&NEXT, HEAP_SIZE, size, 8) {
        Some(off) => heap_base() + off,
        None => 0,
    }
}

/// Pure core of the allocator: bump `next` and return the allocated
/// offset within an arena of `heap_size` bytes, or `None` on exhaustion
/// or bad alignment. Lock-free via CAS loop.
pub fn bump_offset(
    next: &AtomicUsize,
    heap_size: usize,
    size: usize,
    align: usize,
) -> Option<usize> {
    if !align.is_power_of_two() {
        return None;
    }
    loop {
        let cur = next.load(Ordering::Relaxed);
        let start = align_up(cur, align)?;
        let end = start.checked_add(size)?;
        if end > heap_size {
            return None;
        }
        match next.compare_exchange_weak(cur, end, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => return Some(start),
            Err(_) => continue,
        }
    }
}

/// Round `value` up to the next multiple of `align` (power of two).
pub fn align_up(value: usize, align: usize) -> Option<usize> {
    if !align.is_power_of_two() {
        return None;
    }
    let mask = align - 1;
    value.checked_add(mask).map(|v| v & !mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_up_rounds_up() {
        assert_eq!(align_up(0, 8), Some(0));
        assert_eq!(align_up(1, 8), Some(8));
        assert_eq!(align_up(8, 8), Some(8));
        assert_eq!(align_up(9, 8), Some(16));
        assert_eq!(align_up(7, 1), Some(7));
        assert_eq!(align_up(3, 0), None);
        assert_eq!(align_up(3, 6), None);
        assert_eq!(align_up(usize::MAX, 8), None);
    }

    #[test]
    fn bump_allocates_sequential_aligned_offsets() {
        let next = AtomicUsize::new(0);
        assert_eq!(bump_offset(&next, 1024, 16, 8), Some(0));
        assert_eq!(bump_offset(&next, 1024, 10, 8), Some(16));
        assert_eq!(bump_offset(&next, 1024, 1, 16), Some(32));
        assert_eq!(bump_offset(&next, 1024, 4, 8), Some(40));
        assert_eq!(next.load(Ordering::Relaxed), 44);
    }

    #[test]
    fn bump_returns_none_on_exhaustion() {
        let next = AtomicUsize::new(0);
        assert_eq!(bump_offset(&next, 64, 64, 8), Some(0));
        assert_eq!(bump_offset(&next, 64, 1, 8), None);
        // Water mark untouched after failure.
        assert_eq!(next.load(Ordering::Relaxed), 64);
    }

    #[test]
    fn bump_rejects_bad_alignment() {
        let next = AtomicUsize::new(0);
        assert_eq!(bump_offset(&next, 64, 8, 3), None);
        assert_eq!(next.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn global_alloc_yields_writable_memory() {
        // Safe to share across tests: offsets only move forward.
        unsafe {
            let ptr = Bump.alloc(Layout::from_size_align(32, 8).unwrap());
            assert!(!ptr.is_null());
            core::ptr::write_bytes(ptr, 0xAB, 32);
            assert_eq!(*ptr.add(31), 0xAB);
        }
    }
}
