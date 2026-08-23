//! Core types shared by all OpenRusty crates: configuration, request
//! context, and nginx-aligned phase/decision semantics.
//!
//! Everything here is dependency-light and side-effect free.

pub mod config;
pub mod context;
pub mod phase;

pub use config::{load_config, Config, ConfigError, FailPolicy};
pub use context::ReqCtx;
pub use phase::{Decision, Phase};
