//! kv-probe: E2E drill plugin exercising the host KV store across all
//! eight phases: post_read, rewrite, access, content, balancer,
//! header_filter, body_filter and log.
//!
//! Content phase: requests under `/probe` run KV set/get/del roundtrips,
//! read-after-write visibility, TTL-expiry, cross-phase visibility and
//! `kv_scan` checks driven by the `mode` query parameter. The `pset` /
//! `pdel` flags (any path) maintain a marker key that the header_filter
//! phase later echoes into a response header, proving cross-phase KV
//! visibility.
//!
//! KV layout:
//! - `probe:roundtrip` -> set/get/del roundtrip key (TTL = 10s)
//! - `probe:ttl`       -> short-TTL expiry key (TTL = 1.5s)
//! - `probe:marker`    -> marker observed by header_filter (TTL = 10s)
//! - `probe:post_read` -> request path recorded by post_read (TTL = 10s)
//! - `probe:rewrite`   -> request path recorded by rewrite (TTL = 10s)
//! - `probe:body_seen:<path><?query>`
//!                     -> per-request body_filter accumulator
//!                        "<bytes>:<0|1>" (TTL = 10s). The key carries the
//!                        request identity so concurrent or repeated
//!                        requests never alias each other's byte counts.
//! - `probe:ws_auth`   -> Authorization header of the last /ws handshake
//! - `probe:ws_cookie` -> Cookie header of the last /ws handshake
//! - `probe:log_ran`   -> "1" once the log phase marked (TTL = 10s)
//! - `probe:log404`    -> "1" once the log phase ran unrouted (TTL = 10s)
//! - `probe:scan:a|b`  -> transient keys for the `scan` mode
//!
//! Modes (`/probe?mode=<m>`):
//! - `setgetdel`    : full set -> get -> del -> get-absent roundtrip
//! - `absent`       : roundtrip key must be absent
//! - `ttl-set`      : plant the short-TTL key
//! - `ttl-check`    : short-TTL key must have expired
//! - `postread`     : post_read recorded this very request's path
//! - `rewritecheck` : rewrite recorded this very request's path
//! - `bodycheck`    : body_filter accumulator ended "<n>:1", n > 0
//! - `logcheck`     : the log phase marker is present
//! - `l404check`    : the no-route log marker is echoed into `X-Log404`
//! - `scan`         : kv_scan("probe:scan:") sees both scan keys
//! - `wsheaders`    : the last /ws handshake carried the expected
//!                    Authorization and Cookie headers
//! - `resp-body`    : pipeline-side `resp_body_set` + Decision
//!   short-circuit drill (see below)
//!
//! `resp-body` mode (`/probe?mode=resp-body[&text=<t>][&deny=<s>]`):
//! - `text=<t>` (absent `deny`): body is exactly
//!   `kv-probe-resp-body:<t>`, Decision::Done -> 200 + body.
//! - `text=<t>&deny=403`: same body, Decision::Deny(403) -> 403 + body
//!   (ABI: `Deny(s)` + body -> `s` + body; any 100..=599 status works,
//!   the drill only uses 403).
//! - no `text`: no body written, Decision::Done -> empty 204.
//! - a malformed/out-of-range `deny` denies with 400.
//!
//! Flags: `pset`/`pdel` (marker), `phdr` (header echo), `rwdeny` (rewrite
//! denies 418), `rwuri`/`rwhdr` (rewrite-phase request edits), `accdeny`
//! (access denies 403), `redir` (access answers 302), `bmark` (body_filter
//! accumulates), `lmark` (log phase marker), `l404` (log phase also sets
//! the no-route marker).
#![no_std]

extern crate alloc;

use alloc::string::String;
use openrusty_sdk::{host, url, Decision};

// Guest allocator exports (`orr_alloc`/`orr_dealloc`) -- wasm only, so
// host-side unit tests of this crate keep the system allocator.
#[cfg(target_arch = "wasm32")]
openrusty_sdk::export_allocators!();

