//! Ingress watch wiring: apiserver snapshots -> rendered config -> runtime.
//!
//! The library side (`openrusty-k8s`) owns credentials, watching and
//! rendering; this module is the glue that turns that machinery into a
//! live config plane:
//!
//! 1. `run` builds one k8s [`Client`] from the `[ingress]` config
//!    (credentials are never stored in the gateway config - `kubeconfig`
//!    is a path only). Failure here is logged and the task exits: ingress
//!    is an *enhancement*, never a dependency - the gateway keeps serving
//!    its static config.
//! 2. One [`watch_loop`] per resource path runs in its own task
//!    (cluster-wide, or one namespaced loop per configured namespace).
//!    Each hand-over updates the loop's slot and - after merging the
//!    per-loop slots into a single deterministic snapshot - the *latest
//!    pair* of (Ingress snapshot, Secret snapshot) is published through a
//!    `tokio::sync::watch` channel: the single synthetic snapshot the
//!    apply side works from.
//! 3. One merge task consumes that channel. A second debounce window
//!    ([`MERGE_DEBOUNCE`]) folds the two loops' independent hand-overs
//!    (they debounce separately) into one render+apply. Every apply is
//!    all-or-nothing: any render error (including a missing TLS secret)
//!    or a static/rendered route [`Conflict`] is logged and **rejected
//!    without touching the runtime** - the previously applied config stays
//!    authoritative (stale-serve semantics carried over from the k8s
//!    crate). On success [`build_ingress_config`] composes the new
//!    `Config` (static routes first, rendered after; upstreams are the
//!    static set plus the rendered ClusterIP upstreams) and
//!    `state::apply_runtime` publishes it atomically at the *current
//!    plugin registry generation* - a route swap never recompiles plugins.
//!
//! Interplay with file reloads (documented simplification): the merge base
//! is `AppState.static_config`, the boot-time config. A file reload swaps
//! the runtime directly; the next cluster hand-over re-composes on the
//! boot-time static base, so a reload can drop rendered routes until the
//! cluster changes again.
//!
//! Shutdown: the spawned loops park forever in their watch streams; the
//! process exit drops the runtime and reclaims them, so no extra
//! bookkeeping is needed (k8s connections are pooled and short-lived
//! enough to simply die with the process).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openrusty_core::config::{Config, IngressConfig, RouteConfig, UpstreamConfig};
use openrusty_k8s::model::{Ingress, Secret};
use openrusty_k8s::render::{merge, render_routes, render_tls, render_upstreams, TlsPair};
use openrusty_k8s::snapshot::{replace_from_list, Snapshot};
use openrusty_k8s::{watch_loop, Client, ResourceMeta, WatchOptions, WatchStats};

use crate::state::{self, AppState};

/// Second fold window in front of render+apply: the Ingress and Secret
/// loops debounce independently, so their hand-overs can land back-to-back
/// (e.g. an Ingress plus the secret it references). Waiting this long
/// before rendering usually collapses that burst into a single apply.
pub const MERGE_DEBOUNCE: Duration = Duration::from_millis(200);

/// API root of the Ingress resource.
const INGRESS_API: &str = "/apis/networking.k8s.io/v1";
/// API root of the Secret resource.
const SECRET_API: &str = "/api/v1";
/// Only TLS secrets matter for ingress rendering; percent-encoded
/// fieldSelector so the LIST/WATCH payloads stay small.
const TLS_SECRET_SELECTOR: &str = "fieldSelector=type%3Dkubernetes.io%2Ftls";

/// Observable ingress-plane status, published into [`AppState`] and read
/// by `/openrusty/status`. `enabled` mirrors the config switch, `watching`
/// flips once the k8s client exists and the loops actually run.
#[derive(Debug, Clone, Default)]
pub struct WatchStatus {
    pub enabled: bool,
    pub watching: bool,
    /// Hand-over counters of the Ingress loop(s); None before the first
    /// successful LIST.
    pub ingresses: Option<WatchStats>,
    /// Hand-over counters of the TLS Secret loop(s).
    pub secrets: Option<WatchStats>,
}

