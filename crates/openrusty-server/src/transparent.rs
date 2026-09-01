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
//! 4. Outbound: the `[egress]` policy decides ([`crate::egress`]) -
//!    `direct` (default) dials the original destination and tunnels
//!    without any sniffing, `gateway` sniffs and forwards plaintext HTTP
//!    to the configured egress gateway, `deny` refuses everything. Every
//!    disposition increments `openrusty_transparent_conns_total`.
//!
//! Failure semantics: on an inbound socket an unrecoverable original
//! destination degrades to the plain (non-transparent) pipeline -
//! transparency is best-effort, service is not. On an outbound socket the
//! original destination *is* the dial target, so without it the connection
//! is closed. `http1_only` on a transparent inbound shapes only the
//! degraded path; the sniffed protocol decides the fast path. The routing
//! table itself is pure ([`route_pre`], [`route_http`]) and unit-tested.

pub(crate) mod prefixed;

use crate::h2c::{ProtoMode, serve_conn};
use crate::metrics::{Metrics, OUTCOME_HTTP, OUTCOME_LOOP_REJECTED, OUTCOME_NO_ORIG_DST, OUTCOME_TUNNEL};
use openrusty_core::config::{EgressConfig, ListenerConfig, ListenerRole};
use openrusty_proxy as proxy;
use prefixed::PrefixedStream;
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::shutdown::{self, InFlight};

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

/// Per-listener egress context shared by the intercept loop and every
/// handled connection: the `[egress]` policy, the pre-resolved gateway
/// address, and the disposition counters. Cheap to clone per connection.
#[derive(Clone)]
pub(crate) struct EgressPlane {
    pub(crate) egress: EgressConfig,
    pub(crate) gateway: Option<SocketAddr>,
    pub(crate) metrics: Arc<Metrics>,
}


/// Accept loop for a transparent listener: per connection, recover the
/// original destination and split by role and protocol. Mirrors the
/// shutdown handling of `h2c::serve_listener` (transient accept errors are
/// logged, never fatal). `own_ports` is every listener port, all roles
/// included; `in_flight` counts accepted connections for the drain
/// summary. `egress`/`gateway` steer intercepted outbound connections (see
/// `crate::egress`); `metrics` collects the per-disposition counters.
pub(crate) async fn serve_listener(
    router: axum::Router,
    cfg: ListenerConfig,
    plane: EgressPlane,
    listener: TcpListener,
    own_ports: Arc<[u16]>,
    mut shutdown: watch::Receiver<bool>,
    in_flight: InFlight,
) -> io::Result<()> {
    let addr = listener.local_addr()?;
    tracing::info!(
        role = cfg.role.as_str(),
        %addr,
        detect_timeout_ms = cfg.detect_timeout_ms,
        "listening (transparent intercept)"
    );
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
                    let cfg = cfg.clone();
                    let plane = plane.clone();
                    let ports = own_ports.clone();
                    in_flight.fetch_add(1, Ordering::Relaxed);
                    let pending = in_flight.clone();
                    tokio::spawn(async move {
                        handle_conn(svc, stream, remote, cfg, plane, ports, rx).await;
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

/// One intercepted connection: route it through the stage-1/stage-2
/// decisions above, counting the disposition per connection. Dropping the
/// stream at any point closes the connection. HTTP branches share the h2c
/// driver and drain on the unified signal; an established opaque tunnel
/// intentionally keeps serving until either peer closes - the shutdown
/// grace is what force-closes a stuck one.
async fn handle_conn<S>(
    svc: S,
    mut stream: TcpStream,
    remote: SocketAddr,
    cfg: ListenerConfig,
    plane: EgressPlane,
    own_ports: Arc<[u16]>,
    rx: watch::Receiver<bool>,
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
            let outcome = match reason {
                RejectReason::Loop => OUTCOME_LOOP_REJECTED,
                RejectReason::NoOriginalDst => OUTCOME_NO_ORIG_DST,
            };
            plane.metrics.record_transparent(cfg.role.as_str(), outcome);
            tracing::warn!(
                role = cfg.role.as_str(),
                %remote,
                ?reason,
                "transparent loop guard: original destination is an own listen port; \
                 closing connection"
            );
        }
        Step::DegradeHttp => {
            // Degraded or not, the connection ends up served as HTTP.
            plane.metrics.record_transparent(cfg.role.as_str(), OUTCOME_HTTP);
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
        // Egress is the outbound role's business ([egress] policy); the
        // inbound sniff branch is not.
        Step::Dial(dst) => {
            crate::egress::run_outbound(
                stream,
                dst,
                remote,
                &plane.egress,
                plane.gateway,
                Duration::from_millis(cfg.detect_timeout_ms),
                &plane.metrics,
            )
            .await;
        }
        Step::Sniff(dst) => {
            let budget = Duration::from_millis(cfg.detect_timeout_ms);
            match proxy::detect(&mut stream, budget).await {
                Ok((protocol, prefix)) => {
                    // The sniff only *borrowed* the leading bytes, so every
                    // branch must see them again: re-inject the prefix ahead
                    // of the live socket before the stream is consumed.
                    let io = PrefixedStream::new(prefix, stream);
                    match route_http(protocol) {
                        HttpStep::ServeHttp(mode) => {
                            plane.metrics.record_transparent(cfg.role.as_str(), OUTCOME_HTTP);
                            tracing::info!(
                                role = cfg.role.as_str(),
                                %remote,
                                %dst,
                                protocol = ?protocol,
                                "transparent HTTP intercepted; serving via pipeline"
                            );
                            serve_conn(svc, io, remote, mode, rx, timeouts).await;
                        }
                        HttpStep::Tunnel => {
                            plane.metrics.record_transparent(cfg.role.as_str(), OUTCOME_TUNNEL);
                            tunnel_to(io, dst, remote).await;
                        }
                    }
                }
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
/// the intercepted connection until both ends are done. `client` is the
/// (possibly prefix-reinjecting) client side of the intercepted connection.
/// Shared with the egress runtime (`direct` mode dials exactly this way).
pub(crate) async fn tunnel_to<S>(client: S, dst: SocketAddr, remote: SocketAddr)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
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
mod tests;
