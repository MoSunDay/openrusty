//! Request/balancer imports (`req_meta`, `req_peer_count/get`,
//! `balancer_set_peer`) and their pure payload encoders.
//!
//! Two-phase reads return the bytes written (>= 0) or the negated required
//! length (< 0); see docs/wasm-abi.md.

use crate::abi;
use crate::instance::{HostData, PeerView};
use crate::mem;
use openrusty_core::ReqCtx;
use wasmtime::{Caller, Linker};

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
            let payload = {
                let d = caller.data();
                req_meta_payload(&d.ctx, &key)
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
fn req_meta_payload(ctx: &ReqCtx, key: &str) -> Vec<u8> {
    match key {
        "method" => ctx.method.clone().into_bytes(),
        "path" => ctx.path.clone().into_bytes(),
        "query" => ctx.query.clone().into_bytes(),
        "version" => ctx.version.clone().into_bytes(),
        "client_ip" => ctx.client_addr.ip().to_string().into_bytes(),
        "upstream" => ctx.upstream.clone().unwrap_or_default().into_bytes(),
        "headers" => {
            let items: Vec<String> = ctx
                .headers
                .iter()
                .map(|(n, v)| format!("{n}: {v}"))
                .collect();
            let refs: Vec<&[u8]> = items.iter().map(|s| s.as_bytes()).collect();
            abi::encode_tlv(&refs)
        }
        k if let Some(name) = k.strip_prefix("header:") => ctx
            .header(name)
            .map(|v| v.as_bytes().to_vec())
            .unwrap_or_default(),
        _ => Vec::new(),
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
