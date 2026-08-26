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
    pub started_at: std::time::Instant,
    /// Handle of the active-probe task; cancelled and replaced on reload.
    pub probe_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Empty runtime used before the first [`apply_runtime`] call.
pub fn empty_runtime() -> RuntimeSnapshot {
    RuntimeSnapshot {
        generation: 0,
        routes: Vec::new(),
        upstreams: HashMap::new(),
    }
}

/// Rebuild the runtime snapshot from `cfg` and publish it atomically.
///
/// Upstreams are re-derived from config; health slots are refreshed via
/// `health::register`, which keeps existing peer state when the peer list
/// shape is unchanged. Called only after the plugin registry published a
/// new snapshot, so config and plugins never disagree.
pub fn apply_runtime(state: &AppState, cfg: &Config, generation: u64) {
    let mut upstreams = HashMap::new();
    for uc in &cfg.upstreams {
        let up = proxy::from_config(uc);
        proxy::register(&state.health, &up.name, up.peers.len());
        for p in &up.peers {
            // Pre-create the pooled client so the request path never pays
            // client construction (and connects through a warm keep-alive
            // pool). Must run before `up` moves into `UpstreamRt`.
            proxy::get(
                &state.pool,
                p.addr,
                up.connect_timeout,
                up.pool_idle_timeout,
            );
        }
        let swrr = Mutex::new(vec![0i64; up.peers.len()]);
        upstreams.insert(up.name.clone(), Arc::new(UpstreamRt { up, swrr }));
    }
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
