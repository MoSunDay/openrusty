//! Per-plugin shared host state, keyed by plugin name and surviving hot
//! reloads: a KV store with TTLs plus snapshot-based scan cursors.

use dashmap::DashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch (best effort; 0 on a broken clock).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// True once `now_ms` reaches `expires_at`. `u64::MAX` means "never".
pub fn is_expired(expires_at: u64, now_ms: u64) -> bool {
    expires_at != u64::MAX && now_ms >= expires_at
}

#[derive(Debug)]
struct KvEntry {
    value: Vec<u8>,
    /// Absolute expiry in epoch millis; `u64::MAX` = no expiry.
    expires_at: u64,
}

#[derive(Debug)]
struct ScanCursor {
    /// Frozen snapshot taken at scan_begin time.
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    pos: usize,
}

/// Trigger a cheap maintenance sweep after this many scan operations.
const SWEEP_EVERY: u32 = 256;

/// Shared, reload-surviving state of one plugin.
pub struct HostState {
    name: String,
    kv: DashMap<Vec<u8>, KvEntry>,
    cursors: DashMap<u32, ScanCursor>,
    next_cursor: AtomicU32,
    errors: AtomicU64,
    /// Scan ops accumulated since the last opportunistic sweep.
    stale_ops: AtomicU32,
}

