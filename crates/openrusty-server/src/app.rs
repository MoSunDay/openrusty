//! Management endpoints (`/openrusty/status`, `/openrusty/reload`) plus the
//! fallback that hands every other request to the proxy pipeline.
//!
//! Lifecycle endpoints (all three share the unified shutdown signal of
//! `shutdown::ShutdownSignal`):
//!
//! - `GET /openrusty/ready`: `200 {"status":"ready"}` while the gateway
//!   serves; `503 {"status":"draining"}` once the shutdown signal flipped
//!   (accept stopped, in-flight draining). Readiness is pure data - a borrow
//!   of the watch receiver - so load balancers can pull the instance before
//!   the socket stops accepting.
//! - `GET /openrusty/live`: `200 {"status":"live"}` unconditionally. The
//!   process answering is alive, draining or not: livez and readyz stay
//!   orthogonal (k8s semantics).
//! - `POST /openrusty/shutdown`: flips the unified signal (equivalent to
//!   SIGTERM) and answers `200 {"status":"shutting down"}` immediately. The
//!   response itself is an in-flight request and completes through the
//!   normal graceful drain; the bounded wait for the listeners runs in
//!   `shutdown::run`, driven by the binary (or an embedder).

use crate::metrics;
use crate::pipeline::{handle_request, text_response};
use crate::reload;
use crate::shutdown;
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use openrusty_proxy as proxy;
use openrusty_wasm::host_state::now_ms;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;

/// Admin-plane routes (`/openrusty/*`). Shared between the admin listener
/// and the combined single-socket router.
///
/// Mounting rule (pure, see `listeners::mounts`): when a `[[server.listeners]]`
/// entry with `role = "admin"` exists, these routes live ONLY on the admin
/// socket and are removed from every data-plane socket; without an admin
/// listener they stay mounted on the data-plane router (historical shape).
fn admin_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/openrusty/status", get(status))
        .route("/openrusty/reload", post(reload_endpoint))
        .route("/openrusty/metrics", get(metrics_endpoint))
        .route("/openrusty/ready", get(ready))
        .route("/openrusty/live", get(live))
        .route("/openrusty/shutdown", post(shutdown_endpoint))
}

/// Admin-only router: `/openrusty/*` and nothing else; every other path 404s
/// without touching the proxy pipeline.
pub fn admin_router(state: Arc<AppState>) -> Router {
    admin_routes().with_state(state)
}

/// Data-plane router without the admin plane: everything falls through to
/// the proxy pipeline.
pub fn data_router(state: Arc<AppState>) -> Router {
    Router::new().fallback(fallback).with_state(state)
}

/// Combined gateway router: management routes first, everything else falls
/// through to the proxy pipeline. This is the single-socket shape used when
/// no dedicated admin listener is configured (and by embedders/tests).
pub fn router(state: Arc<AppState>) -> Router {
    admin_routes().fallback(fallback).with_state(state)
}

/// Proxy fallback; ConnectInfo is inserted per request by `h2c::serve`.
/// Every request that reaches this handler is counted in the Prometheus
/// request counter and duration histogram under one critical section;
/// `handle_request` performs the single route match and returns the route
/// label from the request's own pinned snapshot (management routes such as
/// `/openrusty/metrics` are matched first and bypass the fallback).
async fn fallback(
    State(state): State<Arc<AppState>>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    req: axum::extract::Request,
) -> Response {
    let start = std::time::Instant::now();
    let (resp, pinned_label) = handle_request(state.clone(), remote, req).await;
    let duration = start.elapsed().as_secs_f64();

    // The label was captured at routing time from the snapshot the request
    // was served with. It must NOT be re-resolved here: a concurrent reload
    // may have swapped the route table, and re-reading the current registry
    // would misattribute the record (404s under a renamed prefix, labels of
    // routes the request never matched).
    let route = pinned_label.as_deref().unwrap_or("unknown");
    state
        .metrics
        .record_request_timed(route, resp.status().as_u16(), duration);
    resp
}

