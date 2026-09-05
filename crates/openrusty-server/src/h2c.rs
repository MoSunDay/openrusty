//! HTTP/1.1 + h2c on one port: a raw TCP accept loop driving hyper-util's
//! `auto` connection builder, which sniffs the preface and speaks either
//! protocol. With `server.http1_only = true` the h2c preface sniffing is
//! skipped and every connection runs the plain HTTP/1.1 code path. Each
//! request is mapped onto the axum router, with `ConnectInfo(remote)`
//! injected per request.
//!
//! Every connection is built with a real timer plus connection-level
//! timeouts (see [`conn_timeouts`]): without them a client that opens a
//! socket and stalls (or silently dies without FIN) would pin accept-loop
//! capacity and buffers forever.
//!
//! Shutdown is the unified `watch` flag (`crate::shutdown`): the accept
//! loop leaves the loop on the flip and hyper's graceful shutdown takes
//! over the in-flight connections ([`GracefulDrain`]); a dropped sender is
//! honoured exactly like the flip (tests and embedders use that shortcut).

use axum::extract::ConnectInfo;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::HttpServerConnExec;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;
use tower::ServiceExt;

use crate::shutdown::{self, InFlight};

/// Connection-level timeouts applied to every accepted socket.
///
/// Deliberately not part of the config file yet: the values live behind
/// [`conn_timeouts`] so there is exactly one place to tune them (and one
/// place tests can override).
#[derive(Clone, Copy, Debug)]
pub struct ConnTimeouts {
    /// The whole request head must arrive within this window or the
    /// connection is closed (nginx `client_header_timeout` semantics).
    /// Also bounds the idle time before the first byte of a keep-alive
    /// request, so stalled or half-dead clients are reaped.
    pub header_read: Duration,
    /// HTTP/2 PING interval keeping an idle connection alive; `None`
    /// disables HTTP/2 keep-alive entirely.
    pub h2_keep_alive_interval: Option<Duration>,
    /// Close an HTTP/2 connection whose keep-alive PING went unanswered
    /// for this long (half-dead peer detection on the h2 path).
    pub h2_keep_alive_timeout: Duration,
}

/// Single source of truth for [`ConnTimeouts`]. Pure on purpose: tests call
/// it, tweak fields, and feed the result into [`serve_conn`] directly.
pub fn conn_timeouts() -> ConnTimeouts {
    ConnTimeouts {
        header_read: Duration::from_secs(60),
        h2_keep_alive_interval: Some(Duration::from_secs(30)),
        h2_keep_alive_timeout: Duration::from_secs(20),
    }
}

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

impl<I, S, E> GracefulDrain for hyper::server::conn::http2::Connection<I, S, E>
where
    S: hyper::service::HttpService<Incoming>,
    S::Error: Into<BoxError>,
    I: hyper::rt::Read + hyper::rt::Write + Unpin,
    S::ResBody: hyper::body::Body + 'static,
    <S::ResBody as hyper::body::Body>::Error: Into<BoxError>,
    // The auto trait is a supertrait alias of hyper's `Http2ServerConnExec`,
    // which `TokioExecutor` satisfies; reusing it keeps this impl bound to
    // the same executor type the rest of the module already names.
    E: HttpServerConnExec<S::Future, S::ResBody>,
{
    fn drain(self: Pin<&mut Self>) {
        self.graceful_shutdown()
    }
}

/// Protocol handling mode for one accepted connection.
///
/// `Auto` is the historical single-port shape: hyper-util's auto builder
/// sniffs the h2c preface per connection. The forced variants skip sniffing
/// entirely: the transparent intercept path (`crate::transparent`) has
/// already classified the stream with `proxy::detect` and re-injects the
/// sniffed bytes, so re-sniffing would only repeat settled work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtoMode {
    /// Sniff per connection (hyper-util auto builder): HTTP/1.1 or h2c.
    Auto,
    /// Plain HTTP/1.1, no sniffing.
    ForceHttp1,
    /// HTTP/2 prior knowledge, no sniffing.
    ForceHttp2,
}

impl From<bool> for ProtoMode {
    fn from(http1_only: bool) -> Self {
        if http1_only {
            ProtoMode::ForceHttp1
        } else {
            ProtoMode::Auto
        }
    }
}

/// Accept connections on `addr` until the shutdown signal fires. When
/// `http1_only` is set, connections are served as plain HTTP/1.1 without
/// h2c detection. Embedder-facing convenience over [`serve_listener`].
pub async fn serve(
    addr: SocketAddr,
    http1_only: bool,
    router: axum::Router,
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_listener(
        router,
        listener,
        http1_only,
        shutdown,
        conn_timeouts(),
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    )
    .await
}

