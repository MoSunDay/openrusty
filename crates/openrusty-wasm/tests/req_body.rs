//! Regression: the buffered request body seeded via
//! `RequestSession::set_req_body` must reach plugins through
//! `req_meta("body")` on every phase run (not only at instantiation).
//!
//! Uses the real `vllm-kv-scheduler.wasm` fixture built by
//! `scripts/build-plugins.sh`; the test is skipped when the fixture is
//! absent (fresh checkout before the plugin build).

use openrusty_core::config::{Config, PluginsConfig, ServerConfig};
use openrusty_core::phase::{Decision, Phase};
use openrusty_core::ReqCtx;
use openrusty_wasm::{PeerView, PluginRegistry, RequestSession};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::sync::atomic::{AtomicU32, Ordering};

static SEQ: AtomicU32 = AtomicU32::new(0);

struct TmpDir(std::path::PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!(
            "openrusty-wasm-reqbody-{tag}-{}-{n}",
            std::process::id()
        ));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The vllm-kv-scheduler fixture; `None` when not built yet (skip).
fn fixture() -> Option<Vec<u8>> {
    fs::read("../../build/plugins/vllm-kv-scheduler.wasm").ok()
}

fn cfg(dir: &str) -> Config {
    let mut kv = HashMap::new();
    kv.insert("extract".to_string(), "body:cache_salt".to_string());
    kv.insert("affinity_ttl_s".to_string(), "6".to_string());
    let mut settings = BTreeMap::new();
    settings.insert("vllm-kv-scheduler".to_string(), kv);
    Config {
        server: ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            log_level: "info".into(),
            http1_only: false,
            listeners: Vec::new(),
            shutdown_grace_ms: 5_000,
        },
        plugins: PluginsConfig {
            dir: dir.into(),
            order: vec!["vllm-kv-scheduler".to_string()],
            timeout_ms: 60_000,
            max_memory_mb: 16,
            settings,
            ..Default::default()
        },
        upstreams: Vec::new(),
        routes: Vec::new(),
        ingress: Default::default(),
        egress: Default::default(),
        dynamic: None,
    }
}

fn ctx() -> ReqCtx {
    ReqCtx {
        method: "POST".into(),
        path: "/v1/chat".into(),
        query: String::new(),
        version: "HTTP/1.1".into(),
        client_addr: "127.0.0.1:4242".parse().unwrap(),
        headers: Vec::new(),
        route_index: Some(0),
        upstream: Some("vllm".into()),
        peer_index: None,
        attempts: 0,
        tried: Vec::new(),
    }
}

fn peers() -> Vec<PeerView> {
    (0..3)
        .map(|i| PeerView {
            name: format!("p{i}"),
            addr: format!("127.0.0.1:900{i}"),
            healthy: true,
        })
        .collect()
}

fn balance_peer(reg: &PluginRegistry, body: &[u8]) -> Option<u32> {
    let mut sess = RequestSession::new(reg, reg.snapshot(), ctx(), peers());
    sess.set_req_body(bytes::Bytes::copy_from_slice(body));
    sess.run_phase(Phase::Balancer);
    sess.ctx().peer_index
}

#[test]
fn body_field_reaches_the_balancer_and_sticks() {
    let Some(wasm) = fixture() else {
        eprintln!("skipped: build/plugins/vllm-kv-scheduler.wasm not built");
        return;
    };
    let dir = TmpDir::new("sched");
    fs::write(dir.0.join("vllm-kv-scheduler.wasm"), wasm).unwrap();
    let reg = PluginRegistry::bootstrap(&cfg(dir.0.to_str().unwrap())).unwrap();

    // The seeded body is visible on every run, so the same salt is
    // pinned to one peer across separate sessions (shared KV affinity).
    let body = br#"{"cache_salt":"agent:s1"}"#;
    let first = balance_peer(&reg, body);
    assert!(first.is_some(), "balancer must pick a peer for body salt");
    for _ in 0..3 {
        assert_eq!(balance_peer(&reg, body), first, "salt must stick");
    }

    // A request without the field declines (default balancer fallback).
    let mut sess = RequestSession::new(&reg, reg.snapshot(), ctx(), peers());
    sess.set_req_body(bytes::Bytes::from_static(br#"{"messages":[]}"#));
    assert_eq!(sess.run_phase(Phase::Balancer), Decision::Declined);
    assert_eq!(sess.ctx().peer_index, None);

    // No body seeded at all (pre-content phases) also declines.
    let mut sess = RequestSession::new(&reg, reg.snapshot(), ctx(), peers());
    assert_eq!(sess.run_phase(Phase::Balancer), Decision::Declined);
}

#[test]
fn body_is_pushed_to_late_instantiated_plugins() {
    // The push happens before every phase call, so a plugin instance
    // created on the first run still sees a body seeded afterwards.
    let Some(wasm) = fixture() else {
        eprintln!("skipped: build/plugins/vllm-kv-scheduler.wasm not built");
        return;
    };
    let dir = TmpDir::new("late");
    fs::write(dir.0.join("vllm-kv-scheduler.wasm"), wasm).unwrap();
    let reg = PluginRegistry::bootstrap(&cfg(dir.0.to_str().unwrap())).unwrap();

    let mut sess = RequestSession::new(&reg, reg.snapshot(), ctx(), peers());
    // First run with no body: the instance is created here.
    assert_eq!(sess.run_phase(Phase::Balancer), Decision::Declined);
    // Second run seeds the body: the live instance must see it now.
    sess.set_req_body(bytes::Bytes::from_static(br#"{"cache_salt":"late:s1"}"#));
    sess.run_phase(Phase::Balancer);
    assert!(
        sess.ctx().peer_index.is_some(),
        "body seeded after instantiation must reach the plugin"
    );
}