/// Which loop a stats update belongs to.
#[derive(Debug, Clone, Copy)]
enum Which {
    Ingresses,
    Secrets,
}

/// The `ingress` node of the status JSON (pure; snake_case keys).
pub fn status_node(status: &WatchStatus) -> serde_json::Value {
    if !status.enabled {
        return serde_json::json!({ "enabled": false });
    }
    let mut node = serde_json::json!({ "enabled": true, "watching": status.watching });
    if let serde_json::Value::Object(fields) = &mut node {
        if let serde_json::Value::Object(stats) = stats_json(status.ingresses.as_ref()) {
            fields.extend(stats);
        }
        fields.insert("secrets".to_string(), stats_json(status.secrets.as_ref()));
    }
    node
}

/// Flat stats shape shared by both loops (pure).
fn stats_json(stats: Option<&WatchStats>) -> serde_json::Value {
    match stats {
        None => serde_json::json!({
            "generation": 0,
            "last_rv": "",
            "reconnects": 0,
            "last_success_age_ms": serde_json::Value::Null,
        }),
        Some(s) => serde_json::json!({
            "generation": s.generation,
            "last_rv": s.last_rv,
            "reconnects": s.reconnects,
            "last_success_age_ms": s.last_success.map(|t| t.elapsed().as_millis() as u64),
        }),
    }
}

/// List/watch paths for `resource`: cluster-wide when `namespaces` is
/// empty, one namespaced path per entry otherwise (pure).
fn resource_paths(api: &str, resource: &str, query: &str, namespaces: &[String]) -> Vec<String> {
    let tail = if query.is_empty() {
        resource.to_string()
    } else {
        format!("{resource}?{query}")
    };
    if namespaces.is_empty() {
        return vec![format!("{api}/{tail}")];
    }
    namespaces
        .iter()
        .map(|ns| format!("{api}/namespaces/{ns}/{tail}"))
        .collect()
}

/// Ingress LIST/WATCH paths for the configured namespaces (pure).
fn ingress_paths(namespaces: &[String]) -> Vec<String> {
    resource_paths(INGRESS_API, "ingresses", "", namespaces)
}

/// Secret LIST/WATCH paths, restricted to `kubernetes.io/tls` (pure).
fn secret_paths(namespaces: &[String]) -> Vec<String> {
    resource_paths(SECRET_API, "secrets", TLS_SECRET_SELECTOR, namespaces)
}

/// Latest snapshot pair, kept as ONE synthetic value so a render always
/// sees a consistent (ingress, secret) view.
type Pair = (Snapshot<Ingress>, Snapshot<Secret>);

/// Per-loop snapshot slots: slot i holds loop i's latest snapshot (None
/// before its first LIST). Shared by the closures of one resource kind.
type Slots<T> = Arc<Mutex<Vec<Option<Snapshot<T>>>>>;

/// Fold `incoming` into slot `index` and re-derive the combined snapshot.
/// Slots are namespace-disjoint by construction (one namespaced loop per
/// configured namespace), so a BTreeMap re-merge is lossless and
/// deterministic. The combined resource version tracks the hand-over that
/// triggered the fold (informational, feeds status only).
fn fold_slot<T: ResourceMeta + Clone>(
    slots: &Slots<T>,
    index: usize,
    incoming: &Snapshot<T>,
) -> Snapshot<T> {
    let mut guard = slots.lock().unwrap_or_else(|e| e.into_inner());
    guard[index] = Some(incoming.clone());
    let items: Vec<T> = guard
        .iter()
        .flatten()
        .flat_map(|snap| snap.iter().map(|(_, item)| item.clone()))
        .collect();
    let rv = incoming.resource_version().to_string();
    replace_from_list(items, &rv)
}

