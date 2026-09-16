//! `{method, path} -> module` binding table for the dynamic execution
//! API.
//!
//! The table lives in `AppState` behind an `ArcSwap`, so - unlike route
//! MOUNTING - bindings take effect on the very next request without a
//! router rebuild: `app::fallback` consults it before the proxy
//! pipeline's route matching (a dynamic binding wins over a `[[routes]]`
//! prefix) and a hit dispatches to the dynamic pipeline
//! (`dynamic_api::dispatch`).
//!
//! Two writers keep it current:
//!
//! - config: `[dynamic.routes]` is reconciled on boot and on every
//!   reload by `state::apply_dynamic_routes` - config-owned keys are
//!   enforced verbatim and keys dropped from the config disappear;
//! - runtime: the registration face (`dynamic_admin`, `PUT/DELETE
//!   /openrusty/dynamic/{name}`) binds/unbinds on the fly.
//!
//! Runtime-added keys are not config-owned, so a reload that does not
//! mention them leaves them alone (file-delivered modules keep their
//! runtime bindings across reloads).
//!
//! Every operation is copy-on-write over the immutable snapshot (build a
//! fresh table, swap it in); tables are bounded by [`MAX_BINDINGS`], so
//! the copy stays cheap and readers never block.

use openrusty_core::config::{
    normalize_method, normalize_route_path, valid_module_name, DynamicRoute,
};
use std::collections::HashMap;

/// Upper bound on bindings (config + runtime combined): the table is
/// copied on every mutation and scanned once per unmatched request, so
/// it must stay small by construction.
pub const MAX_BINDINGS: usize = 4096;

/// Normalized binding key: uppercased method + verbatim path.
type Key = (String, String);

/// Immutable binding set. All transitions build a new value; the live
/// table is whatever `AppState.dynamic_routes` currently holds.
#[derive(Debug, Default, Clone)]
pub struct DynamicRoutes {
    /// `(METHOD, path) -> module` for exact paths.
    exact: HashMap<Key, String>,
    /// `((METHOD, prefix), module)` for paths bound as `<prefix>/*`,
    /// kept longest-prefix-first so the first hit is the most specific.
    prefix: Vec<(Key, String)>,
    /// Config-owned keys: the only ones `reconcile` may remove or
    /// overwrite; runtime-added keys survive reloads.
    config_keys: Vec<Key>,
}

impl DynamicRoutes {
    /// Empty table (feature disabled / no bindings).
    pub fn new() -> Self {
        Self::default()
    }

    /// Total binding count (exact + prefix).
    pub fn len(&self) -> usize {
        self.exact.len() + self.prefix.len()
    }

