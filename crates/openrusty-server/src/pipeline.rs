//! Request lifecycle: plugin phases, routing, balancing with retries,
//! header filtering, and hand-off to the streaming body filter.

use crate::body_filter::FilteredBody;
use crate::metrics::{RESULT_CONNECT_FAIL, RESULT_NO_PEER, RESULT_SUCCESS, RESULT_TIMEOUT};
use crate::state::AppState;
use axum::body::Body;
use axum::response::Response;
use hyper::body::Incoming;
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

pub(crate) use crate::pipeline_peer::{pick_peer, Pick};

/// Run the log phase exactly once for a short-circuited request.
pub(crate) fn finish_log(session: &mut RequestSession, status: u16) {
    session.run_phase(Phase::Log);
    let attempts = session.ctx().attempts;
    tracing::info!(status, path = %session.ctx().path, attempts, "request finished");
}

/// Host-aware route match, mirroring nginx's server_name-then-location
/// precedence. Matching happens in two steps:
///
/// 1. Host class: routes with `host = "..."` whose value equals the
///    request's normalized `Host` form the host-specific class; routes
///    without `host` form the catch-all class. The host-specific class is
///    searched first and wins even against a LONGER catch-all path prefix
///    (a host picks the virtual server, the prefix then picks the location
///    inside it); the catch-all class applies only when the host-specific
///    class yields nothing. A `host`-constrained route whose value differs
///    from the request host never matches, and it never matches a request
///    without a `Host` header.
/// 2. Path precedence inside a class: an `exact` route (`exact = true`,
///    nginx `location =` aligned) matches only when the request path
///    equals its `path_prefix` byte-for-byte, and such a hit wins outright
///    over any (even longer) prefix route in the same class. Without an
///    exact hit the existing rule applies unchanged: longest `path_prefix`
///    wins, first route wins on ties. Host-class precedence is evaluated
///    entirely before path precedence, so an exact hit can never promote
///    the catch-all class over the host-specific one.
///
/// `pub(crate)` so the metrics endpoint can resolve the route label.
pub(crate) fn match_route(
    routes: &[openrusty_core::config::RouteConfig],
    host: Option<&str>,
    path: &str,
) -> Option<usize> {
    path_in_class(
        routes
            .iter()
            .enumerate()
            .filter(|(_, r)| r.host.is_some() && host_matches(r.host.as_deref(), host)),
        path,
    )
    .or_else(|| {
        path_in_class(
            routes.iter().enumerate().filter(|(_, r)| r.host.is_none()),
            path,
        )
    })
}

/// Path precedence inside one host class (single pass, no allocation):
/// the first exact hit wins outright; otherwise the longest prefix wins,
/// first route wins on ties. `exact` routes never behave as prefixes, so
/// a non-matching exact route is simply skipped.
fn path_in_class<'a>(
    cands: impl Iterator<Item = (usize, &'a openrusty_core::config::RouteConfig)>,
    path: &str,
) -> Option<usize> {
    let mut exact: Option<usize> = None;
    let mut best_prefix: Option<(usize, usize)> = None;
    for (i, r) in cands {
        if r.exact {
            // Exact routes only ever match the verbatim path (first wins
            // on ties among exacts).
            if exact.is_none() && r.path_prefix == path {
                exact = Some(i);
            }
            continue;
        }
        let p = r.path_prefix.as_str();
        if path == p || path.starts_with(p) {
            match best_prefix {
                Some((_, len)) if p.len() <= len => {}
                _ => best_prefix = Some((i, p.len())),
            }
        }
    }
    exact.or(best_prefix.map(|(i, _)| i))
}

/// Exact, case-insensitive host comparison (HTTP host semantics). A route
/// without a host constraint matches every request; a constrained route
/// never matches a request that carries no usable `Host`.
fn host_matches(route_host: Option<&str>, req_host: Option<&str>) -> bool {
    match (route_host, req_host) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(r), Some(h)) => r.eq_ignore_ascii_case(h),
    }
}