const ROUNDTRIP_KEY: &str = "probe:roundtrip";
const TTL_KEY: &str = "probe:ttl";
const MARKER_KEY: &str = "probe:marker";
const MARKER_VAL: &[u8] = b"mk";
const POST_READ_KEY: &str = "probe:post_read";
const REWRITE_KEY: &str = "probe:rewrite";
const BODY_SEEN_PREFIX: &str = "probe:body_seen:";
const LOG_RAN_KEY: &str = "probe:log_ran";
const SCAN_PREFIX: &str = "probe:scan:";
const SCAN_A_KEY: &str = "probe:scan:a";
const SCAN_B_KEY: &str = "probe:scan:b";
const WS_HANDSHAKE_PATH: &str = "/ws";
const WS_AUTH_KEY: &str = "probe:ws_auth";
const WS_COOKIE_KEY: &str = "probe:ws_cookie";
// Dummy values the integration drill sends on its WebSocket handshake;
// they are fixtures, not credentials.
const WS_AUTH_EXPECT: &str = "Bearer ws-probe-token";
const WS_COOKIE_EXPECT: &str = "session=ws-probe-cookie";
const PROBE_TTL_MS: i64 = 10_000;
const SHORT_TTL_MS: i64 = 1_500;
const RESP_HEADER: &str = "x-kv-probe";
const RESP_BODY_PREFIX: &str = "kv-probe-resp-body:";
const RW_URI: &str = "/rewritten/by/plugin";
const RW_QUERY: &str = "rw=1";
const RW_HEADER: &str = "X-Rw-Probe";
const RW_DROP_HEADER: &str = "X-Rw-Drop";
const REDIRECT_TO: &str = "/moved/by/plugin";
const LOG_404_KEY: &str = "probe:log404";
const LOG_404_HEADER: &str = "X-Log404";

/// Probe modes selectable via the `mode` query parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeMode {
    /// Full set -> get -> del -> get-absent roundtrip.
    SetGetDel,
    /// The roundtrip key must be absent.
    Absent,
    /// Plant the short-TTL key.
    TtlSet,
    /// The short-TTL key must have expired.
    TtlCheck,
    /// post_read must have recorded this very request's path.
    PostRead,
    /// rewrite must have recorded this very request's path.
    RewriteCheck,
    /// body_filter accumulator must have ended "<n>:1" with n > 0.
    BodyCheck,
    /// The log phase marker must be present.
    LogCheck,
    /// kv_scan over the scan prefix must see both scan keys.
    Scan,
    /// Balancer phase: assert the 3-peer healthy view and pin the
    /// request to the first healthy peer.
    Balancer,
    /// The last /ws handshake must have carried the expected
    /// Authorization and Cookie headers.
    WsHeaders,
    /// Pipeline-side `resp_body_set` + Decision short-circuit drill:
    /// `text=<t>` writes `kv-probe-resp-body:<t>`, `deny=<s>` swaps the
    /// terminal `Done` for `Deny(s)`.
    RespBody,
    /// Echo the no-route log-phase marker (or `-` when absent) into
    /// `X-Log404` and short-circuit so the header reaches the client.
    Log404Check,
}

/// Post-read phase: record the request path for cross-phase checks and,
/// on a `/ws` handshake, the end-to-end auth headers the client sent.
#[openrusty_sdk::phase(post_read)]
fn on_post_read() -> Decision {
    if let Some(path) = host::req_meta("path") {
        host::kv_set(POST_READ_KEY, &path, PROBE_TTL_MS);
    }
    record_ws_handshake_headers();
    Decision::Declined
}

/// Record the Authorization/Cookie headers of a `/ws` handshake request
/// so a later `mode=wsheaders` probe can assert they survived the
/// gateway's WebSocket path.
fn record_ws_handshake_headers() {
    if host::req_meta_str("path").as_deref() != Some(WS_HANDSHAKE_PATH) {
        return;
    }
    if let Some(auth) = host::req_meta_str("header:authorization") {
        host::kv_set(WS_AUTH_KEY, auth.as_bytes(), PROBE_TTL_MS);
    }
    if let Some(cookie) = host::req_meta_str("header:cookie") {
        host::kv_set(WS_COOKIE_KEY, cookie.as_bytes(), PROBE_TTL_MS);
    }
}

