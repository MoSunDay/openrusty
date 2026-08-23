//! Atomic hot reload: re-read + validate the config, recompile plugins off
//! the request path, and only then rebuild the routing/upstream runtime.
//!
//! Ordering matters: `apply_runtime` runs after the plugin registry has
//! published its new snapshot, so any failure (config parse, validation,
//! compile, ABI) leaves both the old plugin snapshot and the old runtime
//! untouched.

use crate::state::{apply_runtime, AppState};
use openrusty_core::load_config;

/// Outcome of a successful reload.
#[derive(Debug)]
pub struct ReloadReport {
    pub generation: u64,
    pub plugins: Vec<String>,
}

/// Run one full reload cycle against `state.config_path`.
///
/// Any error is returned as a plain message and nothing changed: the
/// previous plugin snapshot and runtime stay in effect.
pub async fn reload(state: &AppState) -> Result<ReloadReport, String> {
    let path = state.config_path.clone();
    let cfg = tokio::task::spawn_blocking(move || load_config(&path))
        .await
        .map_err(|e| format!("reload task failed: {e}"))?
        .map_err(|e| format!("config: {e}"))?;

    // Compiles off the request path; Err leaves the old snapshot in place.
    let generation = state
        .registry
        .reload(&cfg)
        .await
        .map_err(|e| format!("plugins: {e}"))?;

    // Plugins are already published; now swap the runtime to match.
    apply_runtime(state, &cfg, generation);

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
        assert!(err.contains("plugins"), "got: {err}");
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
        assert!(err.contains("config"), "got: {err}");
        assert_eq!(state.runtime.load().generation, 2);
    }
}
