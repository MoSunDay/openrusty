//! Raw `openrusty` host imports (see docs/wasm-abi.md).
//!
//! Safety contract for every import here is specified in docs/wasm-abi.md
//! (valid pointer/length ranges within the guest's linear memory).
#![allow(clippy::missing_safety_doc)]
//!
//! On `wasm32-unknown-unknown` these resolve to the `openrusty` import
//! namespace provided by the gateway. On host builds (workspace unit tests)
//! neutral stubs are used instead; they never panic and return
//! "nothing there" values so tests of pure logic stay safe.

use alloc::vec::Vec;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "openrusty")]
extern "C" {
    pub fn host_log(level: i32, ptr: i32, len: i32);
    pub fn host_now_ms() -> i64;

    pub fn req_meta(kp: i32, kl: i32, op: i32, oc: i32) -> i32;
    pub fn req_peer_count() -> i32;
    pub fn req_peer_get(idx: i32, op: i32, oc: i32) -> i32;
    pub fn balancer_set_peer(idx: i32) -> i32;

    pub fn kv_get(kp: i32, kl: i32, op: i32, oc: i32) -> i32;
    pub fn kv_set(kp: i32, kl: i32, vp: i32, vl: i32, ttl_ms: i64) -> i32;
    pub fn kv_del(kp: i32, kl: i32) -> i32;
    pub fn kv_scan_begin(pp: i32, pl: i32) -> i32;
    pub fn kv_scan_next(c: i32, op: i32, oc: i32) -> i32;
    pub fn kv_scan_end(c: i32);

    pub fn resp_header_get(np: i32, nl: i32, op: i32, oc: i32) -> i32;
    pub fn resp_header_set(np: i32, nl: i32, vp: i32, vl: i32) -> i32;
    pub fn resp_header_del(np: i32, nl: i32) -> i32;
    pub fn resp_body_set(ptr: i32, len: i32) -> i32;

    pub fn body_chunk(op: i32, oc: i32) -> i32;
    pub fn body_is_last() -> i32;

    pub fn cfg_get(kp: i32, kl: i32, op: i32, oc: i32) -> i32;
}

