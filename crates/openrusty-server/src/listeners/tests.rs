//! Unit tests for [`super`]: listener assembly and accept loops.
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
    raw(
        port,
        &format!("GET {path} HTTP/1.1\r\nHost: gw\r\nConnection: close\r\n\r\n"),
    )
    .await
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
    let (ls, _state, _tasks) =
        boot(vec![listener(ListenerRole::Inbound, free_port(), false)]).await;
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
    let (ls, state, tasks) = boot(vec![listener(ListenerRole::Inbound, free_port(), false)]).await;
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
