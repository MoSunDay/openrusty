//! Tests for [`super`]: the stat-driven dynamic module registry.

use super::*;
use crate::linker::build_linker;
use crate::registry::new_engine;
use openrusty_core::ReqCtx;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static SEQ: AtomicU32 = AtomicU32::new(0);

/// Self-cleaning temp directory for dynamic module files.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new(tag: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!(
            "openrusty-wasm-dyn-{tag}-{}-{n}",
            std::process::id()
        ));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    fn write(&self, name: &str, bytes: &[u8]) {
        fs::write(self.0.join(name), bytes).unwrap();
    }
    fn remove(&self, name: &str) {
        fs::remove_file(self.0.join(name)).unwrap();
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn cfg(dir: &str) -> DynamicConfig {
    DynamicConfig {
        dir: dir.to_string(),
        ..Default::default()
    }
}

/// Registry plus the ticker keeping its engine's epoch alive.
fn registry(dir: &str) -> (crate::epoch::EpochTicker, Arc<DynamicRegistry>) {
    let ticker = new_engine().unwrap();
    let linker = build_linker(ticker.engine()).unwrap();
    let reg = DynamicRegistry::new(ticker.engine().clone(), linker, &cfg(dir));
    (ticker, reg)
}

fn ctx() -> ReqCtx {
    ReqCtx {
        method: "POST".into(),
        path: "/api/v1/dynamic/echo".into(),
        query: String::new(),
        version: "HTTP/1.1".into(),
        client_addr: "127.0.0.1:40000".parse().unwrap(),
        headers: Vec::new(),
        route_index: None,
        upstream: None,
        peer_index: None,
        attempts: 0,
        tried: Vec::new(),
    }
}

/// Content-phase Done: writes a body, declines earlier phases.
const ECHO_MOD: &str = r#"(module
  (import "openrusty" "resp_body_set" (func $set (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "dynamic echo")
  (func (export "orr_on_phase") (param $phase i32) (param $aux i32) (result i32)
    (if (i32.eq (local.get $phase) (i32.const 3))
      (then
        (drop (call $set (i32.const 0) (i32.const 12)))
        (return (i32.const -4))))
    i32.const -5)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))"#;

/// Always Deny 418; padded so replacements differ in size (mtime
/// granularity alone is not a portable version signal).
const DENY_V1_MOD: &str = r#"(module
  (memory (export "memory") 1)
  (data (i32.const 0) "v1-padding-padding-padding-padding-padding-padding")
  (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const 418)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))"#;

/// Always Done with no body: 204.
const DONE_V2_MOD: &str = r#"(module
  (memory (export "memory") 1)
  (data (i32.const 0) "v2")
  (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const -4)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))"#;

/// Sets a body, then Deny 418: the body must be carried.
const DENY_BODY_MOD: &str = r#"(module
  (import "openrusty" "resp_body_set" (func $set (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "teapot")
  (func (export "orr_on_phase") (param i32 i32) (result i32)
    (drop (call $set (i32.const 0) (i32.const 6)))
    i32.const 418)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))"#;

/// Always Ok: never produces a response -> 500 fall-through.
const OK_MOD: &str = r#"(module
  (memory (export "memory") 1)
  (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const 0)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))"#;

/// Stores `k` = `v` in the shared KV, echoes the value as the body.
const KV_SET_MOD: &str = r#"(module
  (import "openrusty" "kv_set" (func $set (param i32 i32 i32 i32 i64) (result i32)))
  (import "openrusty" "resp_body_set" (func $body (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "k")
  (data (i32.const 16) "v")
  (data (i32.const 32) "kv-set-module-padding")
  (func (export "orr_on_phase") (param $phase i32) (param $aux i32) (result i32)
    (if (i32.eq (local.get $phase) (i32.const 3))
      (then
        (drop (call $set (i32.const 0) (i32.const 1) (i32.const 16) (i32.const 1) (i64.const -1)))
        (drop (call $body (i32.const 16) (i32.const 1)))
        (return (i32.const -4))))
    i32.const -5)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))"#;

/// Reads `k` from the shared KV and echoes it (one-byte read fits).
const KV_GET_MOD: &str = r#"(module
  (import "openrusty" "kv_get" (func $get (param i32 i32 i32 i32) (result i32)))
  (import "openrusty" "resp_body_set" (func $body (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "k")
  (func (export "orr_on_phase") (param $phase i32) (param $aux i32) (result i32)
    (if (i32.eq (local.get $phase) (i32.const 3))
      (then
        (drop (call $get (i32.const 0) (i32.const 1) (i32.const 32) (i32.const 1)))
        (drop (call $body (i32.const 32) (i32.const 1)))
        (return (i32.const -4))))
    i32.const -5)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))"#;

