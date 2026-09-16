//! Egress policy for transparently intercepted outbound connections.
//!
//! The pure decision lives in [`route_egress`]; the per-connection runtime
//! (sniff-when-needed, gateway dial, disposition counters) is
//! [`run_outbound`]. Inbound and admin listeners never enter this module -
//! `[egress]` governs the outbound role only.
//!
//! # Decision matrix
//!
//! `protocol = None` is the state an outbound connection is in *before* any
//! sniffing (the historical path dialed without inspecting bytes at all):
//!
//! | mode      | protocol   | orig_dst port | step                    |
//! |-----------|------------|---------------|-------------------------|
//! | `direct`  | any / None | any           | [`EgressStep::Orig`]    |
//! | `deny`    | any / None | any           | [`EgressStep::Deny`]    |
//! | `gateway` | None       | any           | [`EgressStep::Sniff`]   |
//! | `gateway` | H1 / H2    | != 443        | [`EgressStep::Gateway`] |
//! | `gateway` | H1 / H2    | == 443        | `Deny(TlsPort)`         |
//! | `gateway` | Opaque     | any           | `Deny(Opaque)`          |
//!
//! Rationale for the two gateway-mode refusals:
//!
//! - **Opaque**: the gateway hop is plain TCP; an opaque stream (TLS
//!   ClientHello included) would hand the gateway an undecryptable byte
//!   stream it can only tunnel again - no routing, no policy, no plugins.
//!   The sidecar refuses it instead of pretending to proxy it.
//! - **Port 443**: carrying a *TLS* destination over the plaintext gateway
//!   hop is out of v1 scope. The iptables-init default already exempts 443,
//!   so reaching this verdict requires a deliberate redirect configuration.
//!
//! # Gateway byte semantics
//!
//! A [`EgressStep::Gateway`] verdict forwards the connection **verbatim**:
//! the sniff only borrowed the leading bytes, so the sniffed prefix is
//! re-injected ahead of the live socket (the same [`PrefixedStream`] the
//! inbound path uses) and the whole stream is shoveled to the gateway
//! untouched. No header is rewritten, no Host is touched: the gateway is
//! expected to be another openrusty listener (transparent or plain) whose
//! own routing decides what `Host` means. The drill is
//! `scripts/local-egress-test.sh`.
//!
//! # Failure semantics (fail-close)
//!
//! A gateway dial that fails or exceeds [`GATEWAY_CONNECT_TIMEOUT`] closes
//! the intercepted connection (`egress_gateway_fail`): egress policy must
//! never fall back to direct dialing, or a gateway outage would silently
//! downgrade the mesh to no-egress-control. The same holds for a gateway
//! address that never resolved at boot (`None` here): every gateway verdict
//! fail-closes.

use crate::metrics::{
    Metrics, OUTCOME_EGRESS_DENY, OUTCOME_EGRESS_DIRECT, OUTCOME_EGRESS_GATEWAY_FAIL,
    OUTCOME_EGRESS_GATEWAY_OK,
};
use crate::transparent::prefixed::PrefixedStream;
use openrusty_core::config::{EgressConfig, EgressMode, ListenerRole};
use openrusty_proxy as proxy;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

/// Budget for one egress gateway dial. Generous enough for a same-node
/// gateway, tight enough that a dead gateway fails the connection instead
/// of hanging the workload's `connect()` until its own timeout.
pub const GATEWAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// What an intercepted outbound connection should do. Pure verdict; the
/// runtime effects (counters, logs, dialing) live in [`run_outbound`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EgressStep {
    /// Tunnel verbatim to the original destination (`direct` mode).
    Orig(SocketAddr),
    /// `gateway` mode with no protocol yet: sniff the stream first, then
    /// call [`route_egress`] again with `Some(protocol)`.
    Sniff(SocketAddr),
    /// Forward the byte stream to the egress gateway. Payload is the
    /// original destination, for logging only (the gateway address travels
    /// separately, resolved once per listener).
    Gateway(SocketAddr),
    /// Refuse the connection; the reason goes into the structured log.
    Deny(EgressDenyReason),
}

/// Why an intercepted outbound connection is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EgressDenyReason {
    /// `[egress] mode = "deny"`: the sidecar serves locally only.
    ModeDeny,
    /// `gateway` mode, but the stream is not HTTP (see the module docs).
    Opaque,
    /// `gateway` mode, destination is TCP/443 (see the module docs).
    TlsPort,
}

impl EgressDenyReason {
    fn as_str(&self) -> &'static str {
        match self {
            EgressDenyReason::ModeDeny => "mode_deny",
            EgressDenyReason::Opaque => "opaque",
            EgressDenyReason::TlsPort => "tls_port",
        }
    }
}

/// Pure egress decision. Total over every (mode, protocol, orig_dst)
/// combination - see the module-doc matrix.
pub(crate) fn route_egress(
    mode: EgressMode,
    protocol: Option<proxy::Protocol>,
    orig_dst: SocketAddr,
) -> EgressStep {
    match mode {
        // Historical behaviour: dial the recovered destination and let the
        // bytes flow, no inspection at all.
        EgressMode::Direct => EgressStep::Orig(orig_dst),
        // Locked-down sidecar: nothing leaves the pod through interception.
        EgressMode::Deny => EgressStep::Deny(EgressDenyReason::ModeDeny),
        EgressMode::Gateway => match protocol {
            // Outbound connections are sniffed only on demand: the first
            // pass asks for the protocol, the caller re-asks with it set.
            None => EgressStep::Sniff(orig_dst),
            Some(proxy::Protocol::Opaque) => EgressStep::Deny(EgressDenyReason::Opaque),
            // Plaintext HTTP can be routed by the gateway - unless the
            // destination is TLS-only territory (v1 boundary).
            Some(_) if orig_dst.port() == 443 => EgressStep::Deny(EgressDenyReason::TlsPort),
            Some(_) => EgressStep::Gateway(orig_dst),
        },
    }
}

