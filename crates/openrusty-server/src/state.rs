//! Shared gateway state: plugin registry, upstream runtime snapshots,
//! passive health registry and the pooled client cache.
//!
//! `RuntimeSnapshot` is swapped atomically on reload; in-flight requests
//! keep their own `Arc` clone alive for the whole request.

use crate::metrics;
use openrusty_core::config::{Config, RouteConfig};
use openrusty_proxy as proxy;
use openrusty_wasm::PluginRegistry;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Runtime side of one upstream: the immutable snapshot plus the mutable
/// smooth-weighted-round-robin accounting (one effective weight per peer).
pub struct UpstreamRt {
    pub up: proxy::Upstream,
    pub swrr: Mutex<Vec<i64>>,
}

/// Everything a request needs from the current configuration. Rebuilt and
/// published atomically by [`apply_runtime`].
pub struct RuntimeSnapshot {
    /// Plugin snapshot generation this runtime was built from.
    pub generation: u64,
    pub routes: Vec<RouteConfig>,
    pub upstreams: HashMap<String, Arc<UpstreamRt>>,
}

/// Process-wide state shared by every connection and task.
pub struct AppState {
    pub registry: Arc<PluginRegistry>,
    /// Created once at boot; peer health survives reloads.
    pub health: Arc<proxy::HealthRegistry>,
    /// Pooled keep-alive clients, one per peer address.
    pub pool: Arc<proxy::ClientPool>,
    /// Prometheus counters and histogram; created once at boot and
    /// survives reloads like the health registry.
    pub metrics: Arc<metrics::Metrics>,
    pub runtime: arc_swap::ArcSwap<RuntimeSnapshot>,
    pub config_path: PathBuf,
    /// The boot-time static configuration. Base of the ingress render
    /// merge (static upstreams/routes plus the `[ingress]` segment); a
    /// file reload swaps the runtime directly and never rewrites this
    /// base (see `crate::ingress` for the documented interplay).
    pub static_config: Config,
    /// Live status of the optional ingress watch loops (generation
    /// counters, reconnects, last hand-over); read by
    /// `/openrusty/status`, updated rarely via `rcu`. All-default while
    /// ingress is disabled.
    pub ingress: arc_swap::ArcSwap<crate::ingress::WatchStatus>,
    /// Shared SNI certificate resolver for TLS-terminating listeners
    /// (`Some` iff at least one effective listener sets `tls = true`).
    /// Static material is seeded at boot; ingress applies publish
    /// rendered secrets into it. Rotation swaps resolver tables and
    /// never disturbs established connections.
    pub tls_resolver: Option<Arc<crate::tls::DynamicCertResolver>>,
    pub started_at: std::time::Instant,
    /// Handle of the active-probe task; cancelled and replaced on reload.
    pub probe_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Reload gate: SIGHUP and the POST /openrusty/reload endpoint share
    /// this async mutex, so only one reload runs at a time. A second
    /// requester gets the in-flight (conflict) answer instead of queueing.
    pub reload_gate: tokio::sync::Mutex<()>,
    /// Unified shutdown signal (`shutdown::ShutdownSignal`): SIGTERM/SIGINT
    /// and POST /openrusty/shutdown flip it, the accept loops and the
    /// readiness endpoint read it, the counter feeds the drain summary.
    pub shutdown: crate::shutdown::ShutdownSignal,
}

/// Empty runtime used before the first [`apply_runtime`] call.
pub fn empty_runtime() -> RuntimeSnapshot {
    RuntimeSnapshot {
        generation: 0,
        routes: Vec::new(),
        upstreams: HashMap::new(),
    }
}

/// Construct an `AppState` from an already-parsed and validated `Config`.
///
/// The single construction path, shared by the `openrusty` binary, the
/// test helpers and external embedders (init-pro): bootstraps the plugin
/// registry (compiling every plugin before serving), then publishes the
/// first runtime snapshot. `config_path` is only recorded for the
/// file-based reload path; in-memory embedders can pass any placeholder.
///
/// The only fallible step is the plugin bootstrap, so the error type is
/// the registry's own [`openrusty_wasm::ReloadError`]; config parse and
/// validation errors surface earlier, from `openrusty_core::load_config`.
pub fn from_config(
    cfg: Config,
    config_path: PathBuf,
) -> Result<Arc<AppState>, openrusty_wasm::ReloadError> {
    // Fail fast on unreadable/invalid upstream TLS material, before any
    // plugin is compiled; the plans are threaded into `apply_runtime` so
    // the first snapshot is ready to serve https peers.
    let tls_plans =
        build_tls_plans(&cfg).map_err(openrusty_wasm::ReloadError::Io)?;
    let registry = PluginRegistry::bootstrap(&cfg)?;
    // One shared SNI resolver whenever any listener terminates TLS; the
    // material itself is loaded later, in the listener bind phase (so a
    // bad static cert aborts boot fail-fast, not here).
    let tls_resolver = openrusty_core::effective_listeners(&cfg)
        .iter()
        .any(crate::tls::uses_tls)
        .then(|| Arc::new(crate::tls::DynamicCertResolver::new()));
    let state = Arc::new(AppState {
        registry,
        health: Arc::new(proxy::new()),
        pool: Arc::new(proxy::new_pool()),
        metrics: Arc::new(metrics::Metrics::new()),
        runtime: arc_swap::ArcSwap::from_pointee(empty_runtime()),
        config_path,
        static_config: cfg,
        ingress: arc_swap::ArcSwap::from_pointee(crate::ingress::WatchStatus::default()),
        tls_resolver,
        started_at: std::time::Instant::now(),
        probe_task: Mutex::new(None),
        reload_gate: tokio::sync::Mutex::new(()),
        shutdown: crate::shutdown::new_signal(),
    });
    let generation = state.registry.snapshot().generation;
    apply_runtime(&state, &state.static_config, generation, &tls_plans);
    Ok(state)
}

