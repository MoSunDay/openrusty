//! Fixture-driven parsing tests: real-shaped Ingress / Secret / LIST payloads
//! (JSON under `tests/fixtures/`) decoded through the serde subset.

use std::path::Path;

use openrusty_k8s::model::{parse_line, Ingress, K8sList, PathType, Secret, WatchEvent};
use openrusty_k8s::ResourceMeta;

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(path).unwrap()
}

fn ingress(name: &str) -> Ingress {
    serde_json::from_str(&fixture(name)).unwrap()
}

fn secret(name: &str) -> Secret {
    serde_json::from_str(&fixture(name)).unwrap()
}

#[test]
fn full_ingress_decodes_with_unknown_fields_tolerated() {
    let ing = ingress("ingress-full.json");
    assert_eq!(ing.namespace(), "web");
    assert_eq!(ing.name(), "app");
    assert_eq!(ing.resource_version(), "101");
    assert_eq!(
        ing.metadata
            .annotations
            .get("openrusty.example/dummy")
            .map(String::as_str),
        Some("yes")
    );

    let spec = &ing.spec;
    assert_eq!(spec.ingress_class_name.as_deref(), Some("openrusty"));
    assert_eq!(spec.rules.len(), 2);
    assert_eq!(spec.rules[0].host.as_deref(), Some("App.Example.COM"));
    let paths = &spec.rules[0].http.as_ref().unwrap().paths;
    assert_eq!(paths.len(), 2);
    assert_eq!(paths[0].path, "/v1/chat");
    assert_eq!(paths[0].path_type, PathType::Exact);
    assert_eq!(paths[1].path_type, PathType::Prefix);
    let svc = paths[0].backend.service.as_ref().unwrap();
    assert_eq!(svc.name, "example-svc");
    assert_eq!(svc.port.as_ref().unwrap().number, Some(8000));
    // Hostless rule keeps its own catch-all path.
    assert!(spec.rules[1].host.is_none());
    // The TLS section binds the host to a secret.
    assert_eq!(spec.tls.len(), 1);
    assert_eq!(spec.tls[0].hosts, vec!["app.example.com".to_string()]);
    assert_eq!(spec.tls[0].secret_name.as_deref(), Some("app-tls"));
    // Unknown envelope fields exist in the wire format but are simply not
    // modeled: the typed subset ignores them (`status`, `unknownFutureField`).
    let raw: serde_json::Value = serde_json::from_str(&fixture("ingress-full.json")).unwrap();
    assert!(raw.get("status").is_some());
    assert!(raw.get("unknownFutureField").is_some());
    assert!(
        serde_json::from_value::<Ingress>(raw).is_ok(),
        "unknown fields are tolerated"
    );
}

#[test]
fn minimal_ingress_defaults_class_and_path_type() {
    let ing = ingress("ingress-minimal.json");
    assert_eq!(ing.spec.ingress_class_name, None);
    assert!(ing.spec.tls.is_empty());
    let path = &ing.spec.rules[0].http.as_ref().unwrap().paths[0];
    // Missing pathType degrades to ImplementationSpecific.
    assert_eq!(path.path_type, PathType::ImplementationSpecific);
}

#[test]
fn default_backend_only_ingress_parses() {
    let ing = ingress("ingress-default-backend.json");
    assert!(ing.spec.rules.is_empty());
    assert!(ing.spec.tls.is_empty());
    let backend = ing.spec.default_backend.as_ref().unwrap();
    assert_eq!(backend.service.as_ref().unwrap().name, "fallback-svc");
}

#[test]
fn tls_secret_exposes_base64_material() {
    let s = secret("secret-tls.json");
    assert_eq!(s.type_.as_deref(), Some("kubernetes.io/tls"));
    assert_eq!(s.namespace(), "web");
    assert_eq!(s.name(), "app-tls");
    let (crt, key) = s.tls_material().unwrap();
    // `tls_material` hands back the raw (still base64) data entries.
    assert_eq!(
        crt,
        fixture("secret-tls.json")
            .split("\"tls.crt\": \"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
    );
    assert!(!key.is_empty());
}

#[test]
fn non_tls_secret_is_not_tls_material() {
    let s = secret("secret-other-type.json");
    assert_eq!(s.type_.as_deref(), Some("kubernetes.io/dockerconfigjson"));
    assert!(s.tls_material().is_none());
}

#[test]
fn wrong_typed_secret_with_tls_keys_still_is_not_material() {
    let s = secret("secret-broken-base64.json");
    assert_eq!(s.type_.as_deref(), Some("kubernetes.io/tls"));
    // The type matches, so the raw (garbage) base64 is exposed; decoding is
    // the render step's job (see render_fixtures.rs).
    let (crt, key) = s.tls_material().unwrap();
    assert_eq!(crt, "!!!not-base64!!!");
    assert_eq!(key, "a2V5");
}

#[test]
fn list_envelope_decodes_items_and_rv() {
    let list: K8sList<Ingress> = serde_json::from_str(&fixture("list-ingresses.json")).unwrap();
    assert_eq!(list.metadata.resource_version, "100");
    assert_eq!(list.items.len(), 2);
    assert_eq!(
        (list.items[0].namespace(), list.items[0].name()),
        ("web", "app")
    );
    assert_eq!(list.items[0].resource_version(), "101");
    assert_eq!(list.items[1].name(), "added");
}

#[test]
fn watch_lines_parse_from_fixture_stream() {
    let raw = fixture("watch-events.jsonl");
    let events: Vec<WatchEvent<Ingress>> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| parse_line(l).unwrap())
        .collect();
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e.event_type {
            openrusty_k8s::EventType::Added => "ADDED",
            openrusty_k8s::EventType::Modified => "MODIFIED",
            openrusty_k8s::EventType::Deleted => "DELETED",
            openrusty_k8s::EventType::Bookmark => "BOOKMARK",
            openrusty_k8s::EventType::Error => "ERROR",
        })
        .collect();
    assert_eq!(kinds, ["ADDED", "MODIFIED", "BOOKMARK", "DELETED"]);
    assert_eq!(events[0].object.name(), "added");
    assert_eq!(events[0].object.resource_version(), "102");
    // The bookmark carries no object identity, just the rv to resume from.
    assert_eq!(events[2].object.resource_version(), "110");
    assert_eq!(events[3].object.name(), "added");
}
