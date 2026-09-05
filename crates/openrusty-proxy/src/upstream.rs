//! Upstream definitions derived from configuration, plus header hygiene.
//!
//! An [`Upstream`] is an immutable snapshot built from an
//! [`openrusty_core::config::UpstreamConfig`]. Snapshots are cheap to rebuild
//! on reload and safe to share across worker tasks.
//!
//! The hop-by-hop helpers are pure functions used by both the request and the
//! response forwarding paths. Per RFC 9110 section 7.6.1 these headers are
//! meaningful only for a single transport-level connection and must not be
//! forwarded by proxies.

use std::net::SocketAddr;
use std::time::Duration;

use openrusty_core::config::{BalancerKind, HealthConfig, UpstreamConfig};

use crate::tls::UpstreamTls;

/// One load-balancing target: an address plus a relative weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    /// Socket address the proxy connects to.
    pub addr: SocketAddr,
    /// Relative weight used by weighted balancers (e.g. SWRR). Never zero
    /// for validated configs.
    pub weight: u32,
}

/// A named pool of peers plus the policies applied when proxying to it.
/// Immutable after construction.
#[derive(Debug, Clone)]
pub struct Upstream {
    /// Unique name referenced by routes.
    pub name: String,
    /// Peer selection algorithm.
    pub kind: BalancerKind,
    /// How many extra attempts are allowed after the first failure.
    pub retries: u32,
    /// Retry the request on the next peer when the route times out.
    pub retry_on_timeout: bool,
    /// Per-attempt TCP connect timeout.
    pub connect_timeout: Duration,
    /// Idle keep-alive connections in the pooled clients are closed after
    /// this long.
    pub pool_idle_timeout: Duration,
    /// The balancing targets, in configuration order.
    pub peers: Vec<Peer>,
    /// Outbound TLS material; `None` = plaintext HTTP peers.
    pub tls: Option<UpstreamTls>,
    /// Passive health-check thresholds.
    pub health: HealthConfig,
}

/// Build an immutable [`Upstream`] snapshot from configuration.
///
/// `tls` is the already-built TLS material for the upstream's
/// `[upstreams.tls]` section (the caller builds it fallibly beforehand,
/// keeping this constructor pure and infallible); pass `None` for a
/// plaintext upstream.
pub fn from_config(cfg: &UpstreamConfig, tls: Option<UpstreamTls>) -> Upstream {
    Upstream {
        name: cfg.name.clone(),
        kind: cfg.balancer,
        retries: cfg.retries,
        retry_on_timeout: cfg.retry_on_timeout,
        connect_timeout: Duration::from_millis(cfg.connect_timeout_ms),
        pool_idle_timeout: Duration::from_millis(cfg.pool_idle_timeout_ms),
        peers: cfg
            .peers
            .iter()
            .map(|p| Peer {
                addr: p.addr,
                weight: p.weight,
            })
            .collect(),
        tls,
        health: cfg.health.clone(),
    }
}

/// Header names that apply to a single connection only (RFC 9110 7.6.1).
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "upgrade",
    "transfer-encoding",
];

/// True when `name` is a hop-by-hop header (case-insensitive).
pub fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP
        .iter()
        .any(|h| h.eq_ignore_ascii_case(name.trim()))
}

/// Drop every hop-by-hop header, preserving order of the rest.
pub fn strip_hop_by_hop(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name))
        .cloned()
        .collect()
}

/// First header value whose name matches (case-insensitive).
fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Comma-separated, trimmed, lowercase tokens of a header value.
fn tokens(value: &str) -> impl Iterator<Item = String> + '_ {
    value.split(',').map(|t| t.trim().to_ascii_lowercase())
}

