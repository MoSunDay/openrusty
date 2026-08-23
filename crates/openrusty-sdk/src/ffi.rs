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

/// Two-phase read convention (docs/wasm-abi.md): `ret >= 0` means `ret`
/// bytes were written into the provided buffer; `ret < 0` means the buffer
/// was too small and `-ret` is the required capacity.
///
/// Starts with a 256-byte buffer and retries at most 3 times total.
/// Returns `None` if the host never succeeds or reports an unreasonable
/// size. Pure with respect to the closure, which keeps it testable.
///
/// # Safety
/// `read` must treat its arguments as (out_ptr, out_cap) of a valid buffer.
pub unsafe fn read_two_phase(read: impl Fn(i32, i32) -> i32) -> Option<Vec<u8>> {
    const MAX_LEN: usize = 16 * 1024 * 1024;
    let mut buf = alloc::vec![0u8; 256];
    for _attempt in 0..3 {
        let ret = read(buf.as_mut_ptr() as i32, buf.len() as i32);
        if ret >= 0 {
            buf.truncate(ret as usize);
            return Some(buf);
        }
        let need = (-ret) as usize;
        // Guard against hosts reporting no growth or absurd sizes.
        if need <= buf.len() || need > MAX_LEN {
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
}
