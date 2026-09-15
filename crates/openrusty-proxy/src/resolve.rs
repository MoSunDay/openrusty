//! DNS endpoint resolution for service-discovered upstreams.
//!
//! Kubernetes-rendered upstreams carry `endpoints` (`host:port` strings,
//! e.g. `svc.ns.svc.cluster.local:80`) instead of static `peers`; the
//! config layer deliberately keeps them unresolved so a parse never
//! blocks on DNS. This module folds them into concrete `peers` at APPLY
//! time (reload or ingress render, both off the request path): each
//! endpoint is looked up, and the addresses land in the upstream's peer
//! list as weight-1 candidates - kube-proxy does the actual load
//! balancing behind a ClusterIP VIP, so one address per endpoint is the
//! common case and duplicates across endpoints are dropped.
//!
//! An endpoint that fails to resolve (service not created yet, DNS
//! blip) leaves that upstream with zero peers for THIS apply: it serves
//! nothing until the next apply (any ingress/secret watch event or
//! reload), which re-resolves. The failure is logged, never fatal -
//! one bad upstream must not sink the whole render.

use std::collections::HashSet;
use std::net::SocketAddr;

use openrusty_core::config::{Config, PeerConfig, UpstreamConfig};

/// Look one `host:port` endpoint up (async getaddrinfo), preserving the
/// resolver's order but dropping addresses another endpoint already
/// contributed. Failures resolve to an empty Vec - callers decide how to
/// surface them.
pub async fn resolve_endpoint(endpoint: &str, seen: &mut HashSet<SocketAddr>) -> Vec<SocketAddr> {
    match tokio::net::lookup_host(endpoint).await {
        Ok(addrs) => addrs.filter(|a| seen.insert(*a)).collect(),
        Err(e) => {
            tracing::warn!(endpoint, error = %e, "upstream endpoint DNS lookup failed");
            Vec::new()
        }
    }
}

/// Fold resolved addresses into a config upstream as weight-1 peers and
/// clear the (now consumed) endpoint list (pure).
pub fn merge_resolved(cfg: &UpstreamConfig, resolved: &[SocketAddr]) -> UpstreamConfig {
    let mut merged = cfg.clone();
    for addr in resolved {
        merged.peers.push(PeerConfig {
            addr: *addr,
            weight: 1,
        });
    }
    merged.endpoints.clear();
    merged
}

/// Resolve every upstream's `endpoints` in place, returning the config
/// handed to `apply_runtime`. Static peers pass through untouched; a
/// failed endpoint leaves its upstream peer-less for this apply (see the
/// module docs) instead of failing the whole render.
pub async fn resolve_config(cfg: &Config) -> Config {
    let mut out = cfg.clone();
    for up in &mut out.upstreams {
        if up.endpoints.is_empty() {
            continue;
        }
        let mut seen: HashSet<SocketAddr> =
            up.peers.iter().map(|p| p.addr).collect();
        let mut resolved = Vec::new();
        for endpoint in &up.endpoints {
            resolved.extend(resolve_endpoint(endpoint, &mut seen).await);
        }
        if resolved.is_empty() {
            tracing::warn!(
                upstream = %up.name,
                endpoints = ?up.endpoints,
                "no endpoint resolved; upstream serves nothing until the next apply"
            );
            up.endpoints.clear();
            continue;
        }
        *up = merge_resolved(up, &resolved);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(endpoints: &[&str], peers: &[&str]) -> UpstreamConfig {
        UpstreamConfig {
            name: "u".to_string(),
            endpoints: endpoints.iter().map(|e| e.to_string()).collect(),
            peers: peers
                .iter()
                .map(|p| PeerConfig {
                    addr: p.parse().unwrap(),
                    weight: 1,
                })
                .collect(),
            balancer: Default::default(),
            retries: 0,
            retry_on_timeout: false,
            connect_timeout_ms: 2000,
            pool_idle_timeout_ms: 60_000,
            tls: None,
            health: Default::default(),
        }
    }

    #[test]
    fn merge_appends_weight_one_peers_and_clears_endpoints() {
        let cfg = upstream(&["a.example:80"], &["127.0.0.1:9001"]);
        let addr: SocketAddr = "10.43.0.10:80".parse().unwrap();
        let merged = merge_resolved(&cfg, &[addr]);
        assert_eq!(merged.endpoints, Vec::<String>::new());
        assert_eq!(merged.peers.len(), 2);
        assert_eq!(merged.peers[1].addr, addr);
        assert_eq!(merged.peers[1].weight, 1);
        // The original config is untouched (pure).
        assert_eq!(cfg.endpoints, vec!["a.example:80".to_string()]);
    }

    #[tokio::test]
    async fn localhost_resolves_to_a_loopback_address() {
        let mut seen = HashSet::new();
        let addrs = resolve_endpoint("localhost:1", &mut seen).await;
        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|a| a.ip().is_loopback()));
        // Second lookup of the same address set yields nothing new.
        let again = resolve_endpoint("localhost:1", &mut seen).await;
        assert!(again.is_empty());
    }
}