/// Detect a WebSocket upgrade request:
/// `GET` + `Connection: upgrade` + `Upgrade: websocket` (all
/// case-insensitive, tolerant of multi-token `Connection` values).
pub fn is_websocket_upgrade(method: &str, headers: &[(String, String)]) -> bool {
    if !method.eq_ignore_ascii_case("GET") {
        return false;
    }
    let conn_upgrade = header_value(headers, "connection")
        .map(|v| tokens(v).any(|t| t == "upgrade"))
        .unwrap_or(false);
    let ws = header_value(headers, "upgrade")
        .map(|v| {
            tokens(v).any(|t| {
                // Tolerate parameters like "websocket; version=13".
                t.split(';').next().map(str::trim) == Some("websocket")
            })
        })
        .unwrap_or(false);
    conn_upgrade && ws
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up_cfg() -> UpstreamConfig {
        UpstreamConfig {
            name: "llm".into(),
            balancer: BalancerKind::Swrr,
            retries: 2,
            retry_on_timeout: true,
            connect_timeout_ms: 1500,
            pool_idle_timeout_ms: 30_000,
            endpoints: Vec::new(),
            peers: vec![
                openrusty_core::config::PeerConfig {
                    addr: "127.0.0.1:9001".parse().unwrap(),
                    weight: 5,
                },
                openrusty_core::config::PeerConfig {
                    addr: "127.0.0.1:9002".parse().unwrap(),
                    weight: 1,
                },
            ],
            tls: None,
            health: HealthConfig::default(),
        }
    }

    #[test]
    fn from_config_maps_fields() {
        let up = from_config(&up_cfg(), None);
        assert_eq!(up.name, "llm");
        assert_eq!(up.kind, BalancerKind::Swrr);
        assert_eq!(up.retries, 2);
        assert!(up.retry_on_timeout);
        assert_eq!(up.connect_timeout, Duration::from_millis(1500));
        assert_eq!(up.pool_idle_timeout, Duration::from_millis(30_000));
        assert_eq!(up.peers.len(), 2);
        assert_eq!(up.peers[0].weight, 5);
        assert_eq!(up.peers[1].addr.to_string(), "127.0.0.1:9002");
        assert_eq!(up.health.max_fails, HealthConfig::default().max_fails);
    }

    #[test]
    fn retry_on_timeout_defaults_to_false() {
        let mut cfg = up_cfg();
        cfg.retry_on_timeout = false;
        let up = from_config(&cfg, None);
        assert!(!up.retry_on_timeout);
    }

    #[test]
    fn hop_by_hop_names_are_case_insensitive() {
        for name in [
            "Connection",
            "KEEP-ALIVE",
            "proxy-authenticate",
            "Proxy-Authorization",
            "te",
            "Trailer",
            "Upgrade",
            "Transfer-Encoding",
        ] {
            assert!(is_hop_by_hop(name), "{name} should be hop-by-hop");
        }
        for name in ["content-type", "host", "x-forwarded-for", "accept"] {
            assert!(!is_hop_by_hop(name), "{name} is not hop-by-hop");
        }
    }

    #[test]
    fn strip_keeps_order_and_drops_hop_headers() {
        let headers: Vec<(String, String)> = vec![
            ("Host".into(), "a".into()),
            ("Connection".into(), "keep-alive".into()),
            ("X-One".into(), "1".into()),
            ("transfer-encoding".into(), "chunked".into()),
            ("X-Two".into(), "2".into()),
        ];
        let kept = strip_hop_by_hop(&headers);
        assert_eq!(
            kept,
            vec![
                ("Host".into(), "a".into()),
                ("X-One".into(), "1".into()),
                ("X-Two".into(), "2".into())
            ]
        );
    }

    fn hdr(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn websocket_upgrade_detected() {
        let h = hdr(&[("Connection", "Upgrade"), ("Upgrade", "websocket")]);
        assert!(is_websocket_upgrade("GET", &h));
        // Case-insensitive method and values, multi-token Connection.
        let h2 = hdr(&[
            ("connection", "keep-alive, upgrade"),
            ("upgrade", "WebSocket"),
        ]);
        assert!(is_websocket_upgrade("get", &h2));
    }

    #[test]
    fn websocket_upgrade_rejects_missing_parts() {
        let only_conn = hdr(&[("Connection", "upgrade")]);
        assert!(!is_websocket_upgrade("GET", &only_conn));
        let only_up = hdr(&[("Upgrade", "websocket")]);
        assert!(!is_websocket_upgrade("GET", &only_up));
        let both = hdr(&[("Connection", "upgrade"), ("Upgrade", "websocket")]);
        assert!(!is_websocket_upgrade("POST", &both));
        let h2c = hdr(&[("Connection", "upgrade"), ("Upgrade", "h2c")]);
        assert!(!is_websocket_upgrade("GET", &h2c));
        assert!(!is_websocket_upgrade("GET", &[]));
    }
}