/// Normalized request host for route matching: the `Host` header value
/// (hyper exposes the HTTP/2 `:authority` pseudo-header as `Host`),
/// trimmed, lowercased and with the port stripped (`example.com:8443` and
/// `[::1]:8080` both reduce to the bare hostname). `None` when the request
/// carries no usable Host value.
pub(crate) fn request_host(headers: &[(String, String)]) -> Option<String> {
    let raw = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.trim())?;
    if raw.is_empty() {
        return None;
    }
    let bare = if let Some(rest) = raw.strip_prefix('[') {
        // IPv6 literal: `[::1]:8080` -> `::1`.
        rest.split(']').next().unwrap_or(rest)
    } else {
        match raw.rsplit_once(':') {
            // `example.com:8443` -> `example.com`.
            Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
            _ => raw,
        }
    };
    Some(bare.to_ascii_lowercase())
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

/// Whether a route timeout on the current attempt may fall through to the
/// next peer. Only allowed when the upstream opted in via
/// `retry_on_timeout`, and never on the last attempt (`attempt` is
/// zero-based, `attempts` is the `retries + 1` ceiling).
fn may_retry_timeout(retry_on_timeout: bool, attempt: u32, attempts: u32) -> bool {
    retry_on_timeout && attempt + 1 < attempts
}

/// Route-level timeout for one attempt, `None` when unbounded.
fn route_timeout(timeout_ms: u64) -> Option<Duration> {
    (timeout_ms > 0).then(|| Duration::from_millis(timeout_ms))
}

