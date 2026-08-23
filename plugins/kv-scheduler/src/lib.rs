//! kv-scheduler: vLLM-style KV-cache affinity balancer plugin.
//!
//! Balancer phase: requests carrying the same task key are pinned to the
//! same upstream peer while the affinity record lives (TTL renewal on every
//! hit); otherwise the least-loaded peer is picked and the assignment is
//! recorded. Log phase: one informational line. Slot release happens via
//! TTL expiry, so no bookkeeping is needed there.
//!
//! KV layout (values are ASCII decimal strings):
//! - `aff:<task>`   -> peer index, TTL = affinity_ttl_s
//! - `sched:<idx>`  -> last-scheduled timestamp (ms), TTL = affinity_ttl_s
//!
//! Settings (`[plugins.settings.kv-scheduler]`):
//! - `extract`            : `query:<param>` or `path:<n>` (required)
//! - `affinity_ttl_s`     : default 300
//! - `max_tasks_per_node` : default 0 (unlimited)
#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use openrusty_sdk::sched::{choose_least_loaded, NodeStat};
use openrusty_sdk::{host, url, Decision};

mod choice;

use choice::{aff_key, ascii_u64, parse_ascii_u64, sched_key, ExtractRule};

// Guest allocator exports (`orr_alloc`/`orr_dealloc`) -- wasm only, so
// host-side unit tests of this crate keep the system allocator.
#[cfg(target_arch = "wasm32")]
openrusty_sdk::export_allocators!();

const AFF_PREFIX: &str = "aff:";
const DEFAULT_TTL_S: u64 = 300;

/// Balancer phase: choose the upstream peer for this request.
#[openrusty_sdk::phase(balancer)]
fn on_balancer() -> Decision {
    let Some(task) = task_key() else {
        return Decision::Declined;
    };
    let peers = host::peers();
    if let Some(idx) = affinity_peer(&task, &peers) {
        // Renew the affinity record and stick to the previous peer.
        host::kv_set(&aff_key(&task), ascii_u64(idx as u64).as_bytes(), ttl_ms());
        if host::set_peer(idx as u32) {
            return Decision::Ok;
        }
    }
    gather_and_pick(&task, &peers)
}

/// Log phase: informational trace; nothing to bookkeep (TTL releases).
#[openrusty_sdk::phase(log)]
fn on_log() -> Decision {
    match task_key() {
        Some(task) => {
            let upstream = host::req_meta_str("upstream").unwrap_or_default();
            host::log(
                host::LogLevel::Info,
                &alloc::format!("kv-scheduler done task={task} upstream={upstream}"),
            );
        }
        None => host::log(host::LogLevel::Info, "kv-scheduler done"),
    }
    Decision::Ok
}

openrusty_sdk::dispatch! {
    balancer => on_balancer,
    log => on_log,
}

/// Extract the task key from the request per the `extract` setting.
fn task_key() -> Option<String> {
    let rule = choice::parse_extract(&host::cfg("extract")?)?;
    match rule {
        ExtractRule::QueryParam(param) => {
            let query = host::req_meta_str("query")?;
            url::query_param(&query, &param)
        }
        ExtractRule::PathSegment(n) => {
            let path = host::req_meta_str("path")?;
            url::path_segment(&path, n)
        }
        ExtractRule::BodyField(field) => {
            let body = host::req_body()?;
            let raw = choice::json_string_field(&body, &field)?;
            let s = String::from_utf8_lossy(&raw).into_owned();
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            Some(String::from(s))
        }
    }
}

/// Resolve a live affinity record to a healthy peer index, if any.
fn affinity_peer(task: &str, peers: &[host::PeerInfo]) -> Option<usize> {
    let raw = host::kv_get(&aff_key(task))?;
    let idx = parse_ascii_u64(&raw)? as usize;
    let peer = peers.get(idx)?;
    if peer.healthy { Some(idx) } else { None }
}

/// Count live assignments per peer, then pick the least-loaded one.
fn gather_and_pick(task: &str, peers: &[host::PeerInfo]) -> Decision {
    if peers.is_empty() {
        return Decision::Declined;
    }
    let ttl_s = cfg_num("affinity_ttl_s", DEFAULT_TTL_S);
    let cap = cfg_num("max_tasks_per_node", 0) as u32;
    let ttl = ttl_s.saturating_mul(1000) as i64;

    // Live affinity records per peer (values are ASCII peer indexes).
    let mut active = alloc::vec![0u32; peers.len()];
    for (_key, val) in host::kv_scan(AFF_PREFIX) {
        if let Some(idx) = parse_ascii_u64(&val) {
            let idx = idx as usize;
            if idx < active.len() {
                active[idx] = active[idx].saturating_add(1);
            }
        }
    }

    let nodes: Vec<NodeStat> = (0..peers.len())
        .map(|i| NodeStat {
            healthy: peers[i].healthy,
            active: active[i],
            last_sched_ms: sched_ts(i),
            cap,
        })
        .collect();

    let Some(pick) = choose_least_loaded(&nodes) else {
        return Decision::Declined;
    };

    host::kv_set(&aff_key(task), ascii_u64(pick as u64).as_bytes(), ttl);
    host::kv_set(&sched_key(pick), ascii_u64(host::now_ms()).as_bytes(), ttl);
    if host::set_peer(pick as u32) {
        Decision::Ok
    } else {
        Decision::Declined
    }
}

/// Last-scheduled timestamp for peer `idx`; 0 when never/missing.
fn sched_ts(idx: usize) -> u64 {
    host::kv_get(&sched_key(idx))
        .and_then(|v| parse_ascii_u64(&v))
        .unwrap_or(0)
}

/// Numeric plugin setting with fallback default.
fn cfg_num(key: &str, default: u64) -> u64 {
    host::cfg(key)
        .and_then(|s| parse_ascii_u64(s.trim().as_bytes()))
        .unwrap_or(default)
}

/// Affinity TTL in milliseconds.
fn ttl_ms() -> i64 {
    (cfg_num("affinity_ttl_s", DEFAULT_TTL_S).saturating_mul(1000)) as i64
}
