//! Test upstream used by integration drills.
//!
//! Usage:
//!   cargo run -p openrusty-server --example echo_upstream -- 127.0.0.1:9001 nodeA
//!
//! Endpoints:
//!   GET|POST /echo           -> JSON echo of the request
//!   GET /sse?n=&sleep_ms=&delay_ms= -> text/event-stream, n events
//!   GET /slow?ms=            -> sleeps then 200 JSON (in-flight reload tests)
//!   GET /ws                  -> WebSocket echo (Text->Text, Binary->Binary)
//!   GET /                    -> "node:<name>"
//!   any other method+path    -> same JSON echo as /echo (fallback), so
//!                               arbitrary-path probes always get 200 JSON
//!                               carrying the `node` field

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let addr: SocketAddr = args
        .next()
        .unwrap_or_else(|| "127.0.0.1:9001".to_string())
        .parse()
        .expect("valid listen address, e.g. 127.0.0.1:9001");
    let node = args.next().unwrap_or_else(|| "node?".to_string());

    let app = Router::new()
        .route("/echo", get(echo).post(echo))
        .route("/sse", get(sse))
        .route("/slow", get(slow))
        .route("/ws", get(ws))
        .route("/", get(root))
        .fallback(echo)
        .with_state(node.clone());

    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    println!("echo upstream '{node}' listening on {addr}");
    axum::serve(listener, app).await.expect("serve");
}

/// Echo the request back as JSON. Doubles as the Router fallback so any
/// unmatched path (any method) still yields 200 JSON with the node field.
async fn echo(
    State(node): State<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Json<Value> {
    let mut hdrs: HashMap<String, String> = HashMap::new();
    for (k, v) in headers.iter() {
        hdrs.insert(k.to_string(), v.to_str().unwrap_or("").to_string());
    }
    let text = String::from_utf8_lossy(&body);
    let truncated: String = text.chars().take(4096).collect();
    Json(json!({
        "node": node,
        "method": method.as_str(),
        "path": uri.path(),
        "query": uri.query().unwrap_or(""),
        "headers": hdrs,
        "body_len": body.len(),
        "body": truncated,
    }))
}

#[derive(Deserialize, Default)]
struct SseArgs {
    n: Option<u64>,
    sleep_ms: Option<u64>,
    delay_ms: Option<u64>,
}

/// Server-sent events: `n` events, `sleep_ms` between them, optional
/// `delay_ms` before the first byte (headers included).
async fn sse(Query(args): Query<SseArgs>) -> impl IntoResponse {
    if let Some(d) = args.delay_ms {
        tokio::time::sleep(Duration::from_millis(d)).await;
    }
    let n = args.n.unwrap_or(5);
    let sleep = Duration::from_millis(args.sleep_ms.unwrap_or(20));
    let stream = futures::stream::unfold(0u64, move |i| async move {
        if i >= n {
            return None;
        }
        if i > 0 {
            tokio::time::sleep(sleep).await;
        }
        let event = Ok::<_, Infallible>(Event::default().data(i.to_string()));
        Some((event, i + 1))
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

#[derive(Deserialize, Default)]
struct SlowArgs {
    ms: Option<u64>,
}

/// Sleep `ms` milliseconds (default 2000), then answer. Used to exercise
/// in-flight requests across hot reloads and route timeouts.
async fn slow(State(node): State<String>, Query(args): Query<SlowArgs>) -> Json<Value> {
    let ms = args.ms.unwrap_or(2000);
    tokio::time::sleep(Duration::from_millis(ms)).await;
    Json(json!({ "node": node, "slept_ms": ms }))
}

/// WebSocket upgrade: echo each message back.
async fn ws(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(echo_socket)
}

async fn echo_socket(mut socket: WebSocket) {
    while let Some(Ok(msg)) = socket.recv().await {
        let reply = match msg {
            Message::Text(t) => Message::Text(t),
            Message::Binary(b) => Message::Binary(b),
            Message::Close(_) => break,
            other => other,
        };
        if socket.send(reply).await.is_err() {
            break;
        }
    }
}

async fn root(State(node): State<String>) -> String {
    format!("node:{node}")
}
