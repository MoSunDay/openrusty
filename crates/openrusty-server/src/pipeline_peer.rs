//! Peer selection: balancer-phase outcome, health re-validation of the
//! plugin's choice, and the default balancers restricted to healthy peers.
//! Peers already attempted by this request are never picked again.

use crate::state::{AppState, UpstreamRt};
use openrusty_core::config::BalancerKind;
use openrusty_core::phase::{Decision, Phase};
use openrusty_proxy as proxy;
use openrusty_wasm::host_state::now_ms;
use openrusty_wasm::RequestSession;
use std::net::SocketAddr;

/// Peer selection outcome shared by the normal proxy path and WebSocket.
#[derive(Debug)]
pub(crate) enum Pick {
    Peer(usize),
    Deny(u16),
    None,
}

/// Restrict the healthy peer indices to the ones this request has not
/// attempted yet. Pure so the retry-exclusion semantics stay unit-testable:
/// a failed attempt must never be followed by a second attempt at the same
/// address, even if the peer list repeats it.
pub(crate) fn untried_healthy(
    peers: &[proxy::Peer],
    healthy: &[usize],
    tried: &[SocketAddr],
) -> Vec<usize> {
    healthy
        .iter()
        .copied()
        .filter(|&i| peers.get(i).is_some_and(|p| !tried.contains(&p.addr)))
        .collect()
}

/// True when the plugin-pinned peer index is currently usable: in range,
/// healthy, and not already attempted by this request.
pub(crate) fn pin_is_valid(
    up_rt: &UpstreamRt,
    idx: usize,
    healthy: bool,
    tried: &[SocketAddr],
) -> bool {
    up_rt.up.peers
        .get(idx)
        .is_some_and(|p| healthy && !tried.contains(&p.addr))
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
    let tried = session.ctx().tried.clone();
    if let Some(i) = session.ctx().peer_index {
        let idx = i as usize;
        // Re-validate against CURRENT health: a peer can die between the
        // plugin's (snapshot-age) view and this attempt. A peer this request
        // already tried is equally invalid: retrying it cannot help.
        if pin_is_valid(up_rt, idx, proxy::is_healthy(&state.health, &up_rt.up.name, idx, now_ms()), &tried)
        {
            return Pick::Peer(idx);
        }
        tracing::warn!(
            index = i,
            "plugin chose an invalid, unhealthy or already-tried peer; using default balancer"
        );
        session.ctx().peer_index = None;
    }
    let healthy = proxy::healthy_indices(
        &state.health,
        &up_rt.up.name,
        up_rt.up.peers.len(),
        now_ms(),
    );
    let candidates = untried_healthy(&up_rt.up.peers, &healthy, &tried);
    if candidates.is_empty() {
        return Pick::None;
    }
    let chosen = match up_rt.up.kind {
        BalancerKind::Swrr => swrr_pick(up_rt, &candidates),
        BalancerKind::IpHash => {
            let ip = session.ctx().client_addr.ip().to_string();
            proxy::ip_hash_pick(&ip, &candidates)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{boot_state, TmpDir};
    use openrusty_core::ReqCtx;
    use openrusty_wasm::PeerView;
    use std::net::SocketAddr;

    fn peer(addr: &str) -> proxy::Peer {
        proxy::Peer {
            addr: addr.parse().unwrap(),
            weight: 1,
        }
    }

    #[test]
    fn untried_filter_drops_attempted_and_keeps_others() {
        let peers = vec![peer("127.0.0.1:9001"), peer("127.0.0.1:9002"), peer("127.0.0.1:9003")];
        let healthy = vec![0, 1, 2];
        assert_eq!(untried_healthy(&peers, &healthy, &[]), vec![0, 1, 2]);
        let tried: Vec<SocketAddr> = vec!["127.0.0.1:9002".parse().unwrap()];
        assert_eq!(untried_healthy(&peers, &healthy, &tried), vec![0, 2]);
        // Every healthy peer tried -> no candidates (the retry loop stops).
        let tried: Vec<SocketAddr> = vec![
            "127.0.0.1:9001".parse().unwrap(),
            "127.0.0.1:9002".parse().unwrap(),
            "127.0.0.1:9003".parse().unwrap(),
        ];
        assert!(untried_healthy(&peers, &healthy, &tried).is_empty());
        // A repeated address in the peer list cannot sneak back in.
        let dup = vec![peer("127.0.0.1:9001"), peer("127.0.0.1:9001")];
        let tried: Vec<SocketAddr> = vec!["127.0.0.1:9001".parse().unwrap()];
        assert!(untried_healthy(&dup, &[0, 1], &tried).is_empty());
    }

    #[test]
    fn pin_rejects_out_of_range_and_tried_peers() {
        let dir = TmpDir::new("pin");
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);
        let rt = state.runtime.load();
        let up_rt = rt.upstreams["u"].clone();
        assert!(!pin_is_valid(&up_rt, 7, true, &[]), "out of range");
        let tried = vec![up_rt.up.peers[0].addr];
        assert!(!pin_is_valid(&up_rt, 0, true, &tried), "already tried");
        assert!(!pin_is_valid(&up_rt, 0, false, &[]), "unhealthy");
        assert!(pin_is_valid(&up_rt, 0, true, &[]));
    }

    #[test]
    fn pick_skips_tried_peers_and_pins_fall_back_to_balancer() {
        let dir = TmpDir::new("pick-tried");
        // Two peers: the pinned/tried one and the fallback target.
        let cfg = dir.standard_config().replace(
            "  [[upstreams.peers]]\n  addr = \"127.0.0.1:9001\"",
            "  [[upstreams.peers]]\n  addr = \"127.0.0.1:9001\"\n\n  [[upstreams.peers]]\n  addr = \"127.0.0.1:9002\"",
        );
        dir.write_config(&cfg);
        let state = boot_state(&dir);
        let rt = state.runtime.load();
        let up_rt = rt.upstreams["u"].clone();
        let snap = state.registry.snapshot();
        let views: Vec<PeerView> = up_rt
            .up
            .peers
            .iter()
            .map(|p| PeerView {
                name: p.addr.to_string(),
                addr: p.addr.to_string(),
                healthy: true,
            })
            .collect();
        let ctx = ReqCtx {
            method: "GET".into(),
            path: "/".into(),
            query: String::new(),
            version: "HTTP/1.1".into(),
            client_addr: "127.0.0.1:40000".parse().unwrap(),
            headers: Vec::new(),
            route_index: None,
            upstream: Some("u".into()),
            peer_index: None,
            attempts: 0,
            tried: vec![up_rt.up.peers[0].addr],
        };
        let mut session = RequestSession::new(&state.registry, snap, ctx, views);

        // A plugin pin on the tried peer must fall back to the balancer.
        session.ctx().peer_index = Some(0);
        match pick_peer(&state, &up_rt, &mut session) {
            Pick::Peer(i) => assert_eq!(i, 1, "must fall through to the untried peer"),
            other => panic!("expected a peer, got {other:?}"),
        }
        // With both peers tried there is nothing left to pick.
        session.ctx().peer_index = None;
        session.ctx().mark_tried(up_rt.up.peers[1].addr);
        match pick_peer(&state, &up_rt, &mut session) {
            Pick::None => {}
            other => panic!("expected no candidate, got {other:?}"),
        }
    }
}
