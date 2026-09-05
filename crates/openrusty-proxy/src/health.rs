//! Health checking, keyed by upstream name: a passive plane (nginx
//! `max_fails` / `fail_timeout` style, driven by request outcomes) and an
//! active plane (proactive probes, lua-resty-upstream-healthcheck style,
//! driven by [`record_probe`]).
//!
//! State is stored in a registry that outlives config snapshots, so peer
//! health SURVIVES config reloads: re-registering an upstream remaps the
//! per-peer counters and down state by ADDRESS, so peers keep their history
//! across reordering, additions and removals.
//!
//! Passive model per peer (nginx `max_fails` / `fail_timeout` style):
//! - failures are counted inside a sliding window of `fail_window_s`;
//! - a failure outside the window resets the window and the counter;
//! - reaching `max_fails` marks the peer down for `fail_timeout_s`
//!   (`max_fails=0` disables passive accounting, as in nginx);
//! - a success clears the failure counter.
//!
//! Active model per (upstream, addr) pair (consecutive counters):
//! - a probe success increments `successes` and resets `fails`; a healthy
//!   peer stays healthy, a down peer recovers once
//!   `successes >= healthy_threshold`;
//! - a probe failure increments `fails` and resets `successes`; the peer
//!   becomes unhealthy once `fails >= unhealthy_threshold`;
//! - unknown (upstream, addr) pairs are treated as healthy (fail-open).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use openrusty_core::config::HealthConfig;

/// Mutable failure accounting for one peer.
#[derive(Debug, Default, Clone)]
struct PeerCounters {
    /// Consecutive failures inside the current window.
    fails: u32,
    /// When the current failure window started (ms since epoch-ish clock).
    window_start_ms: u64,
}

/// Per-peer health state. Interior mutability: the registry hands out
/// shared references that outlive individual requests.
#[derive(Debug)]
pub struct PeerHealth {
    counters: Mutex<PeerCounters>,
    /// Peer is considered down while `now_ms < down_until_ms`.
    down_until_ms: AtomicU64,
    /// Active-plane verdict for this index slot, mirrored by
    /// [`record_probe`] so index-based call sites see it without signature
    /// changes. Starts healthy (fail-open).
    active_ok: AtomicBool,
}

impl Clone for PeerHealth {
    fn clone(&self) -> Self {
        PeerHealth {
            counters: Mutex::new(self.counters.lock().unwrap_or_else(|e| e.into_inner()).clone()),
            down_until_ms: AtomicU64::new(self.down_until_ms.load(Ordering::Relaxed)),
            active_ok: AtomicBool::new(self.active_ok.load(Ordering::Relaxed)),
        }
    }
}

/// One registered upstream: the peer addresses of the snapshot this slot
/// block was built from plus the matching passive slots. Addresses are
/// stored so a reload can remap state by ADDRESS (see [`register`]).
#[derive(Debug)]
struct PeerSlots {
    addrs: Vec<SocketAddr>,
    peers: Vec<PeerHealth>,
}

/// Active-plane health for one (upstream, addr) pair.
struct ActivePeer {
    /// Current verdict; starts healthy (fail-open).
    healthy: AtomicBool,
    /// Consecutive failed probes.
    fails: AtomicU32,
    /// Consecutive successful probes.
    successes: AtomicU32,
}

/// Registry of per-upstream peer health state. Cheaply shared via `&`.
pub struct HealthRegistry {
    upstreams: Mutex<HashMap<String, Arc<PeerSlots>>>,
    /// Active-plane state keyed by (upstream name, peer addr). Entries for
    /// peers removed from config stay behind: they are never probed and the
    /// index-based paths never consult them, so they are harmless.
    active: Mutex<HashMap<(String, String), Arc<ActivePeer>>>,
}

/// Create an empty registry.
pub fn new() -> HealthRegistry {
    HealthRegistry {
        upstreams: Mutex::new(HashMap::new()),
        active: Mutex::new(HashMap::new()),
    }
}

impl Default for HealthRegistry {
    fn default() -> Self {
        new()
    }
}

