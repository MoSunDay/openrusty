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

/// Initial capacity offered to `kv_scan_next`: enough for the small pairs
/// plugins usually scan. Larger pairs are retried via the two-phase read
/// protocol instead of being lost.
const SCAN_BUF_LEN: usize = 256;

/// `kv_scan_next` return code: the cursor is invalid or expired (e.g.
/// reclaimed by the host after its TTL, or the per-plugin cursor cap was
/// hit at `kv_scan_begin`). Any other negative value is `-(required_len)`.
const SCAN_CURSOR_DEAD: i32 = -1;

/// Iterator over KV entries with a given prefix. Yields `(key, value)`
/// pairs. On host builds the stubbed scan is simply empty.
///
/// Follows the two-phase read contract of `kv_scan_next`
/// (docs/wasm-abi.md): a pair larger than the initial buffer is retried
/// with a grown buffer, so big values are returned complete rather than
/// truncated. Dropping the iterator ends the cursor, unless the host
/// already declared it dead.
pub struct KvScan {
    cursor: i32,
}

/// Start a scan of all KV keys starting with `prefix`.
pub fn kv_scan(prefix: &str) -> KvScan {
    let cursor = unsafe { ffi::kv_scan_begin(prefix.as_ptr() as i32, prefix.len() as i32) };
    KvScan { cursor }
}

/// Protocol core of [`KvScan::next`], parameterized over the `kv_scan_next`
/// import (`(cursor, out_ptr, out_cap) -> i32`) so it stays unit-testable
/// on host builds.
///
/// Per the ABI (docs/wasm-abi.md):
/// - `0`: exhausted -- iteration ends, the cursor stays valid (the host
///   already consumed it; [`scan_end_if_live`] still runs at `Drop`).
/// - [`SCAN_CURSOR_DEAD`]: the cursor is invalid/expired -- iteration ends
///   and the cursor is marked dead, so `Drop` does not end it again.
/// - any other negative value: `-(required capacity)` -- the same element
///   is retried through [`ffi::read_two_phase`] with a grown buffer while
///   the cursor stays valid (the host consumes the entry only after a
///   successful write, so the retry is lossless).
///
/// Returns the raw TLV pair bytes, or `None` when the scan ends (or a pair
/// can never be delivered, e.g. it exceeds the guest read cap; the cursor
/// then stays valid so `Drop` releases it).
fn scan_step(cursor: &mut i32, mut next: impl FnMut(i32, i32, i32) -> i32) -> Option<Vec<u8>> {
    if *cursor < 0 {
        return None;
    }
    let mut buf = alloc::vec![0u8; SCAN_BUF_LEN];
    let ret = next(*cursor, buf.as_mut_ptr() as i32, buf.len() as i32);
    if ret == 0 {
        // Exhausted: the host auto-removed the cursor; keep it for Drop.
        return None;
    }
    if ret == SCAN_CURSOR_DEAD {
        // Mark dead so Drop does not end an already-gone cursor.
        *cursor = SCAN_CURSOR_DEAD;
        return None;
    }
    if ret < 0 {
        // Two-phase retry of the same element (cursor stays valid).
        return unsafe { ffi::read_two_phase(|op, oc| next(*cursor, op, oc)) };
    }
    buf.truncate(ret as usize);
    Some(buf)
}

/// End a scan unless its cursor is already dead (see [`SCAN_CURSOR_DEAD`]):
/// the host reclaimed or never issued it, so ending it again would be a
/// pointless call at best.
fn scan_end_if_live(cursor: i32, end: impl FnOnce(i32)) {
    if cursor >= 0 {
        end(cursor);
    }
}

impl Iterator for KvScan {
    type Item = (Vec<u8>, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        scan_step(&mut self.cursor, |cursor, op, oc| unsafe {
            ffi::kv_scan_next(cursor, op, oc)
        })
        .map(|tlv| kv_pair_from_tlv(&tlv))
    }
}

