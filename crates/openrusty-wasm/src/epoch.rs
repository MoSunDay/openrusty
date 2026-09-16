//! Engine-scoped epoch ticker for wasmtime epoch interruption.
//!
//! One background thread per engine bumps the engine epoch every
//! [`TICK_MS`]; per-call timeouts are then expressed as store deadlines
//! (`Store::set_epoch_deadline`) computed at call start.
//!
//! This replaces the previous spawn-and-forget watchdog (one task per
//! phase call): those tasks could not be cancelled, so a stale bump from
//! an already-finished call could trap a later or concurrent call. With a
//! shared ticker the deadline always covers exactly the caller's own
//! window, regardless of how often the epoch advances.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use wasmtime::Engine;

/// Epoch tick granularity in milliseconds. Every deadline is a multiple of
/// this, so timeouts are honoured with up to one tick of extra slack.
pub const TICK_MS: u64 = 10;

/// Whole epoch ticks covering `timeout`; always at least one tick so a
/// (theoretically) zero timeout still cannot run unchecked.
pub fn ticks_for(timeout: Duration) -> u64 {
    std::cmp::max(
        1,
        u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX) / TICK_MS,
    )
}

/// Bumps the engine epoch every [`TICK_MS`] until dropped.
///
/// Plain struct + `Drop` (no inheritance): dropping the ticker sets the
/// stop flag and joins the thread, so a dropped engine cannot leak its
/// ticker thread.
pub struct EpochTicker {
    engine: Engine,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl EpochTicker {
    /// Spawn the ticker thread for `engine`.
    pub fn new(engine: Engine) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let ticked = engine.clone();
        let join = thread::Builder::new()
            .name("orr-epoch-ticker".to_string())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(TICK_MS));
                    ticked.increment_epoch();
                }
            });
        match join {
            Ok(join) => EpochTicker {
                engine,
                stop,
                join: Some(join),
            },
            Err(e) => {
                // Without a ticker no deadline ever fires; phase calls
                // would run to completion (or trap for other reasons).
                // Loudly report instead of hanging silently.
                tracing::error!(error = %e, "epoch ticker thread failed to spawn");
                EpochTicker {
                    engine,
                    stop,
                    join: None,
                }
            }
        }
    }

    /// The engine this ticker drives.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use wasmtime::{Instance, Module, Store};

    fn raw_engine() -> Engine {
        let mut cfg = wasmtime::Config::new();
        cfg.epoch_interruption(true);
        Engine::new(&cfg).unwrap()
    }

    #[test]
    fn ticks_are_at_least_one_and_track_timeout() {
        assert_eq!(ticks_for(Duration::from_millis(0)), 1);
        assert_eq!(ticks_for(Duration::from_millis(9)), 1);
        assert_eq!(ticks_for(Duration::from_millis(10)), 1);
        assert_eq!(ticks_for(Duration::from_millis(250)), 25);
        assert_eq!(ticks_for(Duration::from_secs(60)), 6_000);
    }

    #[test]
    fn ticker_advances_the_epoch_until_dropped() {
        // Long-running loop: wasmtime emits epoch checks at loop back-edges
        // (a trivial body would never check and could not trap).
        const LOOPER: &str = r#"(module
  (func (export "f") (result i32)
    (local $i i32)
    (loop $l
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_u (local.get $i) (i32.const 200000000))))
    (local.get $i)))"#;

        // A store whose deadline is one bump away traps as soon as the
        // ticker has bumped the epoch at all.
        fn probe(engine: &Engine, module: &Module) -> wasmtime::Result<i32> {
            let mut store = Store::new(engine, ());
            store.epoch_deadline_trap();
            store.set_epoch_deadline(1);
            let instance = Instance::new(&mut store, module, &[])?;
            let f = instance.get_typed_func::<(), i32>(&mut store, "f")?;
            f.call(&mut store, ())
        }

        let engine = raw_engine();
        let module = Module::new(&engine, wat::parse_str(LOOPER).unwrap()).unwrap();
        let ticker = EpochTicker::new(engine.clone());
        std::thread::sleep(Duration::from_millis(120));
        assert!(
            probe(&engine, &module).is_err(),
            "epoch must advance while the ticker lives"
        );
        drop(ticker);
        std::thread::sleep(Duration::from_millis(120));
        assert!(
            probe(&engine, &module).is_ok(),
            "dropping the ticker must stop the epoch bumps"
        );
    }

    #[test]
    fn drop_joins_promptly() {
        let ticker = EpochTicker::new(raw_engine());
        let started = Instant::now();
        drop(ticker);
        assert!(started.elapsed() <= Duration::from_millis(TICK_MS * 4));
    }
}
