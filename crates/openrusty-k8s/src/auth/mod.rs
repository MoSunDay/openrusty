//! Credential loading: kubeconfig, in-cluster, exec plugins.
//!
//! # Load order (deliberate, do not reshuffle silently)
//!
//! [`load`] resolves a `(Cluster, Credentials)` pair from the first source
//! that applies:
//!
//! 1. the explicit path argument, if given (missing file is an error);
//! 2. `$KUBECONFIG` -- a `:`-separated path list; the first entry whose
//!    file exists wins. If the variable is set but no listed file exists,
//!    this is an error (never silently fall through: a stale `$KUBECONFIG`
//!    falling back to node credentials would target the wrong cluster);
//! 3. `$HOME/.kube/config`, if it exists;
//! 4. the in-cluster service account (`KUBERNETES_SERVICE_HOST`/`PORT` +
//!    the mounted token/CA);
//! 5. otherwise: an error.
//!
//! Environment access is confined to the thinnest shells ([`load`],
//! [`in_cluster::load`]); everything below them (YAML parsing, context
//! resolution, base64 decoding, endpoint composition, candidate selection)
//! is pure and unit-tested without touching the environment.
//!
//! # Credential normalization
//!
//! A kubeconfig user block may declare several credential flavors; at most
//! one is used. Precedence (a deliberate simplification -- real clusters
//! do not mix flavors within one user):
//!
//! 1. static `token` -> [`Credentials::Bearer`];
//! 2. client certificate (`*-data` beats file paths) ->
//!    [`Credentials::ClientCert`];
//! 3. `exec` plugin -> [`Credentials::ExecToken`] (executed lazily;
//!    see [`exec`]).
//!
//! Basic-auth-only users are parsed but rejected: the apiserver removed
//! basic auth and the gateway will not send passwords.

pub mod exec;
pub mod in_cluster;
pub mod kubeconfig;

pub use self::exec::ExecConfig;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use hyper::Uri;

use crate::error::{K8sError, Result};

use self::kubeconfig::{decode_data, parse_kubeconfig, resolve, NamedUser};

/// Endpoint plus trust anchor of one apiserver.
#[derive(Debug, Clone)]
pub struct Cluster {
    /// Apiserver URL, e.g. `https://10.96.0.1:443`.
    pub endpoint: Uri,
    /// PEM-encoded CA bundle used to verify the apiserver certificate.
    pub ca: Vec<u8>,
}

/// Normalized credential handed to the HTTP client.
#[derive(Debug, Clone, PartialEq)]
pub enum Credentials {
    /// Static or exec-derived bearer token (`Authorization: Bearer ...`).
    Bearer(String),
    /// TLS client certificate; identity lives in the TLS handshake.
    ClientCert {
        /// PEM-encoded certificate chain.
        cert: Vec<u8>,
        /// PEM-encoded private key.
        key: Vec<u8>,
    },
    /// Deferred exec credential plugin; executed when the client resolves
    /// its token (see [`exec::run`]).
    ExecToken(ExecConfig),
}

/// Default kubeconfig location under the user home directory.
fn home_kubeconfig(home: &str) -> PathBuf {
    Path::new(home).join(".kube").join("config")
}

/// Ordered kubeconfig candidates for one resolution pass (pure).
///
/// Returns the candidate group for the highest-priority source only, so a
/// stale `$KUBECONFIG` cannot leak into a home-dir fallback: an explicit
/// path yields exactly that path, a set `$KUBECONFIG` yields its entries,
/// otherwise the home config (possibly empty -> caller falls back to
/// in-cluster).
fn kubeconfig_candidates(
    explicit: Option<&Path>,
    kubeconfig_env: Option<&str>,
    home: Option<&str>,
) -> Vec<PathBuf> {
    if let Some(path) = explicit {
        return vec![path.to_path_buf()];
    }
    if let Some(list) = kubeconfig_env.filter(|list| !list.trim().is_empty()) {
        return list
            .split(':')
            .filter(|entry| !entry.is_empty())
            .map(PathBuf::from)
            .collect();
    }
    home.map(home_kubeconfig).into_iter().collect()
}

