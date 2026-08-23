//! Guest linear-memory helpers used by the linker imports.
//!
//! Memory is resolved lazily on every call via the caller's `memory`
//! export; all helpers are defensive and never panic on hostile pointers.

use crate::abi;
use crate::instance::HostData;
use wasmtime::{Caller, Memory};

/// Find the guest memory export for the current call.
pub fn memory_of(caller: &mut Caller<'_, HostData>) -> Option<Memory> {
    caller
        .get_export(abi::MEMORY_EXPORT)
        .and_then(|e| e.into_memory())
}

/// Read `len` bytes at `ptr` from guest memory; `None` on any invalid
/// access (negative pointer/length, missing memory, out of bounds).
pub fn read_guest(caller: &mut Caller<'_, HostData>, ptr: i32, len: i32) -> Option<Vec<u8>> {
    if ptr < 0 || len < 0 {
        return None;
    }
    let mem = memory_of(caller)?;
    let mut buf = vec![0u8; len as usize];
    mem.read(&*caller, ptr as usize, &mut buf).ok()?;
    Some(buf)
}

/// Read guest bytes as a (lossy) UTF-8 string.
pub fn read_guest_str(caller: &mut Caller<'_, HostData>, ptr: i32, len: i32) -> Option<String> {
    read_guest(caller, ptr, len).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Two-phase write of `payload` into the guest buffer `(out_ptr, out_cap)`.
///
/// * payload fits: it is written and its length is returned (>= 0);
/// * otherwise the negated required length is returned so the guest can
///   grow its buffer and retry;
/// * invalid pointers/memory are reported like "too small" (never panic).
pub fn write_out(
    caller: &mut Caller<'_, HostData>,
    out_ptr: i32,
    out_cap: i32,
    payload: &[u8],
) -> i32 {
    if payload.is_empty() {
        return 0;
    }
    let required = -(i32::try_from(payload.len()).unwrap_or(i32::MAX));
    if out_ptr < 0 || out_cap < 0 || (out_cap as usize) < payload.len() {
        return required;
    }
    let Some(mem) = memory_of(caller) else {
        return required;
    };
    if mem.write(&mut *caller, out_ptr as usize, payload).is_err() {
        return required;
    }
    payload.len() as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_state::HostState;
    use crate::instance::new_host_data;
    use crate::linker::build_linker;
    use openrusty_core::ReqCtx;
    use std::collections::HashMap;
    use std::sync::Arc;
    use wasmtime::{Engine, Linker, Module, Store};

    /// Guest echoes back req_meta("path") so the host-side read/write
    /// helpers run inside a real instance.
    const ECHO_PATH: &str = r#"
(module
  (import "openrusty" "req_meta" (func $meta (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "path")
  (func (export "orr_on_phase") (param i32 i32) (result i32)
    ;; First call with a too-small buffer must return -required.
    (if (i32.ne (call $meta (i32.const 0) (i32.const 4) (i32.const 100) (i32.const 0))
                (i32.const -1))
        (then unreachable))
    ;; Second call with enough buffer writes the path.
    (if (i32.ne (call $meta (i32.const 0) (i32.const 4) (i32.const 100) (i32.const 16))
                (i32.const 1))
        (then unreachable))
    i32.const 0)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))
"#;

    fn ctx() -> ReqCtx {
        ReqCtx {
            method: "GET".into(),
            path: "/".into(),
            query: String::new(),
            version: "HTTP/1.1".into(),
            client_addr: "127.0.0.1:1".parse().unwrap(),
            headers: Vec::new(),
            route_index: None,
            upstream: None,
            peer_index: None,
            attempts: 0,
        }
    }

    #[test]
    fn two_phase_read_write_in_guest() {
        let engine = Engine::default();
        let linker: Linker<HostData> = build_linker(&engine).unwrap();
        let module = Module::new(&engine, wat::parse_str(ECHO_PATH).unwrap()).unwrap();
        let host = new_host_data(
            ctx(),
            Vec::new(),
            Arc::new(HostState::new("t".into())),
            Arc::new(HashMap::new()),
        );
        let mut store = Store::new(&engine, host);
        let inst = linker.instantiate(&mut store, &module).unwrap();
        let f = inst
            .get_typed_func::<(i32, i32), i32>(&mut store, "orr_on_phase")
            .unwrap();
        assert_eq!(f.call(&mut store, (0, 0)).unwrap(), 0);
        let mem = inst.get_memory(&mut store, "memory").unwrap();
        let mut buf = [0u8; 1];
        mem.read(&store, 100, &mut buf).unwrap();
        assert_eq!(buf[0], b'/');
    }
}
