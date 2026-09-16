//! Role-based multi-listener assembly.
//!
//! The gateway binds one socket per effective `[[server.listeners]]` entry
//! ([`openrusty_core::config::effective_listeners`] derives them; empty
//! `[[server.listeners]]` yields the historical single inbound socket). Each role
//! serves a dedicated router:
//!
//! - `admin` -> [`app::admin_router`]: `/openrusty/*` only,
//! - `inbound`/`outbound` -> the data plane; the admin plane stays mounted
//!   on it only while no dedicated admin listener exists (see
//!   [`app::admin_routes`] for the mounting rule).
//!
//! Sockets are acquired inherit-or-bind, fail-fast: when the process is
//! socket-activated (`LISTEN_FDS`/`LISTEN_PID`, e.g. a systemd
//! `openrusty.socket` unit or a future exec-based upgrade), the inherited
//! listener fds are adopted instead of binding - [`sd_listen::adopt`]
//! port-matches them against the configured addresses first - otherwise
//! every socket is bound up front. Either way, any acquisition error
//! aborts before a single request is served, matching the historical
//! single-listener behaviour where a failed bind exits the process.

use crate::app;
use crate::h2c;
use crate::metrics;
use crate::sd_listen;
use crate::shutdown::ShutdownSignal;
use crate::state::AppState;
use crate::tls::{self, TlsPlan};
use crate::transparent;
use openrusty_core::config::{EgressConfig, ListenerConfig, ListenerRole};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::task::JoinHandle;

/// One listener with its mounted router: the pure decision output of
/// [`mounts`], consumed by [`serve`]. `tls` carries the TLS plan (shared
/// resolver + classified certificate sources) for `tls = true` listeners;
/// `None` elsewhere. `metrics`/`egress` feed the transparent intercept
/// loop (disposition counters + `[egress]` policy); they are inert for
/// plain and TLS listeners.
pub struct Mount {
    pub listener: ListenerConfig,
    pub router: axum::Router,
    pub tls: Option<TlsPlan>,
    pub metrics: Arc<metrics::Metrics>,
    pub egress: EgressConfig,
    pub gateway: Option<SocketAddr>,
}

/// Pure mounting decision: pair every effective listener with the router its
/// role serves (and, when the listener terminates TLS, with the state's
/// shared SNI resolver). Deliberately side-effect free so the admin-plane
/// placement rule is unit-testable without sockets.
pub fn mounts(state: &Arc<AppState>, listeners: &[ListenerConfig]) -> Vec<Mount> {
    let has_admin = listeners.iter().any(|l| l.role == ListenerRole::Admin);
    listeners
        .iter()
        .map(|l| Mount {
            router: match l.role {
                ListenerRole::Admin => app::admin_router(state.clone()),
                ListenerRole::Inbound | ListenerRole::Outbound => {
                    if has_admin {
                        app::data_router(state.clone())
                    } else {
                        app::router(state.clone())
                    }
                }
            },
            listener: l.clone(),
            tls: state
                .tls_resolver
                .as_ref()
                .filter(|_| tls::uses_tls(l))
                .map(|resolver| TlsPlan {
                    resolver: resolver.clone(),
                    sources: tls::TlsSources::classify(l, state.static_config.ingress.enabled),
                }),
            metrics: state.metrics.clone(),
            egress: state.static_config.egress.clone(),
            gateway: openrusty_core::config::resolve_gateway(&state.static_config.egress),
        })
        .collect()
}

/// True when a listener must run the transparent intercept path: the config
/// declares a REDIRECT in front of the socket and the role carries
/// data-plane traffic. An admin listener ignores `transparent` by design
/// (see `config::listeners`); the assembly layer is where the flag is
/// dropped, so the semantics live in exactly one place.
pub(crate) fn uses_transparent(l: &ListenerConfig) -> bool {
    l.transparent && !matches!(l.role, ListenerRole::Admin)
}

/// Ports of every effective listener, all roles included. The transparent
/// loop guard compares against this whole set: a hijacked connection aimed
/// at *any* of our sockets would recurse if tunneled onward.
pub(crate) fn own_ports<'a>(listeners: impl IntoIterator<Item = &'a ListenerConfig>) -> Arc<[u16]> {
    let mut ports: Vec<u16> = listeners.into_iter().map(|l| l.listen.port()).collect();
    ports.sort_unstable();
    ports.dedup();
    ports.into()
}

