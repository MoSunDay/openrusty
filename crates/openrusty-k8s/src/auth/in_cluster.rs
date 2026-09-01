//! In-cluster service-account credentials.
//!
//! Inside a pod, kubelet mounts a service account under
//! `/var/run/secrets/kubernetes.io/serviceaccount` (`token`, `ca.crt`) and
//! injects `KUBERNETES_SERVICE_HOST`/`KUBERNETES_SERVICE_PORT`. This module
//! turns that into a `(Cluster, Credentials)` pair.
//!
//! Testing without a cluster: [`load_with_root`] takes the mount root and
//! the host/port as plain parameters, so tests point it at a tempdir and
//! fake values; only [`load`] itself touches the real environment, and it
//! is a three-line shell around the testable pieces.

use std::env;
use std::fs;
use std::path::Path;
use std::str::FromStr;

use hyper::Uri;

use crate::error::{K8sError, Result};

use super::{Cluster, Credentials};

/// Default service-account mount point inside a pod.
pub const SERVICE_ACCOUNT_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

/// Environment variable holding the apiserver host.
pub const HOST_ENV: &str = "KUBERNETES_SERVICE_HOST";
/// Environment variable holding the apiserver port.
pub const PORT_ENV: &str = "KUBERNETES_SERVICE_PORT";

/// Build the in-cluster apiserver endpoint from host and port (pure).
///
/// IPv6 hosts are bracketed so `https://[fd00::1]:443` stays a valid URL.
pub fn endpoint(host: &str, port: &str) -> Result<Uri> {
    if host.is_empty() || port.is_empty() {
        return Err(K8sError::CredentialMissing(format!(
            "{HOST_ENV}/{PORT_ENV} not set: not running in a cluster"
        )));
    }
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let url = format!("https://{host}:{port}");
    Uri::from_str(&url).map_err(|_| K8sError::Endpoint(url.clone()))
}

/// Load service-account credentials from an injected mount root (pure
/// apart from the reads under `root`, which tests redirect to a tempdir).
pub fn load_with_root(root: &Path, host: &str, port: &str) -> Result<(Cluster, Credentials)> {
    let token_path = root.join("token");
    let token = fs::read_to_string(&token_path).map_err(|e| {
        K8sError::CredentialMissing(format!("in-cluster token at {}: {e}", token_path.display()))
    })?;

    let ca_path = root.join("ca.crt");
    let ca = fs::read(&ca_path).map_err(|e| {
        K8sError::CredentialMissing(format!("in-cluster CA at {}: {e}", ca_path.display()))
    })?;

    Ok((
        Cluster {
            endpoint: endpoint(host, port)?,
            ca,
        },
        Credentials::Bearer(token.trim().to_string()),
    ))
}

/// Load in-cluster credentials from the real environment and the default
/// mount point. Thin shell: all logic lives in [`endpoint`] and
/// [`load_with_root`].
pub fn load() -> Result<(Cluster, Credentials)> {
    let host = env::var(HOST_ENV).unwrap_or_default();
    let port = env::var(PORT_ENV).unwrap_or_default();
    load_with_root(Path::new(SERVICE_ACCOUNT_DIR), &host, &port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_composes_https_url() {
        let uri = endpoint("10.96.0.1", "443").unwrap();
        assert_eq!(uri.to_string(), "https://10.96.0.1:443/");
        let uri = endpoint("fd00::1", "6443").unwrap();
        assert_eq!(uri.to_string(), "https://[fd00::1]:6443/");
    }

    #[test]
    fn endpoint_rejects_missing_env_values() {
        let err = endpoint("", "443").unwrap_err();
        assert!(err.to_string().contains("KUBERNETES_SERVICE_HOST"), "{err}");
        assert!(endpoint("10.0.0.1", "").is_err());
    }

    #[test]
    fn load_with_root_reads_trimmed_token_and_ca() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("token"), "dummy-in-cluster-token\n").unwrap();
        std::fs::write(dir.path().join("ca.crt"), b"dummy-ca-bytes").unwrap();

        let (cluster, credentials) = load_with_root(dir.path(), "10.96.0.1", "443").unwrap();
        assert_eq!(cluster.endpoint.to_string(), "https://10.96.0.1:443/");
        assert_eq!(cluster.ca, b"dummy-ca-bytes");
        assert_eq!(
            credentials,
            Credentials::Bearer("dummy-in-cluster-token".to_string())
        );
    }

    #[test]
    fn load_with_root_missing_token_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_with_root(dir.path(), "10.96.0.1", "443").unwrap_err();
        assert!(err.to_string().contains("token"), "{err}");
    }
}
