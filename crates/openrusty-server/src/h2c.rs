//! HTTP/1.1 + h2c on one port: a raw TCP accept loop driving hyper-util's
//! `auto` connection builder, which sniffs the preface and speaks either
//! protocol. Each request is mapped onto the axum router, with
//! `ConnectInfo(remote)` injected per request.

use axum::extract::ConnectInfo;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio::sync::broadcast;
use tower::ServiceExt;

/// Accept connections on `addr` until `shutdown` fires.
pub async fn serve(
    addr: SocketAddr,
    router: axum::Router,
    mut shutdown: broadcast::Receiver<()>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "listening (http/1.1 + h2c on one port)");
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
                        serve_conn(svc, stream, remote, rx).await;
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

/// One connection: sniff the protocol, serve requests through the router,
/// and gracefully drain when shutdown is signalled.
async fn serve_conn<S>(
    svc: S,
    stream: tokio::net::TcpStream,
    remote: SocketAddr,
    mut rx: broadcast::Receiver<()>,
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

    let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    builder.http1().keep_alive(true);
    builder.http2().adaptive_window(true);

    let io = TokioIo::new(stream);
    // `_with_upgrades` so WebSocket (HTTP/1 upgrade) requests get the
    // `OnUpgrade` extension and hyper holds the socket after the 101.
    let conn = builder.serve_connection_with_upgrades(io, service);
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
                        conn.as_mut().graceful_shutdown();
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(n, "shutdown channel lagged");
                    }
                }
            }
        }
    }
}
