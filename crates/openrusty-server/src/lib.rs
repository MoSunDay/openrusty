//! Library surface of the `openrusty` gateway.
//!
//! The `openrusty` binary (src/main.rs) is a thin shell over this lib
//! target: every module below is compiled exactly once and re-used by
//! external embedders (init-pro), who can construct the shared gateway
//! state with [`state::AppState::from_config`], wrap it in [`app::router`]
//! and serve HTTP/1.1 + h2c on one port in-process via [`h2c::serve`] (or
//! drive role-scoped sockets with [`listeners::serve`]).
//! Config swaps that never touch the filesystem go through
//! [`reload::apply_config`].
//!
//! Not part of the public API: `testutil` (scratch dirs, `OK_WAT`,
//! `boot_state`) is compiled only for this crate's own tests, and
//! `metrics_render` stays a private submodule of [`metrics`] (its render
//! entry point is re-exported as [`metrics::render`]).

pub mod active_probe;
pub mod app;
pub mod body_filter;
pub mod h2c;
pub mod ingress;
pub mod init;
pub mod listeners;
pub mod metrics;
pub mod pipeline;
pub mod pipeline_peer;
pub mod reload;
pub mod shutdown;
pub mod state;
pub mod tls;
pub mod transparent;
pub mod ws;

#[cfg(test)]
mod testutil;
