//! openrusty: an nginx-like WASM-extensible gateway.
//!
//! HTTP/1.1 + h2c on one port, plugin phases via wasmtime, proxying with
//! retries, WebSocket pass-through, SSE streaming, atomic hot reload.
//!
//! Thin binary glue only: the gateway itself lives in the lib target
//! (`openrusty_server`), so external embedders (init-pro) reuse the same
//! compiled modules and serve the gateway in-process.

use openrusty_core::load_config;
use openrusty_server::{
    active_probe, check, ingress, inject, init, listeners, logging, reload, shutdown, state,
};
use openrusty_wasm::host_state;
use std::path::PathBuf;
use std::time::Duration;
use tokio::signal::unix::SignalKind;
use tracing_subscriber::EnvFilter;

/// Subcommand dispatch happens before any config work: `openrusty
/// iptables-init` is a one-shot parameter-plane tool that must run (and
/// fail fast) without a gateway config file or a serving runtime.
fn arg_is(name: &str) -> bool {
    std::env::args().nth(1).as_deref() == Some(name)
}

fn resolve_config_path() -> PathBuf {
    config_from(std::env::args().nth(1))
}

/// Config path from an explicit CLI argument, else `OPENRUSTY_CONFIG`, else
/// the documented default. Shared by the serve path (arg 1) and the `-t`
/// dry run (arg 2).
fn config_from(arg: Option<String>) -> PathBuf {
    arg.or_else(|| std::env::var("OPENRUSTY_CONFIG").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config/openrusty.toml"))
}

#[tokio::main]
async fn main() {
    if arg_is(init::SUBCOMMAND) {
        std::process::exit(init::exec::run_cli(std::env::args().skip(2).collect()));
    }
    // `openrusty inject` rewrites a workload manifest from stdin to stdout
    // (linkerd-inject shape); same one-shot contract, no config, no runtime.
    if arg_is(inject::SUBCOMMAND) {
        std::process::exit(inject::run_cli(
            std::io::stdin().lock(),
            std::io::stdout().lock(),
        ));
    }
    // `openrusty -t [CONFIG]` (also `--test`): nginx-style dry run. Parse +
    // validate the config and compile every plugin, print the report,
    // exit. Runs before any tracing setup or listener work, so it never
    // serves.
    if arg_is("-t") || arg_is("--test") {
        let path = config_from(std::env::args().nth(2));
        match check::run(&path) {
            Ok(summary) => {
                summary.print();
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("openrusty: configuration test failed: {e}");
                std::process::exit(1);
            }
        }
    }
    let config_path = resolve_config_path();
    if !config_path.is_file() {
        eprintln!(
            "openrusty: config file not found: {}\nusage: openrusty [CONFIG] | openrusty -t [CONFIG] to test it (or set OPENRUSTY_CONFIG)",
            config_path.display()
        );
        std::process::exit(1);
    }
    let cfg = match load_config(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("openrusty: invalid config {}: {e}", config_path.display());
            std::process::exit(1);
        }
    };

    let filter =
        EnvFilter::try_new(&cfg.server.log_level).unwrap_or_else(|_| EnvFilter::new("warn"));
    // Log sink: stdout by default (journald captures it), or the file named
    // by `server.log_file`. An unopenable file is fatal here - tracing is
    // not up yet, so the error goes to stderr where boot problems belong.
    let (log_writer, log_guard, reopen_handle) = match logging::init(&cfg.server) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("openrusty: cannot open log file: {e}");
            std::process::exit(1);
        }
    };
    // Non-blocking writer: log calls hand formatted output to a dedicated
    // thread; the guard must outlive every log call, so main holds it and the
    // buffer is flushed when it drops at shutdown.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(log_writer)
        .init();
    tracing::info!(config = %config_path.display(), "starting openrusty");

    // Compile all plugins before serving; a broken module is fatal at boot.
    // `from_config` is the single construction path, shared with the test
    // helpers and external embedders.
    let state = match state::from_config(cfg.clone(), config_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "plugin bootstrap failed");
            std::process::exit(1);
        }
    };
    // Start active health probing (no-op when no upstream enables it).
    active_probe::spawn(&state);

    // Ingress watch (optional, `[ingress] enabled = true`): cluster
    // snapshots are rendered and applied OFF the serve path via
    // `state::apply_runtime`; any failure only downgrades to the static
    // config. The spawned loops park in their watch streams and are
    // reclaimed by the runtime at process exit - no shutdown bookkeeping.
    if cfg.ingress.enabled {
        let st = state.clone();
        let ingress_cfg = cfg.ingress.clone();
        let static_routes = cfg.routes.clone();
        tokio::spawn(ingress::run(st, ingress_cfg, static_routes));
    }

    // KV sweeper: expire stale plugin KV entries.
    {
        let st = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let now = host_state::now_ms();
                for plugin in &st.registry.snapshot().plugins {
                    plugin.state.sweep(now);
                }
            }
        });
    }

    // SIGHUP: hot reload (config + plugins), never disrupts serving.
    {
        let st = state.clone();
        tokio::spawn(async move {
            let Ok(mut sig) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            else {
                tracing::warn!("could not install SIGHUP handler");
                return;
            };
            while sig.recv().await.is_some() {
                tracing::info!("SIGHUP received; reloading");
                let st = st.clone();
                tokio::spawn(async move {
                    match reload::reload(&st).await {
                        Ok(r) => tracing::info!(
                            generation = r.generation,
                            plugins = ?r.plugins,
                            "reload complete"
                        ),
                        Err(e) => tracing::error!(error = %e, "reload failed"),
                    }
                });
            }
        });
    }

    // SIGUSR1: reopen `server.log_file` after external rotation renamed it
    // (nginx semantics). The handler is installed in BOTH logging modes:
    // with the stdout sink an innocent `systemctl kill -s USR1` must not
    // fall through to the default disposition (terminate) and kill the
    // gateway. Without a log file there is nothing to reopen, so the recv
    // loop is a debug-level no-op. A failed reopen keeps the old
    // descriptor and is logged, never fatal.
    tokio::spawn(async move {
        let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
        else {
            tracing::warn!("could not install SIGUSR1 handler");
            return;
        };
        match reopen_handle {
            Some(writer) => {
                while sig.recv().await.is_some() {
                    tracing::info!("SIGUSR1 received; reopening log file");
                    if let Err(e) = logging::reopen(&writer) {
                        tracing::error!(error = %e, "log reopen failed; continuing on old file");
                    }
                }
            }
            None => {
                tracing::debug!("SIGUSR1 handler active; logging to stdout, no file to reopen");
                while sig.recv().await.is_some() {
                    tracing::debug!("USR1 ignored: logging to stdout, no file to reopen");
                }
            }
        }
    });

    // Role-based multi-listener: `[[server.listeners]]` entries are authoritative
    // when written, otherwise a single inbound listener is derived from
    // `server.listen` (which keeps the historical single-socket shape).
    if !cfg.server.listeners.is_empty() {
        tracing::warn!(
            listen = %cfg.server.listen,
            "server.listen is ignored: [[server.listeners]] entries are authoritative"
        );
    }
    let listeners_cfg = openrusty_core::effective_listeners(&cfg);
    // Bind everything up front (fail-fast) and keep the accept-task handles:
    // the shutdown sequence waits on exactly these.
    let tasks = match listeners::spawn(listeners::mounts(&state, &listeners_cfg), &state.shutdown)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "listener failed");
            std::process::exit(1);
        }
    };

    // Shutdown trigger: SIGTERM, SIGINT and POST /openrusty/shutdown are
    // three doors into the same room - all of them are answered by the one
    // three-phase sequence below (stop accepting, bounded drain, summary).
    // SIGQUIT is the nginx fast-shutdown door: same stop-accepting phase,
    // drain skipped. `fast` picks between the two entry points; everything
    // after the branch (report handling, exit code) is shared.
    let mut fast = false;
    tokio::select! {
        _ = wait_signal(SignalKind::terminate()) => {
            tracing::info!("SIGTERM received; shutting down");
        }
        _ = wait_signal(SignalKind::interrupt()) => {
            tracing::info!("SIGINT received; shutting down");
        }
        _ = wait_signal(SignalKind::quit()) => {
            tracing::info!("SIGQUIT received; fast shutdown (skipping drain)");
            fast = true;
        }
        _ = shutdown::wait_for_shutdown(state.shutdown.rx.clone()) => {
            tracing::info!("shutdown endpoint triggered; shutting down");
        }
    }

    let report = if fast {
        shutdown::run_fast(
            state.shutdown.tx.clone(),
            tasks,
            state.shutdown.in_flight.clone(),
        )
        .await
    } else {
        let grace = Duration::from_millis(cfg.server.shutdown_grace_ms);
        shutdown::run(state.shutdown.tx.clone(), tasks, state.shutdown.in_flight.clone(), grace)
            .await
    };
    if report.task_errors > 0 {
        tracing::error!(errors = report.task_errors, "accept tasks failed during drain");
        // process::exit skips destructors; drop the guard first so the
        // error line reaches the sink before the process goes away.
        drop(log_guard);
        std::process::exit(1);
    }
    // Exit 0 in both outcomes; an expired grace is already logged as a warn
    // with the force-closed connection count.
    // Dropping the guard drains the non-blocking log buffer: the final
    // flush of the shutdown summary happens here, at the end of main.
    drop(log_guard);
}

/// Resolves on the given unix signal. If the handler cannot be installed
/// (extremely constrained environments), log and park forever so the admin
/// endpoint trigger keeps working.
async fn wait_signal(kind: SignalKind) {
    match tokio::signal::unix::signal(kind) {
        Ok(mut sig) => {
            sig.recv().await;
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not install signal handler");
            std::future::pending::<()>().await;
        }
    }
}
