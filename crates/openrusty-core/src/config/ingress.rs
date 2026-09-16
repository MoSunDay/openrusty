//! The `[ingress]` configuration section: Kubernetes ingress adoption.
//!
//! Split out of `config.rs` to keep both files small; the parent module
//! re-exports [`IngressConfig`], so `openrusty_core::config::*` paths are
//! unchanged.
//!
//! Everything defaults to OFF: a config without an `[ingress]` section
//! keeps the gateway a purely static proxy (zero behavioral change for
//! existing deployments). Note the deliberate absence of credential
//! material here - `kubeconfig` is a *path* only; tokens, client keys and
//! CA bundles always come from the referenced file or the in-cluster
//! service account, never from the gateway config.

use serde::Deserialize;

use super::ConfigError;

/// Ingress watch settings (`[ingress]` in the TOML config).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressConfig {
    /// Master switch. `false` (default) spawns no watch task at all.
    #[serde(default)]
    pub enabled: bool,
    /// Only Ingresses whose `spec.ingressClassName` equals this value are
    /// adopted; an absent class never matches (the gateway never silently
    /// picks up Ingresses meant for another controller).
    #[serde(default = "default_ingress_class")]
    pub ingress_class: String,
    /// Explicit kubeconfig path. Empty (default) = the standard load order:
    /// this path, then `$KUBECONFIG`, then `~/.kube/config`, then the
    /// in-cluster service account.
    #[serde(default)]
    pub kubeconfig: String,
    /// Namespaces to watch. Empty (default) = cluster-wide list/watch
    /// (still bounded by the credentials' RBAC); non-empty = one
    /// namespaced list/watch per entry.
    #[serde(default)]
    pub namespaces: Vec<String>,
}

fn default_ingress_class() -> String {
    "openrusty".to_string()
}

impl Default for IngressConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ingress_class: default_ingress_class(),
            kubeconfig: String::new(),
            namespaces: Vec::new(),
        }
    }
}

/// Pure validation of the `[ingress]` section: cross-field invariants the
/// deserializer cannot express. An all-default section is always valid
/// (disabled), so existing configs keep loading.
pub fn validate(cfg: &IngressConfig) -> Result<(), ConfigError> {
    let bad = |m: String| ConfigError::Invalid(m);
    if cfg.ingress_class.trim().is_empty() {
        return Err(bad(
            "ingress.ingress_class must not be empty (ingresses are matched by class)".to_string(),
        ));
    }
    for (i, ns) in cfg.namespaces.iter().enumerate() {
        if ns.trim().is_empty() {
            return Err(bad(format!("ingress.namespaces[{i}] must not be empty")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const MINIMAL: &str = r#"
[server]
listen = "127.0.0.1:8080"
"#;

    #[test]
    fn section_is_optional_and_all_off() {
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        assert!(!cfg.ingress.enabled);
        assert_eq!(cfg.ingress.ingress_class, "openrusty");
        assert_eq!(cfg.ingress.kubeconfig, "");
        assert!(cfg.ingress.namespaces.is_empty());
        assert_eq!(cfg.ingress, IngressConfig::default());
        validate(&cfg.ingress).unwrap();
    }

    #[test]
    fn explicit_section_parses() {
        let cfg: Config = toml::from_str(&format!(
            "{MINIMAL}\n[ingress]\nenabled = true\ningress_class = \"other\"\nkubeconfig = \"/etc/kube/config\"\nnamespaces = [\"web\", \"api\"]"
        ))
        .unwrap();
        assert!(cfg.ingress.enabled);
        assert_eq!(cfg.ingress.ingress_class, "other");
        assert_eq!(cfg.ingress.kubeconfig, "/etc/kube/config");
        assert_eq!(
            cfg.ingress.namespaces,
            vec!["web".to_string(), "api".to_string()]
        );
        validate(&cfg.ingress).unwrap();
    }

    #[test]
    fn rejects_empty_class_and_namespace_entries() {
        let cfg: Config =
            toml::from_str(&format!("{MINIMAL}\n[ingress]\ningress_class = \"  \"")).unwrap();
        let err = validate(&cfg.ingress).unwrap_err();
        assert!(err.to_string().contains("ingress_class"), "got: {err}");

        let cfg: Config = toml::from_str(&format!(
            "{MINIMAL}\n[ingress]\nnamespaces = [\"web\", \"\"]"
        ))
        .unwrap();
        let err = validate(&cfg.ingress).unwrap_err();
        assert!(err.to_string().contains("namespaces[1]"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_fields() {
        let src = format!("{MINIMAL}\n[ingress]\nenabled = true\nsecrets_dir = \"/x\"");
        let err = toml::from_str::<Config>(&src).unwrap_err();
        assert!(err.to_string().contains("secrets_dir"), "got: {err}");
    }
}
