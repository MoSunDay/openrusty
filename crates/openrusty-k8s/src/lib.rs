//! `openrusty-k8s`: credentials, apiserver access, and the ingress watch
//! pipeline that turns cluster state into gateway config.
//!
//! The gateway watches cluster `Ingress` objects and TLS `Secret`s and
//! renders them into routes dynamically. This crate owns the whole
//! library-side pipeline - bytes in, `openrusty_core` config out - with
//! no server attached:
//!
//! 1. *How do we talk to the apiserver?* [`auth`] resolves a
//!    [`Cluster`] (endpoint + CA) and [`Credentials`] pair from a
//!    kubeconfig file, `$KUBECONFIG`, or the in-cluster service account,
//!    normalizing every flavor (static token, client certificate, exec
//!    plugin) into one enum. [`client::Client`] turns that pair into a
//!    pooled HTTPS client with [`Client::get`](client::Client::get) and
//!    [`Client::watch_raw`](client::Client::watch_raw).
//! 2. *What do the bytes mean?* [`model`] deserializes the serde subset
//!    of Ingress and Secret objects plus the watch stream envelope
//!    ([`WatchEvent`], [`parse_line`]).
//! 3. *How does it become state?* [`snapshot`] is the immutable watch
//!    state machine: [`apply_event`] folds one event into a new
//!    [`Snapshot`], never mutating the old one. [`watch_loop`] drives
//!    LIST -> watch -> debounced hand-over with exponential backoff, and
//!    keeps serving the last snapshot while the apiserver is unreachable
//!    (explicit stale-serve semantics, see `snapshot`).
//! 4. *How does it become config?* [`render`] filters by ingress class
//!    and renders routes ([`render_routes`]), ClusterIP upstreams
//!    ([`render_upstreams`]), TLS material ([`render_tls`]), and composes
//!    them with the static TOML routes under a strict conflict policy
//!    ([`merge`]).
//!
//! # Relationship to `openrusty-server`
//!
//! `openrusty-server` will, in a later milestone, construct a `Client` at
//! config load time, spawn [`watch_loop`], and apply the rendered config
//! through its reload path. This crate does not depend on the server (the
//! dependency arrow points the other way).
//!
//! # Non-goals
//!
//! - not a general-purpose Kubernetes client: only the calls the gateway
//!   needs (list/watch of Ingress and Secret today) are planned;
//! - no leader election, informer caches beyond [`Snapshot`], or
//!   server-side apply logic.
//!
//! Design rules mirror the rest of the workspace: parsing, folding and
//! rendering are pure functions unit-tested against fixtures; the only
//! stateful pieces are the resource holders ([`Client`], exec-plugin
//! subprocesses) and the [`watch_loop`] driver; no real cluster is needed
//! for the tests.

pub mod auth;
pub mod client;
pub mod connector;
pub mod error;
pub mod model;
pub mod render;
pub mod snapshot;
pub mod watch;

pub use auth::{load, Cluster, Credentials, ExecConfig};
pub use client::{watch_path, ByteStream, Client};
pub use error::{K8sError, Result};
pub use model::{
    parse_line, EventType, Ingress, K8sList, ObjectMeta, PathType, ResourceMeta, Secret, WatchEvent,
};
pub use render::{
    merge, render_routes, render_tls, render_upstreams, Conflict, RenderError, RouteKey, TlsPair,
};
pub use snapshot::{apply_event, replace_from_list, Snapshot};
pub use watch::{watch_loop, WatchOptions, WatchSource, WatchStats};

#[cfg(test)]
mod tests {
    use crate::{Cluster, Credentials};

    #[test]
    fn public_surface_is_reachable() {
        // Compile-time smoke test of the re-exports callers will use.
        let _: fn(Option<&std::path::Path>) -> crate::Result<(Cluster, Credentials)> = crate::load;
        assert_eq!(
            crate::watch_path("/api/v1/x", "7"),
            "/api/v1/x?watch=1&allowWatchBookmarks=true&resourceVersion=7"
        );
    }
}
