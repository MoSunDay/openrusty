//! Request lifecycle: plugin phases, routing, balancing with retries,
//! header filtering, and hand-off to the streaming body filter.

use crate::body_filter::FilteredBody;
use crate::state::{AppState, UpstreamRt};
use axum::body::Body;
use axum::response::Response;
use hyper::body::Incoming;
use openrusty_core::config::BalancerKind;
use openrusty_core::phase::{Decision, Phase};
use openrusty_core::ReqCtx;
use openrusty_proxy as proxy;
use openrusty_wasm::host_state::now_ms;
use openrusty_wasm::{PeerView, RequestSession};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Request body buffering ceiling (16 MiB).
const MAX_BODY: usize = 16 * 1024 * 1024;

/// Peer selection outcome shared by the normal proxy path and WebSocket.
pub(crate) enum Pick {
    Peer(usize),
    Deny(u16),
    None,
}

/// Run the balancer phase, then fall back to the configured default
/// balancer when the plugins did not pick a peer themselves.
pub(crate) fn pick_peer(
    state: &AppState,
    up_rt: &UpstreamRt,
    session: &mut RequestSession,
) -> Pick {
    if let Decision::Deny(s) = session.run_phase(Phase::Balancer) {
        return Pick::Deny(s);
    }
    if let Some(i) = session.ctx().peer_index {
        let idx = i as usize;
        // Re-validate against CURRENT health: a peer can die between the
        // plugin's (snapshot-age) view and this attempt.
        if idx < up_rt.up.peers.len()
            && proxy::is_healthy(&state.health, &up_rt.up.name, idx, now_ms())
        {
            return Pick::Peer(idx);
        }
        tracing::warn!(
            index = i,
            "plugin chose an invalid/unhealthy peer; using default balancer"
        );
        session.ctx().peer_index = None;
    }
    let healthy = proxy::healthy_indices(
        &state.health,
        &up_rt.up.name,
        up_rt.up.peers.len(),
        now_ms(),
    );
    if healthy.is_empty() {
        return Pick::None;
    }
    let chosen = match up_rt.up.kind {
        BalancerKind::Swrr => swrr_pick(up_rt, &healthy),
        BalancerKind::IpHash => {
            let ip = session.ctx().client_addr.ip().to_string();
            proxy::ip_hash_pick(&ip, &healthy)
        }
    };
    match chosen {
        Some(i) => Pick::Peer(i),
        None => Pick::None,
    }
}

/// SWRR restricted to the healthy subset: project the persistent effective
/// weights, step, and write them back. Preserves smoothness across calls.
fn swrr_pick(up_rt: &UpstreamRt, healthy: &[usize]) -> Option<usize> {
    if healthy.is_empty() {
        return None;
    }
    let mut cur = up_rt.swrr.lock().unwrap();
    if cur.len() != up_rt.up.peers.len() {
        cur.resize(up_rt.up.peers.len(), 0);
    }
    let weights: Vec<u32> = healthy.iter().map(|&i| up_rt.up.peers[i].weight).collect();
    let mut subset: Vec<i64> = healthy.iter().map(|&i| cur[i]).collect();
    let k = proxy::swrr_next(&weights, &mut subset);
    for (j, &i) in healthy.iter().enumerate() {
        cur[i] = subset[j];
    }
    Some(healthy[k])
}

/// Run the log phase exactly once for a short-circuited request.
pub(crate) fn finish_log(session: &mut RequestSession, status: u16) {
    session.run_phase(Phase::Log);
    let path = session.ctx().path.clone();
    let attempts = session.ctx().attempts;
    tracing::info!(status, path = %path, attempts, "request finished");
}

/// Longest-prefix route match (same semantics as
/// `openrusty_core::context::match_route`, specialized for `RouteConfig`
/// to avoid the higher-ranked lifetime bound on its closure argument).
fn match_route(routes: &[openrusty_core::config::RouteConfig], path: &str) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (i, r) in routes.iter().enumerate() {
        let p = r.path_prefix.as_str();
        if path == p || path.starts_with(p) {
            match best {
                Some((_, len)) if p.len() <= len => {}
                _ => best = Some((i, p.len())),
            }
        }
    }
    best.map(|(i, _)| i)
}

