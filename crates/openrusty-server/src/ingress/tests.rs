//! Unit tests for the ingress wiring. Only the pure seams are exercised
//! (path building, slot folding, config composition, apply policy, status
//! shapes); the watch loop itself is covered by `openrusty-k8s` tests, so
//! no apiserver fake is needed here - the apply seam ([`super::apply_pair`])
//! is driven directly with hand-built snapshots.

use super::*;
use crate::testutil::{boot_state, fixture_path, TmpDir};
use openrusty_k8s::parse_line;
use serde::de::DeserializeOwned;

const CATCH_ALL: &str = r#"{"metadata":{"name":"catch-all","namespace":"shop","resourceVersion":"9"},"spec":{"ingressClassName":"openrusty","defaultBackend":{"service":{"name":"svc","port":{"number":80}}}}}"#;

const API_RULE: &str = r#"{"metadata":{"name":"api","namespace":"shop","resourceVersion":"11"},"spec":{"ingressClassName":"openrusty","rules":[{"host":"app.example.com","http":{"paths":[{"path":"/api","pathType":"Prefix","backend":{"service":{"name":"web","port":{"number":80}}}}]}}]}}"#;

const TLS_RULE: &str = r#"{"metadata":{"name":"tls","namespace":"shop","resourceVersion":"12"},"spec":{"ingressClassName":"openrusty","tls":[{"hosts":["app.example.com"],"secretName":"absent"}],"rules":[{"host":"app.example.com","http":{"paths":[{"path":"/api","pathType":"Prefix","backend":{"service":{"name":"web","port":{"number":80}}}}]}}]}}"#;

