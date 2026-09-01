//! HTTPS connector: plain TCP dialing plus a tokio-rustls upgrade.
//!
//! Small `tower::Service<Uri>` handed to hyper's legacy client. DNS
//! resolution, happy eyeballs and the connect timeout come from hyper's
//! `HttpConnector`; this layer only dials, upgrades the established TCP
//! stream to TLS with the server name from the request URI, and adapts the
//! tokio I/O traits to hyper's `rt` traits (the same `TokioIo` wrapping
//! hyper-rustls performs). Kept separate from [`crate::client`] to stay
//! under the per-file size budget.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use hyper::rt::{Read, Write};
use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection, HttpConnector};
use hyper_util::rt::TokioIo;
use rustls_pki_types::ServerName;
use tokio_rustls::TlsConnector;
use tower::Service;

/// The plain TCP stream hyper's `HttpConnector` hands back.
type ConnectorTcp = TokioIo<tokio::net::TcpStream>;

/// TLS-terminated stream hyper reads and writes requests on.
///
/// Composition mirrors hyper-rustls: the connector's TCP stream
/// ([`ConnectorTcp`]) is wrapped once more in `TokioIo` so tokio-rustls
/// sees tokio's I/O traits, and the finished TLS stream is wrapped again
/// so hyper sees its own `rt` traits.
#[derive(Debug)]
pub struct TlsStream(TokioIo<tokio_rustls::client::TlsStream<TokioIo<ConnectorTcp>>>);

impl Read for TlsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl Write for TlsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl Connection for TlsStream {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

/// hyper connector that terminates TLS with tokio-rustls.
#[derive(Clone)]
pub struct HttpsConnector {
    /// Plain TCP dialer (DNS resolution, happy eyeballs, connect timeout).
    http: HttpConnector,
    /// Shared rustls session configuration.
    tls: Arc<rustls::ClientConfig>,
}

impl HttpsConnector {
    /// Create a connector for one TLS configuration.
    pub fn new(tls: Arc<rustls::ClientConfig>) -> Self {
        let mut http = HttpConnector::new();
        http.set_connect_timeout(Some(crate::client::CONNECT_TIMEOUT));
        Self { http, tls }
    }
}

impl Service<Uri> for HttpsConnector {
    type Response = TlsStream;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Service::poll_ready(&mut self.http, cx).map_err(|e| Box::new(e) as Self::Error)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let mut http = self.http.clone();
        let tls = TlsConnector::from(Arc::clone(&self.tls));
        Box::pin(async move {
            let host = uri.host().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "URI without host")
            })?;
            let tcp = http
                .call(uri.clone())
                .await
                .map_err(|e| Box::new(e) as Self::Error)?;
            let server_name =
                ServerName::try_from(host.to_string()).map_err(|e| -> Self::Error {
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("invalid TLS server name `{host}`: {e}"),
                    ))
                })?;
            let stream = tls.connect(server_name, TokioIo::new(tcp)).await?;
            Ok(TlsStream(TokioIo::new(stream)))
        })
    }
}
