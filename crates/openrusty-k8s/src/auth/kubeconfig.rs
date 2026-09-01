//! kubeconfig YAML parsing (pure functions).
//!
//! Only the subset of the kubeconfig schema the gateway consumes is
//! modelled; every other field (`preferences`, `extensions`, `namespace`,
//! `insecure-skip-tls-verify`, ...) is *ignored* on purpose: real
//! kubeconfigs are full of fields we do not need, and `deny_unknown_fields`
//! would reject files `kubectl` happily accepts.
//!
//! Parsing ([`parse_kubeconfig`]) is pure and side-effect free; filesystem
//! access (`certificate-authority` paths, `client-certificate` paths) lives
//! in the loading shell in [`super`].

use serde::Deserialize;

use crate::error::{K8sError, Result};

/// Parsed kubeconfig document (subset; unknown fields dropped by serde).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Kubeconfig {
    /// `clusters[]` entries.
    #[serde(default)]
    pub clusters: Vec<NamedCluster>,
    /// `users[]` entries.
    #[serde(default)]
    pub users: Vec<NamedUser>,
    /// `contexts[]` entries.
    #[serde(default)]
    pub contexts: Vec<NamedContext>,
    /// `current-context`; absent unless the file selects one.
    #[serde(rename = "current-context", default)]
    pub current_context: Option<String>,
}

/// A `clusters[]` entry: name plus its `cluster` block.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NamedCluster {
    /// Cluster name referenced by contexts.
    pub name: String,
    /// Connection parameters of the cluster.
    #[serde(default)]
    pub cluster: ClusterData,
}

/// `cluster` block: apiserver URL plus CA (path or inline data).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ClusterData {
    /// Apiserver URL, e.g. `https://10.0.0.1:6443`.
    pub server: String,
    /// Path to a CA file, relative to the kubeconfig location.
    #[serde(rename = "certificate-authority")]
    pub certificate_authority: Option<String>,
    /// Base64-encoded PEM CA bundle.
    #[serde(rename = "certificate-authority-data")]
    pub certificate_authority_data: Option<String>,
}

/// A `users[]` entry: name plus its `user` block.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NamedUser {
    /// User name referenced by contexts.
    pub name: String,
    /// Credential parameters of the user.
    #[serde(default)]
    pub user: UserData,
}

/// `user` block: every credential flavor kubeconfig supports. At most one
/// is used; [`super::credentials_for_user`] documents the precedence.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct UserData {
    /// Static bearer token.
    pub token: Option<String>,
    /// Basic-auth username (parsed, then rejected: unsupported).
    pub username: Option<String>,
    /// Basic-auth password (parsed, then rejected: unsupported).
    pub password: Option<String>,
    /// Path to the client certificate file.
    #[serde(rename = "client-certificate")]
    pub client_certificate: Option<String>,
    /// Path to the client key file.
    #[serde(rename = "client-key")]
    pub client_key: Option<String>,
    /// Base64-encoded client certificate PEM.
    #[serde(rename = "client-certificate-data")]
    pub client_certificate_data: Option<String>,
    /// Base64-encoded client key PEM.
    #[serde(rename = "client-key-data")]
    pub client_key_data: Option<String>,
    /// exec credential plugin declaration.
    #[serde(default)]
    pub exec: Option<super::exec::ExecConfig>,
}

/// A `contexts[]` entry: name plus its `context` block.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NamedContext {
    /// Context name selected by `current-context`.
    pub name: String,
    /// Cluster/user binding of the context.
    #[serde(default)]
    pub context: ContextData,
}

/// `context` block: which cluster and user this context binds.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ContextData {
    /// Referenced `clusters[].name`.
    pub cluster: String,
    /// Referenced `users[].name`.
    pub user: String,
}

/// Parse kubeconfig YAML into [`Kubeconfig`] (pure).
///
/// Only syntax/type errors surface here; dangling references and a missing
/// `current-context` are semantic errors raised by [`resolve`].
pub fn parse_kubeconfig(yaml: &str) -> std::result::Result<Kubeconfig, serde_yaml::Error> {
    serde_yaml::from_str(yaml)
}

