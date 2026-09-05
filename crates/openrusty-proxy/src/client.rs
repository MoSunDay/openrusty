//! Pooled, keep-alive HTTP/1 clients, one per upstream peer address.
//!
//! Building a hyper client allocates a connection pool, so clients are
//! cached per key and shared (they are cheaply `Clone`able). Plain-TCP
//! clients are keyed by `SocketAddr`; TLS clients by `(SocketAddr,
//! TlsClientKey)`, so a peer address can carry both a plaintext and a TLS
//! client simultaneously and a TLS-material change forces a fresh pool.
//! The first `get` for a key fixes that client's connect timeout and
//! idle keep-alive timeout; the pool's background timer expires idle
//! connections so they cannot leak.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use http_body_util::Full;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};

use crate::tls::{HttpsClient, HttpsConnector, UpstreamTls};

/// Outgoing request body type used by the pooled clients.
pub type HttpBody = Full<bytes::Bytes>;

/// A pooled hyper HTTP/1 client bound to the plain-TCP connector.
pub type HttpClient = Client<HttpConnector, HttpBody>;

/// Identity of one pooled client. `Http` keys the plain-TCP clients by
/// peer address; `Https` keys TLS clients by address plus TLS material
/// identity, so identical TLS configs reuse the warm pool across reloads
/// and changed material does not.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PoolKey {
    Http(SocketAddr),
    Https(SocketAddr, crate::tls::TlsClientKey),
}

/// Cache of one client per [`PoolKey`] (plain and TLS kept separately:
/// the two client types differ).
pub struct ClientPool {
    http: Mutex<HashMap<SocketAddr, HttpClient>>,
    https: Mutex<HashMap<(SocketAddr, crate::tls::TlsClientKey), HttpsClient>>,
}

/// Create an empty pool.
pub fn new_pool() -> ClientPool {
    ClientPool {
        http: Mutex::new(HashMap::new()),
        https: Mutex::new(HashMap::new()),
    }
}

impl Default for ClientPool {
    fn default() -> Self {
        new_pool()
    }
}

/// Get (or create) the plain-TCP client for `addr`.
///
/// The returned handle shares the pool's connection cache, so keep-alive
/// connections survive across requests. `connect_timeout` and
/// `pool_idle_timeout` only apply when the client for `addr` is created;
/// later calls with different values reuse the existing client. Prewarming
/// at boot/reload (see `apply_runtime`) makes the first creation happen off
/// the request path, so requests never pay client construction there.
///
/// The pool timer is what expires idle entries: without a registered timer,
/// hyper's connection pool never reaps keep-alive connections, leaking file
/// descriptors under churn. Idle clients now also honor `pool_idle_timeout`.
pub fn get(
    pool: &ClientPool,
    addr: SocketAddr,
    connect_timeout: Duration,
    pool_idle_timeout: Duration,
) -> HttpClient {
    let mut clients = pool.http.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = clients.get(&addr) {
        return existing.clone();
    }
    let client = build(connect_timeout, pool_idle_timeout);
    clients.insert(addr, client.clone());
    client
}

/// Get (or create) the TLS client for `addr` under the TLS material in
/// `tls`. Same pooling semantics as [`get`]; the key is
/// `(addr, tls.key)` so two different trust anchors or identities on one
/// address keep separate pools, and an unchanged key survives reloads.
pub fn get_tls(
    pool: &ClientPool,
    addr: SocketAddr,
    tls: &UpstreamTls,
    connect_timeout: Duration,
    pool_idle_timeout: Duration,
) -> HttpsClient {
    let key = (addr, tls.key.clone());
    let mut clients = pool.https.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = clients.get(&key) {
        return existing.clone();
    }
    let client = build_tls_client(addr, tls, connect_timeout, pool_idle_timeout);
    clients.insert(key, client.clone());
    client
}

/// Evict the pooled clients that are no longer configured.
///
/// Called by the reload path (`apply_runtime`): a client left behind for a
/// removed peer (or removed TLS material) would keep its keep-alive
/// connections (and file descriptors) alive until the idle timer fires,
/// and an address reuse by another process would otherwise silently reuse
/// the stale client.
pub fn evict_except(pool: &ClientPool, keep: &[PoolKey]) {
    let keep_http: Vec<SocketAddr> = keep
        .iter()
        .filter_map(|k| match k {
            PoolKey::Http(addr) => Some(*addr),
            PoolKey::Https(..) => None,
        })
        .collect();
    pool.http
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|addr, _| keep_http.contains(addr));
    pool.https
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(addr, tls_key), _| {
            keep.iter().any(|k| match k {
                PoolKey::Https(a, t) => a == addr && t == tls_key,
                PoolKey::Http(_) => false,
            })
        });
}

/// Build one HTTP/1 client with a bounded connect and a bounded keep-alive
/// pool drained by its own background timer.
fn build(connect_timeout: Duration, pool_idle_timeout: Duration) -> HttpClient {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(connect_timeout));
    connector.set_nodelay(true);
    // Defaults give an HTTP/1 keep-alive pool; `http2_only` stays off.
    Client::builder(TokioExecutor::new())
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(pool_idle_timeout)
        .build(connector)
}

