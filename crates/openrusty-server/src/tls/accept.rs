//! TLS accept path: handshake, ALPN -> protocol mode, and the accept
//! loop for a TLS-terminating listener.
//!
//! The loop mirrors `h2c::serve_listener` exactly (same shutdown
//! select, same in-flight accounting, same tolerance for transient
//! accept errors) with one addition: the rustls handshake happens
//! per connection before the HTTP pipeline takes over. A failed
//! handshake (unknown SNI with no fallback, bad client, dead socket)
//! is a per-connection event - it is logged and never kills the
//! listener.

use crate::h2c::{self, ProtoMode};
use crate::shutdown::{self, InFlight};
use openrusty_core::config::ListenerConfig;
use rustls::ServerConfig;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;

/// TLS handshake + protocol negotiation for one accepted socket.
///
/// Returns the decrypted stream and the [`ProtoMode`] the ALPN
/// extension picked: `h2` forces HTTP/2 prior knowledge, `http/1.1`
/// is the sniffing `Auto` mode anyway, and a client that sends no
/// ALPN at all yields `None` so the caller can apply the listener's
/// own default (auto sniffing, or forced HTTP/1.1 for `http1_only`).
pub(crate) async fn accept_tls<S>(
    stream: S,
    config: Arc<ServerConfig>,
) -> std::io::Result<(tokio_rustls::server::TlsStream<S>, Option<ProtoMode>)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let acceptor = TlsAcceptor::from(config);
    let stream = acceptor.accept(stream).await?;
    let mode = match stream.get_ref().1.alpn_protocol() {
        Some(b"h2") => Some(ProtoMode::ForceHttp2),
        // `http/1.1` is exactly what Auto sniffs out on plain sockets.
        Some(b"http/1.1") => Some(ProtoMode::Auto),
        // No ALPN extension (or an unknown value): fall through to the
        // listener default and let hyper-util sniff like it always does.
        _ => None,
    };
    Ok((stream, mode))
}

/// Accept loop for a TLS-terminating listener. Handshake per
/// connection, then the shared HTTP pipeline serves the decrypted
/// stream (`h2c::serve_conn` is generic over the IO, so TLS and plain
/// sockets run the identical request path).
///
/// `server_config` carries the shared SNI resolver (see
/// [`crate::tls::boot`]): cert rotation swaps resolver tables and never
/// touches the established sessions.
pub(crate) async fn serve_listener(
    router: axum::Router,
    cfg: ListenerConfig,
    listener: TcpListener,
    server_config: Arc<ServerConfig>,
    mut shutdown: watch::Receiver<bool>,
    in_flight: InFlight,
) -> std::io::Result<()> {
    let addr = listener.local_addr()?;
    tracing::info!(
        role = cfg.role.as_str(),
        %addr,
        http1_only = cfg.http1_only,
        "listening (TLS termination, http/1.1 + h2 via ALPN)"
    );
    let svc = router.into_service::<axum::body::Body>();
    // Listener-level protocol default when the client sends no ALPN.
    let default_mode = ProtoMode::from(cfg.http1_only);

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
                        return Ok(());
                    }
                    Ok(()) if *shutdown.borrow() => {
                        tracing::info!("shutdown signal received; stop accepting");
                        return Ok(());
                    }
                    // Not our flip; keep serving.
                    Ok(()) => {}
                }
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, remote)) => {
                    let svc = svc.clone();
                    let rx = shutdown.clone();
                    let config = server_config.clone();
                    // Copy (not move) so the loop can spawn again.
                    let mode = default_mode;
                    in_flight.fetch_add(1, Ordering::Relaxed);
                    let pending = in_flight.clone();
                    tokio::spawn(async move {
                        let _ = stream.set_nodelay(true);
                        match accept_tls(stream, config).await {
                            Ok((tls, negotiated)) => {
                                h2c::serve_conn(
                                    svc,
                                    tls,
                                    remote,
                                    negotiated.unwrap_or(mode),
                                    rx,
                                    h2c::conn_timeouts(),
                                )
                                .await;
                            }
                            // Unknown SNI without a fallback, junk, or a
                            // scanner: one connection, not the listener.
                            Err(e) => {
                                tracing::debug!(
                                    error = %e,
                                    "TLS handshake failed"
                                );
                            }
                        }
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
}