/// Accept loop over an already-bound listener. Split out from [`serve`] so
/// multi-socket assembly (`crate::listeners::spawn`) can drive one accept
/// task per bound socket, and so tests can bind port 0, learn the port, and
/// inject custom timeouts. On the shutdown flip the loop ends; the accepted
/// connections keep draining (and are counted in `in_flight` until done).
pub async fn serve_listener(
    router: axum::Router,
    listener: tokio::net::TcpListener,
    http1_only: bool,
    mut shutdown: watch::Receiver<bool>,
    timeouts: ConnTimeouts,
    in_flight: InFlight,
) -> std::io::Result<()> {
    let addr = listener.local_addr()?;
    tracing::info!(%addr, http1_only, "listening (http/1.1 + h2c on one port)");
    let svc = router.into_service::<axum::body::Body>();

    // A signal that arrived before the loop started still stops it.
    if shutdown::is_draining(&shutdown) {
        tracing::info!(%addr, "shutdown signalled; not accepting");
        return Ok(());
    }
    loop {
        tokio::select! {
            sig = shutdown.changed() => {
                // The flip - or the sender being dropped, which tests and
                // embedders use as the stop shortcut - ends the loop.
                match sig {
                    Err(_) => {
                        tracing::info!("shutdown channel closed; stop accepting");
                        break;
                    }
                    Ok(()) if *shutdown.borrow() => {
                        tracing::info!("shutdown signal received; stop accepting");
                        break;
                    }
                    // Not our flip; keep serving.
                    Ok(()) => {}
                }
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, remote)) => {
                    let svc = svc.clone();
                    let rx = shutdown.clone();
                    in_flight.fetch_add(1, Ordering::Relaxed);
                    let pending = in_flight.clone();
                    tokio::spawn(async move {
                        let _ = stream.set_nodelay(true);
                        serve_conn(
                            svc,
                            stream,
                            remote,
                            ProtoMode::from(http1_only),
                            rx,
                            timeouts,
                        )
                        .await;
                        pending.fetch_sub(1, Ordering::Relaxed);
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

/// One connection: serve requests through the router in the requested
/// [`ProtoMode`], and gracefully drain when shutdown is signalled. Generic
/// over the IO so the transparent intercept path can hand over a stream
/// with its sniffed prefix re-injected ([`crate::transparent`]). `timeouts`
/// bounds how long a client may take to produce a request head and drives
/// HTTP/2 keep-alive; both need a timer installed to fire at all.
pub(crate) async fn serve_conn<S, I>(
    svc: S,
    io: I,
    remote: SocketAddr,
    mode: ProtoMode,
    rx: watch::Receiver<bool>,
    timeouts: ConnTimeouts,
) where
    S: tower::Service<
            axum::extract::Request,
            Response = axum::response::Response,
            Error = Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
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

    let io = TokioIo::new(io);
    // `with_upgrades` so WebSocket (HTTP/1 upgrade) requests get the
    // `OnUpgrade` extension and hyper holds the socket after the 101.
    match mode {
        ProtoMode::ForceHttp1 => {
            // Cheaper path: no preface sniffing, no http/2 state machine.
            // (`http1::Builder` has no `serve_connection_with_upgrades`; the
            // upgrade wrapper is applied separately below.)
            // `header_read_timeout` panics without a timer, so the timer goes
            // first: it bounds stalled/half-dead clients on this path too.
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .keep_alive(true)
                .header_read_timeout(Some(timeouts.header_read));
            let conn = builder.serve_connection(io, service).with_upgrades();
            drive(conn, rx, remote).await;
        }
        ProtoMode::ForceHttp2 => {
            // Prior-knowledge h2: the transparent sniff already saw the
            // client preface, so no negotiation happens (and none is
            // attempted). No upgrade wrapper: h2 upgrades are extended
            // CONNECTs, handled inside the connection itself.
            let mut builder = hyper::server::conn::http2::Builder::new(TokioExecutor::new());
            builder
                .timer(TokioTimer::new())
                .adaptive_window(true)
                .keep_alive_interval(timeouts.h2_keep_alive_interval)
                .keep_alive_timeout(timeouts.h2_keep_alive_timeout);
            let conn = builder.serve_connection(io, service);
            drive(conn, rx, remote).await;
        }
        ProtoMode::Auto => {
            let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
            // Both protocol halves get a timer: http1 needs it for
            // `header_read_timeout`, http2 needs it for the keep-alive pings.
            builder
                .http1()
                .timer(TokioTimer::new())
                .keep_alive(true)
                .header_read_timeout(Some(timeouts.header_read));
            builder
                .http2()
                .timer(TokioTimer::new())
                .adaptive_window(true)
                .keep_alive_interval(timeouts.h2_keep_alive_interval)
                .keep_alive_timeout(timeouts.h2_keep_alive_timeout);
            let conn = builder.serve_connection_with_upgrades(io, service);
            drive(conn, rx, remote).await;
        }
    }
}

/// Shared shutdown/serve loop: run the connection to completion, or start a
/// graceful drain once the shutdown signal fires. Generic over the http1-only
/// and auto (h2c) connection types, whose error types differ. A signal that
/// fired before the connection even started drains it right away.
async fn drive<C, E>(conn: C, rx: watch::Receiver<bool>, remote: SocketAddr)
where
    C: std::future::Future<Output = Result<(), E>> + GracefulDrain,
    E: std::fmt::Display,
{
    tokio::pin!(conn);

    let mut rx = rx;
    let mut shutting = shutdown::is_draining(&rx);
    loop {
        tokio::select! {
            res = &mut conn => {
                if let Err(e) = res {
                    tracing::debug!(%remote, error = %e, "connection closed with error");
                }
                break;
            }
            _ = rx.changed(), if !shutting => {
                // The flip and a dropped sender both mean: no new requests,
                // finish the in-flight one.
                shutting = true;
                conn.as_mut().drain();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app;
    use crate::testutil::{boot_state, TmpDir};
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Short, test-friendly timeouts: header read fast enough to keep the
    /// test well under a second, h2 keep-alive off (nothing here lives long
    /// enough to ping).
    fn test_timeouts() -> ConnTimeouts {
        ConnTimeouts {
            header_read: Duration::from_millis(250),
            h2_keep_alive_interval: None,
            h2_keep_alive_timeout: Duration::from_secs(20),
        }
    }

    /// Boot a real router on an ephemeral port with the given timeouts and
    /// return the bound address (the shutdown sender is kept alive by the
    /// caller; dropping it stops the accept loop and drains connections).
    async fn spawn_server(dir: &TmpDir, timeouts: ConnTimeouts) -> (SocketAddr, watch::Sender<bool>)
    {
        let state = boot_state(dir);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = watch::channel(false);
        tokio::spawn(serve_listener(
            app::router(state),
            listener,
            false,
            rx,
            timeouts,
            shutdown::new_signal().in_flight,
        ));
        (addr, tx)
    }

    /// Read until the peer closes; returns when EOF or a reset arrives.
    async fn read_until_closed(sock: &mut tokio::net::TcpStream) {
        let mut buf = [0u8; 256];
        loop {
            match sock.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    }

    #[tokio::test]
    async fn http1_only_idle_connection_is_closed_after_header_timeout() {
        let dir = TmpDir::new("h2c-idle");
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_tx, rx) = watch::channel(false);
        // Plain HTTP/1.1 path: the header timeout arms before any byte
        // arrives, so a socket opened and then abandoned is reaped.
        tokio::spawn(serve_listener(
            app::router(state),
            listener,
            true,
            rx,
            test_timeouts(),
            shutdown::new_signal().in_flight,
        ));

        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        // Send nothing: the request head never arrives, so the server must
        // give up after `header_read` instead of waiting forever.
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(5), read_until_closed(&mut sock))
            .await
            .expect("idle connection was not reaped within 5s");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(150),
            "connection closed before the header timeout even started ({elapsed:?})"
        );
        assert!(
            elapsed < Duration::from_millis(2000),
            "connection outlived the 250ms header timeout by far ({elapsed:?})"
        );
    }

    #[tokio::test]
    async fn h2c_stalled_request_head_is_closed_after_header_timeout() {
        let dir = TmpDir::new("h2c-stall");
        dir.write_config(&dir.standard_config());
        let (addr, _tx) = spawn_server(&dir, test_timeouts()).await;

        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        // A partial head (enough to pass protocol sniffing) that never
        // completes: the http1 half must reap it after `header_read`.
        // (With zero bytes the auto sniffer cannot even pick a protocol,
        // so this is the h2c analogue of the idle test above.)
        sock.write_all(b"GET /openrusty/status HTTP/1.1\r\n")
            .await
            .unwrap();
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(5), read_until_closed(&mut sock))
            .await
            .expect("stalled head was not reaped within 5s");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(150),
            "connection closed before the header timeout even started ({elapsed:?})"
        );
        assert!(
            elapsed < Duration::from_millis(2000),
            "connection outlived the 250ms header timeout by far ({elapsed:?})"
        );
    }

    #[tokio::test]
    async fn client_eof_closes_the_connection_promptly() {
        let dir = TmpDir::new("h2c-eof");
        dir.write_config(&dir.standard_config());
        let (addr, _tx) = spawn_server(&dir, test_timeouts()).await;

        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        // Half-close: the client is done and will never send a head. The
        // server must release the connection instead of parking on it.
        sock.shutdown().await.unwrap();
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(2), read_until_closed(&mut sock))
            .await
            .expect("server never released the EOFed connection");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "connection cleanup took suspiciously long ({:?})",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn requests_still_served_with_conn_timers_installed() {
        let dir = TmpDir::new("h2c-timers-ok");
        dir.write_config(&dir.standard_config());
        let (addr, _tx) = spawn_server(&dir, test_timeouts()).await;

        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = "GET /openrusty/status HTTP/1.1\r\nHost: gw\r\nConnection: close\r\n\r\n";
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut raw))
            .await
            .expect("request with timers installed never completed")
            .expect("read error after status line");
        let head = String::from_utf8_lossy(&raw);
        assert!(
            head.starts_with("HTTP/1.1 200"),
            "expected 200 from /openrusty/status, got: {head}"
        );
    }
}
