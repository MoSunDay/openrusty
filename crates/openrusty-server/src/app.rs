//! Management endpoints (`/openrusty/status`, `/openrusty/reload`) plus the
//! fallback that hands every other request to the proxy pipeline.

use crate::pipeline::{handle_request, text_response};
use crate::reload;
use crate::state::AppState;
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

/// Build the gateway router: management routes first, everything else
/// falls through to the proxy pipeline.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/openrusty/status", get(status))
        .route("/openrusty/reload", post(reload_endpoint))
        .fallback(fallback)
        .with_state(state)
}

/// Proxy fallback; ConnectInfo is inserted per request by `h2c::serve`.
async fn fallback(
    State(state): State<Arc<AppState>>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    req: axum::extract::Request,
) -> Response {
    handle_request(state, remote, req).await
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
            json!({
                "name": name,
                "peers": u.up.peers.len(),
                "healthy": healthy,
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
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
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
}
