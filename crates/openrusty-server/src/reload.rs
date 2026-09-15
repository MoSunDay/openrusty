//! Atomic hot reload: re-read + validate the config, recompile plugins off
//! the request path, and only then rebuild the routing/upstream runtime.
//!
//! Ordering matters: `apply_runtime` runs after the plugin registry has
//! published its new snapshot, so any failure (config parse, validation,
//! compile, ABI) leaves both the old plugin snapshot and the old runtime
//! untouched.

use crate::state::{apply_runtime, AppState};
use openrusty_core::config::Config;
use openrusty_core::load_config;
use std::sync::Arc;

/// Outcome of a successful reload.
#[derive(Debug)]
pub struct ReloadReport {
    pub generation: u64,
    pub plugins: Vec<String>,
}

/// Why a reload did not run to completion.
#[derive(Debug)]
pub enum ReloadError {
    /// Another reload is already running: SIGHUP and the HTTP endpoint
    /// share one gate, so concurrent requests are rejected, not queued.
    InFlight,
    /// The reload itself failed; the previous runtime stays in effect.
    Failed(String),
}

impl std::fmt::Display for ReloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReloadError::InFlight => f.write_str("reload already in progress"),
            ReloadError::Failed(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for ReloadError {}

/// Swap the plugin snapshot, then the runtime, then re-arm active probing.
///
/// Memory-only shared tail of every reload path (the file-based [`reload`]
/// and the in-memory [`apply_config`]); never touches the filesystem.
/// Callers must already hold the reload gate. Ordering matters:
/// `apply_runtime` runs after the plugin registry published its new
/// snapshot, so any failure (compile, ABI) leaves both the old plugin
/// snapshot and the old runtime untouched.
async fn publish(state: &Arc<AppState>, cfg: &Config) -> Result<u64, ReloadError> {
    // DNS endpoints (service-discovered upstreams) fold into concrete
    // peers here, off the request path; failures degrade per-upstream.
    let cfg = &openrusty_proxy::resolve::resolve_config(cfg).await;
    // Build the upstream TLS plans first: a bad certificate must abort the
    // swap before the plugin registry compiles anything, leaving the old
    // snapshot and the old runtime untouched.
    let tls_plans = crate::state::build_tls_plans(cfg)
        .map_err(|detail| ReloadError::Failed(format!("tls: {detail}")))?;

    // Compiles off the request path; Err leaves the old snapshot in place.
    let generation = state
        .registry
        .reload(cfg)
        .await
        .map_err(|e| ReloadError::Failed(format!("plugins: {e}")))?;

    // Plugins are already published; now swap the runtime to match, then
    // the dynamic-API registry (only when its `[dynamic]` section
    // changed - an unchanged section keeps the warm compile cache), then
    // re-arm active probing -- all inside the reload gate.
    apply_runtime(state, cfg, generation, &tls_plans);
    crate::state::apply_dynamic(state, cfg);
    crate::active_probe::spawn(state);
    Ok(generation)
}

/// Run one full reload cycle against `state.config_path`.
///
/// Reloads are serialized: whoever gets the gate first runs the whole cycle
/// (config + plugins + runtime swap + probe task swap) inside it, so a
/// reload can never race another one, and the probe task handle is taken,
/// aborted and replaced in the same critical section as the snapshot swap.
/// A second requester gets [`ReloadError::InFlight`].
///
/// Any [`ReloadError::Failed`] means nothing changed: the previous plugin
/// snapshot and runtime stay in effect.
pub async fn reload(state: &Arc<AppState>) -> Result<ReloadReport, ReloadError> {
    let _gate = state.reload_gate.try_lock().map_err(|_| ReloadError::InFlight)?;
    let path = state.config_path.clone();
    let cfg = tokio::task::spawn_blocking(move || load_config(&path))
        .await
        .map_err(|e| ReloadError::Failed(format!("reload task failed: {e}")))?
        .map_err(|e| ReloadError::Failed(format!("config: {e}")))?;

    let generation = publish(state, &cfg).await?;

    let plugins = state
        .registry
        .snapshot()
        .plugins
        .iter()
        .map(|p| p.name.clone())
        .collect();
    tracing::info!(generation, "reload complete");
    Ok(ReloadReport {
        generation,
        plugins,
    })
}

/// Apply an in-memory [`Config`] to a running state: hot-swap the plugins,
/// routing and upstream runtime (and restart the active-probe task) without
/// ever touching the filesystem.
///
/// The file-based entry point stays [`reload`] (it re-reads
/// `state.config_path`); this variant serves embedders (init-pro) that
/// build their `Config` programmatically and share one long-lived
/// [`AppState`]. Same [`ReloadError`] contract as [`reload`]: serialized
/// through `state.reload_gate`, so a caller while another reload runs gets
/// [`ReloadError::InFlight`], and any [`ReloadError::Failed`] leaves the
/// previous plugin snapshot and runtime in effect. Returns the new
/// generation.
pub async fn apply_config(state: &Arc<AppState>, cfg: &Config) -> Result<u64, ReloadError> {
    let _gate = state.reload_gate.try_lock().map_err(|_| ReloadError::InFlight)?;
    let generation = publish(state, cfg).await?;
    tracing::info!(generation, "in-memory config applied");
    Ok(generation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{boot_state, TmpDir, OK_WAT};
    use std::fs;

    #[tokio::test]
    async fn reload_publishes_new_generation() {
        let dir = TmpDir::new("reload-ok");
        dir.write_plugin("p.wasm", OK_WAT.as_bytes());
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);
        assert_eq!(state.runtime.load().generation, 1);

        let report = reload(&state).await.unwrap();
        assert_eq!(report.generation, 2);
        assert_eq!(report.plugins, vec!["p".to_string()]);
        assert_eq!(state.runtime.load().generation, 2);
        assert_eq!(state.registry.snapshot().generation, 2);
    }

    #[tokio::test]
    async fn broken_plugin_keeps_previous_runtime() {
        let dir = TmpDir::new("reload-broken");
        dir.write_plugin("p.wasm", OK_WAT.as_bytes());
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);
        let gen = reload(&state).await.unwrap().generation;
        assert_eq!(gen, 2);

        // Corrupt the module; the whole reload must fail atomically.
        dir.write_plugin("p.wasm", b"garbage");
        let err = reload(&state).await.unwrap_err();
        assert!(err.to_string().contains("plugins"), "got: {err}");
        assert!(matches!(err, ReloadError::Failed(_)));
        assert_eq!(state.runtime.load().generation, 2);
        assert_eq!(state.registry.snapshot().generation, 2);
        assert_eq!(state.registry.snapshot().plugins.len(), 1);
    }

    #[tokio::test]
    async fn invalid_config_keeps_previous_runtime() {
        let dir = TmpDir::new("reload-badcfg");
        dir.write_plugin("p.wasm", OK_WAT.as_bytes());
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);
        reload(&state).await.unwrap();

        // Route referencing an unknown upstream fails validation.
        let bad = fs::read_to_string(dir.config_path())
            .unwrap()
            .replace("upstream = \"u\"", "upstream = \"ghost\"");
        dir.write_config(&bad);
        let err = reload(&state).await.unwrap_err();
        assert!(err.to_string().contains("config"), "got: {err}");
        assert!(matches!(err, ReloadError::Failed(_)));
        assert_eq!(state.runtime.load().generation, 2);
    }

    #[tokio::test]
    async fn second_reload_while_one_is_running_is_rejected() {
        let dir = TmpDir::new("reload-inflight");
        dir.write_plugin("p.wasm", OK_WAT.as_bytes());
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);

        // Hold the gate exactly like an in-flight reload would.
        let _held = state.reload_gate.try_lock().unwrap();
        let err = reload(&state).await.unwrap_err();
        assert!(matches!(err, ReloadError::InFlight), "got: {err:?}");
        assert_eq!(err.to_string(), "reload already in progress");
        // Nothing happened: the boot generation is untouched.
        assert_eq!(state.runtime.load().generation, 1);
        assert_eq!(state.registry.snapshot().generation, 1);

        // Once the gate is released the reload works again.
        drop(_held);
        let report = reload(&state).await.unwrap();
        assert_eq!(report.generation, 2);
    }

    #[tokio::test]
    async fn apply_config_hot_swaps_without_touching_the_filesystem() {
        let dir = TmpDir::new("apply-mem");
        dir.write_plugin("p.wasm", OK_WAT.as_bytes());
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);
        assert_eq!(state.runtime.load().generation, 1);

        // In-memory swap to a config that was never written to disk.
        let mut cfg = openrusty_core::load_config(&dir.config_path()).unwrap();
        cfg.routes[0].path_prefix = "/mem-only".to_string();
        let gen = apply_config(&state, &cfg).await.unwrap();
        assert_eq!(gen, 2);
        assert_eq!(state.runtime.load().generation, 2);
        assert_eq!(state.registry.snapshot().generation, 2);
        assert_eq!(state.runtime.load().routes[0].path_prefix, "/mem-only");

        // The on-disk config is untouched: a file reload still serves "/".
        let report = reload(&state).await.unwrap();
        assert_eq!(report.generation, 3);
        assert_eq!(state.runtime.load().routes[0].path_prefix, "/");
    }

    #[tokio::test]
    async fn failed_apply_config_keeps_previous_runtime() {
        let dir = TmpDir::new("apply-broken");
        dir.write_plugin("p.wasm", OK_WAT.as_bytes());
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);

        // Corrupt the module; the whole apply must fail atomically.
        dir.write_plugin("p.wasm", b"garbage");
        let cfg = openrusty_core::load_config(&dir.config_path()).unwrap();
        let err = apply_config(&state, &cfg).await.unwrap_err();
        assert!(
            matches!(err, ReloadError::Failed(ref e) if e.contains("plugins")),
            "got: {err:?}"
        );
        // Nothing changed: the boot snapshot and runtime stay in effect.
        assert_eq!(state.runtime.load().generation, 1);
        assert_eq!(state.registry.snapshot().generation, 1);
        assert_eq!(state.runtime.load().routes[0].path_prefix, "/");
    }

    #[tokio::test]
    async fn apply_config_shares_the_reload_gate_with_file_reloads() {
        let dir = TmpDir::new("apply-gate");
        dir.write_plugin("p.wasm", OK_WAT.as_bytes());
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);

        // Hold the gate exactly like an in-flight file reload would.
        let cfg = openrusty_core::load_config(&dir.config_path()).unwrap();
        let _held = state.reload_gate.try_lock().unwrap();
        let err = apply_config(&state, &cfg).await.unwrap_err();
        assert!(matches!(err, ReloadError::InFlight), "got: {err:?}");
        assert_eq!(state.runtime.load().generation, 1);
        assert_eq!(state.registry.snapshot().generation, 1);
    }

    /// Dynamic module used by the registry-swap drills: content-phase Done
    /// with a body (200), written as wat text (wasmtime compiles it).
    const DYN_BODY_MOD: &str = r#"(module
  (import "openrusty" "resp_body_set" (func $set (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "dyn-body")
  (func (export "orr_on_phase") (param $phase i32) (param $aux i32) (result i32)
    (if (i32.eq (local.get $phase) (i32.const 3))
      (then
        (drop (call $set (i32.const 0) (i32.const 8)))
        (return (i32.const -4))))
    i32.const -5)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))"#;

    /// Minimal POST context for a direct registry invocation.
    fn dyn_ctx() -> openrusty_core::ReqCtx {
        openrusty_core::ReqCtx {
            method: "POST".into(),
            path: "/api/v1/dynamic/prog".into(),
            query: String::new(),
            version: "HTTP/1.1".into(),
            client_addr: "127.0.0.1:40015".parse().unwrap(),
            headers: Vec::new(),
            route_index: None,
            upstream: None,
            peer_index: None,
            attempts: 0,
            tried: Vec::new(),
        }
    }

    /// Scratch dir (kept alive by the caller) whose config enables
    /// `[dynamic]` pointing at `<dir>/dyn-<tag>` with one `prog.wasm`.
    fn dynamic_state(tag: &str) -> (TmpDir, Arc<AppState>) {
        let dir = TmpDir::new(&format!("dyn-{tag}"));
        dir.write_plugin("p.wasm", OK_WAT.as_bytes());
        let dyn_dir = dir.0.join(format!("dyn-{tag}"));
        fs::create_dir_all(&dyn_dir).unwrap();
        fs::write(dyn_dir.join("prog.wasm"), DYN_BODY_MOD.as_bytes()).unwrap();
        dir.write_config(&format!(
            "{}\n[dynamic]\ndir = \"{}\"\n",
            dir.standard_config(),
            dyn_dir.display()
        ));
        let state = boot_state(&dir);
        (dir, state)
    }

    /// A changed `[dynamic]` dir swaps in a fresh registry: the old name
    /// is gone and the compile cache starts over (stale modules must
    /// never keep serving).
    #[tokio::test]
    async fn changed_dynamic_dir_swaps_the_registry() {
        let (_dir, state) = dynamic_state("swap-a");
        let reg = state.dynamic.load_full();
        let reg = (*reg).as_ref().unwrap();
        assert_eq!(
            reg.invoke("prog", dyn_ctx(), bytes::Bytes::new()).await.status,
            200
        );
        assert_eq!(reg.compiled_count(), 1);

        // Same config shape, different dir (empty: nothing to resolve).
        let dir_b = state.config_path.parent().unwrap().join("dyn-swap-b");
        fs::create_dir_all(&dir_b).unwrap();
        let mut cfg = openrusty_core::load_config(&state.config_path).unwrap();
        cfg.dynamic.as_mut().unwrap().dir = dir_b.display().to_string();
        apply_config(&state, &cfg).await.unwrap();

        // Fresh registry: old name 404s (dir B has no file), the compile
        // counter restarted from zero.
        let reg = state.dynamic.load_full();
        let reg = (*reg).as_ref().unwrap();
        let out = reg.invoke("prog", dyn_ctx(), bytes::Bytes::new()).await;
        assert_eq!(out.status, 404, "old module must be gone");
        assert_eq!(reg.compiled_count(), 0);
    }

    /// An unchanged `[dynamic]` section must NOT rebuild the registry:
    /// same `Arc` identity (in-flight requests and the stat-driven
    /// compile cache keep working across the reload).
    #[tokio::test]
    async fn unchanged_dynamic_config_keeps_the_registry() {
        let (_dir, state) = dynamic_state("keep");
        let before = state.dynamic.load_full();
        let reg = (*before).as_ref().unwrap();
        assert_eq!(
            reg.invoke("prog", dyn_ctx(), bytes::Bytes::new()).await.status,
            200
        );
        assert_eq!(reg.compiled_count(), 1);

        // Identical config file: plugins/runtime reload, the dynamic
        // registry stays exactly as it was.
        let cfg = openrusty_core::load_config(&state.config_path).unwrap();
        apply_config(&state, &cfg).await.unwrap();
        let after = state.dynamic.load_full();
        // `(*x).as_ref()`: borrow the Option inside the outer Arc, so the
        // inner registry Arc is compared by pointer, not cloned.
        let before_reg = (*before).as_ref().unwrap();
        let after_reg = (*after).as_ref().unwrap();
        assert!(
            Arc::ptr_eq(before_reg, after_reg),
            "registry must not be rebuilt for an identical [dynamic] section"
        );

        // Cache stayed warm: the same module serves without recompiling.
        let reg = (*after).as_ref().unwrap();
        assert_eq!(
            reg.invoke("prog", dyn_ctx(), bytes::Bytes::new()).await.status,
            200
        );
        assert_eq!(reg.compiled_count(), 1);
    }
}