/// Register (or refresh) an upstream's passive health slots.
///
/// State is remapped by ADDRESS, not by index: a peer whose address existed
/// in the previous list inherits its failure counters and `down_until`
/// (passive plane) whatever its new position, so a reload neither re-punishes
/// a healthy address nor resurrects a downed one by shuffling order.
/// Brand-new addresses start fresh (healthy); removed addresses are dropped.
/// The active-plane verdict (keyed by address already) is backfilled into
/// the per-index slot so [`is_healthy`] keeps seeing it. An unknown
/// upstream registers fresh, healthy slots.
pub fn register(h: &HealthRegistry, upstream: &str, addrs: &[SocketAddr]) {
    let mut map = h.upstreams.lock().unwrap_or_else(|e| e.into_inner());
    // Value-copy the previous per-address state; slots are behind an `Arc`
    // shared with in-flight requests, so they cannot be moved out.
    let previous: HashMap<SocketAddr, PeerHealth> = match map.get(upstream) {
        Some(slots) => slots
            .addrs
            .iter()
            .zip(slots.peers.iter())
            .map(|(addr, peer)| (*addr, peer.clone()))
            .collect(),
        None => HashMap::new(),
    };
    let active = h.active.lock().unwrap_or_else(|e| e.into_inner());
    let peers = addrs
        .iter()
        .map(|addr| {
            let peer = previous.get(addr).cloned().unwrap_or(PeerHealth {
                counters: Mutex::new(PeerCounters::default()),
                down_until_ms: AtomicU64::new(0),
                active_ok: AtomicBool::new(true),
            });
            // Backfill the active-plane judgment for this address; unknown
            // addresses are healthy (fail-open), matching `record_probe`.
            let verdict = match active.get(&(upstream.to_string(), addr.to_string())) {
                Some(p) => p.healthy.load(Ordering::Relaxed),
                None => true,
            };
            peer.active_ok.store(verdict, Ordering::Relaxed);
            peer
        })
        .collect();
    drop(active);
    map.insert(
        upstream.to_string(),
        Arc::new(PeerSlots {
            addrs: addrs.to_vec(),
            peers,
        }),
    );
}

/// Shared handle to one registered peer, if present.
fn lookup(h: &HealthRegistry, upstream: &str, idx: usize) -> Option<Arc<PeerSlots>> {
    let map = h.upstreams.lock().unwrap_or_else(|e| e.into_inner());
    map.get(upstream).filter(|slots| idx < slots.peers.len()).cloned()
}

/// True when the peer is not currently marked down.
/// A peer is unhealthy when EITHER plane says so: passive down-until has
/// not expired, or the active probe verdict is unhealthy. Unknown
/// upstreams/indices are treated as healthy (fail-open).
pub fn is_healthy(h: &HealthRegistry, upstream: &str, idx: usize, now_ms: u64) -> bool {
    match lookup(h, upstream, idx) {
        Some(slots) => {
            let p = &slots.peers[idx];
            p.down_until_ms.load(Ordering::Relaxed) <= now_ms && p.active_ok.load(Ordering::Relaxed)
        }
        None => true,
    }
}

/// All peer indices in `0..peer_count` currently considered healthy.
/// An unregistered upstream yields every index.
pub fn healthy_indices(
    h: &HealthRegistry,
    upstream: &str,
    peer_count: usize,
    now_ms: u64,
) -> Vec<usize> {
    let map = h.upstreams.lock().unwrap_or_else(|e| e.into_inner());
    let Some(slots) = map.get(upstream) else {
        return (0..peer_count).collect();
    };
    (0..peer_count)
        .filter(|i| {
            slots.peers.get(*i).is_none_or(|p| {
                p.down_until_ms.load(Ordering::Relaxed) <= now_ms
                    && p.active_ok.load(Ordering::Relaxed)
            })
        })
        .collect()
}