/// Inherit or bind every listener, then spawn one accept task per socket.
///
/// Socket acquisition failures (a socket-activation mismatch or a bind
/// error) propagate before any serving starts (fail-fast); the returned
/// handles end when the unified shutdown signal fires (see
/// `shutdown::run`, which awaits them inside the grace window) - their
/// accepted connections drain through the existing per-connection graceful
/// shutdown. The signal is passed by reference so every accept task clones
/// the same `watch` receiver and shares one in-flight counter.
pub async fn spawn(
    mounts: Vec<Mount>,
    signal: &ShutdownSignal,
) -> std::io::Result<Vec<JoinHandle<std::io::Result<()>>>> {
    // Phase 1: acquire every socket up front, and read each TLS listener's
    // static certificate material exactly once (fail-fast: an unreadable
    // file aborts before a single request is served; reloads never
    // re-read it - the dynamic rotation path is ingress-driven). Under
    // socket activation the whole set is inherited (adopt() ports-matches
    // the fds to the configured addresses, in order, or fails); adoption
    // is all-or-nothing, so the queue either empties across this loop or
    // was never created.
    let expected: Vec<SocketAddr> = mounts.iter().map(|m| m.listener.listen).collect();
    let mut inherited = sd_listen::adopt(&expected)?
        .map(|listeners| listeners.into_iter().collect::<VecDeque<_>>());
    let mut bound = Vec::with_capacity(mounts.len());
    let mut tls_cfgs = Vec::with_capacity(mounts.len());
    for m in &mounts {
        let inherited_listener = inherited.as_mut().and_then(|queue| queue.pop_front());
        let from_activation = inherited_listener.is_some();
        let listener = match inherited_listener {
            Some(l) => l,
            None => tokio::net::TcpListener::bind(m.listener.listen).await?,
        };
        let tls_cfg = match &m.tls {
            Some(plan) => Some(tls::boot(plan, m.listener.http1_only)?),
            None => None,
        };
        if from_activation {
            tracing::info!(
                role = m.listener.role.as_str(),
                addr = %m.listener.listen,
                http1_only = m.listener.http1_only,
                transparent = m.listener.transparent,
                tls = tls_cfg.is_some(),
                "listener inherited (socket activation)"
            );
        } else {
            tracing::info!(
                role = m.listener.role.as_str(),
                addr = %m.listener.listen,
                http1_only = m.listener.http1_only,
                transparent = m.listener.transparent,
                tls = tls_cfg.is_some(),
                "listener bound"
            );
        }
        bound.push(listener);
        tls_cfgs.push(tls_cfg);
    }
    // Phase 2: one accept task per socket; all share the shutdown signal
    // and the in-flight counter. Dispatch per shape: TLS termination
    // (handshake + ALPN, then the shared pipeline), the transparent
    // intercept loop (detect router + orig-dst tunnel), or the plain
    // HTTP pipeline.
    let ports = own_ports(mounts.iter().map(|m| &m.listener));
    let mut tasks = Vec::with_capacity(mounts.len());
    for ((m, listener), tls_cfg) in mounts.into_iter().zip(bound).zip(tls_cfgs) {
        let rx = signal.rx.clone();
        let in_flight = signal.in_flight.clone();
        let ports = ports.clone();
        let transparent = uses_transparent(&m.listener);
        tasks.push(tokio::spawn(async move {
            match tls_cfg {
                Some(config) => {
                    tls::serve_listener(m.router, m.listener, listener, config, rx, in_flight).await
                }
                None if transparent => {
                    transparent::serve_listener(
                        m.router,
                        m.listener,
                        crate::transparent::EgressPlane {
                            egress: m.egress,
                            gateway: m.gateway,
                            metrics: m.metrics,
                        },
                        listener,
                        ports,
                        rx,
                        in_flight,
                    )
                    .await
                }
                None => {
                    h2c::serve_listener(
                        m.router,
                        listener,
                        m.listener.http1_only,
                        rx,
                        h2c::conn_timeouts(),
                        in_flight,
                    )
                    .await
                }
            }
        }));
    }
    Ok(tasks)
}

/// Drive every listener to completion: bind, spawn the accept tasks, then
/// await them until the shutdown signal ends each loop. Embedder-facing
/// convenience over [`spawn`] - the binary uses `spawn` directly so it can
/// own the three-phase sequence (`shutdown::run`).
pub async fn serve(mounts: Vec<Mount>, signal: &ShutdownSignal) -> std::io::Result<()> {
    for t in spawn(mounts, signal).await? {
        t.await.map_err(|e| {
            if e.is_panic() {
                std::io::Error::other("listener accept task panicked")
            } else {
                std::io::Error::other("listener accept task cancelled")
            }
        })??;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
