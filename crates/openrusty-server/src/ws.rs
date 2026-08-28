//! WebSocket pass-through: run the balancer phase, forward the upgrade
//! handshake to the chosen peer (retrying other healthy peers like the
//! proxy path, each attempt bounded by the route timeout), then tunnel raw
//! bytes both ways. The log phase runs when the tunnel closes (or the
//! handshake fails).

use crate::pipeline::{finish_log, pick_peer, text_response, Pick};
use crate::state::{AppState, UpstreamRt};
use axum::body::Body;
use axum::response::Response;
use hyper::header::{HeaderName, HeaderValue, CONNECTION, HOST, UPGRADE};
use openrusty_core::phase::Phase;
use openrusty_proxy as proxy;
use openrusty_wasm::host_state::now_ms;
use openrusty_wasm::RequestSession;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `Connection` value for the re-issued handshake.
const CONNECTION_UPGRADE: &str = "upgrade";
/// `Upgrade` value for the re-issued handshake.
const UPGRADE_WEBSOCKET: &str = "websocket";

/// Build the outbound handshake headers from the client request headers.
///
/// Header policy (reusing the forwarding policy from `openrusty_proxy`):
/// hop-by-hop headers (`Connection`, `Upgrade`, `Te`, `Keep-Alive`,
/// `Transfer-Encoding`, ...) are stripped with the SAME list as normal
/// forwarding, `X-Forwarded-For` is MERGED with the client hop instead of
/// overwritten, end-to-end headers (`Authorization`, `Cookie`, ...) pass
/// through untouched, and `Host` is rebuilt for the peer. The upgrade is
/// re-issued with canonical `Connection: upgrade` / `Upgrade: websocket`
/// values because the stripped client values are not trustworthy.
pub(crate) fn handshake_headers(
    client_headers: &[(String, String)],
    peer_addr: &str,
    client_ip: &str,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = proxy::strip_hop_by_hop(client_headers)
        .into_iter()
        .filter(|(name, _)| {
            !(name.eq_ignore_ascii_case("host") || name.eq_ignore_ascii_case("x-forwarded-for"))
        })
        .collect();
    out.push((HOST.as_str().to_string(), peer_addr.to_string()));
    out.push((
        CONNECTION.as_str().to_string(),
        CONNECTION_UPGRADE.to_string(),
    ));
    out.push((
        UPGRADE.as_str().to_string(),
        UPGRADE_WEBSOCKET.to_string(),
    ));
    out.push((
        "x-forwarded-for".to_string(),
        proxy::merge_xff(client_headers, client_ip),
    ));
    out
}