/// Writes 1 MiB + 1 bytes: over the resp_body_set ceiling, refused.
const OVER_LIMIT_MOD: &str = r#"(module
  (import "openrusty" "resp_body_set" (func $set (param i32 i32) (result i32)))
  (memory (export "memory") 17)
  (func (export "orr_on_phase") (param i32 i32) (result i32)
    (drop (call $set (i32.const 0) (i32.const 1048577)))
    i32.const -4)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))"#;

/// Records response-header edits plus a body in the content phase.
const HEADER_MOD: &str = r#"(module
  (import "openrusty" "resp_body_set" (func $body (param i32 i32) (result i32)))
  (import "openrusty" "resp_header_set" (func $set (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "x-foo")
  (data (i32.const 16) "ok")
  (data (i32.const 32) "hdr")
  (func (export "orr_on_phase") (param $phase i32) (param $aux i32) (result i32)
    (if (i32.eq (local.get $phase) (i32.const 3))
      (then
        (drop (call $set (i32.const 0) (i32.const 5) (i32.const 16) (i32.const 2)))
        (drop (call $body (i32.const 32) (i32.const 3)))
        (return (i32.const -4))))
    i32.const -5)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))"#;

#[test]
fn valid_name_rule() {
    assert!(valid_name("a"));
    assert!(valid_name("Mod-1_2.x"));
    assert!(!valid_name(""));
    assert!(!valid_name(".."));
    assert!(!valid_name("a/b"));
    assert!(!valid_name(".hidden"));
    assert!(!valid_name("-x"));
    assert!(!valid_name("_x"));
    assert!(!valid_name("a b"));
    assert!(!valid_name(&"x".repeat(129)));
}

/// Returns Done in content without a body, then writes one in the log
/// phase: the late body must upgrade the outcome to 200 (hyper drops
/// the body of a 204, so serving 204 + body would arrive empty).
const LATE_BODY_MOD: &str = r#"(module
  (import "openrusty" "resp_body_set" (func $body (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "late")
  (func (export "orr_on_phase") (param $phase i32) (param $aux i32) (result i32)
    (if (i32.eq (local.get $phase) (i32.const 3))
      (then (return (i32.const -4))))
    (if (i32.eq (local.get $phase) (i32.const 7))
      (then (drop (call $body (i32.const 0) (i32.const 4)))))
    i32.const -5)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))"#;

#[tokio::test]
async fn late_body_write_upgrades_204_to_200() {
    let dir = TmpDir::new("latebody");
    dir.write("late.wasm", LATE_BODY_MOD.as_bytes());
    let (_ticker, reg) = registry(dir.0.to_str().unwrap());
    let out = reg.invoke("late", ctx(), Bytes::new()).await;
    assert_eq!(out.status, 200);
    assert_eq!(out.body.as_deref(), Some(&b"late"[..]));
}

#[tokio::test]
async fn happy_path_serves_and_caches() {
    let dir = TmpDir::new("happy");
    dir.write("echo.wasm", ECHO_MOD.as_bytes());
    let (_ticker, reg) = registry(dir.0.to_str().unwrap());
    let out = reg.invoke("echo", ctx(), Bytes::new()).await;
    assert_eq!(out.status, 200);
    assert!(out.error.is_none());
    assert_eq!(out.body.as_deref(), Some(&b"dynamic echo"[..]));
    assert_eq!(reg.compiled_count(), 1);
    // Second request: stat version unchanged => no recompile.
    let out = reg.invoke("echo", ctx(), Bytes::new()).await;
    assert_eq!(out.status, 200);
    assert_eq!(out.body.as_deref(), Some(&b"dynamic echo"[..]));
    assert_eq!(reg.compiled_count(), 1);
}

#[tokio::test]
async fn file_replacement_invalidates_cache() {
    let dir = TmpDir::new("replace");
    dir.write("prog.wasm", DENY_V1_MOD.as_bytes());
    let (_ticker, reg) = registry(dir.0.to_str().unwrap());
    let out = reg.invoke("prog", ctx(), Bytes::new()).await;
    assert_eq!(out.status, 418);
    assert!(out.body.is_none());
    assert_eq!(reg.compiled_count(), 1);
    // Replace the file (different size => different version key).
    dir.write("prog.wasm", DONE_V2_MOD.as_bytes());
    let out = reg.invoke("prog", ctx(), Bytes::new()).await;
    assert_eq!(out.status, 204);
    assert!(out.body.is_none());
    assert_eq!(reg.compiled_count(), 2, "replacement must recompile");
}

#[tokio::test]
async fn removed_file_is_404() {
    let dir = TmpDir::new("gone");
    dir.write("prog.wasm", ECHO_MOD.as_bytes());
    let (_ticker, reg) = registry(dir.0.to_str().unwrap());
    assert_eq!(reg.invoke("prog", ctx(), Bytes::new()).await.status, 200);
    dir.remove("prog.wasm");
    let out = reg.invoke("prog", ctx(), Bytes::new()).await;
    assert_eq!(out.status, 404);
    assert!(out.error.is_some());
    // A never-existing name is the same 404.
    let out = reg.invoke("missing", ctx(), Bytes::new()).await;
    assert_eq!(out.status, 404);
}