/// Build the outbound TLS material for every upstream that declares an
/// `[upstreams.tls]` section, keyed by upstream name. Fallible on purpose:
/// certificate files are read and parsed here, once per boot/reload, so
/// the request path only ever sees ready-to-use [`proxy::tls::UpstreamTls`]
/// and a bad anchor aborts the swap before anything is published.
pub fn build_tls_plans(
    cfg: &Config,
) -> Result<HashMap<String, proxy::tls::UpstreamTls>, String> {
    let mut plans = HashMap::new();
    for uc in &cfg.upstreams {
        if let Some(tc) = &uc.tls {
            let tls = proxy::tls::build(tc)
                .map_err(|detail| format!("upstream '{}': {}", uc.name, detail))?;
            plans.insert(uc.name.clone(), tls);
        }
    }
    Ok(plans)
}

/// Log non-fatal health advisories for upstreams whose passive-health
/// settings are risky in their deployment shape. Advisory only: nothing in
/// the runtime depends on this, no behavior changes. The known case is
/// Kubernetes ingress rendering: every rendered upstream is named `ing-*`
/// and must keep `max_fails = 0`, because a single ClusterIP endpoint
/// failing `max_fails` times would mark the *whole ClusterIP peer* down
/// (kube-proxy never surfaces per-pod addresses here).
fn log_health_advisories(cfg: &Config) {
    for up in &cfg.upstreams {
        if up.name.starts_with("ing-") && up.health.max_fails > 0 {
            tracing::warn!(
                upstream = %up.name,
                max_fails = up.health.max_fails,
                "passive health on a k8s-rendered upstream: consider max_fails = 0 \
                 (a single ClusterIP endpoint's 5xx must not mark the ClusterIP down)"
            );
        }
    }
}

/// Rebuild the runtime snapshot from `cfg` and publish it atomically.
///
/// Upstreams are re-derived from config. Passive health slots are refreshed
/// via `health::register`, which remaps peer state by ADDRESS: a peer whose
/// address existed before keeps its counters/down state whatever its new
/// index, new addresses start fresh, removed addresses are dropped. The
/// pooled-client cache is pruned to the configured clients (address for
/// http, address + TLS identity for https) so clients of removed peers or
/// of a changed TLS identity cannot linger. Called only after the plugin
/// registry published a new snapshot, so config and plugins never disagree.
///
/// `tls_plans` carries the pre-built outbound TLS material (see
/// [`build_tls_plans`]); upstreams without an entry stay plaintext.
pub fn apply_runtime(
    state: &AppState,
    cfg: &Config,
    generation: u64,
    tls_plans: &HashMap<String, proxy::tls::UpstreamTls>,
) {
    log_health_advisories(cfg);
    let mut upstreams = HashMap::new();
    let mut live_keys: Vec<proxy::PoolKey> = Vec::new();
    for uc in &cfg.upstreams {
        let up = proxy::from_config(uc, tls_plans.get(&uc.name).cloned());
        let peer_addrs: Vec<SocketAddr> = up.peers.iter().map(|p| p.addr).collect();
        proxy::register(&state.health, &up.name, &peer_addrs);
        for p in &up.peers {
            // Pre-create the pooled client so the request path never pays
            // client construction (and connects through a warm keep-alive
            // pool). Must run before `up` moves into `UpstreamRt`.
            match &up.tls {
                Some(tls) => {
                    proxy::get_tls(
                        &state.pool,
                        p.addr,
                        tls,
                        up.connect_timeout,
                        up.pool_idle_timeout,
                    );
                    live_keys.push(proxy::PoolKey::Https(p.addr, tls.key.clone()));
                }
                None => {
                    proxy::get(
                        &state.pool,
                        p.addr,
                        up.connect_timeout,
                        up.pool_idle_timeout,
                    );
                    live_keys.push(proxy::PoolKey::Http(p.addr));
                }
            }
        }
        let swrr = Mutex::new(vec![0i64; up.peers.len()]);
        upstreams.insert(up.name.clone(), Arc::new(UpstreamRt { up, swrr }));
    }
    // Drop pooled clients for addresses that left the configuration (or
    // whose TLS identity changed and therefore got a new pool slot).
    proxy::evict_except(&state.pool, &live_keys);
    let snap = RuntimeSnapshot {
        generation,
        routes: cfg.routes.clone(),
        upstreams,
    };
    state.runtime.store(Arc::new(snap));
    tracing::info!(
        generation,
        routes = cfg.routes.len(),
        upstreams = cfg.upstreams.len(),
        "runtime snapshot published"
    );
}