/// Rewrite phase: record the request path; `rwdeny` aborts with 418;
/// `rwuri`/`rwhdr` apply the request edits.
#[openrusty_sdk::phase(rewrite)]
fn on_rewrite() -> Decision {
    let query = host::req_meta_str("query").unwrap_or_default();
    if let Some(path) = host::req_meta("path") {
        host::kv_set(REWRITE_KEY, &path, PROBE_TTL_MS);
    }
    if flag(&query, "rwdeny") {
        return Decision::Deny(418);
    }
    apply_req_rewrite(&query)
}

/// Rewrite-phase request edits: `rwuri` swaps in the probe URI + query
/// pair, `rwhdr` sets `X-Rw-Probe` and drops `X-Rw-Drop`. A host refusal
/// denies with 500 (the constants are ABI-valid, so refusal is a host
/// bug), mirroring the resp-body mode's fail-loud contract.
fn apply_req_rewrite(query: &str) -> Decision {
    if flag(query, "rwuri") && !(host::set_req_uri(RW_URI) && host::set_req_query(RW_QUERY)) {
        return Decision::Deny(500);
    }
    if flag(query, "rwhdr") {
        if !host::set_req_header(RW_HEADER, "yes") {
            return Decision::Deny(500);
        }
        host::del_req_header(RW_DROP_HEADER);
    }
    Decision::Declined
}

/// Access phase: `accdeny` aborts with 403, `redir` answers 302.
#[openrusty_sdk::phase(access)]
fn on_access() -> Decision {
    let query = host::req_meta_str("query").unwrap_or_default();
    if flag(&query, "accdeny") {
        return Decision::Deny(403);
    }
    redirect_decision(&query)
}

/// Access-phase redirect: `redir` answers 302 -> REDIRECT_TO via the
/// host helper (which coerces off-range statuses), others decline.
fn redirect_decision(query: &str) -> Decision {
    if flag(query, "redir") {
        host::respond_redirect(302, REDIRECT_TO)
    } else {
        Decision::Declined
    }
}

/// Balancer phase: for `/probe?mode=balancer` requests, assert that the
/// routed upstream exposes exactly 3 healthy peers, that out-of-range
/// `set_peer` calls are rejected, and pin the request to the first
/// healthy peer. Every other request is declined untouched, so the
/// plugin never interferes with normal balancing.
#[openrusty_sdk::phase(balancer)]
fn on_balancer() -> Decision {
    let query = host::req_meta_str("query").unwrap_or_default();
    if !matches!(parse_mode(&query), Some(ProbeMode::Balancer)) {
        return Decision::Declined;
    }
    let peers = host::peers();
    let Some(first_healthy) = valid_peer_view(&peers) else {
        return Decision::Deny(409);
    };
    if host::set_peer(999) || host::set_peer(u32::MAX) {
        return Decision::Deny(409);
    }
    if host::set_peer(first_healthy as u32) {
        Decision::Declined
    } else {
        Decision::Deny(409)
    }
}

/// Pure validation of the balancer-phase peer view: exactly 3 peers, all
/// healthy; returns the first healthy index.
fn valid_peer_view(peers: &[host::PeerInfo]) -> Option<usize> {
    if peers.len() != 3 || peers.iter().any(|p| !p.healthy) {
        return None;
    }
    peers.iter().position(|p| p.healthy)
}

