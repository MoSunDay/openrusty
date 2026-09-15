//! Plugin registry: engine + linker + an atomically hot-swappable snapshot
//! of compiled, ABI-validated plugin modules.
//!
//! Reload compiles everything off the request path; any failure aborts the
//! whole reload and keeps the previous snapshot. Per-plugin shared state
//! (KV, error counters) is carried over by plugin name.

use crate::epoch::EpochTicker;
use crate::host_state::HostState;
use crate::instance::HostData;
use crate::linker::build_linker;
use crate::registry_validate::{discover, load_plugins};
use arc_swap::{ArcSwap, Guard};
use openrusty_core::config::{Config, FailPolicy};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use wasmtime::{Engine, Linker, Module};

/// A compiled, validated plugin plus its per-request parameters.
pub struct LoadedPlugin {
    pub name: String,
    pub module: Module,
    /// Shared state; survives reloads (keyed by name).
    pub state: Arc<HostState>,
    pub settings: Arc<HashMap<String, String>>,
    pub timeout: Duration,
    pub fail_policy: FailPolicy,
    pub memory_mb: u32,
    pub has_dealloc: bool,
}

/// Immutable, atomically-published view of all plugins.
pub struct PluginSnapshot {
    pub generation: u64,
    pub plugins: Vec<Arc<LoadedPlugin>>,
}

/// How many times a reload rebuilds + retries its publish before giving up
/// with [`ReloadError::Conflict`] (each retry observes the latest snapshot,
/// so the winner's work is never repeated more than necessary).
const RELOAD_ATTEMPTS: usize = 3;

/// Reload failure; the previous snapshot is untouched in every case.
#[derive(Debug, Error)]
pub enum ReloadError {
    #[error("io error: {0}")]
    Io(String),
    #[error("compile failed for plugin {plugin}: {detail}")]
    Compile { plugin: String, detail: String },
    #[error("abi validation failed for plugin {plugin}: {detail}")]
    Abi { plugin: String, detail: String },
    /// The publish race against a concurrent reload was lost repeatedly;
    /// the current snapshot is untouched and the caller may simply retry.
    #[error("reload conflict: concurrent reload kept publishing first")]
    Conflict,
}

/// Engine with epoch interruption (required for phase timeouts) plus its
/// epoch ticker. Dropping the ticker stops and joins the bump thread, so
/// an engine built here never leaks it.
pub fn new_engine() -> Result<EpochTicker, wasmtime::Error> {
    let mut cfg = wasmtime::Config::new();
    cfg.epoch_interruption(true);
    Ok(EpochTicker::new(Engine::new(&cfg)?))
}

pub struct PluginRegistry {
    ticker: EpochTicker,
    linker: Linker<HostData>,
    swap: ArcSwap<PluginSnapshot>,
}

