//! Render an Ingress snapshot into routes and upstreams.
//!
//! Class filter: only Ingresses whose `spec.ingressClassName` equals the
//! configured class are collected. An absent or empty class never matches:
//! silently adopting un-classed Ingresses would make the gateway pick up
//! objects meant for some other controller.
//!
//! Upstream naming is deterministic: `ing-{ns}-{svc}-{port}`. Two renders
//! of the same snapshot always agree, so route references stay stable
//! across reloads. Upstreams dial the service's ClusterIP DNS name
//! directly (`svc.ns.svc.cluster.local:port`); kube-proxy does the
//! DNAT/load-balancing behind it.

use std::collections::BTreeSet;

use openrusty_core::config::{BalancerKind, HealthConfig, RouteConfig, UpstreamConfig};

use crate::model::{Ingress, IngressBackend, PathType};
use crate::render::{normalize_host, RenderError};
use crate::snapshot::Snapshot;

/// Deterministic upstream name for a backend service.
pub fn upstream_name(ns: &str, svc: &str, port: u32) -> String {
    format!("ing-{ns}-{svc}-{port}")
}

/// Endpoint (ClusterIP service DNS) for a backend service.
fn endpoint(ns: &str, svc: &str, port: u32) -> String {
    format!("{svc}.{ns}.svc.cluster.local:{port}")
}

/// Where a backend dials: namespace, service name, numeric port.
type Target = (String, String, u32);

/// Extract `(ns, svc, port)` from a backend. The namespace is the
/// Ingress's (namespaced object, same-namespace service reference).
fn backend_target(owner: &str, ns: &str, backend: &IngressBackend) -> Result<Target, RenderError> {
    let service = backend
        .service
        .as_ref()
        .ok_or_else(|| RenderError::ServiceMissing {
            owner: owner.to_string(),
        })?;
    let port = service
        .port
        .as_ref()
        .ok_or_else(|| RenderError::BackendPortMissing {
            owner: owner.to_string(),
            ns: ns.to_string(),
            svc: service.name.clone(),
        })?;
    match port.number {
        Some(number) => Ok((ns.to_string(), service.name.clone(), number)),
        None => Err(RenderError::NamedPort {
            owner: owner.to_string(),
            ns: ns.to_string(),
            svc: service.name.clone(),
            port: port.name.clone().unwrap_or_default(),
        }),
    }
}

/// Class-matching Ingresses in deterministic `(namespace, name)` order,
/// paired with an owner label (`ingress <ns>/<name>`) for error messages.
fn owned<'a>(snap: &'a Snapshot<Ingress>, ingress_class: &str) -> Vec<(String, &'a Ingress)> {
    snap.iter()
        .filter(|(_, ing)| ing.spec.ingress_class_name.as_deref() == Some(ingress_class))
        .map(|((ns, name), ing)| (format!("ingress {ns}/{name}"), ing))
        .collect()
}

/// Render class-matching Ingresses into route configs.
///
/// - every `rule.path` becomes one route (`Exact` pathType -> `exact`,
///   `Prefix`/`ImplementationSpecific` -> prefix semantics);
/// - a rule without `host` becomes a host-less catch-all;
/// - `spec.defaultBackend` becomes `host = None, path = "/"` (catch-all),
///   emitted before the rules.
pub fn render_routes(
    snap: &Snapshot<Ingress>,
    ingress_class: &str,
) -> Result<Vec<RouteConfig>, RenderError> {
    let mut routes = Vec::new();
    for (owner, ing) in owned(snap, ingress_class) {
        let ns = ing.metadata.namespace.as_str();
        if let Some(backend) = &ing.spec.default_backend {
            let (bns, svc, port) = backend_target(&owner, ns, backend)?;
            routes.push(RouteConfig {
                path_prefix: "/".to_string(),
                host: None,
                exact: false,
                upstream: upstream_name(&bns, &svc, port),
                timeout_ms: 0,
            });
        }
        for rule in &ing.spec.rules {
            let host = rule.host.as_deref().map(normalize_host);
            let Some(http) = &rule.http else { continue };
            for path in &http.paths {
                if !path.path.starts_with('/') {
                    return Err(RenderError::BadPath {
                        owner: owner.clone(),
                        path: path.path.clone(),
                    });
                }
                let (bns, svc, port) = backend_target(&owner, ns, &path.backend)?;
                routes.push(RouteConfig {
                    path_prefix: path.path.clone(),
                    host: host.clone(),
                    exact: path.path_type == PathType::Exact,
                    upstream: upstream_name(&bns, &svc, port),
                    timeout_ms: 0,
                });
            }
        }
    }
    Ok(routes)
}

