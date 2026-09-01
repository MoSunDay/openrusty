//! rustls-backed HTTPS client for the Kubernetes apiserver.
//!
//! [`Client`] binds one [`Cluster`] (endpoint + CA) and one resolved
//! [`Credentials`] to a pooled hyper legacy client whose connector
//! ([`HttpsConnector`]) upgrades connections with tokio-rustls. The public
//! surface is deliberately tiny:
//!
//! - [`Client::get`] performs a generic GET and hands back the response
//!   body as a [`ByteStream`];
//! - [`Client::watch_raw`] composes a watch URL via the pure
//!   [`watch_path`] helper and returns the raw newline-delimited JSON
//!   stream. Deserialization into typed watch events belongs to the
//!   upcoming watch-state milestone, not here.
//!
//! Non-goals of this milestone: retries, refresh of exec tokens (the token
//! is resolved once at client construction), and configurable timeouts.
//! The tunables below are constants until the server wires its config in.

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{HeaderValue, AUTHORIZATION};
use hyper::Uri;
use hyper_util::client::legacy::Client as LegacyClient;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use rustls::client::ResolvesClientCert;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

use crate::auth::{exec, Cluster, Credentials};
use crate::connector::HttpsConnector;
use crate::error::{K8sError, Result};

/// Connect timeout applied to every apiserver TCP attempt. Configurable
/// via the server config in a later milestone.
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Idle keep-alive timeout for pooled apiserver connections. Configurable
/// via the server config in a later milestone.
pub const POOL_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Type alias keeping the hyper generics out of call sites.
pub type HttpsClient = LegacyClient<HttpsConnector, Full<Bytes>>;

/// Chunked byte stream of an apiserver response body (watch responses are
/// newline-delimited JSON objects over this stream).
pub type ByteStream = hyper::body::Incoming;

/// Compose the query string of a watch request (pure).
///
/// `resource_version` is passed through verbatim; apiserver resource
/// versions are numeric strings, so no escaping is needed. An empty
/// version omits the parameter, which the apiserver interprets as "start
/// from now".
pub fn watch_path(path: &str, resource_version: &str) -> String {
    // `path` may already carry a query (e.g. the TLS fieldSelector on the
    // secrets watch); join with `&` there instead of emitting a second `?`.
    let sep = if path.contains('?') { '&' } else { '?' };
    if resource_version.is_empty() {
        format!("{path}{sep}watch=1&allowWatchBookmarks=true")
    } else {
        format!("{path}{sep}watch=1&allowWatchBookmarks=true&resourceVersion={resource_version}")
    }
}

/// Join a cluster endpoint and a request path into a full [`Uri`] (pure).
pub fn request_uri(endpoint: &Uri, path_and_query: &str) -> Result<Uri> {
    let scheme = endpoint.scheme_str().unwrap_or("https");
    let authority = endpoint
        .authority()
        .ok_or_else(|| K8sError::Endpoint(endpoint.to_string()))?;
    let path = if path_and_query.starts_with('/') {
        path_and_query.to_string()
    } else {
        format!("/{path_and_query}")
    };
    let uri = Uri::builder()
        .scheme(scheme)
        .authority(authority.as_str())
        .path_and_query(path)
        .build()
        .map_err(|_| K8sError::Endpoint(format!("{scheme}://{}{path_and_query}", authority)))?;
    Ok(uri)
}

/// Parse a PEM bundle into DER certificates (pure).
pub fn parse_cert_pem(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>> {
    rustls_pemfile::certs(&mut &pem[..])
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| K8sError::Tls(format!("unparsable certificate PEM: {e}")))
}

/// Parse a PEM private key (pure).
pub fn parse_key_pem(pem: &[u8]) -> Result<PrivateKeyDer<'static>> {
    rustls_pemfile::private_key(&mut &pem[..])
        .map_err(|e| K8sError::Tls(format!("unparsable key PEM: {e}")))?
        .ok_or_else(|| K8sError::Tls("no private key in PEM input".to_string()))
}

/// Build the rustls client config: CA trust anchors plus optional client
/// certificate (pure).
pub fn build_tls_config(
    ca_pem: &[u8],
    client_cert: Option<(&[u8], &[u8])>,
) -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in parse_cert_pem(ca_pem)? {
        roots
            .add(cert)
            .map_err(|e| K8sError::Tls(format!("rejected CA certificate: {e}")))?;
    }

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|e| K8sError::Tls(format!("no enabled protocol version: {e}")))?
        .with_root_certificates(roots);

    match client_cert {
        Some((cert_pem, key_pem)) => {
            let certs = parse_cert_pem(cert_pem)?;
            if certs.is_empty() {
                return Err(K8sError::Tls(
                    "client certificate chain is empty".to_string(),
                ));
            }
            let signing_key =
                rustls::crypto::ring::sign::any_supported_type(&parse_key_pem(key_pem)?)
                    .map_err(|e| K8sError::Tls(format!("unsupported client key: {e}")))?;
            let certified_key = Arc::new(rustls::sign::CertifiedKey::new(certs, signing_key));
            Ok(config.with_client_cert_resolver(Arc::new(SingleCertResolver(certified_key))))
        }
        None => Ok(config.with_no_client_auth()),
    }
}

