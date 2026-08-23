//! KV-store imports (`kv_get/set/del`, `kv_scan_*`): the per-plugin
//! shared state surface.
//!
//! Two-phase reads return the bytes written (>= 0) or the negated required
//! length (< 0); see docs/wasm-abi.md.

use crate::abi;
use crate::instance::HostData;
use crate::mem;
use wasmtime::{Caller, Linker};

/// Register every KV-store import on the linker.
pub(crate) fn link(linker: &mut Linker<HostData>) -> Result<(), wasmtime::Error> {
    linker.func_wrap(
        abi::NS,
        abi::KV_GET,
        |mut caller: Caller<'_, HostData>,
         key_ptr: i32,
         key_len: i32,
         out_ptr: i32,
         out_cap: i32|
         -> i32 {
            let Some(key) = mem::read_guest(&mut caller, key_ptr, key_len) else {
                return 0;
            };
            let state = caller.data().state.clone();
            match state.kv_get(&key) {
                Some(v) => mem::write_out(&mut caller, out_ptr, out_cap, &v),
                None => 0, // missing key (or unreadable pointer) => 0 bytes
            }
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::KV_SET,
        |mut caller: Caller<'_, HostData>,
         key_ptr: i32,
         key_len: i32,
         val_ptr: i32,
         val_len: i32,
         ttl_ms: i64|
         -> i32 {
            let Some(key) = mem::read_guest(&mut caller, key_ptr, key_len) else {
                return -1;
            };
            let Some(val) = mem::read_guest(&mut caller, val_ptr, val_len) else {
                return -1;
            };
            caller.data().state.kv_set(&key, val, ttl_ms);
            0
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::KV_DEL,
        |mut caller: Caller<'_, HostData>, key_ptr: i32, key_len: i32| -> i32 {
            let Some(key) = mem::read_guest(&mut caller, key_ptr, key_len) else {
                return 0;
            };
            caller.data().state.kv_del(&key) as i32
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::KV_SCAN_BEGIN,
        |mut caller: Caller<'_, HostData>, prefix_ptr: i32, prefix_len: i32| -> i32 {
            let Some(prefix) = mem::read_guest(&mut caller, prefix_ptr, prefix_len) else {
                return -1;
            };
            caller.data().state.scan_begin(&prefix) as i32
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::KV_SCAN_NEXT,
        |mut caller: Caller<'_, HostData>, cursor: i32, out_ptr: i32, out_cap: i32| -> i32 {
            if cursor < 0 {
                return -1; // invalid cursor
            }
            let state = caller.data().state.clone();
            let c = cursor as u32;
            if !state.scan_is_valid(c) {
                return -1; // invalid cursor
            }
            match state.scan_peek(c) {
                Some((k, v)) => {
                    let payload = abi::encode_kv_pair(&k, &v);
                    let r = mem::write_out(&mut caller, out_ptr, out_cap, &payload);
                    // Only consume the entry once the guest actually got it,
                    // so a too-small buffer can be retried losslessly.
                    if r >= 0 {
                        state.scan_advance(c);
                    }
                    r
                }
                None => {
                    state.scan_next(c); // exhausted: auto-remove the cursor
                    0
                }
            }
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::KV_SCAN_END,
        |caller: Caller<'_, HostData>, cursor: i32| {
            if cursor >= 0 {
                caller.data().state.scan_end(cursor as u32);
            }
        },
    )?;

    Ok(())
}
