//! Unified graceful shutdown: one signal, one three-phase sequence.
//!
//! Blueprint (linkerd2-proxy `main.rs`): on a shutdown trigger the proxy
//! stops accepting, notifies in-flight connections to finish, waits a
//! bounded grace period, then exits. Every trigger flips the same `watch`
//! flag - SIGTERM/SIGINT (binary) and `POST /openrusty/shutdown` (admin
//! plane) are strictly equivalent - and every path converges on [`run`]:
//!
//! 1. Flag to `true`: every accept loop (`h2c`, `transparent`) stops and
//!    closes its socket; already accepted connections drain through hyper's
//!    graceful shutdown (`h2c::GracefulDrain`), established opaque tunnels
//!    keep serving until their peers close or the grace expires.
//! 2. Bounded wait until every accept task ended and the in-flight counter
//!    reached zero.
//! 3. Summary log (drained vs. force-closed, drain wall-clock); exit 0
//!    either way, an expired grace only adds a warn. The sequence is a
//!    plain function over the signal sender and the accept task handles,
//!    so binary and in-process embedders drive the same path without
//!    going through OS signals.

use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use std::time::{Duration, Instant};
use tokio::{sync::watch, task::JoinHandle};

/// Accepted-but-unfinished connection count pooled across all listeners:
/// accept loops bump it per connection, the serving task decrements it;
/// [`run`] reads it for the phase-2 wait and phase-3 summary.
pub type InFlight = Arc<AtomicUsize>;

/// How often the phase-2 wait re-samples the in-flight counter.
const DRAIN_POLL: Duration = Duration::from_millis(10);

/// Unified shutdown signal plus drain bookkeeping. Pure data shared through
/// `AppState`; behaviour lives in the free functions below.
pub struct ShutdownSignal {
    /// Flip to `true` to start the sequence (SIGTERM/SIGINT and the admin
    /// endpoint both write this one flag).
    pub tx: watch::Sender<bool>,
    /// Read side: accept loops and connection drivers clone it; the
    /// readiness endpoint answers from a borrow.
    pub rx: watch::Receiver<bool>,
    /// Accepted connections not finished yet.
    pub in_flight: InFlight,
}

/// A fresh signal in the serving state (`false`, nobody draining).
pub fn new_signal() -> ShutdownSignal {
    let (tx, rx) = watch::channel(false);
    ShutdownSignal { tx, rx, in_flight: Arc::new(AtomicUsize::new(0)) }
}

/// True once the flag flipped (or the sender is gone, which every accept
/// loop treats the same way). The whole readiness answer: 200 while false,
/// 503 `draining` while true.
pub fn is_draining(rx: &watch::Receiver<bool>) -> bool {
    *rx.borrow() || rx.has_changed().is_err()
}

/// Resolves as soon as [`is_draining`] would return true; parks while the
/// gateway serves - the await the binary and accept loops share.
pub async fn wait_for_shutdown(mut rx: watch::Receiver<bool>) {
    if !is_draining(&rx) {
        let _ = rx.changed().await;
    }
}

/// Outcome of the three-phase sequence (see [`run`]).
#[derive(Clone, Copy, Debug)]
pub struct ShutdownReport {
    /// Connections that finished inside the grace window.
    pub drained: usize,
    /// Connections still alive when the grace expired; the caller's exit
    /// (or the embedder's teardown) force-closes them.
    pub forced: usize,
    /// Wall-clock time spent in the drain phases.
    pub elapsed: Duration,
    /// True when the grace expired before every connection finished.
    pub timed_out: bool,
    /// Accept tasks that ended with an error instead of the signal.
    pub task_errors: usize,
}

/// The three-phase sequence: stop accepting, bounded drain, summary.
///
/// `tasks` are the accept-loop handles from `listeners::spawn`. Returns the
/// summary instead of exiting so embedders decide what to do next; the
/// binary logs it and exits 0 either way.
pub async fn run(
    tx: watch::Sender<bool>,
    tasks: Vec<JoinHandle<std::io::Result<()>>>,
    in_flight: InFlight,
    grace: Duration,
) -> ShutdownReport {
    let started = Instant::now();
    let at_signal = in_flight.load(Ordering::Relaxed);

    // Phase 1: stop accepting. hyper's graceful shutdown takes over every
    // already accepted connection; running tunnels keep shoveling bytes.
    let _ = tx.send(true);
    tracing::info!(in_flight = at_signal, "shutdown: phase 1/3 - stop accepting");

    // Phase 2: bounded wait. Accept tasks end on the flag themselves; the
    // connections end when their peers are done (counter at zero).
    let mut task_errors = 0usize;
    let wait = async {
        for t in tasks {
            match t.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    task_errors += 1;
                    tracing::error!(error = %e, "accept task failed during drain");
                }
                Err(e) => {
                    task_errors += 1;
                    tracing::error!(error = %e, "accept task panicked during drain");
                }
            }
        }
        until_drained(&in_flight).await;
    };

    let timed_out = tokio::time::timeout(grace, wait).await.is_err();

    // Phase 3: summary. On timeout the still-open connections are counted
    // as force-closed; the warn is the audit trail for the operator.
    let remaining = in_flight.load(Ordering::Relaxed);
    let report = ShutdownReport {
        drained: at_signal.saturating_sub(remaining),
        forced: if timed_out { remaining } else { 0 },
        elapsed: started.elapsed(),
        timed_out,
        task_errors,
    };
    if timed_out {
        tracing::warn!(
            forced = report.forced,
            elapsed_ms = report.elapsed.as_millis() as u64,
            "shutdown: grace expired, force-closing remaining connections"
        );
    }
    tracing::info!(
        drained = report.drained,
        forced = report.forced,
        elapsed_ms = report.elapsed.as_millis() as u64,
        "shutdown: complete"
    );
    report
}

