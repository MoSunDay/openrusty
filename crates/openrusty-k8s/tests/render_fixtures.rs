//! Fixture-driven render tests: the pre-recorded Ingress/Secret shapes are
//! rendered into `RouteConfig` / `UpstreamConfig` / TLS material.

use std::path::Path;

use openrusty_core::config::RouteConfig;
use openrusty_k8s::model::PathType;
use openrusty_k8s::model::{EventType, Ingress, K8sList, Secret, WatchEvent};
use openrusty_k8s::render::{
    merge, render_routes, render_tls, render_upstreams, RenderError, RouteKey,
};
use openrusty_k8s::{apply_event, replace_from_list, Snapshot};

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(path).unwrap()
}

fn ingress(name: &str) -> Ingress {
    serde_json::from_str(&fixture(name)).unwrap()
}

fn snap_of(ings: &[Ingress]) -> Snapshot<Ingress> {
    let mut snap = Snapshot::new();
    for ing in ings {
        snap = apply_event(
            &snap,
            WatchEvent {
                event_type: EventType::Added,
                object: ing.clone(),
            },
        );
    }
    snap
}

fn secret_snap(items: &[&str]) -> Snapshot<Secret> {
    let mut snap = Snapshot::new();
    for name in items {
        let s: Secret = serde_json::from_str(&fixture(name)).unwrap();
        snap = apply_event(
            &snap,
            WatchEvent {
                event_type: EventType::Added,
                object: s,
            },
        );
    }
    snap
}

#[test]
fn full_ingress_renders_routes_and_upstreams() {
    let snap = snap_of(&[ingress("ingress-full.json")]);
    let routes = render_routes(&snap, "openrusty").unwrap();
    // Exact + Prefix on the host rule, plus the hostless catch-all.
    assert_eq!(routes.len(), 3);
    let keys: Vec<RouteKey> = routes.iter().map(RouteKey::of).collect();
    assert!(keys.contains(&RouteKey {
        host: Some("app.example.com".into()),
        path_prefix: "/v1/chat".into(),
        exact: true
    }));
    assert!(keys.contains(&RouteKey {
        host: Some("app.example.com".into()),
        path_prefix: "/".into(),
        exact: false
    }));
    assert!(keys.contains(&RouteKey {
        host: None,
        path_prefix: "/catch-all".into(),
        exact: false
    }));

    // The fixture host is mixed-case; the rendered key is normalized.
    assert!(routes.iter().all(|r| r
        .host
        .as_deref()
        .is_none_or(|h| h == h.to_ascii_lowercase())));

    let ups = render_upstreams(&snap, "openrusty").unwrap();
    assert_eq!(ups.len(), 2);
    assert_eq!(ups[0].name, "ing-web-example-svc-8000");
    assert_eq!(
        ups[0].endpoints,
        vec!["example-svc.web.svc.cluster.local:8000".to_string()]
    );
    assert!(ups[0].peers.is_empty());
    assert_eq!(ups[1].name, "ing-web-wildcard-svc-9000");
    // Passive health is disabled for k8s upstreams: a single ClusterIP
    // endpoint must not be marked down by a few 5xx (kube-proxy owns
    // per-pod health on the other side).
    assert_eq!(ups[0].health.max_fails, 0);
}

#[test]
fn minimal_ingress_without_class_never_renders() {
    let snap = snap_of(&[ingress("ingress-minimal.json")]);
    assert!(render_routes(&snap, "openrusty").unwrap().is_empty());
    assert!(render_upstreams(&snap, "openrusty").unwrap().is_empty());
}

#[test]
fn default_backend_renders_catch_all_route() {
    let snap = snap_of(&[ingress("ingress-default-backend.json")]);
    let routes = render_routes(&snap, "openrusty").unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].host, None);
    assert_eq!(routes[0].path_prefix, "/");
    assert!(!routes[0].exact);
    assert_eq!(routes[0].upstream, "ing-web-fallback-svc-8080");
}

