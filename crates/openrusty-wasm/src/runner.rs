//! Execute one phase in one plugin instance under timeout + failure policy.
//!
//! Timeouts use wasmtime epoch interruption: a watchdog task bumps the
//! engine epoch after `timeout`, and the store traps at its deadline.

use crate::abi;
use crate::instance::HostData;
use openrusty_core::config::FailPolicy;
use openrusty_core::phase::{Decision, Phase};
use std::time::Duration;
use wasmtime::{Engine, Linker, Memory, Module, Store, StoreLimitsBuilder, Trap, TypedFunc};

/// One instantiated plugin, ready to run phases for one request.
pub struct PluginRt {
    store: Store<HostData>,
    on_phase: TypedFunc<(i32, i32), i32>,
    /// Reserved for future host->guest data hand-offs.
    alloc: TypedFunc<i32, i32>,
    /// Reserved for future host->guest data hand-offs.
    memory: Memory,
}

impl PluginRt {
    pub fn host_data(&self) -> &HostData {
        self.store.data()
    }

    pub fn host_data_mut(&mut self) -> &mut HostData {
        self.store.data_mut()
    }

    pub fn alloc_func(&self) -> &TypedFunc<i32, i32> {
        &self.alloc
    }

    pub fn memory(&self) -> &Memory {
        &self.memory
    }
}

/// Instantiate `module` for one request with a per-store memory ceiling.
pub fn instantiate(
    engine: &Engine,
    linker: &Linker<HostData>,
    module: &Module,
    host_data: HostData,
    memory_limit_mb: u32,
) -> Result<PluginRt, wasmtime::Error> {
    let mut host_data = host_data;
    host_data.limits = StoreLimitsBuilder::new()
        .memory_size((memory_limit_mb as usize).saturating_mul(1024 * 1024))
        .trap_on_grow_failure(true)
        .build();
    let mut store = Store::new(engine, host_data);
    store.limiter(|d| &mut d.limits);
    store.epoch_deadline_trap();
    store.set_epoch_deadline(1);
    let inst = linker.instantiate(&mut store, module)?;
    let on_phase = inst.get_typed_func::<(i32, i32), i32>(&mut store, abi::EXPORT_ON_PHASE)?;
    let alloc = inst.get_typed_func::<i32, i32>(&mut store, abi::EXPORT_ALLOC)?;
    let memory = inst
        .get_memory(&mut store, abi::MEMORY_EXPORT)
        .ok_or_else(|| wasmtime::Error::msg("module exports no memory"))?;
    Ok(PluginRt {
        store,
        on_phase,
        alloc,
        memory,
    })
}

/// Why a plugin phase failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Wasm trap (unreachable, OOB, memory-limit grow, ...).
    Trap,
    /// Epoch deadline exceeded (watchdog fired).
    Timeout,
    /// Return value outside the ABI contract.
    BadCode(i32),
}

/// Full outcome of one phase invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseOutcome {
    Decision(Decision),
    Error { plugin: String, kind: ErrorKind },
}

/// Fallback decision for a failure policy.
pub fn fallback_decision(policy: FailPolicy) -> Decision {
    match policy {
        FailPolicy::FailOpen => Decision::Declined,
        FailPolicy::FailClosed => Decision::Deny(503),
    }
}

/// Run one phase and map any failure through `policy`. Errors are logged
/// and counted on the plugin's shared state.
pub fn run_phase(
    rt: &mut PluginRt,
    engine: &Engine,
    timeout: Duration,
    policy: FailPolicy,
    phase: Phase,
) -> Decision {
    match run_phase_outcome(rt, engine, timeout, phase) {
        PhaseOutcome::Decision(d) => d,
        PhaseOutcome::Error { plugin, kind } => {
            let fallback = fallback_decision(policy);
            tracing::warn!(
                plugin = %plugin,
                phase = phase.name(),
                kind = ?kind,
                fallback = ?fallback,
                "plugin phase failed; applying failure policy"
            );
            fallback
        }
    }
}

