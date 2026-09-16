//! Registration face for the dynamic execution API, mounted on the admin
//! plane (`/openrusty/dynamic*`) so the `[admin]` token guard applies
//! like every other management endpoint:
//!
//! - `PUT /openrusty/dynamic/{name}` - body is the module artifact
//!   (wasm binary or WAT text), validated (compile + ABI) before it is
//!   atomically written to `<dynamic.dir>/<name>.wasm`; the stat-driven
//!   cache serves it on the very next request, no reload. Optional
//!   query `method` + `path` binds `{method, path} -> name` at the same
//!   time. An EMPTY body with `method`+`path` binds an already-present
//!   module (file-delivered modules get bindings too).
//! - `DELETE /openrusty/dynamic/{name}` - removes the artifact and every
//!   binding pointing at it; 404 when neither existed.
//! - `GET /openrusty/dynamic` - lists modules on disk and live bindings.
//!
//! These handlers never touch the compile cache: writes land through a
//! temp file + rename (readers see old or new, never partial), and
//! `DELETE` relies on the resolver's missing-file eviction.

use crate::pipeline::text_response;
use crate::state::{self, AppState};
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use openrusty_wasm::valid_name;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Upload cap for one module artifact (413 above): wasm modules stay in
/// the kilobytes-to-low-megabytes range; anything bigger is a mistake.
const MAX_MODULE_BYTES: usize = 64 * 1024 * 1024;

/// Temp-name counter for atomic module writes.
static UPLOAD_SEQ: AtomicU64 = AtomicU64::new(0);

/// Registration routes, unmounted building block. Merged into the admin
/// router by `app::admin_routes` when `[dynamic]` is enabled at boot
/// (same mounting asymmetry as the execution API's own routes).
pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/openrusty/dynamic", get(list)).route(
        "/openrusty/dynamic/{name}",
        put(register).delete(unregister),
    )
}

/// `PUT /openrusty/dynamic/{name}[?method=M&path=P]`.
async fn register(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    req: axum::extract::Request,
) -> Response {
    // Defensive: the routes are only mounted while `[dynamic]` is
    // enabled; a reload that disabled the section swaps in `None`.
    let Some(registry) = state.dynamic.load_full().as_ref().clone() else {
        return text_response(404, "404 not found\n");
    };
    if !valid_name(&name) {
        return text_response(400, "400 invalid dynamic module name\n");
    }
    let bind = match (params.get("method"), params.get("path")) {
        (Some(m), Some(p)) => Some((m.as_str(), p.as_str())),
        (None, None) => None,
        _ => {
            return text_response(
                400,
                "400 bind needs both 'method' and 'path' query params\n",
            )
        }
    };

    let body = match axum::body::to_bytes(req.into_body(), MAX_MODULE_BYTES).await {
        Ok(b) => b,
        Err(_) => return text_response(413, "413 payload too large\n"),
    };

    // Bind-only form: empty body + binding params against an existing
    // artifact (file-delivered modules get routes without re-uploading).
    if body.is_empty() {
        let Some((method, path)) = bind else {
            return text_response(
                400,
                "400 empty body: send the module bytes, or bind an existing \
                 module with 'method' and 'path'\n",
            );
        };
        if !registry.dir().join(format!("{name}.wasm")).is_file() {
            return text_response(404, "404 dynamic module not found\n");
        }
        return match state::bind_dynamic_route(&state, method, path, &name) {
            Ok(()) => bound_response(&name, method, path),
            Err(e) => text_response(400, format!("400 {e}\n")),
        };
    }

    // Validate BEFORE the artifact lands: a broken file would otherwise
    // compile-fail per request until replaced (failures are never
    // cached). Compilation runs off the async workers.
    let size = body.len();
    let validator = Arc::clone(&registry);
    // `Bytes::clone` is a refcount bump: the task validates its own
    // handle while the original is stored right after success.
    let probe = body.clone();
    let validated = tokio::task::spawn_blocking(move || validator.validate_bytes(&probe))
        .await
        .unwrap_or_else(|e| Err(format!("validator task failed: {e}")));
    if let Err(detail) = validated {
        tracing::warn!(module = %name, error = %detail, "dynamic module upload rejected");
        return text_response(400, format!("400 {detail}\n"));
    }
    if let Err(detail) = store_module(registry.dir(), &name, &body) {
        tracing::warn!(module = %name, error = %detail, "dynamic module store failed");
        return text_response(500, format!("500 {detail}\n"));
    }
    tracing::info!(module = %name, bytes = size, "dynamic module stored");

    let bound = bind.map(|(m, p)| {
        state::bind_dynamic_route(&state, m, p, &name)
            .map_err(|e| format!("400 {e}\n"))
            .map(|_| bound_response(&name, m, p))
    });
    match bound {
        Some(Err(detail)) => text_response(400, detail),
        Some(Ok(resp)) => resp,
        None => Json(json!({"status": "ok", "module": name, "bytes": size})).into_response(),
    }
}

