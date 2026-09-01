//! Render watch snapshots into gateway configuration (`openrusty-core`
//! types). Pure functions: bytes in, `Vec<RouteConfig>` out; the server
//! wires the results into a running config in a later milestone.
//!
//! Pipeline: `render_routes` + `render_upstreams` produce the k8s shape
//! from the Ingress snapshot, `render_tls` maps hosts to certificate
//! material, and [`merge`] composes them with the static TOML routes under
//! an explicit conflict policy: a route key claimed twice is a hard error,
//! so the caller rejects the whole apply instead of silently picking a
//! winner (nginx refuses duplicate `location` conflicts the same way).

pub mod routes;
pub mod tls;

use openrusty_core::config::RouteConfig;
use thiserror::Error;

pub use routes::{render_routes, render_upstreams, upstream_name};
pub use tls::{render_tls, TlsPair};

/// Failures while turning watch snapshots into gateway config. Every
/// variant names the offending object so the apply-time log can point at
/// the exact Ingress or Secret.
#[derive(Debug, Error)]
pub enum RenderError {
    /// A rule path did not start with `/` (invalid Ingress object).
    #[error("{owner}: path `{path}` must start with '/'")]
    BadPath {
        /// Which object failed (`ingress <ns>/<name>`).
        owner: String,
        /// The offending path.
        path: String,
    },

    /// The backend names a resource instead of a Service (unsupported).
    #[error("{owner}: backend without a service (resource backends are not supported)")]
    ServiceMissing {
        /// Which object failed.
        owner: String,
    },

    /// The backend service carries no port at all.
    #[error("{owner}: backend service {ns}/{svc} has no port")]
    BackendPortMissing {
        /// Which object failed.
        owner: String,
        /// Service namespace.
        ns: String,
        /// Service name.
        svc: String,
    },

    /// The backend port is a named service port; v1 dials the ClusterIP
    /// directly and needs the number (resolving names requires watching
    /// Service objects, a later milestone).
    #[error("{owner}: backend service {ns}/{svc} uses named port `{port}`; only numeric ports are supported")]
    NamedPort {
        /// Which object failed.
        owner: String,
        /// Service namespace.
        ns: String,
        /// Service name.
        svc: String,
        /// The port name that could not be resolved.
        port: String,
    },

    /// A TLS section references a Secret that is not in the snapshot.
    #[error("{owner}: TLS secret {ns}/{name} not found")]
    SecretMissing {
        /// Which object failed.
        owner: String,
        /// Secret namespace.
        ns: String,
        /// Secret name.
        name: String,
    },

    /// A referenced Secret exists but is not `kubernetes.io/tls`.
    #[error("{owner}: secret {ns}/{name} has type `{ty}`, expected kubernetes.io/tls")]
    SecretNotTls {
        /// Which object failed.
        owner: String,
        /// Secret namespace.
        ns: String,
        /// Secret name.
        name: String,
        /// The wrong type that was found.
        ty: String,
    },

    /// A referenced TLS secret is unusable: missing key, bad base64,
    /// undecodable or empty PEM.
    #[error("{owner}: secret {ns}/{name} field `{field}`: {reason}")]
    SecretPem {
        /// Which object failed.
        owner: String,
        /// Secret namespace.
        ns: String,
        /// Secret name.
        name: String,
        /// Which data key failed.
        field: String,
        /// What exactly is wrong with it.
        reason: String,
    },

    /// Two TLS sections claim the same host with different secrets.
    #[error("TLS host `{host}` is claimed by both {a} and {b}")]
    TlsHostConflict {
        /// The disputed host (lowercase).
        host: String,
        /// First claimant description.
        a: String,
        /// Second claimant description.
        b: String,
    },
}

/// Identity of a route: host (normalized lowercase, `None` = catch-all),
/// path prefix and the exact/prefix match class. The merge conflict policy
/// is expressed over this key; the gateway's host-class/longest-prefix
/// matching makes distinct keys compatible even for the same path text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RouteKey {
    pub host: Option<String>,
    pub path_prefix: String,
    pub exact: bool,
}