/// Resolve `(Cluster, Credentials)` per the documented load order.
///
/// `explicit_path` is the override hook the server will wire to its config
/// in a later milestone.
pub fn load(explicit_path: Option<&Path>) -> Result<(Cluster, Credentials)> {
    let candidates = kubeconfig_candidates(
        explicit_path,
        env::var("KUBECONFIG").ok().as_deref(),
        env::var("HOME").ok().as_deref(),
    );

    if candidates.is_empty() {
        return in_cluster::load();
    }
    for candidate in &candidates {
        if candidate.exists() {
            return load_kubeconfig_file(candidate);
        }
    }
    Err(K8sError::CredentialMissing(format!(
        "no kubeconfig found at any of: {}",
        candidates
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Load `(Cluster, Credentials)` from one kubeconfig file.
///
/// Relative `certificate-authority` / `client-certificate` / `client-key`
/// paths are resolved against the kubeconfig's own directory, matching
/// `kubectl` semantics.
pub fn load_kubeconfig_file(path: &Path) -> Result<(Cluster, Credentials)> {
    let text = fs::read_to_string(path)?;
    let config = parse_kubeconfig(&text).map_err(|source| K8sError::KubeconfigParse {
        path: path.display().to_string(),
        source,
    })?;
    let (named_cluster, named_user) = resolve(&config, None)?;

    let cluster = load_cluster(&named_cluster.cluster.server, &named_cluster.cluster, path)?;
    let credentials = credentials_for_user(named_user, path)?;
    Ok((cluster, credentials))
}

/// Build the [`Cluster`] from a `cluster` block: endpoint URL plus CA
/// (inline `-data` wins over the file path; absence is an error because
/// the gateway never disables certificate verification).
fn load_cluster(
    server: &str,
    cluster: &kubeconfig::ClusterData,
    kubeconfig_path: &Path,
) -> Result<Cluster> {
    let endpoint = parse_endpoint(server)?;
    let ca = if let Some(data) = &cluster.certificate_authority_data {
        decode_data("certificate-authority-data", data)?
    } else if let Some(ca_path) = &cluster.certificate_authority {
        let resolved = resolve_relative(kubeconfig_path.parent(), ca_path);
        fs::read(&resolved).map_err(|e| {
            K8sError::CredentialMissing(format!("CA file {}: {e}", resolved.display()))
        })?
    } else {
        return Err(K8sError::CredentialMissing(
            "cluster has no certificate-authority(-data); insecure mode is not supported"
                .to_string(),
        ));
    };
    Ok(Cluster { endpoint, ca })
}

/// Normalize one `user` block into [`Credentials`]; see the module docs
/// for the precedence. `kubeconfig_path` anchors relative cert paths.
fn credentials_for_user(user: &NamedUser, kubeconfig_path: &Path) -> Result<Credentials> {
    let data = &user.user;
    if let Some(token) = &data.token {
        if token.trim().is_empty() {
            return Err(K8sError::CredentialInvalid(format!(
                "user `{}`: token is empty",
                user.name
            )));
        }
        return Ok(Credentials::Bearer(token.trim().to_string()));
    }
    if data.client_certificate.is_some() || data.client_certificate_data.is_some() {
        return client_cert_credentials(user, kubeconfig_path);
    }
    if let Some(exec_config) = &data.exec {
        return Ok(Credentials::ExecToken(exec_config.clone()));
    }
    if data.username.is_some() || data.password.is_some() {
        return Err(K8sError::CredentialInvalid(format!(
            "user `{}`: basic-auth credentials are not supported; use a token, \
             client certificate or exec plugin",
            user.name
        )));
    }
    Err(K8sError::CredentialMissing(format!(
        "user `{}` carries no usable credential",
        user.name
    )))
}

/// Decode client-certificate material (`-data` fields first, then file
/// paths relative to the kubeconfig).
fn client_cert_credentials(user: &NamedUser, kubeconfig_path: &Path) -> Result<Credentials> {
    let data = &user.user;
    let cert = match &data.client_certificate_data {
        Some(encoded) => decode_data("client-certificate-data", encoded)?,
        None => match &data.client_certificate {
            Some(path) => {
                let resolved = resolve_relative(kubeconfig_path.parent(), path);
                fs::read(&resolved).map_err(|e| {
                    K8sError::CredentialMissing(format!(
                        "client certificate {}: {e}",
                        resolved.display()
                    ))
                })?
            }
            None => {
                return Err(K8sError::CredentialMissing(format!(
                    "user `{}`: client-key given without a certificate",
                    user.name
                )))
            }
        },
    };
    let key = match &data.client_key_data {
        Some(encoded) => decode_data("client-key-data", encoded)?,
        None => match &data.client_key {
            Some(path) => {
                let resolved = resolve_relative(kubeconfig_path.parent(), path);
                fs::read(&resolved).map_err(|e| {
                    K8sError::CredentialMissing(format!("client key {}: {e}", resolved.display()))
                })?
            }
            None => {
                return Err(K8sError::CredentialMissing(format!(
                    "user `{}`: client certificate given without a key",
                    user.name
                )))
            }
        },
    };
    Ok(Credentials::ClientCert { cert, key })
}

/// Resolve a possibly relative kubeconfig path against the file's dir.
fn resolve_relative(base: Option<&Path>, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.unwrap_or_else(|| Path::new(".")).join(path)
    }
}

/// Parse and validate an apiserver URL; only http(s) absolute URLs pass.
fn parse_endpoint(server: &str) -> Result<Uri> {
    let uri =
        Uri::from_str(server.trim()).map_err(|_| K8sError::Endpoint(server.trim().to_string()))?;
    match uri.scheme_str() {
        Some("https") | Some("http") if uri.host().is_some() => Ok(uri),
        _ => Err(K8sError::Endpoint(server.trim().to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::exec::ExecEnvVar;
    use crate::auth::kubeconfig::UserData;

    #[test]
    fn candidates_explicit_wins_over_env_and_home() {
        let explicit = Path::new("/tmp/explicit.yaml");
        assert_eq!(
            kubeconfig_candidates(Some(explicit), Some("/a:/b"), Some("/home/u")),
            vec![PathBuf::from("/tmp/explicit.yaml")]
        );
    }

    #[test]
    fn candidates_env_list_keeps_only_existing_grouping() {
        let candidates = kubeconfig_candidates(None, Some("/a.yaml:/b.yaml:"), None);
        assert_eq!(
            candidates,
            vec![PathBuf::from("/a.yaml"), PathBuf::from("/b.yaml")]
        );
        // Empty / whitespace-only KUBECONFIG falls back to home.
        assert_eq!(
            kubeconfig_candidates(None, Some("   "), Some("/home/u")),
            vec![home_kubeconfig("/home/u")]
        );
    }

    #[test]
    fn candidates_home_group_when_no_env() {
        assert_eq!(
            kubeconfig_candidates(None, None, Some("/home/u")),
            vec![PathBuf::from("/home/u/.kube/config")]
        );
        assert!(kubeconfig_candidates(None, None, None).is_empty());
    }

    #[test]
    fn credentials_precedence_token_over_cert_over_exec() {
        let base = UserData {
            token: Some("tok".to_string()),
            ..UserData::default()
        };
        let user = NamedUser {
            name: "u".to_string(),
            user: base,
        };
        assert_eq!(
            credentials_for_user(&user, Path::new("/k")).unwrap(),
            Credentials::Bearer("tok".to_string())
        );

        let exec_user = NamedUser {
            name: "u".to_string(),
            user: UserData {
                exec: Some(ExecConfig {
                    command: "echo".to_string(),
                    args: vec![],
                    env: vec![ExecEnvVar {
                        name: "K".to_string(),
                        value: "V".to_string(),
                    }],
                    api_version: None,
                }),
                ..UserData::default()
            },
        };
        let resolved = credentials_for_user(&exec_user, Path::new("/k")).unwrap();
        assert!(matches!(resolved, Credentials::ExecToken(_)));
    }

    #[test]
    fn basic_auth_only_user_rejected() {
        let user = NamedUser {
            name: "legacy".to_string(),
            user: UserData {
                username: Some("admin".to_string()),
                password: Some("hunter2".to_string()),
                ..UserData::default()
            },
        };
        let err = credentials_for_user(&user, Path::new("/k")).unwrap_err();
        assert!(err.to_string().contains("basic-auth"), "{err}");
    }

    #[test]
    fn credentialless_user_rejected() {
        let user = NamedUser {
            name: "empty".to_string(),
            user: UserData::default(),
        };
        let err = credentials_for_user(&user, Path::new("/k")).unwrap_err();
        assert!(err.to_string().contains("no usable credential"), "{err}");
    }

    #[test]
    fn endpoint_requires_absolute_http_s() {
        assert!(parse_endpoint("https://10.0.0.1:6443").is_ok());
        assert!(parse_endpoint("http://localhost:8080").is_ok());
        assert!(parse_endpoint("ftp://10.0.0.1").is_err());
        assert!(parse_endpoint("apiserver.example.invalid:6443").is_err());
    }

    #[test]
    fn resolve_relative_prefers_absolute_paths() {
        assert_eq!(
            resolve_relative(Some(Path::new("/cfg")), "/abs/ca.crt"),
            PathBuf::from("/abs/ca.crt")
        );
        assert_eq!(
            resolve_relative(Some(Path::new("/cfg")), "ca.crt"),
            PathBuf::from("/cfg/ca.crt")
        );
    }
}
