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
//! Binding is fail-fast: every socket is bound up front and any bind error
//! aborts before a single request is served, matching the historical
//! single-listener behaviour where a failed bind exits the process.

use crate::app;
use crate::h2c;
use crate::metrics;
use crate::shutdown::ShutdownSignal;
use crate::state::AppState;
use crate::tls::{self, TlsPlan};
use crate::transparent;
use openrusty_core::config::{EgressConfig, ListenerConfig, ListenerRole};
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
    pub(crate) tls: Option<TlsPlan>,
    pub(crate) metrics: Arc<metrics::Metrics>,
    pub(crate) egress: EgressConfig,
    pub(crate) gateway: Option<SocketAddr>,
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
            tls: state.tls_resolver.as_ref().filter(|_| tls::uses_tls(l)).map(
                |resolver| TlsPlan {
                    resolver: resolver.clone(),
                    sources: tls::TlsSources::classify(
                        l,
                        state.static_config.ingress.enabled,
                    ),
                },
            ),
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
pub(crate) fn own_ports<'a>(
    listeners: impl IntoIterator<Item = &'a ListenerConfig>,
) -> Arc<[u16]> {
    let mut ports: Vec<u16> = listeners.into_iter().map(|l| l.listen.port()).collect();
    ports.sort_unstable();
    ports.dedup();
    ports.into()
}

