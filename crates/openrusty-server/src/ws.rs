//! WebSocket pass-through: run the balancer phase, forward the upgrade
//! handshake to the chosen peer, then tunnel raw bytes both ways. The log
//! phase runs when the tunnel closes (or the handshake fails).

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

/// Proxy one WebSocket upgrade request. Takes ownership of the session:
/// on the success path it is moved into the tunnel task, which runs the
/// log phase after the connection closes.
pub async fn proxy_websocket(
    state: Arc<AppState>,
    mut session: RequestSession,
    up_rt: Arc<UpstreamRt>,
    mut req: axum::extract::Request,
) -> Response {
    // Pick a peer: balancer phase first, default balancer as fallback.
    session.ctx().attempts = 1;
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
    session.ctx().peer_index = Some(idx as u32);
    let peer = up_rt.up.peers[idx];

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
    let mut out = hyper::Request::builder()
        .method(req.method().clone())
        .uri(format!("http://{}{}", peer.addr, path_and_query));
    let Some(headers) = out.headers_mut() else {
        finish_log(&mut session, 502);
        return text_response(502, "502 bad request\n");
    };
    // Keep all sec-websocket-* headers; hop-by-hop ones are rebuilt below.
    for (name, value) in req.headers() {
        if !name.as_str().starts_with("sec-websocket-") {
            continue;
        }
        let Ok(v) = value.to_str() else { continue };
        let (Ok(hn), Ok(hv)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_str(v),
        ) else {
            continue;
        };
        headers.append(hn, hv);
    }
    headers.insert(
        HOST,
        HeaderValue::from_str(&peer.addr.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("upstream")),
    );
    headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
    headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
    if let Ok(xff) = HeaderValue::from_str(&session.ctx().client_addr.ip().to_string()) {
        headers.insert(HeaderName::from_static("x-forwarded-for"), xff);
    }

    let outbound = match out.body(http_body_util::Full::new(bytes::Bytes::new())) {
        Ok(r) => r,
        Err(_) => {
            finish_log(&mut session, 502);
            return text_response(502, "502 bad request\n");
        }
    };
    let client = proxy::get(&state.pool, peer.addr, up_rt.up.connect_timeout);
    let mut out_resp = match client.request(outbound).await {
        Ok(r) => r,
        Err(e) => {
            proxy::record_failure(
                &state.health,
                &up_rt.up.name,
                idx,
                &up_rt.up.health,
                now_ms(),
            );
            finish_log(&mut session, 502);
            return text_response(502, format!("502 upstream: {e}\n"));
        }
    };
    if out_resp.status() != hyper::StatusCode::SWITCHING_PROTOCOLS {
        finish_log(&mut session, 502);
        return text_response(
            502,
            format!("502 upstream refused upgrade ({})\n", out_resp.status()),
        );
    }
    proxy::record_success(&state.health, &up_rt.up.name, idx, now_ms());

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
