//! WASM ABI contract: import/export names and the TLV codec
//! (see docs/wasm-abi.md for the authoritative specification).

/// Import namespace for every host function.
pub const NS: &str = "openrusty";

// Host imports (namespace `openrusty`).
pub const HOST_LOG: &str = "host_log";
pub const HOST_NOW_MS: &str = "host_now_ms";
pub const REQ_META: &str = "req_meta";
pub const REQ_PEER_COUNT: &str = "req_peer_count";
pub const REQ_PEER_GET: &str = "req_peer_get";
pub const BALANCER_SET_PEER: &str = "balancer_set_peer";
pub const KV_GET: &str = "kv_get";
pub const KV_SET: &str = "kv_set";
pub const KV_DEL: &str = "kv_del";
pub const KV_SCAN_BEGIN: &str = "kv_scan_begin";
pub const KV_SCAN_NEXT: &str = "kv_scan_next";
pub const KV_SCAN_END: &str = "kv_scan_end";
pub const RESP_HEADER_GET: &str = "resp_header_get";
pub const RESP_HEADER_SET: &str = "resp_header_set";
pub const RESP_HEADER_DEL: &str = "resp_header_del";
pub const BODY_CHUNK: &str = "body_chunk";
pub const BODY_IS_LAST: &str = "body_is_last";
pub const CFG_GET: &str = "cfg_get";

// Guest exports required/recognized by the ABI.
pub const EXPORT_ON_PHASE: &str = "orr_on_phase";
pub const EXPORT_ALLOC: &str = "orr_alloc";
pub const EXPORT_DEALLOC: &str = "orr_dealloc";
/// Conventional memory export name probed by the host when the guest does
/// not pass memory explicitly.
pub const MEMORY_EXPORT: &str = "memory";

/// TLV-encode a list of byte items: each item is `u32 len (LE) | bytes`.
pub fn encode_tlv(items: &[&[u8]]) -> Vec<u8> {
    let total: usize = items.iter().map(|b| 4 + b.len()).sum();
    let mut out = Vec::with_capacity(total);
    for item in items {
        out.extend_from_slice(&(item.len() as u32).to_le_bytes());
        out.extend_from_slice(item);
    }
    out
}

/// Decode a TLV buffer. Returns `None` on any malformed input (truncated
/// length prefix or body shorter than declared).
pub fn decode_tlv(buf: &[u8]) -> Option<Vec<&[u8]>> {
    let mut items = Vec::new();
    let mut pos = 0usize;
    while pos < buf.len() {
        if pos + 4 > buf.len() {
            return None;
        }
        let len = u32::from_le_bytes(buf[pos..pos + 4].try_into().ok()?) as usize;
        pos += 4;
        if pos + len > buf.len() {
            return None;
        }
        items.push(&buf[pos..pos + len]);
        pos += len;
    }
    Some(items)
}

/// Encode one key/value pair as a two-item TLV list (kv_scan_next format).
pub fn encode_kv_pair(key: &[u8], value: &[u8]) -> Vec<u8> {
    encode_tlv(&[key, value])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tlv_roundtrip() {
        let items: [&[u8]; 3] = [b"", b"hello", b"\x00\xffbinary"];
        let buf = encode_tlv(&items);
        let back = decode_tlv(&buf).expect("valid buffer decodes");
        assert_eq!(back.len(), 3);
        assert_eq!(back[0], b"");
        assert_eq!(back[1], b"hello");
        assert_eq!(back[2], b"\x00\xffbinary");
    }

    #[test]
    fn tlv_empty_input_is_empty_list() {
        assert_eq!(decode_tlv(b""), Some(Vec::new()));
    }

    #[test]
    fn tlv_rejects_malformed() {
        // Truncated length header.
        assert_eq!(decode_tlv(&[1, 0]), None);
        // Declared length longer than remaining bytes.
        assert_eq!(decode_tlv(&[9, 0, 0, 0, b'a']), None);
        // Trailing garbage after a valid item.
        assert_eq!(decode_tlv(&[1, 0, 0, 0, b'a', 0xff]), None);
    }

    #[test]
    fn kv_pair_roundtrip() {
        let buf = encode_kv_pair(b"k", b"value");
        let back = decode_tlv(&buf).unwrap();
        assert_eq!(back, vec![b"k".as_slice(), b"value".as_slice()]);
    }
}