/// Build one HTTP/1-over-TLS client with the same pooling policy as
/// [`build`]; the connector dials the fixed peer `addr` and upgrades to
/// TLS with the material in `tls` (SNI from `tls.server_name`).
fn build_tls_client(
    addr: SocketAddr,
    tls: &UpstreamTls,
    connect_timeout: Duration,
    pool_idle_timeout: Duration,
) -> HttpsClient {
    let server_name = rustls_pki_types::ServerName::try_from(tls.server_name.clone())
        .expect("validated server name");
    let connector = HttpsConnector {
        addr,
        server_name,
        tls: std::sync::Arc::clone(&tls.config),
        connect_timeout,
    };
    Client::builder(TokioExecutor::new())
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(pool_idle_timeout)
        .build(connector)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::{build, TlsClientKey};
    use openrusty_core::config::UpstreamTlsConfig;

    fn insecure_tls(name: &str) -> UpstreamTls {
        build(&UpstreamTlsConfig {
            server_name: name.to_string(),
            ca_cert: None,
            client_cert: None,
            client_key: None,
            insecure_skip_verify: true,
        })
        .unwrap()
    }

    #[test]
    fn pool_returns_clients_for_distinct_peers() {
        let pool = new_pool();
        let a: SocketAddr = "127.0.0.1:9001".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:9002".parse().unwrap();
        let connect_timeout = Duration::from_millis(250);
        let idle_timeout = Duration::from_secs(30);
        // Creation must not panic; repeat calls must keep working.
        let _ = get(&pool, a, connect_timeout, idle_timeout);
        let _ = get(&pool, a, connect_timeout, idle_timeout);
        let _ = get(&pool, b, connect_timeout, idle_timeout);
        // Two cached entries after touching two addresses.
        assert_eq!(pool.http.lock().unwrap().len(), 2);
    }

    #[test]
    fn http_and_https_clients_on_same_addr_coexist() {
        let pool = new_pool();
        let a: SocketAddr = "127.0.0.1:9443".parse().unwrap();
        let tls = insecure_tls("localhost");
        let ct = Duration::from_millis(250);
        let it = Duration::from_secs(30);
        let _ = get(&pool, a, ct, it);
        let _ = get_tls(&pool, a, &tls, ct, it);
        // Same address twice returns the same cached entry.
        let _ = get_tls(&pool, a, &tls, ct, it);
        assert_eq!(pool.http.lock().unwrap().len(), 1);
        assert_eq!(pool.https.lock().unwrap().len(), 1);

        // A different TLS identity on the same address is a separate pool.
        let other = insecure_tls("other.local");
        let _ = get_tls(&pool, a, &other, ct, it);
        assert_eq!(pool.https.lock().unwrap().len(), 2);
    }

    #[test]
    fn evict_drops_removed_keys_keeps_live_and_allows_new() {
        let pool = new_pool();
        let a: SocketAddr = "127.0.0.1:9001".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:9002".parse().unwrap();
        let c: SocketAddr = "127.0.0.1:9003".parse().unwrap();
        let ct = Duration::from_millis(250);
        let it = Duration::from_secs(30);
        let _ = get(&pool, a, ct, it);
        let _ = get(&pool, b, ct, it);
        assert_eq!(pool.http.lock().unwrap().len(), 2);

        // Apply the new key set {Http(a), Http(c)}: `b` is gone, `a` lives.
        evict_except(&pool, &[PoolKey::Http(a), PoolKey::Http(c)]);
        {
            let clients = pool.http.lock().unwrap();
            assert!(!clients.contains_key(&b), "removed addr must be evicted");
            assert!(clients.contains_key(&a), "kept addr must survive");
            assert_eq!(clients.len(), 1);
        }
        // The kept address still hands out its pooled client.
        let _ = get(&pool, a, ct, it);
        // The new address works like any first `get`.
        let _c_client = get(&pool, c, ct, it);
        assert_eq!(pool.http.lock().unwrap().len(), 2);
        evict_except(&pool, &[]);
        assert!(pool.http.lock().unwrap().is_empty());
    }

    #[test]
    fn evict_prunes_https_by_addr_and_tls_key() {
        let pool = new_pool();
        let a: SocketAddr = "127.0.0.1:9443".parse().unwrap();
        let tls = insecure_tls("localhost");
        let ct = Duration::from_millis(250);
        let it = Duration::from_secs(30);
        let _ = get_tls(&pool, a, &tls, ct, it);
        let _ = get(&pool, a, ct, it);

        // Same address but a different TLS key must NOT keep the old entry.
        let other_key = TlsClientKey::from_config(&UpstreamTlsConfig {
            server_name: "other.local".into(),
            ca_cert: None,
            client_cert: None,
            client_key: None,
            insecure_skip_verify: true,
        });
        evict_except(&pool, &[PoolKey::Https(a, other_key)]);
        assert!(pool.https.lock().unwrap().is_empty());
        // The plain client on the same address was dropped too (not kept).
        assert!(pool.http.lock().unwrap().is_empty());

        // Keeping the exact key preserves the entry.
        let _ = get_tls(&pool, a, &tls, ct, it);
        evict_except(&pool, &[PoolKey::Https(a, tls.key.clone())]);
        assert_eq!(pool.https.lock().unwrap().len(), 1);
    }
}