/// Handle one intercepted outbound connection end to end: decide, counter,
/// and either tunnel, forward to the gateway, or close. Dropping `io` at
/// any point closes the intercepted connection (fail-close).
///
/// `gateway` is the pre-resolved `[egress].gateway` address (resolved once
/// per listener at boot); `None` fail-closes every gateway verdict.
pub(crate) async fn run_outbound(
    mut io: TcpStream,
    orig_dst: SocketAddr,
    remote: SocketAddr,
    egress: &EgressConfig,
    gateway: Option<SocketAddr>,
    detect_budget: Duration,
    metrics: &Metrics,
) {
    let role = ListenerRole::Outbound.as_str();
    match route_egress(egress.mode, None, orig_dst) {
        EgressStep::Orig(dst) => {
            metrics.record_transparent(role, OUTCOME_EGRESS_DIRECT);
            crate::transparent::tunnel_to(io, dst, remote).await;
        }
        EgressStep::Deny(reason) => {
            metrics.record_transparent(role, OUTCOME_EGRESS_DENY);
            deny_log(orig_dst, remote, reason);
            // Returning drops `io`: the connection closes.
        }
        EgressStep::Sniff(dst) => match proxy::detect(&mut io, detect_budget).await {
            Ok((protocol, prefix)) => {
                // The sniff only borrowed the leading bytes; re-inject them
                // so the gateway sees the stream byte-for-byte.
                let io = PrefixedStream::new(prefix, io);
                match route_egress(egress.mode, Some(protocol), dst) {
                    EgressStep::Gateway(_) => {
                        forward_to_gateway(io, dst, remote, gateway, metrics).await
                    }
                    EgressStep::Deny(reason) => {
                        metrics.record_transparent(role, OUTCOME_EGRESS_DENY);
                        deny_log(dst, remote, reason);
                    }
                    // A gateway re-route with a protocol set never asks for
                    // direct dialing or another sniff (see the matrix).
                    other => unreachable!("gateway re-route produced {other:?}"),
                }
            }
            // EOF before the prefix settled: the peer gave up first.
            Err(e) => {
                tracing::debug!(
                    role,
                    %remote,
                    %dst,
                    error = %e,
                    "egress protocol detection failed; closing connection"
                );
            }
        },
        // `route_egress` only emits `Gateway` when the protocol is known,
        // which never happens on the first (protocol-free) pass.
        other => unreachable!("first-pass egress decision produced {other:?}"),
    }
}

/// Fail-close refusal: log it. The caller drops the connection.
fn deny_log(orig_dst: SocketAddr, remote: SocketAddr, reason: EgressDenyReason) {
    tracing::warn!(
        role = ListenerRole::Outbound.as_str(),
        %remote,
        %orig_dst,
        reason = reason.as_str(),
        "egress denied by policy; closing connection"
    );
}

/// Dial `[egress].gateway` and shovel the intercepted stream to it
/// verbatim. A dial failure or timeout fail-closes the connection.
async fn forward_to_gateway<S>(
    io: S,
    orig_dst: SocketAddr,
    remote: SocketAddr,
    gateway: Option<SocketAddr>,
    metrics: &Metrics,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let role = ListenerRole::Outbound.as_str();
    let Some(gateway) = gateway else {
        // Configuration slipped through unvalidated: fail closed, loudly.
        metrics.record_transparent(role, OUTCOME_EGRESS_GATEWAY_FAIL);
        tracing::error!(%remote, %orig_dst, "egress gateway address unresolved; closing connection");
        return;
    };
    let dial = tokio::time::timeout(GATEWAY_CONNECT_TIMEOUT, TcpStream::connect(gateway));
    match dial.await {
        Ok(Ok(upstream)) => {
            let _ = upstream.set_nodelay(true);
            metrics.record_transparent(role, OUTCOME_EGRESS_GATEWAY_OK);
            tracing::info!(
                role,
                %remote,
                %orig_dst,
                gateway = %gateway,
                "egress gateway connection established"
            );
            match proxy::tcp_tunnel(io, upstream).await {
                Ok((up, down)) => {
                    tracing::debug!(gateway = %gateway, up, down, "egress gateway tunnel finished");
                }
                Err(e) => {
                    tracing::warn!(gateway = %gateway, error = %e, "egress gateway tunnel aborted");
                }
            }
        }
        Ok(Err(e)) => {
            metrics.record_transparent(role, OUTCOME_EGRESS_GATEWAY_FAIL);
            tracing::warn!(
                role,
                %remote,
                %orig_dst,
                gateway = %gateway,
                error = %e,
                "egress gateway dial failed; closing connection (fail-close)"
            );
        }
        Err(_) => {
            metrics.record_transparent(role, OUTCOME_EGRESS_GATEWAY_FAIL);
            tracing::warn!(
                role,
                %remote,
                %orig_dst,
                gateway = %gateway,
                timeout_ms = GATEWAY_CONNECT_TIMEOUT.as_millis() as u64,
                "egress gateway dial timed out; closing connection (fail-close)"
            );
        }
    }
}

#[cfg(test)]
mod tests;