/// One-item snapshot parsed from an object JSON body.
fn snap<T: ResourceMeta + DeserializeOwned>(json: &str, rv: &str) -> Snapshot<T> {
    let item = parse_line::<T>(&format!(r#"{{"type":"ADDED","object":{json}}}"#))
        .unwrap()
        .object;
    replace_from_list(vec![item], rv)
}

/// Boot a real `AppState` from the standard scratch config (static route
/// `/` -> upstream `u` at 127.0.0.1:9001, ingress section disabled).
fn booted(tag: &str) -> (TmpDir, Arc<AppState>) {
    let dir = TmpDir::new(tag);
    dir.write_config(&dir.standard_config());
    let state = boot_state(&dir);
    (dir, state)
}

/// Like [`booted`], but the config declares a TLS-terminating inbound
/// listener (dynamic source: no static material), so the state carries
/// the shared SNI resolver. No listener is ever bound here.
fn booted_with_tls_listener(tag: &str) -> (TmpDir, Arc<AppState>) {
    let dir = TmpDir::new(tag);
    let mut cfg = dir.standard_config();
    cfg.push_str(
        "\n[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:1\"\ntls = true\n\
         \n[ingress]\nenabled = true\n",
    );
    dir.write_config(&cfg);
    let state = boot_state(&dir);
    (dir, state)
}

/// Base64 of a fixture file, for hand-built k8s Secret bodies.
fn b64_fixture(name: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(std::fs::read(fixture_path(name)).unwrap())
}

/// First DER cert of a fixture PEM (identity of published material).
fn fixture_leaf_der(path: &str) -> Vec<u8> {
    let pem = std::fs::read_to_string(path).unwrap();
    let mut cursor = pem.as_bytes();
    let der = rustls_pemfile::certs(&mut cursor).next().unwrap().unwrap();
    der.as_ref().to_vec()
}

#[test]
fn watch_paths_are_cluster_wide_or_namespaced() {
    assert_eq!(
        ingress_paths(&[]),
        vec!["/apis/networking.k8s.io/v1/ingresses".to_string()]
    );
    assert_eq!(
        ingress_paths(&["web".to_string()]),
        vec!["/apis/networking.k8s.io/v1/namespaces/web/ingresses".to_string()]
    );
    assert_eq!(
        secret_paths(&[]),
        vec!["/api/v1/secrets?fieldSelector=type%3Dkubernetes.io%2Ftls".to_string()]
    );
    assert_eq!(
        secret_paths(&["web".to_string()]),
        vec!["/api/v1/namespaces/web/secrets?fieldSelector=type%3Dkubernetes.io%2Ftls".to_string()]
    );
}

#[test]
fn fold_slot_merges_disjoint_namespaces_and_refolds_replacing() {
    let slots: Slots<Ingress> = Arc::new(Mutex::new(vec![None, None]));
    let a = snap::<Ingress>(
        r#"{"metadata":{"name":"x","namespace":"a","resourceVersion":"10"}}"#,
        "10",
    );
    let merged = fold_slot(&slots, 0, &a);
    assert_eq!(merged.len(), 1);

    let b = snap::<Ingress>(
        r#"{"metadata":{"name":"y","namespace":"b","resourceVersion":"20"}}"#,
        "20",
    );
    let merged = fold_slot(&slots, 1, &b);
    assert_eq!(merged.len(), 2);
    assert!(merged.get("a", "x").is_some());
    assert!(merged.get("b", "y").is_some());
    // resource version tracks the hand-over that triggered the fold
    assert_eq!(merged.resource_version(), "20");

    // slot 1 re-lists: full replacement within the slot, slot 0 untouched
    let b2 = snap::<Ingress>(
        r#"{"metadata":{"name":"y2","namespace":"b","resourceVersion":"30"}}"#,
        "30",
    );
    let merged = fold_slot(&slots, 1, &b2);
    assert_eq!(merged.len(), 2);
    assert!(merged.get("a", "x").is_some());
    assert!(merged.get("b", "y2").is_some());
    assert!(merged.get("b", "y").is_none());
}

#[test]
fn build_composes_static_first_rendered_after_and_dedups_upstreams() {
    let (_dir, state) = booted("ingress-build");
    let mut base = state.static_config.clone();
    base.ingress.enabled = true;
    base.ingress.ingress_class = "custom".to_string();

    let rendered = vec![RouteConfig {
        path_prefix: "/api".to_string(),
        host: Some("app.example.com".to_string()),
        exact: false,
        upstream: "ing-shop-web-80".to_string(),
        timeout_ms: 0,
    }];
    let routes = merge(base.routes.clone(), rendered).unwrap();

    // `u` collides with the static upstream: static wins, the rendered
    // one is dropped, the genuinely new name is appended.
    let rendered_upstreams = vec![up("u"), up("ing-shop-web-80")];
    let tls = BTreeMap::new();
    let cfg = build_ingress_config(&base, routes, rendered_upstreams, &tls);

    assert_eq!(cfg.routes.len(), 2);
    assert_eq!(cfg.routes[0].path_prefix, "/", "static first");
    assert_eq!(cfg.routes[1].path_prefix, "/api", "rendered after");
    let names: Vec<&str> = cfg.upstreams.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(names, vec!["u", "ing-shop-web-80"]);
    // server, plugins and the [ingress] segment carry over verbatim
    assert_eq!(cfg.ingress, base.ingress);
    assert_eq!(cfg.plugins.dir, base.plugins.dir);
}

fn up(name: &str) -> UpstreamConfig {
    UpstreamConfig {
        name: name.to_string(),
        balancer: Default::default(),
        retries: 0,
        retry_on_timeout: false,
        connect_timeout_ms: 2_000,
        pool_idle_timeout_ms: 60_000,
        peers: Vec::new(),
        endpoints: vec![],
        tls: None,
        health: Default::default(),
    }
}

/// Comparable projection of routes (`RouteConfig` has no `PartialEq`).
fn route_keys(routes: &[RouteConfig]) -> Vec<(Option<String>, String, bool, String)> {
    routes
        .iter()
        .map(|r| {
            (
                r.host.clone(),
                r.path_prefix.clone(),
                r.exact,
                r.upstream.clone(),
            )
        })
        .collect()
}

#[tokio::test]
async fn route_conflict_keeps_previous_runtime() {
    let (_dir, state) = booted("ingress-conflict");
    let base = state.static_config.clone();
    let gen_before = state.runtime.load().generation;
    let routes_before = route_keys(&state.runtime.load().routes);

    // defaultBackend renders host=None path="/", the static route's key
    let pair = (snap::<Ingress>(CATCH_ALL, "9"), Snapshot::<Secret>::new());
    apply_pair(&state, &base, &base.routes, "openrusty", &pair).await;

    assert_eq!(state.runtime.load().generation, gen_before);
    assert_eq!(route_keys(&state.runtime.load().routes), routes_before);
}

#[tokio::test]
async fn missing_tls_secret_keeps_previous_runtime() {
    let (_dir, state) = booted("ingress-tls-missing");
    let base = state.static_config.clone();
    let routes_before = route_keys(&state.runtime.load().routes);

    // the referenced secret `absent` is not in the (empty) snapshot
    let pair = (snap::<Ingress>(TLS_RULE, "12"), Snapshot::<Secret>::new());
    apply_pair(&state, &base, &base.routes, "openrusty", &pair).await;

    assert_eq!(route_keys(&state.runtime.load().routes), routes_before);
}

#[tokio::test]
async fn apply_publishes_merged_runtime_at_plugin_generation() {
    let (_dir, state) = booted("ingress-apply");
    let base = state.static_config.clone();

    let pair = (snap::<Ingress>(API_RULE, "11"), Snapshot::<Secret>::new());
    apply_pair(&state, &base, &base.routes, "openrusty", &pair).await;

    let rt = state.runtime.load();
    assert_eq!(rt.routes.len(), 2);
    assert_eq!(rt.routes[0].path_prefix, "/");
    assert_eq!(rt.routes[1].path_prefix, "/api");
    assert_eq!(rt.routes[1].host.as_deref(), Some("app.example.com"));
    let up = rt
        .upstreams
        .get("ing-shop-web-80")
        .expect("rendered upstream");
    assert!(
        up.up.peers.is_empty(),
        "unresolvable endpoint: apply-time DNS lookup yields no peers"
    );
    assert_eq!(
        up.up.health.max_fails, 0,
        "passive health off for single ClusterIP"
    );
    // route-only swap: the plugin snapshot generation is carried over
    assert_eq!(rt.generation, state.registry.snapshot().generation);
}

/// The closed TLS loop: a successful apply publishes the rendered
/// secrets into the shared SNI resolver (case-normalized), while a
/// gateway without any TLS listener simply has no resolver to publish
/// into.
#[tokio::test]
async fn apply_publishes_rendered_tls_into_the_sni_resolver() {
    let (_dir, state) = booted_with_tls_listener("ingress-tls-publish");
    let base = state.static_config.clone();
    let resolver = state
        .tls_resolver
        .as_ref()
        .expect("tls listener -> shared resolver");
    assert!(resolver.lookup(Some("app.example.com")).is_none());

    let tls_rule = TLS_RULE.replace("absent", "app-tls");
    let secret = format!(
        r#"{{"metadata":{{"name":"app-tls","namespace":"shop","resourceVersion":"50"}},"type":"kubernetes.io/tls","data":{{"tls.crt":"{crt}","tls.key":"{key}"}}}}"#,
        crt = b64_fixture("server.crt"),
        key = b64_fixture("server.key"),
    );
    let pair = (
        snap::<Ingress>(&tls_rule, "12"),
        snap::<Secret>(&secret, "50"),
    );
    apply_pair(&state, &base, &base.routes, "openrusty", &pair).await;

    // Routes applied AND material published in the same apply.
    assert!(
        route_keys(&state.runtime.load().routes)
            .iter()
            .any(|r| r.0.as_deref() == Some("app.example.com")),
        "routes: {:?}",
        route_keys(&state.runtime.load().routes)
    );
    let got = resolver
        .lookup(Some("app.example.com"))
        .expect("TLS material published by the apply");
    assert_eq!(
        got.cert[0].as_ref(),
        fixture_leaf_der(&fixture_path("server.crt")),
        "published identity is the rendered secret's leaf"
    );
    // SNI matching is case-insensitive, like the resolver promises.
    assert!(resolver.lookup(Some("APP.EXAMPLE.COM")).is_some());
}

/// A gateway without TLS listeners builds no resolver, and apply_pair
/// must not try to publish into one.
#[tokio::test]
async fn apply_without_tls_listeners_needs_no_resolver() {
    let (_dir, state) = booted("ingress-tls-absent");
    assert!(state.tls_resolver.is_none());
    let base = state.static_config.clone();
    let pair = (snap::<Ingress>(API_RULE, "11"), Snapshot::<Secret>::new());
    apply_pair(&state, &base, &base.routes, "openrusty", &pair).await;
    assert_eq!(state.runtime.load().routes.len(), 2);
}

#[tokio::test]
async fn failed_client_setup_marks_watching_false_and_serves_static() {
    let (_dir, state) = booted("ingress-nocreds");
    let cfg = IngressConfig {
        enabled: true,
        kubeconfig: "/nonexistent/openrusty-kubeconfig".to_string(),
        ..Default::default()
    };
    run(state.clone(), cfg, Vec::new()).await;

    let status = state.ingress.load();
    assert!(status.enabled);
    assert!(!status.watching);
    assert!(status.ingresses.is_none());
    // the gateway runtime is untouched
    assert_eq!(state.runtime.load().routes.len(), 1);
}

#[test]
fn status_node_disabled_shape() {
    assert_eq!(
        status_node(&WatchStatus::default()),
        serde_json::json!({"enabled": false})
    );
}

#[test]
fn status_node_enabled_shape_before_first_handover() {
    let status = WatchStatus {
        enabled: true,
        watching: true,
        ..Default::default()
    };
    let node = status_node(&status);
    assert_eq!(node["enabled"], serde_json::json!(true));
    assert_eq!(node["watching"], serde_json::json!(true));
    assert_eq!(node["generation"], serde_json::json!(0));
    assert_eq!(node["last_rv"], serde_json::json!(""));
    assert_eq!(node["reconnects"], serde_json::json!(0));
    assert!(node["last_success_age_ms"].is_null());
    assert_eq!(node["secrets"]["generation"], serde_json::json!(0));
}

#[test]
fn status_node_enabled_shape_after_handover() {
    let status = WatchStatus {
        enabled: true,
        watching: true,
        ingresses: Some(WatchStats {
            generation: 3,
            lists: 3,
            reconnects: 1,
            last_success: Some(std::time::Instant::now()),
            last_rv: "42".to_string(),
        }),
        secrets: Some(WatchStats {
            generation: 2,
            lists: 2,
            reconnects: 0,
            last_success: None,
            last_rv: "41".to_string(),
        }),
    };
    let node = status_node(&status);
    assert_eq!(node["generation"], serde_json::json!(3));
    assert_eq!(node["last_rv"], serde_json::json!("42"));
    assert_eq!(node["reconnects"], serde_json::json!(1));
    assert!(node["last_success_age_ms"].is_u64());
    assert_eq!(node["secrets"]["generation"], serde_json::json!(2));
    assert_eq!(node["secrets"]["last_rv"], serde_json::json!("41"));
    assert!(node["secrets"]["last_success_age_ms"].is_null());
}