impl PluginRegistry {
    /// Build the registry and load plugins from `cfg.plugins.dir`.
    /// A missing or empty directory is not fatal: an empty snapshot
    /// (generation 0) is published and a warning is logged.
    pub fn bootstrap(cfg: &Config) -> Result<Arc<Self>, ReloadError> {
        let ticker = new_engine().map_err(|e| ReloadError::Abi {
            plugin: "engine".into(),
            detail: e.to_string(),
        })?;
        let engine = ticker.engine().clone();
        let linker = build_linker(&engine).map_err(|e| ReloadError::Abi {
            plugin: "linker".into(),
            detail: e.to_string(),
        })?;
        let dir = PathBuf::from(&cfg.plugins.dir);
        let snap = match discover(&dir) {
            Ok(names) if !names.is_empty() => {
                let ordered = order_plugins(&names, &cfg.plugins.order);
                let plugins = load_plugins(&engine, &linker, cfg, &dir, &ordered, None)?;
                PluginSnapshot {
                    generation: 1,
                    plugins,
                }
            }
            Ok(_) => {
                tracing::warn!(dir = %dir.display(), "plugin dir is empty; no plugins loaded");
                PluginSnapshot {
                    generation: 0,
                    plugins: Vec::new(),
                }
            }
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "plugin dir unreadable; no plugins loaded");
                PluginSnapshot {
                    generation: 0,
                    plugins: Vec::new(),
                }
            }
        };
        Ok(Arc::new(PluginRegistry {
            ticker,
            linker,
            swap: ArcSwap::from_pointee(snap),
        }))
    }

    pub fn engine(&self) -> &Engine {
        self.ticker.engine()
    }

    pub fn linker(&self) -> &Linker<HostData> {
        &self.linker
    }

    pub fn snapshot(&self) -> Arc<PluginSnapshot> {
        self.swap.load_full()
    }

    /// Re-read the plugin dir, compile + validate every module off the
    /// request path, and atomically publish a new snapshot. Any failure
    /// returns Err and leaves the current snapshot untouched. Returns the
    /// new generation number.
    ///
    /// Concurrent reloads are safe: the snapshot is built from the
    /// generation observed at build start and published with a
    /// compare-and-swap on that exact snapshot. If another reload won the
    /// race meanwhile, the whole build is redone against the fresh
    /// snapshot (bounded by [`RELOAD_ATTEMPTS`], then `Conflict`).
    pub async fn reload(&self, cfg: &Config) -> Result<u64, ReloadError> {
        for _ in 0..RELOAD_ATTEMPTS {
            let current = self.swap.load_full();
            let engine = self.ticker.engine().clone();
            let linker = self.linker.clone();
            let cfg = cfg.clone();
            let build_from = Arc::clone(&current);
            let work = move || -> Result<PluginSnapshot, ReloadError> {
                let dir = PathBuf::from(&cfg.plugins.dir);
                let names = discover(&dir)?;
                let ordered = order_plugins(&names, &cfg.plugins.order);
                let plugins =
                    load_plugins(&engine, &linker, &cfg, &dir, &ordered, Some(&build_from))?;
                Ok(PluginSnapshot {
                    generation: build_from.generation + 1,
                    plugins,
                })
            };
            // Compile off the async worker; fall back to inline when there
            // is no tokio runtime (e.g. sync callers in tests).
            let snap = if tokio::runtime::Handle::try_current().is_ok() {
                tokio::task::spawn_blocking(work)
                    .await
                    .map_err(|e| ReloadError::Io(e.to_string()))??
            } else {
                work()?
            };
            let generation = snap.generation;
            // Publish only if nobody else reloaded in the meantime; the
            // returned guard holds the previous snapshot either way.
            let published = self.swap.compare_and_swap(&current, Arc::new(snap));
            if Arc::ptr_eq(&Guard::into_inner(published), &current) {
                return Ok(generation);
            }
            tracing::debug!("reload lost the publish race; rebuilding");
        }
        Err(ReloadError::Conflict)
    }

    /// `(plugin name, error_count)` for the current snapshot.
    pub fn status(&self) -> Vec<(String, u64)> {
        self.snapshot()
            .plugins
            .iter()
            .map(|p| (p.name.clone(), p.state.error_count()))
            .collect()
    }

    /// Prometheus view of the current snapshot. Parallel to [`status`] but
    /// with the kind breakdown and KV size the metrics endpoint needs.
    pub fn metric_view(&self) -> Vec<PluginMetrics> {
        self.snapshot()
            .plugins
            .iter()
            .map(|p| (p.name.clone(), p.state.error_kinds(), p.state.kv_len()))
            .collect()
    }
}

/// One plugin's metrics row: `(name, per-kind error counts, live KV entry
/// count)`, as rendered by the `/openrusty/metrics` endpoint.
pub type PluginMetrics = (String, Vec<(String, u64)>, usize);

