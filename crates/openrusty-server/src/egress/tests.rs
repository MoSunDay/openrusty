//! Tests for the egress decision matrix and the runtime dispositions.
//!
//! The runtime drills (denied connection closes, gateway fail-close,
//! verbatim gateway forwarding) use real loopback sockets; the pure matrix
//! is table-checked below.

use super::*;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn dst(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn cfg(mode: EgressMode) -> EgressConfig {
    EgressConfig {
        mode,
        gateway: String::new(),
    }
}

/// The full matrix: 3 modes x {None, H1, H2, Opaque} x {443, non-443}.
#[test]
fn route_egress_matrix() {
    let plain = dst(8080);
    let tls = dst(443);
    let http = [proxy::Protocol::H1, proxy::Protocol::H2];
    let all = http.iter().chain([&proxy::Protocol::Opaque]).copied();

    // direct: everything dials the original destination, always.
    for d in [plain, tls] {
        assert_eq!(route_egress(EgressMode::Direct, None, d), EgressStep::Orig(d));
        for p in all.clone() {
            assert_eq!(route_egress(EgressMode::Direct, Some(p), d), EgressStep::Orig(d));
        }
    }
    // deny: everything is refused, always.
    for d in [plain, tls] {
        assert_eq!(
            route_egress(EgressMode::Deny, None, d),
            EgressStep::Deny(EgressDenyReason::ModeDeny)
        );
        for p in all.clone() {
            assert_eq!(
                route_egress(EgressMode::Deny, Some(p), d),
                EgressStep::Deny(EgressDenyReason::ModeDeny)
            );
        }
    }
    // gateway without a protocol: sniff, whatever the port.
    for d in [plain, tls] {
        assert_eq!(route_egress(EgressMode::Gateway, None, d), EgressStep::Sniff(d));
    }
    // gateway + plaintext HTTP off 443: forward to the gateway.
    for p in http {
        assert_eq!(
            route_egress(EgressMode::Gateway, Some(p), plain),
            EgressStep::Gateway(plain)
        );
    }
    // gateway + plaintext HTTP aimed at 443: refused (v1 TLS boundary).
    for p in http {
        assert_eq!(
            route_egress(EgressMode::Gateway, Some(p), tls),
            EgressStep::Deny(EgressDenyReason::TlsPort)
        );
    }
    // gateway + opaque stream: refused regardless of port.
    for d in [plain, tls] {
        assert_eq!(
            route_egress(EgressMode::Gateway, Some(proxy::Protocol::Opaque), d),
            EgressStep::Deny(EgressDenyReason::Opaque)
        );
    }
}

async fn loopback_pair() -> (tokio::net::TcpStream, tokio::net::TcpStream) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    (server, client)
}

fn count<'a>(
    snap: &'a std::collections::HashMap<(String, String), u64>,
    outcome: &str,
) -> Option<&'a u64> {
    snap.get(&("outbound".to_string(), outcome.to_string()))
}

/// The refused connection must actually *close*: the peer reads EOF, and
/// exactly one `egress_deny` series appears.
#[tokio::test]
async fn deny_mode_closes_the_connection_and_counts() {
    let (io, mut peer) = loopback_pair().await;
    let m = Metrics::new();
    run_outbound(
        io,
        dst(9000),
        dst(40000),
        &cfg(EgressMode::Deny),
        None,
        Duration::from_secs(1),
        &m,
    )
    .await;
    assert_eq!(count(&m.snapshot().transparent, OUTCOME_EGRESS_DENY), Some(&1));
    let mut buf = [0u8; 16];
    assert_eq!(peer.read(&mut buf).await.unwrap(), 0, "peer must see EOF");
}

/// Direct mode keeps the historical disposition: tunnel to orig_dst (the
/// byte-level check lives in the transparent tests) and one `egress_direct`.
#[tokio::test]
async fn direct_mode_tunnels_and_counts() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let orig = listener.local_addr().unwrap();
    let (io, peer) = loopback_pair().await;
    let m = Arc::new(Metrics::new());
    let task_m = m.clone();
    let task = tokio::spawn(async move {
        run_outbound(
            io,
            orig,
            dst(40000),
            &cfg(EgressMode::Direct),
            None,
            Duration::from_secs(1),
            &m,
        )
        .await;
    });
    // The tunnel dials orig; accept and close both sides to finish it.
    let (up, _) = listener.accept().await.unwrap();
    drop(up);
    drop(peer);
    task.await.unwrap();
    assert_eq!(count(&task_m.snapshot().transparent, OUTCOME_EGRESS_DIRECT), Some(&1));
}

/// Gateway dial to a closed port: fail-close - peer EOF, one
/// `egress_gateway_fail`.
#[tokio::test]
async fn gateway_dial_failure_fail_closes() {
    // A bound-then-dropped listener yields a refused port.
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = dead.local_addr().unwrap();
    drop(dead);

    let (io, mut peer) = loopback_pair().await;
    let m = Metrics::new();
    let egress = EgressConfig {
        mode: EgressMode::Gateway,
        gateway: gateway.to_string(),
    };
    // Emit a real H1 request so the sniff succeeds and the Gateway step is
    // reached; only its dial fails.
    peer.write_all(b"GET /x HTTP/1.1\r\nhost: app\r\n\r\n").await.unwrap();
    run_outbound(
        io,
        dst(9000),
        dst(40000),
        &egress,
        Some(gateway),
        Duration::from_secs(1),
        &m,
    )
    .await;
    assert_eq!(count(&m.snapshot().transparent, OUTCOME_EGRESS_GATEWAY_FAIL), Some(&1));
    // Fail-close: the peer sees the connection torn down. A FIN (read 0) is
    // the tidy case; an RST also happens because the sniffed prefix stays
    // in the kernel queue and the socket is dropped with it undrained.
    let mut buf = [0u8; 16];
    match peer.read(&mut buf).await {
        Ok(0) => {}
        Ok(n) => panic!("peer must not receive data, got {n} bytes"),
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset),
    }
}

/// Gateway success: the stream reaches the gateway byte-for-byte (the
/// sniffed prefix included, split across two writes) and
/// `egress_gateway_ok` counts once.
#[tokio::test]
async fn gateway_success_forwards_bytes_verbatim() {
    let gw = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = gw.local_addr().unwrap();
    let (io, mut peer) = loopback_pair().await;
    let m = Arc::new(Metrics::new());
    let task_m = m.clone();
    let egress = EgressConfig {
        mode: EgressMode::Gateway,
        gateway: gateway.to_string(),
    };
    let task = tokio::spawn(async move {
        run_outbound(
            io,
            dst(9000),
            dst(40000),
            &egress,
            Some(gateway),
            Duration::from_secs(2),
            &m,
        )
        .await;
    });

    // Split the H1 request so the sniff sees only a borrowed prefix.
    peer.write_all(b"GE").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    peer.write_all(b"T /mesh HTTP/1.1\r\nhost: app\r\n\r\n").await.unwrap();
    peer.shutdown().await.unwrap();

    let (mut side, _) = gw.accept().await.unwrap();
    let mut got = Vec::new();
    side.read_to_end(&mut got).await.unwrap();
    assert_eq!(
        got, b"GET /mesh HTTP/1.1\r\nhost: app\r\n\r\n" as &[u8],
        "gateway must receive the stream verbatim, prefix included"
    );
    drop(side);
    task.await.unwrap();
    assert_eq!(count(&task_m.snapshot().transparent, OUTCOME_EGRESS_GATEWAY_OK), Some(&1));
}