/// Content phase: maintain the marker on any path, then run the
/// requested KV probe on `/probe` paths.
#[openrusty_sdk::phase(content)]
fn on_content() -> Decision {
    let query = host::req_meta_str("query").unwrap_or_default();
    if flag(&query, "pset") {
        host::kv_set(MARKER_KEY, MARKER_VAL, PROBE_TTL_MS);
    }
    if flag(&query, "pdel") {
        host::kv_del(MARKER_KEY);
    }
    // `meta=1` (any path): verify req_meta method/client_ip/header keys.
    if flag(&query, "meta") {
        let ok = host::req_meta("method").as_deref() == Some(b"GET".as_slice())
            && host::req_meta("client_ip")
                .map(|v| v.starts_with(b"127.0.0.1"))
                .unwrap_or(false)
            && host::req_meta("header:user-agent")
                .map(|v| !v.is_empty())
                .unwrap_or(false);
        return if ok {
            Decision::Done
        } else {
            Decision::Deny(409)
        };
    }
    let path = host::req_meta_str("path").unwrap_or_default();
    if !path.starts_with("/probe") {
        return Decision::Declined;
    }
    match parse_mode(&query) {
        Some(ProbeMode::SetGetDel) => roundtrip(),
        Some(ProbeMode::Absent) => absent(ROUNDTRIP_KEY),
        Some(ProbeMode::TtlSet) => {
            host::kv_set(TTL_KEY, b"x", SHORT_TTL_MS);
            Decision::Done
        }
        Some(ProbeMode::TtlCheck) => absent(TTL_KEY),
        Some(ProbeMode::PostRead) => equals_path(POST_READ_KEY),
        Some(ProbeMode::RewriteCheck) => equals_path(REWRITE_KEY),
        Some(ProbeMode::BodyCheck) => body_check(),
        Some(ProbeMode::LogCheck) => present(LOG_RAN_KEY, b"1"),
        Some(ProbeMode::Scan) => scan(),
        Some(ProbeMode::WsHeaders) => ws_headers(),
        Some(ProbeMode::Balancer) => Decision::Declined,
        Some(ProbeMode::RespBody) => resp_body(&query),
        Some(ProbeMode::Log404Check) => log404_check(),
        None => Decision::Deny(400),
    }
}

/// Header filter phase: echo the marker value (or `-` when absent)
/// into `x-kv-probe` when the `phdr` flag is set. With `respset=1`,
/// set the header and verify it reads back; with `respdel=1`, set,
/// verify, delete and verify it is gone.
#[openrusty_sdk::phase(header_filter)]
fn on_header_filter() -> Decision {
    let query = host::req_meta_str("query").unwrap_or_default();
    if flag(&query, "phdr") {
        let value = match host::kv_get(MARKER_KEY) {
            Some(raw) => String::from_utf8_lossy(&raw).into_owned(),
            None => String::from("-"),
        };
        host::set_resp_header(RESP_HEADER, &value);
        return Decision::Ok;
    }
    if flag(&query, "respset") {
        host::set_resp_header(RESP_HEADER, "mk");
        if host::resp_header(RESP_HEADER).as_deref() != Some("mk") {
            return Decision::Deny(409);
        }
        return Decision::Declined;
    }
    if flag(&query, "respdel") {
        host::set_resp_header(RESP_HEADER, "mk");
        if host::resp_header(RESP_HEADER).as_deref() != Some("mk") {
            return Decision::Deny(409);
        }
        host::del_resp_header(RESP_HEADER);
        if host::resp_header(RESP_HEADER).is_some() {
            return Decision::Deny(409);
        }
        return Decision::Declined;
    }
    Decision::Declined
}

/// Body filter phase: with `bmark`, accumulate upstream body bytes and
/// the final-chunk flag into the per-request key
/// `probe:body_seen:<path><?query>` ("<bytes>:<0|1>"). Observe-only;
/// every chunk is passed through unchanged.
#[openrusty_sdk::phase(body_filter)]
fn on_body_filter() -> Decision {
    let query = host::req_meta_str("query").unwrap_or_default();
    if !flag(&query, "bmark") {
        return Decision::Declined;
    }
    let (chunk, last) = host::body_chunk();
    let key = body_seen_key();
    let current = host::kv_get(&key)
        .map(|raw| String::from_utf8_lossy(&raw).into_owned())
        .unwrap_or_default();
    let (seen, _) = parse_body_seen(&current).unwrap_or((0, false));
    let marker = alloc::format!("{}:{}", seen + chunk.len() as u64, u8::from(last));
    host::kv_set(&key, marker.as_bytes(), PROBE_TTL_MS);
    Decision::Declined
}

/// Log phase: with `lmark`, drop the marker and emit a log line; with
/// `l404`, additionally record the no-route marker for `l404check`.
#[openrusty_sdk::phase(log)]
fn on_log() -> Decision {
    let query = host::req_meta_str("query").unwrap_or_default();
    if flag(&query, "lmark") {
        host::kv_set(LOG_RAN_KEY, b"1", PROBE_TTL_MS);
        host::log(host::LogLevel::Info, "kv-probe log phase marker");
    }
    if flag(&query, "l404") {
        host::kv_set(LOG_404_KEY, b"1", PROBE_TTL_MS);
    }
    Decision::Declined
}