// ---------------------------------------------------------------------------
// Host-build stubs (never panic; return neutral values).
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn host_log(_level: i32, _ptr: i32, _len: i32) {}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn host_now_ms() -> i64 {
    0
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn req_meta(_kp: i32, _kl: i32, _op: i32, _oc: i32) -> i32 {
    0
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn req_peer_count() -> i32 {
    0
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn req_peer_get(_idx: i32, _op: i32, _oc: i32) -> i32 {
    0
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn balancer_set_peer(_idx: i32) -> i32 {
    0
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn kv_get(_kp: i32, _kl: i32, _op: i32, _oc: i32) -> i32 {
    0
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn kv_set(_kp: i32, _kl: i32, _vp: i32, _vl: i32, _ttl_ms: i64) -> i32 {
    0
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn kv_del(_kp: i32, _kl: i32) -> i32 {
    0
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn kv_scan_begin(_pp: i32, _pl: i32) -> i32 {
    -1
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn kv_scan_next(_c: i32, _op: i32, _oc: i32) -> i32 {
    0
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn kv_scan_end(_c: i32) {}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn resp_header_get(_np: i32, _nl: i32, _op: i32, _oc: i32) -> i32 {
    0
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn resp_header_set(_np: i32, _nl: i32, _vp: i32, _vl: i32) -> i32 {
    0
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn resp_header_del(_np: i32, _nl: i32) -> i32 {
    0
}

// The stub mirrors the success contract (`ret == len`) so host-side
// logic tests behave like the gateway.
#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn resp_body_set(_ptr: i32, len: i32) -> i32 {
    len
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn body_chunk(_op: i32, _oc: i32) -> i32 {
    0
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn body_is_last() -> i32 {
    1
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused)]
pub unsafe fn cfg_get(_kp: i32, _kl: i32, _op: i32, _oc: i32) -> i32 {
    0
}

/// Upper bound for a single host read: the guest's scratch arena is
/// [`crate::alloc_heap::HEAP_SIZE`] (1 MiB), so a host requirement above
/// it can never be satisfied -- growing that far would exhaust the arena
/// and trap the guest. A real guest also shares the arena with its own
/// state, so the cap is an upper bound, not a target.
const MAX_READ_LEN: usize = crate::alloc_heap::HEAP_SIZE;

/// Two-phase read convention (docs/wasm-abi.md): `ret >= 0` means `ret`
/// bytes were written into the provided buffer; `ret < 0` means the buffer
/// was too small and `-ret` is the required capacity.
///
/// Starts with a 256-byte buffer and retries at most 3 times total.
/// Read limit: requirements above the 1 MiB guest scratch arena
/// ([`MAX_READ_LEN`]) fail cleanly with `None` instead of growing into an
/// arena OOM trap. The same defensive `None` covers a misbehaving host:
/// a demand with no growth, or `i32::MIN` whose negation would overflow
/// `i32` (sizes are negated in `i64` first, so it cannot panic).
/// Returns `None` if the host never succeeds or reports an unreasonable
/// size. Pure with respect to the closure, which keeps it testable.
///
/// # Safety
/// `read` must treat its arguments as (out_ptr, out_cap) of a valid buffer.
pub unsafe fn read_two_phase(mut read: impl FnMut(i32, i32) -> i32) -> Option<Vec<u8>> {
    const START_LEN: usize = 256;
    let mut buf = alloc::vec![0u8; START_LEN];
    for _attempt in 0..3 {
        let ret = read(buf.as_mut_ptr() as i32, buf.len() as i32);
        if ret >= 0 {
            buf.truncate(ret as usize);
            return Some(buf);
        }
        // Negate in i64: `ret == i32::MIN` must not overflow a plain `-ret`.
        let need = (-(ret as i64)) as usize;
        // Guard against hosts reporting no growth, or more than the guest
        // scratch arena (see MAX_READ_LEN) can ever hold.
        if need <= buf.len() || need > MAX_READ_LEN {
            return None;
        }
        buf.resize(need, 0);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    // NOTE: the i32 pointer ABI matches wasm32 linear memory; on a 64-bit
    // host the pointer truncates, so these tests exercise the control
    // flow (grow / retry / give up) without writing through `out`.

    #[test]
    fn two_phase_fits_in_first_buffer() {
        let calls = Cell::new(0);
        let buf = unsafe {
            read_two_phase(|_out, cap| {
                calls.set(calls.get() + 1);
                assert_eq!(cap, 256);
                3 // pretend 3 bytes were written
            })
        };
        assert_eq!(calls.get(), 1);
        assert_eq!(buf.map(|b| b.len()), Some(3));
    }

    #[test]
    fn two_phase_grows_and_retries() {
        let calls = Cell::new(0);
        let buf = unsafe {
            read_two_phase(|_out, cap| {
                calls.set(calls.get() + 1);
                if cap < 300 {
                    -300 // demand 300 bytes of capacity
                } else {
                    300
                }
            })
        };
        assert_eq!(calls.get(), 2);
        assert_eq!(buf.map(|b| b.len()), Some(300));
    }

    #[test]
    fn two_phase_gives_up_when_host_stalls() {
        let calls = Cell::new(0);
        let buf = unsafe {
            read_two_phase(|_out, _cap| {
                calls.set(calls.get() + 1);
                -9999 // always demands more than the (already grown) buffer
            })
        };
        assert!(buf.is_none());
        assert!(calls.get() <= 3);
    }

    #[test]
    fn two_phase_satisfies_demand_up_to_arena_size() {
        let calls = Cell::new(0);
        let seen_cap = Cell::new(0i32);
        let buf = unsafe {
            read_two_phase(|_out, cap| {
                calls.set(calls.get() + 1);
                seen_cap.set(cap);
                if cap < crate::alloc_heap::HEAP_SIZE as i32 {
                    -(crate::alloc_heap::HEAP_SIZE as i32)
                } else {
                    crate::alloc_heap::HEAP_SIZE as i32
                }
            })
        };
        assert_eq!(calls.get(), 2);
        assert_eq!(seen_cap.get(), crate::alloc_heap::HEAP_SIZE as i32);
        assert_eq!(buf.map(|b| b.len()), Some(crate::alloc_heap::HEAP_SIZE));
    }

    #[test]
    fn two_phase_refuses_demand_beyond_arena() {
        let calls = Cell::new(0);
        let buf = unsafe {
            read_two_phase(|_out, _cap| {
                calls.set(calls.get() + 1);
                -(crate::alloc_heap::HEAP_SIZE as i64 + 1) as i32
            })
        };
        // Clean refusal: no retry, no 1 MiB+ allocation, no arena OOM trap.
        assert!(buf.is_none());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn two_phase_growth_never_exceeds_arena_cap() {
        let calls = Cell::new(0);
        let max_cap = Cell::new(0i32);
        let buf = unsafe {
            read_two_phase(|_out, cap| {
                calls.set(calls.get() + 1);
                if cap > max_cap.get() {
                    max_cap.set(cap);
                }
                // Escalating demands, the second one past the arena cap.
                if cap < 4096 {
                    -4096
                } else {
                    -(crate::alloc_heap::HEAP_SIZE as i64 + 7) as i32
                }
            })
        };
        assert!(buf.is_none());
        assert_eq!(calls.get(), 2);
        assert!(max_cap.get() <= crate::alloc_heap::HEAP_SIZE as i32);
    }

    #[test]
    fn two_phase_survives_i32_min_demand() {
        // Hostile host: `-(i32::MIN)` overflows i32, so the negation must go
        // through i64 and land in the "unreasonable size" bucket instead of
        // panicking (debug) or wrapping (release).
        let calls = Cell::new(0);
        let buf = unsafe {
            read_two_phase(|_out, _cap| {
                calls.set(calls.get() + 1);
                i32::MIN
            })
        };
        assert!(buf.is_none());
        assert_eq!(calls.get(), 1);
    }
}