    /// Whether the table holds no bindings at all.
    pub fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.prefix.is_empty()
    }

    /// All bindings in stable order (exact keys sorted, then prefixes),
    /// prefix paths re-suffixed with `/*` for display.
    pub fn entries(&self) -> Vec<DynamicRoute> {
        let mut exact: Vec<_> = self
            .exact
            .iter()
            .map(|((m, p), module)| DynamicRoute {
                method: m.clone(),
                path: p.clone(),
                module: module.clone(),
            })
            .collect();
        exact.sort_by(|a, b| (&a.method, &a.path).cmp(&(&b.method, &b.path)));
        let mut prefix: Vec<_> = self
            .prefix
            .iter()
            .map(|((m, p), module)| DynamicRoute {
                method: m.clone(),
                path: format!("{p}/*"),
                module: module.clone(),
            })
            .collect();
        prefix.sort_by(|a, b| (&a.method, &a.path).cmp(&(&b.method, &b.path)));
        exact.into_iter().chain(prefix).collect()
    }

    /// Resolve one request: exact match first, then the longest matching
    /// prefix; the request method is uppercased to match the stored
    /// (normalized) keys. `None` = not dynamically routed.
    pub fn lookup(&self, method: &str, path: &str) -> Option<&str> {
        let method = method.to_ascii_uppercase();
        if let Some(module) = self.exact.get(&(method.clone(), path.to_string())) {
            return Some(module.as_str());
        }
        // Longest-prefix-first invariant: `/api/orders/*` outranks
        // `/api/*`, which outranks `/*`.
        self.prefix
            .iter()
            .find(|((m, p), _)| {
                m.as_str() == method
                    && path
                        .strip_prefix(p.as_str())
                        // `/api/*` owns `/api` itself and everything below
                        // it, but not a longer sibling like `/apifoo`.
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
            })
            .map(|(_, module)| module.as_str())
    }

    /// Add or replace one runtime binding (not config-owned). `Err`
    /// carries the client-facing reason for a 400.
    pub fn bind(&self, method: &str, path: &str, module: &str) -> Result<Self, String> {
        self.bind_owned(method, path, module, false)
    }

    /// `bind` plus ownership: config-reconciled keys are tracked in
    /// `config_keys` so a later reload can remove or overwrite them.
    fn bind_owned(
        &self,
        method: &str,
        path: &str,
        module: &str,
        owned: bool,
    ) -> Result<Self, String> {
        let Some(m) = normalize_method(method) else {
            return Err(format!("invalid method {method:?}"));
        };
        let Some(p) = normalize_route_path(path) else {
            return Err(format!("invalid path {path:?}: must start with '/'"));
        };
        if !valid_module_name(module) {
            return Err(format!(
                "invalid module {module:?}: must match ^[A-Za-z0-9][A-Za-z0-9._-]*$"
            ));
        }
        // One binding per (method, base path): binding `/api/orders`
        // after `/api/orders/*` (or the other way round) REPLACES it -
        // a prefix `/api/orders/*` already covers the bare path, so the
        // two shapes never need to coexist.
        let key = base_key(m, &p);
        let replacing = self.exact.contains_key(&key) || self.prefix.iter().any(|(k, _)| *k == key);
        if !replacing && self.len() >= MAX_BINDINGS {
            return Err(format!(
                "dynamic route table full ({MAX_BINDINGS} bindings)"
            ));
        }
        let mut next = self.clone();
        if p.ends_with("/*") {
            next.exact.remove(&key);
            next.prefix.retain(|(k, _)| *k != key);
            next.prefix.push((key.clone(), module.to_string()));
            next.prefix.sort_by(|((am, ap), _), ((bm, bp), _)| {
                (bm.len() + bp.len()).cmp(&(am.len() + ap.len()))
            });
        } else {
            next.prefix.retain(|(k, _)| *k != key);
            next.exact.insert(key.clone(), module.to_string());
        }
        if owned {
            next.config_keys.retain(|k| *k != key);
            next.config_keys.push(key.clone());
        }
        Ok(next)
    }

    /// Remove one binding; `path` may be spelled with or without the
    /// `/*` suffix (both name the same base binding). Missing key is a
    /// no-op.
    pub fn unbind(&self, method: &str, path: &str) -> Self {
        let (Some(m), Some(p)) = (normalize_method(method), normalize_route_path(path)) else {
            return self.clone();
        };
        let key = base_key(m, &p);
        let mut next = self.clone();
        next.exact.remove(&key);
        next.prefix.retain(|(k, _)| *k != key);
        next.config_keys.retain(|k| *k != key);
        next
    }

    /// Remove every binding pointing at `module` (used by the DELETE
    /// registration endpoint). Returns the pruned table and how many
    /// bindings went away.
    pub fn unbind_module(&self, module: &str) -> (Self, usize) {
        let before = self.len();
        let mut next = self.clone();
        next.exact.retain(|_, m| m.as_str() != module);
        next.prefix.retain(|(_, m)| m.as_str() != module);
        // Prune config keys whose binding just disappeared, so a later
        // reconcile only tracks keys that are actually live.
        next.config_keys
            .retain(|k| next.exact.contains_key(k) || next.prefix.iter().any(|(pk, _)| pk == k));
        let removed = before - next.len();
        (next, removed)
    }

    /// Reload reconcile against the new config's `[[dynamic.routes]]`:
    /// keys owned by the OLD config but absent from the new one are
    /// dropped, the new set is overlaid (config wins over a runtime
    /// binding on the same key), and runtime-added keys survive
    /// untouched. Config bindings are pre-validated, so a failure here
    /// only skips that entry (defensive; never fails the reload).
    pub fn reconcile(&self, cfg: &[DynamicRoute]) -> Self {
        let mut next = self.clone();
        let stale = self.config_keys.clone();
        next.config_keys.clear();
        for key in &stale {
            next.exact.remove(key);
            next.prefix.retain(|(k, _)| k != key);
        }
        for r in cfg {
            if let Ok(with) = next.bind_owned(&r.method, &r.path, &r.module, true) {
                next = with;
            }
        }
        next
    }
}

