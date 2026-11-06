//! Request-rewrite imports (`req_set_uri`, `req_set_query`,
//! `req_set_header`, `req_del_header`) and their pure helpers.
//!
//! Mirrors the resp_header_* dual pattern: each import mutates `ctx`
//! immediately (so later plugins in the chain see the rewrite) and
//! records a `ReqEdit` for the server to replay on the request it
//! forwards upstream. Invalid input is refused with `-1` and leaves
//! both `ctx` and the edit log untouched.

use crate::abi;
use crate::instance::{HostData, ReqEdit};
use crate::mem;
use wasmtime::{Caller, Linker};

/// Ceiling for one rewritten URI or query string.
const MAX_PART_BYTES: usize = 8192;

/// Header the rewrite imports refuse to touch: the forwarded body
/// length is derived from the buffered request body, not the header.
const PROTECTED_HEADER: &str = "content-length";

/// Register the request-rewrite imports on the linker.
pub(crate) fn link(linker: &mut Linker<HostData>) -> Result<(), wasmtime::Error> {
    linker.func_wrap(
        abi::NS,
        abi::REQ_SET_URI,
        |mut caller: Caller<'_, HostData>, ptr: i32, len: i32| -> i32 {
            let Some(value) = mem::read_guest_str(&mut caller, ptr, len) else {
                return -1;
            };
            if !valid_uri(&value) {
                return -1;
            }
            let d = caller.data_mut();
            d.ctx.path = value.clone();
            d.req_edits.push(ReqEdit::SetUri(value));
            0
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::REQ_SET_QUERY,
        |mut caller: Caller<'_, HostData>, ptr: i32, len: i32| -> i32 {
            let Some(value) = mem::read_guest_str(&mut caller, ptr, len) else {
                return -1;
            };
            if !valid_query(&value) {
                return -1;
            }
            let d = caller.data_mut();
            d.ctx.query = value.clone();
            d.req_edits.push(ReqEdit::SetQuery(value));
            0
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::REQ_SET_HEADER,
        |mut caller: Caller<'_, HostData>,
         name_ptr: i32,
         name_len: i32,
         val_ptr: i32,
         val_len: i32|
         -> i32 {
            // Read and validate exactly like resp_header_set: -1 only on
            // an unreadable (out-of-bounds) name or value.
            let Some(name) = mem::read_guest_str(&mut caller, name_ptr, name_len) else {
                return -1;
            };
            let Some(val) = mem::read_guest_str(&mut caller, val_ptr, val_len) else {
                return -1;
            };
            if is_protected_header(&name) {
                return -1;
            }
            let d = caller.data_mut();
            set_header(&mut d.ctx.headers, &name, &val);
            d.req_edits.push(ReqEdit::SetHeader(name, val));
            0
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::REQ_DEL_HEADER,
        |mut caller: Caller<'_, HostData>, name_ptr: i32, name_len: i32| -> i32 {
            let Some(name) = mem::read_guest_str(&mut caller, name_ptr, name_len) else {
                return -1;
            };
            // Protected names keep reporting success (the read itself was
            // valid, mirroring resp_header_del's always-0 success path)
            // but neither the live headers nor the edit log change.
            if is_protected_header(&name) {
                return 0;
            }
            let d = caller.data_mut();
            del_header(&mut d.ctx.headers, &name);
            d.req_edits.push(ReqEdit::DelHeader(name));
            0
        },
    )?;

    Ok(())
}

/// Absolute path: non-empty, leading `/`, no query/fragment separator,
/// no control characters, at most [`MAX_PART_BYTES`] bytes.
fn valid_uri(value: &str) -> bool {
    let bad_char = |c: char| c == '?' || c == '#' || c.is_control();
    !value.is_empty()
        && value.starts_with('/')
        && value.len() <= MAX_PART_BYTES
        && !value.chars().any(bad_char)
}

/// Raw query string: at most [`MAX_PART_BYTES`] bytes, no control
/// characters, and no leading `?` (the separator belongs to the URI).
/// Empty clears the query.
fn valid_query(value: &str) -> bool {
    value.len() <= MAX_PART_BYTES
        && !value.starts_with('?')
        && !value.chars().any(|c| c.is_control())
}

