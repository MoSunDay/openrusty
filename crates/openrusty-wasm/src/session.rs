//! Per-request facade: runs every phase across the plugin chain against a
//! pinned snapshot. Instances are created lazily on first use.

use crate::host_state::HostState;
use crate::instance::{new_host_data, HeaderEdit, PeerView};
use crate::registry::{LoadedPlugin, PluginRegistry, PluginSnapshot};
use crate::runner::{self, PluginRt};
use bytes::Bytes;
use openrusty_core::phase::{Decision, Phase};
use openrusty_core::ReqCtx;
use std::sync::Arc;
use wasmtime::{Engine, Trap};

/// One request flowing through the plugin chain.
pub struct RequestSession {
    engine: Engine,
    snap: Arc<PluginSnapshot>,
    ctx: ReqCtx,
    peers: Vec<PeerView>,
    /// Lazily instantiated runtime per plugin (same index as snap.plugins).
    rts: Vec<Option<PluginRt>>,
    /// Shared body_filter state pushed into each instance before its call.
    body_chunk: Bytes,
    body_last: bool,
    /// Current response headers, pushed before each call and pulled back
    /// after (resp_header_set/del mutate them).
    resp_headers: Vec<(String, String)>,
    /// Response body written by a plugin via `resp_body_set`, pulled back
    /// after each call; consumed when a phase short-circuits.
    resp_body: Option<Bytes>,
    /// Buffered request body, pushed into each instance before its call.
    /// Empty until the server seeds it (content phase onward).
    req_body: Bytes,
    /// Per-phase accumulated wasm wall time (nanoseconds), indexed by
    /// `phase as usize`; drained by [`RequestSession::take_phase_stats`].
    phase_ns: [u64; Phase::COUNT],
    /// Per-phase plugin invocation counts, indexed like `phase_ns`
    /// (instantiation failures included: they burn the timeout budget
    /// the same way a slow call does).
    phase_calls: [u64; Phase::COUNT],
}

impl RequestSession {
    pub fn new(
        reg: &PluginRegistry,
        snap: Arc<PluginSnapshot>,
        ctx: ReqCtx,
        peers: Vec<PeerView>,
    ) -> Self {
        Self::for_parts(reg.engine().clone(), snap, ctx, peers)
    }

    /// [`RequestSession::new`] for callers that already hold the engine
    /// (e.g. embedders sharing one registry's runtime). Instances come
    /// from each plugin's pre-resolved imports, so no linker is needed.
    pub fn for_parts(
        engine: Engine,
        snap: Arc<PluginSnapshot>,
        ctx: ReqCtx,
        peers: Vec<PeerView>,
    ) -> Self {
        let rts = snap.plugins.iter().map(|_| None).collect();
        RequestSession {
            engine,
            snap,
            ctx,
            peers,
            rts,
            body_chunk: Bytes::new(),
            body_last: false,
            resp_headers: Vec::new(),
            resp_body: None,
            req_body: Bytes::new(),
            phase_ns: [0; Phase::COUNT],
            phase_calls: [0; Phase::COUNT],
        }
    }

    /// Mutable request context (read peer_index after the balancer phase,
    /// set upstream/attempts between phases).
    pub fn ctx(&mut self) -> &mut ReqCtx {
        &mut self.ctx
    }

    pub fn plugins(&self) -> &[Arc<LoadedPlugin>] {
        &self.snap.plugins
    }

    /// Set the current body_filter chunk (pushed into instances on the
    /// next phase run).
    pub fn set_body_chunk(&mut self, chunk: Bytes, last: bool) {
        self.body_chunk = chunk;
        self.body_last = last;
    }

    /// Seed the response headers visible to header_filter plugins.
    pub fn set_resp_headers(&mut self, h: Vec<(String, String)>) {
        self.resp_headers = h;
    }

    /// Seed the buffered request body visible to content/balancer/later
    /// phases (pushed into instances on the next phase run).
    pub fn set_req_body(&mut self, body: Bytes) {
        self.req_body = body;
    }

    /// Current response headers as mutated by header_filter plugins
    /// (seed with [`Self::set_resp_headers`] before running the phase).
    pub fn resp_headers(&self) -> &[(String, String)] {
        &self.resp_headers
    }

    /// Response body written by a plugin via `resp_body_set`, if any
    /// (`None` = no body carried by a short-circuit).
    pub fn resp_body(&self) -> Option<&Bytes> {
        self.resp_body.as_ref()
    }

