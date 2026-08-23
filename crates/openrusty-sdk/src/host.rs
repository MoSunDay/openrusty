//! Safe wrappers over the raw `openrusty` host imports.
//!
//! Conventions:
//! - Absent vs empty: `req_meta`/`kv_get`/`cfg_get` return `0` when a key
//!   is missing; empty values are indistinguishable from absent per the
//!   ABI, so any zero-byte read maps to `None`. Plugins must not store
//!   empty values.
//! - Lists are TLV-encoded: repeated `u32 LE length | bytes` items.

use alloc::string::String;
use alloc::vec::Vec;

use crate::ffi;

/// Log severity passed to `host_log`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum LogLevel {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
}

/// Emit one log line through the gateway log.
pub fn log(level: LogLevel, msg: &str) {
    unsafe { ffi::host_log(level as i32, msg.as_ptr() as i32, msg.len() as i32) }
}

/// Gateway-provided clock in milliseconds.
pub fn now_ms() -> u64 {
    unsafe { ffi::host_now_ms() as u64 }
}

fn nonempty(bytes: Vec<u8>) -> Option<Vec<u8>> {
    if bytes.is_empty() {
        None
    } else {
        Some(bytes)
    }
}

/// Request metadata (`method`, `path`, `query`, `header:<name>`, ...) as
/// raw bytes. `None` when absent or empty (ABI limitation, documented).
pub fn req_meta(key: &str) -> Option<Vec<u8>> {
    let out = unsafe {
        ffi::read_two_phase(|op, oc| ffi::req_meta(key.as_ptr() as i32, key.len() as i32, op, oc))?
    };
    nonempty(out)
}

/// Like [`req_meta`], decoded lossily to UTF-8.
pub fn req_meta_str(key: &str) -> Option<String> {
    req_meta(key).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Raw request body bytes. The gateway buffers the body (capped at
/// 16 MiB) before the content phase, so this is available from
/// content/balancer onward; earlier phases see no body. `None` for an
/// empty request body (same ABI limitation as [`req_meta`]).
pub fn req_body() -> Option<Vec<u8>> {
    req_meta("body")
}

/// One upstream peer of the routed upstream group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub name: String,
    pub addr: String,
    pub healthy: bool,
}

/// Peers of the routed upstream (check `healthy` before use).
pub fn peers() -> Vec<PeerInfo> {
    let count = unsafe { ffi::req_peer_count() };
    if count <= 0 {
        return Vec::new();
    }
    (0..count).filter_map(peer_at).collect()
}

fn peer_at(idx: i32) -> Option<PeerInfo> {
    let tlv = unsafe { ffi::read_two_phase(|op, oc| ffi::req_peer_get(idx, op, oc))? };
    Some(peer_from_tlv(&tlv))
}

/// Decode one `req_peer_get` TLV payload: name, addr, healthy ("1"/"0").
/// Malformed items decode as empty/false.
pub fn peer_from_tlv(tlv: &[u8]) -> PeerInfo {
    let items = parse_tlv(tlv);
    let text = |i: usize| {
        items
            .get(i)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default()
    };
    PeerInfo {
        name: text(0),
        addr: text(1),
        healthy: item_is(&items, 2, b"1"),
    }
}

fn item_is(items: &[&[u8]], idx: usize, expect: &[u8]) -> bool {
    items.get(idx).copied() == Some(expect)
}

/// Pin the current request to peer `idx` (balancer phase). True when the
/// host accepted the choice.
pub fn set_peer(idx: u32) -> bool {
    unsafe { ffi::balancer_set_peer(idx as i32) == 0 }
}

/// Read a key from the shared KV store. `None` when missing (`0` return);
/// empty values are indistinguishable from missing per the ABI.
pub fn kv_get(key: &str) -> Option<Vec<u8>> {
    let out = unsafe {
        ffi::read_two_phase(|op, oc| ffi::kv_get(key.as_ptr() as i32, key.len() as i32, op, oc))?
    };
    nonempty(out)
}

/// Write a key with a TTL in milliseconds.
pub fn kv_set(key: &str, val: &[u8], ttl_ms: i64) {
    unsafe {
        ffi::kv_set(
            key.as_ptr() as i32,
            key.len() as i32,
            val.as_ptr() as i32,
            val.len() as i32,
            ttl_ms,
        );
    }
}

/// Delete a key from the shared KV store.
pub fn kv_del(key: &str) {
    unsafe { ffi::kv_del(key.as_ptr() as i32, key.len() as i32) };
}

/// Iterator over KV entries with a given prefix. Yields `(key, value)`
/// pairs. On host builds the stubbed scan is simply empty.
pub struct KvScan {
    cursor: i32,
}

/// Start a scan of all KV keys starting with `prefix`.
pub fn kv_scan(prefix: &str) -> KvScan {
    let cursor = unsafe { ffi::kv_scan_begin(prefix.as_ptr() as i32, prefix.len() as i32) };
    KvScan { cursor }
}

impl Iterator for KvScan {
    type Item = (Vec<u8>, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor < 0 {
            return None;
        }
        let mut buf = alloc::vec![0u8; 256];
        // Per ABI: ret == 0 exhausted, ret < 0 invalid cursor (no retry).
        let ret =
            unsafe { ffi::kv_scan_next(self.cursor, buf.as_mut_ptr() as i32, buf.len() as i32) };
        if ret == 0 {
            return None;
        }
        if ret < 0 {
            self.cursor = -1;
            return None;
        }
        buf.truncate(ret as usize);
        Some(kv_pair_from_tlv(&buf))
    }
}

impl Drop for KvScan {
    fn drop(&mut self) {
        if self.cursor >= 0 {
            unsafe { ffi::kv_scan_end(self.cursor) };
        }
    }
}

/// Split one TLV-encoded `(key, value)` pair; malformed parts become empty.
pub fn kv_pair_from_tlv(tlv: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let items = parse_tlv(tlv);
    let key = items.first().map(|b| b.to_vec()).unwrap_or_default();
    let val = items.get(1).map(|b| b.to_vec()).unwrap_or_default();
    (key, val)
}

/// First value of an upstream response header, if set.
pub fn resp_header(name: &str) -> Option<String> {
    let out = unsafe {
        ffi::read_two_phase(|op, oc| {
            ffi::resp_header_get(name.as_ptr() as i32, name.len() as i32, op, oc)
        })?
    };
    if out.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&out).into_owned())
    }
}

