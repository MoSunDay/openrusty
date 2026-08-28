//! OpenRusty WASM plugin host: wasmtime-based execution of nginx-aligned
//! plugin phases with hot reload, per-request sessions and failure policy.
//!
//! The ABI contract lives in docs/wasm-abi.md.

pub mod abi;
pub mod epoch;
pub mod host_state;
pub mod instance;
pub mod linker;
pub mod mem;
pub mod registry;
pub mod runner;
pub mod session;

mod linker_kv;
mod linker_req;
mod registry_validate;

pub use epoch::{ticks_for, EpochTicker, TICK_MS};
pub use host_state::HostState;
pub use instance::{new_host_data, HeaderEdit, HostData, PeerView};
pub use linker::{build_linker, validate_module, AbiError, AbiInfo};
pub use registry::{new_engine, LoadedPlugin, PluginRegistry, PluginSnapshot, ReloadError};
pub use runner::{fallback_decision, instantiate, run_phase, ErrorKind, PhaseOutcome, PluginRt};
pub use session::RequestSession;