    /// Drain the buffered response body written by plugins so far,
    /// clearing both the session copy and every instance's host data.
    /// Returns `None` when unset or empty (an empty body is "no body").
    pub fn take_resp_body(&mut self) -> Option<Bytes> {
        let buffered = self.resp_body.take();
        for rt in self.rts.iter_mut().flatten() {
            rt.host_data_mut().resp_body = None;
        }
        buffered.filter(|b| !b.is_empty())
    }

    /// Drain all header edits recorded by plugins so far.
    pub fn take_header_edits(&mut self) -> Vec<HeaderEdit> {
        let mut out = Vec::new();
        for rt in self.rts.iter_mut().flatten() {
            out.append(&mut rt.host_data_mut().resp_edits);
        }
        out
    }

    /// Run one phase across the plugin chain, in snapshot order.
    ///
    /// Terminal decisions stop the chain early, except in the Log phase
    /// (log never aborts; its terminal decisions are ignored).
    pub fn run_phase(&mut self, phase: Phase) -> Decision {
        let is_log = matches!(phase, Phase::Log);
        // Stats slot for this phase (Phase ids are dense 0..COUNT).
        let slot = phase as usize;
        let mut last = Decision::Declined;
        for i in 0..self.snap.plugins.len() {
            let plugin = self.snap.plugins[i].clone();
            // One measurement per plugin per phase: instantiation (when
            // lazily needed) plus the runner call. No allocation.
            let started = std::time::Instant::now();
            if self.rts[i].is_none() {
                match self.instantiate_one(&plugin) {
                    Ok(rt) => self.rts[i] = Some(rt),
                    Err(e) => {
                        // An epoch trap means the instantiation budget ran
                        // out (kind label mirrors KIND_TIMEOUT in
                        // openrusty-server/src/metrics.rs); everything else
                        // is trap-like (KIND_TRAP).
                        let kind = if e.downcast_ref::<Trap>() == Some(&Trap::Interrupt) {
                            "timeout"
                        } else {
                            "trap"
                        };
                        plugin.state.record_error(kind);
                        tracing::warn!(plugin = %plugin.name, error = %e, "plugin instantiation failed");
                        let fallback = runner::fallback_decision(plugin.fail_policy);
                        self.record_phase_time(slot, started);
                        last = fallback;
                        if fallback.is_terminal() && !is_log {
                            return fallback;
                        }
                        continue;
                    }
                }
            }
            let rt = self.rts[i].as_mut().expect("just instantiated");
            // Push shared per-request state into the instance by
            // ownership transfer: `mem::take` moves the real data in
            // (zero copies) and leaves default empties behind.
            //
            // Invariant: the take-back below is the symmetric half and
            // MUST run in the same loop iteration with no `return` or
            // `continue` between the two brackets - between phases the
            // session (not the host data) holds the real ctx/peers/
            // resp_headers, which the server reads and mutates via
            // `session.ctx()`.
            let hd = rt.host_data_mut();
            hd.ctx = std::mem::take(&mut self.ctx);
            hd.peers = std::mem::take(&mut self.peers);
            hd.resp_headers = std::mem::take(&mut self.resp_headers);
            hd.req_body = std::mem::take(&mut self.req_body);
            hd.body_chunk = std::mem::take(&mut self.body_chunk);
            hd.body_last = self.body_last;

            let decision = runner::run_phase(rt, plugin.timeout, plugin.fail_policy, phase);

            // Pull mutated state back out (the take-back half of the
            // bracket above): the session owns the real data again and
            // the host data is left with empties for the next push.
            let hd = rt.host_data_mut();
            self.ctx = std::mem::take(&mut hd.ctx);
            self.peers = std::mem::take(&mut hd.peers);
            self.resp_headers = std::mem::take(&mut hd.resp_headers);
            self.req_body = std::mem::take(&mut hd.req_body);
            self.body_chunk = std::mem::take(&mut hd.body_chunk);
            self.body_last = hd.body_last;
            // Host data is not seeded with the session's body, so a
            // plugin that writes none must not clobber a body written by
            // an earlier plugin in the chain (last writer wins; an
            // untouched `None` preserves what is already carried).
            if hd.resp_body.is_some() {
                self.resp_body = hd.resp_body.take();
            }

            self.record_phase_time(slot, started);
            last = decision;
            if decision.is_terminal() && !is_log {
                return decision;
            }
        }
        last
    }

    /// Fold one plugin invocation's wall time (started to now) into the
    /// per-phase nanosecond total and bump the invocation count.
    fn record_phase_time(&mut self, slot: usize, started: std::time::Instant) {
        self.phase_ns[slot] += started.elapsed().as_nanos() as u64;
        self.phase_calls[slot] += 1;
    }

