//! Upstream (outbound) TLS material: the rustls client config built from
//! an `[upstreams.tls]` section. The hyper-facing half (stream adapter
//! and the one-peer connector) lives in [`connector`].
//!
//! Mirrors nginx `proxy_ssl_*`: the section names the SNI/verification
//! name, an optional trust anchor and an optional mTLS client pair.
//! [`build`] turns that section into an [`UpstreamTls`] (fallible: cert
//! files are read and parsed here, so boot/reload fails fast on bad
//! material); pooled clients are keyed by the content-based
//! [`TlsClientKey`], so a reload with identical TLS settings reuses the
//! warm pool.

use std::path::Path;
use std::sync::Arc;

use openrusty_core::config::UpstreamTlsConfig;
use rustls::client::ResolvesClientCert;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};

/// Content identity of one TLS configuration: two upstreams whose keys are
/// equal share a pooled client (same trust anchor, same identity), while a
/// changed key forces a fresh pool. Paths (not file contents) decide, so a
/// re-issued cert inside the same files needs a reload-visible change or a
/// restart; that matches the static-listener TLS semantics.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TlsClientKey {
    pub server_name: String,
    pub ca_cert: Option<std::path::PathBuf>,
    pub client_cert: Option<std::path::PathBuf>,
    pub client_key: Option<std::path::PathBuf>,
    pub insecure_skip_verify: bool,
}

impl TlsClientKey {
    /// Derive the key from the config section (pure).
    pub fn from_config(cfg: &UpstreamTlsConfig) -> TlsClientKey {
        TlsClientKey {
            server_name: cfg.server_name.clone(),
            ca_cert: cfg.ca_cert.clone(),
            client_cert: cfg.client_cert.clone(),
            client_key: cfg.client_key.clone(),
            insecure_skip_verify: cfg.insecure_skip_verify,
        }
    }
}

/// Built TLS material for one upstream: the rustls config shared by every
/// pooled connection, plus the identity key the client pool keys on.
#[derive(Debug, Clone)]
pub struct UpstreamTls {
    /// SNI name and the name the certificate is verified against.
    pub server_name: String,
    /// Pool identity (see [`TlsClientKey`]).
    pub key: TlsClientKey,
    /// Shared rustls client configuration (cheap to clone via `Arc`).
    pub config: Arc<ClientConfig>,
}

/// Read one PEM file into bytes.
fn read_pem(path: &Path, what: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("cannot read {what} {}: {e}", path.display()))
}

/// Parse a PEM bundle into DER certificates (pure).
fn parse_certs(pem: &[u8], what: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    rustls_pemfile::certs(&mut &pem[..])
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("unparsable {what} PEM: {e}"))
}

/// Parse a PEM private key (pure).
fn parse_key(pem: &[u8], what: &str) -> Result<PrivateKeyDer<'static>, String> {
    rustls_pemfile::private_key(&mut &pem[..])
        .map_err(|e| format!("unparsable {what} PEM: {e}"))?
        .ok_or_else(|| format!("no private key found in {what} PEM"))
}

/// Build the complete TLS material for one upstream from its config
/// section. Fallible on purpose: certificate files are read and parsed
/// here so a broken anchor or key fails boot/reload with a precise
/// message instead of failing every request later.
pub fn build(cfg: &UpstreamTlsConfig) -> Result<UpstreamTls, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = TlsClientKey::from_config(cfg);

    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|e| format!("no enabled TLS protocol version: {e}"))?;

    let builder = match (&cfg.ca_cert, cfg.insecure_skip_verify) {
        (Some(path), _) => {
            let pem = read_pem(path, "ca_cert")?;
            let certs = parse_certs(&pem, "ca_cert")?;
            if certs.is_empty() {
                return Err(format!(
                    "ca_cert {} contains no certificates",
                    path.display()
                ));
            }
            let mut roots = rustls::RootCertStore::empty();
            for cert in certs {
                roots
                    .add(cert)
                    .map_err(|e| format!("rejected CA certificate in {}: {e}", path.display()))?;
            }
            builder.with_root_certificates(roots)
        }
        (None, true) => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert(provider))),
        (None, false) => {
            return Err(
                "tls requires ca_cert or insecure_skip_verify = true (config validation miss)"
                    .to_string(),
            )
        }
    };

    let mut config = match (&cfg.client_cert, &cfg.client_key) {
        (Some(cert_path), Some(key_path)) => {
            let certs = parse_certs(&read_pem(cert_path, "client_cert")?, "client_cert")?;
            if certs.is_empty() {
                return Err(format!(
                    "client_cert {} contains no certificates",
                    cert_path.display()
                ));
            }
            let key_pem = read_pem(key_path, "client_key")?;
            let signing =
                rustls::crypto::ring::sign::any_supported_type(&parse_key(&key_pem, "client_key")?)
                    .map_err(|e| format!("unsupported client key {}: {e}", key_path.display()))?;
            let certified = Arc::new(rustls::sign::CertifiedKey::new(certs, signing));
            builder.with_client_cert_resolver(Arc::new(SingleCertResolver(certified)))
        }
        (None, None) => builder.with_no_client_auth(),
        // XOR pair: rejected by config validation; keep build defensive.
        _ => return Err("tls.client_cert and tls.client_key must be set together".to_string()),
    };
    // One protocol only: the pooled legacy client speaks HTTP/1.1, so ALPN
    // must never negotiate h2 against a multi-protocol upstream.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(UpstreamTls {
        server_name: cfg.server_name.clone(),
        key,
        config: Arc::new(config),
    })
}

