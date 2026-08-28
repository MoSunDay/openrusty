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

/// Sorted plugin names (file stems) for every `*.wasm` regular file in
/// `dir`. Non-wasm entries are ignored silently; a `*.wasm` that is not a
/// regular file, is unreadable, or has a non-UTF-8 name is skipped with a
/// warning (the log uses the lossy file name).
pub(crate) fn discover(dir: &Path) -> Result<Vec<String>, ReloadError> {
    let mut names = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| ReloadError::Io(e.to_string()))?;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "unreadable plugin dir entry skipped");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("wasm") {
            continue;
        }
        // Directories (or stat errors) named `*.wasm` would only fail
        // confusingly later in the read/compile pass; drop them here.
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => {}
            Ok(_) => {
                tracing::warn!(path = %path.display(), "skipping `*.wasm` that is not a regular file");
                continue;
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skipping unreadable `*.wasm`");
                continue;
            }
        }
        match path.file_stem().and_then(|s| s.to_str()) {
            Some(stem) => names.push(stem.to_string()),
            None => {
                tracing::warn!(path = %path.display(), "skipping `*.wasm` with a non-UTF-8 file name");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// Self-cleaning temp directory for plugin files.
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::SeqCst);
            let p = std::env::temp_dir().join(format!(
                "openrusty-wasm-discover-{tag}-{}-{n}",
                std::process::id()
            ));
            fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
        fn write(&self, name: &OsStr, bytes: &[u8]) {
            fs::write(self.0.join(name), bytes).unwrap();
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn discover_lists_sorted_wasm_stems_ignoring_other_files() {
        let dir = TmpDir::new("normal");
        dir.write(OsStr::new("b.wasm"), b"\0asm");
        dir.write(OsStr::new("a.wasm"), b"\0asm");
        dir.write(OsStr::new("notes.txt"), b"ignored");
        dir.write(OsStr::new("noext"), b"ignored");
        assert_eq!(
            discover(&dir.0).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn discover_skips_directory_named_wasm() {
        let dir = TmpDir::new("dir-wasm");
        fs::create_dir_all(dir.0.join("fake.wasm")).unwrap();
        dir.write(OsStr::new("real.wasm"), b"\0asm");
        // Only the regular file is discovered; the `*.wasm` directory is
        // skipped instead of failing confusingly in the read/compile pass.
        assert_eq!(discover(&dir.0).unwrap(), vec!["real".to_string()]);
    }

    #[test]
    fn discover_skips_non_utf8_wasm_name() {
        // Raw-bytes file name: no valid plugin name can come from it. Some
        // filesystems (e.g. zfs mounted with `utf8only`) refuse to create
        // such names at all, and there the skip path cannot be exercised.
        let Some(dir) = non_utf8_dir() else {
            eprintln!("skipping: no local filesystem allows non-UTF-8 file names");
            return;
        };
        dir.write(OsStr::from_bytes(b"bad\xff.wasm"), b"\0asm");
        dir.write(OsStr::new("good.wasm"), b"\0asm");
        assert_eq!(discover(&dir.0).unwrap(), vec!["good".to_string()]);
    }

    /// Temp directory on a filesystem that accepts non-UTF-8 file names:
    /// the default temp dir first, `/dev/shm` (tmpfs) as the fallback.
    /// `None` when neither can hold such a name.
    fn non_utf8_dir() -> Option<TmpDir> {
        for base in [std::env::temp_dir(), PathBuf::from("/dev/shm")] {
            let n = SEQ.fetch_add(1, Ordering::SeqCst);
            let p = base.join(format!(
                "openrusty-wasm-discover-non-utf8-{}-{n}",
                std::process::id()
            ));
            if fs::create_dir_all(&p).is_err() {
                continue;
            }
            let dir = TmpDir(p);
            let probe = dir.0.join(OsStr::from_bytes(b"probe\xff"));
            match fs::write(&probe, b"") {
                Ok(()) => {
                    let _ = fs::remove_file(&probe);
                    return Some(dir);
                }
                Err(_) => {
                    let _ = fs::remove_dir_all(&dir.0);
                }
            }
        }
        None
    }
}
