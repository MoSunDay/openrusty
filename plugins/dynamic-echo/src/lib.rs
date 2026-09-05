//! dynamic-echo: example module for the dynamic single-module execution
//! API (`POST /api/v1/dynamic/echo`, enabled by the `[dynamic]` config
//! section). Served as `<dynamic.dir>/echo.wasm`.
//!
//! Content phase: answer with the request body (or `dynamic-echo` when
//! the request carried none), prefixed `"<greeting>: "` when the
//! `[dynamic.settings.echo] greeting = "..."` setting is present -- one
//! roundtrip proving both the request-body and the per-module config
//! plumbing. The module sets `x-dynamic: echo` and its own
//! `content-type: application/octet-stream` (module headers win over
//! the endpoint default), writes the body via `set_resp_body` and
//! seals it with `Decision::Done` (Done + body -> 200 + body).
//!
//! The `access` phase declines untouched (multi-phase dispatch in one
//! module) and the `log` phase emits one debug line, mirroring the
//! synthesized single-plugin pipeline (post_read -> rewrite -> access
//! -> content -> header_filter -> log; no balancer/body_filter/proxy).
#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use openrusty_sdk::{host, Decision};

// Guest allocator exports (`orr_alloc`/`orr_dealloc`) -- wasm only, so
// host-side unit tests of this crate keep the system allocator.
#[cfg(target_arch = "wasm32")]
openrusty_sdk::export_allocators!();

/// Body answered when the request carries no body.
const DEFAULT_BODY: &[u8] = b"dynamic-echo";
/// Setting key read from `[dynamic.settings.echo]`.
const GREETING_KEY: &str = "greeting";

/// Effective base body: the request body, or the module default when
/// the request is body-less (`req_body` maps empty to `None`).
fn base_body(req: Option<&[u8]>) -> &[u8] {
    match req {
        Some(bytes) if !bytes.is_empty() => bytes,
        _ => DEFAULT_BODY,
    }
}

/// Join greeting and body: `"<greeting>: <body>"` when the setting is
/// present and non-empty, else the body unchanged.
fn join_greeting(greeting: Option<&str>, body: &[u8]) -> Vec<u8> {
    match greeting {
        Some(prefix) if !prefix.is_empty() => {
            let mut out = Vec::with_capacity(prefix.len() + 2 + body.len());
            out.extend_from_slice(prefix.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(body);
            out
        }
        _ => body.to_vec(),
    }
}

/// Access phase: nothing to gate, decline so the synthesized pipeline
/// keeps running.
#[openrusty_sdk::phase(access)]
fn on_access() -> Decision {
    Decision::Declined
}

/// Content phase: compose the body, stamp the module headers and
/// short-circuit with the response body.
#[openrusty_sdk::phase(content)]
fn on_content() -> Decision {
    let body = join_greeting(
        host::cfg(GREETING_KEY).as_deref(),
        base_body(host::req_body().as_deref()),
    );
    host::set_resp_header("x-dynamic", "echo");
    host::set_resp_header("content-type", "application/octet-stream");
    if !host::set_resp_body(&body) {
        return Decision::Deny(500);
    }
    Decision::Done
}

/// Log phase: one debug line per request (lands in the gateway log).
#[openrusty_sdk::phase(log)]
fn on_log() -> Decision {
    host::log(host::LogLevel::Debug, "dynamic-echo log phase");
    Decision::Declined
}

openrusty_sdk::dispatch! {
    access => on_access,
    content => on_content,
    log => on_log,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_wins_over_default() {
        assert_eq!(base_body(Some(b"hello")), b"hello".as_slice());
        assert_eq!(base_body(Some(b"")), DEFAULT_BODY);
        assert_eq!(base_body(None), DEFAULT_BODY);
    }

    #[test]
    fn greeting_prefixes_when_set() {
        assert_eq!(
            join_greeting(Some("hi"), b"hello").as_slice(),
            b"hi: hello".as_slice()
        );
        assert_eq!(
            join_greeting(Some(""), b"hello").as_slice(),
            b"hello".as_slice()
        );
        assert_eq!(
            join_greeting(None, b"hello").as_slice(),
            b"hello".as_slice()
        );
    }

    #[test]
    fn bodyless_request_answers_greeted_default() {
        assert_eq!(
            join_greeting(None, base_body(None)).as_slice(),
            b"dynamic-echo".as_slice()
        );
        assert_eq!(
            join_greeting(Some("hi"), base_body(None)).as_slice(),
            b"hi: dynamic-echo".as_slice()
        );
    }
}
