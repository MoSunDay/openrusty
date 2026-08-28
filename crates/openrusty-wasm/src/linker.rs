//! Import whitelist (namespace `openrusty`) and module ABI validation.
//!
//! Every import follows docs/wasm-abi.md: two-phase reads return the bytes
//! written (>= 0) or the negated required length (< 0).

use crate::abi;
use crate::epoch::ticks_for;
use crate::host_state::{self, HostState};
use crate::instance::{new_host_data, HeaderEdit, HostData};
use crate::linker_kv;
use crate::linker_req;
use crate::mem;
use openrusty_core::ReqCtx;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use wasmtime::{Caller, Engine, Linker, Module, Store, StoreLimitsBuilder, Trap};

/// Epoch budget for the validation probe instantiation: a module whose
/// start section loops forever must fail validation instead of hanging
/// the reload.
const PROBE_INSTANTIATE_TIMEOUT: Duration = Duration::from_secs(5);

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

    linker_req::link(&mut linker)?;
    linker_kv::link(&mut linker)?;
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

/// Instantiate `module` in a throwaway store against the whitelist.
///
/// Requires `orr_on_phase: (i32,i32) -> i32` and `orr_alloc: i32 -> i32`;
/// records whether `orr_dealloc` is present. Unknown/extra imports fail
/// instantiation naturally.
///
/// The probe instantiation is bounded by [`PROBE_INSTANTIATE_TIMEOUT`];
/// the epoch must be advancing (engine ticker running) for that budget to
/// fire.
pub fn validate_module(
    engine: &Engine,
    linker: &Linker<HostData>,
    module: &Module,
) -> Result<AbiInfo, AbiError> {
    validate_module_with_budget(engine, linker, module, PROBE_INSTANTIATE_TIMEOUT)
}

/// [`validate_module`] with an explicit instantiation epoch budget (used
/// by tests to keep the timeout path fast).
pub fn validate_module_with_budget(
    engine: &Engine,
    linker: &Linker<HostData>,
    module: &Module,
    budget: Duration,
) -> Result<AbiInfo, AbiError> {
    let mut store = Store::new(engine, probe_host_data());
    store.limiter(|d| &mut d.limits);
    store.epoch_deadline_trap();
    // The probe instantiation is bounded by an epoch budget so a module
    // with a non-terminating start section fails validation (with a
    // timeout-flavoured error) instead of hanging the reload forever.
    store.set_epoch_deadline(ticks_for(budget));
    let inst = linker
        .instantiate(&mut store, module)
        .map_err(|e| match e.downcast_ref::<Trap>() {
            Some(&Trap::Interrupt) => AbiError::Instantiate(format!(
                "instantiation timed out after {}ms (non-terminating start section?)",
                budget.as_millis()
            )),
            _ => AbiError::Instantiate(e.to_string()),
        })?;
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
        tried: Vec::new(),
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
    use std::time::{Duration, Instant};

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

    /// Regression: a module whose start section loops forever must fail
    /// validation within the probe's epoch budget instead of hanging the
    /// reload forever.
    #[test]
    fn rejects_non_terminating_start_section() {
        let src = r#"(module
            (start $s)
            (func $s (loop $l (br $l)))
            (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const 0)
            (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
            (memory (export "memory") 1))"#;
        // The probe budget needs an epoch-interrupted engine AND its
        // ticker (the real registry engine is built exactly that way).
        let ticker = crate::registry::new_engine().unwrap();
        let engine = ticker.engine();
        let linker = build_linker(engine).unwrap();
        let module = Module::new(engine, wat::parse_str(src).unwrap()).unwrap();
        let started = Instant::now();
        let outcome = validate_module_with_budget(engine, &linker, &module, Duration::from_millis(250));
        match outcome {
            Err(AbiError::Instantiate(detail)) => {
                assert!(
                    detail.contains("timed out"),
                    "expected a timeout-flavoured error, got: {detail}"
                );
            }
            other => panic!("expected an instantiation error, got: {other:?}"),
        }
        assert!(
            started.elapsed() <= Duration::from_secs(2),
            "validation must fail within its budget, took {:?}",
            started.elapsed()
        );
    }
}
