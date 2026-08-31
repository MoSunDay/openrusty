//! openrusty: an nginx-like WASM-extensible gateway.
//!
//! HTTP/1.1 + h2c on one port, plugin phases via wasmtime, proxying with
//! retries, WebSocket pass-through, SSE streaming, atomic hot reload.
//!
//! Thin binary glue only: the gateway itself lives in the lib target
//! (`openrusty_server`), so external embedders (init-pro) reuse the same
//! compiled modules and serve the gateway in-process.

use openrusty_core::load_config;
use openrusty_server::{active_probe, listeners, reload, state};
use openrusty_wasm::host_state;
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

fn resolve_config_path() -> PathBuf {
    std::env::args()
        .nth(1)
        .or_else(|| std::env::var("OPENRUSTY_CONFIG").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config/openrusty.toml"))
}

#[tokio::main]
async fn main() {
    let config_path = resolve_config_path();
    if !config_path.is_file() {
        eprintln!(
            "openrusty: config file not found: {}\nusage: openrusty [CONFIG] (or set OPENRUSTY_CONFIG)",
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
    // Non-blocking writer: log calls hand formatted output to a dedicated
    // thread; the guard must outlive every log call, so main holds it and the
    // buffer is flushed when it drops at shutdown.
    let (log_writer, _log_guard) = tracing_appender::non_blocking(std::io::stdout());
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

    let (shutdown_tx, rx) = tokio::sync::broadcast::channel::<()>(1);

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

    // Ctrl-C: broadcast shutdown and let connections drain.
    {
        let tx = shutdown_tx.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("ctrl-c received; shutting down");
                let _ = tx.send(());
            }
        });
    }

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
    if let Err(e) = listeners::serve(listeners::mounts(&state, &listeners_cfg), rx).await {
        tracing::error!(error = %e, "listener failed");
        std::process::exit(1);
    }
    drop(shutdown_tx);
}
