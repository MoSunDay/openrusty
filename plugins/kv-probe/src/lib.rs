//! kv-probe: E2E drill plugin exercising the host KV store across the
//! post_read, rewrite, access, content, header_filter, body_filter and
//! log phases.
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
//! - `probe:body_seen` -> body_filter accumulator "<bytes>:<0|1>" (10s)
//! - `probe:log_ran`   -> "1" once the log phase marked (TTL = 10s)
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
//! - `scan`         : kv_scan("probe:scan:") sees both scan keys
//!
//! Flags: `pset`/`pdel` (marker), `phdr` (header echo), `rwdeny` (rewrite
//! denies 418), `accdeny` (access denies 403), `bmark` (body_filter
//! accumulates), `lmark` (log phase marker).
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
const BODY_SEEN_KEY: &str = "probe:body_seen";
const LOG_RAN_KEY: &str = "probe:log_ran";
const SCAN_PREFIX: &str = "probe:scan:";
const SCAN_A_KEY: &str = "probe:scan:a";
const SCAN_B_KEY: &str = "probe:scan:b";
const PROBE_TTL_MS: i64 = 10_000;
const SHORT_TTL_MS: i64 = 1_500;
const RESP_HEADER: &str = "x-kv-probe";

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
}

/// Post-read phase: record the request path for cross-phase checks.
#[openrusty_sdk::phase(post_read)]
fn on_post_read() -> Decision {
    if let Some(path) = host::req_meta("path") {
        host::kv_set(POST_READ_KEY, &path, PROBE_TTL_MS);
    }
    Decision::Declined
}

/// Rewrite phase: record the request path; `rwdeny` aborts with 418.
#[openrusty_sdk::phase(rewrite)]
fn on_rewrite() -> Decision {
    let query = host::req_meta_str("query").unwrap_or_default();
    if let Some(path) = host::req_meta("path") {
        host::kv_set(REWRITE_KEY, &path, PROBE_TTL_MS);
    }
    if flag(&query, "rwdeny") {
        return Decision::Deny(418);
    }
    Decision::Declined
}

/// Access phase: `accdeny` aborts with 403.
#[openrusty_sdk::phase(access)]
fn on_access() -> Decision {
    let query = host::req_meta_str("query").unwrap_or_default();
    if flag(&query, "accdeny") {
        Decision::Deny(403)
    } else {
        Decision::Declined
    }
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
        None => Decision::Deny(400),
    }
}

/// Header filter phase: echo the marker value (or `-` when absent)
/// into `x-kv-probe` when the `phdr` flag is set.
#[openrusty_sdk::phase(header_filter)]
fn on_header_filter() -> Decision {
    let query = host::req_meta_str("query").unwrap_or_default();
    if !flag(&query, "phdr") {
        return Decision::Declined;
    }
    let value = match host::kv_get(MARKER_KEY) {
        Some(raw) => String::from_utf8_lossy(&raw).into_owned(),
        None => String::from("-"),
    };
    host::set_resp_header(RESP_HEADER, &value);
    Decision::Ok
}

/// Body filter phase: with `bmark`, accumulate upstream body bytes and
/// the final-chunk flag into `probe:body_seen` ("<bytes>:<0|1>").
/// Observe-only; every chunk is passed through unchanged.
#[openrusty_sdk::phase(body_filter)]
fn on_body_filter() -> Decision {
    let query = host::req_meta_str("query").unwrap_or_default();
    if !flag(&query, "bmark") {
        return Decision::Declined;
    }
    let (chunk, last) = host::body_chunk();
    let current = host::kv_get(BODY_SEEN_KEY)
        .map(|raw| String::from_utf8_lossy(&raw).into_owned())
        .unwrap_or_default();
    let (seen, _) = parse_body_seen(&current).unwrap_or((0, false));
    let marker =
        alloc::format!("{}:{}", seen + chunk.len() as u64, u8::from(last));
    host::kv_set(BODY_SEEN_KEY, marker.as_bytes(), PROBE_TTL_MS);
    Decision::Declined
}

