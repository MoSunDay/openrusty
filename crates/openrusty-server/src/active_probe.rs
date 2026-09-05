//! Active (proactive) health checking, modeled on lua-resty-upstream-healthcheck.
//!
//! A background task probes every peer of every upstream that has
//! `[upstreams.health.active]` configured. Each probe is a GET to the
//! configured `path` sent through the shared pooled clients; a 2xx response
//! counts as success, anything else (connect/send error, timeout, non-2xx)
//! counts as failure. Verdicts are fed to [`proxy::record_probe`], which
//! maintains the consecutive-counter state (unhealthy after
//! `unhealthy_threshold` consecutive failures, healthy again after
//! `healthy_threshold` consecutive successes) and mirrors the result into
//! the per-index passive health slots, so request routing picks it up
//! without any signature changes.
//!
//! Reload safety: the task re-reads the runtime snapshot every tick, so
//! upstreams added/removed by a reload are picked up automatically, and
//! [`spawn`] cancels and replaces the task whenever the config changes
//! (probing at the smallest configured `interval_ms`).

use crate::state::{AppState, UpstreamRt};
use openrusty_core::config::ActiveHealthConfig;
use openrusty_proxy as proxy;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Cancel any existing probe task and spawn a fresh one when at least one
/// upstream has active health checking enabled.
///
/// The new task probes at the smallest configured `interval_ms` across all
/// enabled upstreams. When no upstream enables active health the previous
/// task (if any) is still cancelled and nothing is spawned.
///
/// The task owns an `Arc` clone of the state; the handle stored in
/// `state.probe_task` keeps the task alive until the next reload aborts and
/// replaces it (which also releases the task's clone).
pub fn spawn(state: &Arc<AppState>) {
    // A reload may change the interval or drop the last enabled upstream:
    // the old task is always cancelled, the new one reflects the snapshot.
    if let Some(handle) = state.probe_task.lock().unwrap_or_else(|e| e.into_inner()).take() {
        handle.abort();
    }
    let rt = state.runtime.load();
    let mut interval_ms: Option<u64> = None;
    for up in rt.upstreams.values() {
        if let Some(active) = up.up.health.active.as_ref() {
            interval_ms = Some(match interval_ms {
                Some(min) => min.min(active.interval_ms),
                None => active.interval_ms,
            });
        }
    }
    let Some(interval_ms) = interval_ms else {
        return;
    };
    let st = Arc::clone(state);
    let handle = tokio::spawn(async move { run(st, Duration::from_millis(interval_ms)).await });
    *state.probe_task.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
}

/// Probe loop: sleep, sweep every enabled peer, log verdict transitions.
/// Runs until aborted by [`spawn`] (reload) or process shutdown.
async fn run(state: Arc<AppState>, interval: Duration) {
    // Previous verdict per (upstream, addr); only real changes are logged.
    let mut prev: HashMap<(String, String), bool> = HashMap::new();
    loop {
        tokio::time::sleep(interval).await;
        probe_tick(&state, &mut prev).await;
    }
}

/// One sweep across every enabled upstream. All peers are probed
/// concurrently ([`futures::future::join_all`]): a slow or hanging peer
/// must not delay the verdicts (or the tick cadence) of the others, which
/// is exactly what happened when the sweep was a plain sequential loop.
///
/// Verdicts are fed through [`transition`] against `prev`, so only real
/// flips produce a log line.
async fn probe_tick(state: &AppState, prev: &mut HashMap<(String, String), bool>) {
    let rt = state.runtime.load();
    let mut jobs: Vec<(&UpstreamRt, usize, &proxy::Peer, ActiveHealthConfig)> = Vec::new();
    for up in rt.upstreams.values() {
        let Some(active) = up.up.health.active.clone() else {
            continue;
        };
        for (idx, peer) in up.up.peers.iter().enumerate() {
            jobs.push((up, idx, peer, active.clone()));
        }
    }
    let results = futures::future::join_all(jobs.into_iter().map(
        |(up, idx, peer, active)| async move {
            let healthy = probe_peer(state, &up.up, idx, peer, &active).await;
            (up.up.name.clone(), peer.addr.to_string(), healthy)
        },
    ))
    .await;
    for (name, peer, healthy) in results {
        let key = (name, peer);
        match transition(prev.get(&key), healthy) {
            Some(true) => tracing::info!(
                upstream = %key.0,
                peer = %key.1,
                "active health check: peer healthy again"
            ),
            Some(false) => tracing::warn!(
                upstream = %key.0,
                peer = %key.1,
                "active health check: peer marked unhealthy"
            ),
            None => {}
        }
        prev.insert(key, healthy);
    }
}

