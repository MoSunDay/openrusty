//! Import whitelist (namespace `openrusty`) and module ABI validation.
//!
//! Every import follows docs/wasm-abi.md: two-phase reads return the bytes
//! written (>= 0) or the negated required length (< 0).

use crate::abi;
use crate::host_state::{self, HostState};
use crate::instance::{new_host_data, HeaderEdit, HostData, PeerView};
use crate::mem;
use openrusty_core::ReqCtx;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use wasmtime::{Caller, Engine, Linker, Module, Store, StoreLimitsBuilder};

/// ABI validation failure.
#[derive(Debug, Error)]
pub enum AbiError {
    #[error("instantiation failed: {0}")]
    Instantiate(String),
    #[error("missing or mistyped export: {0}")]
    BadExport(String),
}

/// What ABI validation learned about a module.
#[derive(Debug, Clone, Copy)]
pub struct AbiInfo {
    pub has_dealloc: bool,
}

/// Build the linker containing the full whitelisted import set.
pub fn build_linker(engine: &Engine) -> Result<Linker<HostData>, wasmtime::Error> {
    let mut linker: Linker<HostData> = Linker::new(engine);

    linker.func_wrap(
        abi::NS,
        abi::HOST_LOG,
        |mut caller: Caller<'_, HostData>, level: i32, ptr: i32, len: i32| {
            let Some(msg) = mem::read_guest(&mut caller, ptr, len) else {
                return;
            };
            let msg = String::from_utf8_lossy(&msg);
            let plugin = caller.data().state.name().to_string();
            match level {
                1 => tracing::error!(plugin = %plugin, "{msg}"),
                2 => tracing::warn!(plugin = %plugin, "{msg}"),
                3 => tracing::info!(plugin = %plugin, "{msg}"),
                _ => tracing::debug!(plugin = %plugin, "{msg}"), // 4 = debug
            }
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::HOST_NOW_MS,
        |_caller: Caller<'_, HostData>| -> i64 { host_state::now_ms() as i64 },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::REQ_META,
        |mut caller: Caller<'_, HostData>,
         key_ptr: i32,
         key_len: i32,
         out_ptr: i32,
         out_cap: i32|
         -> i32 {
            let Some(key) = mem::read_guest_str(&mut caller, key_ptr, key_len) else {
                return 0;
            };
            let payload = {
                let d = caller.data();
                req_meta_payload(&d.ctx, &key)
            };
            mem::write_out(&mut caller, out_ptr, out_cap, &payload)
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::REQ_PEER_COUNT,
        |caller: Caller<'_, HostData>| -> i32 { caller.data().peers.len() as i32 },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::REQ_PEER_GET,
        |mut caller: Caller<'_, HostData>, idx: i32, out_ptr: i32, out_cap: i32| -> i32 {
            // Invalid index is not a buffer problem: report -2 (negatives are
            // otherwise reserved for -required_length).
            let Some(peer) = caller.data().peers.get(idx as usize) else {
                return -2;
            };
            let payload = peer_payload(peer);
            mem::write_out(&mut caller, out_ptr, out_cap, &payload)
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::BALANCER_SET_PEER,
        |mut caller: Caller<'_, HostData>, idx: i32| -> i32 {
            let d = caller.data_mut();
            if idx >= 0 && (idx as usize) < d.peers.len() {
                // Health is enforced by the scheduler, not here.
                d.ctx.peer_index = Some(idx as u32);
                0
            } else {
                -1
            }
        },
    )?;

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

    linker.func_wrap(
        abi::NS,
        abi::RESP_HEADER_GET,
        |mut caller: Caller<'_, HostData>,
         name_ptr: i32,
         name_len: i32,
         out_ptr: i32,
         out_cap: i32|
         -> i32 {
            let Some(name) = mem::read_guest_str(&mut caller, name_ptr, name_len) else {
                return 0;
            };
            let val = {
                let d = caller.data();
                d.resp_headers
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(&name))
                    .map(|(_, v)| v.clone())
            };
            match val {
                Some(v) => mem::write_out(&mut caller, out_ptr, out_cap, v.as_bytes()),
                None => 0,
            }
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::RESP_HEADER_SET,
        |mut caller: Caller<'_, HostData>,
         name_ptr: i32,
         name_len: i32,
         val_ptr: i32,
         val_len: i32|
         -> i32 {
            let Some(name) = mem::read_guest_str(&mut caller, name_ptr, name_len) else {
                return -1;
            };
            let Some(val) = mem::read_guest_str(&mut caller, val_ptr, val_len) else {
                return -1;
            };
            let d = caller.data_mut();
            d.resp_edits
                .push(HeaderEdit::Set(name.clone(), val.clone()));
            match d
                .resp_headers
                .iter_mut()
                .find(|(n, _)| n.eq_ignore_ascii_case(&name))
            {
                Some(entry) => entry.1 = val,             // replace in place
                None => d.resp_headers.push((name, val)), // append new
            }
            0
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::RESP_HEADER_DEL,
        |mut caller: Caller<'_, HostData>, name_ptr: i32, name_len: i32| -> i32 {
            let Some(name) = mem::read_guest_str(&mut caller, name_ptr, name_len) else {
                return -1;
            };
            let d = caller.data_mut();
            d.resp_edits.push(HeaderEdit::Del(name.clone()));
            d.resp_headers
                .retain(|(n, _)| !n.eq_ignore_ascii_case(&name));
            0
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::BODY_CHUNK,
        |mut caller: Caller<'_, HostData>, out_ptr: i32, out_cap: i32| -> i32 {
            let chunk = caller.data().body_chunk.clone();
            mem::write_out(&mut caller, out_ptr, out_cap, &chunk)
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::BODY_IS_LAST,
        |caller: Caller<'_, HostData>| -> i32 {
            if caller.data().body_last {
                1
            } else {
                0
            }
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::CFG_GET,
        |mut caller: Caller<'_, HostData>,
         key_ptr: i32,
         key_len: i32,
         out_ptr: i32,
         out_cap: i32|
         -> i32 {
            let Some(key) = mem::read_guest_str(&mut caller, key_ptr, key_len) else {
                return 0;
            };
            let val = { caller.data().settings.get(&key).cloned() };
            match val {
                Some(v) => mem::write_out(&mut caller, out_ptr, out_cap, v.as_bytes()),
                None => 0,
            }
        },
    )?;

    Ok(linker)
}

/// Payload for `req_meta` (pure). Unknown keys yield an empty payload.
fn req_meta_payload(ctx: &ReqCtx, key: &str) -> Vec<u8> {
    match key {
        "method" => ctx.method.clone().into_bytes(),
        "path" => ctx.path.clone().into_bytes(),
        "query" => ctx.query.clone().into_bytes(),
        "version" => ctx.version.clone().into_bytes(),
        "client_ip" => ctx.client_addr.ip().to_string().into_bytes(),
        "upstream" => ctx.upstream.clone().unwrap_or_default().into_bytes(),
        "headers" => {
            let items: Vec<String> = ctx
                .headers
                .iter()
                .map(|(n, v)| format!("{n}: {v}"))
                .collect();
            let refs: Vec<&[u8]> = items.iter().map(|s| s.as_bytes()).collect();
            abi::encode_tlv(&refs)
        }
        k if let Some(name) = k.strip_prefix("header:") => ctx
            .header(name)
            .map(|v| v.as_bytes().to_vec())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// TLV payload for `req_peer_get`: [name, addr, healthy("1"/"0")].
fn peer_payload(peer: &PeerView) -> Vec<u8> {
    let healthy = if peer.healthy { "1" } else { "0" };
    abi::encode_tlv(&[
        peer.name.as_bytes(),
        peer.addr.as_bytes(),
        healthy.as_bytes(),
    ])
}

/// Instantiate `module` in a throwaway store against the whitelist.
///
/// Requires `orr_on_phase: (i32,i32) -> i32` and `orr_alloc: i32 -> i32`;
/// records whether `orr_dealloc` is present. Unknown/extra imports fail
/// instantiation naturally.
pub fn validate_module(
    engine: &Engine,
    linker: &Linker<HostData>,
    module: &Module,
) -> Result<AbiInfo, AbiError> {
    let mut store = Store::new(engine, probe_host_data());
    store.limiter(|d| &mut d.limits);
    let inst = linker
        .instantiate(&mut store, module)
        .map_err(|e| AbiError::Instantiate(e.to_string()))?;
    inst.get_typed_func::<(i32, i32), i32>(&mut store, abi::EXPORT_ON_PHASE)
        .map_err(|e| AbiError::BadExport(format!("{}: {e}", abi::EXPORT_ON_PHASE)))?;
    inst.get_typed_func::<i32, i32>(&mut store, abi::EXPORT_ALLOC)
        .map_err(|e| AbiError::BadExport(format!("{}: {e}", abi::EXPORT_ALLOC)))?;
    let has_dealloc = inst.get_func(&mut store, abi::EXPORT_DEALLOC).is_some();
    Ok(AbiInfo { has_dealloc })
}

/// Dummy HostData used only for throwaway validation instances. The memory
/// ceiling keeps hostile static memories from eating host RAM at load time.
fn probe_host_data() -> HostData {
    let ctx = ReqCtx {
        method: "GET".into(),
        path: "/".into(),
        query: String::new(),
        version: "HTTP/1.1".into(),
        client_addr: "0.0.0.0:0".parse().unwrap(),
        headers: Vec::new(),
        route_index: None,
        upstream: None,
        peer_index: None,
        attempts: 0,
    };
    let mut d = new_host_data(
        ctx,
        Vec::new(),
        Arc::new(HostState::new("__probe__".into())),
        Arc::new(HashMap::new()),
    );
    d.limits = StoreLimitsBuilder::new().memory_size(256 << 20).build();
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
(module
  (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const 0)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
  (func (export "orr_dealloc") (param i32 i32))
  (memory (export "memory") 1))
"#;

    #[test]
    fn validates_good_module() {
        let engine = Engine::default();
        let linker = build_linker(&engine).unwrap();
        let module = Module::new(&engine, wat::parse_str(GOOD).unwrap()).unwrap();
        let info = validate_module(&engine, &linker, &module).unwrap();
        assert!(info.has_dealloc);
    }

    #[test]
    fn rejects_missing_alloc() {
        let src = r#"(module
            (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const 0)
            (memory (export "memory") 1))"#;
        let engine = Engine::default();
        let linker = build_linker(&engine).unwrap();
        let module = Module::new(&engine, wat::parse_str(src).unwrap()).unwrap();
        assert!(matches!(
            validate_module(&engine, &linker, &module),
            Err(AbiError::BadExport(_))
        ));
    }

    #[test]
    fn rejects_unknown_import() {
        let src = r#"(module
            (import "openrusty" "no_such_import" (func))
            (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const 0)
            (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
            (memory (export "memory") 1))"#;
        let engine = Engine::default();
        let linker = build_linker(&engine).unwrap();
        let module = Module::new(&engine, wat::parse_str(src).unwrap()).unwrap();
        assert!(matches!(
            validate_module(&engine, &linker, &module),
            Err(AbiError::Instantiate(_))
        ));
    }
}