/// Bind every listener, then spawn one accept task per bound socket.
///
/// Bind failures propagate before any serving starts (fail-fast); the
/// returned handles end when the unified shutdown signal fires (see
/// `shutdown::run`, which awaits them inside the grace window) - their
/// accepted connections drain through the existing per-connection graceful
/// shutdown. The signal is passed by reference so every accept task clones
/// the same `watch` receiver and shares one in-flight counter.
pub async fn spawn(
    mounts: Vec<Mount>,
    signal: &ShutdownSignal,
) -> std::io::Result<Vec<JoinHandle<std::io::Result<()>>>> {
    // Phase 1: bind everything up front, and read each TLS listener's
    // static certificate material exactly once (fail-fast: an unreadable
    // file aborts before a single request is served; reloads never
    // re-read it - the dynamic rotation path is ingress-driven).
    let mut bound = Vec::with_capacity(mounts.len());
    let mut tls_cfgs = Vec::with_capacity(mounts.len());
    for m in &mounts {
        let listener = tokio::net::TcpListener::bind(m.listener.listen).await?;
        let tls_cfg = match &m.tls {
            Some(plan) => Some(tls::boot(plan, m.listener.http1_only)?),
            None => None,
        };
        tracing::info!(
            role = m.listener.role.as_str(),
            addr = %m.listener.listen,
            http1_only = m.listener.http1_only,
            transparent = m.listener.transparent,
            tls = tls_cfg.is_some(),
            "listener bound"
        );
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
                    tls::serve_listener(m.router, m.listener, listener, config, rx, in_flight)
                        .await
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
mod tests {
    use super::*;
    use crate::testutil::{boot_state, TmpDir};
    use std::net::SocketAddr;

    /// The assembly selection rule: `transparent` only takes effect on data
    /// plane roles; the management plane parses but ignores the flag.
    #[test]
    fn transparent_flag_applies_only_to_data_plane_roles() {
        let mut admin = listener(ListenerRole::Admin, 4191, false);
        assert!(!uses_transparent(&admin));
        admin.transparent = true;
        assert!(!uses_transparent(&admin), "admin ignores transparent");

        for role in [ListenerRole::Inbound, ListenerRole::Outbound] {
            let mut l = listener(role, 4143, false);
            assert!(!uses_transparent(&l));
            l.transparent = true;
            assert!(uses_transparent(&l));
        }
    }

    /// Every listener port feeds the loop guard, deduplicated and sorted.
    #[test]
    fn own_ports_collect_all_listener_ports() {
        let ports = own_ports(&[
            listener(ListenerRole::Inbound, 4143, false),
            listener(ListenerRole::Outbound, 4140, true),
            listener(ListenerRole::Admin, 4191, false),
        ]);
        assert_eq!(&ports[..], &[4140, 4143, 4191]);
    }
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Reserve a free ephemeral port by bind-then-release (the gateway will
    /// re-bind it moments later; good enough for tests).
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn listener(role: ListenerRole, port: u16, http1_only: bool) -> ListenerConfig {
        ListenerConfig {
            role,
            listen: SocketAddr::from(([127, 0, 0, 1], port)),
            http1_only,
            transparent: false,
            detect_timeout_ms: 3_000,
            tls: false,
            tls_cert: None,
            tls_key: None,
        }
    }

    /// Serve the given listeners in the background on freshly reserved ports.
    /// Uses the prefix-only config so no route matches `/openrusty/*`: any
    /// admin path on a data socket then 404s via the pipeline's unmatched
    /// branch, proving the admin plane was detached (hermetic: no upstream
    /// stub needed, the request never leaves the proxy pipeline). Returns
    /// the accept-task handles so shutdown tests can drive the real
    /// three-phase sequence.
    async fn boot(
        listeners: Vec<ListenerConfig>,
    ) -> (
        Vec<ListenerConfig>,
        Arc<AppState>,
        Vec<JoinHandle<std::io::Result<()>>>,
    ) {
        let dir = TmpDir::new("listeners");
        dir.write_config(&dir.prefix_only_config());
        let state = boot_state(&dir);
        let mts = mounts(&state, &listeners);
        // The listener assembly shares the state's own signal, exactly like
        // main does - so an endpoint flip is visible to the accept tasks.
        let tasks = spawn(mts, &state.shutdown).await.unwrap();
        (listeners, state, tasks)
    }

    /// Raw HTTP/1.1 GET returning the status line + head, retrying until the
    /// listener accepts (the bind-then-release port reservation is racy).
    async fn get(port: u16, path: &str) -> String {
        raw(port, &format!("GET {path} HTTP/1.1\r\nHost: gw\r\nConnection: close\r\n\r\n")).await
    }

    /// Raw HTTP/1.1 POST with an empty body, same retry contract as [`get`].
    async fn post(port: u16, path: &str) -> String {
        raw(
            port,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: gw\r\nContent-Length: 0\r\n\
                 Connection: close\r\n\r\n"
            ),
        )
        .await
    }

    async fn raw(port: u16, req: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match tokio::net::TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port))).await {
                Ok(mut sock) => {
                    sock.write_all(req.as_bytes()).await.unwrap();
                    let mut buf = Vec::new();
                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        sock.read_to_end(&mut buf),
                    )
                    .await
                    .expect("response never completed")
                    .unwrap();
                    return String::from_utf8_lossy(&buf).into_owned();
                }
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => panic!("connect to {port} failed: {e}"),
            }
        }
    }

    #[tokio::test]
    async fn no_admin_listener_mounts_admin_plane_on_data_router() {
        let (ls, _state, _tasks) = boot(vec![listener(ListenerRole::Inbound, free_port(), false)]).await;
        let port = ls[0].listen.port();
        let head = get(port, "/openrusty/status").await;
        assert!(
            head.starts_with("HTTP/1.1 200"),
            "admin plane missing on the merged data socket: {head}"
        );
    }

    #[tokio::test]
    async fn admin_listener_detaches_admin_plane_from_data_router() {
        let (ls, _state, _tasks) = boot(vec![
            listener(ListenerRole::Inbound, free_port(), false),
            listener(ListenerRole::Outbound, free_port(), true),
            listener(ListenerRole::Admin, free_port(), false),
        ])
        .await;
        // Admin socket: management routes served.
        let head = get(ls[2].listen.port(), "/openrusty/status").await;
        assert!(
            head.starts_with("HTTP/1.1 200"),
            "admin router did not serve /openrusty/status: {head}"
        );
        // Inbound socket: admin plane removed; the request reaches the proxy
        // pipeline instead and 404s (no route matches).
        let head = get(ls[0].listen.port(), "/openrusty/status").await;
        assert!(
            head.starts_with("HTTP/1.1 404"),
            "admin plane still mounted on inbound: {head}"
        );
        // Outbound socket: same rule, and it honours http1_only (the h2c
        // preface is answered as a plain HTTP/1.1 parse failure).
        let head = get(ls[1].listen.port(), "/openrusty/status").await;
        assert!(
            head.starts_with("HTTP/1.1 404"),
            "admin plane still mounted on outbound: {head}"
        );
    }

    #[tokio::test]
    async fn data_plane_still_served_alongside_split_admin() {
        let (ls, _state, _tasks) = boot(vec![
            listener(ListenerRole::Inbound, free_port(), false),
            listener(ListenerRole::Admin, free_port(), false),
        ])
        .await;
        // A data request on inbound must fall through to the proxy pipeline
        // (404: no route matches /nowhere), proving the fallback survived.
        let head = get(ls[0].listen.port(), "/nowhere").await;
        assert!(
            head.starts_with("HTTP/1.1 404"),
            "data plane broken when admin is split out: {head}"
        );
    }

    /// The endpoint-triggered three-phase sequence over real sockets: a
    /// serving listener answers /openrusty/ready with 200, the shutdown
    /// POST flips the unified signal while still answering, the accept task
    /// then ends and new connections are refused, and `shutdown::run`
    /// finishes with a clean report - exactly what main does.
    #[tokio::test]
    async fn shutdown_endpoint_drives_the_three_phase_sequence() {
        let (ls, state, tasks) =
            boot(vec![listener(ListenerRole::Inbound, free_port(), false)]).await;
        let port = ls[0].listen.port();

        // Phase 0: serving. Readiness answers 200 on the merged socket.
        let head = get(port, "/openrusty/ready").await;
        assert!(head.starts_with("HTTP/1.1 200"), "ready pre: {head}");

        // Trigger: POST /openrusty/shutdown must answer first (the response
        // itself is in-flight and finishes through the drain), and the flip
        // must be visible on the shared signal.
        let head = post(port, "/openrusty/shutdown").await;
        assert!(head.starts_with("HTTP/1.1 200"), "shutdown post: {head}");
        assert!(head.contains("shutting down"), "shutdown post: {head}");
        assert!(
            crate::shutdown::is_draining(&state.shutdown.rx),
            "signal not flipped by the endpoint"
        );

        // Stop accepting: every fresh connect is eventually refused once
        // the accept task closed the socket.
        let refused = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if std::time::Instant::now() > refused {
                panic!("gateway still accepting after the shutdown flip");
            }
            match tokio::net::TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port))).await {
                Err(_) => break,
                Ok(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            }
        }

        // Phase 2/3: bounded drain ends cleanly with nothing force-closed.
        let report = crate::shutdown::run(
            state.shutdown.tx.clone(),
            tasks,
            state.shutdown.in_flight.clone(),
            std::time::Duration::from_secs(5),
        )
        .await;
        assert!(!report.timed_out, "drain hit the grace window: {report:?}");
        assert_eq!(report.task_errors, 0, "accept task failed: {report:?}");
    }
}
