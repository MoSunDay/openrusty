//! kv-probe: E2E drill plugin exercising the host KV store and the
//! content / header_filter phases.
//!
//! Content phase: requests under `/probe` run KV set/get/del roundtrips,
//! read-after-write visibility and TTL-expiry checks driven by the `mode`
//! query parameter. The `pset` / `pdel` flags (any path) maintain a
//! marker key that the header_filter phase later echoes into a response
//! header, proving cross-phase KV visibility.
//!
//! KV layout:
//! - `probe:roundtrip` -> set/get/del roundtrip key (TTL = 10s)
//! - `probe:ttl`       -> short-TTL expiry key (TTL = 1.5s)
//! - `probe:marker`    -> marker observed by header_filter (TTL = 10s)
//!
//! Modes (`/probe?mode=<m>`):
//! - `setgetdel` : full set -> get -> del -> get-absent roundtrip
//! - `absent`    : roundtrip key must be absent
//! - `ttl-set`   : plant the short-TTL key
//! - `ttl-check` : short-TTL key must have expired
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

openrusty_sdk::dispatch! {
    content => on_content,
    header_filter => on_header_filter,
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

/// Map the raw query string to a probe mode (pure, host-unit-testable).
fn parse_mode(query: &str) -> Option<ProbeMode> {
    match url::query_param(query, "mode")?.as_str() {
        "setgetdel" => Some(ProbeMode::SetGetDel),
        "absent" => Some(ProbeMode::Absent),
        "ttl-set" => Some(ProbeMode::TtlSet),
        "ttl-check" => Some(ProbeMode::TtlCheck),
        _ => None,
    }
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
}