/// Prometheus text exposition 0.0.4: process counters from the collector
/// snapshot plus live gauges sampled at scrape time (peer health from the
/// health registry, plugin KV sizes and error kinds from the registry).
async fn metrics_endpoint(State(state): State<Arc<AppState>>) -> Response {
    // Live peer health gauge, upstreams sorted by name.
    let rt = state.runtime.load();
    let now = now_ms();
    let mut peers = Vec::new();
    let mut names: Vec<&String> = rt.upstreams.keys().collect();
    names.sort();
    for name in names {
        let up = &rt.upstreams[name];
        for (i, peer) in up.up.peers.iter().enumerate() {
            let healthy = proxy::is_healthy(&state.health, name, i, now);
            peers.push((name.clone(), peer.addr.to_string(), healthy));
        }
    }

    // Live plugin gauges/counters: KV entry count + per-kind error counts.
    let mut kv = Vec::new();
    let mut plugin_errors = Vec::new();
    for (plugin, kinds, kv_len) in state.registry.metric_view() {
        kv.push((plugin.clone(), kv_len as u64));
        for (kind, count) in kinds {
            plugin_errors.push((plugin.clone(), kind, count));
        }
    }

    let body = metrics::render(&state.metrics.snapshot(), &peers, &kv, &plugin_errors);
    Response::builder()
        .status(200)
        .header("content-type", "text/plain; version=0.0.4")
        .body(Body::from(body))
        .unwrap_or_else(|_| text_response(500, "500 bad response\n"))
}

/// JSON status: runtime generation, uptime, plugin error counters, route
/// and upstream health summary.
async fn status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let rt = state.runtime.load();
    let now = now_ms();
    let mut upstreams: Vec<serde_json::Value> = rt
        .upstreams
        .iter()
        .map(|(name, u)| {
            let healthy = proxy::healthy_indices(&state.health, name, u.up.peers.len(), now).len();
            // Active-plane count: fail-open when active health is disabled
            // (report all peers), otherwise count healthy addr-plane entries.
            let active_healthy = if u.up.health.active.is_some() {
                proxy::active_peers(&state.health)
                    .iter()
                    .filter(|(n, _, h)| n == name && *h)
                    .count()
            } else {
                u.up.peers.len()
            };
            json!({
                "name": name,
                "peers": u.up.peers.len(),
                "healthy": healthy,
                "active_healthy": active_healthy,
            })
        })
        .collect();
    upstreams.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let plugins: Vec<serde_json::Value> = state
        .registry
        .status()
        .into_iter()
        .map(|(name, errors)| json!({ "name": name, "errors": errors }))
        .collect();
    Json(json!({
        "generation": rt.generation,
        "uptime_secs": state.started_at.elapsed().as_secs(),
        "plugins": plugins,
        "routes": rt.routes.len(),
        "upstreams": upstreams,
        "ingress": crate::ingress::status_node(&state.ingress.load()),
    }))
}

/// Loopback-only hot reload trigger.
async fn reload_endpoint(
    State(state): State<Arc<AppState>>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
) -> Response {
    if !remote.ip().is_loopback() {
        return text_response(403, "403 loopback only\n");
    }
    match reload::reload(&state).await {
        Ok(report) => Json(json!({
            "generation": report.generation,
            "plugins": report.plugins,
        }))
        .into_response(),
        // A reload is already running (SIGHUP or another POST): conflict,
        // not failure. The caller can retry once the generation settles.
        Err(reload::ReloadError::InFlight) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "reload already in progress" })),
        )
            .into_response(),
        Err(reload::ReloadError::Failed(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

/// Readiness (`GET /openrusty/ready`): 200 while serving, 503 draining once
/// the unified shutdown signal flipped. Answers from a borrow of the watch
/// receiver - no locks, no side effects.
async fn ready(State(state): State<Arc<AppState>>) -> Response {
    if shutdown::is_draining(&state.shutdown.rx) {
        (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"status": "draining"}))).into_response()
    } else {
        (StatusCode::OK, Json(json!({"status": "ready"}))).into_response()
    }
}

/// Liveness (`GET /openrusty/live`): 200 always - the process answering is
/// alive, draining or not, so orchestrators never restart a draining
/// instance that is merely finishing its connections.
async fn live() -> Response {
    (StatusCode::OK, Json(json!({"status": "live"}))).into_response()
}

