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
use crate::state::AppState;
use crate::transparent;
use openrusty_core::config::{ListenerConfig, ListenerRole};
use std::sync::Arc;
use tokio::sync::broadcast;

/// One listener with its mounted router: the pure decision output of
/// [`mounts`], consumed by [`serve`].
pub struct Mount {
    pub listener: ListenerConfig,
    pub router: axum::Router,
}

/// Pure mounting decision: pair every effective listener with the router its
/// role serves. Deliberately side-effect free so the admin-plane placement
/// rule is unit-testable without sockets.
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

/// Bind every listener, then drive one accept task per bound socket until
/// `shutdown` fires. Bind failures propagate before any serving starts
/// (fail-fast); accept loops run until the shutdown broadcast reaches them.
pub async fn serve(mounts: Vec<Mount>, shutdown: broadcast::Receiver<()>) -> std::io::Result<()> {
    // Phase 1: bind everything up front. A failure here leaves nothing
    // serving, so the caller can exit without tearing down live sockets.
    let mut bound = Vec::with_capacity(mounts.len());
    for m in &mounts {
        let listener = tokio::net::TcpListener::bind(m.listener.listen).await?;
        tracing::info!(
            role = m.listener.role.as_str(),
            addr = %m.listener.listen,
            http1_only = m.listener.http1_only,
            transparent = m.listener.transparent,
            "listener bound"
        );
        bound.push(listener);
    }
    // Phase 2: one accept task per socket; all share the shutdown broadcast.
    // Transparent listeners get the intercept accept loop (detect router +
    // orig-dst tunnel), everyone else the plain HTTP pipeline.
    let ports = own_ports(mounts.iter().map(|m| &m.listener));
    let mut tasks = Vec::with_capacity(mounts.len());
    for (m, listener) in mounts.into_iter().zip(bound) {
        let rx = shutdown.resubscribe();
        let ports = ports.clone();
        let transparent = uses_transparent(&m.listener);
        tasks.push(tokio::spawn(async move {
            if transparent {
                transparent::serve_listener(m.router, m.listener, listener, ports, rx).await
            } else {
                h2c::serve_listener(
                    m.router,
                    listener,
                    m.listener.http1_only,
                    rx,
                    h2c::conn_timeouts(),
                )
                .await
            }
        }));
    }
    // Phase 3: drain. Every task ends only when shutdown fires (accept
    // errors are logged, never fatal), so awaiting them drains cleanly.
    for t in tasks {
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
        }
    }

    /// Serve the given listeners in the background on freshly reserved ports.
    /// Uses the prefix-only config so no route matches `/openrusty/*`: any
    /// admin path on a data socket then 404s via the pipeline's unmatched
    /// branch, proving the admin plane was detached (hermetic: no upstream
    /// stub needed, the request never leaves the proxy pipeline).
    async fn spawn(listeners: Vec<ListenerConfig>) -> (Vec<ListenerConfig>, broadcast::Sender<()>) {
        let dir = TmpDir::new("listeners");
        dir.write_config(&dir.prefix_only_config());
        let state = boot_state(&dir);
        let (tx, rx) = broadcast::channel::<()>(1);
        let mts = mounts(&state, &listeners);
        tokio::spawn(serve(mts, rx));
        (listeners, tx)
    }

    /// Raw HTTP/1.1 GET returning the status line + head, retrying until the
    /// listener accepts (the bind-then-release port reservation is racy).
    async fn get(port: u16, path: &str) -> String {
        let req = format!("GET {path} HTTP/1.1\r\nHost: gw\r\nConnection: close\r\n\r\n");
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
        let (ls, _tx) = spawn(vec![listener(ListenerRole::Inbound, free_port(), false)]).await;
        let port = ls[0].listen.port();
        let head = get(port, "/openrusty/status").await;
        assert!(
            head.starts_with("HTTP/1.1 200"),
            "admin plane missing on the merged data socket: {head}"
        );
    }

    #[tokio::test]
    async fn admin_listener_detaches_admin_plane_from_data_router() {
        let (ls, _tx) = spawn(vec![
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
        let (ls, _tx) = spawn(vec![
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
}