/// Plain text response helper shared across the crate.
pub fn text_response(status: u16, msg: impl Into<String>) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(msg.into()))
        .unwrap_or_else(|_| empty_response(500))
}

/// Empty response with the given status.
pub fn empty_response(status: u16) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Full request lifecycle for one proxied request.
pub async fn handle_request(
    state: Arc<AppState>,
    remote: SocketAddr,
    req: axum::extract::Request,
) -> Response {
    // 1. Pin the plugin snapshot and the runtime for the whole request.
    let snap = state.registry.snapshot();
    let rt = state.runtime.load_full();

    // 2. Build the request context.
    let method = req.method().clone();
    let method_str = method.as_str().to_string();
    let path = req.uri().path().to_string();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| path.clone());
    let query = req.uri().query().unwrap_or_default().to_string();
    let version = match req.version() {
        hyper::Version::HTTP_2 => "HTTP/2",
        _ => "HTTP/1.1",
    }
    .to_string();
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_string(), s.to_string()))
        })
        .collect();

    // 3. Route match (longest prefix wins; first wins on ties).
    let Some(route_idx) = match_route(&rt.routes, &path) else {
        return text_response(404, "404 not found\n");
    };
    let route = rt.routes[route_idx].clone();
    let Some(up_rt) = rt.upstreams.get(&route.upstream).cloned() else {
        return text_response(502, "502 unknown upstream\n");
    };

    let ctx = ReqCtx {
        method: method_str.clone(),
        path,
        query,
        version,
        client_addr: remote,
        headers: headers.clone(),
        route_index: Some(route_idx),
        upstream: Some(up_rt.up.name.clone()),
        peer_index: None,
        attempts: 0,
    };

    // 4. Peer views with current health.
    let now = now_ms();
    let peer_views: Vec<PeerView> = up_rt
        .up
        .peers
        .iter()
        .enumerate()
        .map(|(i, p)| PeerView {
            name: p.addr.to_string(),
            addr: p.addr.to_string(),
            healthy: proxy::is_healthy(&state.health, &up_rt.up.name, i, now),
        })
        .collect();

    // 5. Session pinned to the current snapshot.
    let mut session = RequestSession::new(&state.registry, snap, ctx, peer_views);

    // 6. Pre-proxy phases.
    for phase in [Phase::PostRead, Phase::Rewrite, Phase::Access] {
        match session.run_phase(phase) {
            Decision::Deny(s) => {
                finish_log(&mut session, s);
                return text_response(s, format!("{s}\n"));
            }
            Decision::Done => {
                finish_log(&mut session, 204);
                return empty_response(204);
            }
            _ => {}
        }
    }

    // 7. WebSocket branch, before the body is touched.
    if proxy::is_websocket_upgrade(&method_str, &headers) {
        return crate::ws::proxy_websocket(state, session, up_rt, req).await;
    }

    // 8. Buffer the request body.
    let body = match axum::body::to_bytes(req.into_body(), MAX_BODY).await {
        Ok(b) => b,
        Err(_) => {
            finish_log(&mut session, 413);
            return text_response(413, "413 payload too large\n");
        }
    };

    // 9. Content phase.
    match session.run_phase(Phase::Content) {
        Decision::Deny(s) => {
            finish_log(&mut session, s);
            return text_response(s, format!("{s}\n"));
        }
        Decision::Done => {
            finish_log(&mut session, 204);
            return empty_response(204);
        }
        _ => {}
    }

    // 10. Proxy attempts with retries.
    let attempts = up_rt.up.retries + 1;
    let timeout = if route.timeout_ms > 0 {
        Some(Duration::from_millis(route.timeout_ms))
    } else {
        None
    };
    let client_ip = remote.ip().to_string();

    let mut resp: Option<hyper::Response<Incoming>> = None;
    for attempt in 0..attempts {
        session.ctx().attempts = attempt + 1;
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
        let client = proxy::get(&state.pool, peer.addr, up_rt.up.connect_timeout);
        let fwd = proxy::ForwardRequest {
            method: method.clone(),
            path_and_query: path_and_query.clone(),
            headers: headers.clone(),
            body: body.clone(),
            client_ip: client_ip.clone(),
        };
        let fut = proxy::forward(&client, &peer, &fwd);
        let outcome = match timeout {
            Some(t) => tokio::time::timeout(t, fut).await.ok(),
            None => Some(fut.await),
        };
        match outcome {
            Some(Ok(r)) => {
                proxy::record_success(&state.health, &up_rt.up.name, idx, now_ms());
                resp = Some(r);
                break;
            }
            Some(Err(e)) => {
                proxy::record_failure(
                    &state.health,
                    &up_rt.up.name,
                    idx,
                    &up_rt.up.health,
                    now_ms(),
                );
                if e.is_retryable() {
                    tracing::warn!(
                        upstream = %up_rt.up.name, peer = %peer.addr,
                        attempt, error = %e, "retryable upstream failure"
                    );
                    continue;
                }
                finish_log(&mut session, 502);
                return text_response(502, format!("502 upstream error: {e}\n"));
            }
            None => {
                proxy::record_failure(
                    &state.health,
                    &up_rt.up.name,
                    idx,
                    &up_rt.up.health,
                    now_ms(),
                );
                finish_log(&mut session, 502);
                return text_response(502, "502 upstream timeout\n");
            }
        }
    }
    let Some(resp) = resp else {
        finish_log(&mut session, 502);
        return text_response(502, "502 no upstream responded\n");
    };

    // 11. Header filter phase.
    let status = resp.status().as_u16();
    let mut seeded = Vec::new();
    for (k, v) in resp.headers() {
        let name = k.as_str();
        if name.starts_with(':') || proxy::is_hop_by_hop(name) {
            continue;
        }
        if let Ok(s) = v.to_str() {
            seeded.push((name.to_string(), s.to_string()));
        }
    }
    session.set_resp_headers(seeded);
    if let Decision::Deny(s) = session.run_phase(Phase::HeaderFilter) {
        finish_log(&mut session, s);
        return text_response(s, format!("{s}\n"));
    }
    let final_headers = session.resp_headers().to_vec();

    // 12. Build the response; the streaming body runs body_filter + log.
    let peer_name = up_rt.up.peers[session.ctx().peer_index.map(|i| i as usize).unwrap_or(0)]
        .addr
        .to_string();
    let session = Arc::new(Mutex::new(session));
    let filtered = FilteredBody::new(session, resp.into_body(), status, peer_name);

    let mut builder = Response::builder().status(status);
    if let Some(hm) = builder.headers_mut() {
        for (name, value) in final_headers {
            let (Ok(n), Ok(v)) = (
                hyper::header::HeaderName::from_bytes(name.as_bytes()),
                hyper::header::HeaderValue::from_str(&value),
            ) else {
                continue;
            };
            hm.append(n, v);
        }
    }
    builder
        .body(Body::new(filtered))
        .unwrap_or_else(|_| text_response(500, "500 bad response\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{boot_state, TmpDir};
    use axum::extract::ConnectInfo;
    use tower::ServiceExt;

    #[tokio::test]
    async fn unmatched_route_is_404() {
        let dir = TmpDir::new("pipe");
        dir.write_config(&dir.prefix_only_config());
        let state = boot_state(&dir);
        let svc = crate::app::router(state).into_service::<axum::body::Body>();
        let req = hyper::Request::builder()
            .uri("/definitely/not/routed")
            .extension(ConnectInfo::<SocketAddr>(
                "127.0.0.1:40000".parse().unwrap(),
            ))
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 404);
    }
}
