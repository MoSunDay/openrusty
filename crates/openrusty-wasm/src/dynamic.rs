//! Stat-driven registry of single-module "dynamic" wasm programs served
//! under `POST /api/v1/dynamic/<name>`: each request synthesizes a
//! pipeline of exactly one plugin (post_read -> rewrite -> access ->
//! content -> header_filter -> log; no balancer/body_filter/proxy).
//!
//! Modules come straight from `[dynamic].dir` and are resolved per
//! request: the compile cache is keyed by the file's `(mtime_secs,
//! mtime_nanos, size)`, so replacing a `<name>.wasm` takes effect on the
//! next request with no reload. Compilation is singleflight per name and
//! runs off the async workers. Shared state (`HostState`: KV + error
//! counters) is keyed by name and survives both module replacement and
//! deletion, mirroring plugin reload semantics. The engine + linker are
//! shared with the caller's plugin registry, so the same epoch ticker
//! and import whitelist apply.

use crate::host_state::HostState;
use crate::instance::{HeaderEdit, HostData};
use crate::linker::validate_module;
use crate::registry::{LoadedPlugin, PluginSnapshot};
use crate::session::RequestSession;
use bytes::Bytes;
use openrusty_core::config::{DynamicConfig, FailPolicy};
use openrusty_core::phase::{Decision, Phase};
use openrusty_core::ReqCtx;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use wasmtime::{Engine, Linker, Module};

/// Longest module name accepted (cache key hygiene, nothing more).
const MAX_NAME_LEN: usize = 128;

/// Cache/accept name rule: `^[A-Za-z0-9][A-Za-z0-9._-]*$`, 1..=128 chars.
/// Hand-rolled (no regex dependency); rejects `..`, `/`, empty and
/// leading `.`/`_`/`-`.
pub fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Why a dynamic module could not be resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveError {
    /// The name failed [`valid_name`] (mapped to HTTP 400).
    InvalidName,
    /// No readable `<dir>/<name>.wasm` regular file (mapped to HTTP 404).
    NotFound,
}

/// Result of one dynamic invocation, mapped by the server into a response.
pub struct DynOutcome {
    /// HTTP status the server should answer with.
    pub status: u16,
    /// Response body written by the module (`resp_body_set`).
    pub body: Option<Bytes>,
    /// Response headers after header_filter (raw; the server adds a
    /// Content-Type default when absent and a body exists).
    pub headers: Vec<(String, String)>,
    /// Human-readable failure (bad name, missing file, compile/ABI error,
    /// or no response produced) for logging; `None` on the happy path.
    pub error: Option<String>,
}

/// Internal resolution failure: the public kinds plus a compile/ABI
/// failure (500, never cached).
enum ResolveFail {
    Resolve(ResolveError),
    Build(String),
}

/// One cache entry: the compiled plugin and the file version it was
/// built from.
struct CachedModule {
    version: (u64, u32, u64),
    plugin: Arc<LoadedPlugin>,
}

/// Stat-derived version key `(mtime_secs, mtime_nanos, size)`. `None`
/// when the path is missing, unreadable or not a regular file.
fn file_version(path: &Path) -> Option<(u64, u32, u64)> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() {
        return None;
    }
    let mtime = md.modified().ok()?;
    let dur = mtime.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some((dur.as_secs(), dur.subsec_nanos(), md.len()))
}

/// Read + compile + ABI-validate one module file. Pure apart from file IO;
/// the error string feeds the 500 response body/log.
fn compile_file(
    engine: &Engine,
    linker: &Linker<HostData>,
    path: &Path,
) -> Result<(Module, crate::linker::AbiInfo), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let module = Module::new(engine, bytes).map_err(|e| format!("compile failed: {e}"))?;
    let info = validate_module(engine, linker, &module)
        .map_err(|e| format!("abi validation failed: {e}"))?;
    Ok((module, info))
}

/// Lock a std mutex without propagating poisoning (a panicked compiler
/// thread must not take the gateway down).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Stat-driven registry of dynamic modules: resolves `<dir>/<name>.wasm`
/// per request, compiles + ABI-validates it once per file version, and
/// runs one synthesized single-plugin pipeline per request.
pub struct DynamicRegistry {
    engine: Engine,
    linker: Linker<HostData>,
    /// The `[dynamic]` section this registry was built from. Kept whole so
    /// the server can (a) diff it against a freshly loaded config on
    /// reload (rebuild only on change, `PartialEq`) and (b) read
    /// `max_body_bytes` as the POST body cap.
    cfg: DynamicConfig,
    dir: PathBuf,
    timeout: Duration,
    fail_policy: FailPolicy,
    memory_mb: u32,
    settings: BTreeMap<String, HashMap<String, String>>,
    /// Compiled modules keyed by name, validated per request against the
    /// file's current stat version.
    cache: Mutex<HashMap<String, Arc<CachedModule>>>,
    /// Per-name singleflight gates for compiles.
    gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// HostState per name, kept separately so KV and error counters
    /// survive module replacement and eviction alike.
    states: Mutex<HashMap<String, Arc<HostState>>>,
    /// Cumulative compile count (tests/metrics).
    compiled: AtomicUsize,
}

