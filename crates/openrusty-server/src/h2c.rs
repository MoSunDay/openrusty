//! HTTP/1.1 + h2c on one port: a raw TCP accept loop driving hyper-util's
//! `auto` connection builder, which sniffs the preface and speaks either
//! protocol. With `server.http1_only = true` the h2c preface sniffing is
//! skipped and every connection runs the plain HTTP/1.1 code path. Each
//! request is mapped onto the axum router, with `ConnectInfo(remote)`
//! injected per request.

use axum::extract::ConnectInfo;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::HttpServerConnExec;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use tokio::sync::broadcast;
use tower::ServiceExt;

/// `graceful_shutdown` is an inherent method with receiver `Pin<&mut Self>`
/// on both connection types (hyper 1.x dropped the `ext::GracefulShutdown`
/// trait). This private trait re-exposes it so the drain loop below can stay
/// generic over the http1-only and auto (h2c) connection builders.
trait GracefulDrain {
    fn drain(self: Pin<&mut Self>);
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl<I, S> GracefulDrain for hyper::server::conn::http1::UpgradeableConnection<I, S>
where
    S: hyper::service::HttpService<Incoming>,
    S::Error: Into<BoxError>,
    I: hyper::rt::Read + hyper::rt::Write + Unpin,
    S::ResBody: hyper::body::Body + 'static,
    <S::ResBody as hyper::body::Body>::Error: Into<BoxError>,
{
    fn drain(self: Pin<&mut Self>) {
        self.graceful_shutdown()
    }
}

impl<I, S, E> GracefulDrain for hyper_util::server::conn::auto::UpgradeableConnection<'_, I, S, E>
where
    S: hyper::service::HttpService<Incoming>,
    S::Error: Into<BoxError>,
    I: hyper::rt::Read + hyper::rt::Write + Unpin,
    S::ResBody: hyper::body::Body + 'static,
    <S::ResBody as hyper::body::Body>::Error: Into<BoxError>,
    E: HttpServerConnExec<S::Future, S::ResBody>,
{
    fn drain(self: Pin<&mut Self>) {
        self.graceful_shutdown()
    }
}

/// Accept connections on `addr` until `shutdown` fires. When `http1_only`
/// is set, connections are served as plain HTTP/1.1 without h2c detection.
pub async fn serve(
    addr: SocketAddr,
    http1_only: bool,
    router: axum::Router,
    mut shutdown: broadcast::Receiver<()>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, http1_only, "listening (http/1.1 + h2c on one port)");
    let svc = router.into_service::<axum::body::Body>();

    loop {
        tokio::select! {
            sig = shutdown.recv() => {
                match sig {
                    Ok(()) | Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("shutdown signal received; stop accepting");
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(n, "shutdown channel lagged");
                    }
                }
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, remote)) => {
                    let svc = svc.clone();
                    let rx = shutdown.resubscribe();
                    tokio::spawn(async move {
                        serve_conn(svc, stream, remote, http1_only, rx).await;
                    });
                }
                Err(e) => {
                    // Transient accept errors must not kill the listener.
                    tracing::debug!(error = %e, "accept failed");
                }
            },
        }
    }
    Ok(())
}

/// One connection: sniff the protocol (or, when `http1_only` is set, serve
/// plain HTTP/1.1 directly), serve requests through the router, and
/// gracefully drain when shutdown is signalled.
async fn serve_conn<S>(
    svc: S,
    stream: tokio::net::TcpStream,
    remote: SocketAddr,
    http1_only: bool,
    rx: broadcast::Receiver<()>,
) where
    S: tower::Service<
            axum::extract::Request,
            Response = axum::response::Response,
            Error = Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    let _ = stream.set_nodelay(true);
    let service = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
        let svc = svc.clone();
        async move {
            let (mut parts, body) = req.into_parts();
            parts.extensions.insert(ConnectInfo(remote));
            let req = hyper::Request::from_parts(parts, axum::body::Body::new(body));
            let resp = match ServiceExt::oneshot(svc, req).await {
                Ok(resp) => resp,
                Err(never) => match never {},
            };
            Ok::<_, Infallible>(resp)
        }
    });

    let io = TokioIo::new(stream);
    // `with_upgrades` so WebSocket (HTTP/1 upgrade) requests get the
    // `OnUpgrade` extension and hyper holds the socket after the 101.
    if http1_only {
        // Cheaper path: no preface sniffing, no http/2 state machine.
        // (`http1::Builder` has no `serve_connection_with_upgrades`; the
        // upgrade wrapper is applied separately below.)
        let conn = hyper::server::conn::http1::Builder::new()
            .keep_alive(true)
            .serve_connection(io, service)
            .with_upgrades();
        drive(conn, rx, remote).await;
    } else {
        let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
        builder.http1().keep_alive(true);
        builder.http2().adaptive_window(true);
        let conn = builder.serve_connection_with_upgrades(io, service);
        drive(conn, rx, remote).await;
    }
}

/// Shared shutdown/serve loop: run the connection to completion, or start a
/// graceful drain once the shutdown signal fires. Generic over the http1-only
/// and auto (h2c) connection types, whose error types differ.
async fn drive<C, E>(conn: C, mut rx: broadcast::Receiver<()>, remote: SocketAddr)
where
    C: std::future::Future<Output = Result<(), E>> + GracefulDrain,
    E: std::fmt::Display,
{
    tokio::pin!(conn);

    let mut shutting = false;
    loop {
        tokio::select! {
            res = &mut conn => {
                if let Err(e) = res {
                    tracing::debug!(%remote, error = %e, "connection closed with error");
                }
                break;
            }
            sig = rx.recv(), if !shutting => {
                match sig {
                    Ok(()) | Err(broadcast::error::RecvError::Closed) => {
                        shutting = true;
                        conn.as_mut().drain();
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(n, "shutdown channel lagged");
                    }
                }
            }
        }
    }
}
