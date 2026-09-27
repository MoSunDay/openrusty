//! Request/balancer imports (`req_meta`, `req_peer_count/get`,
//! `balancer_set_peer`) and their pure payload encoders.
//!
//! Two-phase reads return the bytes written (>= 0) or the negated required
//! length (< 0); see docs/wasm-abi.md.

use crate::abi;
use crate::instance::{HostData, PeerView};
use crate::mem;
use openrusty_core::ReqCtx;
use std::borrow::Cow;
use wasmtime::{Caller, Linker};

/// The `req_meta` key for the buffered request body: the hot key, up to
/// 16 MiB per call (probe and write alike).
const REQ_META_BODY: &str = "body";

/// Register the request/balancer imports on the linker.
pub(crate) fn link(linker: &mut Linker<HostData>) -> Result<(), wasmtime::Error> {
    linker.func_wrap(
        abi::NS,
        abi::REQ_META,
        |mut caller: Caller<'_, HostData>,
         key_ptr: i32,
         key_len: i32,
         out_ptr: i32,
         out_cap: i32|
         -> i32 {
            let Some(key) = mem::read_guest_str(&mut caller, key_ptr, key_len) else {
                return 0;
            };
            // Guest writes take the caller mutably (wasmtime's
            // `Memory::write` takes the store mutably), so whatever is
            // handed to `write_out` must not borrow the store data. For
            // the body - previously a full copy on every call, the size
            // probe included - the refcounted `Bytes` handle keeps the
            // path copy-free apart from the guest-memory write itself.
            if key == REQ_META_BODY {
                let body = caller.data().req_body.clone(); // refcount bump
                return mem::write_out(&mut caller, out_ptr, out_cap, &body);
            }
            // Small keys: the encoder produces a borrowed view, this
            // call materializes it once (as before).
            let payload = {
                let d = caller.data();
                req_meta_payload(&d.ctx, &d.req_body, &key).into_owned()
            };
            mem::write_out(&mut caller, out_ptr, out_cap, &payload)
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::REQ_PEER_COUNT,
        |caller: Caller<'_, HostData>| -> i32 { caller.data().peers.len() as i32 },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::REQ_PEER_GET,
        |mut caller: Caller<'_, HostData>, idx: i32, out_ptr: i32, out_cap: i32| -> i32 {
            // Invalid index is not a buffer problem: report -2 (negatives are
            // otherwise reserved for -required_length).
            let Some(peer) = caller.data().peers.get(idx as usize) else {
                return -2;
            };
            let payload = peer_payload(peer);
            mem::write_out(&mut caller, out_ptr, out_cap, &payload)
        },
    )?;

    linker.func_wrap(
        abi::NS,
        abi::BALANCER_SET_PEER,
        |mut caller: Caller<'_, HostData>, idx: i32| -> i32 {
            let d = caller.data_mut();
            if idx >= 0 && (idx as usize) < d.peers.len() {
                // Health is enforced by the scheduler, not here.
                d.ctx.peer_index = Some(idx as u32);
                0
            } else {
                -1
            }
        },
    )?;

    Ok(())
}

/// Payload for `req_meta` (pure). Unknown keys yield an empty payload.
/// `body` is the buffered request body: seeded before the content phase,
/// so earlier phases see an empty slice. Returns a borrowed view wherever
/// the source can be referenced (the scalar keys and the body - zero
/// copies); only the keys that must reformat (`client_ip`, `headers`,
/// `header:<name>`) come back owned.
fn req_meta_payload<'a>(ctx: &'a ReqCtx, body: &'a [u8], key: &str) -> Cow<'a, [u8]> {
    match key {
        "method" => Cow::Borrowed(ctx.method.as_bytes()),
        "path" => Cow::Borrowed(ctx.path.as_bytes()),
        "query" => Cow::Borrowed(ctx.query.as_bytes()),
        "version" => Cow::Borrowed(ctx.version.as_bytes()),
        "client_ip" => Cow::Owned(ctx.client_ip().into_bytes()),
        "upstream" => Cow::Borrowed(ctx.upstream.as_deref().unwrap_or_default().as_bytes()),
        REQ_META_BODY => Cow::Borrowed(body),
        "headers" => {
            let items: Vec<String> = ctx
                .headers
                .iter()
                .map(|(n, v)| format!("{n}: {v}"))
                .collect();
            let refs: Vec<&[u8]> = items.iter().map(|s| s.as_bytes()).collect();
            Cow::Owned(abi::encode_tlv(&refs))
        }
        k if let Some(name) = k.strip_prefix("header:") => Cow::Owned(
            ctx.header(name)
                .map(|v| v.as_bytes().to_vec())
                .unwrap_or_default(),
        ),
        _ => Cow::Borrowed(&[][..]),
    }
}