impl DynamicRegistry {
    /// Shares the plugin registry's engine + linker (same epoch ticker,
    /// same import whitelist). Compile/validate happens lazily per name.
    pub fn new(engine: Engine, linker: Linker<HostData>, cfg: &DynamicConfig) -> Arc<Self> {
        Arc::new(Self {
            engine,
            linker,
            cfg: cfg.clone(),
            dir: PathBuf::from(&cfg.dir),
            timeout: Duration::from_millis(cfg.timeout_ms),
            fail_policy: cfg.on_failure,
            memory_mb: cfg.max_memory_mb,
            settings: cfg.settings.clone(),
            cache: Mutex::new(HashMap::new()),
            gates: Mutex::new(HashMap::new()),
            states: Mutex::new(HashMap::new()),
            compiled: AtomicUsize::new(0),
        })
    }

    /// Cumulative compile attempts: every cache miss that ran the
    /// compiler (successful or not - failures are never cached, so they
    /// are attempted again).
    pub fn compiled_count(&self) -> usize {
        self.compiled.load(Ordering::Relaxed)
    }

    /// The `[dynamic]` config this registry was built from: the server
    /// compares it against a freshly loaded config on reload (rebuild
    /// only when the section changed) and reads `max_body_bytes` as the
    /// POST body cap.
    pub fn config(&self) -> &DynamicConfig {
        &self.cfg
    }

    /// Module directory (`[dynamic].dir`): where the registration face
    /// (`PUT /openrusty/dynamic/{name}`) lands `<name>.wasm` artifacts.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Upload-time artifact check: compile + ABI-validate raw module
    /// bytes with the shared engine/linker. Pure (no cache writes), so
    /// the registration face can reject a bad artifact BEFORE it lands
    /// in `dir` - a broken file would otherwise compile-fail per
    /// request until replaced. Accepts wasm binaries and WAT text
    /// (wasmtime compiles either).
    pub fn validate_bytes(&self, bytes: &[u8]) -> Result<(), String> {
        let module =
            Module::new(&self.engine, bytes).map_err(|e| format!("compile failed: {e}"))?;
        validate_module(&self.engine, &self.linker, &module)
            .map_err(|e| format!("abi validation failed: {e}"))?;
        Ok(())
    }

    /// Run one request through the synthesized single-plugin pipeline.
    /// `ctx` should carry method POST and the dynamic path; `body` is the
    /// buffered request body (already capped by the caller).
    pub async fn invoke(&self, name: &str, ctx: ReqCtx, body: Bytes) -> DynOutcome {
        let plugin = match self.resolve(name).await {
            Ok(plugin) => plugin,
            Err(f) => return failed(f),
        };
        let snap = Arc::new(PluginSnapshot {
            generation: 0,
            plugins: vec![plugin],
        });
        let mut sess = RequestSession::for_parts(
            self.engine.clone(),
            self.linker.clone(),
            snap,
            ctx,
            Vec::new(),
        );
        sess.set_req_body(body);

        // Pre-proxy phases in nginx order; a terminal decision fixes the
        // status, otherwise the module produced no response at all.
        let mut terminal: Option<u16> = None;
        for phase in [
            Phase::PostRead,
            Phase::Rewrite,
            Phase::Access,
            Phase::Content,
        ] {
            match sess.run_phase(phase) {
                Decision::Deny(status) => {
                    terminal = Some(status);
                    break;
                }
                Decision::Done => {
                    terminal = Some(if sess.resp_body().is_some_and(|b| !b.is_empty()) {
                        200
                    } else {
                        204
                    });
                    break;
                }
                _ => {}
            }
        }
        let (status, error) = match terminal {
            Some(status) => (status, None),
            None => (500, Some("dynamic module produced no response".to_string())),
        };

        // Post-response phases: seed the header list with whatever the
        // module already set (idempotent - run_phase round-trips them),
        // let header_filter adjust, then log (log never aborts; header
        // decisions cannot retroactively change the status).
        sess.set_resp_headers(sess.resp_headers().to_vec());
        sess.run_phase(Phase::HeaderFilter);
        sess.run_phase(Phase::Log);

        // A body written after the status was fixed (header_filter or
        // log) upgrades the would-be empty 204 to a 200, mirroring the
        // pipeline's `done_response` (built after log): hyper would
        // otherwise drop the body of a 204 and the client would see an
        // empty response.
        let status = if status == 204 && sess.resp_body().is_some_and(|b| !b.is_empty()) {
            200
        } else {
            status
        };

        DynOutcome {
            status,
            body: sess.take_resp_body(),
            headers: final_headers(&mut sess),
            error,
        }
    }