/// Pick `(cluster, user)` for a context, `override_context` winning over the
/// document's `current-context` (pure).
///
/// Every failure is a [`K8sError::CredentialMissing`] naming what is absent.
pub fn resolve<'a>(
    config: &'a Kubeconfig,
    override_context: Option<&str>,
) -> Result<(&'a NamedCluster, &'a NamedUser)> {
    let name = override_context
        .map(|name| name.to_string())
        .or_else(|| config.current_context.clone())
        .ok_or_else(|| K8sError::CredentialMissing("current-context is not set".to_string()))?;

    let context = config
        .contexts
        .iter()
        .find(|context| context.name == name)
        .ok_or_else(|| K8sError::CredentialMissing(format!("context `{name}` not found")))?;

    let cluster = config
        .clusters
        .iter()
        .find(|cluster| cluster.name == context.context.cluster)
        .ok_or_else(|| {
            K8sError::CredentialMissing(format!(
                "cluster `{}` referenced by context `{name}` not found",
                context.context.cluster
            ))
        })?;

    let user = config
        .users
        .iter()
        .find(|user| user.name == context.context.user)
        .ok_or_else(|| {
            K8sError::CredentialMissing(format!(
                "user `{}` referenced by context `{name}` not found",
                context.context.user
            ))
        })?;

    Ok((cluster, user))
}

/// Decode a base64 `-data` kubeconfig field into raw PEM bytes (pure).
pub fn decode_data(field: &str, value: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .map_err(|source| K8sError::Base64 {
            field: field.to_string(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN_YAML: &str = r#"
apiVersion: v1
kind: Config
preferences: {}
current-context: ctx-dev
clusters:
  - name: cluster-dev
    cluster:
      server: https://apiserver.example.invalid:6443
      certificate-authority-data: LS0tLS1CRUdJTiBDRVJUSUZJQ0FURS0tLS0tCg==
contexts:
  - name: ctx-dev
    context:
      cluster: cluster-dev
      user: user-dev
users:
  - name: user-dev
    user:
      token: dummy-token-for-tests
"#;

    #[test]
    fn parses_subset_and_ignores_unknown_fields() {
        let config = parse_kubeconfig(TOKEN_YAML).unwrap();
        assert_eq!(config.current_context.as_deref(), Some("ctx-dev"));
        assert_eq!(config.clusters.len(), 1);
        assert_eq!(
            config.clusters[0].cluster.server,
            "https://apiserver.example.invalid:6443"
        );
        assert_eq!(
            config.users[0].user.token.as_deref(),
            Some("dummy-token-for-tests")
        );
    }

    #[test]
    fn resolve_selects_current_and_explicit_context() {
        let config = parse_kubeconfig(TOKEN_YAML).unwrap();
        let (cluster, user) = resolve(&config, None).unwrap();
        assert_eq!(cluster.name, "cluster-dev");
        assert_eq!(user.name, "user-dev");

        let err = resolve(&config, Some("nope")).unwrap_err();
        assert!(err.to_string().contains("`nope`"), "{err}");
    }

    #[test]
    fn resolve_without_current_context_errors() {
        let config = parse_kubeconfig("clusters: []\nusers: []\ncontexts: []\n").unwrap();
        let err = resolve(&config, None).unwrap_err();
        assert!(err.to_string().contains("current-context"), "{err}");
    }

    #[test]
    fn resolve_dangling_references_errors() {
        let config = parse_kubeconfig(
            r#"
current-context: c
clusters: []
users: []
contexts:
  - name: c
    context:
      cluster: ghost
      user: ghost-user
"#,
        )
        .unwrap();
        assert!(resolve(&config, None)
            .unwrap_err()
            .to_string()
            .contains("cluster `ghost`"));
    }

    #[test]
    fn decode_data_roundtrip_and_error() {
        let decoded = decode_data("certificate-authority-data", "Zm9vCg==").unwrap();
        assert_eq!(decoded, b"foo\n");
        let err = decode_data("certificate-authority-data", "!!!").unwrap_err();
        assert!(
            err.to_string().contains("certificate-authority-data"),
            "{err}"
        );
    }

    #[test]
    fn rejects_yaml_type_mismatches() {
        assert!(parse_kubeconfig("clusters: 42").is_err());
        assert!(parse_kubeconfig("clusters: [oops").is_err());
    }
}