/// The lookup key of a binding: uppercased method plus the path with a
/// trailing `/*` stripped (`/api/*` and `/api` share the key - only one
/// of the two shapes can be bound at a time).
fn base_key(method: String, path: &str) -> Key {
    let base = path.strip_suffix("/*").unwrap_or(path);
    (method, base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound(specs: &[(&str, &str, &str)]) -> DynamicRoutes {
        let mut t = DynamicRoutes::new();
        for (m, p, module) in specs {
            t = t.bind(m, p, module).unwrap();
        }
        t
    }

    #[test]
    fn exact_match_and_method_case() {
        let t = bound(&[("get", "/api/orders", "orders")]);
        assert_eq!(t.lookup("GET", "/api/orders"), Some("orders"));
        assert_eq!(t.lookup("POST", "/api/orders"), None);
        assert_eq!(t.lookup("GET", "/api/orders/1"), None);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn prefix_match_boundaries() {
        let t = bound(&[("GET", "/api/items/*", "items")]);
        assert_eq!(t.lookup("GET", "/api/items"), Some("items"));
        assert_eq!(t.lookup("GET", "/api/items/42"), Some("items"));
        assert_eq!(t.lookup("GET", "/api/itemsx"), None);
        assert_eq!(t.lookup("GET", "/api"), None);
    }

    #[test]
    fn longest_prefix_wins_and_methods_partition() {
        let t = bound(&[
            ("GET", "/*", "root"),
            ("GET", "/api/*", "api"),
            ("GET", "/api/orders/*", "orders"),
            ("POST", "/api/orders", "poster"),
        ]);
        assert_eq!(t.lookup("GET", "/x"), Some("root"));
        assert_eq!(t.lookup("GET", "/api"), Some("api"));
        // The deeper prefix owns the bare path AND everything below it.
        assert_eq!(t.lookup("GET", "/api/orders"), Some("orders"));
        assert_eq!(t.lookup("GET", "/api/orders/1"), Some("orders"));
        // Method partitioning: only POST is bound exactly.
        assert_eq!(t.lookup("POST", "/api/orders"), Some("poster"));
        assert_eq!(t.lookup("POST", "/api/orders/1"), None);
        assert_eq!(t.lookup("DELETE", "/api/orders"), None);
    }

    /// `/api/orders` and `/api/orders/*` name the same slot: the later
    /// binding replaces the earlier, whichever shape it takes.
    #[test]
    fn exact_and_prefix_shapes_replace_each_other() {
        let mut t = bound(&[("GET", "/api/orders", "exact")]);
        assert_eq!(t.lookup("GET", "/api/orders/1"), None);
        t = t.bind("GET", "/api/orders/*", "prefix").unwrap();
        assert_eq!(t.lookup("GET", "/api/orders"), Some("prefix"));
        assert_eq!(t.lookup("GET", "/api/orders/1"), Some("prefix"));
        assert_eq!(t.len(), 1);
        t = t.bind("GET", "/api/orders", "exact2").unwrap();
        assert_eq!(t.lookup("GET", "/api/orders"), Some("exact2"));
        assert_eq!(t.lookup("GET", "/api/orders/1"), None);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn rebind_replaces_and_removes_shadowed_prefix() {
        let mut t = bound(&[("GET", "/api", "a")]);
        t = t.bind("GET", "/api", "b").unwrap();
        assert_eq!(t.lookup("GET", "/api"), Some("b"));
        assert_eq!(t.len(), 1);
        // Binding a prefix on a path that had an exact entry replaces it.
        t = t.bind("GET", "/api/*", "c").unwrap();
        assert_eq!(t.lookup("GET", "/api"), Some("c"));
        assert_eq!(t.lookup("GET", "/api/x"), Some("c"));
        assert_eq!(t.entries().len(), 1);
        assert_eq!(t.entries()[0].path, "/api/*");
    }

    #[test]
    fn invalid_input_rejected() {
        let t = DynamicRoutes::new();
        assert!(t.bind("", "/x", "m").is_err());
        assert!(t.bind("BAD METHOD", "/x", "m").is_err());
        assert!(t.bind("GET", "no-slash", "m").is_err());
        assert!(t.bind("GET", "/x", "../evil").is_err());
        assert!(t.bind("GET", "/x?q", "m").is_err());
    }

    #[test]
    fn table_cap_enforced() {
        let mut t = DynamicRoutes::new();
        for i in 0..MAX_BINDINGS {
            t = t.bind("GET", &format!("/p{i}"), "m").unwrap();
        }
        assert!(t.bind("GET", "/overflow", "m").is_err());
        // Rebinding an existing key still fits (no growth).
        assert!(t.bind("GET", "/p0", "m2").is_ok());
    }

    #[test]
    fn unbind_and_unbind_module() {
        let t = bound(&[
            ("GET", "/a", "m1"),
            ("POST", "/b", "m1"),
            ("GET", "/c/*", "m2"),
        ]);
        let t = t.unbind("get", "/a");
        assert_eq!(t.lookup("GET", "/a"), None);
        assert_eq!(t.lookup("POST", "/b"), Some("m1"));
        let (t, removed) = t.unbind_module("m1");
        assert_eq!(removed, 1);
        assert_eq!(t.lookup("POST", "/b"), None);
        assert_eq!(t.lookup("GET", "/c/x"), Some("m2"));
        let (_, removed) = t.unbind_module("nope");
        assert_eq!(removed, 0);
    }

    #[test]
    fn reconcile_enforces_config_and_keeps_runtime_keys() {
        let cfg1 = vec![DynamicRoute {
            method: "get".into(),
            path: "/cfg".into(),
            module: "cfgmod".into(),
        }];
        let mut t = DynamicRoutes::new().reconcile(&cfg1);
        t = t.bind("GET", "/runtime", "rtmod").unwrap();
        assert_eq!(t.lookup("GET", "/cfg"), Some("cfgmod"));
        assert_eq!(t.lookup("GET", "/runtime"), Some("rtmod"));

        // Same config again: nothing changes.
        let t2 = t.reconcile(&cfg1);
        assert_eq!(t2.lookup("GET", "/runtime"), Some("rtmod"));

        // Config drops /cfg and adds /cfg2: stale key goes, runtime key
        // survives, config wins on collision with a runtime binding.
        let cfg2 = vec![DynamicRoute {
            method: "GET".into(),
            path: "/runtime".into(),
            module: "cfgwins".into(),
        }];
        let t3 = t.reconcile(&cfg2);
        assert_eq!(t3.lookup("GET", "/cfg"), None);
        assert_eq!(t3.lookup("GET", "/runtime"), Some("cfgwins"));

        // Empty config: only config-owned keys are dropped.
        let t4 = t.reconcile(&[]);
        assert_eq!(t4.lookup("GET", "/runtime"), Some("rtmod"));
        assert_eq!(t4.lookup("GET", "/cfg"), None);
    }
}