    /// Drain per-phase totals accumulated by [`RequestSession::run_phase`]:
    /// `(phase name, plugin invocations, total wall time)`. Only phases that
    /// ran are included, in phase-id order. Draining is destructive: a
    /// second call yields nothing, so a request records its stats once.
    pub fn take_phase_stats(&mut self) -> Vec<(&'static str, u64, std::time::Duration)> {
        let mut out = Vec::new();
        for i in 0..Phase::COUNT {
            if self.phase_calls[i] == 0 {
                continue;
            }
            let phase = Phase::from_i32(i as i32).expect("phase ids are dense");
            out.push((
                phase.name(),
                self.phase_calls[i],
                std::time::Duration::from_nanos(self.phase_ns[i]),
            ));
            self.phase_calls[i] = 0;
            self.phase_ns[i] = 0;
        }
        out
    }

    fn instantiate_one(&self, p: &LoadedPlugin) -> Result<PluginRt, wasmtime::Error> {
        // Host data is seeded with placeholders: the first phase push
        // moves the real ctx/peers in before any plugin call, so an
        // instantiation failure cannot lose session state.
        let host = new_host_data(
            ReqCtx::empty(),
            Vec::new(),
            p.state.clone(),
            p.settings.clone(),
        );
        runner::instantiate(&p.pre, &self.engine, host, p.memory_mb)
    }
}