/// Render class-matching Ingresses into upstream configs: one upstream per
/// unique `(namespace, service, port)` backend, sorted by name. Every
/// upstream dials the ClusterIP DNS endpoint directly with the default
/// balancer and passive health DISABLED (`max_fails = 0`): with a single
/// ClusterIP endpoint per service, a few 5xx must not mark the whole
/// service down - kube-proxy owns per-pod health on the other side.
pub fn render_upstreams(
    snap: &Snapshot<Ingress>,
    ingress_class: &str,
) -> Result<Vec<UpstreamConfig>, RenderError> {
    let mut targets: BTreeSet<Target> = BTreeSet::new();
    for (owner, ing) in owned(snap, ingress_class) {
        let ns = ing.metadata.namespace.as_str();
        if let Some(backend) = &ing.spec.default_backend {
            targets.insert(backend_target(&owner, ns, backend)?);
        }
        for rule in &ing.spec.rules {
            let Some(http) = &rule.http else { continue };
            for path in &http.paths {
                targets.insert(backend_target(&owner, ns, &path.backend)?);
            }
        }
    }
    Ok(targets
        .into_iter()
        .map(|(ns, svc, port)| build_upstream(ns, svc, port))
        .collect())
}

/// One k8s upstream config. `connect_timeout_ms` / `pool_idle_timeout_ms`
/// mirror the `openrusty-core` defaults; peers are empty because the
/// endpoint resolves on the connect path at apply time.
fn build_upstream(ns: String, svc: String, port: u32) -> UpstreamConfig {
    UpstreamConfig {
        name: upstream_name(&ns, &svc, port),
        balancer: BalancerKind::default(),
        retries: 0,
        retry_on_timeout: false,
        connect_timeout_ms: 2_000,
        pool_idle_timeout_ms: 60_000,
        peers: Vec::new(),
        endpoints: vec![endpoint(&ns, &svc, port)],
        // passive health off: single ClusterIP endpoint semantics
        health: HealthConfig {
            max_fails: 0,
            ..HealthConfig::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::parse_line;
    use crate::snapshot::replace_from_list;

    fn ingress(json: &str) -> Ingress {
        parse_line::<Ingress>(&format!(r#"{{"type":"ADDED","object":{json}}}"#))
            .unwrap()
            .object
    }

    fn snap(items: Vec<Ingress>) -> Snapshot<Ingress> {
        replace_from_list(items, "1")
    }

    /// One-rule Ingress with the given shape (built with json! so the
    /// braces stay readable).
    #[allow(clippy::too_many_arguments)]
    fn rule_ingress(
        ns: &str,
        name: &str,
        class: Option<&str>,
        host: Option<&str>,
        path: &str,
        path_type: &str,
        port: u32,
    ) -> Ingress {
        let mut spec = serde_json::json!({
            "rules": [{
                "http": {"paths": [{
                    "path": path,
                    "pathType": path_type,
                    "backend": {"service": {"name": "example-svc", "port": {"number": port}}}
                }]}
            }]
        });
        if let Some(class) = class {
            spec["ingressClassName"] = serde_json::json!(class);
        }
        let mut object =
            serde_json::json!({"metadata": {"namespace": ns, "name": name}, "spec": spec});
        if let Some(host) = host {
            object["spec"]["rules"][0]["host"] = serde_json::json!(host);
        }
        ingress(&object.to_string())
    }

    #[test]
    fn class_filter_collects_only_matching_class() {
        let matching = rule_ingress(
            "web",
            "a",
            Some("openrusty"),
            Some("app.example.com"),
            "/",
            "Prefix",
            8000,
        );
        let other_class = rule_ingress(
            "web",
            "b",
            Some("nginx"),
            Some("x.example.com"),
            "/",
            "Prefix",
            8000,
        );
        let no_class = rule_ingress("web", "c", None, Some("y.example.com"), "/", "Prefix", 8000);
        let snap = snap(vec![no_class, other_class, matching]);
        let routes = render_routes(&snap, "openrusty").unwrap();
        assert_eq!(routes.len(), 1, "only the class match survives");
        assert_eq!(routes[0].host.as_deref(), Some("app.example.com"));
    }

    #[test]
    fn exact_flag_host_normalization_and_deterministic_naming() {
        let snap = snap(vec![rule_ingress(
            "web",
            "a",
            Some("openrusty"),
            Some(" App.Example.COM "),
            "/v1/chat",
            "Exact",
            8000,
        )]);
        let routes = render_routes(&snap, "openrusty").unwrap();
        assert_eq!(routes.len(), 1);
        assert!(routes[0].exact, "Exact pathType must set exact");
        assert_eq!(routes[0].host.as_deref(), Some("app.example.com"));
        assert_eq!(routes[0].path_prefix, "/v1/chat");
        assert_eq!(routes[0].upstream, "ing-web-example-svc-8000");
    }

    #[test]
    fn hostless_rule_is_catch_all() {
        let snap = snap(vec![rule_ingress(
            "web",
            "a",
            Some("openrusty"),
            None,
            "/",
            "Prefix",
            8000,
        )]);
        let routes = render_routes(&snap, "openrusty").unwrap();
        assert_eq!(routes[0].host, None);
        assert!(!routes[0].exact);
    }

    #[test]
    fn default_backend_becomes_root_catch_all() {
        let json = r#"{"metadata":{"namespace":"web","name":"d"},"spec":{"ingressClassName":"openrusty","defaultBackend":{"service":{"name":"fallback","port":{"number":8080}}}}}"#;
        let routes = render_routes(&snap(vec![ingress(json)]), "openrusty").unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].path_prefix, "/");
        assert_eq!(routes[0].host, None);
        assert_eq!(routes[0].upstream, "ing-web-fallback-8080");
    }

    #[test]
    fn upstreams_are_deduplicated_and_cluster_ip_shaped() {
        let a = rule_ingress(
            "web",
            "a",
            Some("openrusty"),
            Some("a.example.com"),
            "/",
            "Prefix",
            8000,
        );
        let b = rule_ingress(
            "web",
            "b",
            Some("openrusty"),
            Some("b.example.com"),
            "/",
            "Prefix",
            8000,
        );
        let ups = render_upstreams(&snap(vec![a, b]), "openrusty").unwrap();
        assert_eq!(ups.len(), 1, "same (ns, svc, port) must collapse");
        assert_eq!(ups[0].name, "ing-web-example-svc-8000");
        assert_eq!(
            ups[0].endpoints,
            vec!["example-svc.web.svc.cluster.local:8000"]
        );
        assert!(ups[0].peers.is_empty());
        assert_eq!(
            ups[0].health.max_fails, 0,
            "passive health must default off"
        );
        assert_eq!(ups[0].balancer, BalancerKind::default());
    }

    #[test]
    fn named_port_is_a_render_error() {
        let json = r#"{"metadata":{"namespace":"web","name":"e"},"spec":{"ingressClassName":"openrusty","rules":[{"http":{"paths":[{"path":"/","pathType":"Prefix","backend":{"service":{"name":"example-svc","port":{"name":"http"}}}}]}}]}}"#;
        let snap = snap(vec![ingress(json)]);
        let err = render_routes(&snap, "openrusty").unwrap_err();
        assert!(err.to_string().contains("named port `http`"), "{err}");
    }

    #[test]
    fn upstream_names_sort_deterministically() {
        let items = vec![
            rule_ingress(
                "web",
                "zeta",
                Some("openrusty"),
                Some("z.example.com"),
                "/",
                "Prefix",
                80,
            ),
            rule_ingress(
                "api",
                "alpha",
                Some("openrusty"),
                Some("a.example.com"),
                "/",
                "Prefix",
                80,
            ),
        ];
        let ups = render_upstreams(&snap(items), "openrusty").unwrap();
        let names: Vec<&str> = ups.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["ing-api-example-svc-80", "ing-web-example-svc-80"]
        );
    }
}