/// Shutdown trigger (`POST /openrusty/shutdown`): equivalent to SIGTERM.
/// Flips the unified signal and answers 200 right away - the flip only
/// stops *accepting*, so this response still goes out through the normal
/// graceful drain. The bounded wait and the summary log live in
/// `shutdown::run`, which the binary drives after any trigger.
async fn shutdown_endpoint(State(state): State<Arc<AppState>>) -> Response {
    let _ = state.shutdown.tx.send(true);
    tracing::warn!("shutdown requested via admin endpoint");
    (StatusCode::OK, Json(json!({"status": "shutting down"}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{boot_state, TmpDir};
    use tower::ServiceExt;

    fn request(method: &str, uri: &str, remote: &str) -> axum::extract::Request {
        hyper::Request::builder()
            .method(method)
            .uri(uri)
            .extension(ConnectInfo::<SocketAddr>(remote.parse().unwrap()))
            .body(axum::body::Body::empty())
            .unwrap()
    }

    async fn body_string(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    #[tokio::test]
    async fn status_returns_generation_and_summary() {
        let dir = TmpDir::new("status");
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);
        let svc = router(state).into_service::<axum::body::Body>();

        let resp = svc
            .oneshot(request("GET", "/openrusty/status", "127.0.0.1:40001"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = body_string(resp).await;
        assert!(body.contains("\"generation\""), "body: {body}");
        assert!(body.contains("\"upstreams\""), "body: {body}");
        assert!(body.contains("\"routes\":1"), "body: {body}");
        // Active health is disabled in the standard config: fail-open means
        // active_healthy reports the full peer count.
        assert!(body.contains("\"active_healthy\":1"), "body: {body}");
        assert!(body.contains("\"peers\":1"), "body: {body}");
    }

    #[tokio::test]
    async fn reload_from_non_loopback_is_forbidden() {
        let dir = TmpDir::new("reload403");
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);
        let svc = router(state).into_service::<axum::body::Body>();

        let resp = svc
            .oneshot(request("POST", "/openrusty/reload", "8.8.8.8:1"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
    }

    #[tokio::test]
    async fn reload_from_loopback_succeeds() {
        let dir = TmpDir::new("reload200");
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);
        let svc = router(state.clone()).into_service::<axum::body::Body>();

        let resp = svc
            .oneshot(request("POST", "/openrusty/reload", "127.0.0.1:40002"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = body_string(resp).await;
        assert!(body.contains("\"generation\":1"), "body: {body}");
    }

    #[tokio::test]
    async fn reload_while_another_reload_holds_the_gate_is_conflict() {
        let dir = TmpDir::new("reload409");
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);
        let svc = router(state.clone()).into_service::<axum::body::Body>();

        // An in-flight reload holds the async gate for the whole config +
        // plugin + apply window; a second requester must get the conflict
        // answer immediately instead of queueing behind it.
        let _held = state.reload_gate.lock().await;
        let resp = svc
            .oneshot(request("POST", "/openrusty/reload", "127.0.0.1:40005"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 409);
        let body = body_string(resp).await;
        assert!(body.contains("reload already in progress"), "body: {body}");
    }

    /// Readiness: 200 `ready` while serving; the flip of the unified signal
    /// (SIGTERM and the shutdown endpoint are the same flag) turns it into
    /// 503 `draining` - the LB pull happens before the socket closes.
    #[tokio::test]
    async fn ready_flips_to_draining_when_shutdown_signal_fires() {
        let dir = TmpDir::new("ready");
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);
        let svc = router(state.clone()).into_service::<axum::body::Body>();

        let resp = svc
            .clone()
            .oneshot(request("GET", "/openrusty/ready", "127.0.0.1:40006"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(body_string(resp).await.contains("\"ready\""),);

        state.shutdown.tx.send(true).unwrap();

        let resp = svc
            .clone()
            .oneshot(request("GET", "/openrusty/ready", "127.0.0.1:40007"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 503);
        assert!(body_string(resp).await.contains("\"draining\""));
    }

    /// Liveness stays 200 while draining: livez and readyz are orthogonal.
    #[tokio::test]
    async fn live_stays_ok_while_draining() {
        let dir = TmpDir::new("live");
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);
        let svc = router(state.clone()).into_service::<axum::body::Body>();

        let resp = svc
            .clone()
            .oneshot(request("GET", "/openrusty/live", "127.0.0.1:40008"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(body_string(resp).await.contains("\"live\""));

        state.shutdown.tx.send(true).unwrap();

        let resp = svc
            .clone()
            .oneshot(request("GET", "/openrusty/live", "127.0.0.1:40009"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(body_string(resp).await.contains("\"live\""));
    }

    /// The shutdown endpoint is the SIGTERM equivalent: it flips the very
    /// same signal and answers first, so the response can still be written
    /// through the drain.
    #[tokio::test]
    async fn shutdown_endpoint_flips_the_unified_signal() {
        let dir = TmpDir::new("shutdown-endpoint");
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);
        let svc = router(state.clone()).into_service::<axum::body::Body>();

        let resp = svc
            .clone()
            .oneshot(request("POST", "/openrusty/shutdown", "127.0.0.1:40010"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(body_string(resp).await.contains("\"shutting down\""));

        assert!(
            crate::shutdown::is_draining(&state.shutdown.rx),
            "endpoint did not flip the unified signal"
        );
        // Second trigger is a no-op on the flag, still a 200.
        let resp = svc
            .clone()
            .oneshot(request("POST", "/openrusty/shutdown", "127.0.0.1:40011"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn metrics_endpoint_exposes_counters_and_gauges() {
        let dir = TmpDir::new("metrics");
        // Use a port that is certainly closed: grab an ephemeral port and
        // release it, then point the upstream there. The proxied request
        // then fails with a connect error instead of hitting a stray
        // listener (e.g. an echo_upstream left over from an e2e run).
        let free_port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let peer_addr = format!("127.0.0.1:{free_port}");
        let config = dir.standard_config().replace("127.0.0.1:9001", &peer_addr);
        dir.write_config(&config);
        let state = boot_state(&dir);
        let svc = router(state.clone()).into_service::<axum::body::Body>();

        // One proxied request: no upstream is listening on the test peer,
        // so it 502s after a recorded connect_fail attempt.
        let resp = svc
            .clone()
            .oneshot(request("GET", "/api/hello", "127.0.0.1:40003"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);

        // The scrape itself bypasses the fallback, so it is not counted.
        let resp = svc
            .oneshot(request("GET", "/openrusty/metrics", "127.0.0.1:40004"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap(),
            "text/plain; version=0.0.4"
        );
        let body = body_string(resp).await;

        // Request counter with the routed prefix and the actual status.
        assert!(
            body.contains("openrusty_requests_total{route=\"/\",code=\"502\"} 1"),
            "body: {body}"
        );
        // Duration histogram: buckets, sum and count.
        assert!(
            body.contains("openrusty_request_duration_seconds_bucket{le=\"0.005\"}"),
            "body: {body}"
        );
        assert!(
            body.contains("openrusty_request_duration_seconds_sum "),
            "body: {body}"
        );
        assert!(
            body.contains("openrusty_request_duration_seconds_count 1"),
            "body: {body}"
        );
        // Upstream attempt counter with the recorded result label.
        assert!(
            body.contains(
                "openrusty_upstream_attempts_total{upstream=\"u\",result=\"connect_fail\"} 1"
            ),
            "body: {body}"
        );
        // Live gauges: peer health line for upstream u, KV family present.
        assert!(
            body.contains(&format!(
                "openrusty_peer_healthy{{upstream=\"u\",addr=\"{peer_addr}\"}}"
            )),
            "body: {body}"
        );
        assert!(
            body.contains("# HELP openrusty_kv_entries "),
            "body: {body}"
        );
        assert!(
            body.contains("# TYPE openrusty_kv_entries gauge"),
            "body: {body}"
        );
        // TYPE lines for every family.
        for ty in [
            "# TYPE openrusty_requests_total counter",
            "# TYPE openrusty_request_duration_seconds histogram",
            "# TYPE openrusty_upstream_attempts_total counter",
            "# TYPE openrusty_plugin_errors_total counter",
            "# TYPE openrusty_peer_healthy gauge",
        ] {
            assert!(body.contains(ty), "missing {ty:?} in:\n{body}");
        }
        // The metrics request itself must not be instrumented: a counted
        // scrape would show up as route="unknown" (no route matches).
        assert!(!body.contains("route=\"unknown\""), "body: {body}");
    }
}