/// [`ResolvesClientCert`] that always presents one configured identity
/// (mTLS client pair).
#[derive(Debug)]
struct SingleCertResolver(Arc<rustls::sign::CertifiedKey>);

impl ResolvesClientCert for SingleCertResolver {
    fn resolve(
        &self,
        _root_hint_subjects: &[&[u8]],
        _sigschemes: &[rustls::SignatureScheme],
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(Arc::clone(&self.0))
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// `danger::ServerCertVerifier` that accepts any server certificate.
/// Used only for `insecure_skip_verify = true`; handshake signatures are
/// still cryptographically checked against the provider so the handshake
/// itself cannot be downgraded into garbage.
#[derive(Debug)]
struct AcceptAnyServerCert(Arc<CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub mod connector;

pub use connector::{HttpsClient, HttpsConnector, TlsStream};

#[cfg(test)]
mod tests {
    use super::*;

    fn tls_cfg() -> UpstreamTlsConfig {
        UpstreamTlsConfig {
            server_name: "localhost".into(),
            ca_cert: None,
            client_cert: None,
            client_key: None,
            insecure_skip_verify: true,
        }
    }

    /// One unique scratch path under the system temp dir (no cleanup
    /// dependency needed: only written, never left meaningful).
    fn scratch(name: &str) -> std::path::PathBuf {
        let unique = format!(
            "openrusty-proxy-tls-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        std::env::temp_dir().join(unique)
    }

    #[test]
    fn build_rejects_missing_ca_file() {
        let mut cfg = tls_cfg();
        cfg.insecure_skip_verify = false;
        cfg.ca_cert = Some(scratch("ca.pem"));
        let err = build(&cfg).unwrap_err();
        assert!(err.contains("ca_cert"), "got: {err}");
    }

    #[test]
    fn build_rejects_ca_without_certificates() {
        let path = scratch("empty.pem");
        std::fs::write(&path, b"not a pem file").unwrap();
        let mut cfg = tls_cfg();
        cfg.insecure_skip_verify = false;
        cfg.ca_cert = Some(path);
        let err = build(&cfg).unwrap_err();
        assert!(err.contains("no certificates"), "got: {err}");
    }

    #[test]
    fn insecure_build_ok_with_http1_alpn_only() {
        let tls = build(&tls_cfg()).unwrap();
        assert_eq!(tls.server_name, "localhost");
        assert_eq!(tls.config.alpn_protocols, vec![b"http/1.1".to_vec()]);
        assert!(tls.key.insecure_skip_verify);
    }

    #[test]
    fn build_rejects_client_pair_xor() {
        let mut cfg = tls_cfg();
        cfg.client_cert = Some(scratch("c.pem"));
        assert!(build(&cfg).is_err());
    }

    #[test]
    fn keys_are_content_based() {
        let base = TlsClientKey::from_config(&tls_cfg());
        assert_eq!(base, TlsClientKey::from_config(&tls_cfg()));

        let mut other = tls_cfg();
        other.server_name = "example.com".into();
        assert_ne!(base, TlsClientKey::from_config(&other));

        other = tls_cfg();
        other.insecure_skip_verify = false;
        other.ca_cert = Some(scratch("ca.pem"));
        assert_ne!(base, TlsClientKey::from_config(&other));
    }
}