/// TLV payload for `req_peer_get`: [name, addr, healthy("1"/"0")].
fn peer_payload(peer: &PeerView) -> Vec<u8> {
    let healthy = if peer.healthy { "1" } else { "0" };
    abi::encode_tlv(&[
        peer.name.as_bytes(),
        peer.addr.as_bytes(),
        healthy.as_bytes(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ReqCtx {
        ReqCtx {
            method: "POST".into(),
            path: "/echo".into(),
            query: "task=t1".into(),
            version: "HTTP/1.1".into(),
            client_addr: "127.0.0.1:9999".parse().unwrap(),
            headers: vec![("x-task".into(), "t1".into())],
            route_index: None,
            upstream: Some("vllm".into()),
            peer_index: None,
            attempts: 0,
            tried: Vec::new(),
        }
    }

    #[test]
    fn body_key_returns_buffered_request_body() {
        let body: &[u8] = br#"{"cache_salt":"agent:s1"}"#;
        assert_eq!(&*req_meta_payload(&ctx(), body, "body"), &body[..]);
    }

    #[test]
    fn body_key_is_empty_before_buffering() {
        assert!((&*req_meta_payload(&ctx(), &[], "body")).is_empty());
    }

    #[test]
    fn known_keys_still_resolve() {
        let c = ctx();
        assert_eq!(&*req_meta_payload(&c, &[], "method"), b"POST".as_slice());
        assert_eq!(&*req_meta_payload(&c, &[], "path"), b"/echo".as_slice());
        assert_eq!(&*req_meta_payload(&c, &[], "query"), b"task=t1".as_slice());
        assert_eq!(
            &*req_meta_payload(&c, &[], "client_ip"),
            b"127.0.0.1".as_slice()
        );
        assert_eq!(&*req_meta_payload(&c, &[], "upstream"), b"vllm".as_slice());
        assert_eq!(
            &*req_meta_payload(&c, &[], "header:x-task"),
            b"t1".as_slice()
        );
        assert!((&*req_meta_payload(&c, &[], "no_such_key")).is_empty());
        // No upstream selected: empty, not an error.
        let mut bare = ctx();
        bare.upstream = None;
        assert!((&*req_meta_payload(&bare, &[], "upstream")).is_empty());
    }

    #[test]
    fn scalar_keys_and_body_borrow_their_source() {
        // The Cow must actually borrow: no copy is paid for the body
        // (up to 16 MiB) or the scalar request facts.
        let c = ctx();
        let body: &[u8] = b"0123456789abcdef";
        match req_meta_payload(&c, body, "body") {
            Cow::Borrowed(b) => assert!(std::ptr::eq(b.as_ptr(), body.as_ptr())),
            Cow::Owned(_) => panic!("body must be borrowed"),
        }
        match req_meta_payload(&c, body, "method") {
            Cow::Borrowed(b) => assert!(std::ptr::eq(b.as_ptr(), c.method.as_ptr())),
            Cow::Owned(_) => panic!("method must be borrowed"),
        }
        // Reformatting keys stay owned by construction.
        assert!(matches!(
            req_meta_payload(&c, body, "client_ip"),
            Cow::Owned(_)
        ));
        assert!(matches!(
            req_meta_payload(&c, body, "header:x-task"),
            Cow::Owned(_)
        ));
    }
}
