//! Per-request runtime state stored inside each wasmtime `Store`.
//!
//! `HostData` is plain data: the server seeds it, the linker imports read
//! and mutate it, and the session pulls results back out after each call.

use crate::host_state::HostState;
use bytes::Bytes;
use openrusty_core::ReqCtx;
use std::collections::HashMap;
use std::sync::Arc;
use wasmtime::{StoreLimits, StoreLimitsBuilder};

/// One upstream peer as visible to plugins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerView {
    pub name: String,
    pub addr: String,
    pub healthy: bool,
}

/// A deferred response-header mutation recorded by a plugin. The server
/// applies these after the header_filter phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderEdit {
    Set(String, String),
    Del(String),
}

/// Data carried by one wasmtime `Store` for the duration of one request.
pub struct HostData {
    /// Request facts + mutable routing state (peer_index is written by
    /// `balancer_set_peer`).
    pub ctx: ReqCtx,
    /// Peers of the routed upstream.
    pub peers: Vec<PeerView>,
    /// Plugin shared KV (survives reloads).
    pub state: Arc<HostState>,
    /// Plugin settings from `[plugins.settings.<name>]`.
    pub settings: Arc<HashMap<String, String>>,
    /// Header edits requested by the plugin (applied by the server).
    pub resp_edits: Vec<HeaderEdit>,
    /// Current response headers; seeded by the server before header_filter
    /// and kept in sync by the resp_header_* imports.
    pub resp_headers: Vec<(String, String)>,
    /// Response body written by the plugin via `resp_body_set`; consumed
    /// by the server when a phase short-circuits (`Done`/`Deny`). `None`
    /// = not set.
    pub resp_body: Option<Bytes>,
    /// Buffered request body; seeded by the server before the content
    /// phase (empty in earlier phases and for WebSocket upgrades).
    pub req_body: Bytes,
    /// Current body_filter chunk.
    pub body_chunk: Bytes,
    /// True when `body_chunk` is the final one.
    pub body_last: bool,
    /// Per-store resource limits (memory ceiling), installed by the runner.
    pub limits: StoreLimits,
}

/// Build a fresh `HostData` with empty response/body state and unlimited
/// store limits (the runner installs the real memory ceiling).
pub fn new_host_data(
    ctx: ReqCtx,
    peers: Vec<PeerView>,
    state: Arc<HostState>,
    settings: Arc<HashMap<String, String>>,
) -> HostData {
    HostData {
        ctx,
        peers,
        state,
        settings,
        resp_edits: Vec::new(),
        resp_headers: Vec::new(),
        resp_body: None,
        req_body: Bytes::new(),
        body_chunk: Bytes::new(),
        body_last: false,
        limits: StoreLimitsBuilder::new().build(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_state::HostState;

    fn ctx() -> ReqCtx {
        ReqCtx {
            method: "GET".into(),
            path: "/".into(),
            query: String::new(),
            version: "HTTP/1.1".into(),
            client_addr: "127.0.0.1:9999".parse().unwrap(),
            headers: Vec::new(),
            route_index: None,
            upstream: None,
            peer_index: None,
            attempts: 0,
            tried: Vec::new(),
        }
    }

    #[test]
    fn defaults_are_empty() {
        let d = new_host_data(
            ctx(),
            vec![PeerView {
                name: "p0".into(),
                addr: "127.0.0.1:1".into(),
                healthy: true,
            }],
            Arc::new(HostState::new("t".into())),
            Arc::new(HashMap::new()),
        );
        assert!(d.resp_edits.is_empty());
        assert!(d.resp_headers.is_empty());
        assert!(d.resp_body.is_none());
        assert!(d.req_body.is_empty());
        assert!(d.body_chunk.is_empty());
        assert!(!d.body_last);
        assert_eq!(d.peers.len(), 1);
    }
}
