//! `[dynamic]` config section: the single-module execution API
//! (`POST /api/v1/dynamic/<name>`). Extracted from `config.rs` to keep
//! that file under the size cap; `DynamicConfig` stays reachable as
//! `openrusty_core::config::DynamicConfig`.

use super::ConfigError;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

use super::FailPolicy;

/// One `{method, path} -> module` binding served by the dynamic
/// execution API: requests whose method and path match run `<module>.wasm`
/// through the dynamic pipeline (intercepted before the proxy routes).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DynamicRoute {
    /// HTTP method (`"GET"`, `"POST"`, ...); trimmed and ASCII-uppercased
    /// before matching.
    pub method: String,
    /// Request path starting with `/`; a trailing `/*` marks a prefix
    /// binding (`/api/*` matches `/api` and everything below it).
    pub path: String,
    /// Dynamic module name served on match
    /// (`^[A-Za-z0-9][A-Za-z0-9._-]*$`).
    pub module: String,
}

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
    /// `{method, path} -> module` bindings (`[[dynamic.routes]]`)
    /// intercepted by the gateway fallback before the proxy routes.
    /// Reconciled on reload; runtime bindings added through the
    /// registration face survive (they are not config-owned).
    #[serde(default)]
    pub routes: Vec<DynamicRoute>,
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
            routes: Vec::new(),
        }
    }
}

/// Module-name rule shared by `[dynamic.routes].module`,
/// `[dynamic.settings]` keys and the registration endpoint:
/// first char alphanumeric, then alnum/`.`/`_`/`-` (hand-rolled; no
/// regex dependency).
pub fn valid_module_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Setting keys follow the module-name rule.
fn valid_setting_key(name: &str) -> bool {
    valid_module_name(name)
}

/// Longest accepted route path (bindings live in memory and are copied
/// on every mutation, so they must stay small by construction).
const MAX_ROUTE_PATH_LEN: usize = 1024;

/// Normalize a route method: trim + ASCII-uppercase; `None` when empty,
/// longer than 32 bytes, or not an HTTP token (`tchar`).
pub fn normalize_method(method: &str) -> Option<String> {
    let m = method.trim();
    if m.is_empty() || m.len() > 32 || !m.bytes().all(is_tchar) {
        return None;
    }
    Some(m.to_ascii_uppercase())
}

/// One `tchar` byte (RFC 7230 token character).
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Normalize a route path: kept verbatim when it starts with `/`, is at
/// most [`MAX_ROUTE_PATH_LEN`] bytes and holds no control char, `?` or
/// `#` (those belong to the query/fragment, never to a matching path).
/// A trailing `/*` stays: it is the prefix-match marker.
pub fn normalize_route_path(path: &str) -> Option<String> {
    if !path.starts_with('/') || path.len() > MAX_ROUTE_PATH_LEN {
        return None;
    }
    if path
        .bytes()
        .any(|b| b.is_ascii_control() || matches!(b, b'?' | b'#'))
    {
        return None;
    }
    Some(path.to_string())
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
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for r in &d.routes {
        let Some(method) = normalize_method(&r.method) else {
            return Err(bad(&format!(
                "dynamic.routes method {:?} is not a valid HTTP method",
                r.method
            )));
        };
        if normalize_route_path(&r.path).is_none() {
            return Err(bad(&format!(
                "dynamic.routes path {:?} must start with '/' (trailing '/*' = prefix match)",
                r.path
            )));
        }
        if !valid_module_name(&r.module) {
            return Err(bad(&format!(
                "dynamic.routes module {:?} must match ^[A-Za-z0-9][A-Za-z0-9._-]*$",
                r.module
            )));
        }
        // Dedupe on the base key: `/api` and `/api/*` name the same
        // binding slot (one shape replaces the other).
        let base = r.path.strip_suffix("/*").unwrap_or(&r.path);
        if !seen.insert((method, base.to_string())) {
            return Err(bad(&format!(
                "dynamic.routes duplicate binding {} {}",
                r.method, r.path
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
    /// `[[dynamic.routes]]` parses with method/path/module and defaults
    /// to an empty list without the key.
    #[test]
    fn routes_parse_and_default() {
        let cfg = with_dynamic(
            r#"dir = "build/dynamic"

[[dynamic.routes]]
method = "get"
path = "/api/orders"
module = "orders"

[[dynamic.routes]]
method = "POST"
path = "/api/items/*"
module = "items""#,
        );
        let d = cfg.dynamic.expect("section present");
        assert_eq!(d.routes.len(), 2);
        assert_eq!(d.routes[0].method, "get");
        assert_eq!(d.routes[0].path, "/api/orders");
        assert_eq!(d.routes[0].module, "orders");
        assert_eq!(d.routes[1].path, "/api/items/*");

        let bare = with_dynamic(r#"dir = "build/dynamic""#);
        assert!(bare.dynamic.unwrap().routes.is_empty());
    }

    #[test]
    fn rejects_bad_route_fields() {
        for (extra, needle) in [
            (
                r#"dir = "d"

[[dynamic.routes]]
method = ""
path = "/api"
module = "m""#,
                "not a valid HTTP method",
            ),
            (
                r#"dir = "d"

[[dynamic.routes]]
method = "GET"
path = "api"
module = "m""#,
                "must start with '/'",
            ),
            (
                r#"dir = "d"

[[dynamic.routes]]
method = "GET"
path = "/api?x"
module = "m""#,
                "must start with '/'",
            ),
            (
                r#"dir = "d"

[[dynamic.routes]]
method = "GET"
path = "/api"
module = "../evil""#,
                "must match",
            ),
        ] {
            let cfg = with_dynamic(extra);
            let err = crate::config::validate(&cfg).unwrap_err();
            assert!(err.to_string().contains(needle), "unexpected error: {err}");
        }
    }

    /// Two bindings for the same normalized `(method, path)` collide,
    /// even when the methods differ only by case.
    #[test]
    fn rejects_duplicate_bindings() {
        let cfg = with_dynamic(
            r#"dir = "d"

[[dynamic.routes]]
method = "GET"
path = "/api"
module = "a"

[[dynamic.routes]]
method = "get"
path = "/api"
module = "b""#,
        );
        let err = crate::config::validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("duplicate binding"),
            "unexpected error: {err}"
        );
    }

    /// Normalization helpers: methods trim+uppercase, `?`/`#`/control
    /// chars and a missing leading `/` reject, `/*` stays verbatim.
    #[test]
    fn route_normalization_helpers() {
        assert_eq!(normalize_method(" get "), Some("GET".to_string()));
        assert_eq!(normalize_method("PATCH"), Some("PATCH".to_string()));
        assert_eq!(normalize_method(""), None);
        assert_eq!(normalize_method("BAD METHOD"), None);
        assert_eq!(normalize_method("BAD\u{b5}"), None);
        assert_eq!(normalize_route_path("/api/*"), Some("/api/*".to_string()));
        assert_eq!(normalize_route_path("api"), None);
        assert_eq!(normalize_route_path("/a?b"), None);
        assert_eq!(normalize_route_path("/a#b"), None);
        assert_eq!(normalize_route_path("/a\tb"), None);
        assert_eq!(normalize_route_path(&"/a".repeat(1025)), None);
    }
}
