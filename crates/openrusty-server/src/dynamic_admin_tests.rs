//! Tests for the dynamic-API registration face (`dynamic_admin`).

use super::*;
use crate::app;
use crate::testutil::{boot_state, TmpDir, OK_WAT};
use axum::body::Body;
use std::fs;
use tower::ServiceExt;

/// Content-phase Done with a fixed body ("hi <name>"): small and
/// method-agnostic, so GET bindings can reuse it.
const ECHO_MOD: &str = r#"(module
  (import "openrusty" "resp_body_set" (func $set (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "hi bound")
  (func (export "orr_on_phase") (param $phase i32) (param $aux i32) (result i32)
(if (i32.eq (local.get $phase) (i32.const 3))
  (then
    (drop (call $set (i32.const 0) (i32.const 8)))
    (return (i32.const -4))))
i32.const -5)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))"#;

fn boot(tag: &str) -> (Arc<AppState>, TmpDir, std::path::PathBuf) {
    let dir = TmpDir::new(tag);
    let dyn_dir = dir.0.join("dyn");
    fs::create_dir_all(&dyn_dir).unwrap();
    // The proxy route is narrowed to /__proxy: whatever listens on the
    // config's peer port must not turn unmatched dynamic paths into 200s.
    let mut cfg = dir
        .standard_config()
        .replace("path_prefix = \"/\"", "path_prefix = \"/__proxy\"");
    cfg.push_str(&format!("\n[dynamic]\ndir = \"{}\"\n", dyn_dir.display()));
    dir.write_config(&cfg);
    (boot_state(&dir), dir, dyn_dir)
}

fn svc(state: &Arc<AppState>) -> axum::Router {
    app::router(Arc::clone(state))
}

fn req(method: &str, uri: &str, body: &'static [u8]) -> axum::extract::Request {
    hyper::Request::builder()
        .method(method)
        .uri(uri)
        .extension(axum::extract::ConnectInfo::<std::net::SocketAddr>(
            "127.0.0.1:40020".parse().unwrap(),
        ))
        .body(Body::from(body))
        .unwrap()
}

