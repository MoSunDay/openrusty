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
}

impl ReqCtx {
    pub fn client_ip(&self) -> &str {
        // Strip the port; works for v4 and bracketed v6 alike.
        self.client_addr.ip().to_string().leak()
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
        let ctx = ReqCtx {
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
        };
        assert_eq!(ctx.header("content-type"), Some("text/plain"));
        assert_eq!(ctx.header("x-missing"), None);
        assert_eq!(ctx.client_ip(), "127.0.0.1");
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
