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

/// An opaque intercepted connection must reach its original destination
/// byte-for-byte: `detect` only *borrows* the leading bytes, so the
/// client side handed to the tunnel must carry the re-injected prefix -
/// dropping it would corrupt e.g. a TLS ClientHello. The call site is
/// exercised end to end by `scripts/local-netns-test.sh` under a real
/// REDIRECT (a unit test cannot fake `SO_ORIGINAL_DST`).
#[tokio::test]
async fn tunnel_forwards_the_reinjected_prefix_verbatim() {
    use tokio::io::AsyncWriteExt;

    let dst = TcpListener::bind("127.0.0.1:0").await.expect("bind dst");
    let dst_addr = dst.local_addr().expect("dst addr");

    let local = TcpListener::bind("127.0.0.1:0").await.expect("bind local");
    let addr = local.local_addr().expect("local addr");
    let mut client = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (stream, remote) = local.accept().await.expect("accept");

    let prefix = bytes::Bytes::from_static(b"\x16\x03\x01 borrowed!");
    let task = tokio::spawn(tunnel_to(
        PrefixedStream::new(prefix, stream),
        dst_addr,
        remote,
    ));

    client.write_all(b"rest-of-stream").await.expect("write");
    client.shutdown().await.expect("half-close");
    drop(client);

    let (mut up, _) = dst.accept().await.expect("tunnel dialed dst");
    let mut got = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut up, &mut got)
        .await
        .expect("read tunneled bytes");
    assert_eq!(
        got, b"\x16\x03\x01 borrowed!rest-of-stream",
        "tunnel must deliver every byte, prefix included"
    );
    // The tunnel finishes only when both directions are done; close the
    // destination side so the b -> a copy sees its EOF.
    drop(up);
    task.await.expect("tunnel_to ends");
}

/// The transparent inbound plane must preserve the real client address for
/// `X-Forwarded-For`: `serve_conn` stamps the accept-side socket address
/// (`ConnectInfo`) into every request, and the proxy forward path appends
/// it to any client-supplied `X-Forwarded-For`. The sniffed prefix is
/// re-injected through [`PrefixedStream`], so this also proves the HTTP
/// intercept path sees an uncorrupted request under REDIRECT. The e2e twin
/// of this assertion lives in `scripts/local-netns-test.sh`.
#[tokio::test]
async fn transparent_http_injects_xff_from_the_accept_side_address() {
    use crate::h2c::conn_timeouts;
    use axum::response::Response;
    use openrusty_proxy as proxy;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Minimal HTTP/1 echo upstream: reflects the `X-Forwarded-For` header
    // it received back in the response body.
    let up = TcpListener::bind("127.0.0.1:0").await.expect("bind upstream");
    let up_addr = up.local_addr().expect("upstream addr");
    let echo = tokio::spawn(async move {
        let (mut sock, _) = up.accept().await.expect("upstream accept");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = sock.read(&mut chunk).await.expect("upstream read");
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || n == 0 {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf);
        let xff = head
            .lines()
            .find_map(|l| {
                let (name, value) = l.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("x-forwarded-for")
                    .then(|| value.trim().to_string())
            })
            .unwrap_or_default();
        let body = format!("xff={xff}");
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = sock.write_all(resp.as_bytes()).await;
    });

    // The "pipeline" under test: forwards through the real proxy path,
    // with the client IP taken from the ConnectInfo extension stamped by
    // `serve_conn` - exactly what the transparent HTTP branch serves.
    let pool = std::sync::Arc::new(proxy::new_pool());
    let svc = tower::service_fn(move |req: axum::extract::Request| {
        let pool = pool.clone();
        async move {
            let remote = req
                .extensions()
                .get::<axum::extract::ConnectInfo<SocketAddr>>()
                .expect("ConnectInfo stamped by serve_conn")
                .0;
            let (parts, body) = req.into_parts();
            let body = axum::body::to_bytes(body, 64 * 1024).await.expect("body");
            let headers = parts
                .headers
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();
            let fwd = proxy::ForwardRequest {
                method: parts.method.clone(),
                path_and_query: parts
                    .uri
                    .path_and_query()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_else(|| "/".to_string()),
                headers,
                body: body.clone(),
                client_ip: remote.ip().to_string(),
            };
            let client = proxy::get(&pool, up_addr, Duration::from_secs(2), Duration::from_secs(5));
            let peer = proxy::Peer {
                addr: up_addr,
                weight: 1,
            };
            let resp = proxy::forward(&client, &peer, &fwd).await.expect("forward");
            let status = resp.status();
            Ok(Response::builder()
                .status(status)
                .body(axum::body::Body::new(resp.into_body()))
                .expect("response"))
        }
    });

    let local = TcpListener::bind("127.0.0.1:0").await.expect("bind local");
    let addr = local.local_addr().expect("local addr");
    let mut client = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (stream, remote) = local.accept().await.expect("accept");

    // Split the request the way the sniffer does: `prefix` is the borrowed
    // leading bytes, the rest arrives after the sniff window.
    let prefix = bytes::Bytes::from_static(b"GET /xff HTTP/1.1\r\n");
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(serve_conn(
        svc,
        PrefixedStream::new(prefix, stream),
        remote,
        ProtoMode::ForceHttp1,
        rx,
        conn_timeouts(),
    ));

    client
        .write_all(b"host: t\r\nx-forwarded-for: 10.9.9.9\r\n\r\n")
        .await
        .expect("write request");

    // Read exactly one response (head + content-length body); the
    // connection is keep-alive, so no EOF ever comes until we hang up.
    let mut raw = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = client.read(&mut chunk).await.expect("read response");
        raw.extend_from_slice(&chunk[..n]);
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| p + 4);
        if let Some(head_end) = split {
            let head = String::from_utf8_lossy(&raw[..head_end]).to_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if raw.len() >= head_end + len {
                break;
            }
        }
        assert!(n > 0, "connection closed before a full response");
    }
    let raw = String::from_utf8_lossy(&raw);
    assert!(
        raw.contains("xff=10.9.9.9, 127.0.0.1"),
        "upstream must see the client hop appended from the accept-side \
         address, got: {raw}"
    );

    // Keep-alive: only dropping the client socket ends serve_conn.
    drop(client);
    drop(echo);
    task.await.expect("serve_conn ends");
}