async fn text(resp: Response) -> String {
    String::from_utf8_lossy(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .into_owned()
}

#[tokio::test]
async fn put_stores_module_and_it_serves() {
    let (state, _dir, dyn_dir) = boot("dynadm-put");
    let svc = svc(&state).into_service::<Body>();

    let resp = svc
        .clone()
        .oneshot(req("PUT", "/openrusty/dynamic/hello", ECHO_MOD.as_bytes()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(dyn_dir.join("hello.wasm").is_file());

    let resp = svc
        .oneshot(req("POST", "/api/v1/dynamic/hello", b""))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(text(resp).await, "hi bound");
    // Counted like every dynamic invocation.
    assert_eq!(
        state.metrics.snapshot().dynamic[&("hello".to_string(), 200u16)],
        1
    );
}

#[tokio::test]
async fn put_rejects_garbage_and_bad_names() {
    let (state, _dir, dyn_dir) = boot("dynadm-bad");
    let svc = svc(&state).into_service::<Body>();

    let resp = svc
        .clone()
        .oneshot(req("PUT", "/openrusty/dynamic/broken", b"not wasm at all"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(!dyn_dir.join("broken.wasm").exists());

    let resp = svc
        .clone()
        .oneshot(req(
            "PUT",
            "/openrusty/dynamic/.hidden",
            ECHO_MOD.as_bytes(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    let resp = svc
        .oneshot(req("PUT", "/openrusty/dynamic/a", b""))
        .await
        .unwrap();
    assert_eq!(resp.status(), 400); // empty body without bind params
}

#[tokio::test]
async fn put_with_bind_serves_any_method_path() {
    let (state, _dir, _dyn_dir) = boot("dynadm-bind");
    let svc = svc(&state).into_service::<Body>();

    let resp = svc
        .clone()
        .oneshot(req(
            "PUT",
            "/openrusty/dynamic/orders?method=GET&path=/api/orders",
            ECHO_MOD.as_bytes(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // The bound method+path answers through the fallback interception.
    let resp = svc
        .clone()
        .oneshot(req("GET", "/api/orders", b""))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(text(resp).await, "hi bound");

    // Other methods/paths on the same path do NOT match; /api/... is
    // not a proxy route here, so the pipeline answers 404.
    let resp = svc
        .clone()
        .oneshot(req("POST", "/api/orders", b""))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let resp = svc
        .clone()
        .oneshot(req("GET", "/api/orders/1", b""))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn bind_only_and_prefix_binding() {
    let (state, _dir, dyn_dir) = boot("dynadm-prefix");
    // File-delivered module.
    fs::write(dyn_dir.join("filemod.wasm"), ECHO_MOD.as_bytes()).unwrap();
    let svc = svc(&state).into_service::<Body>();

    // Bind-only: empty body + params against the existing artifact.
    let resp = svc
        .clone()
        .oneshot(req(
            "PUT",
            "/openrusty/dynamic/filemod?method=GET&path=/api/items/*",
            b"",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    for path in ["/api/items", "/api/items/42"] {
        let resp = svc.clone().oneshot(req("GET", path, b"")).await.unwrap();
        assert_eq!(resp.status(), 200, "GET {path}");
        assert_eq!(text(resp).await, "hi bound");
    }
    let resp = svc
        .clone()
        .oneshot(req("GET", "/api/itemsx", b""))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Bind-only against a MISSING artifact: 404.
    let resp = svc
        .oneshot(req(
            "PUT",
            "/openrusty/dynamic/ghost?method=GET&path=/api/ghost",
            b"",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn delete_removes_module_and_bindings() {
    let (state, _dir, dyn_dir) = boot("dynadm-del");
    fs::write(dyn_dir.join("gone.wasm"), ECHO_MOD.as_bytes()).unwrap();
    let svc = svc(&state).into_service::<Body>();

    let resp = svc
        .clone()
        .oneshot(req(
            "PUT",
            "/openrusty/dynamic/gone?method=GET&path=/api/gone",
            b"",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        svc.clone()
            .oneshot(req("GET", "/api/gone", b""))
            .await
            .unwrap()
            .status(),
        200
    );

    let resp = svc
        .clone()
        .oneshot(req("DELETE", "/openrusty/dynamic/gone", b""))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(text(resp).await.contains("\"unbound\":1"));
    assert!(!dyn_dir.join("gone.wasm").exists());
    // Binding gone: falls back to the pipeline's 404.
    assert_eq!(
        svc.clone()
            .oneshot(req("GET", "/api/gone", b""))
            .await
            .unwrap()
            .status(),
        404
    );
    // Second delete: nothing left.
    let resp = svc
        .clone()
        .oneshot(req("DELETE", "/openrusty/dynamic/gone", b""))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn listing_shows_modules_and_routes() {
    let (state, _dir, _dyn_dir) = boot("dynadm-list");
    let svc = svc(&state).into_service::<Body>();
    let resp = svc
        .clone()
        .oneshot(req(
            "PUT",
            "/openrusty/dynamic/orders?method=GET&path=/api/orders",
            ECHO_MOD.as_bytes(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = svc
        .clone()
        .oneshot(req("GET", "/openrusty/dynamic", b""))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = text(resp).await;
    assert!(body.contains("\"orders\""), "modules missing: {body}");
    assert!(
        body.contains("\"path\":\"/api/orders\"") || body.contains("\"path\": \"/api/orders\""),
        "routes missing: {body}"
    );
}

/// With `[admin] token` set, the registration face requires the
/// shared secret like the rest of the admin plane.
#[tokio::test]
async fn token_guards_the_face() {
    let dir = TmpDir::new("dynadm-auth");
    let dyn_dir = dir.0.join("dyn");
    fs::create_dir_all(&dyn_dir).unwrap();
    let mut cfg = dir.standard_config();
    cfg.push_str(&format!(
        "\n[dynamic]\ndir = \"{}\"\n\n[admin]\ntoken = \"s3cr3t\"\n",
        dyn_dir.display()
    ));
    dir.write_config(&cfg);
    let state = boot_state(&dir);
    let svc = svc(&state).into_service::<Body>();

    let resp = svc
        .clone()
        .oneshot(req("PUT", "/openrusty/dynamic/x", ECHO_MOD.as_bytes()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let mut authed = req("PUT", "/openrusty/dynamic/x", ECHO_MOD.as_bytes());
    authed.headers_mut().insert(
        "authorization",
        axum::http::HeaderValue::from_static("Bearer s3cr3t"),
    );
    let resp = svc.oneshot(authed).await.unwrap();
    assert_eq!(resp.status(), 200);
}

/// Without `[dynamic]`, the face is not mounted at all.
#[tokio::test]
async fn disabled_feature_mounts_nothing() {
    let dir = TmpDir::new("dynadm-off");
    dir.write_plugin("p.wasm", OK_WAT.as_bytes());
    dir.write_config(
        &dir.standard_config()
            .replace("path_prefix = \"/\"", "path_prefix = \"/__proxy\""),
    );
    let state = boot_state(&dir);
    let svc = svc(&state).into_service::<Body>();
    let resp = svc
        .oneshot(req("PUT", "/openrusty/dynamic/x", ECHO_MOD.as_bytes()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