fn is_protected_header(name: &str) -> bool {
    name.eq_ignore_ascii_case(PROTECTED_HEADER)
}

/// Replace-all semantics: drop every case-insensitive name match, then
/// append the new pair (order-preserving for the untouched headers).
fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
    headers.push((name.to_string(), value.to_string()));
}

/// Drop every case-insensitive name match.
fn del_header(headers: &mut Vec<(String, String)>, name: &str) {
    headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
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
    use wasmtime::{Engine, Module, Store};

    fn ctx() -> ReqCtx {
        ReqCtx {
            method: "GET".into(),
            path: "/echo".into(),
            query: "task=t1".into(),
            version: "HTTP/1.1".into(),
            client_addr: "127.0.0.1:1".parse().unwrap(),
            headers: Vec::new(),
            route_index: None,
            upstream: None,
            peer_index: None,
            attempts: 0,
            tried: Vec::new(),
        }
    }

    fn host_data() -> HostData {
        new_host_data(
            ctx(),
            Vec::new(),
            Arc::new(HostState::new("t".into())),
            Arc::new(HashMap::new()),
        )
    }

    fn call(src: &str, host: HostData) -> Store<HostData> {
        let engine = Engine::default();
        let linker = build_linker(&engine).unwrap();
        let module = Module::new(&engine, wat::parse_str(src).unwrap()).unwrap();
        let mut store = Store::new(&engine, host);
        let inst = linker.instantiate(&mut store, &module).unwrap();
        let f = inst
            .get_typed_func::<(i32, i32), i32>(&mut store, "orr_on_phase")
            .unwrap();
        assert_eq!(f.call(&mut store, (0, 0)).unwrap(), 0);
        store
    }

    #[test]
    fn uri_validation() {
        assert!(valid_uri("/v1/chat"));
        assert!(valid_uri("/a%20b/c"));
        assert!(!valid_uri(""));
        assert!(!valid_uri("v1/chat"));
        assert!(!valid_uri("/v1?q=1"));
        assert!(!valid_uri("/v1#frag"));
        assert!(!valid_uri("/v1\u{0}x"));
        assert!(!valid_uri("/v1\u{7}x"));
        assert!(valid_uri(&"/".repeat(MAX_PART_BYTES)));
        assert!(!valid_uri(&format!("/{}", "a".repeat(MAX_PART_BYTES))));
    }

    #[test]
    fn query_validation() {
        assert!(valid_query("model=x&top_k=5"));
        assert!(valid_query(""));
        assert!(!valid_query("?model=x"));
        assert!(!valid_query("a=b\u{1}"));
        assert!(valid_query(&"a".repeat(MAX_PART_BYTES)));
        assert!(!valid_query(&"a".repeat(MAX_PART_BYTES + 1)));
    }

    #[test]
    fn set_header_replaces_all_case_insensitive_matches() {
        let mut headers = vec![
            ("X-Keep".to_string(), "one".to_string()),
            ("x-keep".to_string(), "two".to_string()),
            ("Other".to_string(), "v".to_string()),
        ];
        set_header(&mut headers, "X-KEEP", "new");
        assert_eq!(
            headers,
            vec![
                ("Other".to_string(), "v".to_string()),
                ("X-KEEP".to_string(), "new".to_string()),
            ]
        );
    }

    #[test]
    fn del_header_removes_all_case_insensitive_matches() {
        let mut headers = vec![
            ("X-Keep".to_string(), "one".to_string()),
            ("x-keep".to_string(), "two".to_string()),
            ("Other".to_string(), "v".to_string()),
        ];
        del_header(&mut headers, "x-KEEP");
        assert_eq!(headers, vec![("Other".to_string(), "v".to_string())]);
    }

    #[test]
    fn content_length_is_protected() {
        assert!(is_protected_header("content-length"));
        assert!(is_protected_header("Content-Length"));
        assert!(!is_protected_header("content-type"));
    }

    #[test]
    fn set_and_del_roundtrip_on_ctx_facts() {
        let mut c = ctx();
        set_header(&mut c.headers, "X-Model", "big");
        del_header(&mut c.headers, "x-model");
        assert!(c.headers.is_empty());
    }

    /// The happy path through a real instance: every rewrite mutates the
    /// live ctx facts and records one edit, in call order.
    #[test]
    fn rewrites_mutate_ctx_and_record_edits_in_order() {
        const REWRITE_MOD: &str = r#"
(module
  (import "openrusty" "req_set_uri" (func $set_uri (param i32 i32) (result i32)))
  (import "openrusty" "req_set_query" (func $set_query (param i32 i32) (result i32)))
  (import "openrusty" "req_set_header" (func $set_hdr (param i32 i32 i32 i32) (result i32)))
  (import "openrusty" "req_del_header" (func $del_hdr (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "/v2/chat")
  (data (i32.const 16) "model=x")
  (data (i32.const 32) "X-Model")
  (data (i32.const 48) "big")
  (data (i32.const 64) "x-trace")
  (data (i32.const 80) "Content-Length")
  (data (i32.const 104) "5")
  (func (export "orr_on_phase") (param i32 i32) (result i32)
    ;; all four rewrites must report success
    (if (i32.ne (call $set_uri (i32.const 0) (i32.const 8)) (i32.const 0))
        (then unreachable))
    (if (i32.ne (call $set_query (i32.const 16) (i32.const 7)) (i32.const 0))
        (then unreachable))
    (if (i32.ne (call $set_hdr (i32.const 32) (i32.const 7)
                              (i32.const 48) (i32.const 3)) (i32.const 0))
        (then unreachable))
    (if (i32.ne (call $del_hdr (i32.const 64) (i32.const 7)) (i32.const 0))
        (then unreachable))
    ;; content-length is protected: set refuses, del is a no-op success
    (if (i32.ne (call $set_hdr (i32.const 80) (i32.const 14)
                              (i32.const 104) (i32.const 1)) (i32.const -1))
        (then unreachable))
    (drop (call $del_hdr (i32.const 80) (i32.const 14)))
    i32.const 0)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))
"#;
        let mut host = host_data();
        host.ctx.headers = vec![
            ("X-Trace".into(), "one".into()),
            ("x-trace".into(), "two".into()),
            ("x-model".into(), "small".into()),
        ];
        let store = call(REWRITE_MOD, host);
        let d = store.data();
        assert_eq!(d.ctx.path, "/v2/chat");
        assert_eq!(d.ctx.query, "model=x");
        assert_eq!(
            d.ctx.headers,
            vec![("X-Model".to_string(), "big".to_string())]
        );
        assert_eq!(
            d.req_edits,
            vec![
                ReqEdit::SetUri("/v2/chat".into()),
                ReqEdit::SetQuery("model=x".into()),
                ReqEdit::SetHeader("X-Model".into(), "big".into()),
                ReqEdit::DelHeader("x-trace".into()),
            ]
        );
    }

    /// Invalid rewrites are refused with -1 and leave the ctx facts and
    /// the edit log untouched; an empty query is a valid clear.
    #[test]
    fn invalid_rewrites_leave_state_untouched() {
        const BAD_MOD: &str = r#"
(module
  (import "openrusty" "req_set_uri" (func $set_uri (param i32 i32) (result i32)))
  (import "openrusty" "req_set_query" (func $set_query (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "no-slash")
  (data (i32.const 16) "/with?q=1")
  (data (i32.const 40) "?lead")
  (func (export "orr_on_phase") (param i32 i32) (result i32)
    (if (i32.ne (call $set_uri (i32.const 0) (i32.const 8)) (i32.const -1))
        (then unreachable))
    (if (i32.ne (call $set_uri (i32.const 16) (i32.const 9)) (i32.const -1))
        (then unreachable))
    (if (i32.ne (call $set_query (i32.const 40) (i32.const 5)) (i32.const -1))
        (then unreachable))
    ;; empty query is a valid clear
    (if (i32.ne (call $set_query (i32.const 0) (i32.const 0)) (i32.const 0))
        (then unreachable))
    i32.const 0)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))
"#;
        let store = call(BAD_MOD, host_data());
        let d = store.data();
        assert_eq!(d.ctx.path, "/echo");
        assert_eq!(d.ctx.query, "");
        assert_eq!(d.req_edits, vec![ReqEdit::SetQuery(String::new())]);
    }
}