/// Proxy one WebSocket upgrade request. Takes ownership of the session:
/// on the success path it is moved into the tunnel task, which runs the
/// log phase after the connection closes.
///
/// `timeout` (from the matched route) bounds each handshake attempt.
pub async fn proxy_websocket(
    state: Arc<AppState>,
    mut session: RequestSession,
    up_rt: Arc<UpstreamRt>,
    mut req: axum::extract::Request,
    timeout: Option<Duration>,
) -> Response {
    // h2c connections have no HTTP/1-style upgrade extension.
    if req.version() == hyper::Version::HTTP_2 {
        finish_log(&mut session, 502);
        return text_response(502, "502 websocket requires http/1.1\n");
    }
    // Extracted while req is still intact; resolves once we answer 101.
    let on_upgrade = hyper::upgrade::on(&mut req);

    // Build the outbound upgrade request.
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());
    let client_headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_string(), s.to_string()))
        })
        .collect();
    let client_ip = session.ctx().client_addr.ip().to_string();

    // Peer attempts: the same healthy/tried-aware pick as the proxy retry
    // loop. A failed peer is marked tried so it is never picked twice; the
    // walk stops when no untried healthy peer is left or the route timeout
    // budget for each attempt runs out. Every failure path returns early,
    // so the loop only ever `break`s with a handshake response in hand.
    let (mut out_resp, picked_idx) = loop {
        session.ctx().peer_index = None;
        let idx = match pick_peer(&state, &up_rt, &mut session) {
            Pick::Peer(i) => i,
            Pick::Deny(s) => {
                finish_log(&mut session, s);
                return text_response(s, format!("{s}\n"));
            }
            Pick::None => {
                finish_log(&mut session, 502);
                return text_response(502, "502 no healthy upstream\n");
            }
        };
        session.ctx().attempts += 1;
        session.ctx().peer_index = Some(idx as u32);
        let peer = up_rt.up.peers[idx];

        let mut out = hyper::Request::builder()
            .method(req.method().clone())
            .uri(format!("http://{}{}", peer.addr, path_and_query));
        let Some(headers) = out.headers_mut() else {
            finish_log(&mut session, 502);
            return text_response(502, "502 bad request\n");
        };
        for (name, value) in
            handshake_headers(&client_headers, &peer.addr.to_string(), &client_ip)
        {
            let (Ok(hn), Ok(hv)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(&value),
            ) else {
                continue;
            };
            headers.append(hn, hv);
        }
        let outbound = match out.body(http_body_util::Full::new(bytes::Bytes::new())) {
            Ok(r) => r,
            Err(_) => {
                finish_log(&mut session, 502);
                return text_response(502, "502 bad request\n");
            }
        };

        let client = proxy::get(
            &state.pool,
            peer.addr,
            up_rt.up.connect_timeout,
            up_rt.up.pool_idle_timeout,
        );
        let fut = client.request(outbound);
        let outcome = match timeout {
            Some(t) => tokio::time::timeout(t, fut).await.ok(),
            None => Some(fut.await),
        };
        match outcome {
            Some(Ok(r)) => break (r, idx),
            Some(Err(e)) => {
                proxy::record_failure(
                    &state.health,
                    &up_rt.up.name,
                    idx,
                    &up_rt.up.health,
                    now_ms(),
                );
                session.ctx().mark_tried(peer.addr);
                tracing::warn!(
                    upstream = %up_rt.up.name,
                    peer = %peer.addr,
                    error = %e,
                    "websocket handshake failed; trying next peer"
                );
            }
            None => {
                proxy::record_failure(
                    &state.health,
                    &up_rt.up.name,
                    idx,
                    &up_rt.up.health,
                    now_ms(),
                );
                session.ctx().mark_tried(peer.addr);
                tracing::warn!(
                    upstream = %up_rt.up.name,
                    peer = %peer.addr,
                    "websocket handshake timed out; trying next peer"
                );
            }
        }
    };
    if out_resp.status() != hyper::StatusCode::SWITCHING_PROTOCOLS {
        // A healthy peer answered with a definitive non-101 (bad client
        // request, auth failure, ...). That is not a peer failure: no
        // retry, pass the refusal through as a 502.
        finish_log(&mut session, 502);
        return text_response(
            502,
            format!("502 upstream refused upgrade ({})\n", out_resp.status()),
        );
    }
    proxy::record_success(&state.health, &up_rt.up.name, picked_idx, now_ms());

    // Client-facing 101: copy the handshake headers from upstream.
    let mut resp = Response::builder().status(hyper::StatusCode::SWITCHING_PROTOCOLS);
    if let Some(hm) = resp.headers_mut() {
        for (name, value) in out_resp.headers() {
            let n = name.as_str();
            if !(n == "upgrade" || n == "connection" || n == "sec-websocket-accept") {
                continue;
            }
            let Ok(v) = value.to_str() else { continue };
            let (Ok(hn), Ok(hv)) = (
                HeaderName::from_bytes(n.as_bytes()),
                HeaderValue::from_str(v),
            ) else {
                continue;
            };
            hm.append(hn, hv);
        }
    }
    // Outbound upgrade future; resolves once the 101 is on the wire.
    let out_upgraded = hyper::upgrade::on(&mut out_resp);
    let Ok(resp) = resp.body(Body::empty()) else {
        finish_log(&mut session, 502);
        return text_response(502, "502 bad response\n");
    };

    // Tunnel task: owns the session; log phase runs when it closes.
    let session = Arc::new(Mutex::new(session));
    let log_session = session.clone();
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let in_io = match on_upgrade.await {
            Ok(io) => io,
            Err(e) => {
                tracing::warn!(error = %e, "client websocket upgrade failed");
                run_ws_log(&log_session);
                return;
            }
        };
        let out_io = match out_upgraded.await {
            Ok(io) => io,
            Err(e) => {
                tracing::warn!(error = %e, "upstream websocket upgrade failed");
                run_ws_log(&log_session);
                return;
            }
        };
        let res = proxy::tunnel(
            hyper_util::rt::TokioIo::new(in_io),
            hyper_util::rt::TokioIo::new(out_io),
        )
        .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "websocket tunnel error");
        }
        run_ws_log(&log_session);
        tracing::info!(
            ms = started.elapsed().as_millis() as u64,
            "websocket closed"
        );
    });

    resp
}

