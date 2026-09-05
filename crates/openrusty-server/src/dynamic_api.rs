//! Dynamic single-module execution API: `POST /api/v1/dynamic/{name}`.
//!
//! Each request resolves `<dynamic.dir>/<name>.wasm` through the
//! stat-driven [`DynamicRegistry`] (compile cache keyed by mtime/size, so
//! replacing a file takes effect on the next request - no reload) and
//! runs one synthesized single-plugin pipeline (post_read -> rewrite ->
//! access -> content -> header_filter -> log). The handler is a thin
//! adapter: buffer the body under the configured cap, build a `ReqCtx`
//! mirroring the proxy pipeline's request facts, invoke, then map the
//! [`DynOutcome`] to a response with the same header rules as the
//! pipeline's short-circuits (`crate::resp_shortcut`).
//!
//! The routes are mounted by `app::router`/`app::data_router` only while
//! the `[dynamic]` config section is enabled; nothing is mounted when the
//! feature is off (zero routing cost for everyone else).

use crate::pipeline::{empty_response, text_response};
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::header::{HeaderName, HeaderValue};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use openrusty_core::ReqCtx;
use openrusty_wasm::DynOutcome;
use std::net::SocketAddr;
use std::sync::Arc;

/// Dynamic-API routes, unmounted stateless building block. Merged into
/// the data-plane routers by `app` when `[dynamic]` is enabled.
pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/v1/dynamic/{name}", post(handle_dynamic))
}

/// `POST /api/v1/dynamic/{name}`: run one dynamic module for this
/// request. Outcomes handled here (module response, unknown name, bad
/// name, oversized body) are counted in
/// `openrusty_dynamic_requests_total{module,code}`; the disabled-feature
/// 404 below and the router-level 405 are not counted.
async fn handle_dynamic(
    State(state): State<Arc<AppState>>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    Path(name): Path<String>,
    req: axum::extract::Request,
) -> Response {
    // Defensive: the route is only mounted while the feature is enabled,
    // and a reload that disables the section swaps in `None` - answer
    // 404 rather than panic in that window.
    let Some(registry) = state.dynamic.load_full().as_ref().clone() else {
        return text_response(404, "404 not found\n");
    };

    // Buffer the body under the configured cap (413 above it), matching
    // the pipeline's body handling.
    let (parts, body) = req.into_parts();
    let body = match axum::body::to_bytes(body, registry.config().max_body_bytes).await {
        Ok(b) => b,
        Err(_) => {
            state.metrics.record_dynamic(&name, 413);
            return text_response(413, "413 payload too large\n");
        }
    };

    // Request facts mirror the proxy pipeline's ReqCtx: actual method
    // (POST by route), verbatim path/query/version, order-preserving
    // header list, the connecting client address. No route, upstream or
    // peer applies to a single-module pipeline.
    let ctx = ReqCtx {
        method: parts.method.as_str().to_string(),
        path: parts.uri.path().to_string(),
        query: parts.uri.query().unwrap_or_default().to_string(),
        version: version_str(parts.version).to_string(),
        client_addr: remote,
        headers: header_list(&parts.headers),
        route_index: None,
        upstream: None,
        peer_index: None,
        attempts: 0,
        tried: Vec::new(),
    };

    let outcome = registry.invoke(&name, ctx, body).await;
    if let Some(error) = &outcome.error {
        tracing::warn!(
            module = %name,
            status = outcome.status,
            error = %error,
            "dynamic module request failed"
        );
    }
    state.metrics.record_dynamic(&name, outcome.status);
    outcome_response(&outcome)
}

/// HTTP version string, mirroring the pipeline's ReqCtx derivation.
fn version_str(v: hyper::Version) -> &'static str {
    match v {
        hyper::Version::HTTP_2 => "HTTP/2",
        _ => "HTTP/1.1",
    }
}

/// Order-preserving header list; non-UTF-8 values are skipped, exactly
/// like the pipeline's header collection.
fn header_list(headers: &hyper::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_string(), s.to_string()))
        })
        .collect()
}

