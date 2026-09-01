//! Shared test helpers: scratch directories, config generation, a fully
//! wired `AppState` without any real listener, plus fixture paths and a
//! one-shot echo upstream for proxy drills.

use crate::state::{self, AppState};
use openrusty_core::load_config;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Path to a committed fixture under `tests/fixtures/`.
pub fn fixture_path(name: &str) -> String {
    format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name)
}

/// Bind-then-release port reservation, matching `listeners.rs` tests.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One-shot HTTP/1.1 upstream: reads the request head, answers with a
/// fixed 200 "hello" and closes; enough for one proxied GET per test.
pub async fn spawn_echo_upstream() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = head; // request head content is irrelevant here
                let resp = "HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nhello";
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    port
}

static SEQ: AtomicU32 = AtomicU32::new(0);

/// Minimal ABI-valid plugin: declines every phase.
pub const OK_WAT: &str = r#"(module
    (func (export "orr_on_phase") (param i32 i32) (result i32) i32.const -5)
    (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
    (memory (export "memory") 1))"#;

/// Scratch directory removed on drop. Layout:
///   <dir>/plugins/      plugin dir referenced by the generated config
///   <dir>/openrusty.toml
pub struct TmpDir(pub PathBuf);

impl TmpDir {
    pub fn new(tag: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let p =
            std::env::temp_dir().join(format!("openrusty-srv-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(p.join("plugins")).unwrap();
        TmpDir(p)
    }

    pub fn plugins_dir(&self) -> PathBuf {
        self.0.join("plugins")
    }

    pub fn config_path(&self) -> PathBuf {
        self.0.join("openrusty.toml")
    }

    pub fn write_plugin(&self, name: &str, body: &[u8]) {
        fs::write(self.plugins_dir().join(name), body).unwrap();
    }

    pub fn write_config(&self, body: &str) {
        fs::write(self.config_path(), body).unwrap();
    }

    /// Standard test config: one upstream `u` with one peer, one catch-all
    /// route, plugins loaded from this scratch dir.
    pub fn standard_config(&self) -> String {
        format!(
            r#"
[server]
listen = "127.0.0.1:18080"

[plugins]
dir = "{}"

[[upstreams]]
name = "u"
  [[upstreams.peers]]
  addr = "127.0.0.1:9001"

[[routes]]
path_prefix = "/"
upstream = "u"
"#,
            self.plugins_dir().display()
        )
    }
    /// Same as [`standard_config`] but the single route only matches the
    /// `/api` prefix, so unmatched paths exercise the 404 branch.
    pub fn prefix_only_config(&self) -> String {
        self.standard_config()
            .replace("path_prefix = \"/\"", "path_prefix = \"/api\"")
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Build an `AppState` from the scratch dir's config: bootstrap the plugin
/// registry and publish the first runtime snapshot. Delegates to
/// [`crate::state::from_config`], the single construction path shared with
/// the binary and external embedders.
pub fn boot_state(dir: &TmpDir) -> Arc<AppState> {
    let cfg = load_config(&dir.config_path()).unwrap();
    state::from_config(cfg, dir.config_path()).unwrap()
}