/// Publish one loop's hand-over counters into the shared status (rare
/// enough that `rcu` compare-and-swap is the simplest race-free write).
fn record_stats(state: &AppState, which: Which, stats: &WatchStats) {
    let stats = stats.clone();
    state.ingress.rcu(|current| {
        let mut next = WatchStatus::clone(current);
        match which {
            Which::Ingresses => next.ingresses = Some(stats.clone()),
            Which::Secrets => next.secrets = Some(stats.clone()),
        }
        next
    });
}

/// Flip the `watching` flag once the client exists and the loops run.
fn set_status_watching(state: &AppState) {
    state.ingress.rcu(|current| {
        let mut next = WatchStatus::clone(current);
        next.watching = true;
        next
    });
}

/// Run the ingress config plane. Never blocks serving: call sites spawn
/// this; a credentials or client failure logs an error and ends the task,
/// leaving the gateway on its static config.
pub async fn run(state: Arc<AppState>, ingress: IngressConfig, static_routes: Vec<RouteConfig>) {
    state.ingress.rcu(|current| {
        let mut next = WatchStatus::clone(current);
        next.enabled = true;
        next
    });

    let kubeconfig = if ingress.kubeconfig.trim().is_empty() {
        None
    } else {
        Some(Path::new(ingress.kubeconfig.trim()))
    };
    let (cluster, credentials) = match openrusty_k8s::load(kubeconfig) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(error = %e, "ingress watch: no usable cluster credentials; staying on the static config");
            return;
        }
    };
    let client = match Client::new(&cluster, &credentials) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            tracing::error!(error = %e, "ingress watch: apiserver client build failed; staying on the static config");
            return;
        }
    };

    let class = ingress.ingress_class.clone();
    let ing_paths = ingress_paths(&ingress.namespaces);
    let sec_paths = secret_paths(&ingress.namespaces);
    let (pair_tx, pair_rx) = tokio::sync::watch::channel(Pair::default());
    let ing_slots: Slots<Ingress> = Arc::new(Mutex::new(vec![None; ing_paths.len()]));
    let sec_slots: Slots<Secret> = Arc::new(Mutex::new(vec![None; sec_paths.len()]));

    set_status_watching(&state);

    for (index, path) in ing_paths.iter().enumerate() {
        let state = state.clone();
        let slots = ing_slots.clone();
        let tx = pair_tx.clone();
        let client = client.clone();
        let opts = WatchOptions::new(class.clone());
        let path = path.clone();
        tokio::spawn(async move {
            watch_loop(client.as_ref(), &path, opts, move |snap, stats| {
                record_stats(&state, Which::Ingresses, stats);
                let merged = fold_slot(&slots, index, snap);
                tx.send_if_modified(|pair| {
                    pair.0 = merged;
                    true
                });
            })
            .await;
        });
    }
    for (index, path) in sec_paths.iter().enumerate() {
        let state = state.clone();
        let slots = sec_slots.clone();
        let tx = pair_tx.clone();
        let client = client.clone();
        let opts = WatchOptions::new(class.clone());
        let path = path.clone();
        tokio::spawn(async move {
            watch_loop(client.as_ref(), &path, opts, move |snap, stats| {
                record_stats(&state, Which::Secrets, stats);
                let merged = fold_slot(&slots, index, snap);
                tx.send_if_modified(|pair| {
                    pair.1 = merged;
                    true
                });
            })
            .await;
        });
    }

    // The single consumer: debounced render + all-or-nothing apply. Ends
    // only when every sender is gone (process shutdown).
    let apply_state = state.clone();
    let base = state.static_config.clone();
    let apply_class = class.clone();
    tokio::spawn(async move {
        let mut rx = pair_rx;
        while rx.changed().await.is_ok() {
            tokio::time::sleep(MERGE_DEBOUNCE).await;
            let pair = rx.borrow_and_update().clone();
            apply_pair(&apply_state, &base, &static_routes, &apply_class, &pair).await;
        }
    });

    tracing::info!(
        class = %class,
        ingress_paths = ?ing_paths,
        "ingress watch running"
    );
}

