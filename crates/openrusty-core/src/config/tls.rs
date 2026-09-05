//! Upstream (outbound) TLS settings: the `[upstreams.tls]` section.
//!
//! Split out of `config.rs` to keep both files small; the parent module
//! re-exports [`UpstreamTlsConfig`], so the historical
//! `openrusty_core::config::` paths stay flat.
//!
//! The struct is pure data; the certificate files themselves are read by
//! `openrusty-proxy::tls::build` at runtime-apply time (boot or reload),
//! so a rotated cert file takes effect on the next reload without a
//! config change. Validation here only covers the cross-field invariants
//! the deserializer cannot express.

use serde::Deserialize;
use std::path::PathBuf;

use super::ConfigError;

/// Outbound TLS policy for one upstream (`[upstreams.tls]` in TOML).
///
/// Absent section = plaintext HTTP upstream (the historical behaviour);
/// present section = every peer connection is upgraded to TLS with these
/// settings, mirroring nginx `proxy_pass https://` + `proxy_ssl_*`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamTlsConfig {
    /// SNI name sent in the ClientHello and the DNS name verified in the
    /// server certificate. Independent of the dial address (`peers`), so
    /// an upstream can be reached by IP while presenting a named cert.
    pub server_name: String,
    /// PEM file with the trust anchor(s) for server certificate
    /// verification. Required unless `insecure_skip_verify` is set.
    pub ca_cert: Option<PathBuf>,
    /// Client certificate (mTLS). Must be paired with `client_key`.
    pub client_cert: Option<PathBuf>,
    /// Client private key (mTLS). Must be paired with `client_cert`.
    pub client_key: Option<PathBuf>,
    /// Accept any server certificate (development only). When true the
    /// server certificate chain and name are not verified at all.
    #[serde(default)]
    pub insecure_skip_verify: bool,
}

/// Pure validation of one upstream's TLS section: cross-field invariants
/// the deserializer cannot express. File existence/readability is checked
/// later, by `openrusty-proxy::tls::build` at apply time.
pub fn validate(upstream_name: &str, cfg: &UpstreamTlsConfig) -> Result<(), ConfigError> {
    let bad = |m: String| ConfigError::Invalid(m);
    if cfg.server_name.trim().is_empty() {
        return Err(bad(format!(
            "upstream {upstream_name} tls.server_name must not be empty"
        )));
    }
    if cfg.ca_cert.is_none() && !cfg.insecure_skip_verify {
        return Err(bad(format!(
            "upstream {upstream_name} tls requires ca_cert or insecure_skip_verify = true"
        )));
    }
    if cfg.client_cert.is_some() != cfg.client_key.is_some() {
        return Err(bad(format!(
            "upstream {upstream_name} tls.client_cert and tls.client_key must be set together"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tls_cfg() -> UpstreamTlsConfig {
        UpstreamTlsConfig {
            server_name: "api.example.com".into(),
            ca_cert: Some(PathBuf::from("/etc/ca.pem")),
            client_cert: None,
            client_key: None,
            insecure_skip_verify: false,
        }
    }

    #[test]
    fn valid_section_passes() {
        assert!(validate("u", &tls_cfg()).is_ok());
    }

    #[test]
    fn insecure_without_ca_is_valid() {
        let mut cfg = tls_cfg();
        cfg.ca_cert = None;
        cfg.insecure_skip_verify = true;
        assert!(validate("u", &cfg).is_ok());
    }

    #[test]
    fn missing_ca_without_insecure_is_rejected() {
        let mut cfg = tls_cfg();
        cfg.ca_cert = None;
        let err = validate("u", &cfg).unwrap_err();
        assert!(
            err.to_string().contains("ca_cert or insecure_skip_verify"),
            "got: {err}"
        );
    }

    #[test]
    fn empty_server_name_is_rejected() {
        let mut cfg = tls_cfg();
        cfg.server_name = "  ".into();
        assert!(validate("u", &cfg).is_err());
    }

    #[test]
    fn client_pair_must_be_complete() {
        let mut cfg = tls_cfg();
        cfg.client_cert = Some(PathBuf::from("/etc/client.pem"));
        let err = validate("u", &cfg).unwrap_err();
        assert!(
            err.to_string().contains("client_cert and tls.client_key"),
            "got: {err}"
        );
        cfg.client_key = Some(PathBuf::from("/etc/client.key"));
        assert!(validate("u", &cfg).is_ok());
    }

    #[test]
    fn toml_parses_tls_section_and_unknown_fields_rejected() {
        let base = r#"
[server]
listen = "127.0.0.1:8080"
[plugins]
dir = "build/plugins"
[[upstreams]]
name = "u"
tls = { server_name = "api.example.com", ca_cert = "/etc/ca.pem" }
[[routes]]
path_prefix = "/"
upstream = "u"
"#;
        let cfg: crate::Config = toml::from_str(base).unwrap();
        let tls = cfg.upstreams[0].tls.as_ref().unwrap();
        assert_eq!(tls.server_name, "api.example.com");
        assert_eq!(tls.ca_cert, Some(PathBuf::from("/etc/ca.pem")));
        assert!(!tls.insecure_skip_verify);

        let broken = base.replace("ca_cert", "ca_path");
        assert!(toml::from_str::<crate::Config>(&broken).is_err());
    }
}
