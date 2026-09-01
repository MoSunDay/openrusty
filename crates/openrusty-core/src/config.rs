//! TOML configuration loading and validation (pure functions).
//!
//! Role-scoped listener types live in the [`listeners`] submodule and are
//! re-exported here, so `openrusty_core::config::ListenerConfig` and friends
//! keep their historical paths.

mod listeners;

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::Path;
use thiserror::Error;

pub use listeners::{ListenerConfig, ListenerRole, effective_listeners};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default)]
    pub upstreams: Vec<UpstreamConfig>,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Serve HTTP/1.1 only; `false` (default) auto-detects h2c on the same port.
    #[serde(default)]
    pub http1_only: bool,
    /// Role-scoped listeners under `[[server.listeners]]`. Empty (default) keeps the
    /// single-socket shape: one inbound listener derived from `listen`. When
    /// non-empty this list is the sole authority and `listen` above is
    /// ignored (see [`effective_listeners`]).
    #[serde(default)]
    pub listeners: Vec<ListenerConfig>,
}

fn default_log_level() -> String {
    "warn".to_string()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailPolicy {
    #[default]
    FailOpen,
    FailClosed,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PluginsConfig {
    /// Directory scanned for `*.wasm` plugin modules.
    pub dir: String,
    /// Execution order of plugin names within a phase.
    pub order: Vec<String>,
    /// Hard timeout for one plugin phase invocation.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Per-request memory ceiling for one plugin instance (MiB).
    #[serde(default = "default_max_memory_mb")]
    pub max_memory_mb: u32,
    #[serde(default)]
    pub on_failure: FailPolicy,
    /// Free-form per-plugin settings: plugin name -> key/value map.
    pub settings: BTreeMap<String, HashMap<String, String>>,
}

fn default_timeout_ms() -> u64 {
    50
}

fn default_max_memory_mb() -> u32 {
    16
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BalancerKind {
    #[default]
    Swrr,
    IpHash,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerConfig {
    pub addr: SocketAddr,
    #[serde(default = "default_weight")]
    pub weight: u32,
}

fn default_weight() -> u32 {
    1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthConfig {
    /// Passive failures allowed inside `fail_window_s` before the peer is
    /// marked down; `0` disables passive accounting (nginx semantics).
    #[serde(default = "default_max_fails")]
    pub max_fails: u32,
    #[serde(default = "default_fail_window_s")]
    pub fail_window_s: u64,
    #[serde(default = "default_fail_timeout_s")]
    pub fail_timeout_s: u64,
    /// Active health check; `Some` when `[upstreams.health.active]` is present.
    #[serde(default)]
    pub active: Option<ActiveHealthConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveHealthConfig {
    #[serde(default = "default_active_interval_ms")]
    pub interval_ms: u64,
    #[serde(default = "default_active_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_active_path")]
    pub path: String,
    #[serde(default = "default_unhealthy_threshold")]
    pub unhealthy_threshold: u32,
    #[serde(default = "default_healthy_threshold")]
    pub healthy_threshold: u32,
}

fn default_max_fails() -> u32 {
    3
}
fn default_fail_window_s() -> u64 {
    10
}
fn default_fail_timeout_s() -> u64 {
    10
}
fn default_active_interval_ms() -> u64 {
    1000
}
fn default_active_timeout_ms() -> u64 {
    1000
}
fn default_active_path() -> String {
    "/".to_string()
}
fn default_unhealthy_threshold() -> u32 {
    2
}
fn default_healthy_threshold() -> u32 {
    2
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            max_fails: default_max_fails(),
            fail_window_s: default_fail_window_s(),
            fail_timeout_s: default_fail_timeout_s(),
            active: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub name: String,
    #[serde(default)]
    pub balancer: BalancerKind,
    #[serde(default)]
    pub retries: u32,
    /// Retry the request on another peer when the route times out.
    #[serde(default)]
    pub retry_on_timeout: bool,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    /// Idle keep-alive connections are closed after this long (milliseconds).
    #[serde(default = "default_pool_idle_timeout_ms")]
    pub pool_idle_timeout_ms: u64,
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
    #[serde(default)]
    pub health: HealthConfig,
}

fn default_connect_timeout_ms() -> u64 {
    2000
}

fn default_pool_idle_timeout_ms() -> u64 {
    60_000
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    pub path_prefix: String,
    /// Optional exact host constraint (TOML: `host = "example.com"` under
    /// `[[routes]]`). When set, the route only matches requests whose
    /// `Host` header (port stripped, compared case-insensitively) equals
    /// this value; when absent the route matches any host. Host selection
    /// runs before path matching and never changes path precedence within
    /// a match class (see `openrusty-server` `pipeline::match_route`).
    #[serde(default)]
    pub host: Option<String>,
    pub upstream: String,
    #[serde(default)]
    pub timeout_ms: u64,
}

/// Read and parse the TOML configuration file, then validate it.
pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    let raw = std::fs::read_to_string(path)?;
    let cfg: Config = toml::from_str(&raw)?;
    validate(&cfg)?;
    Ok(cfg)
}

/// Pure validation: cross-field invariants the deserializer cannot express.
pub fn validate(cfg: &Config) -> Result<(), ConfigError> {
    let bad = |m: &str| ConfigError::Invalid(m.to_string());

    if !matches!(
        cfg.server.log_level.as_str(),
        "trace" | "debug" | "info" | "warn" | "error"
    ) {
        return Err(bad("server.log_level must be trace|debug|info|warn|error"));
    }
    if cfg.plugins.timeout_ms == 0 {
        return Err(bad("plugins.timeout_ms must be > 0"));
    }
    if cfg.plugins.max_memory_mb == 0 {
        return Err(bad("plugins.max_memory_mb must be > 0"));
    }
    // Listener-specific invariants (role/address uniqueness, transparency
    // sanity) live with the listener types.
    listeners::validate_listeners(cfg)?;
    // The plugin registry (openrusty-wasm) trusts `order` to name each plugin
    // at most once; a duplicate would make execution order ambiguous.
    let mut seen_order = std::collections::HashSet::new();
    for name in &cfg.plugins.order {
        if !seen_order.insert(name.as_str()) {
            return Err(bad(&format!(
                "plugins.order lists plugin {} more than once",
                name
            )));
        }
    }

    if cfg.upstreams.is_empty() && !cfg.routes.is_empty() {
        return Err(bad("routes defined but no upstreams"));
    }
    let mut seen = std::collections::HashSet::new();
    for up in &cfg.upstreams {
        if up.name.is_empty() {
            return Err(bad("upstream with empty name"));
        }
        if !seen.insert(up.name.clone()) {
            return Err(bad(&format!("duplicate upstream name: {}", up.name)));
        }
        if up.peers.is_empty() {
            return Err(bad(&format!("upstream {} has no peers", up.name)));
        }
        for (i, p) in up.peers.iter().enumerate() {
            if p.weight == 0 {
                return Err(bad(&format!(
                    "upstream {} peer #{i} has zero weight",
                    up.name
                )));
            }
        }
        if let Some(active) = &up.health.active {
            if active.interval_ms == 0 {
                return Err(bad(&format!(
                    "upstream {} health.active.interval_ms must be > 0",
                    up.name
                )));
            }
            if active.timeout_ms == 0 {
                return Err(bad(&format!(
                    "upstream {} health.active.timeout_ms must be > 0",
                    up.name
                )));
            }
            if active.unhealthy_threshold == 0 {
                return Err(bad(&format!(
                    "upstream {} health.active.unhealthy_threshold must be >= 1",
                    up.name
                )));
            }
            if active.healthy_threshold == 0 {
                return Err(bad(&format!(
                    "upstream {} health.active.healthy_threshold must be >= 1",
                    up.name
                )));
            }
            if !active.path.starts_with('/') {
                return Err(bad(&format!(
                    "upstream {} health.active.path must start with '/'",
                    up.name
                )));
            }
        }
    }
    for (i, route) in cfg.routes.iter().enumerate() {
        if !route.path_prefix.starts_with('/') {
            return Err(bad(&format!("route #{i} path_prefix must start with '/'")));
        }
        if let Some(h) = &route.host {
            if h.trim().is_empty() {
                return Err(bad(&format!("route #{i} host must not be empty")));
            }
        }
        if !cfg.upstreams.iter().any(|u| u.name == route.upstream) {
            return Err(bad(&format!(
                "route #{i} references unknown upstream: {}",
                route.upstream
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
[server]
listen = "127.0.0.1:8080"

[plugins]
dir = "build/plugins"

[[upstreams]]
name = "vllm"
  [[upstreams.peers]]
  addr = "127.0.0.1:9001"

[[routes]]
path_prefix = "/"
upstream = "vllm"
"#;

    #[test]
    fn parses_good_config() {
        let cfg: Config = toml::from_str(GOOD).unwrap();
        validate(&cfg).unwrap();
        assert_eq!(cfg.plugins.timeout_ms, 50);
        assert_eq!(cfg.plugins.on_failure, FailPolicy::FailOpen);
        assert_eq!(cfg.upstreams[0].balancer, BalancerKind::Swrr);
        // No [[server.listeners]] written: derive a single inbound listener from
        // server.listen (the historical single-socket shape).
        let ls = effective_listeners(&cfg);
        assert_eq!(ls.len(), 1);
        assert_eq!(ls[0].role, ListenerRole::Inbound);
        assert_eq!(ls[0].listen, "127.0.0.1:8080".parse().unwrap());
        assert!(!ls[0].http1_only);
    }

    #[test]
    fn route_host_is_optional_and_defaults_to_none() {
        // Absent `host`: backward compatible, matches any host.
        let plain: Config = toml::from_str(GOOD).unwrap();
        validate(&plain).unwrap();
        assert_eq!(plain.routes[0].host, None);

        // Present `host`: parsed verbatim (matching normalizes case later).
        let with_host: Config = toml::from_str(&GOOD.replace(
            "path_prefix = \"/\"",
            "host = \"Example.COM\"\npath_prefix = \"/\"",
        ))
        .unwrap();
        validate(&with_host).unwrap();
        assert_eq!(with_host.routes[0].host.as_deref(), Some("Example.COM"));

        // An empty host can never match; reject it at validation time.
        let blank: Config = toml::from_str(
            &GOOD.replace("path_prefix = \"/\"", "host = \"  \"\npath_prefix = \"/\""),
        )
        .unwrap();
        assert!(validate(&blank).is_err());
    }

    #[test]
    fn rejects_unknown_upstream() {
        let cfg: Config =
            toml::from_str(&GOOD.replace("upstream = \"vllm\"", "upstream = \"nope\"")).unwrap();
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn rejects_duplicate_plugin_order() {
        let cfg: Config = toml::from_str(&GOOD.replace(
            "dir = \"build/plugins\"",
            "dir = \"build/plugins\"\norder = [\"auth\", \"router\", \"auth\"]",
        ))
        .unwrap();
        let err = validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("plugins.order") && err.to_string().contains("auth"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn accepts_unique_plugin_order() {
        let cfg: Config = toml::from_str(&GOOD.replace(
            "dir = \"build/plugins\"",
            "dir = \"build/plugins\"\norder = [\"auth\", \"router\"]",
        ))
        .unwrap();
        validate(&cfg).unwrap();
    }

    #[test]
    fn rejects_zero_weight() {
        let cfg: Config = toml::from_str(&GOOD.replace(
            "addr = \"127.0.0.1:9001\"",
            "addr = \"127.0.0.1:9001\"\n  weight = 0",
        ))
        .unwrap();
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn rejects_bad_log_level() {
        let cfg: Config =
            toml::from_str("[server]\nlisten = \"127.0.0.1:8080\"\nlog_level = \"loud\"\n")
                .unwrap();
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn parses_retry_on_timeout_and_active_health() {
        let cfg: Config = toml::from_str(
            &GOOD
                .replace("name = \"vllm\"", "name = \"vllm\"\nretry_on_timeout = true")
                .replace(
                    "addr = \"127.0.0.1:9001\"",
                    "addr = \"127.0.0.1:9001\"\n  [upstreams.health.active]\n  interval_ms = 500\n  timeout_ms = 250\n  path = \"/healthz\"\n  unhealthy_threshold = 3\n  healthy_threshold = 1",
                ),
        )
        .unwrap();
        validate(&cfg).unwrap();
        let up = &cfg.upstreams[0];
        assert!(up.retry_on_timeout);
        let active = up.health.active.as_ref().unwrap();
        assert_eq!(active.interval_ms, 500);
        assert_eq!(active.timeout_ms, 250);
        assert_eq!(active.path, "/healthz");
        assert_eq!(active.unhealthy_threshold, 3);
        assert_eq!(active.healthy_threshold, 1);
    }

    #[test]
    fn active_health_defaults_to_none() {
        let cfg: Config = toml::from_str(GOOD).unwrap();
        validate(&cfg).unwrap();
        let up = &cfg.upstreams[0];
        assert!(!up.retry_on_timeout);
        assert!(up.health.active.is_none());
    }

    #[test]
    fn rejects_zero_active_interval() {
        let cfg: Config = toml::from_str(&GOOD.replace(
            "addr = \"127.0.0.1:9001\"",
            "addr = \"127.0.0.1:9001\"\n  [upstreams.health.active]\n  interval_ms = 0",
        ))
        .unwrap();
        let err = validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("health.active.interval_ms"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_zero_active_timeout() {
        let cfg: Config = toml::from_str(&GOOD.replace(
            "addr = \"127.0.0.1:9001\"",
            "addr = \"127.0.0.1:9001\"\n  [upstreams.health.active]\n  timeout_ms = 0",
        ))
        .unwrap();
        let err = validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("health.active.timeout_ms"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_zero_active_thresholds() {
        let cfg: Config = toml::from_str(&GOOD.replace(
            "addr = \"127.0.0.1:9001\"",
            "addr = \"127.0.0.1:9001\"\n  [upstreams.health.active]\n  unhealthy_threshold = 0",
        ))
        .unwrap();
        let err = validate(&cfg).unwrap_err();
        assert!(
            err.to_string()
                .contains("health.active.unhealthy_threshold"),
            "unexpected error: {err}"
        );

        let cfg: Config = toml::from_str(&GOOD.replace(
            "addr = \"127.0.0.1:9001\"",
            "addr = \"127.0.0.1:9001\"\n  [upstreams.health.active]\n  healthy_threshold = 0",
        ))
        .unwrap();
        let err = validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("health.active.healthy_threshold"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_active_path_without_leading_slash() {
        let cfg: Config = toml::from_str(&GOOD.replace(
            "addr = \"127.0.0.1:9001\"",
            "addr = \"127.0.0.1:9001\"\n  [upstreams.health.active]\n  path = \"healthz\"",
        ))
        .unwrap();
        let err = validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("health.active.path"),
            "unexpected error: {err}"
        );
    }
}
