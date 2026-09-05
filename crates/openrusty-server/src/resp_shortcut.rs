//! Body-aware short-circuit responses: pure mappings from the plugin
//! session state (body + response headers written via `resp_body_set` /
//! `resp_header_set`) to an `axum` response. Shared by the proxy pipeline
//! (`Done`/`Deny` short-circuits); the dynamic API reuses them later.

use crate::pipeline::{empty_response, text_response};
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
/// names/values are skipped. A default
/// `content-type: text/plain; charset=utf-8` is added only when the
/// plugin did not set one itself.
fn with_plugin_headers(mut builder: Builder, sess: &RequestSession) -> Builder {
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
    if !has_content_type {
        builder = builder.header("content-type", "text/plain; charset=utf-8");
    }
    builder
}

/// One body-carrying short-circuit: the status, the plugin body and the
/// plugin headers.
fn body_response(sess: &RequestSession, status: u16, body: Bytes) -> Response {
    with_plugin_headers(Response::builder().status(status), sess)
        .body(Body::from(body))
        .unwrap_or_else(|_| empty_response(500))
}

/// Response for a `Decision::Done` short-circuit. A plugin body upgrades
/// the usual empty 204 to a 200 carrying that body (plus the plugin's
/// response headers); no body keeps the empty 204.
pub fn done_response(sess: &RequestSession) -> Response {
    match carried_body(sess) {
        Some(body) => body_response(sess, 200, body.clone()),
        None => empty_response(204),
    }
}

/// Response for a `Decision::Deny(status)` short-circuit. A plugin body
/// replaces the plain `"<status>\n"` text with that status and body (plus
/// the plugin's response headers); no body keeps the plain text.
pub fn deny_response(sess: &RequestSession, status: u16) -> Response {
    match carried_body(sess) {
        Some(body) => body_response(sess, status, body.clone()),
        None => text_response(status, format!("{status}\n")),
    }
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
    use openrusty_core::ReqCtx;
    use openrusty_wasm::{build_linker, new_engine, PluginSnapshot};
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

    /// A session with no plugins and no body: `for_parts` on an empty
    /// snapshot, so no registry or wasm module is needed.
    fn bare_session() -> RequestSession {
        let ticker = new_engine().unwrap();
        let engine = ticker.engine().clone();
        let linker = build_linker(&engine).unwrap();
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
            linker,
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
}
