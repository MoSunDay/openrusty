//! Reload plumbing: on-disk plugin discovery and the read/compile/validate
//! pass that builds the next snapshot, carrying over shared state by name.

use crate::host_state::HostState;
use crate::instance::HostData;
use crate::linker::validate_module;
use crate::registry::{LoadedPlugin, PluginSnapshot, ReloadError};
use openrusty_core::config::Config;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use wasmtime::{Engine, Linker, Module};

/// Sorted plugin names (file stems) for every `*.wasm` in `dir`.
pub(crate) fn discover(dir: &Path) -> Result<Vec<String>, ReloadError> {
    let mut names = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| ReloadError::Io(e.to_string()))?;
    for entry in entries {
        let path = entry.map_err(|e| ReloadError::Io(e.to_string()))?.path();
        if path.extension().and_then(|s| s.to_str()) == Some("wasm") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

/// Read, compile and validate every plugin; carry over shared state from
/// `prev` by name. Sequential on purpose: one blocking task does it all.
pub(crate) fn load_plugins(
    engine: &Engine,
    linker: &Linker<HostData>,
    cfg: &Config,
    dir: &Path,
    names: &[String],
    prev: Option<&PluginSnapshot>,
) -> Result<Vec<Arc<LoadedPlugin>>, ReloadError> {
    let prev_states: HashMap<&str, Arc<HostState>> = prev
        .map(|s| {
            s.plugins
                .iter()
                .map(|p| (p.name.as_str(), p.state.clone()))
                .collect()
        })
        .unwrap_or_default();
    let mut plugins = Vec::with_capacity(names.len());
    for name in names {
        let bytes = std::fs::read(dir.join(format!("{name}.wasm")))
            .map_err(|e| ReloadError::Io(e.to_string()))?;
        let module = Module::new(engine, bytes).map_err(|e| ReloadError::Compile {
            plugin: name.clone(),
            detail: e.to_string(),
        })?;
        let info = validate_module(engine, linker, &module).map_err(|e| ReloadError::Abi {
            plugin: name.clone(),
            detail: e.to_string(),
        })?;
        let state = prev_states
            .get(name.as_str())
            .cloned()
            .unwrap_or_else(|| Arc::new(HostState::new(name.clone())));
        let settings = Arc::new(cfg.plugins.settings.get(name).cloned().unwrap_or_default());
        plugins.push(Arc::new(LoadedPlugin {
            name: name.clone(),
            module,
            state,
            settings,
            timeout: Duration::from_millis(cfg.plugins.timeout_ms),
            fail_policy: cfg.plugins.on_failure,
            memory_mb: cfg.plugins.max_memory_mb,
            has_dealloc: info.has_dealloc,
        }));
    }
    Ok(plugins)
}
