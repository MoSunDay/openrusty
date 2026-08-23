//! Pooled, keep-alive HTTP/1 clients, one per upstream peer address.
//!
//! Building a hyper client allocates a connection pool, so clients are
//! cached per `SocketAddr` and shared (they are cheaply `Clone`able).
//! The first `get` for an address fixes that client's connect timeout.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use http_body_util::Full;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

/// Outgoing request body type used by the pooled clients.
pub type HttpBody = Full<bytes::Bytes>;

/// A pooled hyper HTTP/1 client bound to the plain-TCP connector.
pub type HttpClient = Client<HttpConnector, HttpBody>;

/// Cache of one [`HttpClient`] per peer address.
pub struct ClientPool {
    clients: Mutex<HashMap<SocketAddr, HttpClient>>,
}

/// Create an empty pool.
pub fn new_pool() -> ClientPool {
    ClientPool {
        clients: Mutex::new(HashMap::new()),
    }
}

impl Default for ClientPool {
    fn default() -> Self {
        new_pool()
    }
}

/// Get (or create) the client for `addr`.
///
/// The returned handle shares the pool's connection cache, so keep-alive
/// connections survive across requests. `connect_timeout` only applies when
/// the client for `addr` is created; later calls with a different timeout
/// reuse the existing client.
pub fn get(pool: &ClientPool, addr: SocketAddr, connect_timeout: Duration) -> HttpClient {
    let mut clients = pool.clients.lock().unwrap();
    if let Some(existing) = clients.get(&addr) {
        return existing.clone();
    }
    let client = build(connect_timeout);
    clients.insert(addr, client.clone());
    client
}

/// Build one HTTP/1 client with keep-alive pooling and a bounded connect.
fn build(connect_timeout: Duration) -> HttpClient {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(connect_timeout));
    connector.set_nodelay(true);
    // Defaults give an HTTP/1 keep-alive pool; `http2_only` stays off.
    Client::builder(TokioExecutor::new()).build(connector)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_returns_clients_for_distinct_peers() {
        let pool = new_pool();
        let a: SocketAddr = "127.0.0.1:9001".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:9002".parse().unwrap();
        let timeout = Duration::from_millis(250);
        // Creation must not panic; repeat calls must keep working.
        let _ = get(&pool, a, timeout);
        let _ = get(&pool, a, timeout);
        let _ = get(&pool, b, timeout);
        // Two cached entries after touching two addresses.
        assert_eq!(pool.clients.lock().unwrap().len(), 2);
    }
}