/// Probe one peer: GET `active.path` through the pooled client, bounded by
/// `active.timeout_ms`. A 2xx response is a success; errors, timeouts and
/// non-2xx responses are failures. HTTPS upstreams are probed through
/// their TLS plan (SNI and verification name from the upstream section).
/// The verdict is recorded via [`proxy::record_probe`] and its new healthy
/// state is returned.
///
/// Connection errors during a probe are ordinary failures: they land in
/// the record_failure path inside `record_probe` (ok = false).
async fn probe_peer(
    state: &AppState,
    up: &proxy::Upstream,
    idx: usize,
    peer: &proxy::Peer,
    active: &ActiveHealthConfig,
) -> bool {
    let req = proxy::ForwardRequest {
        method: hyper::Method::GET,
        path_and_query: active.path.clone(),
        headers: Vec::new(),
        body: Default::default(),
        client_ip: "127.0.0.1".to_string(),
    };
    // The pool picks the plaintext or TLS client per the upstream's plan
    // (see `proxy::forward_peer`).
    let outcome = tokio::time::timeout(
        Duration::from_millis(active.timeout_ms),
        proxy::forward_peer(&state.pool, up, peer, &req),
    )
    .await;
    // Every failure is worth a line: threshold logic stays quiet about the
    // reason, and "probe timed out" vs "connect refused" vs "non-2xx" point
    // at very different problems.
    if !matches!(outcome, Ok(Ok(ref resp)) if resp.status().is_success()) {
        let reason = match &outcome {
            Err(_) => "timeout".to_string(),
            Ok(Err(e)) => e.to_string(),
            Ok(Ok(resp)) => format!("status {}", resp.status().as_u16()),
        };
        tracing::warn!(
            upstream = %up.name,
            peer = %peer.addr,
            reason = %reason,
            "active health check: probe failed"
        );
    }
    let ok = matches!(outcome, Ok(Ok(resp)) if resp.status().is_success());
    proxy::record_probe(
        &state.health,
        &up.name,
        idx,
        &peer.addr.to_string(),
        ok,
        active.unhealthy_threshold,
        active.healthy_threshold,
    )
}

/// When a verdict change deserves a log line: `Some(new)` when the peer was
/// observed before and its verdict changed, `None` otherwise (first
/// observation included, so boot does not spam).
fn transition(prev: Option<&bool>, new: bool) -> Option<bool> {
    match prev {
        Some(p) if *p != new => Some(new),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{boot_state, TmpDir};
    use std::time::Instant;
    use tokio::io::AsyncWriteExt;

    /// Upstream section of the test config for one slow-but-healthy peer.
    fn upstream_block(name: &str, addr: &std::net::SocketAddr) -> String {
        format!(
            r#"
[[upstreams]]
name = "{name}"
  [[upstreams.peers]]
  addr = "{addr}"
  [upstreams.health.active]
  interval_ms = 60000
  timeout_ms = 5000
  path = "/health"
"#
        )
    }

    /// A peer that answers every connection with a 200 after `delay`: slow
    /// enough that sequential probing of two of these would be visible.
    async fn spawn_slow_peer(delay: Duration) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    // Not reading the request head: the response is legal
                    // regardless, and we only care about the timing.
                    tokio::time::sleep(delay).await;
                    let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = sock.write_all(resp).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        addr
    }

    #[test]
    fn transition_logs_only_real_changes() {
        // First observation is never logged.
        assert_eq!(transition(None, true), None);
        assert_eq!(transition(None, false), None);
        // Unchanged verdicts are not logged.
        assert_eq!(transition(Some(&true), true), None);
        assert_eq!(transition(Some(&false), false), None);
        // Only actual flips produce a log line.
        assert_eq!(transition(Some(&true), false), Some(false));
        assert_eq!(transition(Some(&false), true), Some(true));
    }

    #[tokio::test]
    async fn probe_tick_probes_peers_concurrently() {
        // Two slow peers in two upstreams. Sequential sweeping needs
        // >= 2 * 300ms = 600ms; a concurrent sweep must land far below
        // that (probes themselves take ~300ms).
        let slow = Duration::from_millis(300);
        let a = spawn_slow_peer(slow).await;
        let b = spawn_slow_peer(slow).await;

        let dir = TmpDir::new("probe-parallel");
        let config = format!(
            "[server]\nlisten = \"127.0.0.1:18080\"\n\n[plugins]\ndir = \"{}\"\n{}{}",
            dir.plugins_dir().display(),
            upstream_block("a", &a),
            upstream_block("b", &b),
        );
        dir.write_config(&config);
        let state = boot_state(&dir);

        let mut prev: HashMap<(String, String), bool> = HashMap::new();
        let started = Instant::now();
        probe_tick(&state, &mut prev).await;
        let elapsed = started.elapsed();

        assert_eq!(prev.len(), 2, "both peers must have been probed");
        assert!(
            prev.values().all(|ok| *ok),
            "both slow peers answered 200: {prev:?}"
        );
        assert!(
            elapsed < Duration::from_millis(550),
            "peers were probed sequentially ({elapsed:?}); join_all must overlap them"
        );
    }
}