    /// Resolve a name to a compiled plugin: stat the file, serve a cache
    /// hit when the version matches, otherwise singleflight a compile.
    async fn resolve(&self, name: &str) -> Result<Arc<LoadedPlugin>, ResolveFail> {
        if !valid_name(name) {
            return Err(ResolveFail::Resolve(ResolveError::InvalidName));
        }
        let path = self.dir.join(format!("{name}.wasm"));
        let Some(version) = file_version(&path) else {
            if lock(&self.cache).remove(name).is_some() {
                tracing::warn!(plugin = %name, "dynamic module gone; cache entry evicted");
            }
            return Err(ResolveFail::Resolve(ResolveError::NotFound));
        };
        if let Some(hit) = lock(&self.cache).get(name) {
            if hit.version == version {
                tracing::debug!(plugin = %name, "dynamic cache hit");
                return Ok(Arc::clone(&hit.plugin));
            }
        }
        // Singleflight: one compiler per name; late arrivals reuse its
        // result after re-checking the cache under the gate.
        let gate = Arc::clone(lock(&self.gates).entry(name.to_string()).or_default());
        let _guard = gate.lock().await;
        if let Some(hit) = lock(&self.cache).get(name) {
            if hit.version == version {
                return Ok(Arc::clone(&hit.plugin));
            }
        }
        let engine = self.engine.clone();
        let linker = self.linker.clone();
        let work_path = path.clone();
        let work = move || compile_file(&engine, &linker, &work_path);
        // Compile off the async workers; fall back to inline when there
        // is no tokio runtime (e.g. sync callers in tests).
        let built = if tokio::runtime::Handle::try_current().is_ok() {
            match tokio::task::spawn_blocking(work).await {
                Ok(inner) => inner,
                Err(e) => Err(format!("compiler task failed: {e}")),
            }
        } else {
            work()
        };
        self.compiled.fetch_add(1, Ordering::Relaxed);
        let (module, info) = built.map_err(|e| {
            tracing::warn!(plugin = %name, error = %e, "dynamic module rejected");
            ResolveFail::Build(e)
        })?;

        // Same name => the old HostState carries over (KV + error
        // counters survive module replacement).
        let state = Arc::clone(
            lock(&self.states)
                .entry(name.to_string())
                .or_insert_with(|| Arc::new(HostState::new(name.to_string()))),
        );
        let settings = Arc::new(self.settings.get(name).cloned().unwrap_or_default());
        let plugin = Arc::new(LoadedPlugin {
            name: name.to_string(),
            module,
            state,
            settings,
            timeout: self.timeout,
            fail_policy: self.fail_policy,
            memory_mb: self.memory_mb,
            has_dealloc: info.has_dealloc,
        });
        lock(&self.cache).insert(
            name.to_string(),
            Arc::new(CachedModule {
                version,
                plugin: Arc::clone(&plugin),
            }),
        );
        tracing::debug!(plugin = %name, "dynamic module compiled");
        Ok(plugin)
    }
}

/// Apply drained header edits on top of the session's header list
/// (Set replaces in place / appends, Del removes; case-insensitive).
fn final_headers(sess: &mut RequestSession) -> Vec<(String, String)> {
    let mut headers = sess.resp_headers().to_vec();
    for edit in sess.take_header_edits() {
        match edit {
            HeaderEdit::Set(name, val) => {
                match headers
                    .iter_mut()
                    .find(|(n, _)| n.eq_ignore_ascii_case(&name))
                {
                    Some(entry) => entry.1 = val,
                    None => headers.push((name, val)),
                }
            }
            HeaderEdit::Del(name) => headers.retain(|(n, _)| !n.eq_ignore_ascii_case(&name)),
        }
    }
    headers
}

/// Map a resolution failure to its client-facing outcome.
fn failed(f: ResolveFail) -> DynOutcome {
    let (status, error) = match f {
        ResolveFail::Resolve(ResolveError::InvalidName) => {
            (400, "invalid dynamic module name".to_string())
        }
        ResolveFail::Resolve(ResolveError::NotFound) => {
            (404, "dynamic module not found".to_string())
        }
        ResolveFail::Build(detail) => (500, detail),
    };
    DynOutcome {
        status,
        body: None,
        headers: Vec::new(),
        error: Some(error),
    }
}

#[cfg(test)]
#[path = "dynamic_tests.rs"]
mod tests;
