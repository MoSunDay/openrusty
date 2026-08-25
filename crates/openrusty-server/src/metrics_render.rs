//! Pure text renderer for [`MetricsSnapshot`]: turns a snapshot plus the
//! live peer/KV views into Prometheus text exposition format 0.0.4.
//!
//! Rendering is deterministic: families appear in a fixed order, label
//! combinations within a family are sorted, bucket lines use the fixed
//! [`DURATION_BUCKETS`] order. No I/O, no locks — the caller samples the
//! snapshot and the live gauges first.

use super::{MetricsSnapshot, DURATION_BUCKETS};
use std::collections::HashMap;

/// Escape a label value per the exposition format: backslash, double-quote
/// and newline become their backslash-escaped forms.
fn escape_label(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

/// Format a `_sum` sample with up to six decimals, trailing zeros trimmed
/// ("0.005000" -> "0.005", "120.000000" -> "120").
fn format_sum(v: f64) -> String {
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// Render the full exposition text for one snapshot.
///
/// Families appear in a fixed order: request counters, duration histogram,
/// upstream attempts, plugin errors, peer health gauge, KV gauge. Label
/// combinations within a family are sorted; buckets use the fixed
/// [`DURATION_BUCKETS`] order with `+Inf` implied by `_count`.
///
/// `peers` is the live peer view `(upstream, addr, healthy)` and `kv` the
/// live plugin KV view `(plugin, entry_count)`; both are gauges sampled at
/// render time rather than accumulated state. `plugin_errors` is the live
/// `(plugin, kind, count)` view from the plugin registry (the only real
/// source of plugin error counts — the wasm crate cannot call back into
/// the server), merged with the snapshot's plugin-errors counter map (for
/// API completeness the counts are added).
pub fn render(
    snap: &MetricsSnapshot,
    peers: &[(String, String, bool)],
    kv: &[(String, u64)],
    plugin_errors: &[(String, String, u64)],
) -> String {
    let mut out = String::new();

    // openrusty_requests_total
    out.push_str(
        "# HELP openrusty_requests_total Total HTTP requests handled by route and status code.\n",
    );
    out.push_str("# TYPE openrusty_requests_total counter\n");
    let mut reqs: Vec<_> = snap.requests.iter().collect();
    reqs.sort_by(|a, b| a.0.cmp(b.0));
    for ((route, code), n) in reqs {
        out.push_str(&format!(
            "openrusty_requests_total{{route=\"{}\",code=\"{}\"}} {n}\n",
            escape_label(route),
            code
        ));
    }

    // openrusty_request_duration_seconds (histogram)
    out.push_str(
        "# HELP openrusty_request_duration_seconds Request handling duration in seconds.\n",
    );
    out.push_str("# TYPE openrusty_request_duration_seconds histogram\n");
    for (b, n) in DURATION_BUCKETS.iter().zip(snap.buckets.iter()) {
        out.push_str(&format!(
            "openrusty_request_duration_seconds_bucket{{le=\"{b}\"}} {n}\n"
        ));
    }
    out.push_str(&format!(
        "openrusty_request_duration_seconds_bucket{{le=\"+Inf\"}} {}\n",
        snap.count
    ));
    out.push_str(&format!(
        "openrusty_request_duration_seconds_sum {}\n",
        format_sum(snap.sum)
    ));
    out.push_str(&format!(
        "openrusty_request_duration_seconds_count {}\n",
        snap.count
    ));

    // openrusty_upstream_attempts_total
    out.push_str(
        "# HELP openrusty_upstream_attempts_total Total upstream attempts by upstream and result.\n",
    );
    out.push_str("# TYPE openrusty_upstream_attempts_total counter\n");
    let mut attempts: Vec<_> = snap.attempts.iter().collect();
    attempts.sort_by(|a, b| a.0.cmp(b.0));
    for ((upstream, result), n) in attempts {
        out.push_str(&format!(
            "openrusty_upstream_attempts_total{{upstream=\"{}\",result=\"{}\"}} {n}\n",
            escape_label(upstream),
            escape_label(result)
        ));
    }

    // openrusty_plugin_errors_total
    out.push_str(
        "# HELP openrusty_plugin_errors_total Total plugin execution errors by plugin and kind.\n",
    );
    out.push_str("# TYPE openrusty_plugin_errors_total counter\n");
    let mut merged: HashMap<(String, String), u64> = snap.plugin_errors.clone();
    for (plugin, kind, n) in plugin_errors {
        *merged.entry((plugin.clone(), kind.clone())).or_insert(0) += n;
    }
    let mut errors: Vec<_> = merged.iter().collect();
    errors.sort_by(|a, b| a.0.cmp(b.0));
    for ((plugin, kind), n) in errors {
        out.push_str(&format!(
            "openrusty_plugin_errors_total{{plugin=\"{}\",kind=\"{}\"}} {n}\n",
            escape_label(plugin),
            escape_label(kind)
        ));
    }

    // openrusty_peer_healthy (gauge, sampled at render time)
    out.push_str("# HELP openrusty_peer_healthy Whether an upstream peer is currently healthy.\n");
    out.push_str("# TYPE openrusty_peer_healthy gauge\n");
    let mut peers_sorted = peers.to_vec();
    peers_sorted.sort();
    for (upstream, addr, healthy) in peers_sorted {
        out.push_str(&format!(
            "openrusty_peer_healthy{{upstream=\"{}\",addr=\"{}\"}} {}\n",
            escape_label(&upstream),
            escape_label(&addr),
            if healthy { 1 } else { 0 }
        ));
    }

    // openrusty_kv_entries (gauge, sampled at render time)
    out.push_str("# HELP openrusty_kv_entries Live KV entries held by each plugin.\n");
    out.push_str("# TYPE openrusty_kv_entries gauge\n");
    let mut kv_sorted = kv.to_vec();
    kv_sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (plugin, n) in kv_sorted {
        out.push_str(&format!(
            "openrusty_kv_entries{{plugin=\"{}\"}} {n}\n",
            escape_label(&plugin)
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    // `super::super` is the `metrics` module both standalone (crate root)
    // and once wired as `mod metrics;` from main.rs.
    use super::super::{Metrics, KIND_TRAP, RESULT_SUCCESS, RESULT_TIMEOUT};

    #[test]
    fn render_emits_help_type_and_escaped_labels() {
        let m = Metrics::new();
        // route contains a quote, a backslash and a newline
        m.record_request("weird\"route\\x\n", 200);
        m.record_request("/v1", 200);
        m.record_duration(0.5);
        m.record_attempt("vllm", RESULT_TIMEOUT);
        m.record_plugin_error("sched", KIND_TRAP);
        let snap = m.snapshot();
        let peers = vec![
            ("vllm".to_string(), "127.0.0.1:8000".to_string(), true),
            ("vllm".to_string(), "127.0.0.1:8001".to_string(), false),
        ];
        let kv = vec![("sched".to_string(), 3u64)];
        // Live plugin-error view (from the registry at scrape time).
        let live_errors = vec![("sched".to_string(), KIND_TRAP.to_string(), 2u64)];
        let out = render(&snap, &peers, &kv, &live_errors);

        // HELP/TYPE lines for every family.
        for help in [
            "# HELP openrusty_requests_total ",
            "# TYPE openrusty_requests_total counter",
            "# HELP openrusty_request_duration_seconds ",
            "# TYPE openrusty_request_duration_seconds histogram",
            "# HELP openrusty_upstream_attempts_total ",
            "# TYPE openrusty_upstream_attempts_total counter",
            "# HELP openrusty_plugin_errors_total ",
            "# TYPE openrusty_plugin_errors_total counter",
            "# HELP openrusty_peer_healthy ",
            "# TYPE openrusty_peer_healthy gauge",
            "# HELP openrusty_kv_entries ",
            "# TYPE openrusty_kv_entries gauge",
        ] {
            assert!(out.contains(help), "missing {help:?} in:\n{out}");
        }

        // Label values escaped: " -> \", \ -> \\, newline -> \n.
        assert!(out
            .contains("openrusty_requests_total{route=\"weird\\\"route\\\\x\\n\",code=\"200\"} 1"));
        assert!(out.contains("openrusty_requests_total{route=\"/v1\",code=\"200\"} 1"));
        assert!(out
            .contains("openrusty_upstream_attempts_total{upstream=\"vllm\",result=\"timeout\"} 1"));
        // Snapshot counter (1) + live registry view (2) merge into 3.
        assert!(out
            .contains("openrusty_plugin_errors_total{plugin=\"sched\",kind=\"trap\"} 3"));
        assert!(out.contains("openrusty_peer_healthy{upstream=\"vllm\",addr=\"127.0.0.1:8000\"} 1"));
        assert!(out.contains("openrusty_peer_healthy{upstream=\"vllm\",addr=\"127.0.0.1:8001\"} 0"));
        assert!(out.contains("openrusty_kv_entries{plugin=\"sched\"} 3"));

        // Histogram: fixed bucket order plus +Inf/sum/count; sum trimmed.
        assert!(out.contains("openrusty_request_duration_seconds_bucket{le=\"0.005\"} 0"));
        // the 0.5s observation accumulates into every bucket >= 0.5
        assert!(out.contains("openrusty_request_duration_seconds_bucket{le=\"120\"} 1"));
        assert!(out.contains("openrusty_request_duration_seconds_bucket{le=\"+Inf\"} 1"));
        assert!(out.contains("openrusty_request_duration_seconds_sum 0.5"));
        assert!(out.contains("openrusty_request_duration_seconds_count 1"));
    }

    #[test]
    fn render_is_deterministic_and_sorted() {
        let m = Metrics::new();
        m.record_request("/z", 200);
        m.record_request("/a", 500);
        m.record_request("/a", 200);
        m.record_duration(0.005);
        m.record_attempt("z-up", RESULT_SUCCESS);
        m.record_attempt("a-up", RESULT_SUCCESS);
        let snap = m.snapshot();
        let peers = vec![
            ("z-up".to_string(), "b".to_string(), true),
            ("a-up".to_string(), "a".to_string(), true),
        ];
        let out = render(&snap, &peers, &[], &[]);

        // Label combos sorted: route /a before /z, attempts a-up before z-up.
        let a_line = out.find("openrusty_requests_total{route=\"/a\"").unwrap();
        let z_line = out.find("openrusty_requests_total{route=\"/z\"").unwrap();
        assert!(a_line < z_line);
        let a_att = out
            .find("openrusty_upstream_attempts_total{upstream=\"a-up\"")
            .unwrap();
        let z_att = out
            .find("openrusty_upstream_attempts_total{upstream=\"z-up\"")
            .unwrap();
        assert!(a_att < z_att);

        // Bucket lines in fixed order, +Inf after every fixed bucket.
        let b0 = out.find("bucket{le=\"0.005\"}").unwrap();
        let b1 = out.find("bucket{le=\"0.01\"}").unwrap();
        let inf = out.find("bucket{le=\"+Inf\"}").unwrap();
        assert!(b0 < b1 && b1 < inf);

        // Peer lines sorted by (upstream, addr).
        let p_a = out.find("addr=\"a\"").unwrap();
        let p_b = out.find("addr=\"b\"").unwrap();
        assert!(p_a < p_b);

        // Same input renders byte-identically.
        assert_eq!(out, render(&snap, &peers, &[], &[]));
        assert!(out.contains("openrusty_request_duration_seconds_sum 0.005"));
    }

    #[test]
    fn live_plugin_errors_merge_with_snapshot_counter() {
        let m = Metrics::new();
        m.record_plugin_error("sched", KIND_TRAP); // snapshot side (API completeness)
        let snap = m.snapshot();
        let live = vec![
            ("sched".to_string(), KIND_TRAP.to_string(), 2u64),
            ("sched".to_string(), "timeout".to_string(), 3u64),
        ];
        let out = render(&snap, &[], &[], &live);
        // Same key: snapshot 1 + live 2 = 3; live-only key passes through.
        assert!(out
            .contains("openrusty_plugin_errors_total{plugin=\"sched\",kind=\"trap\"} 3"));
        assert!(out
            .contains("openrusty_plugin_errors_total{plugin=\"sched\",kind=\"timeout\"} 3"));
    }
}