/// Pure decision core of [`record_failure`].
///
/// `fails_after` is the failure count AFTER the new failure was added to the
/// window that started at `window_start`. Returns `Some(down_until_ms)` when
/// the peer must be marked down. When the window is stale (older than
/// `fail_window_s`) it returns `None`; the caller must then reset the window
/// and the counter before counting the failure.
pub fn evaluate_failure(
    fails_after: u32,
    window_start: u64,
    now_ms: u64,
    cfg: &HealthConfig,
) -> Option<u64> {
    // nginx semantics: `max_fails=0` disables passive accounting entirely;
    // without this guard `fails_after >= 0` would trip on the first failure.
    if cfg.max_fails == 0 {
        return None;
    }
    let window_ms = cfg.fail_window_s.saturating_mul(1000);
    if now_ms.saturating_sub(window_start) > window_ms {
        // Stale window: signal the caller to reset; do not trip the peer.
        return None;
    }
    if fails_after >= cfg.max_fails {
        Some(now_ms + cfg.fail_timeout_s.saturating_mul(1000))
    } else {
        None
    }
}

/// Record an upstream failure for peer `idx`.
///
/// Applies the windowed counter; when `max_fails` is reached inside the
/// window the peer is marked down for `fail_timeout_s` and the counter is
/// reset (the next failure starts a fresh accounting). `max_fails=0`
/// disables passive accounting: failures are not counted at all.
pub fn record_failure(
    h: &HealthRegistry,
    upstream: &str,
    idx: usize,
    cfg: &HealthConfig,
    now_ms: u64,
) {
    if cfg.max_fails == 0 {
        return;
    }
    let Some(peers) = lookup(h, upstream, idx) else {
        return;
    };
    let peer = &peers.peers[idx];
    let mut counters = peer.counters.lock().unwrap_or_else(|e| e.into_inner());
    let window_ms = cfg.fail_window_s.saturating_mul(1000);
    if now_ms.saturating_sub(counters.window_start_ms) > window_ms {
        counters.fails = 0;
        counters.window_start_ms = now_ms;
    }
    counters.fails += 1;
    if let Some(down_until) =
        evaluate_failure(counters.fails, counters.window_start_ms, now_ms, cfg)
    {
        peer.down_until_ms.store(down_until, Ordering::Relaxed);
        counters.fails = 0;
    }
}

/// Record a success for peer `idx`: clears the failure counter and closes
/// the current failure window. It does NOT lift an active down marking;
/// the peer returns when its `fail_timeout` expires.
pub fn record_success(h: &HealthRegistry, upstream: &str, idx: usize, now_ms: u64) {
    let Some(peers) = lookup(h, upstream, idx) else {
        return;
    };
    let mut counters = peers.peers[idx].counters.lock().unwrap_or_else(|e| e.into_inner());
    counters.fails = 0;
    counters.window_start_ms = now_ms;
}

/// Pure decision core of [`record_probe`]: apply one probe outcome to the
/// consecutive counters and return the new `(healthy, fails, successes)`.
///
/// - success: `successes` grows, `fails` resets; a healthy peer stays
///   healthy, a down one recovers once `successes >= healthy_threshold`;
/// - failure: `fails` grows, `successes` resets; a down peer stays down,
///   a healthy one turns unhealthy once `fails >= unhealthy_threshold`.
pub fn evaluate_active(
    healthy: bool,
    fails: u32,
    successes: u32,
    ok: bool,
    unhealthy_threshold: u32,
    healthy_threshold: u32,
) -> (bool, u32, u32) {
    if ok {
        let successes = successes + 1;
        // A success never un-healths a healthy peer: the threshold only
        // gates recovery from a down state. Without the `healthy` term a
        // freshly-booted peer's first successful probe would silently mark
        // it down until the threshold accumulates.
        (healthy || successes >= healthy_threshold, 0, successes)
    } else {
        let fails = fails + 1;
        // Symmetric guard: a failure never heals a down peer; only the
        // success threshold may lift it. `fails >= threshold` un-healths.
        (healthy && fails < unhealthy_threshold, fails, 0)
    }
}