/// One synthetic-snapshot apply: render, merge, compose, publish. Every
/// failure path logs and returns WITHOUT touching the runtime.
async fn apply_pair(
    state: &AppState,
    base: &Config,
    static_routes: &[RouteConfig],
    class: &str,
    pair: &Pair,
) {
    let rendered_routes = match render_routes(&pair.0, class) {
        Ok(routes) => routes,
        Err(e) => {
            tracing::warn!(error = %e, "ingress render failed; keeping previous runtime");
            return;
        }
    };
    let rendered_upstreams = match render_upstreams(&pair.0, class) {
        Ok(upstreams) => upstreams,
        Err(e) => {
            tracing::warn!(error = %e, "ingress upstream render failed; keeping previous runtime");
            return;
        }
    };
    let tls = match render_tls(&pair.0, &pair.1, class) {
        Ok(tls) => tls,
        Err(e) => {
            tracing::warn!(error = %e, "ingress TLS render failed; keeping previous runtime");
            return;
        }
    };
    let merged_routes = match merge(static_routes.to_vec(), rendered_routes) {
        Ok(routes) => routes,
        Err(conflict) => {
            tracing::warn!(
                host = ?conflict.key.host,
                path = %conflict.key.path_prefix,
                exact = conflict.key.exact,
                a = %conflict.a,
                b = %conflict.b,
                "static/ingress route conflict; keeping previous runtime"
            );
            return;
        }
    };
    let cfg = build_ingress_config(base, merged_routes, rendered_upstreams, &tls);
    // DNS endpoints (service-discovered upstreams) fold into concrete
    // peers here, off the request path; an unresolved service leaves
    // that upstream peer-less until the next watch event re-renders.
    let cfg = openrusty_proxy::resolve::resolve_config(&cfg).await;
    // Outbound TLS material is built before anything is published; a
    // failure keeps the previous runtime, like every other render error.
    let tls_plans = match state::build_tls_plans(&cfg) {
        Ok(plans) => plans,
        Err(e) => {
            tracing::warn!(error = %e, "ingress upstream TLS build failed; keeping previous runtime");
            return;
        }
    };
    // Route hot-swap only: reuse the current plugin snapshot generation so
    // the runtime can never disagree with the compiled plugin set.
    let generation = state.registry.snapshot().generation;
    state::apply_runtime(state, &cfg, generation, &tls_plans);
    // TLS: publish the freshly rendered secrets into the shared SNI
    // resolver. Broken entries are skipped inside (one bad Secret must
    // not sink the table), and the swap only affects new handshakes -
    // established connections keep their session certificates.
    if let Some(resolver) = &state.tls_resolver {
        let hosts = crate::tls::update_from_tls_pairs(resolver, &tls);
        tracing::info!(hosts, "ingress TLS material published to the SNI resolver");
    }
}

/// Compose the applied config from the static base and the rendered view
/// (pure). Routes arrive already merged by the caller (`merge` puts static
/// first, rendered after). Upstreams are the base set plus rendered ones
/// whose name is free; a rendered name that collides with a static
/// upstream is dropped in favor of the static definition. Everything else
/// (server, plugins, the `[ingress]` segment) carries over unchanged.
fn build_ingress_config(
    base: &Config,
    routes: Vec<RouteConfig>,
    rendered_upstreams: Vec<UpstreamConfig>,
    _tls: &BTreeMap<String, TlsPair>,
) -> Config {
    // TLS material is fully validated at render time; the runtime config
    // grows a TLS termination section in a later milestone.
    let mut upstreams = base.upstreams.clone();
    let taken: HashSet<String> = upstreams.iter().map(|u| u.name.clone()).collect();
    upstreams.extend(
        rendered_upstreams
            .into_iter()
            .filter(|u| !taken.contains(&u.name)),
    );
    let mut cfg = base.clone();
    cfg.routes = routes;
    cfg.upstreams = upstreams;
    cfg
}

#[cfg(test)]
mod tests;
