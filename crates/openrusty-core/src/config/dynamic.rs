//! `[dynamic]` config section: the single-module execution API
//! (`POST /api/v1/dynamic/<name>`). Extracted from `config.rs` to keep
//! that file under the size cap; `DynamicConfig` stays reachable as
//! `openrusty_core::config::DynamicConfig`.

use super::ConfigError;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

use super::FailPolicy;

/// One dynamic WASM module served by `POST /api/v1/dynamic/<name>`.
/// Absent `[dynamic]` section = feature fully disabled (no routes).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DynamicConfig {
    /// Directory of `<name>.wasm` modules; served as-is, resolved per
    /// request (stat-driven cache: replacing a file takes effect on the
    /// next request, no reload). Default overridable via the
    /// `OPENRUSTY_DYNAMIC_DIR` environment variable.
    pub dir: String,
    /// Hard timeout for one plugin phase invocation (default 50 ms).
    #[serde(default = "default_dynamic_timeout_ms")]
    pub timeout_ms: u64,
    /// Per-request memory ceiling (MiB, default 16).
    #[serde(default = "default_dynamic_memory_mb")]
    pub max_memory_mb: u32,
    #[serde(default)]
    pub on_failure: FailPolicy,
    /// POST body cap for `/api/v1/dynamic/*` (default 1 MiB, 413 above).
    #[serde(default = "default_dynamic_max_body_bytes")]
    pub max_body_bytes: usize,
    /// Free-form per-module settings (`[dynamic.settings.<name>]`),
    /// readable by the module via `cfg_get`.
    #[serde(default)]
    pub settings: BTreeMap<String, HashMap<String, String>>,
}

fn default_dynamic_timeout_ms() -> u64 {
    50
}

fn default_dynamic_memory_mb() -> u32 {
    16
}

fn default_dynamic_max_body_bytes() -> usize {
    1024 * 1024
}

impl Default for DynamicConfig {
    fn default() -> Self {
        Self {
            dir: String::new(),
            timeout_ms: default_dynamic_timeout_ms(),
            max_memory_mb: default_dynamic_memory_mb(),
            on_failure: FailPolicy::default(),
            max_body_bytes: default_dynamic_max_body_bytes(),
            settings: BTreeMap::new(),
        }
    }
}

