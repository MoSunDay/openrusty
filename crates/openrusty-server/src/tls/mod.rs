//! Listener TLS termination: configuration classification, boot-time
//! material loading, and the SNI certificate resolver.
//!
//! Layout (kept under the 400-line file budget by responsibility):
//!
//! - this module: the `tls` config interpretation ([`TlsSources`]) and
//!   the one-shot boot that turns it into a rustls [`ServerConfig`];
//! - [`resolver`]: the shared [`DynamicCertResolver`] (SNI map + static
//!   fallback) and the ingress apply entry point `update_from_tls_pairs`;
//! - [`accept`]: per-connection handshake/ALPN and the accept loop.
//!
//! Semantics pinned here:
//!
//! - **Sources.** A `tls = true` listener needs either a static
//!   `tls_cert + tls_key` pair, `[ingress]` enabled (rendered Secrets),
//!   or both. With both, ingress wins for every SNI name it serves and
//!   the static pair is the fallback default for misses; with no static
//!   pair a miss fails the handshake (logged at debug per connection).
//! - **One resolver, many listeners.** Every TLS-terminating listener
//!   wraps the same `Arc<DynamicCertResolver>` (via `AppState`), so one
//!   ingress apply is immediately visible everywhere. The first
//!   listener that declares a static pair seeds the shared fallback; a
//!   second distinct static pair would silently lose that race (v1
//!   carries at most one static default - keep one static TLS listener
//!   per gateway).
//! - **Rotation never disturbs in-flight connections.** rustls clones
//!   the selected `Arc<CertifiedKey>` into each session, so swapping
//!   resolver tables only affects handshakes started afterwards.
//! - **Static material is boot-only.** It is read once while binding
//!   listeners (fail-fast); config reloads never re-read it and never
//!   rebind sockets. The dynamic rotation path is ingress-driven
//!   (`update_from_tls_pairs` on every successful apply).

mod accept;
mod resolver;

pub use resolver::{update_from_tls_pairs, DynamicCertResolver};

pub(crate) use accept::serve_listener;

// Test-only re-export: data-plane code consumes `accept_tls` inside
// `accept.rs`; unit tests reach it through `super::*`.
#[cfg(test)]
pub(crate) use accept::accept_tls;

use openrusty_core::config::ListenerConfig;
use rustls::ServerConfig;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(test)]
mod tests;

/// Does this listener actually terminate TLS? The `tls` flag is
/// meaningful on `inbound`/`admin`; an outbound listener parses but
/// ignores it (egress termination is not v1 scope). Mirrors
/// `listeners::uses_transparent` so the dispatch rule lives with the
/// other assembly predicates.
pub(crate) fn uses_tls(l: &ListenerConfig) -> bool {
    l.tls && !matches!(l.role, openrusty_core::config::ListenerRole::Outbound)
}

/// Which certificate sources back one TLS listener (pure derivation
/// from the config; `ingress_enabled` is `[ingress].enabled`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsSources {
    /// Static pair only: seeded as the resolver fallback.
    Static { cert: PathBuf, key: PathBuf },
    /// Ingress-rendered secrets only: the SNI map, no fallback.
    Ingress,
    /// Both: ingress owns the SNI map, the static pair is the fallback
    /// default for SNI misses.
    StaticWithIngress { cert: PathBuf, key: PathBuf },
}

impl TlsSources {
    /// Classify one listener. Validation guarantees a source exists
    /// whenever `uses_tls` holds; the `Ingress` arm keeps that verdict
    /// even if it is ever reached in an odd state.
    pub fn classify(l: &ListenerConfig, ingress_enabled: bool) -> Self {
        let static_pair = match (l.tls_cert.clone(), l.tls_key.clone()) {
            (Some(cert), Some(key)) => Some((cert, key)),
            _ => None,
        };
        match (static_pair, ingress_enabled) {
            (Some((cert, key)), true) => Self::StaticWithIngress { cert, key },
            (Some((cert, key)), false) => Self::Static { cert, key },
            (None, _) => Self::Ingress,
        }
    }
}

/// One TLS listener's assembly plan: the shared resolver plus the
/// classified sources (pure output of `listeners::mounts`, consumed by
/// `listeners::spawn`).
pub struct TlsPlan {
    pub resolver: Arc<DynamicCertResolver>,
    pub sources: TlsSources,
}

/// Read the PEM material named by `sources` and build the rustls server
/// config one TLS listener accepts with (boot-time, fail-fast: an
/// unreadable file aborts the bind phase before anything serves).
///
/// - static sources are seeded into the shared resolver as the fallback;
/// - ALPN offers `h2` + `http/1.1` (or only `http/1.1` when the listener
///   forces HTTP/1.1), and `accept::accept_tls` maps the negotiated
///   value onto [`ProtoMode`];
/// - the resolver reference is shared, so the returned config never
///   pins certificate material: rotation happens entirely inside it.
pub(crate) fn boot(
    plan: &TlsPlan,
    http1_only: bool,
) -> io::Result<Arc<ServerConfig>> {
    if let TlsSources::Static { cert, key } | TlsSources::StaticWithIngress { cert, key } =
        &plan.sources
    {
        let cert_pem = std::fs::read_to_string(cert).map_err(|e| {
            io::Error::other(format!("tls_cert {}: {e}", cert.display()))
        })?;
        let key_pem = std::fs::read_to_string(key)
            .map_err(|e| io::Error::other(format!("tls_key {}: {e}", key.display())))?;
        resolver::seed_fallback(&plan.resolver, &openrusty_k8s::render::TlsPair {
            cert_pem,
            key_pem,
        })
        .map_err(io::Error::other)?;
    }

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|e| io::Error::other(format!("no enabled TLS version: {e}")))?
        .with_no_client_auth()
        .with_cert_resolver(plan.resolver.clone());
    config.alpn_protocols = if http1_only {
        vec![b"http/1.1".to_vec()]
    } else {
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    };
    Ok(Arc::new(config))
}