/// `l404check` mode: echo the no-route log-phase marker (or `-` when
/// absent) into `X-Log404` and short-circuit, so the header survives to
/// the client (the gateway's proxy path reseeds plugin resp headers).
fn log404_check() -> Decision {
    let value = match host::kv_get(LOG_404_KEY) {
        Some(raw) => String::from_utf8_lossy(&raw).into_owned(),
        None => String::from("-"),
    };
    host::set_resp_header(LOG_404_HEADER, &value);
    Decision::Done
}

openrusty_sdk::dispatch! {
    post_read => on_post_read,
    rewrite => on_rewrite,
    access => on_access,
    content => on_content,
    balancer => on_balancer,
    header_filter => on_header_filter,
    body_filter => on_body_filter,
    log => on_log,
}

/// Full set -> get -> del -> get roundtrip against the host KV store.
fn roundtrip() -> Decision {
    host::kv_set(ROUNDTRIP_KEY, b"v1", PROBE_TTL_MS);
    if host::kv_get(ROUNDTRIP_KEY).as_deref() != Some(b"v1" as &[u8]) {
        return Decision::Deny(501);
    }
    host::kv_del(ROUNDTRIP_KEY);
    if host::kv_get(ROUNDTRIP_KEY).is_some() {
        return Decision::Deny(502);
    }
    Decision::Done
}

/// Done when `key` is absent from the KV store; conflict otherwise.
fn absent(key: &str) -> Decision {
    if host::kv_get(key).is_none() {
        Decision::Done
    } else {
        Decision::Deny(409)
    }
}

/// Done when `key` holds exactly `want`; conflict otherwise.
fn present(key: &str, want: &[u8]) -> Decision {
    if host::kv_get(key).as_deref() == Some(want) {
        Decision::Done
    } else {
        Decision::Deny(409)
    }
}

/// Done when `key` holds the current request path; conflict otherwise.
fn equals_path(key: &str) -> Decision {
    let path = host::req_meta("path").unwrap_or_default();
    let stored = host::kv_get(key);
    if !path.is_empty() && stored.as_deref() == Some(path.as_slice()) {
        Decision::Done
    } else {
        Decision::Deny(409)
    }
}

/// Done when at least one completed request's per-request body_filter
/// accumulator holds "<n>:1" with n > 0. The bookkeeping keys live under
/// the `probe:body_seen:` prefix, one per request, so this scans them
/// instead of reading a single (aliasing) global key.
fn body_check() -> Decision {
    let mut saw_full_body = false;
    for (_, value) in host::kv_scan(BODY_SEEN_PREFIX) {
        let raw = String::from_utf8_lossy(&value).into_owned();
        if let Some((total, true)) = parse_body_seen(&raw) {
            if total > 0 {
                saw_full_body = true;
            }
        }
    }
    if saw_full_body {
        Decision::Done
    } else {
        Decision::Deny(409)
    }
}

/// Done when the last `/ws` handshake carried the expected
/// Authorization and Cookie headers.
fn ws_headers() -> Decision {
    let auth_ok = host::kv_get(WS_AUTH_KEY).as_deref() == Some(WS_AUTH_EXPECT.as_bytes());
    let cookie_ok = host::kv_get(WS_COOKIE_KEY).as_deref() == Some(WS_COOKIE_EXPECT.as_bytes());
    if auth_ok && cookie_ok {
        Decision::Done
    } else {
        Decision::Deny(409)
    }
}

/// Body written by the `resp-body` mode: exactly
/// `kv-probe-resp-body:<t>` (pure, so tests pin the wire format).
fn resp_body_text(text: &str) -> String {
    alloc::format!("{RESP_BODY_PREFIX}{text}")
}

/// Terminal decision for the `resp-body` mode from the raw `deny`
/// parameter value (pure): absent -> `Done` (200 with the body, or the
/// empty 204 when none was written); a valid `100..=599` status ->
/// `Deny(s)` (the body, when present, rides along: `s` + body); a
/// malformed or out-of-range value -> `Deny(400)`.
fn deny_decision(deny: Option<&str>) -> Decision {
    match deny {
        None => Decision::Done,
        Some(raw) => match raw.parse::<u16>() {
            Ok(status) if (100..=599).contains(&status) => Decision::Deny(status),
            _ => Decision::Deny(400),
        },
    }
}

