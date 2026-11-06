//! Body-aware short-circuit responses: pure mappings from the plugin
//! session state (body + response headers written via `resp_body_set` /
//! `resp_header_set`) to an `axum` response. Shared by the proxy pipeline
//! (`Done`/`Deny` short-circuits); the dynamic API reuses them later.

use crate::pipeline::empty_response;
use axum::body::Body;
use axum::http::header::{HeaderName, HeaderValue};
use axum::http::response::Builder;
use axum::response::Response;
use bytes::Bytes;
use openrusty_wasm::RequestSession;

/// Short-circuit body carried by the session, if any. An empty body is
/// "no body" (matches [`RequestSession::take_resp_body`]).
fn carried_body(sess: &RequestSession) -> Option<&Bytes> {
    sess.resp_body().filter(|b| !b.is_empty())
}

/// Apply the plugin's response headers to a builder: every header wins
/// except `content-length` (axum derives it from the body); invalid
/// names/values are skipped. When `default_content_type` is set and the
/// plugin did not set a `content-type` itself, a default
/// `content-type: text/plain; charset=utf-8` is added — body-carrying
/// responses only; an empty 204 must not gain one.
fn with_plugin_headers(
    mut builder: Builder,
    sess: &RequestSession,
    default_content_type: bool,
) -> Builder {
    let has_content_type = sess
        .resp_headers()
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case("content-type"));
    if let Some(hm) = builder.headers_mut() {
        for (name, value) in sess.resp_headers() {
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
    if default_content_type && !has_content_type {
        builder = builder.header("content-type", "text/plain; charset=utf-8");
    }
    builder
}

/// One body-carrying short-circuit: the status, the plugin body and the
/// plugin headers.
fn body_response(sess: &RequestSession, status: u16, body: Bytes) -> Response {
    with_plugin_headers(Response::builder().status(status), sess, true)
        .body(Body::from(body))
        .unwrap_or_else(|_| empty_response(500))
}

/// Response for a `Decision::Done` short-circuit. A plugin body upgrades
/// the usual empty 204 to a 200 carrying that body; without a body the
/// empty 204 stands — either way the plugin's response headers are
/// applied (no default content-type on the bodyless 204).
pub fn done_response(sess: &RequestSession) -> Response {
    match carried_body(sess) {
        Some(body) => body_response(sess, 200, body.clone()),
        None => with_plugin_headers(Response::builder().status(204), sess, false)
            .body(Body::empty())
            .unwrap_or_else(|_| empty_response(204)),
    }
}

/// Response for a `Decision::Deny(status)` short-circuit. A plugin body
/// replaces the plain `"<status>\n"` text with that status and body;
/// without a body the plain text stands. Both carry the plugin's
/// response headers (e.g. the `Location` written by `respond_redirect`).
pub fn deny_response(sess: &RequestSession, status: u16) -> Response {
    let body = carried_body(sess)
        .cloned()
        .unwrap_or_else(|| Bytes::from(format!("{status}\n")));
    body_response(sess, status, body)
}

/// Status to log for a `Done` short-circuit: 200 when the plugin wrote a
/// body (the client sees a 200), else the fallback.
pub fn log_status(sess: &RequestSession, fallback: u16) -> u16 {
    if carried_body(sess).is_some() {
        200
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::handle_request;
    use crate::testutil::{boot_state, TmpDir};
    use openrusty_core::config::PluginsConfig;
    use openrusty_core::ReqCtx;
    use openrusty_wasm::{new_engine, PluginSnapshot};
    use std::sync::Arc;

    /// Body-read ceiling for the assertions below (bodies are tiny).
    const READ_LIMIT: usize = 1024 * 1024;

    /// Content-phase Done short-circuit carrying a plugin body: 200 +
    /// body + plugin headers (the plugin's content-type wins over the
    /// text/plain default). The module gates on the phase id so the
    /// pre-proxy phases still decline and the content path is exercised.
    const BODY_DONE_WAT: &str = r#"(module
    (import "openrusty" "resp_body_set" (func $set (param i32 i32) (result i32)))
    (import "openrusty" "resp_header_set" (func $hdr (param i32 i32 i32 i32) (result i32)))
    (memory (export "memory") 1)
    (data (i32.const 0) "served by plugin")
    (data (i32.const 32) "content-type")
    (data (i32.const 48) "application/json")
    (func (export "orr_on_phase") (param $phase i32) (param $ctx i32) (result i32)
      (if (i32.ne (local.get $phase) (i32.const 3))
        (then (return (i32.const -5))))
      (drop (call $set (i32.const 0) (i32.const 16)))
      (drop (call $hdr (i32.const 32) (i32.const 12) (i32.const 48) (i32.const 16)))
      i32.const -4)
    (func (export "orr_alloc") (param i32) (result i32) i32.const 0))"#;

    /// Content-phase Done short-circuit without a body: still the empty
    /// 204 (the previous behavior).
    const DONE_WAT: &str = r#"(module
    (func (export "orr_on_phase") (param $phase i32) (param $ctx i32) (result i32)
      (if (i32.ne (local.get $phase) (i32.const 3))
        (then (return (i32.const -5))))
      i32.const -4)
    (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
    (memory (export "memory") 1))"#;

    /// Access-phase `respond_redirect` shape: a `Location` header plus a
    /// body-less `Deny(302)` — the header must reach the client.
    const REDIR_WAT: &str = r#"(module
    (import "openrusty" "resp_header_set" (func $hdr (param i32 i32 i32 i32) (result i32)))
    (memory (export "memory") 1)
    (data (i32.const 0) "Location")
    (data (i32.const 16) "/moved/by/plugin")
    (func (export "orr_on_phase") (param $phase i32) (param $ctx i32) (result i32)
      (if (i32.ne (local.get $phase) (i32.const 2))
        (then (return (i32.const -5))))
      (drop (call $hdr (i32.const 0) (i32.const 8) (i32.const 16) (i32.const 16)))
      i32.const 302)
    (func (export "orr_alloc") (param i32) (result i32) i32.const 0))"#;

    /// A session with no plugins and no body: `for_parts` on an empty
    /// snapshot, so no registry or wasm module is needed.
    fn bare_session() -> RequestSession {
        let ticker = new_engine(&PluginsConfig::default()).unwrap();
        let engine = ticker.engine().clone();
        let ctx = ReqCtx {
            method: "GET".into(),
            path: "/".into(),
            query: String::new(),
            version: "HTTP/1.1".into(),
            client_addr: "127.0.0.1:40000".parse().unwrap(),
            headers: Vec::new(),
            route_index: None,
            upstream: None,
            peer_index: None,
            attempts: 0,
            tried: Vec::new(),
        };
        RequestSession::for_parts(
            engine,
            Arc::new(PluginSnapshot {
                generation: 0,
                plugins: Vec::new(),
            }),
            ctx,
            Vec::new(),
        )
    }

    /// Serve one request through the full pipeline with `wat` as the only
    /// plugin.
    async fn serve_with_plugin(tag: &str, wat: &str) -> Response {
        let dir = TmpDir::new(tag);
        dir.write_plugin("plug.wasm", wat.as_bytes());
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);
        let req = hyper::Request::builder()
            .uri("/x")
            .body(axum::body::Body::empty())
            .unwrap();
        let (resp, label) = handle_request(state, "127.0.0.1:40010".parse().unwrap(), req).await;
        assert_eq!(label.as_deref(), Some("/"));
        resp
    }

    #[test]
    fn done_without_body_is_empty_204() {
        let sess = bare_session();
        let resp = done_response(&sess);
        assert_eq!(resp.status(), 204);
        assert_eq!(log_status(&sess, 204), 204);
    }

    #[test]
    fn deny_without_body_is_plain_status_text() {
        let sess = bare_session();
        let resp = deny_response(&sess, 403);
        assert_eq!(resp.status(), 403);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .map(|v| v.to_str().unwrap()),
            Some("text/plain; charset=utf-8")
        );
    }

    #[tokio::test]
    async fn done_short_circuit_with_body_is_200() {
        let resp = serve_with_plugin("shortcut-done-body", BODY_DONE_WAT).await;
        assert_eq!(resp.status(), 200);
        // Plugin headers win; no text/plain default is injected.
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            &"application/json"
        );
        let body = axum::body::to_bytes(resp.into_body(), READ_LIMIT)
            .await
            .unwrap();
        assert_eq!(&body[..], b"served by plugin");
    }

    #[tokio::test]
    async fn done_short_circuit_without_body_stays_204() {
        let resp = serve_with_plugin("shortcut-done-empty", DONE_WAT).await;
        assert_eq!(resp.status(), 204);
        let body = axum::body::to_bytes(resp.into_body(), READ_LIMIT)
            .await
            .unwrap();
        assert!(body.is_empty());
    }

    /// Body-less Deny with a plugin header (the `respond_redirect`
    /// shape): status kept, header applied, plain `"<status>\n"` body,
    /// default content-type still injected.
    #[tokio::test]
    async fn deny_without_body_applies_plugin_headers() {
        let mut sess = bare_session();
        sess.set_resp_headers(vec![("Location".into(), "/moved/by/plugin".into())]);
        let resp = deny_response(&sess, 302);
        assert_eq!(resp.status(), 302);
        assert_eq!(resp.headers().get("location").unwrap(), &"/moved/by/plugin");
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            &"text/plain; charset=utf-8"
        );
        let body = axum::body::to_bytes(resp.into_body(), READ_LIMIT)
            .await
            .unwrap();
        assert_eq!(&body[..], b"302\n");
    }

    /// Body-less Done with a plugin header (kv-probe `l404check` shape):
    /// 204, header survives, empty body, and no default content-type is
    /// injected onto the bodyless 204.
    #[tokio::test]
    async fn done_without_body_applies_plugin_headers() {
        let mut sess = bare_session();
        sess.set_resp_headers(vec![("X-Log404".into(), "1".into())]);
        let resp = done_response(&sess);
        assert_eq!(resp.status(), 204);
        assert_eq!(log_status(&sess, 204), 204);
        assert_eq!(resp.headers().get("x-log404").unwrap(), &"1");
        assert!(resp.headers().get("content-type").is_none());
        let body = axum::body::to_bytes(resp.into_body(), READ_LIMIT)
            .await
            .unwrap();
        assert!(body.is_empty());
    }

    /// End-to-end: an access-phase plugin redirect (Location header +
    /// body-less Deny(302), exactly what `respond_redirect` produces)
    /// answers 302 with the `Location` header on the wire.
    #[tokio::test]
    async fn deny_redirect_carries_location_header() {
        let resp = serve_with_plugin("shortcut-deny-redir", REDIR_WAT).await;
        assert_eq!(resp.status(), 302);
        assert_eq!(resp.headers().get("location").unwrap(), &"/moved/by/plugin");
        let body = axum::body::to_bytes(resp.into_body(), READ_LIMIT)
            .await
            .unwrap();
        assert_eq!(&body[..], b"302\n");
    }
}