/// Final plugin order: names listed in `order` first (in that order, if
/// the file exists), then the remaining files alphabetically. Pure.
fn order_plugins(files: &[String], order: &[String]) -> Vec<String> {
    let mut out: Vec<String> = order
        .iter()
        .filter(|n| files.iter().any(|f| f == *n))
        .cloned()
        .collect();
    for f in files {
        if !out.contains(f) {
            out.push(f.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrusty_core::config::{PluginsConfig, ServerConfig};
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    const OK_WAT: &str = r#"
(module
  (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const 0)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
  (memory (export "memory") 1))
"#;

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// Self-cleaning temp directory for plugin files.
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::SeqCst);
            let p = std::env::temp_dir()
                .join(format!("openrusty-wasm-{tag}-{}-{n}", std::process::id()));
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

    #[test]
    fn ordering_prefers_configured_list() {
        let files: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let order: Vec<String> = ["c", "zz-missing", "a"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            order_plugins(&files, &order),
            vec!["c".to_string(), "a".to_string(), "b".to_string()]
        );
        assert_eq!(
            order_plugins(&files, &[]),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn bootstrap_missing_dir_is_empty_gen0() {
        let cfg = test_cfg("/nonexistent/openrusty/plugins", &[]);
        let reg = PluginRegistry::bootstrap(&cfg).unwrap();
        let snap = reg.snapshot();
        assert_eq!(snap.generation, 0);
        assert!(snap.plugins.is_empty());
    }

    #[tokio::test]
    async fn bootstrap_and_hot_reload() {
        let dir = TmpDir::new("registry");
        // Module::new auto-detects the text format, so wat-in-.wasm works.
        dir.write("a.wasm", OK_WAT.as_bytes());
        dir.write("b.wasm", OK_WAT.as_bytes());
        let cfg = test_cfg(dir.0.to_str().unwrap(), &["b", "a"]);

        let reg = PluginRegistry::bootstrap(&cfg).unwrap();
        let snap = reg.snapshot();
        assert_eq!(snap.generation, 1);
        let names: Vec<&str> = snap.plugins.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["b", "a"]);

        // Pre-seed KV on plugin "a" (second in order).
        snap.plugins[1].state.kv_set(b"k", b"v".to_vec(), 0);

        // A broken module aborts the whole reload.
        dir.write("c.wasm", b"garbage");
        assert!(reg.reload(&cfg).await.is_err());
        let snap = reg.snapshot();
        assert_eq!(snap.generation, 1);
        assert_eq!(snap.plugins.len(), 2);

        // Fix the dir: reload succeeds and shared state survives.
        dir.remove("c.wasm");
        let gen = reg.reload(&cfg).await.unwrap();
        assert_eq!(gen, 2);
        let snap = reg.snapshot();
        assert_eq!(snap.generation, 2);
        let a = snap.plugins.iter().find(|p| p.name == "a").unwrap();
        assert_eq!(a.state.kv_get(b"k"), Some(b"v".to_vec()));
        let status = reg.status();
        assert_eq!(status.len(), 2);
        assert!(status.iter().all(|(_, errs)| *errs == 0));
    }

    #[tokio::test]
    async fn reload_removes_deleted_and_adds_new_plugins() {
        let dir = TmpDir::new("registry2");
        dir.write("a.wasm", OK_WAT.as_bytes());
        dir.write("b.wasm", OK_WAT.as_bytes());
        let cfg = test_cfg(dir.0.to_str().unwrap(), &[]);
        let reg = PluginRegistry::bootstrap(&cfg).unwrap();
        assert_eq!(reg.snapshot().plugins.len(), 2);

        dir.remove("b.wasm");
        dir.write("c.wasm", OK_WAT.as_bytes());
        let gen = reg.reload(&cfg).await.unwrap();
        assert_eq!(gen, 2);
        let snap = reg.snapshot();
        let names: Vec<&str> = snap.plugins.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["a", "c"]);
    }

    /// Regression: concurrent reloads must publish one generation per
    /// success (never a stale last-writer-wins generation), and the final
    /// snapshot must be internally consistent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reloads_publish_one_generation_each() {
        let dir = TmpDir::new("registry-race");
        dir.write("a.wasm", OK_WAT.as_bytes());
        dir.write("b.wasm", OK_WAT.as_bytes());
        let reg = PluginRegistry::bootstrap(&test_cfg(dir.0.to_str().unwrap(), &[])).unwrap();
        assert_eq!(reg.snapshot().generation, 1);

        const N: usize = 8;
        let mut handles = Vec::new();
        for i in 0..N {
            // Different configs (per-reload plugin orders) so each reload
            // really builds its own snapshot.
            let order: Vec<&str> = if i % 2 == 0 {
                vec!["b", "a"]
            } else {
                vec!["a", "b"]
            };
            let cfg = test_cfg(dir.0.to_str().unwrap(), &order);
            let reg = Arc::clone(&reg);
            handles.push(tokio::spawn(async move { reg.reload(&cfg).await }));
        }

        // JoinHandle::await yields Result<Result<u64, ReloadError>>; only
        // the inner Ok counts as a published generation.
        let mut succeeded = 0u64;
        for h in handles {
            if matches!(h.await, Ok(Ok(_))) {
                succeeded += 1;
            }
        }
        assert!(
            succeeded >= 1,
            "at least one concurrent reload must win the publish race"
        );

        let snap = reg.snapshot();
        assert_eq!(
            snap.generation,
            1 + succeeded,
            "every successful reload must advance the generation by exactly one"
        );
        assert_eq!(snap.plugins.len(), 2);
        let names: Vec<&str> = snap.plugins.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"a") && names.contains(&"b"));
    }

    #[tokio::test]
    async fn bad_abi_module_fails_reload() {
        let dir = TmpDir::new("registry3");
        dir.write("a.wasm", OK_WAT.as_bytes());
        // Compiles fine but lacks orr_alloc -> ABI validation failure.
        let no_alloc = r#"(module
            (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const 0)
            (memory (export "memory") 1))"#;
        dir.write("bad.wasm", no_alloc.as_bytes());
        let cfg = test_cfg(dir.0.to_str().unwrap(), &[]);
        // Bootstrap aborts entirely on the bad module.
        assert!(matches!(
            PluginRegistry::bootstrap(&cfg),
            Err(ReloadError::Abi { .. })
        ));
        // Without it, bootstrap succeeds.
        dir.remove("bad.wasm");
        let reg = PluginRegistry::bootstrap(&cfg).unwrap();
        assert_eq!(reg.snapshot().generation, 1);
        assert_eq!(reg.snapshot().plugins.len(), 1);
        // And re-adding it makes the next reload fail atomically.
        dir.write("bad.wasm", no_alloc.as_bytes());
        assert!(reg.reload(&cfg).await.is_err());
        assert_eq!(reg.snapshot().generation, 1);
        assert_eq!(reg.snapshot().plugins.len(), 1);
    }
}