/// Log phase: with `lmark`, drop the marker and emit a log line.
#[openrusty_sdk::phase(log)]
fn on_log() -> Decision {
    let query = host::req_meta_str("query").unwrap_or_default();
    if flag(&query, "lmark") {
        host::kv_set(LOG_RAN_KEY, b"1", PROBE_TTL_MS);
        host::log(host::LogLevel::Info, "kv-probe log phase marker");
    }
    Decision::Declined
}

openrusty_sdk::dispatch! {
    post_read => on_post_read,
    rewrite => on_rewrite,
    access => on_access,
    content => on_content,
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

/// Done when the body_filter accumulator ended "<n>:1" with n > 0.
fn body_check() -> Decision {
    let raw = host::kv_get(BODY_SEEN_KEY)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    match parse_body_seen(&raw) {
        Some((total, true)) if total > 0 => Decision::Done,
        _ => Decision::Deny(409),
    }
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
mod tests {
    use super::*;

    #[test]
    fn flag_accepts_only_one() {
        assert!(flag("pset=1", "pset"));
        assert!(flag("a=2&pset=1&b=3", "pset"));
        assert!(flag("pset=1&b=3", "pset"));
        assert!(flag("a=2&pset=1", "pset"));
    }

    #[test]
    fn flag_rejects_other_values() {
        assert!(!flag("pset=", "pset"));
        assert!(!flag("pset", "pset"));
        assert!(!flag("pset=0", "pset"));
        assert!(!flag("pset=11", "pset"));
        assert!(!flag("pset=true", "pset"));
        assert!(!flag("other=1", "pset"));
        assert!(!flag("", "pset"));
    }

    #[test]
    fn parse_mode_maps_all_modes() {
        assert_eq!(parse_mode("mode=setgetdel"), Some(ProbeMode::SetGetDel));
        assert_eq!(parse_mode("mode=absent"), Some(ProbeMode::Absent));
        assert_eq!(parse_mode("mode=ttl-set"), Some(ProbeMode::TtlSet));
        assert_eq!(parse_mode("mode=ttl-check"), Some(ProbeMode::TtlCheck));
        assert_eq!(parse_mode("mode=postread"), Some(ProbeMode::PostRead));
        assert_eq!(
            parse_mode("mode=rewritecheck"),
            Some(ProbeMode::RewriteCheck)
        );
        assert_eq!(parse_mode("mode=bodycheck"), Some(ProbeMode::BodyCheck));
        assert_eq!(parse_mode("mode=logcheck"), Some(ProbeMode::LogCheck));
        assert_eq!(parse_mode("mode=scan"), Some(ProbeMode::Scan));
        assert_eq!(
            parse_mode("a=1&mode=absent&b=2"),
            Some(ProbeMode::Absent)
        );
    }

    #[test]
    fn parse_mode_rejects_unknown_or_missing() {
        assert_eq!(parse_mode("mode=bogus"), None);
        assert_eq!(parse_mode("mode="), None);
        assert_eq!(parse_mode("mode"), None);
        assert_eq!(parse_mode("other=setgetdel"), None);
        assert_eq!(parse_mode(""), None);
    }

    #[test]
    fn parse_body_seen_accepts_valid_values() {
        assert_eq!(parse_body_seen("0:0"), Some((0, false)));
        assert_eq!(parse_body_seen("123:0"), Some((123, false)));
        assert_eq!(parse_body_seen("1:1"), Some((1, true)));
        assert_eq!(parse_body_seen("4096:1"), Some((4096, true)));
    }

    #[test]
    fn parse_body_seen_rejects_malformed_values() {
        assert_eq!(parse_body_seen(""), None);
        assert_eq!(parse_body_seen("123"), None);
        assert_eq!(parse_body_seen(":1"), None);
        assert_eq!(parse_body_seen("1:"), None);
        assert_eq!(parse_body_seen("x:1"), None);
        assert_eq!(parse_body_seen("1:2"), None);
        assert_eq!(parse_body_seen("-1:1"), None);
        assert_eq!(parse_body_seen("1:1:1"), None);
    }
}
