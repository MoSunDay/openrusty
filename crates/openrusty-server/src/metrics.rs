//! Prometheus metrics: process-wide counters, a duration histogram and
//! live gauges, rendered as text exposition format 0.0.4 by hand.
//!
//! `Metrics` is the collector: plain counters behind a `Mutex`, std only,
//! no extra dependencies. `snapshot` clones all state cheaply so the
//! render path never holds the collector lock; [`render`] (defined in
//! `metrics_render`) turns a snapshot plus the live peer/KV views into
//! exposition text.

#[path = "metrics_render.rs"]
mod metrics_render;

use std::collections::HashMap;
use std::sync::Mutex;

/// Fixed histogram buckets for `openrusty_request_duration_seconds`,
/// exported as cumulative `le` boundaries in this order. The +Inf bucket
/// is implied by `_count` and never stored.
pub const DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
];

/// Upstream attempt `result` label values, shared with the proxy/balancer
/// wiring that calls [`Metrics::record_attempt`].
pub const RESULT_SUCCESS: &str = "success";
pub const RESULT_CONNECT_FAIL: &str = "connect_fail";
pub const RESULT_TIMEOUT: &str = "timeout";
pub const RESULT_NO_PEER: &str = "no_peer";

/// Plugin error `kind` label values for `openrusty_plugin_errors_total`.
///
/// These are consumed by the plugin runner in `openrusty-wasm`, which
/// cannot depend on this crate and therefore uses the literal strings
/// ("trap", "timeout", "bad_code"); these constants are the documented
/// contract for that cross-crate ABI.
#[allow(dead_code)]
pub const KIND_TRAP: &str = "trap";
#[allow(dead_code)]
pub const KIND_TIMEOUT: &str = "timeout";
#[allow(dead_code)]
pub const KIND_BAD_CODE: &str = "bad_code";

/// Mutable collector state behind the [`Metrics`] mutex.
struct MetricsState {
    /// `openrusty_requests_total{route,code}`.
    requests: HashMap<(String, u16), u64>,
    /// `openrusty_upstream_attempts_total{upstream,result}`.
    attempts: HashMap<(String, String), u64>,
    /// `openrusty_plugin_errors_total{plugin,kind}`.
    plugin_errors: HashMap<(String, String), u64>,
    /// Cumulative per-bucket counts: `buckets[i]` counts observations
    /// `<= DURATION_BUCKETS[i]`. Length always matches the constant.
    buckets: Vec<u64>,
    /// `openrusty_request_duration_seconds_count` (total observations).
    count: u64,
    /// `openrusty_request_duration_seconds_sum`.
    sum: f64,
}

impl MetricsState {
    fn new() -> MetricsState {
        MetricsState {
            requests: HashMap::new(),
            attempts: HashMap::new(),
            plugin_errors: HashMap::new(),
            buckets: vec![0; DURATION_BUCKETS.len()],
            count: 0,
            sum: 0.0,
        }
    }
}

/// Process-wide metrics collector. Interior mutability via `Mutex`, like
/// the rest of the gateway state; share it as an `Arc` and call the
/// `record_*` methods from any task.
pub struct Metrics {
    state: Mutex<MetricsState>,
}

impl Metrics {
    /// Create an empty collector.
    pub fn new() -> Metrics {
        Metrics {
            state: Mutex::new(MetricsState::new()),
        }
    }

    /// Count one HTTP request by route and status code.
    pub fn record_request(&self, route: &str, code: u16) {
        let mut s = self.state.lock().unwrap();
        *s.requests.entry((route.to_string(), code)).or_insert(0) += 1;
    }

    /// Record one request duration into the fixed buckets plus `_count`
    /// and `_sum`. Values beyond the last bucket only feed count/sum.
    pub fn record_duration(&self, seconds: f64) {
        let mut s = self.state.lock().unwrap();
        s.count += 1;
        s.sum += seconds;
        if let Some(i) = bucket_index(seconds) {
            for b in s.buckets.iter_mut().skip(i) {
                *b += 1;
            }
        }
    }

    /// Count one upstream attempt by upstream name and result label.
    pub fn record_attempt(&self, upstream: &str, result: &str) {
        let mut s = self.state.lock().unwrap();
        *s.attempts
            .entry((upstream.to_string(), result.to_string()))
            .or_insert(0) += 1;
    }

    /// Count one plugin error by plugin name and kind label.
    ///
    /// Production plugin errors are recorded in the wasm crate's
    /// `HostState` (this crate has no hook per plugin error) and merged
    /// into the exposition at scrape time by [`render`]. This method is
    /// kept for API completeness and exercised by the render tests.
    #[allow(dead_code)]
    pub fn record_plugin_error(&self, plugin: &str, kind: &str) {
        let mut s = self.state.lock().unwrap();
        *s.plugin_errors
            .entry((plugin.to_string(), kind.to_string()))
            .or_insert(0) += 1;
    }

