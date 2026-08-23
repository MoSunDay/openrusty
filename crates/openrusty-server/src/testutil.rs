//! Shared test helpers: scratch directories, config generation and a
//! fully wired `AppState` without any real listener.

use crate::state::{self, AppState};
use openrusty_core::load_config;
use openrusty_proxy as proxy;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

static SEQ: AtomicU32 = AtomicU32::new(0);

/// Minimal ABI-valid plugin: declines every phase.
pub const OK_WAT: &str = r#"(module
    (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const -5)
    (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
    (memory (export "memory") 1))"#;

/// Scratch directory removed on drop. Layout:
///   <dir>/plugins/      plugin dir referenced by the generated config
///   <dir>/openrusty.toml
pub struct TmpDir(pub PathBuf);

impl TmpDir {
    pub fn new(tag: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let p =
            std::env::temp_dir().join(format!("openrusty-srv-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(p.join("plugins")).unwrap();
        TmpDir(p)
    }

    pub fn plugins_dir(&self) -> PathBuf {
        self.0.join("plugins")
    }

    pub fn config_path(&self) -> PathBuf {
        self.0.join("openrusty.toml")
    }

    pub fn write_plugin(&self, name: &str, body: &[u8]) {
        fs::write(self.plugins_dir().join(name), body).unwrap();
    }

    pub fn write_config(&self, body: &str) {
        fs::write(self.config_path(), body).unwrap();
    }

    /// Standard test config: one upstream `u` with one peer, one catch-all
    /// route, plugins loaded from this scratch dir.
    pub fn standard_config(&self) -> String {
        format!(
            r#"
[server]
listen = "127.0.0.1:18080"

[plugins]
dir = "{}"

[[upstreams]]
name = "u"
  [[upstreams.peers]]
  addr = "127.0.0.1:9001"

[[routes]]
path_prefix = "/"
upstream = "u"
"#,
            self.plugins_dir().display()
        )
    }
    /// Same as [`standard_config`] but the single route only matches the
    /// `/api` prefix, so unmatched paths exercise the 404 branch.
    pub fn prefix_only_config(&self) -> String {
        self.standard_config()
            .replace("path_prefix = \"/\"", "path_prefix = \"/api\"")
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Build an `AppState` from the scratch dir's config: bootstrap the plugin
/// registry and publish the first runtime snapshot.
pub fn boot_state(dir: &TmpDir) -> Arc<AppState> {
    let cfg = load_config(&dir.config_path()).unwrap();
    let registry = openrusty_wasm::PluginRegistry::bootstrap(&cfg).unwrap();
    let state = Arc::new(AppState {
        registry,
        health: Arc::new(proxy::new()),
        pool: Arc::new(proxy::new_pool()),
        runtime: arc_swap::ArcSwap::from_pointee(state::empty_runtime()),
        config_path: dir.config_path(),
        started_at: std::time::Instant::now(),
    });
    let gen = state.registry.snapshot().generation;
    state::apply_runtime(&state, &cfg, gen);
    state
}