/// [`ResolvesClientCert`] that always presents one configured identity.
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

/// HTTPS client for one apiserver, carrying resolved credentials.
///
/// Constructing a [`Client`] is the only place I/O-adjacent work happens:
/// a [`Credentials::ExecToken`] is executed once here and the resulting
/// token pinned for the client's lifetime. The struct owns the connection
/// pool (a Rust resource holder, not an OO design); all request math lives
/// in pure free functions ([`watch_path`], [`request_uri`]).
pub struct Client {
    endpoint: Uri,
    http: HttpsClient,
    authorization: Option<HeaderValue>,
}

impl Client {
    /// Build a client from a loaded cluster and credentials.
    ///
    /// No network I/O happens here beyond (possibly) running the exec
    /// credential plugin.
    pub fn new(cluster: &Cluster, credentials: &Credentials) -> Result<Self> {
        let (authorization, client_cert) = match credentials {
            Credentials::Bearer(token) => (Some(bearer_header(token)?), None),
            Credentials::ExecToken(exec_config) => {
                let token = exec::run(exec_config)?;
                (Some(bearer_header(&token)?), None)
            }
            Credentials::ClientCert { cert, key } => {
                (None, Some((cert.as_slice(), key.as_slice())))
            }
        };

        let tls = Arc::new(build_tls_config(&cluster.ca, client_cert)?);
        let http = LegacyClient::builder(TokioExecutor::new())
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .pool_timer(TokioTimer::new())
            .build(HttpsConnector::new(tls));

        Ok(Self {
            endpoint: cluster.endpoint.clone(),
            http,
            authorization,
        })
    }

    /// Generic GET of an apiserver path, e.g.
    /// `/apis/networking.k8s.io/v1/ingresses`.
    pub async fn get(&self, path: &str) -> Result<hyper::Response<ByteStream>> {
        let uri = request_uri(&self.endpoint, path)?;
        let request = self.request_builder(uri).body(Full::new(Bytes::new()))?;
        Ok(self.http.request(request).await?)
    }

    /// Open a watch stream at `path` pinned to `resource_version`.
    ///
    /// Returns the raw body: newline-delimited JSON watch objects
    /// (bookmarks enabled). Typed deserialization is the next milestone's
    /// job; this stays at the byte-stream layer.
    pub async fn watch_raw(
        &self,
        path: &str,
        resource_version: &str,
    ) -> Result<hyper::Response<ByteStream>> {
        self.get(&watch_path(path, resource_version)).await
    }

    /// Prepend the scheme/authority of the cluster endpoint to `path` and
    /// return the absolute URI (exposed for tests and future callers).
    pub fn uri_for(&self, path: &str) -> Result<Uri> {
        request_uri(&self.endpoint, path)
    }

    /// Start building a request against `uri`, injecting auth headers.
    fn request_builder(&self, uri: Uri) -> hyper::http::request::Builder {
        let mut builder = hyper::Request::builder()
            .uri(uri)
            .header(hyper::header::ACCEPT, "application/json");
        if let Some(authorization) = &self.authorization {
            builder = builder.header(AUTHORIZATION, authorization.clone());
        }
        builder
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The pooled hyper client is intentionally elided: it is a
        // connection cache, not something error reports should render.
        f.debug_struct("Client")
            .field("endpoint", &self.endpoint)
            .field("authorization", &self.authorization.as_ref().map(|_| "set"))
            .finish_non_exhaustive()
    }
}

/// Render a bearer token as a header value (pure).
fn bearer_header(token: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| K8sError::CredentialInvalid("token contains invalid header bytes".to_string()))
}

/// Convenience: load `(Cluster, Credentials)` from a kubeconfig path and
/// build a [`Client`] in one step (no env lookups, no in-cluster fallback).
pub fn client_from_kubeconfig(path: &Path) -> Result<Client> {
    let (cluster, credentials) = crate::auth::load_kubeconfig_file(path)?;
    Client::new(&cluster, &credentials)
}

#[cfg(test)]
mod tests;