/// `resp-body` mode: exercise pipeline-side `resp_body_set` + Decision
/// short-circuit semantics. With `text=<t>` the body is exactly
/// `kv-probe-resp-body:<t>`; without it no body is written, so `Done`
/// keeps its body-less empty-204 shape. `deny=<s>` swaps the terminal
/// `Done` for `Deny(s)` on both shapes.
fn resp_body(query: &str) -> Decision {
    if let Some(text) = url::query_param(query, "text") {
        let body = resp_body_text(&text);
        // Refused by the host (over the 1 MiB cap / unreadable pointer):
        // fail loudly instead of answering a misleading empty 204.
        if !host::set_resp_body(body.as_bytes()) {
            return Decision::Deny(500);
        }
    }
    deny_decision(url::query_param(query, "deny").as_deref())
}

/// Per-request body_filter bookkeeping key: `<prefix><path>`, plus
/// `?<query>` when the request has one. Pure, so tests pin the layout.
fn body_seen_key_for(path: &str, query: &str) -> String {
    if query.is_empty() {
        alloc::format!("{}{}", BODY_SEEN_PREFIX, path)
    } else {
        alloc::format!("{}{}?{}", BODY_SEEN_PREFIX, path, query)
    }
}

/// The calling request's body_filter bookkeeping key.
fn body_seen_key() -> String {
    body_seen_key_for(
        &host::req_meta_str("path").unwrap_or_default(),
        &host::req_meta_str("query").unwrap_or_default(),
    )
}

/// Plant two transient keys, require `kv_scan` over the prefix to see
/// both (count >= 2 and both present), then clean them up again.
fn scan() -> Decision {
    host::kv_set(SCAN_A_KEY, b"1", PROBE_TTL_MS);
    host::kv_set(SCAN_B_KEY, b"2", PROBE_TTL_MS);
    let mut count = 0usize;
    let (mut saw_a, mut saw_b) = (false, false);
    for (key, _) in host::kv_scan(SCAN_PREFIX) {
        count += 1;
        saw_a |= key.as_slice() == SCAN_A_KEY.as_bytes();
        saw_b |= key.as_slice() == SCAN_B_KEY.as_bytes();
    }
    host::kv_del(SCAN_A_KEY);
    host::kv_del(SCAN_B_KEY);
    if count >= 2 && saw_a && saw_b {
        Decision::Done
    } else {
        Decision::Deny(409)
    }
}

/// Map the raw query string to a probe mode (pure, host-unit-testable).
fn parse_mode(query: &str) -> Option<ProbeMode> {
    match url::query_param(query, "mode")?.as_str() {
        "setgetdel" => Some(ProbeMode::SetGetDel),
        "absent" => Some(ProbeMode::Absent),
        "ttl-set" => Some(ProbeMode::TtlSet),
        "ttl-check" => Some(ProbeMode::TtlCheck),
        "postread" => Some(ProbeMode::PostRead),
        "rewritecheck" => Some(ProbeMode::RewriteCheck),
        "bodycheck" => Some(ProbeMode::BodyCheck),
        "logcheck" => Some(ProbeMode::LogCheck),
        "scan" => Some(ProbeMode::Scan),
        "wsheaders" => Some(ProbeMode::WsHeaders),
        "balancer" => Some(ProbeMode::Balancer),
        "resp-body" => Some(ProbeMode::RespBody),
        "l404check" => Some(ProbeMode::Log404Check),
        _ => None,
    }
}

/// Parse a `probe:body_seen` value "<bytes>:<0|1>" (pure).
fn parse_body_seen(raw: &str) -> Option<(u64, bool)> {
    let (total, last) = raw.split_once(':')?;
    let total: u64 = total.parse().ok()?;
    let last = match last {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    Some((total, last))
}

/// True only when query parameter `name` is exactly `1`.
fn flag(query: &str, name: &str) -> bool {
    url::query_param(query, name).as_deref() == Some("1")
}

#[cfg(test)]
mod tests;