/// Shared KV access for servers that need it outside a phase run.
pub fn plugin_state(p: &LoadedPlugin) -> &HostState {
    &p.state
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::PluginRegistry;
    use openrusty_core::config::{Config, PluginsConfig, ServerConfig};
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    struct TmpDir(std::path::PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::SeqCst);
            let p = std::env::temp_dir().join(format!(
                "openrusty-wasm-sess-{tag}-{}-{n}",
                std::process::id()
            ));
            fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
        fn write(&self, name: &str, body: &str) {
            fs::write(self.0.join(name), body.as_bytes()).unwrap();
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn ret_module(code: i32) -> String {
        format!(
            r#"(module
              (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const {code})
              (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
              (memory (export "memory") 1))"#
        )
    }

    const BALANCER_MOD: &str = r#"
(module
  (import "openrusty" "balancer_set_peer" (func $set (param i32) (result i32)))
  (func (export "orr_on_phase") (param i32 i32) (result i32)
    (drop (call $set (i32.const 1)))
    i32.const 0)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
  (memory (export "memory") 1))
"#;

    const HEADER_MOD: &str = r#"
(module
  (import "openrusty" "resp_header_set" (func $set (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "x-foo")
  (data (i32.const 16) "ok")
  (func (export "orr_on_phase") (param i32 i32) (result i32)
    (drop (call $set (i32.const 0) (i32.const 5) (i32.const 16) (i32.const 2)))
    i32.const 0)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))
"#;

    const BODY_DONE_MOD: &str = r#"
(module
  (import "openrusty" "resp_body_set" (func $set (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "served by plugin")
  (func (export "orr_on_phase") (param i32 i32) (result i32)
    (drop (call $set (i32.const 0) (i32.const 16)))
    i32.const -4)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))
"#;

    fn test_cfg(dir: &str, order: &[&str]) -> Config {
        Config {
            server: ServerConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                log_level: "info".into(),
                http1_only: false,
                listeners: Vec::new(),
                shutdown_grace_ms: 5_000,
                log_file: None,
            },
            admin: Default::default(),
            plugins: PluginsConfig {
                dir: dir.into(),
                order: order.iter().map(|s| s.to_string()).collect(),
                timeout_ms: 60_000,
                max_memory_mb: 16,
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
            method: "GET".into(),
            path: "/v1/models".into(),
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
        vec![
            PeerView {
                name: "p0".into(),
                addr: "127.0.0.1:9001".into(),
                healthy: true,
            },
            PeerView {
                name: "p1".into(),
                addr: "127.0.0.1:9002".into(),
                healthy: true,
            },
        ]
    }

    #[tokio::test]
    async fn chained_decisions_stop_early() {
        let dir = TmpDir::new("chain");
        dir.write("a.wasm", &ret_module(0)); // Ok: continue
        dir.write("b.wasm", &ret_module(403)); // Deny: terminal
        dir.write("c.wasm", &ret_module(0)); // must never run
        let cfg = test_cfg(dir.0.to_str().unwrap(), &["a", "b", "c"]);
        let reg = PluginRegistry::bootstrap(&cfg).unwrap();
        let mut sess = RequestSession::new(&reg, reg.snapshot(), ctx(), Vec::new());
        assert_eq!(sess.run_phase(Phase::PostRead), Decision::Deny(403));
        // "c" was never instantiated, so its KV stayed untouched.
        assert_eq!(sess.plugins().len(), 3);
    }

    #[tokio::test]
    async fn log_phase_ignores_terminal() {
        let dir = TmpDir::new("logchain");
        dir.write("a.wasm", &ret_module(403));
        dir.write("b.wasm", &ret_module(0));
        let cfg = test_cfg(dir.0.to_str().unwrap(), &["a", "b"]);
        let reg = PluginRegistry::bootstrap(&cfg).unwrap();
        let mut sess = RequestSession::new(&reg, reg.snapshot(), ctx(), Vec::new());
        // Log runs ALL plugins even though "a" returns a terminal code;
        // the final decision is the last plugin's.
        assert_eq!(sess.run_phase(Phase::Log), Decision::Ok);
        // Both plugins were instantiated.
        let mut edits = sess.take_header_edits();
        assert!(edits.is_empty());
        edits.clear();
    }

    #[tokio::test]
    async fn balancer_sets_peer_index() {
        let dir = TmpDir::new("balancer");
        dir.write("pick.wasm", BALANCER_MOD);
        let cfg = test_cfg(dir.0.to_str().unwrap(), &[]);
        let reg = PluginRegistry::bootstrap(&cfg).unwrap();
        let mut sess = RequestSession::new(&reg, reg.snapshot(), ctx(), peers());
        assert_eq!(sess.run_phase(Phase::Balancer), Decision::Ok);
        assert_eq!(sess.ctx().peer_index, Some(1));
    }

    #[tokio::test]
    async fn header_filter_records_edits() {
        let dir = TmpDir::new("headers");
        dir.write("hdr.wasm", HEADER_MOD);
        let cfg = test_cfg(dir.0.to_str().unwrap(), &[]);
        let reg = PluginRegistry::bootstrap(&cfg).unwrap();
        let mut sess = RequestSession::new(&reg, reg.snapshot(), ctx(), Vec::new());
        sess.set_resp_headers(vec![("Content-Type".into(), "text/plain".into())]);
        assert_eq!(sess.run_phase(Phase::HeaderFilter), Decision::Ok);
        assert_eq!(
            sess.take_header_edits(),
            vec![HeaderEdit::Set("x-foo".into(), "ok".into())]
        );
        // Edits are drained now.
        assert!(sess.take_header_edits().is_empty());
    }

    /// Echo module: copies `req_meta("body")` into `resp_body_set` on
    /// every call, so each phase's outcome directly reflects the request
    /// body the session pushed in.
    const ECHO_BODY_MOD: &str = r#"
(module
  (import "openrusty" "req_meta" (func $meta (param i32 i32 i32 i32) (result i32)))
  (import "openrusty" "resp_body_set" (func $set (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "body")
  (func (export "orr_on_phase") (param i32 i32) (result i32)
    (local $len i32)
    (local.set $len (call $meta (i32.const 0) (i32.const 4) (i32.const 64) (i32.const 1024)))
    (if (i32.lt_s (local.get $len) (i32.const 0))
      (then unreachable))
    (drop (call $set (i32.const 64) (local.get $len)))
    i32.const 0)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))
"#;

    #[tokio::test]
    async fn ownership_transfer_round_trips_session_state() {
        // Push/pull moves the real state in and out per call; between
        // phases the session must hold it again (the server reads and
        // mutates `session.ctx()` there). If the take-back half were
        // missing, the second run below would see an empty body.
        let dir = TmpDir::new("pullback");
        dir.write("echo.wasm", ECHO_BODY_MOD);
        let cfg = test_cfg(dir.0.to_str().unwrap(), &[]);
        let reg = PluginRegistry::bootstrap(&cfg).unwrap();
        let mut sess = RequestSession::new(&reg, reg.snapshot(), ctx(), peers());
        sess.set_req_body(Bytes::from_static(b"hello body"));
        for phase in [Phase::Access, Phase::Content, Phase::Log] {
            assert_eq!(sess.run_phase(phase), Decision::Ok);
            // ctx and peers came back intact...
            assert_eq!(sess.ctx().path, "/v1/models");
            assert_eq!(sess.ctx().route_index, Some(0));
            assert_eq!(sess.ctx().upstream.as_deref(), Some("vllm"));
            assert_eq!(sess.ctx().peer_index, None);
            // ...and the seeded body reached the plugin through the push
            // (an empty echo would mean the session lost it).
            assert_eq!(sess.take_resp_body().as_deref(), Some(&b"hello body"[..]));
        }
    }

    /// Content-phase module: writes a body but returns Ok (non-terminal).
    const CHAIN_BODY_OK_MOD: &str = r#"
(module
  (import "openrusty" "resp_body_set" (func $set (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "from-first")
  (func (export "orr_on_phase") (param $phase i32) (param $ctx i32) (result i32)
    (if (i32.eq (local.get $phase) (i32.const 3))
      (then
        (drop (call $set (i32.const 0) (i32.const 10)))
        (return (i32.const 0))))
    i32.const -5)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))
"#;

    /// Content-phase module: returns Done without touching the body.
    const CHAIN_DONE_MOD: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "orr_on_phase") (param $phase i32) (param $ctx i32) (result i32)
    (if (i32.eq (local.get $phase) (i32.const 3))
      (then (return (i32.const -4))))
    i32.const -5)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))
"#;

    #[tokio::test]
    async fn chain_body_survives_later_done_plugin() {
        let dir = TmpDir::new("respchain");
        dir.write("first.wasm", CHAIN_BODY_OK_MOD);
        dir.write("second.wasm", CHAIN_DONE_MOD);
        let cfg = test_cfg(dir.0.to_str().unwrap(), &["first", "second"]);
        let reg = PluginRegistry::bootstrap(&cfg).unwrap();
        let mut sess = RequestSession::new(&reg, reg.snapshot(), ctx(), Vec::new());
        assert_eq!(sess.run_phase(Phase::Content), Decision::Done);
        // first wrote a body and returned Ok; second's Done must not
        // clobber it with second's untouched (empty) host-data copy.
        assert_eq!(sess.take_resp_body().as_deref(), Some(&b"from-first"[..]));
    }

    #[tokio::test]
    async fn done_short_circuit_carries_body() {
        let dir = TmpDir::new("respbody");
        dir.write("body.wasm", BODY_DONE_MOD);
        let cfg = test_cfg(dir.0.to_str().unwrap(), &[]);
        let reg = PluginRegistry::bootstrap(&cfg).unwrap();
        let mut sess = RequestSession::new(&reg, reg.snapshot(), ctx(), Vec::new());
        assert_eq!(sess.run_phase(Phase::Content), Decision::Done);
        assert_eq!(
            sess.take_resp_body().as_deref(),
            Some(&b"served by plugin"[..])
        );
        // The body is drained (session copy and instance host data).
        assert!(sess.take_resp_body().is_none());
    }

    #[tokio::test]
    async fn broken_module_falls_back_to_policy() {
        let dir = TmpDir::new("broken");
        let no_memory = r#"(module
            (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const 0)
            (func (export "orr_alloc") (param i32) (result i32) i32.const 0))"#;
        dir.write("ok.wasm", &ret_module(0));
        dir.write("nomem.wasm", no_memory);
        // Validation passes (required exports present) but per-request
        // instantiation fails on the missing memory export, so the
        // fail_closed policy applies.
        let mut cfg = test_cfg(dir.0.to_str().unwrap(), &["ok", "nomem"]);
        cfg.plugins.on_failure = openrusty_core::config::FailPolicy::FailClosed;
        let reg = PluginRegistry::bootstrap(&cfg).unwrap();
        let mut sess = RequestSession::new(&reg, reg.snapshot(), ctx(), Vec::new());
        assert_eq!(sess.run_phase(Phase::Access), Decision::Deny(503));
        let nomem = &sess.plugins()[1];
        assert_eq!(plugin_state(nomem).error_count(), 1);
    }

    #[tokio::test]
    async fn phase_stats_track_runs_and_drain_once() {
        let dir = TmpDir::new("stats");
        dir.write("ok.wasm", &ret_module(0));
        let cfg = test_cfg(dir.0.to_str().unwrap(), &["ok"]);
        let reg = PluginRegistry::bootstrap(&cfg).unwrap();
        let mut sess = RequestSession::new(&reg, reg.snapshot(), ctx(), peers());

        // Fresh session: nothing ran yet.
        assert!(sess.take_phase_stats().is_empty());
        assert_eq!(sess.run_phase(Phase::Access), Decision::Ok);

        let stats = sess.take_phase_stats();
        // Only the phase that ran, carrying its invocation count and a
        // non-negative duration.
        assert_eq!(stats.len(), 1, "one phase recorded: {stats:?}");
        assert_eq!(stats[0].0, Phase::Access.name());
        assert!(stats[0].1 >= 1, "at least one plugin invocation");
        assert!(stats[0].2 >= std::time::Duration::ZERO);

        // Draining is destructive: the second take is empty (a request
        // records its stats exactly once).
        assert!(sess.take_phase_stats().is_empty());
    }
}