/// Set/replace an upstream response header (header_filter phase).
pub fn set_resp_header(name: &str, val: &str) {
    unsafe {
        ffi::resp_header_set(
            name.as_ptr() as i32,
            name.len() as i32,
            val.as_ptr() as i32,
            val.len() as i32,
        );
    }
}

/// Remove an upstream response header.
pub fn del_resp_header(name: &str) {
    unsafe { ffi::resp_header_del(name.as_ptr() as i32, name.len() as i32) };
}

/// Current response body chunk and whether it is the final one
/// (body_filter phase).
pub fn body_chunk() -> (Vec<u8>, bool) {
    let chunk =
        unsafe { ffi::read_two_phase(|op, oc| ffi::body_chunk(op, oc)) }.unwrap_or_default();
    let last = unsafe { ffi::body_is_last() } == 1;
    (chunk, last)
}

/// Plugin setting from `[plugins.settings.<name>]`; `None` when unset.
pub fn cfg(key: &str) -> Option<String> {
    let out = unsafe {
        ffi::read_two_phase(|op, oc| ffi::cfg_get(key.as_ptr() as i32, key.len() as i32, op, oc))?
    };
    if out.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&out).into_owned())
    }
}

/// Parse a TLV list (`u32 LE length | bytes` repeated). Stops at the first
/// malformed item (truncated payload or overflowing length).
pub fn parse_tlv(mut data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    while data.len() >= 4 {
        let (len_bytes, rest) = data.split_at(4);
        let len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
        if rest.len() < len {
            break;
        }
        out.push(&rest[..len]);
        data = &rest[len..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tlv(items: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for item in items {
            out.extend_from_slice(&(item.len() as u32).to_le_bytes());
            out.extend_from_slice(item);
        }
        out
    }

    #[test]
    fn parses_well_formed_tlv() {
        let data = tlv(&[b"web-1", b"10.0.0.1:8080", b"1"]);
        let items = parse_tlv(&data);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], b"web-1");
        assert_eq!(items[1], b"10.0.0.1:8080");
        assert_eq!(items[2], b"1");
    }

    #[test]
    fn tlv_len_is_little_endian() {
        let mut data = alloc::vec![0x02, 0x01, 0x00, 0x00]; // 258 LE
        data.extend_from_slice(&[9u8; 258]);
        let items = parse_tlv(&data);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].len(), 258);
    }

    #[test]
    fn stops_at_overflowing_length() {
        let mut data = tlv(&[b"ok"]);
        data.extend_from_slice(&[0xFF, 0x00, 0x00, 0x00]); // claims 255 bytes
        data.extend_from_slice(b"short");
        let items = parse_tlv(&data);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0], b"ok");
    }

    #[test]
    fn stops_at_truncated_tail() {
        let mut data = tlv(&[b"a"]);
        data.extend_from_slice(&[5, 0, 0, 0, b'x']); // len 5, payload 1 byte
        assert_eq!(parse_tlv(&data), alloc::vec![&b"a"[..]]);
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert!(parse_tlv(&[]).is_empty());
        assert!(parse_tlv(&[1, 0, 0]).is_empty());
    }

    #[test]
    fn decodes_peer_tlv() {
        let data = tlv(&[b"web-1", b"10.0.0.1:8080", b"1"]);
        let peer = peer_from_tlv(&data);
        assert_eq!(peer.name, "web-1");
        assert_eq!(peer.addr, "10.0.0.1:8080");
        assert!(peer.healthy);

        let unhealthy = peer_from_tlv(&tlv(&[b"web-2", b"10.0.0.2:8080", b"0"]));
        assert!(!unhealthy.healthy);
    }

    #[test]
    fn peer_from_malformed_tlv_is_empty() {
        let peer = peer_from_tlv(&[]);
        assert_eq!(peer.name, "");
        assert_eq!(peer.addr, "");
        assert!(!peer.healthy);
    }

    #[test]
    fn decodes_kv_pair_tlv() {
        let (k, v) = kv_pair_from_tlv(&tlv(&[b"aff:t1", b"3"]));
        assert_eq!(k, b"aff:t1");
        assert_eq!(v, b"3");

        let (k, v) = kv_pair_from_tlv(&[]);
        assert!(k.is_empty() && v.is_empty());
    }

    #[test]
    fn host_build_scan_is_empty() {
        // Stubs return a negative cursor, so iteration yields nothing and
        // Drop is a no-op. (Real behavior is exercised by the host runtime.)
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = kv_scan("aff:").collect();
        assert!(pairs.is_empty());
    }
}
