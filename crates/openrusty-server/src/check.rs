//! `openrusty -t [CONFIG]`: dry-run configuration test (nginx `-t` vibe).
//!
//! Exercises every stage that would otherwise fail at boot - TOML parse,
//! cross-field validation, plugin discovery/compile/ABI - without ever
//! binding a socket or entering the serve loop. Pure plumbing over the
//! exact code paths the server uses ([`load_config`],
//! [`openrusty_core::config::validate`], [`PluginRegistry::bootstrap`]);
//! no validation logic lives here.

use openrusty_core::config::{self, effective_listeners, ConfigError};
use openrusty_core::load_config;
use openrusty_wasm::PluginRegistry;
use std::fmt::Write;
use std::path::Path;

/// Owned result of a successful dry run: the report borrows neither the
/// config nor the registry, so neither outlives [`run`].
#[derive(Debug)]
pub struct Summary {
    pub config_path: String,
    /// Effective sockets as `role@addr` (see `effective_listeners`).
    pub listeners: Vec<String>,
    pub route_count: usize,
    pub upstream_names: Vec<String>,
    /// Plugin names in effective phase order.
    pub plugin_names: Vec<String>,
    pub plugin_dir: String,
    pub generation: u64,
}

/// Dry-run `path`: parse + validate the config, then compile and
/// ABI-check every plugin in `cfg.plugins.dir` (a missing or empty dir is
/// not fatal, matching server boot). Synchronous on purpose: the CLI
/// invokes it before any listener or runtime work exists.
pub fn run(path: &Path) -> Result<Summary, String> {
    let cfg = load_config(path).map_err(|e| match e {
        ConfigError::Io(e) => format!("cannot read config file {}: {e}", path.display()),
        ConfigError::Parse(e) => format!("config file {} parse error: {e}", path.display()),
        ConfigError::Invalid(m) => format!("config file {} invalid: {m}", path.display()),
    })?;
    // `load_config` already ran this; the explicit re-run keeps the
    // dry-run stages (parse -> validate -> compile) self-documenting.
    // It is pure and cheap.
    config::validate(&cfg).map_err(|e| format!("config file {} invalid: {e}", path.display()))?;
    let registry = PluginRegistry::bootstrap(&cfg)
        .map_err(|e| format!("plugin load failed from {}: {e}", cfg.plugins.dir))?;
    let snap = registry.snapshot();
    Ok(Summary {
        config_path: path.display().to_string(),
        listeners: effective_listeners(&cfg)
            .into_iter()
            .map(|l| format!("{}@{}", l.role.as_str(), l.listen))
            .collect(),
        route_count: cfg.routes.len(),
        upstream_names: cfg.upstreams.iter().map(|u| u.name.clone()).collect(),
        plugin_names: snap.plugins.iter().map(|p| p.name.clone()).collect(),
        plugin_dir: cfg.plugins.dir.clone(),
        generation: snap.generation,
    })
}

impl Summary {
    /// Plain-text report to stdout, nginx `-t` flavored; error paths are
    /// printed to stderr by the caller.
    pub fn print(&self) {
        let mut out = String::new();
        let _ = writeln!(out, "config file {}", self.config_path);
        for l in &self.listeners {
            let _ = writeln!(out, "listener    {l}");
        }
        let _ = writeln!(out, "routes      {}", self.route_count);
        let _ = writeln!(out, "upstreams   {}", counted(&self.upstream_names));
        let _ = writeln!(out, "plugin dir  {}", self.plugin_dir);
        let _ = writeln!(
            out,
            "plugins     {} (phase order)",
            counted(&self.plugin_names)
        );
        let _ = writeln!(out, "generation  {}", self.generation);
        let _ = writeln!(out, "syntax is ok");
        print!("{out}");
    }
}

/// `0` for an empty list, else `n: a, b` - keeps report lines uniform.
fn counted(names: &[String]) -> String {
    if names.is_empty() {
        "0".to_string()
    } else {
        format!("{}: {}", names.len(), names.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TmpDir, OK_WAT};

    #[test]
    fn dry_run_ok_with_empty_plugin_dir() {
        let dir = TmpDir::new("check-ok");
        dir.write_config(&dir.standard_config());
        let s = run(&dir.config_path()).unwrap();
        assert_eq!(s.listeners, vec!["inbound@127.0.0.1:18080".to_string()]);
        assert_eq!(s.route_count, 1);
        assert_eq!(s.upstream_names, vec!["u".to_string()]);
        // An empty plugin dir is not fatal; it publishes generation 0.
        assert!(s.plugin_names.is_empty());
        assert_eq!(s.generation, 0);
    }

    #[test]
    fn dry_run_ok_lists_plugins_in_phase_order() {
        let dir = TmpDir::new("check-plugin");
        dir.write_config(&dir.standard_config());
        dir.write_plugin("p.wasm", OK_WAT.as_bytes());
        let s = run(&dir.config_path()).unwrap();
        assert_eq!(s.plugin_names, vec!["p".to_string()]);
        assert_eq!(s.generation, 1);
    }

    #[test]
    fn dry_run_rejects_malformed_toml() {
        let dir = TmpDir::new("check-parse");
        dir.write_config("not = [valid\n");
        let err = run(&dir.config_path()).unwrap_err();
        assert!(err.contains("parse error"), "unexpected: {err}");
        assert!(
            err.contains(dir.config_path().display().to_string().as_str()),
            "error must name the config path: {err}"
        );
    }

    #[test]
    fn dry_run_rejects_corrupt_plugin() {
        let dir = TmpDir::new("check-plugin-bad");
        dir.write_config(&dir.standard_config());
        dir.write_plugin("broken.wasm", b"definitely not a wasm module");
        let err = run(&dir.config_path()).unwrap_err();
        assert!(err.contains("broken"), "unexpected: {err}");
        assert!(err.contains("compile failed"), "unexpected: {err}");
    }

    #[test]
    fn dry_run_rejects_config_failing_validate() {
        let dir = TmpDir::new("check-invalid");
        dir.write_config(
            &dir.standard_config()
                .replace("upstream = \"u\"", "upstream = \"missing\""),
        );
        let err = run(&dir.config_path()).unwrap_err();
        assert!(err.contains("unknown upstream"), "unexpected: {err}");
    }
}
