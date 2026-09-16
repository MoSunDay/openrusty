use super::*;
use crate::testutil::{boot_state, TmpDir};
use axum::extract::ConnectInfo;
use tower::ServiceExt;

#[test]
fn h2_authority_becomes_host_when_absent() {
    let headers = fold_h2_authority(
        vec![("content-length".to_string(), "3".to_string())],
        Some("orr-e2e.example.com:8443"),
    );
    assert_eq!(
        headers.last().unwrap(),
        &("host".to_string(), "orr-e2e.example.com:8443".to_string())
    );
}

#[test]
fn h2_authority_never_overrides_an_explicit_host() {
    let headers = fold_h2_authority(
        vec![("HoSt".to_string(), "explicit.example".to_string())],
        Some("ignored.example"),
    );
    assert_eq!(headers.len(), 1);
    assert_eq!(headers[0].1, "explicit.example");
}

#[test]
fn h1_request_without_authority_is_untouched() {
    let headers = fold_h2_authority(vec![("accept".to_string(), "*/*".to_string())], None);
    assert_eq!(headers.len(), 1);
}

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
    assert_eq!(
        match_route(&only_exact, Some("example.com"), "/api/x"),
        None
    );
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
    assert_eq!(match_route(&host_first, Some("example.com"), "/x"), Some(0));
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
    let routes = vec![route(Some("example.com"), "/api"), route(None, "/")];
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
    assert_eq!(
        request_host(&headers("EXAMPLE.com:8443")).as_deref(),
        Some("example.com")
    );
    assert_eq!(request_host(&headers("[::1]:8080")).as_deref(), Some("::1"));
    assert_eq!(request_host(&[]), None);
    assert_eq!(request_host(&headers("   ")), None);

    assert_eq!(
        match_route(
            &routes,
            request_host(&headers("EXAMPLE.com:8443")).as_deref(),
            "/x"
        ),
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
    let (resp, label) = handle_request(state, "127.0.0.1:40000".parse().unwrap(), req).await;
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
    dir.write_config(
        &dir.standard_config()
            .replace("path_prefix = \"/\"", "path_prefix = \"/v2\""),
    );
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
