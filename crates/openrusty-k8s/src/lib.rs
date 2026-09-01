//! `openrusty-k8s`: credentials + HTTPS access to the Kubernetes apiserver.
//!
//! The gateway's ingress shape watches cluster `Ingress` objects and TLS
//! `Secret`s to render routes dynamically. This crate is the foundation
//! for that: it answers two questions and nothing more.
//!
//! 1. *How do we talk to the apiserver?* [`auth`] resolves a
//!    [`Cluster`] (endpoint + CA) and [`Credentials`] pair from a
//!    kubeconfig file, `$KUBECONFIG`, or the in-cluster service account,
//!    normalizing every flavor (static token, client certificate, exec
//!    plugin) into one enum.
//! 2. *What do requests look like?* [`client::Client`] turns that pair
//!    into a pooled HTTPS client (hyper + tokio-rustls) with
//!    [`Client::get`](client::Client::get) for generic reads and
//!    [`Client::watch_raw`](client::Client::watch_raw) for watch streams
//!    at the raw byte level.
//!
//! # Relationship to `openrusty-server`
//!
//! `openrusty-server` will, in a later milestone, construct a `Client` at
//! config load time and hand it to the ingress renderer. This crate does
//! not depend on the server (the dependency arrow points the other way)
//! and stays free of gateway concepts such as routes or upstreams.
//!
//! # Non-goals
//!
//! - not a general-purpose Kubernetes client: only the calls the gateway
//!   needs (list/watch of Ingress and Secret today) are planned;
//! - no watch state machine, no Ingress model, no route rendering (next
//!   milestone);
//! - no leader election, informer caches, or retry/backoff policies.
//!
//! Design rules mirror the rest of the workspace: parsing, resolution and
//! URL math are pure functions unit-tested against fixtures; the only
//! stateful pieces are the resource holders themselves (`Client`,
//! exec-plugin subprocesses); no real cluster is needed for the tests.

pub mod auth;
pub mod client;
pub mod connector;
pub mod error;

pub use auth::{load, Cluster, Credentials, ExecConfig};
pub use client::{watch_path, ByteStream, Client};
pub use error::{K8sError, Result};

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
