//! Transparent interception: protocol split for redirected connections.
//!
//! In the sidecar (transparent) form an iptables/nftables REDIRECT rule
//! hijacks the workload's connections into a gateway listener. Such a
//! socket carries an arbitrary byte stream aimed at the *original*
//! destination, so before the HTTP pipeline can run, the gateway must
//! recover that destination and decide what it is looking at.
//! [`serve_listener`] implements the split for listeners configured with
//! `transparent = true`:
//!
//! 1. `proxy::original_dst` recovers the pre-NAT destination (conntrack).
//! 2. The loop guard refuses connections whose destination is one of the
//!    gateway's own listen ports. This is fail-fast by design: a
//!    transparent listener *without* a REDIRECT in front of it sees every
//!    conntrack answer equal to its own port (tracked-but-un-NATed
//!    connections report the real destination), so misconfiguration is
//!    rejected loudly on every connection instead of silently proxied.
//! 3. Inbound: `proxy::detect` sniffs H1 / H2 / Opaque. HTTP goes through
//!    the normal pipeline (plugins apply as usual) over a
//!    [`PrefixedStream`] that re-injects the sniffed bytes, so nothing the
//!    probe consumed is lost; opaque streams are tunneled verbatim to the
//!    original destination.
//! 4. Outbound: no sniffing - egress is dialed at the original destination
//!    and tunneled (egress HTTP semantics are a later milestone).
//!
//! Failure semantics: on an inbound socket an unrecoverable original
//! destination degrades to the plain (non-transparent) pipeline -
//! transparency is best-effort, service is not. On an outbound socket the
//! original destination *is* the dial target, so without it the connection
//! is closed. `http1_only` on a transparent inbound shapes only the
//! degraded path; the sniffed protocol decides the fast path. The routing
//! table itself is pure ([`route_pre`], [`route_http`]) and unit-tested.

mod prefixed;

use crate::h2c::{ProtoMode, serve_conn};
use openrusty_core::config::{ListenerConfig, ListenerRole};
use openrusty_proxy as proxy;
use prefixed::PrefixedStream;
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

/// Why a transparently intercepted connection is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RejectReason {
    /// The recovered destination is one of our own listen ports: serving it
    /// would recurse into the gateway (or the listener has no REDIRECT in
    /// front of it - see the module docs).
    Loop,
    /// Outbound socket with no recoverable original destination: there is
    /// nowhere to dial.
    NoOriginalDst,
}

/// Stage-1 verdict for an intercepted connection: what to do once the
/// original destination is known (or known to be unrecoverable). Pure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Inbound without orig-dst: serve the plain pipeline (auto sniff).
    DegradeHttp,
    /// Inbound with a usable destination: sniff the protocol next.
    Sniff(SocketAddr),
    /// Egress: tunnel verbatim to the original destination.
    Dial(SocketAddr),
    /// Refuse the connection; the reason goes into the structured log.
    Reject(RejectReason),
}

/// Pure stage-1 routing decision. `orig_dst` is `None` when the conntrack
/// lookup failed; `own_ports` is the set of every listener port (all roles).
pub(crate) fn route_pre(
    role: ListenerRole,
    orig_dst: Option<SocketAddr>,
    own_ports: &[u16],
) -> Step {
    match orig_dst {
        Some(dst) if proxy::is_loopback(dst, own_ports) => Step::Reject(RejectReason::Loop),
        Some(dst) if role == ListenerRole::Outbound => Step::Dial(dst),
        // Admin sockets never reach this path (transparent is ignored on the
        // management plane); they degrade like inbound for totality.
        Some(dst) => Step::Sniff(dst),
        None if role == ListenerRole::Outbound => Step::Reject(RejectReason::NoOriginalDst),
        None => Step::DegradeHttp,
    }
}

/// Stage-2 verdict for an inbound connection whose sniff settled. Pure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HttpStep {
    /// HTTP traffic: serve through the pipeline, protocol forced.
    ServeHttp(ProtoMode),
    /// Opaque byte stream: tunnel to the original destination.
    Tunnel,
}

/// Pure stage-2 routing decision: the sniffed protocol picks the serving
/// mode, and anything the gateway does not speak is tunneled untouched.
pub(crate) fn route_http(protocol: proxy::Protocol) -> HttpStep {
    match protocol {
        proxy::Protocol::H1 => HttpStep::ServeHttp(ProtoMode::ForceHttp1),
        proxy::Protocol::H2 => HttpStep::ServeHttp(ProtoMode::ForceHttp2),
        proxy::Protocol::Opaque => HttpStep::Tunnel,
    }
}

