//! Error type shared by the whole [`openrusty-k8s`](crate) crate.
//!
//! Variants mirror the failure modes of the two jobs this crate does today:
//! loading a `Cluster`/`Credentials` pair (kubeconfig, in-cluster,
//! exec plugin) and talking HTTPS to the apiserver. Every variant names the
//! offending artifact so operators can act on the message alone.

use thiserror::Error;

/// Errors produced while loading credentials or talking to the apiserver.
#[derive(Debug, Error)]
pub enum K8sError {
    /// Filesystem failure (unreadable kubeconfig, missing token file, ...).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The kubeconfig file is not valid YAML or its fields have the wrong
    /// shape. `path` names the file (or `<memory>` for inline parses).
    #[error("failed to parse kubeconfig {path}: {source}")]
    KubeconfigParse {
        /// File the broken YAML came from.
        path: String,
        /// Underlying YAML error.
        #[source]
        source: serde_yaml::Error,
    },

    /// A required credential or reference is absent: no current-context,
    /// dangling cluster/user reference, missing CA, no in-cluster token...
    #[error("missing credential: {0}")]
    CredentialMissing(String),

    /// Credentials are present but not usable: unsupported basic-auth-only
    /// users, empty tokens, unparsable endpoint URLs.
    #[error("invalid credential: {0}")]
    CredentialInvalid(String),

    /// base64 decoding of a `-data` suffixed kubeconfig field failed.
    #[error("base64 decode error in kubeconfig field `{field}`: {source}")]
    Base64 {
        /// Name of the kubeconfig field that failed to decode.
        field: String,
        /// Underlying decoder error.
        #[source]
        source: base64::DecodeError,
    },

    /// The exec credential plugin produced no usable output: it exited
    /// non-zero, wrote non-JSON to stdout, or returned no token.
    #[error("exec command `{command}` failed: {message}")]
    Exec {
        /// The plugin command as configured in the kubeconfig.
        command: String,
        /// What exactly went wrong (exit status, stderr, parse failure...).
        message: String,
    },

    /// Building the rustls client config failed: unparsable CA PEM,
    /// unparsable client key, no usable TLS protocol version.
    #[error("failed to build TLS configuration: {0}")]
    Tls(String),

    /// The configured cluster endpoint is not a usable absolute URL.
    #[error("invalid cluster endpoint `{0}`")]
    Endpoint(String),

    /// The HTTP exchange with the apiserver failed (connect error,
    /// reset connection, hyper-level protocol failure).
    #[error("http request failed: {0}")]
    Http(#[from] hyper_util::client::legacy::Error),

    /// Assembling an HTTP request failed (bad URI/header values).
    #[error("http request build failed: {0}")]
    RequestBuild(#[from] hyper::http::Error),
}

/// Crate-wide result alias over [`K8sError`].
pub type Result<T> = std::result::Result<T, K8sError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_variant_reports_field_name() {
        let err = K8sError::Base64 {
            field: "certificate-authority-data".to_string(),
            source: base64::DecodeError::InvalidByte(0, b'!'),
        };
        let msg = err.to_string();
        assert!(msg.contains("certificate-authority-data"), "{msg}");
        assert!(msg.contains("base64"), "{msg}");
    }

    #[test]
    fn missing_credential_names_the_gap() {
        let err = K8sError::CredentialMissing("current-context is not set".to_string());
        assert_eq!(
            err.to_string(),
            "missing credential: current-context is not set"
        );
    }
}
