//! Request forwarding to a chosen peer, plus tunneling for upgrades.
//!
//! [`forward`] sends one buffered request through a pooled client and
//! classifies transport failures so the retry loop can decide whether the
//! next peer is worth trying ([`ForwardError::is_retryable`]).
//! [`tunnel`] shovels bytes both ways for upgraded connections (WebSocket).

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{HeaderName, HeaderValue, HOST};

use crate::client::{get, get_tls, ClientPool, HttpClient};
use crate::tls::HttpsClient;
use crate::upstream::{is_hop_by_hop, Peer, Upstream};

/// `X-Forwarded-For` is not in http's standard header constants set.
const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

/// One buffered request ready to be sent upstream.
#[derive(Debug, Clone)]
pub struct ForwardRequest {
    /// HTTP method.
    pub method: hyper::Method,
    /// Path and query, e.g. `"/v1/chat?task=x"`.
    pub path_and_query: String,
    /// Request headers. Hop-by-hop headers are dropped defensively here
    /// even if the caller already stripped them.
    pub headers: Vec<(String, String)>,
    /// Fully buffered request body.
    pub body: Bytes,
    /// Client IP appended to `X-Forwarded-For`.
    pub client_ip: String,
}

/// Transport failure while talking to one peer.
#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    /// Could not establish the connection to the peer.
    #[error("connect error: {0}")]
    Connect(String),
    /// Connection came up but the request could not be fully sent.
    #[error("send error: {0}")]
    Send(String),
    /// The response could not be received/decoded. Not retryable: the peer
    /// already saw the request, so replaying it may duplicate side effects.
    #[error("response error: {0}")]
    Response(String),
}

impl ForwardError {
    /// True when retrying on another peer is safe (no response was seen).
    pub fn is_retryable(&self) -> bool {
        matches!(self, ForwardError::Connect(_) | ForwardError::Send(_))
    }
}

/// Failure classes the retry gate reasons about. The distinction that
/// matters is whether the request (possibly partially) reached the peer:
/// replaying such a request can duplicate side effects (nginx:
/// `proxy_next_upstream non_idempotent` semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The connection never came up: nothing was sent.
    Connect,
    /// The connection was up, so the request may have been (partially) sent.
    Sent,
    /// The peer answered, but the response was unusable.
    Response,
    /// The route timeout fired while the attempt was in flight; the
    /// request may have been sent and may even be processed right now.
    Timeout,
}

/// Classify a transport failure for the retry gate.
pub fn failure_kind(err: &ForwardError) -> FailureKind {
    match err {
        ForwardError::Connect(_) => FailureKind::Connect,
        ForwardError::Send(_) => FailureKind::Sent,
        ForwardError::Response(_) => FailureKind::Response,
    }
}

/// True when replaying `method` on another peer cannot cause side effects
/// beyond the peer's own response (RFC 9110 9.2.2). Everything not on the
/// safe list is treated as non-idempotent.
pub fn is_idempotent(method: &hyper::Method) -> bool {
    matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS" | "TRACE")
}

/// nginx-style retry gate for one failed attempt.
///
/// Idempotent requests may be replayed after any failure that produced no
/// usable response (connect/send failures and route timeouts; a decoded
/// response is never replayed). Non-idempotent requests may only be
/// replayed when the connection never came up (`Connect`): every other
/// failure class implies the peer saw (part of) the request.
pub fn may_retry(method: &hyper::Method, kind: FailureKind) -> bool {
    if is_idempotent(method) {
        matches!(
            kind,
            FailureKind::Connect | FailureKind::Sent | FailureKind::Timeout
        )
    } else {
        matches!(kind, FailureKind::Connect)
    }
}

/// Flatten an error chain into one message for logs/telemetry.
fn describe(mut err: &dyn std::error::Error) -> String {
    let mut message = err.to_string();
    while let Some(source) = err.source() {
        message.push_str(": ");
        message.push_str(&source.to_string());
        err = source;
    }
    message
}

/// Map a hyper-util legacy client error onto [`ForwardError`].
///
/// `is_connect()` is the only public kind predicate, so send-phase failures
/// are detected via the `SendRequest` kind name in the Debug output.
fn classify(err: hyper_util::client::legacy::Error) -> ForwardError {
    if err.is_connect() {
        return ForwardError::Connect(describe(&err));
    }
    if format!("{err:?}").contains("SendRequest") {
        return ForwardError::Send(describe(&err));
    }
    ForwardError::Response(describe(&err))
}