/// Map an invocation outcome to a response, mirroring the short-circuit
/// rules of `crate::resp_shortcut` (built inline here because there is
/// no session to read - the registry already drained it):
///
/// - module headers win verbatim, except `content-length` (axum derives
///   it from the body); invalid names/values are skipped;
/// - a default `content-type: text/plain; charset=utf-8` is added only
///   when a body is present and the module set none itself;
/// - no body keeps the plain status text (`"<status>\n"`), so registry
///   errors (400/404/500) and body-less denies read like the pipeline's;
/// - a body-less 204 stays truly empty: no body AND no content-type.
fn outcome_response(outcome: &DynOutcome) -> Response {
    let Some(body) = outcome.body.as_ref().filter(|b| !b.is_empty()) else {
        return match outcome.status {
            204 => empty_response(204),
            status => text_response(status, format!("{status}\n")),
        };
    };
    let has_content_type = outcome
        .headers
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case("content-type"));
    let mut builder = Response::builder().status(outcome.status);
    if let Some(hm) = builder.headers_mut() {
        for (name, value) in &outcome.headers {
            if name.eq_ignore_ascii_case("content-length") {
                continue;
            }
            let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) else {
                continue;
            };
            hm.append(n, v);
        }
    }
    if !has_content_type {
        builder = builder.header("content-type", "text/plain; charset=utf-8");
    }
    builder
        .body(Body::from(body.clone()))
        .unwrap_or_else(|_| text_response(500, "500 bad response\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{boot_state, TmpDir};
    use std::fs;
    use tower::ServiceExt;

    /// Content-phase Done with a body and an explicit content-type:
    /// 200, body honored, module content-type wins.
    const BODY_MOD: &str = r#"(module
  (import "openrusty" "resp_body_set" (func $set (param i32 i32) (result i32)))
  (import "openrusty" "resp_header_set" (func $hdr (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "hello dyn")
  (data (i32.const 32) "content-type")
  (data (i32.const 48) "application/json")
  (func (export "orr_on_phase") (param $phase i32) (param $aux i32) (result i32)
    (if (i32.eq (local.get $phase) (i32.const 3))
      (then
        (drop (call $set (i32.const 0) (i32.const 9)))
        (drop (call $hdr (i32.const 32) (i32.const 12) (i32.const 48) (i32.const 16)))
        (return (i32.const -4))))
    i32.const -5)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))"#;

    /// Always Done without a body: 204, truly empty.
    const DONE_MOD: &str = r#"(module
  (memory (export "memory") 1)
  (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const -4)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))"#;

    /// Scratch dir with the standard proxy config plus a `[dynamic]`
    /// section pointing at `<dir>/dyn`, and one module written into it.
    fn dynamic_dir(tag: &str, module: (&str, &str), max_body_bytes: Option<usize>) -> TmpDir {
        let dir = TmpDir::new(tag);
        let dyn_dir = dir.0.join("dyn");
        fs::create_dir_all(&dyn_dir).unwrap();
        fs::write(dyn_dir.join(format!("{}.wasm", module.0)), module.1.as_bytes()).unwrap();
        let mut cfg = dir.standard_config();
        cfg.push_str(&format!("\n[dynamic]\ndir = \"{}\"\n", dyn_dir.display()));
        if let Some(cap) = max_body_bytes {
            cfg.push_str(&format!("max_body_bytes = {cap}\n"));
        }
        dir.write_config(&cfg);
        dir
    }

    fn request(method: &str, uri: &str, body: &'static [u8]) -> axum::extract::Request {
        hyper::Request::builder()
            .method(method)
            .uri(uri)
            .extension(ConnectInfo::<SocketAddr>("127.0.0.1:40010".parse().unwrap()))
            .body(Body::from(body))
            .unwrap()
    }

    async fn body_bytes(resp: Response) -> bytes::Bytes {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn module_response_honors_body_and_content_type() {
        let dir = dynamic_dir("dyn-200", ("echo", BODY_MOD), None);
        let state = boot_state(&dir);
        let svc = routes().with_state(state.clone()).into_service::<Body>();

        let resp = svc
            .oneshot(request("POST", "/api/v1/dynamic/echo", b"req"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get("content-type").unwrap(), "application/json");
        assert_eq!(&body_bytes(resp).await[..], b"hello dyn");
        // Counted under the module name with the served status.
        assert_eq!(
            state.metrics.snapshot().dynamic[&("echo".to_string(), 200u16)],
            1
        );
    }

    #[tokio::test]
    async fn done_without_body_is_truly_empty_204() {
        let dir = dynamic_dir("dyn-204", ("quiet", DONE_MOD), None);
        let state = boot_state(&dir);
        let svc = routes().with_state(state).into_service::<Body>();

        let resp = svc
            .oneshot(request("POST", "/api/v1/dynamic/quiet", b""))
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
        assert!(resp.headers().get("content-type").is_none());
        assert!(body_bytes(resp).await.is_empty());
    }

    #[tokio::test]
    async fn unknown_name_is_404_plain_status_text() {
        let dir = dynamic_dir("dyn-404", ("echo", BODY_MOD), None);
        let state = boot_state(&dir);
        let svc = routes().with_state(state.clone()).into_service::<Body>();

        let resp = svc
            .oneshot(request("POST", "/api/v1/dynamic/missing", b""))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        assert_eq!(&body_bytes(resp).await[..], b"404\n");
        assert_eq!(
            state.metrics.snapshot().dynamic[&("missing".to_string(), 404u16)],
            1
        );
    }

    #[tokio::test]
    async fn invalid_name_is_400() {
        let dir = dynamic_dir("dyn-400", ("echo", BODY_MOD), None);
        let state = boot_state(&dir);
        let svc = routes().with_state(state.clone()).into_service::<Body>();

        let resp = svc
            .oneshot(request("POST", "/api/v1/dynamic/..", b""))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        assert_eq!(&body_bytes(resp).await[..], b"400\n");
        assert_eq!(
            state.metrics.snapshot().dynamic[&("..".to_string(), 400u16)],
            1
        );
    }

    #[tokio::test]
    async fn get_is_rejected_405() {
        let dir = dynamic_dir("dyn-405", ("echo", BODY_MOD), None);
        let state = boot_state(&dir);
        let svc = routes().with_state(state).into_service::<Body>();

        let resp = svc
            .oneshot(request("GET", "/api/v1/dynamic/echo", b""))
            .await
            .unwrap();
        assert_eq!(resp.status(), 405);
    }

    #[tokio::test]
    async fn body_over_cap_is_413() {
        let dir = dynamic_dir("dyn-413", ("echo", BODY_MOD), Some(8));
        let state = boot_state(&dir);
        let svc = routes().with_state(state.clone()).into_service::<Body>();

        let resp = svc
            .oneshot(request("POST", "/api/v1/dynamic/echo", b"123456789"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 413);
        assert_eq!(&body_bytes(resp).await[..], b"413 payload too large\n");
        assert_eq!(
            state.metrics.snapshot().dynamic[&("echo".to_string(), 413u16)],
            1
        );
    }

    #[tokio::test]
    async fn disabled_state_answers_defensive_404() {
        // No [dynamic] section: the routes are never mounted in a real
        // deployment, but the handler itself must still answer 404 if
        // reached (e.g. a reload disabled the section after mounting).
        let dir = TmpDir::new("dyn-off");
        dir.write_plugin("p.wasm", crate::testutil::OK_WAT.as_bytes());
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);
        assert!(state.dynamic.load_full().is_none());
        let svc = routes().with_state(state.clone()).into_service::<Body>();

        let resp = svc
            .oneshot(request("POST", "/api/v1/dynamic/echo", b""))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        assert_eq!(&body_bytes(resp).await[..], b"404 not found\n");
        // Not counted: the feature is off.
        assert!(state.metrics.snapshot().dynamic.is_empty());
    }
}