/// Run the log phase for a finished (or failed) WebSocket session.
fn run_ws_log(session: &Mutex<RequestSession>) {
    if let Ok(mut s) = session.lock() {
        s.run_phase(Phase::Log);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{boot_state, TmpDir};
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn headers(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn find<'a>(list: &'a [(String, String)], name: &str) -> Option<&'a str> {
        list.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn handshake_drops_hop_by_hop_rebuilds_upgrade_pair() {
        let client = headers(&[
            ("Host", "chat.example.com"),
            ("Connection", "keep-alive, TE"),
            ("Upgrade", "WebSocket"),
            ("Te", "trailers"),
            ("Keep-Alive", "timeout=5"),
            ("Transfer-Encoding", "chunked"),
            ("X-Junk", "end-to-end-keep-me"),
        ]);
        let out = handshake_headers(&client, "10.0.0.9:9000", "203.0.113.7");
        // Hop-by-hop names are gone entirely (the upgrade pair is rebuilt
        // canonically below), and so is the Host the client sent.
        assert!(find(&out, "Te").is_none(), "Te survived: {out:?}");
        assert!(find(&out, "Keep-Alive").is_none(), "{out:?}");
        assert!(find(&out, "Transfer-Encoding").is_none(), "{out:?}");
        // Unknown end-to-end headers are NOT dropped by the gateway.
        assert_eq!(find(&out, "X-Junk"), Some("end-to-end-keep-me"));
        // Host is rebuilt for the peer, not taken from the client.
        assert_eq!(find(&out, "Host"), Some("10.0.0.9:9000"));
        // Upgrade is re-issued with canonical values.
        assert_eq!(find(&out, "Connection"), Some("upgrade"));
        assert_eq!(find(&out, "Upgrade"), Some("websocket"));
    }

    #[test]
    fn handshake_keeps_end_to_end_headers_and_merges_xff() {
        let client = headers(&[
            ("Host", "chat.example.com"),
            ("Connection", "upgrade"),
            ("Upgrade", "websocket"),
            ("Authorization", "Bearer abc"),
            ("Cookie", "sid=42"),
            ("X-Forwarded-For", "198.51.100.1"),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ]);
        let out = handshake_headers(&client, "10.0.0.9:9000", "203.0.113.7");
        assert_eq!(find(&out, "Authorization"), Some("Bearer abc"));
        assert_eq!(find(&out, "Cookie"), Some("sid=42"));
        assert_eq!(find(&out, "Sec-WebSocket-Key"), Some("dGhlIHNhbXBsZSBub25jZQ=="));
        // Existing XFF chain is appended to, not replaced.
        assert_eq!(find(&out, "X-Forwarded-For"), Some("198.51.100.1, 203.0.113.7"));
    }

    /// A peer that completes the TCP + HTTP exchange up to the request,
    /// then never answers: the handshake attempt can only end in the route
    /// timeout.
    async fn spawn_hanging_peer() -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    // Consume the request head so the failure is squarely
                    // "no response" (not a connect/send error), then park.
                    let mut buf = [0u8; 512];
                    let _ = tokio::time::timeout(
                        Duration::from_secs(2),
                        sock.read(&mut buf),
                    )
                    .await;
                    futures::future::pending::<()>().await;
                });
            }
        });
        addr
    }

    /// A peer that answers every handshake with a canned response head.
    async fn spawn_answer_peer(head: &'static str) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 512];
                    let _ = tokio::time::timeout(
                        Duration::from_secs(2),
                        sock.read(&mut buf),
                    )
                    .await;
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        addr
    }

    fn config_with_peers(dir: &TmpDir, peers: &[SocketAddr]) -> String {
        let mut cfg = format!(
            "[server]\nlisten = \"127.0.0.1:18080\"\n\n[plugins]\ndir = \"{}\"\n\n[[upstreams]]\nname = \"u\"\n",
            dir.plugins_dir().display()
        );
        for p in peers {
            cfg.push_str(&format!("  [[upstreams.peers]]\n  addr = \"{p}\"\n"));
        }
        cfg.push_str("\n[[routes]]\npath_prefix = \"/\"\nupstream = \"u\"\n");
        cfg
    }

    fn ws_request() -> axum::extract::Request {
        let mut builder = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri("http://gw/ws?room=1")
            .version(hyper::Version::HTTP_11);
        let h = builder.headers_mut().unwrap();
        h.insert(HOST, HeaderValue::from_static("chat.example.com"));
        h.insert(CONNECTION, HeaderValue::from_static("upgrade"));
        h.insert(UPGRADE, HeaderValue::from_static("websocket"));
        h.insert(
            HeaderName::from_static("sec-websocket-key"),
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        builder.body(Body::empty()).unwrap()
    }

    fn ws_session(
        state: &Arc<AppState>,
    ) -> (RequestSession, Arc<UpstreamRt>) {
        let rt = state.runtime.load();
        let up_rt = rt.upstreams["u"].clone();
        drop(rt);
        let snap = state.registry.snapshot();
        let views: Vec<openrusty_wasm::PeerView> = up_rt
            .up
            .peers
            .iter()
            .map(|p| openrusty_wasm::PeerView {
                name: p.addr.to_string(),
                addr: p.addr.to_string(),
                healthy: true,
            })
            .collect();
        let ctx = openrusty_core::context::ReqCtx {
            method: "GET".into(),
            path: "/ws".into(),
            query: String::new(),
            version: "HTTP/1.1".into(),
            client_addr: "127.0.0.1:41000".parse().unwrap(),
            headers: Vec::new(),
            route_index: None,
            upstream: Some("u".into()),
            peer_index: None,
            attempts: 0,
            tried: Vec::new(),
        };
        let session = RequestSession::new(&state.registry, snap, ctx, views);
        (session, up_rt)
    }

    async fn body_text(resp: &mut Response) -> String {
        let bytes = axum::body::to_bytes(
            std::mem::take(resp.body_mut()),
            usize::MAX,
        )
        .await
        .unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    #[tokio::test]
    async fn handshake_exhausts_peers_after_route_timeouts() {
        let hang = spawn_hanging_peer().await;
        let dir = TmpDir::new("ws-timeout");
        dir.write_config(&config_with_peers(&dir, &[hang]));
        let state = boot_state(&dir);
        let (session, up_rt) = ws_session(&state);

        let started = std::time::Instant::now();
        let mut resp = super::proxy_websocket(
            state,
            session,
            up_rt,
            ws_request(),
            Some(Duration::from_millis(200)),
        )
        .await;
        let elapsed = started.elapsed();

        assert_eq!(resp.status(), hyper::StatusCode::BAD_GATEWAY);
        let body = body_text(&mut resp).await;
        assert!(
            body.contains("no healthy upstream"),
            "expected exhaustion, got: {body}"
        );
        // The single peer had to time out first (200ms) before the walk
        // stopped: well above 0, far below "waited forever".
        assert!(elapsed >= Duration::from_millis(100), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    }

    #[tokio::test]
    async fn handshake_falls_over_to_next_peer_after_timeout() {
        let hang = spawn_hanging_peer().await;
        let refuse = spawn_answer_peer(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        let dir = TmpDir::new("ws-fallback");
        dir.write_config(&config_with_peers(&dir, &[hang, refuse]));
        let state = boot_state(&dir);
        let (session, up_rt) = ws_session(&state);

        let mut resp = super::proxy_websocket(
            state,
            session,
            up_rt,
            ws_request(),
            Some(Duration::from_millis(200)),
        )
        .await;

        // Whichever peer the balancer picks first, the walk must end at
        // the second peer's refusal (not at the hang, not at exhaustion).
        assert_eq!(resp.status(), hyper::StatusCode::BAD_GATEWAY);
        let body = body_text(&mut resp).await;
        assert!(
            body.contains("refused upgrade (400"),
            "expected the peer refusal to surface, got: {body}"
        );
    }

    #[tokio::test]
    async fn upstream_101_is_passed_through_to_the_client() {
        let ok = spawn_answer_peer(
            "HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n",
        )
        .await;
        let dir = TmpDir::new("ws-101");
        dir.write_config(&config_with_peers(&dir, &[ok]));
        let state = boot_state(&dir);
        let (session, up_rt) = ws_session(&state);

        let resp = super::proxy_websocket(
            state,
            session,
            up_rt,
            ws_request(),
            Some(Duration::from_secs(2)),
        )
        .await;

        assert_eq!(resp.status(), hyper::StatusCode::SWITCHING_PROTOCOLS);
        let upgrade = resp
            .headers()
            .get("upgrade")
            .map(|v| v.to_str().unwrap().to_string());
        assert_eq!(upgrade.as_deref(), Some("websocket"));
    }
}