/// Full request lifecycle for one proxied request.
///
/// Returns the response plus the route label for the request metrics: the
/// label is the path prefix from the request's own pinned runtime snapshot,
/// so a reload that swaps the route table mid-request cannot misattribute
/// the record. The label is `None` only when no route matched (the 404
/// path); every routed response, including the unknown-upstream 502, carries
/// `Some(label)`.
pub async fn handle_request(
    state: Arc<AppState>,
    remote: SocketAddr,
    req: axum::extract::Request,
) -> (Response, Option<String>) {
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

    // 3. Route match: host class first (host-specific routes win over
    // catch-alls), then longest prefix, first wins on ties.
    let host = request_host(&headers);
    let Some(route_idx) = match_route(&rt.routes, host.as_deref(), &path) else {
        return (text_response(404, "404 not found\n"), None);
    };
    let route = rt.routes[route_idx].clone();
    // Metrics label pinned at routing time (item: label must come from the
    // snapshot the request is served with, not from a later registry read).
    let route_label = route.path_prefix.clone();
    let Some(up_rt) = rt.upstreams.get(&route.upstream).cloned() else {
        return (
            text_response(502, "502 unknown upstream\n"),
            Some(route_label),
        );
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
        tried: Vec::new(),
    };

    // 4. Peer views with current health.
    // (`now` is only the routing-time view; health records below re-read
    // the clock so windows reflect when an outcome actually happened.)
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
                return (text_response(s, format!("{s}\n")), Some(route_label));
            }
            Decision::Done => {
                finish_log(&mut session, 204);
                return (empty_response(204), Some(route_label));
            }
            _ => {}
        }
    }

    // 7. WebSocket branch, before the body is touched.
    if proxy::is_websocket_upgrade(&method_str, &headers) {
        // WebSocket upgrades are routed responses too.
        return (
            crate::ws::proxy_websocket(state, session, up_rt, req, route_timeout(route.timeout_ms))
                .await,
            Some(route_label),
        );
    }

    // 8. Buffer the request body.
    let body = match axum::body::to_bytes(req.into_body(), MAX_BODY).await {
        Ok(b) => b,
        Err(_) => {
            finish_log(&mut session, 413);
            return (
                text_response(413, "413 payload too large\n"),
                Some(route_label),
            );
        }
    };
    // Expose the buffered body to plugins from the content phase onward
    // (balancer and log included); earlier phases already ran without it.
    session.set_req_body(body.clone());

    // 9. Content phase.
    match session.run_phase(Phase::Content) {
        Decision::Deny(s) => {
            finish_log(&mut session, s);
            return (text_response(s, format!("{s}\n")), Some(route_label));
        }
        Decision::Done => {
            finish_log(&mut session, 204);
            return (empty_response(204), Some(route_label));
        }
        _ => {}
    }

    // 10. Proxy attempts with retries.
    let attempts = up_rt.up.retries + 1;
    let timeout = route_timeout(route.timeout_ms);
    let client_ip = remote.ip().to_string();

    let mut resp: Option<hyper::Response<Incoming>> = None;
    for attempt in 0..attempts {
        session.ctx().attempts = attempt + 1;
        session.ctx().peer_index = None;
        let idx = match pick_peer(&state, &up_rt, &mut session) {
            Pick::Peer(i) => i,
            Pick::Deny(s) => {
                finish_log(&mut session, s);
                return (text_response(s, format!("{s}\n")), Some(route_label));
            }
            Pick::None => {
                // No candidate: either nothing is healthy or every peer was
                // already tried. Either way the retry loop must stop here.
                state.metrics.record_attempt(&up_rt.up.name, RESULT_NO_PEER);
                finish_log(&mut session, 502);
                return (
                    text_response(502, "502 no healthy upstream\n"),
                    Some(route_label),
                );
            }
        };
        session.ctx().peer_index = Some(idx as u32);

        let peer = up_rt.up.peers[idx];
        let fwd = proxy::ForwardRequest {
            method: method.clone(),
            path_and_query: path_and_query.clone(),
            headers: headers.clone(),
            body: body.clone(),
            client_ip: client_ip.clone(),
        };
        let fut = proxy::forward_peer(&state.pool, &up_rt.up, &peer, &fwd);
        let outcome = match timeout {
            Some(t) => tokio::time::timeout(t, fut).await.ok(),
            None => Some(fut.await),
        };
        match outcome {
            Some(Ok(r)) => {
                // Fresh clock at the record point: `now` above was taken
                // before the (possibly slow) attempt, and passive health
                // windows must be judged against when the outcome happened.
                proxy::record_success(&state.health, &up_rt.up.name, idx, now_ms());
                state.metrics.record_attempt(&up_rt.up.name, RESULT_SUCCESS);
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
                state
                    .metrics
                    .record_attempt(&up_rt.up.name, RESULT_CONNECT_FAIL);
                let kind = proxy::failure_kind(&e);
                if proxy::may_retry(&method, kind) {
                    // Never revisit a peer this request already tried.
                    session.ctx().mark_tried(peer.addr);
                    tracing::warn!(
                        upstream = %up_rt.up.name, peer = %peer.addr,
                        attempt, error = %e, "retryable upstream failure"
                    );
                    continue;
                }
                finish_log(&mut session, 502);
                return (
                    text_response(502, format!("502 upstream error: {e}\n")),
                    Some(route_label),
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
                state.metrics.record_attempt(&up_rt.up.name, RESULT_TIMEOUT);
                // A timed-out attempt may have delivered the request, so a
                // replay is only allowed for idempotent methods.
                if proxy::may_retry(&method, proxy::FailureKind::Timeout)
                    && may_retry_timeout(up_rt.up.retry_on_timeout, attempt, attempts)
                {
                    session.ctx().mark_tried(peer.addr);
                    tracing::warn!(
                        upstream = %up_rt.up.name, peer = %peer.addr,
                        attempt, "route timeout, retrying next peer"
                    );
                    continue;
                }
                finish_log(&mut session, 502);
                return (
                    text_response(502, "502 upstream timeout\n"),
                    Some(route_label),
                );
            }
        }
    }
    let Some(resp) = resp else {
        finish_log(&mut session, 502);
        return (
            text_response(502, "502 no upstream responded\n"),
            Some(route_label),
        );
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
        return (text_response(s, format!("{s}\n")), Some(route_label));
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
        .map(|r| (r, Some(route_label.clone())))
        .unwrap_or_else(|_| (text_response(500, "500 bad response\n"), Some(route_label)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{boot_state, TmpDir};
    use axum::extract::ConnectInfo;
    use tower::ServiceExt;

    fn route(host: Option<&str>, prefix: &str) -> openrusty_core::config::RouteConfig {
        openrusty_core::config::RouteConfig {
            path_prefix: prefix.to_string(),
            host: host.map(str::to_string),
            exact: false,
            upstream: "u".to_string(),
            timeout_ms: 0,
        }
    }

    fn exact_route(host: Option<&str>, path: &str) -> openrusty_core::config::RouteConfig {
        openrusty_core::config::RouteConfig {
            exact: true,
            ..route(host, path)
        }
    }

    /// Exact hit wins outright inside its host class, even against a
    /// longer prefix route, regardless of table order.
    #[test]
    fn exact_hit_wins_over_longer_prefix() {
        let routes = vec![
            route(Some("example.com"), "/api/v1/chain"),
            exact_route(Some("example.com"), "/api"),
        ];
        assert_eq!(
            match_route(&routes, Some("example.com"), "/api"),
            Some(1),
            "exact hit beats the longer /api/v1/chain prefix"
        );
        // Same verdict when the exact route comes first.
        let routes = vec![
            exact_route(Some("example.com"), "/api"),
            route(Some("example.com"), "/api/v1/chain"),
        ];
        assert_eq!(match_route(&routes, Some("example.com"), "/api"), Some(0));
    }

    /// An exact route never matches longer or shorter paths: `/api`
    /// exact is not `/api/x`, and a non-matching exact route falls back
    /// to the prefix rules of its class.
    #[test]
    fn exact_route_matches_only_verbatim_path() {
        let routes = vec![
            exact_route(Some("example.com"), "/api"),
            route(Some("example.com"), "/"),
        ];
        // Sub-path: the exact route is skipped, the "/" prefix applies.
        assert_eq!(match_route(&routes, Some("example.com"), "/api/x"), Some(1));
        // Shorter path: same.
        assert_eq!(match_route(&routes, Some("example.com"), "/ap"), Some(1));
        // Verbatim path: exact wins even over the catch-all prefix.
        assert_eq!(match_route(&routes, Some("example.com"), "/api"), Some(0));
        // No exact hit and no prefix hit -> no route at all.
        let only_exact = vec![exact_route(Some("example.com"), "/api")];
        assert_eq!(match_route(&only_exact, Some("example.com"), "/api/x"), None);
    }

    /// Host semantics are untouched: a host-specific exact route wins
    /// over a catch-all prefix for its host, and other hosts fall back to
    /// the catch-all class (exact never crosses the class boundary).
    #[test]
    fn host_specific_exact_vs_catch_all_prefix() {
        let routes = vec![
            exact_route(Some("api.example.com"), "/v1"),
            route(None, "/v1"),
        ];
        assert_eq!(
            match_route(&routes, Some("api.example.com"), "/v1"),
            Some(0),
            "host-specific exact wins inside the host class"
        );
        assert_eq!(
            match_route(&routes, Some("other.com"), "/v1"),
            Some(1),
            "other hosts never see the host-specific exact route"
        );
        assert_eq!(
            match_route(&routes, Some("api.example.com"), "/v1/models"),
            Some(1),
            "the exact route does not swallow sub-paths of its own host"
        );
    }

    /// Regression: `exact = false` keeps the longest-prefix, first-on-ties
    /// semantics bit-for-bit, including when exact routes are present in
    /// the table but miss.
    #[test]
    fn prefix_semantics_unchanged_when_exact_absent_or_missing() {
        let routes = vec![route(Some("example.com"), "/"), route(None, "/")];
        assert_eq!(
            match_route(&routes, Some("example.com"), "/x"),
            Some(0),
            "host-specific prefix still beats the catch-all prefix"
        );
        let nested = vec![route(None, "/api"), route(None, "/api/v2")];
        assert_eq!(
            match_route(&nested, None, "/api/v2/x"),
            Some(1),
            "longest prefix still wins"
        );
        // A missing exact route never disturbs prefix precedence.
        let mixed = vec![
            exact_route(None, "/nope"),
            route(None, "/api"),
            route(None, "/api/v2"),
        ];
        assert_eq!(match_route(&mixed, None, "/api/v2/x"), Some(2));
    }

    #[test]
    fn host_specific_route_wins_over_catch_all() {
        // Both orders: the host-specific class is searched first, so the
        // position in the route table does not matter.
        let host_first = vec![route(Some("example.com"), "/"), route(None, "/")];
        let catch_all_first = vec![route(None, "/"), route(Some("example.com"), "/")];
        assert_eq!(
            match_route(&host_first, Some("example.com"), "/x"),
            Some(0)
        );
        assert_eq!(
            match_route(&catch_all_first, Some("example.com"), "/x"),
            Some(1)
        );

        // A host-specific class with a shorter prefix still wins over a
        // longer catch-all prefix: host picks the server, prefix picks the
        // location inside it.
        let nested = vec![route(Some("example.com"), "/"), route(None, "/api")];
        assert_eq!(match_route(&nested, Some("example.com"), "/api/x"), Some(0));
        // Another host falls through to the catch-all class.
        assert_eq!(match_route(&nested, Some("other.com"), "/api/x"), Some(1));
    }

    #[test]
    fn host_mismatch_falls_through() {
        let routes = vec![
            route(Some("example.com"), "/api"),
            route(None, "/"),
        ];
        // A different host never enters the host-specific class; the
        // catch-all route takes the request.
        assert_eq!(match_route(&routes, Some("other.com"), "/api"), Some(1));
        // No Host header at all: constrained routes never match.
        assert_eq!(match_route(&routes, None, "/api"), Some(1));

        // Without a catch-all, a host mismatch (or missing Host) is a miss.
        let strict = vec![route(Some("example.com"), "/")];
        assert_eq!(match_route(&strict, Some("other.com"), "/x"), None);
        assert_eq!(match_route(&strict, None, "/x"), None);
    }

    #[test]
    fn no_host_route_matches_as_before() {
        let routes = vec![route(None, "/api"), route(None, "/api/v2")];
        // Any host, longest prefix wins, first wins on ties.
        assert_eq!(
            match_route(&routes, Some("anything.org"), "/api/v2/x"),
            Some(1)
        );
        assert_eq!(match_route(&routes, None, "/api/x"), Some(0));
    }

    #[test]
    fn request_host_is_normalized_for_matching() {
        let routes = vec![route(Some("example.com"), "/")];
        // Case-insensitive, port stripped, IPv6 literal handled.
        let headers = |host: &str| vec![("Host".to_string(), host.to_string())];
        assert_eq!(request_host(&headers("EXAMPLE.com:8443")).as_deref(), Some("example.com"));
        assert_eq!(request_host(&headers("[::1]:8080")).as_deref(), Some("::1"));
        assert_eq!(request_host(&[]), None);
        assert_eq!(request_host(&headers("   ")), None);

        assert_eq!(
            match_route(&routes, request_host(&headers("EXAMPLE.com:8443")).as_deref(), "/x"),
            Some(0)
        );
    }

    #[tokio::test]
    async fn unmatched_route_is_404_without_label() {
        let dir = TmpDir::new("pipe");
        dir.write_config(&dir.prefix_only_config());
        let state = boot_state(&dir);
        let svc = crate::app::router(state.clone()).into_service::<axum::body::Body>();
        let req = hyper::Request::builder()
            .uri("/definitely/not/routed")
            .extension(ConnectInfo::<SocketAddr>(
                "127.0.0.1:40000".parse().unwrap(),
            ))
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 404);

        // At the handle_request level the 404 is exactly the None-label
        // branch: the fallback then records it under "unknown" instead of
        // inventing a route name.
        let req = hyper::Request::builder()
            .uri("/definitely/not/routed")
            .body(axum::body::Body::empty())
            .unwrap();
        let (resp, label) =
            handle_request(state, "127.0.0.1:40000".parse().unwrap(), req).await;
        assert_eq!(resp.status(), 404);
        assert_eq!(label, None);
    }

    #[test]
    fn retry_timeout_guard_semantics() {
        assert!(may_retry_timeout(true, 0, 2), "first attempt may retry");
        assert!(!may_retry_timeout(true, 1, 2), "last attempt never retries");
        assert!(!may_retry_timeout(false, 0, 2), "opt-out retries nothing");
        assert!(!may_retry_timeout(true, 0, 1), "no retries configured");
    }

    #[tokio::test]
    async fn route_label_is_pinned_from_the_serving_snapshot() {
        let dir = TmpDir::new("pipe-label");
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);
        let remote: SocketAddr = "127.0.0.1:40001".parse().unwrap();
        // Metrics attribution happens in the router fallback, label values
        // are directly visible on handle_request's return: exercise both.
        let router = crate::app::router(state.clone()).into_service::<axum::body::Body>();
        let via_router = |path: &'static str| {
            let svc = router.clone();
            async move {
                svc.oneshot(
                    hyper::Request::builder()
                        .uri(path)
                        .extension(ConnectInfo::<SocketAddr>(remote))
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };
        let via_handle = |path: &'static str| {
            let state = state.clone();
            async move {
                handle_request(
                    state,
                    remote,
                    hyper::Request::builder()
                        .uri(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
            }
        };

        // Served while "/" is the matched prefix: the label must come from
        // THIS request's snapshot.
        let (_resp, label) = via_handle("/x").await;
        assert_eq!(label.as_deref(), Some("/"));
        via_router("/x").await;

        // Swap the runtime: the same path no longer matches, a new prefix
        // appears. A label re-read after the swap would misattribute the
        // requests above (and the ones below).
        dir.write_config(&dir.standard_config().replace(
            "path_prefix = \"/\"",
            "path_prefix = \"/v2\"",
        ));
        let cfg = openrusty_core::load_config(&dir.config_path()).unwrap();
        let gen = state.registry.snapshot().generation + 1;
        crate::state::apply_runtime(&state, &cfg, gen, &std::collections::HashMap::new());

        // Old path now 404s (recorded as "unknown", not as some stale or
        // current route name).
        let (resp, label) = via_handle("/x").await;
        assert_eq!(resp.status(), 404);
        assert_eq!(label, None);

        // New path matches under its own prefix label.
        let (_resp, label) = via_handle("/v2/x").await;
        assert_eq!(label.as_deref(), Some("/v2"));
        via_router("/x").await; // 404
        via_router("/v2/x").await;

        // Metrics attribution used each request's pinned label: the first
        // request stays under "/", the 404s land on "unknown", the new
        // prefix on "/v2" -- nothing moved after the fact.
        let snap = state.metrics.snapshot();
        let routes: Vec<&str> = snap.requests.keys().map(|(r, _)| r.as_str()).collect();
        assert!(routes.contains(&"/"), "missing '/' label: {routes:?}");
        assert!(routes.contains(&"unknown"), "missing 404 label: {routes:?}");
        assert!(routes.contains(&"/v2"), "missing '/v2' label: {routes:?}");
    }
}