    /// Cheap clone of all state for rendering. The caller can keep the
    /// snapshot while the collector keeps recording.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let s = self.state.lock().unwrap();
        MetricsSnapshot {
            requests: s.requests.clone(),
            attempts: s.attempts.clone(),
            plugin_errors: s.plugin_errors.clone(),
            buckets: s.buckets.clone(),
            count: s.count,
            sum: s.sum,
        }
    }
}

impl Default for Metrics {
    fn default() -> Metrics {
        Metrics::new()
    }
}

/// Immutable copy of all collector state, cheap to clone and safe to
/// render from any thread.
#[derive(Clone)]
pub struct MetricsSnapshot {
    /// `openrusty_requests_total{route,code}` -> count.
    pub requests: HashMap<(String, u16), u64>,
    /// `openrusty_upstream_attempts_total{upstream,result}` -> count.
    pub attempts: HashMap<(String, String), u64>,
    /// `openrusty_plugin_errors_total{plugin,kind}` -> count.
    pub plugin_errors: HashMap<(String, String), u64>,
    /// Cumulative per-bucket counts, aligned with [`DURATION_BUCKETS`].
    pub buckets: Vec<u64>,
    /// `openrusty_request_duration_seconds_count`.
    pub count: u64,
    /// `openrusty_request_duration_seconds_sum`.
    pub sum: f64,
}

/// Index of the smallest bucket boundary `>= seconds`, or `None` when the
/// value exceeds every boundary (then it only feeds `_count`/`_sum`).
fn bucket_index(seconds: f64) -> Option<usize> {
    DURATION_BUCKETS.iter().position(|b| seconds <= *b)
}

pub use metrics_render::render;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_edges_assign_to_expected_buckets() {
        assert_eq!(bucket_index(0.005), Some(0));
        assert_eq!(bucket_index(0.0050001), Some(1));
        assert_eq!(bucket_index(0.01), Some(1));
        assert_eq!(bucket_index(120.0), Some(DURATION_BUCKETS.len() - 1));
        assert_eq!(bucket_index(120.0001), None);
        assert_eq!(bucket_index(200.0), None);
        // Values at or below the first boundary land in bucket 0.
        assert_eq!(bucket_index(0.0), Some(0));
    }

    #[test]
    fn duration_records_feed_cumulative_buckets_and_count_sum() {
        let m = Metrics::new();
        m.record_duration(0.005); // bucket 0
        m.record_duration(0.0050001); // bucket 1
        m.record_duration(120.0); // last bucket
        m.record_duration(200.0); // beyond every bucket: count/sum only
        let s = m.snapshot();
        assert_eq!(s.count, 4);
        assert_eq!(s.buckets.len(), DURATION_BUCKETS.len());
        assert_eq!(s.buckets[0], 1); // <= 0.005
        assert_eq!(s.buckets[1], 2); // <= 0.01
        assert_eq!(s.buckets[2], 2); // <= 0.025: unchanged from bucket 1
        assert_eq!(*s.buckets.last().unwrap(), 3); // <= 120.0
        assert!((s.sum - 320.0100001).abs() < 1e-9);
    }

    #[test]
    fn counters_accumulate() {
        let m = Metrics::new();
        m.record_request("/v1", 200);
        m.record_request("/v1", 200);
        m.record_request("/v1", 500);
        m.record_attempt("vllm", RESULT_SUCCESS);
        m.record_attempt("vllm", RESULT_SUCCESS);
        m.record_attempt("vllm", RESULT_TIMEOUT);
        m.record_plugin_error("sched", KIND_TIMEOUT);
        m.record_plugin_error("sched", KIND_TIMEOUT);
        m.record_plugin_error("sched", KIND_BAD_CODE);
        let s = m.snapshot();
        assert_eq!(s.requests[&("/v1".to_string(), 200u16)], 2);
        assert_eq!(s.requests[&("/v1".to_string(), 500u16)], 1);
        assert_eq!(
            s.attempts[&("vllm".to_string(), RESULT_SUCCESS.to_string())],
            2
        );
        assert_eq!(
            s.attempts[&("vllm".to_string(), RESULT_TIMEOUT.to_string())],
            1
        );
        assert_eq!(
            s.plugin_errors[&("sched".to_string(), KIND_TIMEOUT.to_string())],
            2
        );
        assert_eq!(
            s.plugin_errors[&("sched".to_string(), KIND_BAD_CODE.to_string())],
            1
        );
    }

    #[test]
    fn snapshot_is_isolated_from_later_records() {
        let m = Metrics::new();
        m.record_request("/v1", 200);
        m.record_duration(0.5);
        let s = m.snapshot();
        m.record_request("/v1", 200);
        m.record_duration(0.5);
        m.record_attempt("vllm", RESULT_SUCCESS);
        assert_eq!(s.requests[&("/v1".to_string(), 200u16)], 1);
        assert_eq!(s.count, 1);
        assert_eq!(s.attempts.len(), 0);
        assert_eq!(s.sum, 0.5);
    }
}
