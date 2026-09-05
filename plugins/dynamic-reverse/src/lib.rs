//! dynamic-reverse: second example module for the dynamic single-module
//! execution API (`POST /api/v1/dynamic/reverse`). Served as
//! `<dynamic.dir>/reverse.wasm`; the integration drill also copies its
//! file OVER `echo.wasm` to prove the stat-driven module cache:
//! replacing a module file changes the endpoint's behavior on the very
//! next request, with no reload in between.
//!
//! Content phase: answer with the request body's bytes reversed (or
//! `dynamic-reverse` reversed when the request carried none). No custom
//! headers are set, so the endpoint's default
//! `content-type: text/plain; charset=utf-8` applies; the body goes
//! through `set_resp_body` and `Decision::Done` (Done + body -> 200).
#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use openrusty_sdk::{host, Decision};

// Guest allocator exports (`orr_alloc`/`orr_dealloc`) -- wasm only, so
// host-side unit tests of this crate keep the system allocator.
#[cfg(target_arch = "wasm32")]
openrusty_sdk::export_allocators!();

/// Body answered (reversed) when the request carries no body.
const DEFAULT_BODY: &[u8] = b"dynamic-reverse";

/// Reverse a byte slice end to end.
fn reverse_bytes(data: &[u8]) -> Vec<u8> {
    data.iter().rev().copied().collect()
}

/// Content phase: short-circuit with the reversed request body.
#[openrusty_sdk::phase(content)]
fn on_content() -> Decision {
    let body = host::req_body();
    let base = match body.as_deref() {
        Some(bytes) if !bytes.is_empty() => bytes,
        _ => DEFAULT_BODY,
    };
    if !host::set_resp_body(&reverse_bytes(base)) {
        return Decision::Deny(500);
    }
    Decision::Done
}

openrusty_sdk::dispatch! {
    content => on_content,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverses_request_bytes() {
        assert_eq!(reverse_bytes(b"hello").as_slice(), b"olleh".as_slice());
        assert_eq!(reverse_bytes(b"abc").as_slice(), b"cba".as_slice());
        assert_eq!(reverse_bytes(b"a").as_slice(), b"a".as_slice());
        assert!(reverse_bytes(b"").is_empty());
    }

    #[test]
    fn bodyless_request_answers_reversed_default() {
        assert_eq!(
            reverse_bytes(DEFAULT_BODY).as_slice(),
            b"esrever-cimanyd".as_slice()
        );
    }
}
