//! Atomic hot reload: re-read + validate the config, recompile plugins off
//! the request path, and only then rebuild the routing/upstream runtime.
//!
//! Ordering matters: `apply_runtime` runs after the plugin registry has
//! published its new snapshot, so any failure (config parse, validation,
//! compile, ABI) leaves both the old plugin snapshot and the old runtime
//! untouched.

use crate::state::{apply_runtime, AppState};
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

    // Compiles off the request path; Err leaves the old snapshot in place.
    let generation = state
        .registry
        .reload(&cfg)
        .await
        .map_err(|e| ReloadError::Failed(format!("plugins: {e}")))?;

    // Plugins are already published; now swap the runtime to match, then
    // re-arm active probing -- both inside the reload gate.
    apply_runtime(state, &cfg, generation);
    crate::active_probe::spawn(state);

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
}