/// Run one phase, distinguishing decisions from error kinds. Every error
/// path records an error on the plugin's shared state.
pub fn run_phase_outcome(
    rt: &mut PluginRt,
    engine: &Engine,
    timeout: Duration,
    phase: Phase,
) -> PhaseOutcome {
    let plugin = rt.store.data().state.name().to_string();
    // Rebase the deadline: also shields this call from a watchdog bump that
    // a previous (already returned) call's watchdog may still fire. A bump
    // arriving from an unrelated stale watchdog during THIS call can still
    // trap it early; that collateral risk is accepted in v1.
    rt.store.set_epoch_deadline(1);
    arm_watchdog(engine, timeout);
    let result = rt.on_phase.call(&mut rt.store, (phase as i32, 0));
    match result {
        Ok(code) => match Decision::from_abi(code) {
            Some(d) => PhaseOutcome::Decision(d),
            None => {
                rt.store.data().state.record_error();
                PhaseOutcome::Error {
                    plugin,
                    kind: ErrorKind::BadCode(code),
                }
            }
        },
        Err(err) => {
            rt.store.data().state.record_error();
            let kind = if err.downcast_ref::<Trap>() == Some(&Trap::Interrupt) {
                ErrorKind::Timeout
            } else {
                ErrorKind::Trap
            };
            tracing::warn!(plugin = %plugin, phase = phase.name(), error = %err, "plugin trap");
            PhaseOutcome::Error { plugin, kind }
        }
    }
}

/// Spawn a watchdog that bumps the engine epoch after `timeout`. Runs only
/// inside a tokio runtime; without one (e.g. plain unit tests) the call
/// proceeds without a watchdog.
///
/// The watchdog cannot be cancelled once spawned: if the wasm call returns
/// first, the bump may still fire later. Each call rebases its deadline
/// before running, so a stale bump at worst shortens the NEXT call by one
/// epoch increment worth of work (documented collateral in run_phase_outcome).
fn arm_watchdog(engine: &Engine, timeout: Duration) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let engine = engine.clone();
    handle.spawn(async move {
        tokio::time::sleep(timeout).await;
        engine.increment_epoch();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_state::HostState;
    use crate::instance::new_host_data;
    use crate::linker::build_linker;
    use openrusty_core::ReqCtx;
    use std::collections::HashMap;
    use std::sync::Arc;

    const OK_MOD: &str = r#"
(module
  (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const 0)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
  (memory (export "memory") 1))
"#;

    fn ret_mod(code: i32) -> String {
        format!(
            r#"(module
              (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const {code})
              (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
              (memory (export "memory") 1))"#
        )
    }

    const TRAP_MOD: &str = r#"
(module
  (func (export "orr_on_phase") (param i32 i32) (result i32) unreachable)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
  (memory (export "memory") 1))
"#;

    const LOOP_MOD: &str = r#"
(module
  (func (export "orr_on_phase") (param i32 i32) (result i32)
    (loop $l (br $l))
    i32.const 0)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
  (memory (export "memory") 1))
"#;

    const GROW_MOD: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "orr_on_phase") (param i32 i32) (result i32)
    (loop $l
      (if (i32.eq (memory.grow (i32.const 1)) (i32.const -1)) (then unreachable))
      (br $l))
    i32.const 0)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0))
