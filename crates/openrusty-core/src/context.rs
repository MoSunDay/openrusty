//! Per-request context: immutable request facts plus mutable routing state.
//!
//! `ReqCtx` is plain data shared between the HTTP pipeline and the WASM host;
//! it lives inside the wasmtime `Store` data during plugin execution.

use std::net::SocketAddr;

/// One in-flight request. Fields are plain data so the wasm host can read
/// and mutate them without interior mutability tricks.
#[derive(Debug, Clone)]
pub struct ReqCtx {
    pub method: String,
    pub path: String,
    pub query: String,
    pub version: String,
    pub client_addr: SocketAddr,
    /// Request headers, order-preserving.
    pub headers: Vec<(String, String)>,
    /// Index of the matched route (into `Config::routes`), if any.
    pub route_index: Option<usize>,
    /// Upstream selected by routing.
    pub upstream: Option<String>,
    /// Peer index chosen by the balancer phase (into the upstream's peers).
    pub peer_index: Option<u32>,
    /// How many upstream attempts were made.
    pub attempts: u32,
    /// Addresses of peers already attempted for this request; the retry
    /// loop must never pick one of these again.
    pub tried: Vec<SocketAddr>,
}

impl ReqCtx {
    /// Client IP without the port (works for v4 and bracketed v6 alike).
    /// Returns an owned `String`; the context never leaks memory.
    pub fn client_ip(&self) -> String {
        self.client_addr.ip().to_string()
    }

    /// Record a peer address as attempted (idempotent).
    pub fn mark_tried(&mut self, addr: SocketAddr) {
        if !self.tried.contains(&addr) {
            self.tried.push(addr);
        }
    }

    /// True when this peer address was already attempted.
    pub fn is_tried(&self, addr: SocketAddr) -> bool {
        self.tried.contains(&addr)
    }

    /// First header value with the given name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Find the first route whose prefix matches `path` (pure).
pub fn match_route<'a, R>(
    routes: &'a [R],
    path: &str,
    prefix_of: impl Fn(&R) -> &'a str,
) -> Option<usize> {
    routes.iter().position(|r| path.starts_with(prefix_of(r)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_lookup_is_case_insensitive() {
        let mut ctx = ReqCtx {
            method: "GET".into(),
            path: "/".into(),
            query: String::new(),
            version: "HTTP/1.1".into(),
            client_addr: "127.0.0.1:1234".parse().unwrap(),
            headers: vec![("Content-Type".into(), "text/plain".into())],
            route_index: None,
            upstream: None,
            peer_index: None,
            attempts: 0,
            tried: Vec::new(),
        };
        assert_eq!(ctx.header("content-type"), Some("text/plain"));
        assert_eq!(ctx.header("x-missing"), None);
        let a: SocketAddr = "10.0.0.1:8000".parse().unwrap();
        assert!(!ctx.is_tried(a));
        ctx.mark_tried(a);
        ctx.mark_tried(a);
        assert!(ctx.is_tried(a));
        assert_eq!(ctx.tried.len(), 1);
        assert_eq!(ctx.client_ip(), "127.0.0.1");

        // The port is stripped for bracketed v6 as well, and the result is
        // owned (no leak).
        let mut v6 = ctx.clone();
        v6.client_addr = "[2001:db8::1]:9000".parse().unwrap();
        assert_eq!(v6.client_ip(), "2001:db8::1");
    }

    #[test]
    fn route_prefix_match() {
        let prefixes = ["/v1/chat", "/v1", "/"];
        let idx = match_route(&prefixes, "/v1/models", |p| *p);
        assert_eq!(idx, Some(1));
        let idx = match_route(&prefixes, "/v1/chat/completions", |p| *p);
        assert_eq!(idx, Some(0));
    }
}