impl RouteKey {
    /// Extract the normalized key of a route.
    pub fn of(route: &RouteConfig) -> Self {
        Self {
            host: route.host.as_deref().map(normalize_host),
            path_prefix: route.path_prefix.clone(),
            exact: route.exact,
        }
    }
}

/// Normalize a host for matching keys: trim + lowercase (the gateway
/// compares hosts case-insensitively; normalizing here keeps static and
/// rendered spellings from colliding artificially).
pub fn normalize_host(host: &str) -> String {
    host.trim().to_ascii_lowercase()
}

/// A merge failure: `key` is claimed by both `a` and `b` (source
/// descriptions such as `static[2]` or `rendered[0]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub key: RouteKey,
    pub a: String,
    pub b: String,
}

/// Compose static routes with ingress-rendered routes under the conflict
/// policy: a duplicate [`RouteKey`] anywhere - inside either input or
/// across both - is an error carrying the key and both source labels.
/// Without conflicts the result is static routes first, rendered after.
pub fn merge(
    static_routes: Vec<RouteConfig>,
    rendered: Vec<RouteConfig>,
) -> std::result::Result<Vec<RouteConfig>, Conflict> {
    let mut seen: std::collections::BTreeMap<RouteKey, String> = std::collections::BTreeMap::new();
    let check = |routes: &[RouteConfig],
                 label: &str,
                 seen: &mut std::collections::BTreeMap<RouteKey, String>|
     -> std::result::Result<(), Conflict> {
        for (i, route) in routes.iter().enumerate() {
            let key = RouteKey::of(route);
            let here = format!("{label}[{i}]");
            if let Some(there) = seen.get(&key) {
                return Err(Conflict {
                    key,
                    a: there.clone(),
                    b: here,
                });
            }
            seen.insert(key, here);
        }
        Ok(())
    };
    check(&static_routes, "static", &mut seen)?;
    check(&rendered, "rendered", &mut seen)?;
    let mut all = static_routes;
    all.extend(rendered);
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(host: Option<&str>, path: &str, exact: bool) -> RouteConfig {
        RouteConfig {
            path_prefix: path.to_string(),
            host: host.map(str::to_string),
            exact,
            upstream: "up".to_string(),
            timeout_ms: 0,
        }
    }

    #[test]
    fn host_conflict_between_static_and_rendered_is_rejected() {
        let stat = vec![route(Some("App.Example.COM"), "/api", false)];
        let rendered = vec![route(Some("app.example.com"), "/api", false)];
        let err = merge(stat, rendered).unwrap_err();
        assert_eq!(err.key.host.as_deref(), Some("app.example.com"));
        assert_eq!(err.a, "static[0]");
        assert_eq!(err.b, "rendered[0]");
    }

    #[test]
    fn path_conflict_within_rendered_is_rejected() {
        let rendered = vec![
            route(Some("app.example.com"), "/api", false),
            route(Some("app.example.com"), "/api", false),
        ];
        let err = merge(vec![], rendered).unwrap_err();
        assert_eq!(err.a, "rendered[0]");
        assert_eq!(err.b, "rendered[1]");
    }

    #[test]
    fn duplicate_key_inside_static_is_rejected() {
        let stat = vec![
            route(Some("a.example.com"), "/x", true),
            route(Some("a.example.com"), "/x", true),
        ];
        let err = merge(stat, vec![]).unwrap_err();
        assert_eq!(err.a, "static[0]");
        assert_eq!(err.b, "static[1]");
    }

    #[test]
    fn compatible_sets_concatenate_static_first() {
        // Same path text, different match class or different host: distinct
        // keys, so the gateway's match precedence stays meaningful.
        let stat = vec![
            route(Some("a.example.com"), "/", false),
            route(Some("b.example.com"), "/api", true),
        ];
        let rendered = vec![
            route(Some("a.example.com"), "/api", false),
            route(None, "/", false),
        ];
        let merged = merge(stat, rendered).unwrap();
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[0].host.as_deref(), Some("a.example.com"));
        assert_eq!(merged[3].host, None);
    }
}