/// `DELETE /openrusty/dynamic/{name}`: drop the artifact (if present)
/// and every binding pointing at it; the resolver evicts its cache entry
/// on the next request's stat.
async fn unregister(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let Some(registry) = state.dynamic.load_full().as_ref().clone() else {
        return text_response(404, "404 not found\n");
    };
    if !valid_name(&name) {
        return text_response(400, "400 invalid dynamic module name\n");
    }
    let path = registry.dir().join(format!("{name}.wasm"));
    let deleted = match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return text_response(500, format!("500 remove {}: {e}\n", path.display())),
    };
    let unbound = state::unbind_dynamic_module(&state, &name);
    if !deleted && unbound == 0 {
        return text_response(404, "404 dynamic module not found\n");
    }
    tracing::info!(module = %name, deleted, unbound, "dynamic module removed");
    Json(json!({"status": "ok", "module": name, "deleted": deleted, "unbound": unbound}))
        .into_response()
}

/// `GET /openrusty/dynamic`: modules on disk plus live bindings.
async fn list(State(state): State<Arc<AppState>>) -> Response {
    let Some(registry) = state.dynamic.load_full().as_ref().clone() else {
        return text_response(404, "404 not found\n");
    };
    let mut modules: Vec<String> = match std::fs::read_dir(registry.dir()) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|f| f.strip_suffix(".wasm").map(str::to_string))
            .filter(|stem| valid_name(stem))
            .collect(),
        Err(_) => Vec::new(),
    };
    modules.sort();
    let routes: Vec<serde_json::Value> = state
        .dynamic_routes
        .load()
        .entries()
        .into_iter()
        .map(|r| json!({"method": r.method, "path": r.path, "module": r.module}))
        .collect();
    Json(json!({
        "dir": registry.dir().display().to_string(),
        "modules": modules,
        "routes": routes,
    }))
    .into_response()
}

/// Success body for a (re)binding, method echoed normalized.
fn bound_response(name: &str, method: &str, path: &str) -> Response {
    let method = method.trim().to_ascii_uppercase();
    Json(json!({
        "status": "ok",
        "module": name,
        "bound": {"method": method, "path": path},
    }))
    .into_response()
}

/// Atomic artifact store: write `<name>.wasm.put-<pid>-<seq>` then
/// rename over `<name>.wasm`. The temp name deliberately does NOT end
/// in `.wasm`, so it is invisible to name resolution while being
/// written, and the rename is atomic: readers see the old or the new
/// module, never a partial file.
fn store_module(dir: &std::path::Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let final_path = dir.join(format!("{name}.wasm"));
    let tmp = dir.join(format!(
        "{name}.wasm.put-{}-{}",
        std::process::id(),
        UPLOAD_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let io = || -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &final_path)?;
        Ok(())
    };
    io().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("store {}: {e}", final_path.display())
    })
}

#[cfg(test)]
#[path = "dynamic_admin_tests.rs"]
mod tests;
