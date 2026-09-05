//! hyper-facing half of upstream TLS: the stream adapter and the
//! one-peer connector handed to the pooled legacy client. The TLS
//! material itself (rustls config, trust anchors, identity key) lives
//! in [`super`].

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::rt::{Read, Write};
use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use rustls::ClientConfig;
use tokio_rustls::TlsConnector;
use tower::Service;

use crate::client::HttpBody;

/// TLS-terminated stream hyper reads and writes requests on. The tokio
/// TLS stream is wrapped in `TokioIo` so hyper sees its own `rt` traits;
/// because the connector dials the raw `TcpStream` itself, a single wrap
/// suffices (hyper-rustls double-wraps only since `HttpConnector` hands
/// it an already-wrapped stream).
#[derive(Debug)]
pub struct TlsStream(TokioIo<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>);

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

/// hyper connector for one fixed TLS peer: dials `addr` under
/// `connect_timeout` (TCP connect plus TLS handshake), then presents the
/// TLS stream to the legacy client. The URI authority is ignored: the
/// address and the SNI/verification name are fixed per pool key.
#[derive(Clone)]
pub struct HttpsConnector {
    pub(crate) addr: SocketAddr,
    pub(crate) server_name: ServerName<'static>,
    pub(crate) tls: Arc<ClientConfig>,
    pub(crate) connect_timeout: Duration,
}

impl Service<Uri> for HttpsConnector {
    type Response = TlsStream;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _uri: Uri) -> Self::Future {
        let addr = self.addr;
        let server_name = self.server_name.clone();
        let tls = TlsConnector::from(Arc::clone(&self.tls));
        let timeout = self.connect_timeout;
        Box::pin(async move {
            let tcp = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr))
                .await
                .map_err(|_| -> Self::Error {
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("connect to {addr} timed out after {timeout:?}"),
                    ))
                })??;
            tcp.set_nodelay(true)?;
            let stream = tls
                .connect(server_name, tcp)
                .await
                .map_err(|e| -> Self::Error { Box::new(std::io::Error::other(e)) })?;
            Ok(TlsStream(TokioIo::new(stream)))
        })
    }
}

/// A pooled hyper HTTP/1 client bound to the TLS connector.
pub type HttpsClient = hyper_util::client::legacy::Client<HttpsConnector, HttpBody>;
