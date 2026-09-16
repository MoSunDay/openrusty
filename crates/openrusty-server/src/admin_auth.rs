//! Management-plane token guard for `/openrusty/*`.
//!
//! Opt-in via `[admin] token`: when set, every admin endpoint except the
//! `ready`/`live` probes requires the shared secret as `Authorization:
//! Bearer <token>` or `X-OpenRusty-Token: <token>`. Absent token = open
//! (loopback/listener-role isolation stays the primary control).

use crate::state::AppState;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use std::sync::Arc;

/// Probes that never require the token: load balancers and k8s pull them
/// without credentials, and a wrong secret must not evict a live pod.
const ALWAYS_OPEN: [&str; 2] = ["/openrusty/ready", "/openrusty/live"];

/// Middleware body: pure predicates first (open path, absent secret),
/// constant-time compare last; a mismatch short-circuits with 401 before
/// the routed handler runs.
pub(crate) async fn admin_auth(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    if is_always_open(req.uri().path()) {
        return next.run(req).await;
    }
    let expected = state.admin_token.load();
    let Some(secret) = expected.as_deref() else {
        return next.run(req).await;
    };
    match presented_token(req.headers()).map(|presented| token_eq(presented, secret)) {
        Some(true) => next.run(req).await,
        _ => crate::pipeline::text_response(401, "401 unauthorized\n"),
    }
}

/// Paths exempt from the token (LB/k8s probes); exact match only.
fn is_always_open(path: &str) -> bool {
    ALWAYS_OPEN.contains(&path)
}

/// The credential a client presented, if any: `Authorization` with a
/// case-insensitive `Bearer ` scheme prefix, else the dedicated
/// `X-OpenRusty-Token` header; both trimmed.
fn presented_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        let rest = auth
            .strip_prefix("Bearer ")
            .or_else(|| auth.strip_prefix("bearer "))
            .map(str::trim);
        if let Some(token) = rest.filter(|t| !t.is_empty()) {
            return Some(token);
        }
    }
    headers
        .get("x-openrusty-token")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// Constant-time equality: length check first, then an XOR-fold over the
/// bytes so no early exit leaks a matching-prefix oracle.
fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::router;
    use crate::testutil::{boot_state, TmpDir};
    use axum::extract::ConnectInfo;
    use std::net::SocketAddr;
    use tower::ServiceExt;

    fn request(
        method: &str,
        uri: &str,
        remote: &str,
        bearer: Option<&str>,
        token_header: Option<&str>,
    ) -> axum::extract::Request {
        let mut builder = hyper::Request::builder()
            .method(method)
            .uri(uri)
            .extension(ConnectInfo::<SocketAddr>(remote.parse().unwrap()));
        if let Some(token) = bearer {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        if let Some(token) = token_header {
            builder = builder.header("x-openrusty-token", token);
        }
        builder.body(axum::body::Body::empty()).unwrap()
    }

    #[test]
    fn open_paths_are_exactly_the_probes() {
        assert!(is_always_open("/openrusty/ready"));
        assert!(is_always_open("/openrusty/live"));
        assert!(!is_always_open("/openrusty/status"));
        assert!(!is_always_open("/openrusty/readyz"));
        assert!(!is_always_open("/openrusty/ready/extra"));
        assert!(!is_always_open("/openrusty"));
    }

    #[test]
    fn presented_token_reads_both_headers() {
        let mut headers = axum::http::HeaderMap::new();
        assert_eq!(presented_token(&headers), None);
        headers.insert("authorization", "Bearer abc".parse().unwrap());
        assert_eq!(presented_token(&headers), Some("abc"));
        headers.insert("authorization", "bearer  spaced  ".parse().unwrap());
        assert_eq!(presented_token(&headers), Some("spaced"));
        headers.insert("authorization", "Basic zzz".parse().unwrap());
        assert_eq!(presented_token(&headers), None);
        headers.remove("authorization");
        headers.insert("x-openrusty-token", "  tok  ".parse().unwrap());
        assert_eq!(presented_token(&headers), Some("tok"));
    }

    #[test]
    fn token_eq_is_exact() {
        assert!(token_eq("s3cr3t", "s3cr3t"));
        assert!(!token_eq("s3cr3t", "s3cr3u"));
        assert!(!token_eq("s3cr3t", "s3cr3"));
        assert!(!token_eq("s3cr3t", "s3cr3t0"));
        assert!(!token_eq("", "x"));
        assert!(token_eq("", ""));
    }

    #[tokio::test]
    async fn without_admin_section_the_plane_stays_open() {
        let dir = TmpDir::new("adminauth-off");
        dir.write_config(&dir.standard_config());
        let state = boot_state(&dir);
        let svc = router(state).into_service::<axum::body::Body>();
        let resp = svc
            .oneshot(request("GET", "/openrusty/status", "8.8.8.8:1", None, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn token_gate_on_status() {
        let dir = TmpDir::new("adminauth-status");
        dir.write_config(&format!(
            "{}\n[admin]\ntoken = \"s3cr3t\"\n",
            dir.standard_config()
        ));
        let state = boot_state(&dir);
        let svc = router(state).into_service::<axum::body::Body>();

        let resp = svc
            .clone()
            .oneshot(request("GET", "/openrusty/status", "8.8.8.8:1", None, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        let resp = svc
            .clone()
            .oneshot(request(
                "GET",
                "/openrusty/status",
                "8.8.8.8:1",
                Some("wrong"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        let resp = svc
            .clone()
            .oneshot(request(
                "GET",
                "/openrusty/status",
                "8.8.8.8:1",
                Some("s3cr3t"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let resp = svc
            .oneshot(request(
                "GET",
                "/openrusty/status",
                "8.8.8.8:1",
                None,
                Some("s3cr3t"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn probes_stay_open_with_token_set() {
        let dir = TmpDir::new("adminauth-probes");
        dir.write_config(&format!(
            "{}\n[admin]\ntoken = \"s3cr3t\"\n",
            dir.standard_config()
        ));
        let state = boot_state(&dir);
        let svc = router(state).into_service::<axum::body::Body>();
        for probe in ["/openrusty/ready", "/openrusty/live"] {
            let resp = svc
                .clone()
                .oneshot(request("GET", probe, "8.8.8.8:1", None, None))
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "{probe} must stay open");
        }
    }

    #[tokio::test]
    async fn reload_token_and_loopback_stack() {
        let dir = TmpDir::new("adminauth-reload");
        dir.write_config(&format!(
            "{}\n[admin]\ntoken = \"s3cr3t\"\n",
            dir.standard_config()
        ));
        let state = boot_state(&dir);
        let svc = router(state).into_service::<axum::body::Body>();

        // Loopback + correct token: the auth passes, the loopback rule too.
        let resp = svc
            .clone()
            .oneshot(request(
                "POST",
                "/openrusty/reload",
                "127.0.0.1:40003",
                Some("s3cr3t"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // Auth passes but the remote is not loopback: still forbidden.
        let resp = svc
            .oneshot(request(
                "POST",
                "/openrusty/reload",
                "8.8.8.8:1",
                Some("s3cr3t"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
    }
}