"#;

    fn engine() -> Engine {
        let mut cfg = wasmtime::Config::new();
        cfg.epoch_interruption(true);
        Engine::new(&cfg).unwrap()
    }

    fn ctx() -> ReqCtx {
        ReqCtx {
            method: "GET".into(),
            path: "/".into(),
            query: String::new(),
            version: "HTTP/1.1".into(),
            client_addr: "127.0.0.1:1".parse().unwrap(),
            headers: Vec::new(),
            route_index: None,
            upstream: None,
            peer_index: None,
            attempts: 0,
        }
    }

    fn setup(src: &str, mem_mb: u32) -> (Engine, PluginRt) {
        let engine = engine();
        let linker = build_linker(&engine).unwrap();
        let module = Module::new(&engine, wat::parse_str(src).unwrap()).unwrap();
        let host = new_host_data(
            ctx(),
            Vec::new(),
            Arc::new(HostState::new("t".into())),
            Arc::new(HashMap::new()),
        );
        let rt = instantiate(&engine, &linker, &module, host, mem_mb).unwrap();
        (engine, rt)
    }

    const CALM: Duration = Duration::from_secs(60);

    #[tokio::test]
    async fn ok_module_returns_ok() {
        let (engine, mut rt) = setup(OK_MOD, 16);
        assert_eq!(
            run_phase(
                &mut rt,
                &engine,
                CALM,
                FailPolicy::FailOpen,
                Phase::PostRead
            ),
            Decision::Ok
        );
        assert_eq!(rt.host_data().state.error_count(), 0);
    }

    #[tokio::test]
    async fn done_and_deny_codes() {
        let (engine, mut rt) = setup(&ret_mod(-4), 16);
        assert_eq!(
            run_phase(&mut rt, &engine, CALM, FailPolicy::FailOpen, Phase::Content),
            Decision::Done
        );
        let (engine, mut rt) = setup(&ret_mod(403), 16);
        assert_eq!(
            run_phase(&mut rt, &engine, CALM, FailPolicy::FailOpen, Phase::Access),
            Decision::Deny(403)
        );
    }

    #[tokio::test]
    async fn trap_uses_failure_policy() {
        // fail_open -> Declined
        let (engine, mut rt) = setup(TRAP_MOD, 16);
        assert_eq!(
            run_phase(&mut rt, &engine, CALM, FailPolicy::FailOpen, Phase::Access),
            Decision::Declined
        );
        assert_eq!(rt.host_data().state.error_count(), 1);
        // fail_closed -> Deny(503)
        let (engine, mut rt) = setup(TRAP_MOD, 16);
        assert_eq!(
            run_phase(
                &mut rt,
                &engine,
                CALM,
                FailPolicy::FailClosed,
                Phase::Access
            ),
            Decision::Deny(503)
        );
        assert_eq!(rt.host_data().state.error_count(), 1);
    }

    #[tokio::test]
    async fn bad_code_is_protocol_error() {
        let (engine, mut rt) = setup(&ret_mod(-2), 16);
        let outcome = run_phase_outcome(&mut rt, &engine, CALM, Phase::Access);
        assert_eq!(
            outcome,
            PhaseOutcome::Error {
                plugin: "t".into(),
                kind: ErrorKind::BadCode(-2)
            }
        );
        assert_eq!(rt.host_data().state.error_count(), 1);
    }

    // The watchdog needs a worker thread to fire while the wasm call
    // blocks, so this test runs on a multi-thread runtime (like the real
    // server does).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn infinite_loop_times_out() {
        let (engine, mut rt) = setup(LOOP_MOD, 16);
        let outcome = run_phase_outcome(&mut rt, &engine, Duration::from_millis(30), Phase::Access);
        assert_eq!(
            outcome,
            PhaseOutcome::Error {
                plugin: "t".into(),
                kind: ErrorKind::Timeout
            }
        );
        assert_eq!(rt.host_data().state.error_count(), 1);

        // Policy mapping on a fresh instance.
        let (engine, mut rt) = setup(LOOP_MOD, 16);
        assert_eq!(
            run_phase(
                &mut rt,
                &engine,
                Duration::from_millis(30),
                FailPolicy::FailClosed,
                Phase::Access
            ),
            Decision::Deny(503)
        );
        assert_eq!(rt.host_data().state.error_count(), 1);
    }

    #[tokio::test]
    async fn memory_limit_traps_grow() {
        // 1 MiB ceiling; the guest grows one page at a time until the
        // limiter refuses, which traps (trap_on_grow_failure).
        let (engine, mut rt) = setup(GROW_MOD, 1);
        assert_eq!(
            run_phase(
                &mut rt,
                &engine,
                CALM,
                FailPolicy::FailOpen,
                Phase::BodyFilter
            ),
            Decision::Declined
        );
        assert_eq!(rt.host_data().state.error_count(), 1);
    }
}