#[tokio::test]
async fn bad_name_is_400() {
    let dir = TmpDir::new("badname");
    let (_ticker, reg) = registry(dir.0.to_str().unwrap());
    for name in ["..", "a/b", "", ".hidden", "-x", "_x", "a b"] {
        let out = reg.invoke(name, ctx(), Bytes::new()).await;
        assert_eq!(out.status, 400, "name {name:?}");
        assert!(out.error.is_some());
    }
}

#[tokio::test]
async fn host_state_survives_replacement() {
    let dir = TmpDir::new("state");
    dir.write("prog.wasm", KV_SET_MOD.as_bytes());
    let (_ticker, reg) = registry(dir.0.to_str().unwrap());
    let out = reg.invoke("prog", ctx(), Bytes::new()).await;
    assert_eq!(out.status, 200);
    assert_eq!(out.body.as_deref(), Some(&b"v"[..]));
    // Replace with a reader module: same name => same HostState.
    dir.write("prog.wasm", KV_GET_MOD.as_bytes());
    let out = reg.invoke("prog", ctx(), Bytes::new()).await;
    assert_eq!(out.status, 200);
    assert_eq!(
        out.body.as_deref(),
        Some(&b"v"[..]),
        "KV written by the replaced module must survive"
    );
    assert_eq!(reg.compiled_count(), 2);
}

#[tokio::test]
async fn over_limit_body_write_is_refused() {
    let dir = TmpDir::new("overlimit");
    dir.write("big.wasm", OVER_LIMIT_MOD.as_bytes());
    let (_ticker, reg) = registry(dir.0.to_str().unwrap());
    let out = reg.invoke("big", ctx(), Bytes::new()).await;
    assert_eq!(out.status, 204, "Done without a body is 204");
    assert!(out.body.is_none());
}

#[tokio::test]
async fn deny_carries_body() {
    let dir = TmpDir::new("denybody");
    dir.write("teapot.wasm", DENY_BODY_MOD.as_bytes());
    let (_ticker, reg) = registry(dir.0.to_str().unwrap());
    let out = reg.invoke("teapot", ctx(), Bytes::new()).await;
    assert_eq!(out.status, 418);
    assert_eq!(out.body.as_deref(), Some(&b"teapot"[..]));
}

#[tokio::test]
async fn fall_through_without_response_is_500() {
    let dir = TmpDir::new("noop");
    dir.write("noop.wasm", OK_MOD.as_bytes());
    let (_ticker, reg) = registry(dir.0.to_str().unwrap());
    let out = reg.invoke("noop", ctx(), Bytes::new()).await;
    assert_eq!(out.status, 500);
    assert!(out.error.as_deref().unwrap().contains("no response"));
}

#[tokio::test]
async fn plugin_headers_reach_the_outcome() {
    let dir = TmpDir::new("headers");
    dir.write("hdr.wasm", HEADER_MOD.as_bytes());
    let (_ticker, reg) = registry(dir.0.to_str().unwrap());
    let out = reg.invoke("hdr", ctx(), Bytes::new()).await;
    assert_eq!(out.status, 200);
    assert_eq!(out.body.as_deref(), Some(&b"hdr"[..]));
    assert!(out
        .headers
        .contains(&("x-foo".to_string(), "ok".to_string())));
}

#[tokio::test]
async fn uncompilable_file_is_500_and_not_cached() {
    let dir = TmpDir::new("garbage");
    dir.write("junk.wasm", b"definitely not wasm");
    let (_ticker, reg) = registry(dir.0.to_str().unwrap());
    let out = reg.invoke("junk", ctx(), Bytes::new()).await;
    assert_eq!(out.status, 500);
    assert!(out.error.is_some());
    // Failures are never cached: the next attempt compiles again.
    let out = reg.invoke("junk", ctx(), Bytes::new()).await;
    assert_eq!(out.status, 500);
    assert_eq!(reg.compiled_count(), 2);
}

#[tokio::test]
async fn bad_abi_module_is_500() {
    let dir = TmpDir::new("badabi");
    // Compiles fine but lacks orr_alloc -> ABI validation failure.
    let no_alloc = r#"(module
        (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const 0)
        (memory (export "memory") 1))"#;
    dir.write("bad.wasm", no_alloc.as_bytes());
    let (_ticker, reg) = registry(dir.0.to_str().unwrap());
    let out = reg.invoke("bad", ctx(), Bytes::new()).await;
    assert_eq!(out.status, 500);
    assert!(out.error.as_deref().unwrap().contains("abi"));
}