impl Drop for KvScan {
    fn drop(&mut self) {
        scan_end_if_live(self.cursor, |cursor| unsafe { ffi::kv_scan_end(cursor) });
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
    use core::cell::{Cell, RefCell};

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

    // `scan_step` mocks: the mock host returns status codes / required
    // lengths but cannot write guest memory (the i32 out pointer truncates
    // the 64-bit host address), so assertions observe the capacity the
    // guest offers and the length the host reports, not buffer contents.

    /// Regression: a TLV pair larger than the initial 256-byte buffer must
    /// be retried with a grown buffer and delivered COMPLETE, not dropped
    /// as an invalid cursor (the old code stopped at any negative return).
    #[test]
    fn scan_retries_large_pair_with_grown_buffer() {
        let caps = RefCell::new(alloc::vec![]);
        let mut cursor = 7;
        let raw = scan_step(&mut cursor, |_c, _op, cap| {
            caps.borrow_mut().push(cap);
            if cap < 1040 {
                -1040 // 4+8 key + 4+1024 value = 1040 required bytes
            } else {
                1040
            }
        });
        assert_eq!(raw.map(|b| b.len()), Some(1040)); // complete, not 256
        // Probe at 256, then the read_two_phase retry re-probes at 256 and
        // succeeds on the grown buffer (the host consumes nothing until a
        // write succeeds, so the repeated probe is lossless).
        assert_eq!(*caps.borrow(), alloc::vec![256, 256, 1040]);
        // Cursor stays valid: Drop must still end it.
        assert_eq!(cursor, 7);
    }

    #[test]
    fn scan_continues_to_next_element_after_growth() {
        let calls = Cell::new(0);
        let caps = RefCell::new(alloc::vec![]);
        let mut cursor = 7;
        let mut next = |_c: i32, _op: i32, cap: i32| {
            caps.borrow_mut().push(cap);
            let call = calls.get() + 1;
            calls.set(call);
            match call {
                1 | 2 => -1040, // element 1 needs 1040 bytes (probe + retry)
                3 => 1040,      // element 1 delivered on the grown buffer
                4 => 16,        // element 2 fits the initial buffer
                _ => 0,         // exhausted
            }
        };
        let first = scan_step(&mut cursor, &mut next);
        let second = scan_step(&mut cursor, &mut next);
        let third = scan_step(&mut cursor, &mut next);
        assert_eq!(first.map(|b| b.len()), Some(1040));
        assert_eq!(second.map(|b| b.len()), Some(16));
        assert!(third.is_none());
        assert_eq!(*caps.borrow(), alloc::vec![256, 256, 1040, 256, 256]);
        assert_eq!(cursor, 7);
        scan_end_if_live(cursor, |c| assert_eq!(c, 7));
    }

    /// Regression: `-1` is the dead-cursor marker (not a 1-byte required
    /// length) -- iteration stops and the cursor is NOT ended again.
    #[test]
    fn scan_dead_cursor_stops_and_blocks_scan_end() {
        let calls = Cell::new(0);
        let mut cursor = 7;
        let raw = scan_step(&mut cursor, |_c, _op, _cap| {
            calls.set(calls.get() + 1);
            SCAN_CURSOR_DEAD
        });
        assert!(raw.is_none());
        assert_eq!(calls.get(), 1);
        assert_eq!(cursor, SCAN_CURSOR_DEAD); // marked dead

        let ended = Cell::new(0);
        scan_end_if_live(cursor, |c| ended.set(c));
        assert_eq!(ended.get(), 0); // dead cursor is never ended
    }

    #[test]
    fn scan_exhaustion_keeps_cursor_for_drop() {
        let mut cursor = 9;
        let raw = scan_step(&mut cursor, |_c, _op, _cap| 0);
        assert!(raw.is_none());
        assert_eq!(cursor, 9); // still live: Drop ends it
        let ended = Cell::new(0);
        scan_end_if_live(cursor, |c| ended.set(c));
        assert_eq!(ended.get(), 9);
    }

    /// A pair that can never be delivered (e.g. beyond the guest read cap)
    /// ends iteration but must NOT strand the host cursor: Drop releases it.
    #[test]
    fn scan_undeliverable_pair_releases_cursor_at_drop() {
        let caps = RefCell::new(alloc::vec![]);
        let mut cursor = 5;
        let raw = scan_step(&mut cursor, |_c, _op, cap| {
            caps.borrow_mut().push(cap);
            -(crate::alloc_heap::HEAP_SIZE as i64 + 1) as i32 // beyond the arena
        });
        assert!(raw.is_none());
        assert_eq!(*caps.borrow(), alloc::vec![256, 256]); // probe + retry
        assert_eq!(cursor, 5); // still valid -> Drop ends it
        let ended = Cell::new(0);
        scan_end_if_live(cursor, |c| ended.set(c));
        assert_eq!(ended.get(), 5);
    }

    #[test]
    fn scan_end_if_live_ends_valid_cursor() {
        let ended = Cell::new(0);
        scan_end_if_live(12, |c| ended.set(c));
        assert_eq!(ended.get(), 12);
    }
}