#[test]
fn duplicate_route_from_two_ingresses_is_left_to_merge() {
    let full = ingress("ingress-full.json");
    let mut dupe = ingress("ingress-minimal.json");
    dupe.metadata.name = "dupe".to_string();
    dupe.spec.ingress_class_name = Some("openrusty".to_string());
    dupe.spec.rules[0].host = Some("app.example.com".to_string());
    // Same host+path+exactness as ingress-full's Prefix "/" route.
    let paths = &mut dupe.spec.rules[0].http.as_mut().unwrap().paths;
    paths[0].path = "/".to_string();
    paths[0].path_type = PathType::Prefix;

    let snap = snap_of(&[full, dupe]);
    let routes = render_routes(&snap, "openrusty").unwrap();
    let slash = routes
        .iter()
        .filter(|r| r.host.as_deref() == Some("app.example.com") && !r.exact)
        .count();
    assert_eq!(slash, 2, "render is faithful; conflicts are merge's job");

    // Different upstreams behind the same key must be a hard conflict.
    let err = merge(Vec::new(), routes).unwrap_err();
    assert_eq!(err.key.host.as_deref(), Some("app.example.com"));
    assert_eq!(err.key.path_prefix, "/");
}

#[test]
fn list_fixture_round_trips_through_snapshot_into_render() {
    let list: K8sList<Ingress> = serde_json::from_str(&fixture("list-ingresses.json")).unwrap();
    let snap = replace_from_list(list.items, &list.metadata.resource_version);
    let routes = render_routes(&snap, "openrusty").unwrap();
    let ups = render_upstreams(&snap, "openrusty").unwrap();
    assert_eq!(ups.len(), 2);
    assert_eq!(ups[0].name, "ing-web-example-svc-8000");
    assert_eq!(ups[1].name, "ing-web-second-svc-8100");
    let hosts: Vec<Option<&str>> = routes.iter().map(|r| r.host.as_deref()).collect();
    assert!(hosts.contains(&Some("app.example.com")));
    assert!(hosts.contains(&Some("second.example.com")));
}

#[test]
fn tls_from_fixture_pairs_ingress_host_with_secret() {
    let ing_snap = snap_of(&[ingress("ingress-full.json")]);
    let secrets = secret_snap(&["secret-tls.json", "secret-other-type.json"]);
    let map = render_tls(&ing_snap, &secrets, "openrusty").unwrap();
    assert_eq!(map.len(), 1);
    let pair = map.get("app.example.com").unwrap();
    assert!(pair.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
    assert!(pair.cert_pem.contains("dummy-cert-for-tests"));
    assert!(pair.key_pem.starts_with("-----BEGIN PRIVATE KEY-----"));
}

#[test]
fn tls_missing_secret_is_a_render_error() {
    let ing_snap = snap_of(&[ingress("ingress-full.json")]);
    let err = render_tls(&ing_snap, &Snapshot::new(), "openrusty").unwrap_err();
    assert!(matches!(err, RenderError::SecretMissing { ref name, .. } if name == "app-tls"));
}

#[test]
fn tls_broken_base64_is_a_render_error() {
    let mut ing = ingress("ingress-full.json");
    ing.spec.tls[0].secret_name = Some("broken".to_string());
    let ing_snap = snap_of(&[ing]);
    let secrets = secret_snap(&["secret-broken-base64.json"]);
    let err = render_tls(&ing_snap, &secrets, "openrusty").unwrap_err();
    assert!(matches!(err, RenderError::SecretPem { ref field, .. } if field == "tls.crt"));
}

#[test]
fn merge_composes_static_then_rendered_and_rejects_overlap() {
    let route = |host: &str, path: &str, upstream: &str| RouteConfig {
        path_prefix: path.to_string(),
        host: Some(host.to_string()),
        exact: false,
        upstream: upstream.to_string(),
        timeout_ms: 0,
    };
    let statics = vec![route("static.example.com", "/", "static-up")];
    let rendered = vec![route("app.example.com", "/", "ing-web-example-svc-8000")];

    let merged = merge(statics.clone(), rendered).unwrap();
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].upstream, "static-up", "static routes come first");

    // static vs rendered clash (same key, different upstream)
    let clash = vec![route("static.example.com", "/", "other-up")];
    assert!(merge(statics.clone(), clash).is_err());
    // clash inside the static set itself
    assert!(merge(vec![statics[0].clone(), statics[0].clone()], Vec::new()).is_err());
}
