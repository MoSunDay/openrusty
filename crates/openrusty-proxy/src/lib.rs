//! `openrusty-proxy`: nginx-like gateway building blocks.
//!
//! This crate provides the proxy-side machinery that `openrusty-server`
//! composes into a running gateway:
//!
//! - [`upstream`] - immutable upstream snapshots derived from config, plus
//!   hop-by-hop header hygiene and WebSocket-upgrade detection.
//! - [`health`] - passive health checking keyed by upstream name so peer
//!   state survives config reloads.
//! - [`balancer`] - pure peer selection (smooth weighted round-robin,
//!   FNV-1a based ip_hash).
//! - [`client`] - pooled keep-alive HTTP/1 clients, one per peer address.
//! - [`forward`] - request forwarding with retryability classification and
//!   bidirectional tunneling for upgraded connections.
//! - [`orig_dst`] - original-destination lookup (`SO_ORIGINAL_DST` /
//!   `IP6T_SO_ORIGINAL_DST`) for transparently redirected connections.
//! - [`detect`] - raw protocol sniffing (`H1` / `H2` / `Opaque`) over a
//!   redirected connection's first bytes, bytes never dropped.
//! - [`tunnel`] - opaque TCP passthrough (`copy_bidirectional` wrapper)
//!   for transparent inbound and outbound forwarding.
//! - [`loop_guard`] - refusal test ([`is_loopback`]) for transparently
//!   intercepted connections whose original destination is the gateway
//!   itself.
//!
//! The public surface is plain structs and (mostly) pure functions: state
//! lives in explicitly passed registries/pools, selection math is pure, and
//! the only async code is the network I/O in [`forward`] and [`tunnel`].

pub mod balancer;
pub mod client;
pub mod detect;
pub mod forward;
pub mod health;
pub mod loop_guard;
pub mod orig_dst;
pub mod tunnel;
pub mod upstream;

pub use balancer::{ip_hash_pick, swrr_next};
pub use client::{evict_except, get, new_pool, ClientPool, HttpBody, HttpClient};
pub use detect::{detect, Protocol};
pub use forward::{
    failure_kind, forward, is_idempotent, may_retry, merge_xff, tunnel, FailureKind, ForwardError,
    ForwardRequest,
};
pub use health::{
    active_peers, evaluate_active, evaluate_failure, healthy_indices, is_active_healthy,
    is_healthy, new, record_failure, record_probe, record_success, register, HealthRegistry,
};
pub use loop_guard::is_loopback;
pub use orig_dst::original_dst;
// `tunnel::tunnel` aliased in the flat surface: the name `tunnel` is taken
// by `forward::tunnel`; the function stays reachable as `tunnel::tunnel`.
pub use tunnel::tunnel as tcp_tunnel;
pub use upstream::{
    from_config, is_hop_by_hop, is_websocket_upgrade, strip_hop_by_hop, Peer, Upstream,
};
