//! Execute one phase in one plugin instance under timeout + failure policy.
//!
//! Timeouts use wasmtime epoch interruption: an engine-scoped ticker
//! ([`crate::epoch`]) bumps the engine epoch every `TICK_MS`, and every
//! call sets its own store deadline (`set_epoch_deadline`) covering
//! exactly its own timeout window.

use crate::abi;
use crate::epoch::ticks_for;
use crate::instance::HostData;
use openrusty_core::config::FailPolicy;
use openrusty_core::phase::{Decision, Phase};
use std::time::Duration;
use wasmtime::{Engine, Linker, Memory, Module, Store, StoreLimitsBuilder, Trap, TypedFunc};

/// Epoch budget for instantiating a module (the phase-call timeout does not
/// exist yet at that point). A module whose start section loops forever
/// must fail the instantiation instead of hanging it.
const INSTANTIATE_TIMEOUT: Duration = Duration::from_secs(5);

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
///
/// Instantiation is bounded by [`INSTANTIATE_TIMEOUT`] (the epoch must be
/// advancing for the budget to fire): a `(start (loop (br 0)))` module
/// fails with an epoch trap instead of hanging the caller.
pub fn instantiate(
    engine: &Engine,
    linker: &Linker<HostData>,
    module: &Module,
    host_data: HostData,
    memory_limit_mb: u32,
) -> Result<PluginRt, wasmtime::Error> {
    instantiate_with_budget(
        engine,
        linker,
        module,
        host_data,
        memory_limit_mb,
        INSTANTIATE_TIMEOUT,
    )
}