/// Setting keys follow the module-name rule: first char alphanumeric,
/// then alnum/`.`/`_`/`-` (hand-rolled; no regex dependency).
fn valid_setting_key(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Pure validation of the `[dynamic]` invariants the deserializer cannot
/// express; called by [`super::validate`] when the section is present.
pub(crate) fn validate(d: &DynamicConfig) -> Result<(), ConfigError> {
    let bad = |m: &str| ConfigError::Invalid(m.to_string());
    if d.timeout_ms == 0 {
        return Err(bad("dynamic.timeout_ms must be > 0"));
    }
    if d.max_memory_mb == 0 {
        return Err(bad("dynamic.max_memory_mb must be > 0"));
    }
    if d.max_body_bytes == 0 {
        return Err(bad("dynamic.max_body_bytes must be > 0"));
    }
    if d.dir.trim().is_empty() {
        return Err(bad("dynamic.dir must not be empty"));
    }
    for key in d.settings.keys() {
        if !valid_setting_key(key) {
            return Err(bad(&format!(
                "dynamic.settings key {key:?} must match ^[A-Za-z0-9][A-Za-z0-9._-]*$"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load_config, Config};

    /// Minimal good config the `[dynamic]` section anchors against.
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

    fn with_dynamic(extra: &str) -> Config {
        toml::from_str(&GOOD.replace(
            "[[upstreams]]",
            &format!("[dynamic]\n{extra}\n\n[[upstreams]]"),
        ))
        .unwrap()
    }

    /// Absent `[dynamic]` keeps the feature disabled.
    #[test]
    fn section_is_optional() {
        let cfg: Config = toml::from_str(GOOD).unwrap();
        crate::config::validate(&cfg).unwrap();
        assert!(cfg.dynamic.is_none());
    }

    /// Present `[dynamic]` parses with the documented defaults.
    #[test]
    fn parses_with_defaults() {
        let cfg: Config = toml::from_str(&GOOD.replace(
            "[[upstreams]]",
            "[dynamic]\ndir = \"build/dynamic\"\n\n[[upstreams]]",
        ))
        .unwrap();
        crate::config::validate(&cfg).unwrap();
        let d = cfg.dynamic.expect("section present");
        assert_eq!(d.dir, "build/dynamic");
        assert_eq!(d.timeout_ms, 50);
        assert_eq!(d.max_memory_mb, 16);
        assert_eq!(d.max_body_bytes, 1024 * 1024);
        assert_eq!(d.on_failure, FailPolicy::FailOpen);
        assert!(d.settings.is_empty());
    }

    /// `[dynamic.settings.<name>]` parses verbatim and validates.
    #[test]
    fn settings_parse() {
        let cfg: Config = toml::from_str(&GOOD.replace(
            "[[upstreams]]",
            "[dynamic]\ndir = \"build/dynamic\"\n\n[dynamic.settings.\"Mod-1_2.x\"]\nkey = \"value\"\n\n[[upstreams]]",
        ))
        .unwrap();
        crate::config::validate(&cfg).unwrap();
        let d = cfg.dynamic.unwrap();
        assert_eq!(
            d.settings.get("Mod-1_2.x").and_then(|m| m.get("key")),
            Some(&"value".to_string())
        );
    }

    #[test]
    fn rejects_unknown_field() {
        let raw = GOOD.replace(
            "[[upstreams]]",
            "[dynamic]\ndir = \"build/dynamic\"\nbogus = 1\n\n[[upstreams]]",
        );
        // deny_unknown_fields rejects the unknown key at parse time.
        assert!(toml::from_str::<Config>(&raw).is_err());
    }

    #[test]
    fn rejects_zero_scalars() {
        for (field, msg) in [
            ("timeout_ms", "dynamic.timeout_ms"),
            ("max_memory_mb", "dynamic.max_memory_mb"),
            ("max_body_bytes", "dynamic.max_body_bytes"),
        ] {
            let cfg = with_dynamic(&format!("dir = \"build/dynamic\"\n{field} = 0"));
            let err = crate::config::validate(&cfg).unwrap_err();
            assert!(
                err.to_string().contains(msg),
                "unexpected error for {field}: {err}"
            );
        }
    }

    #[test]
    fn rejects_empty_dir() {
        let cfg = with_dynamic("dir = \"  \"");
        let err = crate::config::validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("dynamic.dir"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_bad_settings_key() {
        let cfg: Config = toml::from_str(&GOOD.replace(
            "[[upstreams]]",
            "[dynamic]\ndir = \"build/dynamic\"\n\n[dynamic.settings.\"../evil\"]\nkey = \"v\"\n\n[[upstreams]]",
        ))
        .unwrap();
        let err = crate::config::validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("dynamic.settings"),
            "unexpected error: {err}"
        );
    }

    /// `OPENRUSTY_DYNAMIC_DIR` enables the feature without the section and
    /// overrides `dir` when the section is present.
    #[test]
    fn dir_env_enables_and_overrides() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "openrusty-core-dynenv-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, GOOD).unwrap();
        std::env::set_var("OPENRUSTY_DYNAMIC_DIR", "build/from-env");
        let cfg = load_config(&path).unwrap();
        std::env::remove_var("OPENRUSTY_DYNAMIC_DIR");
        let _ = std::fs::remove_file(&path);
        let d = cfg.dynamic.expect("env enables the feature");
        assert_eq!(d.dir, "build/from-env");
        assert_eq!(d.timeout_ms, 50);

        // With the section present, the env value wins over the file.
        let with_section = GOOD.replace(
            "[[upstreams]]",
            "[dynamic]\ndir = \"build/on-disk\"\n\n[[upstreams]]",
        );
        std::fs::write(&path, with_section).unwrap();
        std::env::set_var("OPENRUSTY_DYNAMIC_DIR", "build/from-env");
        let cfg = load_config(&path).unwrap();
        std::env::remove_var("OPENRUSTY_DYNAMIC_DIR");
        let _ = std::fs::remove_file(&path);
        assert_eq!(cfg.dynamic.unwrap().dir, "build/from-env");
    }
}