/// Record one active-probe outcome for peer `idx` at `addr` and mirror the
/// resulting healthy flag into the per-index passive slot, so existing
/// [`is_healthy`] / [`healthy_indices`] call sites pick it up without
/// signature changes.
///
/// Returns the peer's active-plane healthy state after the update.
pub fn record_probe(
    h: &HealthRegistry,
    upstream: &str,
    idx: usize,
    addr: &str,
    ok: bool,
    unhealthy_threshold: u32,
    healthy_threshold: u32,
) -> bool {
    let key = (upstream.to_string(), addr.to_string());
    let peer = h
        .active
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key)
        .or_insert_with(|| {
            Arc::new(ActivePeer {
                healthy: AtomicBool::new(true),
                fails: AtomicU32::new(0),
                successes: AtomicU32::new(0),
            })
        })
        .clone();
    let (healthy, fails, successes) = evaluate_active(
        peer.healthy.load(Ordering::Relaxed),
        peer.fails.load(Ordering::Relaxed),
        peer.successes.load(Ordering::Relaxed),
        ok,
        unhealthy_threshold,
        healthy_threshold,
    );
    peer.healthy.store(healthy, Ordering::Relaxed);
    peer.fails.store(fails, Ordering::Relaxed);
    peer.successes.store(successes, Ordering::Relaxed);
    // Mirror into the per-index passive slot when the upstream is known.
    if let Some(slots) = lookup(h, upstream, idx) {
        slots.peers[idx].active_ok.store(healthy, Ordering::Relaxed);
    }
    healthy
}

/// Active-plane verdict for one (upstream, addr) pair.
/// Unknown pairs are treated as healthy (fail-open).
pub fn is_active_healthy(h: &HealthRegistry, upstream: &str, addr: &str) -> bool {
    h.active
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(upstream.to_string(), addr.to_string()))
        .is_none_or(|p| p.healthy.load(Ordering::Relaxed))
}