/// [`instantiate`] with an explicit instantiation epoch budget.
pub fn instantiate_with_budget(
    engine: &Engine,
    linker: &Linker<HostData>,
    module: &Module,
    host_data: HostData,
    memory_limit_mb: u32,
    budget: Duration,
) -> Result<PluginRt, wasmtime::Error> {
    let mut host_data = host_data;
    host_data.limits = StoreLimitsBuilder::new()
        .memory_size((memory_limit_mb as usize).saturating_mul(1024 * 1024))
        .trap_on_grow_failure(true)
        .build();
    let mut store = Store::new(engine, host_data);
    store.limiter(|d| &mut d.limits);
    store.epoch_deadline_trap();
    store.set_epoch_deadline(ticks_for(budget));
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
    timeout: Duration,
    policy: FailPolicy,
    phase: Phase,
) -> Decision {
    match run_phase_outcome(rt, timeout, phase) {
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
pub fn run_phase_outcome(rt: &mut PluginRt, timeout: Duration, phase: Phase) -> PhaseOutcome {
    let plugin = rt.store.data().state.name().to_string();
    // The deadline is computed at call start against the engine-scoped
    // ticker: it covers exactly this call's window, so no stale timer from
    // a previous (or concurrent) call can shorten it.
    rt.store.set_epoch_deadline(ticks_for(timeout));
    let result = rt.on_phase.call(&mut rt.store, (phase as i32, 0));
    match result {
        Ok(code) => match Decision::from_abi(code) {
            Some(d) => PhaseOutcome::Decision(d),
            None => {
                // Kind label mirrors KIND_BAD_CODE in
                // openrusty-server/src/metrics.rs.
                rt.store.data().state.record_error("bad_code");
                PhaseOutcome::Error {
                    plugin,
                    kind: ErrorKind::BadCode(code),
                }
            }
        },
        Err(err) => {
            // Kind labels mirror KIND_TIMEOUT/KIND_TRAP in
            // openrusty-server/src/metrics.rs.
            let kind = if err.downcast_ref::<Trap>() == Some(&Trap::Interrupt) {
                rt.store.data().state.record_error("timeout");
                ErrorKind::Timeout
            } else {
                rt.store.data().state.record_error("trap");
                ErrorKind::Trap
            };
            tracing::warn!(plugin = %plugin, phase = phase.name(), error = %err, "plugin trap");
            PhaseOutcome::Error { plugin, kind }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epoch::{EpochTicker, TICK_MS};
    use crate::host_state::HostState;
    use crate::instance::new_host_data;
    use crate::linker::build_linker;
    use crate::registry::new_engine;
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

    /// Busy loop gated on the host clock: runs for a deterministic ~600ms
    /// wall window regardless of machine speed, so it always overlaps the
    /// slow session's 300ms timeout and still finishes far inside its own
    /// multi-second budget.
    const BUSY_MOD: &str = r#"
(module
  (import "openrusty" "host_now_ms" (func $now (result i64)))
  (func (export "orr_on_phase") (param i32 i32) (result i32)
    (local $deadline i64)
    (local.set $deadline (i64.add (call $now) (i64.const 600)))
    (loop $l (br_if $l (i64.lt_s (call $now) (local.get $deadline))))
    i32.const 0)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
  (memory (export "memory") 1))
"#;

    fn test_ticker() -> EpochTicker {
        new_engine().unwrap()
    }

    /// One instance on `engine`, ready for a phase run.
    fn rt_on(engine: &Engine, src: &str, mem_mb: u32) -> PluginRt {
        let linker = build_linker(engine).unwrap();
        let module = Module::new(engine, wat::parse_str(src).unwrap()).unwrap();
        let host = new_host_data(
            ctx(),
            Vec::new(),
            Arc::new(HostState::new("t".into())),
            Arc::new(HashMap::new()),
        );
        instantiate(engine, &linker, &module, host, mem_mb).unwrap()
    }

    /// A ticker plus one instance on its engine. The ticker MUST stay
    /// alive while the instance is used (it owns the epoch bumps).
    fn setup(src: &str, mem_mb: u32) -> (EpochTicker, PluginRt) {
        let ticker = test_ticker();
        let rt = rt_on(ticker.engine(), src, mem_mb);
        (ticker, rt)
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
            tried: Vec::new(),
        }
    }

    const CALM: Duration = Duration::from_secs(60);

    #[test]
    fn ok_module_returns_ok() {
        let (_ticker, mut rt) = setup(OK_MOD, 16);
        assert_eq!(
            run_phase(&mut rt, CALM, FailPolicy::FailOpen, Phase::PostRead),
            Decision::Ok
        );
        assert_eq!(rt.host_data().state.error_count(), 0);
    }

    #[test]
    fn done_and_deny_codes() {
        let (_ticker, mut rt) = setup(&ret_mod(-4), 16);
        assert_eq!(
            run_phase(&mut rt, CALM, FailPolicy::FailOpen, Phase::Content),
            Decision::Done
        );
        let (_ticker, mut rt) = setup(&ret_mod(403), 16);
        assert_eq!(
            run_phase(&mut rt, CALM, FailPolicy::FailOpen, Phase::Access),
            Decision::Deny(403)
        );
    }

    #[test]
    fn trap_uses_failure_policy() {
        // fail_open -> Declined
        let (_ticker, mut rt) = setup(TRAP_MOD, 16);
        assert_eq!(
            run_phase(&mut rt, CALM, FailPolicy::FailOpen, Phase::Access),
            Decision::Declined
        );
        assert_eq!(rt.host_data().state.error_count(), 1);
        // fail_closed -> Deny(503)
        let (_ticker, mut rt) = setup(TRAP_MOD, 16);
        assert_eq!(
            run_phase(&mut rt, CALM, FailPolicy::FailClosed, Phase::Access),
            Decision::Deny(503)
        );
        assert_eq!(rt.host_data().state.error_count(), 1);
    }

    #[test]
    fn bad_code_is_protocol_error() {
        let (_ticker, mut rt) = setup(&ret_mod(-2), 16);
        let outcome = run_phase_outcome(&mut rt, CALM, Phase::Access);
        assert_eq!(
            outcome,
            PhaseOutcome::Error {
                plugin: "t".into(),
                kind: ErrorKind::BadCode(-2)
            }
        );
        assert_eq!(rt.host_data().state.error_count(), 1);
    }

    // The engine-scoped ticker bumps the epoch from its own thread, so the
    // deadline fires even while the wasm call blocks this thread.
    #[test]
    fn infinite_loop_times_out() {
        let (_ticker, mut rt) = setup(LOOP_MOD, 16);
        let outcome = run_phase_outcome(&mut rt, Duration::from_millis(200), Phase::Access);
        assert_eq!(
            outcome,
            PhaseOutcome::Error {
                plugin: "t".into(),
                kind: ErrorKind::Timeout
            }
        );
        assert_eq!(rt.host_data().state.error_count(), 1);

        // Policy mapping on a fresh instance.
        let (_ticker, mut rt) = setup(LOOP_MOD, 16);
        assert_eq!(
            run_phase(
                &mut rt,
                Duration::from_millis(200),
                FailPolicy::FailClosed,
                Phase::Access
            ),
            Decision::Deny(503)
        );
        assert_eq!(rt.host_data().state.error_count(), 1);
    }

    /// Regression: a call that consumed (nearly) its whole timeout window
    /// must not leave the next call with a shortened deadline. Under the
    /// old spawn-and-forget watchdog the first call's stale bump trapped
    /// the follow-up call almost immediately.
    #[test]
    fn stale_bump_does_not_shorten_the_next_call() {
        let (_ticker, mut rt) = setup(LOOP_MOD, 16);

        let first_started = std::time::Instant::now();
        assert_eq!(
            run_phase(
                &mut rt,
                Duration::from_millis(300),
                FailPolicy::FailClosed,
                Phase::Access
            ),
            Decision::Deny(503)
        );
        // The deadline is tick-quantized against the ticker thread's own
        // 10ms grid (not the call's start), so a full window can complete
        // up to one tick early in wall time; anything below
        // `300ms - TICK_MS` would mean the trap fired before its budget.
        assert!(
            first_started.elapsed() >= Duration::from_millis(300 - TICK_MS),
            "first call must consume its own window (minus tick slack), took {:?}",
            first_started.elapsed()
        );

        // Immediately following call: must survive well past the previous
        // call's 300ms window. Threshold is half the budget so scheduler
        // jitter under parallel `cargo test` load cannot false-fail, while a
        // stale bump (trap ~300ms in) still fails the assertion.
        let second_started = std::time::Instant::now();
        assert_eq!(
            run_phase(
                &mut rt,
                Duration::from_millis(900),
                FailPolicy::FailClosed,
                Phase::Access
            ),
            Decision::Deny(503)
        );
        let second = second_started.elapsed();
        assert!(
            second >= Duration::from_millis(450),
            "a stale bump from the previous call trapped this call early: {second:?}"
        );
    }

    /// Regression: on one shared engine, one session timing out must leave
    /// a concurrent session untouched; the sibling keeps running through
    /// the slow call's timeout and completes on its own budget.
    #[test]
    fn concurrent_timeout_leaves_sibling_session_intact() {
        let ticker = test_ticker();
        let engine = ticker.engine().clone();
        let mut slow = rt_on(&engine, LOOP_MOD, 16);
        let mut fast = rt_on(&engine, BUSY_MOD, 16);

        let fast = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let decision = run_phase(
                &mut fast,
                Duration::from_secs(10),
                FailPolicy::FailClosed,
                Phase::Access,
            );
            (decision, started.elapsed())
        });
        // Slow session times out while the busy one is still running.
        assert_eq!(
            run_phase(
                &mut slow,
                Duration::from_millis(300),
                FailPolicy::FailClosed,
                Phase::Access
            ),
            Decision::Deny(503)
        );
        let (decision, elapsed) = fast.join().unwrap();
        assert_eq!(decision, Decision::Ok, "sibling session must succeed");
        assert!(
            elapsed >= Duration::from_millis(500),
            "sibling must run its own full window (past the slow call's 300ms timeout), took {elapsed:?}"
        );
    }

    #[test]
    fn memory_limit_traps_grow() {
        // 1 MiB ceiling; the guest grows one page at a time until the
        // limiter refuses, which traps (trap_on_grow_failure).
        let (_ticker, mut rt) = setup(GROW_MOD, 1);
        assert_eq!(
            run_phase(&mut rt, CALM, FailPolicy::FailOpen, Phase::BodyFilter),
            Decision::Declined
        );
        assert_eq!(rt.host_data().state.error_count(), 1);
    }

    /// A module whose start section loops forever must fail instantiation
    /// with an epoch trap instead of hanging the caller forever.
    #[test]
    fn instantiation_of_infinite_start_module_times_out() {
        const START_LOOP: &str = r#"
(module
  (start $s)
  (func $s (loop $l (br $l)))
  (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const 0)
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
  (memory (export "memory") 1))
"#;
        let ticker = test_ticker();
        let engine = ticker.engine();
        let linker = build_linker(engine).unwrap();
        let module = Module::new(engine, wat::parse_str(START_LOOP).unwrap()).unwrap();
        let host = new_host_data(
            ctx(),
            Vec::new(),
            Arc::new(HostState::new("t".into())),
            Arc::new(HashMap::new()),
        );
        let started = std::time::Instant::now();
        let err = match instantiate_with_budget(
            engine,
            &linker,
            &module,
            host,
            16,
            Duration::from_millis(250),
        ) {
            Ok(_) => panic!("infinite start section must fail instantiation"),
            Err(err) => err,
        };
        assert!(
            err.downcast_ref::<Trap>() == Some(&Trap::Interrupt),
            "expected an epoch trap, got: {err}"
        );
        assert!(
            started.elapsed() <= Duration::from_secs(2),
            "instantiation must fail within its epoch budget, took {:?}",
            started.elapsed()
        );
    }
}