/// Resolves when every accepted connection has finished. Polls the plain
/// counter: bounded by `grace` anyway, and connections have no single
/// completion point to notify on.
async fn until_drained(in_flight: &InFlight) {
    while in_flight.load(Ordering::Relaxed) > 0 {
        tokio::time::sleep(DRAIN_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-in accept task: ends once the signal reaches it, like a real accept loop.
    fn accept_task(rx: watch::Receiver<bool>) -> JoinHandle<std::io::Result<()>> {
        let mut rx = rx;
        tokio::spawn(async move {
            let _ = rx.changed().await;
            Ok(())
        })
    }

    /// Drive the sequence the way the tests all do.
    async fn drive(
        sig: &ShutdownSignal,
        tasks: Vec<JoinHandle<std::io::Result<()>>>,
        grace: Duration,
    ) -> ShutdownReport {
        run(sig.tx.clone(), tasks, sig.in_flight.clone(), grace).await
    }


    /// A connection finishing shortly after the flip, decrementing the
    /// shared counter exactly like a drained `serve_conn`.
    fn draining_conn(signal: &ShutdownSignal, delay: Duration) {
        signal.in_flight.fetch_add(1, Ordering::Relaxed);
        let in_flight = signal.in_flight.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            in_flight.fetch_sub(1, Ordering::Relaxed);
        });
    }

    #[tokio::test]
    async fn drains_in_flight_connections_within_grace() {
        let sig = new_signal();
        draining_conn(&sig, Duration::from_millis(20));
        draining_conn(&sig, Duration::from_millis(40));
        let report = drive(&sig, vec![accept_task(sig.rx.clone())], Duration::from_secs(5)).await;
        assert!(!report.timed_out, "drain must fit the grace window");
        assert_eq!(report.drained, 2);
        assert_eq!(report.forced, 0);
        assert_eq!(report.task_errors, 0);
    }

    #[tokio::test]
    async fn expired_grace_force_closes_the_remaining() {
        let sig = new_signal();
        // A connection that never finishes (peer gone silent).
        sig.in_flight.fetch_add(1, Ordering::Relaxed);
        let report = drive(&sig, vec![accept_task(sig.rx.clone())], Duration::from_millis(50)).await;
        assert!(report.timed_out);
        assert_eq!(report.forced, 1);
        assert_eq!(report.drained, 0);
        assert!(report.elapsed >= Duration::from_millis(50));
    }

    #[tokio::test]
    async fn accept_task_failures_are_counted_not_swallowed() {
        let sig = new_signal();
        let broken: JoinHandle<std::io::Result<()>> =
            tokio::spawn(async { Err(std::io::Error::other("boom")) });
        let tasks = vec![accept_task(sig.rx.clone()), broken];
        let report = drive(&sig, tasks, Duration::from_secs(5)).await;
        assert_eq!(report.task_errors, 1);
        assert!(!report.timed_out);
    }

    #[tokio::test]
    async fn draining_flag_covers_flip_and_dropped_sender() {
        let sig = new_signal();
        assert!(!is_draining(&sig.rx));
        // A receiver cloned before the flip still observes it, and a parked
        // waiter resolves on it.
        let waiter = tokio::spawn(wait_for_shutdown(sig.rx.clone()));
        assert!(!waiter.is_finished());
        sig.tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter never observed the flip")
            .unwrap();
        assert!(is_draining(&sig.rx));

        // A dropped sender is the historical stop-everything path (tests,
        // embedders): same answer as an explicit flip.
        let (tx, rx) = watch::channel(false);
        drop(tx);
        assert!(is_draining(&rx));
        wait_for_shutdown(rx).await;
    }
}