/// All (upstream, addr, healthy) triples from the active plane, sorted by
/// (upstream, addr) for deterministic output.
pub fn active_peers(h: &HealthRegistry) -> Vec<(String, String, bool)> {
    let mut out: Vec<(String, String, bool)> = h
        .active
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|((upstream, addr), p)| {
            (
                upstream.clone(),
                addr.clone(),
                p.healthy.load(Ordering::Relaxed),
            )
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse one address literal (test helper).
    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// `n` distinct loopback addresses, 127.0.0.1:9001.. (test helper for
    /// the old index-count shape of [`register`]).
    fn addrs_helper(n: usize) -> Vec<SocketAddr> {
        (0..n)
            .map(|i| addr(&format!("127.0.0.1:{}", 9001 + i)))
            .collect()
    }

    fn cfg(max_fails: u32, window_s: u64, timeout_s: u64) -> HealthConfig {
        HealthConfig {
            max_fails,
            fail_window_s: window_s,
            fail_timeout_s: timeout_s,
            active: None,
        }
    }

    #[test]
    fn evaluate_failure_pure_semantics() {
        let c = cfg(3, 10, 30);
        // Below threshold: stays up.
        assert_eq!(evaluate_failure(1, 0, 1_000, &c), None);
        assert_eq!(evaluate_failure(2, 0, 1_000, &c), None);
        // At threshold inside the window: down until now + 30s.
        assert_eq!(evaluate_failure(3, 0, 1_000, &c), Some(31_000));
        // Stale window (started 0, now 11s, window 10s): caller resets.
        assert_eq!(evaluate_failure(3, 0, 11_001, &c), None);
    }

    #[test]
    fn max_fails_zero_disables_passive_accounting() {
        // nginx semantics: `max_fails=0` turns passive accounting off; the
        // peer must never be marked down by request failures.
        let c = cfg(0, 10, 30);
        assert_eq!(evaluate_failure(1, 0, 1_000, &c), None);
        assert_eq!(evaluate_failure(u32::MAX, 0, 1_000, &c), None);

        let h = HealthRegistry::default();
        register(&h, "u", &addrs_helper(1));
        for _ in 0..10 {
            record_failure(&h, "u", 0, &c, 1_000);
        }
        assert!(
            is_healthy(&h, "u", 0, 2_000),
            "max_fails=0 must never trip the peer down"
        );
    }

    #[test]
    fn healthy_by_default() {
        let h = new();
        register(&h, "u", &addrs_helper(3));
        for i in 0..3 {
            assert!(is_healthy(&h, "u", i, 0));
        }
        assert_eq!(healthy_indices(&h, "u", 3, 0), vec![0, 1, 2]);
    }

    #[test]
    fn unknown_upstream_is_fail_open() {
        let h = new();
        assert!(is_healthy(&h, "ghost", 0, 0));
        assert_eq!(healthy_indices(&h, "ghost", 2, 0), vec![0, 1]);
        // Recording on an unknown upstream is a harmless no-op.
        record_failure(&h, "ghost", 0, &cfg(1, 10, 10), 0);
        record_success(&h, "ghost", 0, 0);
    }

    #[test]
    fn single_failure_below_threshold_stays_up() {
        let h = new();
        register(&h, "u", &addrs_helper(2));
        let c = cfg(3, 10, 10);
        record_failure(&h, "u", 0, &c, 1_000);
        assert!(is_healthy(&h, "u", 0, 1_000));
        assert_eq!(healthy_indices(&h, "u", 2, 1_000), vec![0, 1]);
    }

    #[test]
    fn max_fails_within_window_trips_down_then_expires() {
        let h = new();
        register(&h, "u", &addrs_helper(2));
        let c = cfg(3, 10, 10);
        record_failure(&h, "u", 1, &c, 1_000);
        record_failure(&h, "u", 1, &c, 2_000);
        record_failure(&h, "u", 1, &c, 3_000);
        assert!(!is_healthy(&h, "u", 1, 3_000));
        assert_eq!(healthy_indices(&h, "u", 2, 3_000), vec![0]);
        // Still down just before the timeout elapses...
        assert!(!is_healthy(&h, "u", 1, 12_999));
        // ...and up again once fail_timeout (10s after t=3000) passes.
        assert!(is_healthy(&h, "u", 1, 13_000));
        assert_eq!(healthy_indices(&h, "u", 2, 13_000), vec![0, 1]);
    }

    #[test]
    fn stale_window_resets_counts() {
        let h = new();
        register(&h, "u", &addrs_helper(1));
        let c = cfg(2, 1, 60); // 1s window
        record_failure(&h, "u", 0, &c, 0);
        // Second failure arrives after the window expired: counter resets,
        // this counts as the first failure of a new window -> still up.
        record_failure(&h, "u", 0, &c, 2_000);
        assert!(is_healthy(&h, "u", 0, 2_000));
        // A second failure inside the new window trips it.
        record_failure(&h, "u", 0, &c, 2_500);
        assert!(!is_healthy(&h, "u", 0, 2_500));
    }

    #[test]
    fn success_clears_fail_counter() {
        let h = new();
        register(&h, "u", &addrs_helper(1));
        let c = cfg(3, 10, 10);
        record_failure(&h, "u", 0, &c, 1_000);
        record_failure(&h, "u", 0, &c, 1_500);
        record_success(&h, "u", 0, 2_000);
        // After the reset, one more failure is below the threshold.
        record_failure(&h, "u", 0, &c, 2_500);
        assert!(is_healthy(&h, "u", 0, 2_500));
    }

    #[test]
    fn register_keeps_state_when_shape_matches() {
        let h = new();
        register(&h, "u", &addrs_helper(2));
        let c = cfg(1, 10, 60);
        record_failure(&h, "u", 1, &c, 100);
        assert!(!is_healthy(&h, "u", 1, 100));
        // Reload with the same addresses (same order): down state survives.
        register(&h, "u", &addrs_helper(2));
        assert!(!is_healthy(&h, "u", 1, 100));
        // Reload with a brand-new address list: the replaced peer starts
        // fresh while its neighbors keep their state by address.
        register(&h, "u", &addrs_helper(3));
        assert!(is_healthy(&h, "u", 2, 100));
        assert!(!is_healthy(&h, "u", 1, 100));
        assert_eq!(healthy_indices(&h, "u", 3, 100), vec![0, 2]);
    }

    #[test]
    fn register_remaps_passive_state_by_address() {
        let h = new();
        let a = addr("127.0.0.1:9001");
        let b = addr("127.0.0.1:9002");
        let c = addr("127.0.0.1:9003");
        register(&h, "u", &[a, b]);
        let cfg = cfg(1, 10, 60); // one failure is enough to trip the peer
        record_failure(&h, "u", 0, &cfg, 100);
        assert!(!is_healthy(&h, "u", 0, 100));
        // Reordered peer list: `a` keeps its down state at the new index,
        // `b` is untouched, `c` is brand new and starts healthy.
        register(&h, "u", &[b, a, c]);
        assert!(is_healthy(&h, "u", 0, 100), "b must stay healthy");
        assert!(!is_healthy(&h, "u", 1, 100), "a must still be down");
        assert!(is_healthy(&h, "u", 2, 100), "c must start clean");
        // The down marking still expires normally at the new index.
        assert!(is_healthy(&h, "u", 1, 100 + 60_000));
    }

    #[test]
    fn register_backfills_active_verdict_by_address() {
        let h = new();
        let a = addr("127.0.0.1:9001");
        let b = addr("127.0.0.1:9002");
        register(&h, "u", &[a, b]);
        // Probe `a` down (active plane); the verdict mirrors into index 0.
        assert!(!record_probe(&h, "u", 0, "127.0.0.1:9001", false, 1, 1));
        assert!(!is_healthy(&h, "u", 0, 0));
        // Re-register with `a` moved to index 1: the active judgment for
        // the ADDRESS must follow it, and `b` stays healthy.
        register(&h, "u", &[b, a]);
        assert!(is_healthy(&h, "u", 0, 0));
        assert!(!is_healthy(&h, "u", 1, 0));
    }

    #[test]
    fn active_threshold_transitions() {
        let h = new();
        register(&h, "u", &addrs_helper(1));
        let addr = "127.0.0.1:9001";
        // One failure is below the unhealthy threshold -> still healthy.
        assert!(record_probe(&h, "u", 0, addr, false, 2, 2));
        assert!(is_active_healthy(&h, "u", addr));
        assert!(is_healthy(&h, "u", 0, 0));
        // Two consecutive failures trip the peer.
        assert!(!record_probe(&h, "u", 0, addr, false, 2, 2));
        assert!(!is_active_healthy(&h, "u", addr));
        assert!(!is_healthy(&h, "u", 0, 0));
        // A single success resets the failure counter but does not recover.
        assert!(!record_probe(&h, "u", 0, addr, true, 2, 2));
        assert!(!is_healthy(&h, "u", 0, 0));
        // Two consecutive successes recover the peer.
        assert!(record_probe(&h, "u", 0, addr, true, 2, 2));
        assert!(is_active_healthy(&h, "u", addr));
        assert!(is_healthy(&h, "u", 0, 0));
        assert_eq!(healthy_indices(&h, "u", 1, 0), vec![0]);
    }

    #[test]
    fn active_success_resets_fail_counter() {
        let h = new();
        register(&h, "u", &addrs_helper(1));
        let addr = "127.0.0.1:9001";
        // fail, success, fail: never two failures in a row, so the peer
        // stays up throughout. The success resets the failure counter and
        // never un-healths a healthy peer (boot-race regression guard).
        assert!(record_probe(&h, "u", 0, addr, false, 2, 2));
        assert!(record_probe(&h, "u", 0, addr, true, 2, 2));
        assert!(record_probe(&h, "u", 0, addr, false, 2, 2));
        assert!(is_healthy(&h, "u", 0, 0));
        // Second consecutive failure trips it.
        assert!(!record_probe(&h, "u", 0, addr, false, 2, 2));
        assert!(!is_healthy(&h, "u", 0, 0));
    }

    #[test]
    fn active_unknown_pair_is_fail_open() {
        let h = new();
        assert!(is_active_healthy(&h, "ghost", "127.0.0.1:1"));
        // Probing an unknown upstream updates only the addr plane; the
        // missing per-index slot is a harmless no-op mirror.
        assert!(record_probe(&h, "ghost", 0, "127.0.0.1:1", false, 2, 2));
        assert!(is_healthy(&h, "ghost", 0, 0));
        assert!(!record_probe(&h, "ghost", 0, "127.0.0.1:1", false, 2, 2));
        assert!(!is_active_healthy(&h, "ghost", "127.0.0.1:1"));
    }

    #[test]
    fn active_and_passive_planes_both_required() {
        let h = new();
        register(&h, "u", &addrs_helper(1));
        let addr = "127.0.0.1:9001";
        // Passive down (max_fails=1 trips immediately) + active ok -> down.
        let c = cfg(1, 10, 10);
        record_failure(&h, "u", 0, &c, 1_000); // down until 11_000
        record_probe(&h, "u", 0, addr, true, 2, 2);
        record_probe(&h, "u", 0, addr, true, 2, 2); // active plane healthy
        assert!(is_active_healthy(&h, "u", addr));
        assert!(!is_healthy(&h, "u", 0, 1_000));
        assert_eq!(healthy_indices(&h, "u", 1, 1_000), vec![]);
        // Passive recovers when the timeout elapses; active still ok -> up.
        assert!(is_healthy(&h, "u", 0, 11_000));
        // Passive ok + active unhealthy (2 consecutive failures) -> down.
        record_probe(&h, "u", 0, addr, false, 2, 2);
        assert!(!record_probe(&h, "u", 0, addr, false, 2, 2));
        assert!(!is_active_healthy(&h, "u", addr));
        assert!(!is_healthy(&h, "u", 0, 11_000));
        assert_eq!(healthy_indices(&h, "u", 1, 11_000), vec![]);
        // Both ok again: two consecutive successes recover the peer.
        record_probe(&h, "u", 0, addr, true, 2, 2);
        assert!(!is_healthy(&h, "u", 0, 11_000));
        record_probe(&h, "u", 0, addr, true, 2, 2);
        assert!(is_healthy(&h, "u", 0, 11_000));
        assert_eq!(healthy_indices(&h, "u", 1, 11_000), vec![0]);
    }

    #[test]
    fn active_peers_sorted_and_deterministic() {
        let h = new();
        register(&h, "u", &addrs_helper(2));
        record_probe(&h, "u", 0, "127.0.0.1:9001", false, 2, 2);
        record_probe(&h, "u", 0, "127.0.0.1:9001", false, 2, 2); // down
        record_probe(&h, "u", 1, "127.0.0.1:9002", true, 2, 2);
        record_probe(&h, "u", 1, "127.0.0.1:9002", true, 2, 2); // healthy
        record_probe(&h, "v", 0, "127.0.0.1:9003", true, 2, 2);
        record_probe(&h, "v", 0, "127.0.0.1:9003", true, 2, 2); // healthy
        let peers = active_peers(&h);
        assert_eq!(
            peers,
            vec![
                ("u".to_string(), "127.0.0.1:9001".to_string(), false),
                ("u".to_string(), "127.0.0.1:9002".to_string(), true),
                ("v".to_string(), "127.0.0.1:9003".to_string(), true),
            ]
        );
        assert_eq!(active_peers(&h), peers);
    }

    #[test]
    fn evaluate_active_pure_semantics() {
        // Success path: counters grow, healthy only at the threshold.
        assert_eq!(evaluate_active(false, 0, 0, true, 2, 2), (false, 0, 1));
        assert_eq!(evaluate_active(false, 0, 1, true, 2, 2), (true, 0, 2));
        // A healthy (or never-seen-down) peer stays healthy on success;
        // the first boot probe must not silently mark it down.
        assert_eq!(evaluate_active(true, 0, 0, true, 2, 2), (true, 0, 1));
        // A success resets the failure counter.
        assert_eq!(evaluate_active(false, 5, 0, true, 2, 2), (false, 0, 1));
        // Failure path: counters grow, unhealthy only at the threshold.
        assert_eq!(evaluate_active(true, 0, 0, false, 2, 2), (true, 1, 0));
        assert_eq!(evaluate_active(true, 1, 0, false, 2, 2), (false, 2, 0));
        // A failure resets the success counter.
        assert_eq!(evaluate_active(false, 0, 5, false, 2, 2), (false, 1, 0));
    }
}
