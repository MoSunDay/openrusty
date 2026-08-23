//! Peer selection: balancer-phase outcome, health re-validation of the
//! plugin's choice, and the default balancers restricted to healthy peers.

use crate::state::{AppState, UpstreamRt};
use openrusty_core::config::BalancerKind;
use openrusty_core::phase::{Decision, Phase};
use openrusty_proxy as proxy;
use openrusty_wasm::host_state::now_ms;
use openrusty_wasm::RequestSession;

/// Peer selection outcome shared by the normal proxy path and WebSocket.
pub(crate) enum Pick {
    Peer(usize),
    Deny(u16),
    None,
}

/// Run the balancer phase, then fall back to the configured default
/// balancer when the plugins did not pick a peer themselves.
pub(crate) fn pick_peer(
    state: &AppState,
    up_rt: &UpstreamRt,
    session: &mut RequestSession,
) -> Pick {
    if let Decision::Deny(s) = session.run_phase(Phase::Balancer) {
        return Pick::Deny(s);
    }
    if let Some(i) = session.ctx().peer_index {
        let idx = i as usize;
        // Re-validate against CURRENT health: a peer can die between the
        // plugin's (snapshot-age) view and this attempt.
        if idx < up_rt.up.peers.len()
            && proxy::is_healthy(&state.health, &up_rt.up.name, idx, now_ms())
        {
            return Pick::Peer(idx);
        }
        tracing::warn!(
            index = i,
            "plugin chose an invalid/unhealthy peer; using default balancer"
        );
        session.ctx().peer_index = None;
    }
    let healthy = proxy::healthy_indices(
        &state.health,
        &up_rt.up.name,
        up_rt.up.peers.len(),
        now_ms(),
    );
    if healthy.is_empty() {
        return Pick::None;
    }
    let chosen = match up_rt.up.kind {
        BalancerKind::Swrr => swrr_pick(up_rt, &healthy),
        BalancerKind::IpHash => {
            let ip = session.ctx().client_addr.ip().to_string();
            proxy::ip_hash_pick(&ip, &healthy)
        }
    };
    match chosen {
        Some(i) => Pick::Peer(i),
        None => Pick::None,
    }
}

/// SWRR restricted to the healthy subset: project the persistent effective
/// weights, step, and write them back. Preserves smoothness across calls.
fn swrr_pick(up_rt: &UpstreamRt, healthy: &[usize]) -> Option<usize> {
    if healthy.is_empty() {
        return None;
    }
    let mut cur = up_rt.swrr.lock().unwrap();
    if cur.len() != up_rt.up.peers.len() {
        cur.resize(up_rt.up.peers.len(), 0);
    }
    let weights: Vec<u32> = healthy.iter().map(|&i| up_rt.up.peers[i].weight).collect();
    let mut subset: Vec<i64> = healthy.iter().map(|&i| cur[i]).collect();
    let k = proxy::swrr_next(&weights, &mut subset);
    for (j, &i) in healthy.iter().enumerate() {
        cur[i] = subset[j];
    }
    Some(healthy[k])
}
