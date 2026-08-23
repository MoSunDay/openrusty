//! OpenRusty guest-side WASM plugin SDK.
//!
//! Plugins are `#![no_std]` cdylibs targeting `wasm32-unknown-unknown`; the
//! gateway links them against the `openrusty` import namespace (see
//! `docs/wasm-abi.md`). On host builds the imports become neutral stubs so
//! pure plugin logic stays unit-testable with `cargo test`.
//!
//! Minimal plugin skeleton:
//!
//! ```ignore
//! #![no_std]
//! extern crate alloc;
//! use openrusty_sdk::Decision;
//!
//! #[cfg(target_arch = "wasm32")]
//! openrusty_sdk::export_allocators!();
//!
//! #[openrusty_sdk::phase(access)]
//! fn on_access() -> Decision { Decision::Ok }
//!
//! openrusty_sdk::dispatch! { access => on_access }
//! ```
#![no_std]

extern crate alloc;

pub use openrusty_macros::phase;

pub mod alloc_heap;
pub mod dispatch;
pub mod ffi;
pub mod host;
pub mod sched;
pub mod url;

pub use dispatch::Decision;

/// Export the guest allocator surface required by the ABI (`orr_alloc` /
/// `orr_dealloc`) and wire the global allocator to the SDK bump heap.
/// Invoke exactly once, at crate root, in each wasm plugin crate.
///
/// On host builds gate the invocation behind
/// `#[cfg(target_arch = "wasm32")]` so unit tests keep the system
/// allocator.
#[macro_export]
macro_rules! export_allocators {
    () => {
        #[global_allocator]
        static ALLOC: $crate::alloc_heap::Bump = $crate::alloc_heap::Bump;

        #[no_mangle]
        pub extern "C" fn orr_alloc(size: i32) -> i32 {
            $crate::alloc_heap::alloc(size as usize) as i32
        }

        #[no_mangle]
        pub extern "C" fn orr_dealloc(_ptr: i32, _size: i32) {}
    };
}

// Wasm guests have no std panic runtime; provide the handler here. Traps
// immediately -- the host failure policy (fail_open/fail_closed) takes over.
// Host builds use the std handler instead.
#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn wasm_panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
