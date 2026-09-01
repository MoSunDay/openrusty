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