impl HostState {
    pub fn new(plugin_name: String) -> Self {
        HostState {
            name: plugin_name,
            kv: DashMap::new(),
            cursors: DashMap::new(),
            next_cursor: AtomicU32::new(0),
            errors: AtomicU64::new(0),
            stale_ops: AtomicU32::new(0),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Value for `key`, or `None` if missing or expired (lazy expiry).
    pub fn kv_get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let now = now_ms();
        let live = self
            .kv
            .get(key)
            .and_then(|e| (!is_expired(e.expires_at, now)).then(|| e.value.clone()));
        if live.is_none() {
            // Drop a stale entry if one exists (avoids clobbering a fresh set).
            self.kv.remove_if(key, |_, e| is_expired(e.expires_at, now));
        }
        live
    }

    /// Insert/overwrite. `ttl_ms <= 0` means no expiry.
    pub fn kv_set(&self, key: &[u8], val: Vec<u8>, ttl_ms: i64) {
        let expires_at = if ttl_ms <= 0 {
            u64::MAX
        } else {
            now_ms().saturating_add(ttl_ms as u64)
        };
        self.kv.insert(
            key.to_vec(),
            KvEntry {
                value: val,
                expires_at,
            },
        );
    }

    /// Delete `key`; true if an entry was removed.
    pub fn kv_del(&self, key: &[u8]) -> bool {
        self.kv.remove(key).is_some()
    }

    /// Snapshot of live (non-expired) entries whose key starts with
    /// `prefix`, sorted by key.
    pub fn kv_scan(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let now = now_ms();
        let mut out: Vec<(Vec<u8>, Vec<u8>)> = self
            .kv
            .iter()
            .filter(|e| e.key().starts_with(prefix) && !is_expired(e.value().expires_at, now))
            .map(|e| (e.key().clone(), e.value().value.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Start a snapshot scan over keys starting with `prefix`; returns a
    /// cursor id. Periodically triggers a cheap maintenance sweep.
    pub fn scan_begin(&self, prefix: &[u8]) -> u32 {
        if self.stale_ops.fetch_add(1, Ordering::Relaxed) >= SWEEP_EVERY {
            self.stale_ops.store(0, Ordering::Relaxed);
            self.sweep(now_ms());
        }
        let cursor = self.next_cursor.fetch_add(1, Ordering::Relaxed);
        self.cursors.insert(
            cursor,
            ScanCursor {
                entries: self.kv_scan(prefix),
                pos: 0,
            },
        );
        cursor
    }

    /// Current cursor entry without advancing it. `None` = exhausted or
    /// invalid cursor.
    pub fn scan_peek(&self, cursor: u32) -> Option<(Vec<u8>, Vec<u8>)> {
        self.cursors
            .get(&cursor)
            .and_then(|c| c.entries.get(c.pos).cloned())
    }

    /// Advance a cursor by one; removes it once exhausted.
    pub fn scan_advance(&self, cursor: u32) {
        let exhausted = self
            .cursors
            .get_mut(&cursor)
            .map(|mut c| {
                c.pos += 1;
                c.pos >= c.entries.len()
            })
            .unwrap_or(false);
        if exhausted {
            self.cursors.remove(&cursor);
        }
    }

    /// True while the cursor id refers to a live scan.
    pub fn scan_is_valid(&self, cursor: u32) -> bool {
        self.cursors.contains_key(&cursor)
    }

    /// Consume the next cursor entry. `None` = exhausted or invalid cursor;
    /// an exhausted cursor is removed automatically.
    pub fn scan_next(&self, cursor: u32) -> Option<(Vec<u8>, Vec<u8>)> {
        let item = self.scan_peek(cursor);
        if item.is_some() {
            self.scan_advance(cursor);
        } else if self.scan_is_valid(cursor) {
            self.cursors.remove(&cursor);
        }
        item
    }

    /// Explicitly drop a cursor.
    pub fn scan_end(&self, cursor: u32) {
        self.cursors.remove(&cursor);
    }

    /// Drop expired KV entries and fully-consumed cursors; returns the
    /// number of items removed.
    pub fn sweep(&self, now_ms: u64) -> usize {
        let mut removed = 0usize;
        self.kv.retain(|_, e| {
            if is_expired(e.expires_at, now_ms) {
                removed += 1;
                false
            } else {
                true
            }
        });
        self.cursors.retain(|_, c| {
            if c.pos >= c.entries.len() {
                removed += 1;
                false
            } else {
                true
            }
        });
        removed
    }

    pub fn record_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn error_count(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    fn state() -> HostState {
        HostState::new("test".into())
    }

    #[test]
    fn set_get_del() {
        let s = state();
        assert_eq!(s.kv_get(b"a"), None);
        s.kv_set(b"a", b"1".to_vec(), 0);
        assert_eq!(s.kv_get(b"a"), Some(b"1".to_vec()));
        s.kv_set(b"a", b"2".to_vec(), 0);
        assert_eq!(s.kv_get(b"a"), Some(b"2".to_vec()));
        assert!(s.kv_del(b"a"));
        assert!(!s.kv_del(b"a"));
        assert_eq!(s.kv_get(b"a"), None);
    }

    #[test]
    fn expiry_semantics() {
        assert!(!is_expired(u64::MAX, u64::MAX));
        assert!(!is_expired(10, 9));
        assert!(is_expired(10, 10));
        assert!(is_expired(10, 11));

        let s = state();
        s.kv_set(b"t", b"v".to_vec(), 1);
        let expires_at = now_ms() + 1;
        // Live before expiry...
        assert_eq!(s.kv_get(b"t"), Some(b"v".to_vec()));
        // ...and sweep at an explicit later time removes it.
        assert_eq!(s.sweep(expires_at + 1), 1);
        assert_eq!(s.kv_get(b"t"), None);

        // Real-clock lazy expiry.
        s.kv_set(b"t2", b"v".to_vec(), 1);
        thread::sleep(Duration::from_millis(5));
        assert_eq!(s.kv_get(b"t2"), None);
    }

    #[test]
    fn scan_prefix_and_order() {
        let s = state();
        s.kv_set(b"a:2", b"two".to_vec(), 0);
        s.kv_set(b"a:1", b"one".to_vec(), 0);
        s.kv_set(b"b:1", b"bee".to_vec(), 0);
        let got = s.kv_scan(b"a:");
        assert_eq!(
            got,
            vec![
                (b"a:1".to_vec(), b"one".to_vec()),
                (b"a:2".to_vec(), b"two".to_vec())
            ]
        );
        assert_eq!(s.kv_scan(b"z").len(), 0);
    }

    #[test]
    fn cursor_iteration_and_autoremove() {
        let s = state();
        s.kv_set(b"p:1", b"1".to_vec(), 0);
        s.kv_set(b"p:2", b"2".to_vec(), 0);
        let c = s.scan_begin(b"p:");
        assert!(s.scan_is_valid(c));
        assert_eq!(s.scan_next(c), Some((b"p:1".to_vec(), b"1".to_vec())));
        assert_eq!(s.scan_next(c), Some((b"p:2".to_vec(), b"2".to_vec())));
        assert_eq!(s.scan_next(c), None); // exhausted
        assert!(!s.scan_is_valid(c)); // auto-removed
        assert_eq!(s.scan_next(c), None); // unknown cursor stays None
    }

    #[test]
    fn unknown_cursor_and_scan_end() {
        let s = state();
        assert_eq!(s.scan_next(12345), None);
        s.kv_set(b"k", b"v".to_vec(), 0);
        let c = s.scan_begin(b"");
        s.scan_end(c);
        assert!(!s.scan_is_valid(c));
        assert_eq!(s.scan_next(c), None);
    }

    #[test]
    fn sweep_removes_dead_cursors_and_expired_keys() {
        let s = state();
        s.kv_set(b"k", b"v".to_vec(), 1); // expires almost immediately
                                          // Cursor with an empty snapshot: consumable without any advance.
        let c = s.scan_begin(b"nomatch:");
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(s.sweep(now_ms()), 2); // expired key + empty cursor
        assert!(!s.scan_is_valid(c));
        assert_eq!(s.kv_get(b"k"), None);
    }

    #[test]
    fn error_counter() {
        let s = state();
        assert_eq!(s.error_count(), 0);
        s.record_error();
        s.record_error();
        assert_eq!(s.error_count(), 2);
    }
}