/// Accept loop for a transparent listener: per connection, recover the
/// original destination and split by protocol. Mirrors the shutdown
/// handling of `h2c::serve_listener` (transient accept errors are logged,
/// never fatal). `own_ports` is every listener port, all roles included.
pub(crate) async fn serve_listener(
    router: axum::Router,
    cfg: ListenerConfig,
    listener: TcpListener,
    own_ports: Arc<[u16]>,
    mut shutdown: broadcast::Receiver<()>,
) -> io::Result<()> {
    let addr = listener.local_addr()?;
    tracing::info!(
        role = cfg.role.as_str(),
        %addr,
        detect_timeout_ms = cfg.detect_timeout_ms,
        "listening (transparent intercept)"
    );
    let svc = router.into_service::<axum::body::Body>();
    loop {
        tokio::select! {
            sig = shutdown.recv() => {
                match sig {
                    Ok(()) | Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("shutdown signal received; stop accepting");
                        return Ok(());
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
                    let cfg = cfg.clone();
                    let ports = own_ports.clone();
                    tokio::spawn(async move {
                        handle_conn(svc, stream, remote, cfg, ports, rx).await;
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

/// One intercepted connection: route it through the stage-1/stage-2
/// decisions above. Dropping the stream at any point closes the connection.
async fn handle_conn<S>(
    svc: S,
    mut stream: TcpStream,
    remote: SocketAddr,
    cfg: ListenerConfig,
    own_ports: Arc<[u16]>,
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
    let timeouts = crate::h2c::conn_timeouts();
    let orig = proxy::original_dst(&stream).ok();
    match route_pre(cfg.role, orig, &own_ports) {
        Step::Reject(reason) => {
            tracing::warn!(
                role = cfg.role.as_str(),
                %remote,
                ?reason,
                "transparent loop guard: original destination is an own listen port; \
                 closing connection"
            );
        }
        Step::DegradeHttp => {
            tracing::warn!(
                role = cfg.role.as_str(),
                %remote,
                "orig_dst unavailable; serving plain HTTP (transparent degraded)"
            );
            serve_conn(
                svc,
                stream,
                remote,
                ProtoMode::from(cfg.http1_only),
                rx,
                timeouts,
            )
            .await;
        }
        Step::Dial(dst) => tunnel_to(stream, dst, remote).await,
        Step::Sniff(dst) => {
            let budget = Duration::from_millis(cfg.detect_timeout_ms);
            match proxy::detect(&mut stream, budget).await {
                Ok((protocol, prefix)) => match route_http(protocol) {
                    HttpStep::ServeHttp(mode) => {
                        let io = PrefixedStream::new(prefix, stream);
                        serve_conn(svc, io, remote, mode, rx, timeouts).await;
                    }
                    HttpStep::Tunnel => tunnel_to(stream, dst, remote).await,
                },
                // EOF before the prefix settled: the peer gave up first.
                Err(e) => {
                    tracing::debug!(
                        role = cfg.role.as_str(),
                        %remote,
                        error = %e,
                        "protocol detection failed; closing connection"
                    );
                }
            }
        }
    }
}

/// Dials the original destination and shovels bytes verbatim between it and
/// the intercepted connection until both ends are done.
async fn tunnel_to(client: TcpStream, dst: SocketAddr, remote: SocketAddr) {
    match TcpStream::connect(dst).await {
        Ok(upstream) => {
            let _ = upstream.set_nodelay(true);
            tracing::info!(%dst, %remote, "opaque tunnel established");
            match proxy::tcp_tunnel(client, upstream).await {
                Ok((up, down)) => {
                    tracing::debug!(%dst, up, down, "opaque tunnel finished");
                }
                Err(e) => {
                    tracing::warn!(%dst, error = %e, "opaque tunnel aborted");
                }
            }
        }
        // Dropping `client` closes the intercepted connection.
        Err(e) => {
            tracing::warn!(%dst, %remote, error = %e, "opaque tunnel dial failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ports() -> Vec<u16> {
        vec![4143, 4140, 4191]
    }

    #[test]
    fn outbound_without_orig_dst_is_closed() {
        let step = route_pre(ListenerRole::Outbound, None, &ports());
        assert_eq!(step, Step::Reject(RejectReason::NoOriginalDst));
    }

    #[test]
    fn inbound_without_orig_dst_degrades_to_plain_http() {
        assert_eq!(
            route_pre(ListenerRole::Inbound, None, &ports()),
            Step::DegradeHttp
        );
    }

    #[test]
    fn loop_guard_rejects_own_ports_for_every_role() {
        let dst: SocketAddr = "127.0.0.1:4143".parse().unwrap();
        for role in [
            ListenerRole::Inbound,
            ListenerRole::Outbound,
            ListenerRole::Admin,
        ] {
            assert_eq!(
                route_pre(role, Some(dst), &ports()),
                Step::Reject(RejectReason::Loop)
            );
        }
        // A different port (even on the loopback address) is not a loop.
        let dst: SocketAddr = "127.0.0.1:9001".parse().unwrap();
        assert_eq!(
            route_pre(ListenerRole::Inbound, Some(dst), &ports()),
            Step::Sniff(dst)
        );
    }

    #[test]
    fn known_destination_sniffs_inbound_and_dials_outbound() {
        let dst: SocketAddr = "10.0.0.7:8080".parse().unwrap();
        assert_eq!(
            route_pre(ListenerRole::Inbound, Some(dst), &ports()),
            Step::Sniff(dst)
        );
        assert_eq!(
            route_pre(ListenerRole::Outbound, Some(dst), &ports()),
            Step::Dial(dst)
        );
    }

    #[test]
    fn sniff_result_selects_the_serving_mode() {
        assert_eq!(
            route_http(proxy::Protocol::H1),
            HttpStep::ServeHttp(ProtoMode::ForceHttp1)
        );
        assert_eq!(
            route_http(proxy::Protocol::H2),
            HttpStep::ServeHttp(ProtoMode::ForceHttp2)
        );
        assert_eq!(route_http(proxy::Protocol::Opaque), HttpStep::Tunnel);
    }
}