/// Merge any incoming `X-Forwarded-For` values with the direct client IP.
///
/// Shared with the WebSocket handshake builder so both paths append (never
/// overwrite) the client hop.
pub fn merge_xff(headers: &[(String, String)], client_ip: &str) -> String {
    let mut parts: Vec<&str> = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("x-forwarded-for"))
        .map(|(_, value)| value.as_str())
        .collect();
    parts.push(client_ip);
    parts.join(", ")
}

/// Build the outbound request shared by the plain and TLS forward paths.
///
/// Header policy: hop-by-hop headers and any `Host` header from the caller
/// are dropped; `Host` is set to `host` (nginx `proxy_pass` semantics: the
/// Host header does not change with the scheme, only the URI authority
/// does); `X-Forwarded-For` gets the client IP appended. Invalid header
/// names/values are skipped rather than failing the whole request.
/// `uri_authority` is the URI host (peer address for http, the TLS
/// server_name for https - the TLS connector ignores it, but the URI must
/// carry the right scheme for any future consumer).
fn build_outgoing(
    scheme: &str,
    uri_authority: &str,
    host: &str,
    req: &ForwardRequest,
) -> Result<hyper::Request<Full<Bytes>>, ForwardError> {
    let uri = format!("{scheme}://{uri_authority}{}", req.path_and_query);
    let mut builder = hyper::Request::builder()
        .method(req.method.clone())
        .uri(uri);
    let Some(headers) = builder.headers_mut() else {
        return Err(ForwardError::Send("invalid method or URI".to_string()));
    };

    for (name, value) in &req.headers {
        if is_hop_by_hop(name)
            || name.eq_ignore_ascii_case("host")
            || name.eq_ignore_ascii_case("x-forwarded-for")
        {
            continue;
        }
        let (Ok(header_name), Ok(header_value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            continue;
        };
        headers.append(header_name, header_value);
    }

    if let Ok(xff) = HeaderValue::from_str(&merge_xff(&req.headers, &req.client_ip)) {
        headers.append(X_FORWARDED_FOR, xff);
    }
    let host = HeaderValue::from_str(host).map_err(|e| ForwardError::Send(e.to_string()))?;
    headers.insert(HOST, host);

    builder
        .body(Full::new(req.body.clone()))
        .map_err(|e| ForwardError::Send(e.to_string()))
}

/// Send `req` to `peer` through a pooled plain-TCP client.
pub async fn forward(
    client: &HttpClient,
    peer: &Peer,
    req: &ForwardRequest,
) -> Result<hyper::Response<hyper::body::Incoming>, ForwardError> {
    let outgoing = build_outgoing("http", &peer.addr.to_string(), &peer.addr.to_string(), req)?;
    client.request(outgoing).await.map_err(classify)
}

/// Send `req` to `peer` through a pooled TLS client.
///
/// Same header policy as [`forward`]; the URI authority becomes the TLS
/// `server_name` (so SNI and the URI agree) while the `Host` header keeps
/// the nginx `proxy_pass` behaviour of the peer address. TCP connect and
/// TLS handshake failures surface as [`ForwardError::Connect`]
/// (retryable), exactly like the plaintext path.
pub async fn forward_https(
    client: &HttpsClient,
    peer: &Peer,
    server_name: &str,
    req: &ForwardRequest,
) -> Result<hyper::Response<hyper::body::Incoming>, ForwardError> {
    let outgoing = build_outgoing("https", server_name, &peer.addr.to_string(), req)?;
    client.request(outgoing).await.map_err(classify)
}

/// Forward one attempt to `peer` through the shared client pool, picking
/// the plaintext or the TLS client per the upstream's TLS plan. Both arms
/// resolve to the same result type, so callers keep one retry loop; the
/// pooled client is borrowed only inside this function, where its
/// temporary lives across the single `.await`.
pub async fn forward_peer(
    pool: &ClientPool,
    up: &Upstream,
    peer: &Peer,
    req: &ForwardRequest,
) -> Result<hyper::Response<hyper::body::Incoming>, ForwardError> {
    match &up.tls {
        Some(tls) => {
            forward_https(
                &get_tls(
                    pool,
                    peer.addr,
                    tls,
                    up.connect_timeout,
                    up.pool_idle_timeout,
                ),
                peer,
                &tls.server_name,
                req,
            )
            .await
        }
        None => {
            forward(
                &get(pool, peer.addr, up.connect_timeout, up.pool_idle_timeout),
                peer,
                req,
            )
            .await
        }
    }
}

/// Bidirectionally copy two upgraded streams until either side closes.
///
/// A clean close, EOF, reset, or abort on either side ends the tunnel with
/// `Ok`; only unexpected I/O failures surface as `Err`.
pub async fn tunnel<A, B>(mut a: A, mut b: B) -> std::io::Result<()>
where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset, UnexpectedEof};
    match tokio::io::copy_bidirectional(&mut a, &mut b).await {
        Ok(_) => Ok(()),
        Err(e)
            if matches!(
                e.kind(),
                UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use hyper::service::service_fn;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    #[test]
    fn retryability() {
        assert!(ForwardError::Connect("x".into()).is_retryable());
        assert!(ForwardError::Send("x".into()).is_retryable());
        assert!(!ForwardError::Response("x".into()).is_retryable());
    }

    #[test]
    fn idempotent_methods_are_the_safe_list() {
        for m in ["GET", "HEAD", "OPTIONS", "TRACE"] {
            assert!(
                is_idempotent(&hyper::Method::from_bytes(m.as_bytes()).unwrap()),
                "{m}"
            );
        }
        for m in ["POST", "PUT", "PATCH", "DELETE", "FOO"] {
            assert!(
                !is_idempotent(&hyper::Method::from_bytes(m.as_bytes()).unwrap()),
                "{m}"
            );
        }
    }

    #[test]
    fn retry_gate_method_by_error_matrix() {
        let kinds = [
            (FailureKind::Connect, "connect"),
            (FailureKind::Sent, "sent"),
            (FailureKind::Response, "response"),
            (FailureKind::Timeout, "timeout"),
        ];
        let idempotent = ["GET", "HEAD", "OPTIONS", "TRACE"];
        let mutating = ["POST", "PUT", "PATCH", "DELETE", "PROPFIND"];
        for (kind, kname) in kinds {
            for m in idempotent {
                let method = hyper::Method::from_bytes(m.as_bytes()).unwrap();
                let allowed = may_retry(&method, kind);
                let expected = !matches!(kind, FailureKind::Response);
                assert_eq!(allowed, expected, "{m} x {kname}");
            }
            for m in mutating {
                let method = hyper::Method::from_bytes(m.as_bytes()).unwrap();
                assert_eq!(
                    may_retry(&method, kind),
                    matches!(kind, FailureKind::Connect),
                    "{m} x {kname}"
                );
            }
        }
    }

    #[test]
    fn failure_kind_maps_error_variants() {
        assert_eq!(
            failure_kind(&ForwardError::Connect("x".into())),
            FailureKind::Connect
        );
        assert_eq!(
            failure_kind(&ForwardError::Send("x".into())),
            FailureKind::Sent
        );
        assert_eq!(
            failure_kind(&ForwardError::Response("x".into())),
            FailureKind::Response
        );
    }

    #[test]
    fn merge_xff_appends_client_ip() {
        let headers: Vec<(String, String)> = vec![
            ("X-Forwarded-For".into(), "203.0.113.7".into()),
            ("Accept".into(), "*/*".into()),
        ];
        assert_eq!(
            merge_xff(&headers, "198.51.100.9"),
            "203.0.113.7, 198.51.100.9"
        );
        assert_eq!(merge_xff(&[], "198.51.100.9"), "198.51.100.9");
    }

    /// Echo server: replies 200 with the request body and header facts.
    async fn echo(
        req: hyper::Request<hyper::body::Incoming>,
    ) -> Result<hyper::Response<Full<Bytes>>, std::convert::Infallible> {
        let (parts, incoming) = req.into_parts();
        let body = incoming.collect().await.unwrap().to_bytes();
        let mut resp = hyper::Response::new(Full::new(body));
        let headers = resp.headers_mut();
        if let Some(host) = parts.headers.get(HOST) {
            headers.insert("x-echo-host", host.clone());
        }
        if let Some(xff) = parts.headers.get(X_FORWARDED_FOR) {
            headers.insert("x-echo-xff", xff.clone());
        }
        if let Some(custom) = parts.headers.get("x-custom") {
            headers.insert("x-echo-custom", custom.clone());
        }
        headers.insert(
            "x-echo-conn",
            HeaderValue::from_static(if parts.headers.contains_key("connection") {
                "present"
            } else {
                "absent"
            }),
        );
        headers.insert(
            "x-echo-keepalive",
            HeaderValue::from_static(if parts.headers.contains_key("keep-alive") {
                "present"
            } else {
                "absent"
            }),
        );
        headers.insert(
            "x-echo-path",
            HeaderValue::from_str(parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or(""))
                .unwrap(),
        );
        Ok(resp)
    }

    async fn spawn_echo_server() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), service_fn(echo))
                        .await;
                });
            }
        });
        addr
    }

    fn sample_request() -> ForwardRequest {
        ForwardRequest {
            method: hyper::Method::POST,
            path_and_query: "/v1/chat?task=x".to_string(),
            headers: vec![
                ("Connection".into(), "keep-alive".into()),
                ("Keep-Alive".into(), "timeout=5".into()),
                ("Host".into(), "client.example".into()),
                ("X-Forwarded-For".into(), "203.0.113.7".into()),
                ("X-Custom".into(), "abc".into()),
            ],
            body: Bytes::from_static(b"hello upstream"),
            client_ip: "198.51.100.9".into(),
        }
    }

    #[tokio::test]
    async fn forwards_request_with_header_policy() {
        let addr = spawn_echo_server().await;
        let pool = crate::client::new_pool();
        let client = crate::client::get(
            &pool,
            addr,
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(30),
        );
        let peer = Peer { addr, weight: 1 };

        let resp = forward(&client, &peer, &sample_request()).await.unwrap();
        assert_eq!(resp.status(), hyper::StatusCode::OK);
        let headers = resp.headers().clone();
        assert_eq!(headers["x-echo-host"], addr.to_string());
        assert_eq!(headers["x-echo-xff"], "203.0.113.7, 198.51.100.9");
        assert_eq!(headers["x-echo-custom"], "abc");
        assert_eq!(headers["x-echo-conn"], "absent");
        assert_eq!(headers["x-echo-keepalive"], "absent");
        assert_eq!(headers["x-echo-path"], "/v1/chat?task=x");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, Bytes::from_static(b"hello upstream"));
    }

    #[tokio::test]
    async fn refused_connection_maps_to_connect_error() {
        // Port 1 on loopback: nothing listens, connect is refused fast.
        let addr: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
        let pool = crate::client::new_pool();
        let client = crate::client::get(
            &pool,
            addr,
            std::time::Duration::from_millis(500),
            std::time::Duration::from_secs(30),
        );
        let peer = Peer { addr, weight: 1 };

        let err = forward(&client, &peer, &sample_request())
            .await
            .unwrap_err();
        assert!(matches!(err, ForwardError::Connect(_)), "got {err:?}");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn refused_tls_connection_maps_to_connect_error() {
        let addr: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
        let pool = crate::client::new_pool();
        let tls = crate::tls::build(&openrusty_core::config::UpstreamTlsConfig {
            server_name: "localhost".into(),
            ca_cert: None,
            client_cert: None,
            client_key: None,
            insecure_skip_verify: true,
        })
        .unwrap();
        let client = crate::client::get_tls(
            &pool,
            addr,
            &tls,
            std::time::Duration::from_millis(500),
            std::time::Duration::from_secs(30),
        );
        let peer = Peer { addr, weight: 1 };

        let err = forward_https(&client, &peer, "localhost", &sample_request())
            .await
            .unwrap_err();
        assert!(matches!(err, ForwardError::Connect(_)), "got {err:?}");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn tls_handshake_failure_maps_to_connect_error() {
        // A plaintext HTTP server: the TCP connect succeeds but the TLS
        // handshake can never complete, so the failure must still be a
        // retryable Connect error (the peer saw nothing usable).
        let addr = spawn_echo_server().await;
        let pool = crate::client::new_pool();
        let tls = crate::tls::build(&openrusty_core::config::UpstreamTlsConfig {
            server_name: "localhost".into(),
            ca_cert: None,
            client_cert: None,
            client_key: None,
            insecure_skip_verify: true,
        })
        .unwrap();
        let client = crate::client::get_tls(
            &pool,
            addr,
            &tls,
            std::time::Duration::from_millis(500),
            std::time::Duration::from_secs(30),
        );
        let peer = Peer { addr, weight: 1 };

        let err = forward_https(&client, &peer, "localhost", &sample_request())
            .await
            .unwrap_err();
        assert!(matches!(err, ForwardError::Connect(_)), "got {err:?}");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn tunnel_copies_both_ways_and_ends_on_close() {
        let (client_side, server_side) = tokio::io::duplex(64);
        let (upstream_side, upstream_echo) = tokio::io::duplex(64);

        // Peer echo: returns everything it reads, then closes.
        let echo_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 64];
            let mut peer = upstream_side;
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                match peer.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => peer.write_all(&buf[..n]).await.unwrap(),
                    Err(_) => break,
                }
            }
            peer
        });

        // Run the tunnel in parallel with a client driving it.
        let tunnel_task = tokio::spawn(tunnel(client_side, upstream_echo));
        let driver = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut end = server_side;
            end.write_all(b"ping").await.unwrap();
            end.flush().await.unwrap();
            let mut buf = [0u8; 4];
            end.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"ping");
        });

        driver.await.unwrap();
        let _ = echo_task.await.unwrap();
        // Both duplex halves dropped: tunnel must finish cleanly.
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), tunnel_task)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_ok(), "tunnel error: {result:?}");
    }
}
