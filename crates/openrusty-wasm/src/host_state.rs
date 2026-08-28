//! Per-plugin shared host state, keyed by plugin name and surviving hot
//! reloads: a KV store with TTLs plus snapshot-based scan cursors.

use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
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
    /// `now_ms` at scan_begin; cursors older than [`CURSOR_TTL_MS`] are
    /// reclaimed by sweep (guests that abandon a scan must not leak the
    /// snapshot forever).
    created_at: u64,
}

/// Trigger a cheap maintenance sweep after this many scan operations.
const SWEEP_EVERY: u32 = 256;

/// Snapshot cursors older than this are reclaimed by
/// [`HostState::sweep`] even if the guest never called `kv_scan_end`.
const CURSOR_TTL_MS: u64 = 60_000;

/// Maximum live snapshot cursors per plugin. `kv_scan_begin` refuses with
/// the invalid-cursor marker (`-1`) once this many cursors are live, so a
/// plugin cannot accumulate unbounded snapshots.
const MAX_CURSORS: usize = 64;

/// Maximum size of a single KV value. A `kv_set` above this is refused
/// with the failure marker (`-1`) so a plugin cannot hand the host one
/// unbounded allocation.
const KV_MAX_VALUE_BYTES: usize = 64 * 1024;

/// Maximum live KV footprint per plugin: the sum of key+value bytes over
/// all entries currently held. A `kv_set` that would push the total past
/// this is refused with the failure marker (`-1`), so a plugin cannot OOM
/// the host. Bytes are released when an entry is deleted or overwritten,
/// and again when TTL expiry drops it (via [`HostState::sweep`] or the
/// lazy expiry in [`HostState::kv_get`]).
const KV_MAX_TOTAL_BYTES: usize = 1024 * 1024;

/// Pure admission decision for `kv_set`: reject when the value alone
/// exceeds [`KV_MAX_VALUE_BYTES`] or when `key_len + val_len` bytes on
/// top of `total_bytes` (the plugin's live total *after* whatever an
/// overwritten entry releases) would exceed [`KV_MAX_TOTAL_BYTES`].
fn kv_set_verdict(key_len: usize, val_len: usize, total_bytes: usize) -> bool {
    val_len <= KV_MAX_VALUE_BYTES
        && total_bytes.saturating_add(key_len + val_len) <= KV_MAX_TOTAL_BYTES
}

/// Shared, reload-surviving state of one plugin.
pub struct HostState {
    name: String,
    kv: DashMap<Vec<u8>, KvEntry>,
    /// Live scan cursors. A plain mutex keeps the cursor cap exact (the
    /// count and the insert are decided under one lock); cursor ops are
    /// rare and never re-enter the host.
    cursors: Mutex<HashMap<u32, ScanCursor>>,
    next_cursor: AtomicU32,
    errors: AtomicU64,
    /// Per-kind error counts (`openrusty_plugin_errors_total{kind=...}`),
    /// keyed by kind label. Kind values mirror the server-side constants
    /// in `openrusty-server/src/metrics.rs` (KIND_TRAP/KIND_TIMEOUT/
    /// KIND_BAD_CODE): "trap", "timeout", "bad_code".
    error_kinds: Mutex<HashMap<String, u64>>,
    /// Scan ops accumulated since the last opportunistic sweep.
    stale_ops: AtomicU32,
    /// Live KV footprint in bytes (sum of key+value over entries currently
    /// held), enforced against [`KV_MAX_TOTAL_BYTES`]. Every mutation
    /// applies the exact delta of the entry it inserted/removed, so this
    /// stays the sum of the map contents; admission only reads it, which
    /// makes a concurrent write's verdict mildly conservative at worst.
    total_bytes: AtomicUsize,
}

impl HostState {
    pub fn new(plugin_name: String) -> Self {
        HostState {
            name: plugin_name,
            kv: DashMap::new(),
            cursors: Mutex::new(HashMap::new()),
            next_cursor: AtomicU32::new(0),
            errors: AtomicU64::new(0),
            error_kinds: Mutex::new(HashMap::new()),
            stale_ops: AtomicU32::new(0),
            total_bytes: AtomicUsize::new(0),
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
            // Drop a stale entry if one exists (avoids clobbering a fresh set);
            // its bytes must leave the quota total with it.
            if let Some((k, e)) = self.kv.remove_if(key, |_, e| is_expired(e.expires_at, now)) {
                self.account(0, k.len() + e.value.len());
            }
        }
        live
    }

    /// Insert/overwrite. `ttl_ms <= 0` means no expiry. Returns `false` -
    /// leaving the store untouched - when the write is refused by the KV
    /// quotas: the value over [`KV_MAX_VALUE_BYTES`], or the plugin's live
    /// total over [`KV_MAX_TOTAL_BYTES`].
    pub fn kv_set(&self, key: &[u8], val: Vec<u8>, ttl_ms: i64) -> bool {
        // An overwrite releases the displaced entry's bytes; read the old
        // entry up front so the admission verdict credits that. A concurrent
        // mutation in between can only make the verdict conservative (it
        // decides on a slightly stale total) - the accounting below stays
        // exact because the delta comes from the entry actually displaced.
        let released = self
            .kv
            .get(key)
            .map(|e| key.len() + e.value.len())
            .unwrap_or(0);
        let total = self.total_bytes.load(Ordering::Relaxed);
        if !kv_set_verdict(key.len(), val.len(), total.saturating_sub(released)) {
            return false;
        }
        let expires_at = if ttl_ms <= 0 {
            u64::MAX
        } else {
            now_ms().saturating_add(ttl_ms as u64)
        };
        let added = key.len() + val.len();
        let displaced = self.kv.insert(
            key.to_vec(),
            KvEntry {
                value: val,
                expires_at,
            },
        );
        let removed = displaced.map_or(0, |e| key.len() + e.value.len());
        self.account(added, removed);
        true
    }

    /// Delete `key`; true if an entry was removed.
    pub fn kv_del(&self, key: &[u8]) -> bool {
        match self.kv.remove(key) {
            Some((k, e)) => {
                self.account(0, k.len() + e.value.len());
                true
            }
            None => false,
        }
    }

    /// Move the live-bytes total by `added - removed`. Both sides are the
    /// byte sizes of entries actually inserted/removed, never projections,
    /// so the total cannot drift from the map contents.
    fn account(&self, added: usize, removed: usize) {
        if added > removed {
            self.total_bytes
                .fetch_add(added - removed, Ordering::Relaxed);
        } else if removed > added {
            self.total_bytes
                .fetch_sub(removed - added, Ordering::Relaxed);
        }
    }

    /// Sum of key+value bytes over the entries currently held - the number
    /// [`KV_MAX_TOTAL_BYTES`] is enforced against.
    pub fn kv_total_bytes(&self) -> usize {
        self.total_bytes.load(Ordering::Relaxed)
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
    /// cursor id, or `-1` (the invalid-cursor marker guests already know)
    /// when [`MAX_CURSORS`] cursors are already live for this plugin.
    /// Periodically triggers a cheap maintenance sweep.
    pub fn scan_begin(&self, prefix: &[u8]) -> i32 {
        if self.stale_ops.fetch_add(1, Ordering::Relaxed) >= SWEEP_EVERY {
            self.stale_ops.store(0, Ordering::Relaxed);
            self.sweep(now_ms());
        }
        // Snapshot is taken before taking the cursor lock (kv and cursors
        // are independent maps; there is no lock-order inversion).
        let entries = self.kv_scan(prefix);
        let mut cursors = self.lock_cursors();
        if cursors.len() >= MAX_CURSORS {
            return -1;
        }
        let id = self.next_cursor.fetch_add(1, Ordering::Relaxed);
        let Ok(id) = i32::try_from(id) else {
            return -1; // unreachable in practice (u32 ids above i32::MAX)
        };
        cursors.insert(
            id as u32,
            ScanCursor {
                entries,
                pos: 0,
                created_at: now_ms(),
            },
        );
        id
    }

    /// Current cursor entry without advancing it. `None` = exhausted or
    /// invalid cursor.
    pub fn scan_peek(&self, cursor: u32) -> Option<(Vec<u8>, Vec<u8>)> {
        self.lock_cursors()
            .get(&cursor)
            .and_then(|c| c.entries.get(c.pos).cloned())
    }

    /// Advance a cursor by one; removes it once exhausted.
    pub fn scan_advance(&self, cursor: u32) {
        let mut cursors = self.lock_cursors();
        let exhausted = cursors
            .get_mut(&cursor)
            .map(|c| {
                c.pos += 1;
                c.pos >= c.entries.len()
            })
            .unwrap_or(false);
        if exhausted {
            cursors.remove(&cursor);
        }
    }

    /// True while the cursor id refers to a live scan.
    pub fn scan_is_valid(&self, cursor: u32) -> bool {
        self.lock_cursors().contains_key(&cursor)
    }

    /// Consume the next cursor entry. `None` = exhausted or invalid cursor;
    /// an exhausted cursor is removed automatically.
    pub fn scan_next(&self, cursor: u32) -> Option<(Vec<u8>, Vec<u8>)> {
        let mut cursors = self.lock_cursors();
        let item = cursors
            .get(&cursor)
            .and_then(|c| c.entries.get(c.pos).cloned());
        if item.is_some() {
            if let Some(c) = cursors.get_mut(&cursor) {
                c.pos += 1;
            }
            if cursors
                .get(&cursor)
                .map(|c| c.pos >= c.entries.len())
                .unwrap_or(true)
            {
                cursors.remove(&cursor);
            }
        } else if cursors.contains_key(&cursor) {
            cursors.remove(&cursor); // exhausted: auto-remove
        }
        item
    }

    /// Explicitly drop a cursor.
    pub fn scan_end(&self, cursor: u32) {
        self.lock_cursors().remove(&cursor);
    }

    /// Drop expired KV entries, fully-consumed cursors and cursors older
    /// than [`CURSOR_TTL_MS`] (abandoned scans); returns the number of
    /// items removed.
    pub fn sweep(&self, now_ms: u64) -> usize {
        let mut removed = 0usize;
        let mut expired_bytes = 0usize;
        self.kv.retain(|k, e| {
            if is_expired(e.expires_at, now_ms) {
                removed += 1;
                expired_bytes += k.len() + e.value.len();
                false
            } else {
                true
            }
        });
        self.account(0, expired_bytes);
        self.lock_cursors().retain(|_, c| {
            let expired = now_ms.saturating_sub(c.created_at) >= CURSOR_TTL_MS;
            if c.pos >= c.entries.len() || expired {
                removed += 1;
                false
            } else {
                true
            }
        });
        removed
    }

    /// Number of live snapshot cursors (bounded by [`MAX_CURSORS`]).
    pub fn cursors_len(&self) -> usize {
        self.lock_cursors().len()
    }

    fn lock_cursors(&self) -> MutexGuard<'_, HashMap<u32, ScanCursor>> {
        self.cursors.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Count one plugin error: the aggregate counter (consumed by
    /// `/openrusty/status`) and the per-kind breakdown (consumed by the
    /// Prometheus metrics endpoint). `kind` must match the server-side
    /// label values in `openrusty-server/src/metrics.rs`: "trap",
    /// "timeout", "bad_code".
    pub fn record_error(&self, kind: &str) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        let mut kinds = self.error_kinds.lock().unwrap();
        *kinds.entry(kind.to_string()).or_insert(0) += 1;
    }

    pub fn error_count(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// Per-kind error counts as `(kind, count)`, sorted by kind name.
    pub fn error_kinds(&self) -> Vec<(String, u64)> {
        let kinds = self.error_kinds.lock().unwrap();
        let mut out: Vec<(String, u64)> = kinds.iter().map(|(k, n)| (k.clone(), *n)).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Number of live KV entries currently held (expired entries are
    /// removed lazily by [`sweep`](Self::sweep)).
    pub fn kv_len(&self) -> usize {
        self.kv.len()
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

    /// `scan_begin` returns an i32 for the guest ABI; host-side cursor
    /// methods take the raw u32 id.
    fn id(c: i32) -> u32 {
        u32::try_from(c).expect("live cursor ids are non-negative")
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
        // Deterministic expiry: inject the deadline directly so no clock
        // race between set and get can flip the "live" assertion.
        s.kv_set(b"t", b"v".to_vec(), 0);
        let expires_at = now_ms() + 10_000;
        if let Some(mut e) = s.kv.get_mut(&b"t"[..]) {
            e.expires_at = expires_at;
        }
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
        assert!(s.scan_is_valid(id(c)));
        assert_eq!(s.scan_next(id(c)), Some((b"p:1".to_vec(), b"1".to_vec())));
        assert_eq!(s.scan_next(id(c)), Some((b"p:2".to_vec(), b"2".to_vec())));
        assert_eq!(s.scan_next(id(c)), None); // exhausted
        assert!(!s.scan_is_valid(id(c))); // auto-removed
        assert_eq!(s.scan_next(id(c)), None); // unknown cursor stays None
    }

    #[test]
    fn unknown_cursor_and_scan_end() {
        let s = state();
        assert_eq!(s.scan_next(12345), None);
        s.kv_set(b"k", b"v".to_vec(), 0);
        let c = s.scan_begin(b"");
        s.scan_end(id(c));
        assert!(!s.scan_is_valid(id(c)));
        assert_eq!(s.scan_next(id(c)), None);
    }

    #[test]
    fn sweep_removes_dead_cursors_and_expired_keys() {
        let s = state();
        s.kv_set(b"k", b"v".to_vec(), 1); // expires almost immediately
                                          // Cursor with an empty snapshot: consumable without any advance.
        let c = s.scan_begin(b"nomatch:");
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(s.sweep(now_ms()), 2); // expired key + empty cursor
        assert!(!s.scan_is_valid(id(c)));
        assert_eq!(s.kv_get(b"k"), None);
    }

    /// Regression: a guest that abandons a scan (no `kv_scan_end`) must
    /// not leak the cursor and its snapshot forever.
    #[test]
    fn abandoned_cursor_is_reclaimed_after_ttl() {
        let s = state();
        s.kv_set(b"a", b"1".to_vec(), 0);
        let c = s.scan_begin(b"");
        assert!(s.scan_is_valid(id(c)));
        assert_eq!(s.cursors_len(), 1);
        // The snapshot is alive and iterating still works before expiry.
        assert_eq!(s.scan_peek(id(c)), Some((b"a".to_vec(), b"1".to_vec())));

        // Age the cursor past the TTL (injected, no sleeping) and sweep.
        s.cursors.lock().unwrap().get_mut(&id(c)).unwrap().created_at = 0;
        let reclaimed = s.sweep(CURSOR_TTL_MS + 1);
        assert_eq!(reclaimed, 1);
        assert!(!s.scan_is_valid(id(c)));
        assert_eq!(s.cursors_len(), 0);
        assert_eq!(s.scan_peek(id(c)), None);

        // A fresh scan keeps working and gets a fresh cursor id.
        let c2 = s.scan_begin(b"");
        assert_ne!(c2, c);
        assert!(s.scan_is_valid(id(c2)));
    }

    /// Regression: live cursors per plugin are capped; the cap frees up
    /// again once cursors are ended.
    #[test]
    fn cursor_cap_refuses_new_scans_until_freed() {
        let s = state();
        s.kv_set(b"k", b"v".to_vec(), 0);
        let mut ids = Vec::new();
        for _ in 0..MAX_CURSORS {
            let c = s.scan_begin(b"");
            assert!(c >= 0, "cap not reached yet, begin must succeed");
            ids.push(id(c));
        }
        assert_eq!(s.cursors_len(), MAX_CURSORS);
        // At the cap the invalid-cursor marker (-1) comes back.
        assert_eq!(s.scan_begin(b""), -1);
        assert_eq!(s.cursors_len(), MAX_CURSORS);

        // Ending cursors frees slots again.
        s.scan_end(ids[0]);
        assert_eq!(s.cursors_len(), MAX_CURSORS - 1);
        let c = s.scan_begin(b"");
        assert!(c >= 0, "a freed slot must accept a new scan");
    }

    /// Cursor ids handed to guests stay within the i32 ABI (negative means
    /// "no cursor"), so a valid id is always non-negative and never -1.
    #[test]
    fn scan_begin_never_returns_negative_for_a_live_cursor() {
        let s = state();
        for _ in 0..(MAX_CURSORS + 8) {
            let c = s.scan_begin(b"");
            if c < 0 {
                assert_eq!(c, -1, "the only negative marker is -1");
                break;
            }
            s.scan_end(id(c));
        }
    }

    #[test]
    fn error_counter() {
        let s = state();
        assert_eq!(s.error_count(), 0);
        s.record_error("trap");
        s.record_error("trap");
        assert_eq!(s.error_count(), 2);
    }

    #[test]
    fn error_kinds_sorted_and_kv_len_counts_entries() {
        let s = state();
        s.kv_set(b"a", b"1".to_vec(), 0);
        s.kv_set(b"b", b"2".to_vec(), 0);
        assert_eq!(s.kv_len(), 2);
        s.record_error("bad_code");
        s.record_error("timeout");
        s.record_error("trap");
        s.record_error("trap");
        assert_eq!(
            s.error_kinds(),
            vec![
                ("bad_code".to_string(), 1),
                ("timeout".to_string(), 1),
                ("trap".to_string(), 2),
            ]
        );
        // Aggregate counter still counts every kind.
        assert_eq!(s.error_count(), 4);
    }

    #[test]
    fn kv_set_verdict_boundaries() {
        assert!(kv_set_verdict(0, 0, 0));
        // The value cap is absolute, regardless of the total.
        assert!(kv_set_verdict(1, KV_MAX_VALUE_BYTES, 0));
        assert!(!kv_set_verdict(1, KV_MAX_VALUE_BYTES + 1, 0));
        // The total boundary sits exactly at key+value on top of the fill.
        let fill = KV_MAX_TOTAL_BYTES - KV_MAX_VALUE_BYTES - 1;
        assert!(kv_set_verdict(1, KV_MAX_VALUE_BYTES, fill));
        assert!(!kv_set_verdict(1, KV_MAX_VALUE_BYTES, fill + 1));
        // Saturating math must not panic on a nonsensical total.
        assert!(!kv_set_verdict(0, KV_MAX_VALUE_BYTES, usize::MAX));
    }

    /// Fill `s` with the 15 full-cap values (3-byte keys `k00`..`k14`)
    /// that fit below [`KV_MAX_TOTAL_BYTES`]; returns the remaining
    /// headroom, too small for another full-cap value on a fresh key.
    fn fill_to_near_quota(s: &HostState) -> usize {
        let entry = 3 + KV_MAX_VALUE_BYTES;
        for i in 0..15u32 {
            let key = format!("k{i:02}");
            assert!(s.kv_set(key.as_bytes(), vec![0u8; KV_MAX_VALUE_BYTES], 0));
        }
        assert_eq!(s.kv_total_bytes(), 15 * entry);
        KV_MAX_TOTAL_BYTES - 15 * entry
    }

    #[test]
    fn value_over_cap_is_rejected_without_mutating_state() {
        let s = state();
        assert!(s.kv_set(b"ok", vec![0u8; KV_MAX_VALUE_BYTES], 0));
        let total = s.kv_total_bytes();
        let entries = s.kv_len();

        // One byte over the per-value cap: refused, and the refused write
        // must not partially mutate the store.
        assert!(!s.kv_set(b"big", vec![0u8; KV_MAX_VALUE_BYTES + 1], 0));
        assert_eq!(s.kv_get(b"big"), None);
        assert_eq!(s.kv_total_bytes(), total);
        assert_eq!(s.kv_len(), entries);
    }

    #[test]
    fn total_quota_full_rejects_further_sets() {
        let s = state();
        let headroom = fill_to_near_quota(&s);

        // A fresh write larger than the remaining headroom is refused and
        // leaves the store untouched.
        assert!(!s.kv_set(b"extra", vec![0u8; headroom], 0));
        assert_eq!(s.kv_get(b"extra"), None);
        assert_eq!(s.kv_total_bytes(), KV_MAX_TOTAL_BYTES - headroom);

        // The headroom itself fits exactly ...
        assert!(s.kv_set(b"fill", vec![0u8; headroom - 4], 0));
        assert_eq!(s.kv_total_bytes(), KV_MAX_TOTAL_BYTES);
        // ... and with the total quota reached, nothing else gets in.
        assert!(!s.kv_set(b"x", b"y".to_vec(), 0));

        // An overwrite still fits, because the displaced bytes are
        // released before the admission decision.
        assert!(s.kv_set(b"k00", b"small".to_vec(), 0));
        assert_eq!(s.kv_get(b"k00"), Some(b"small".to_vec()));
        assert_eq!(
            s.kv_total_bytes(),
            KV_MAX_TOTAL_BYTES - KV_MAX_VALUE_BYTES + 5
        );
    }

    #[test]
    fn overwrite_accounts_the_size_delta() {
        let s = state();
        s.kv_set(b"k", vec![0u8; 100], 0);
        assert_eq!(s.kv_total_bytes(), 101); // key + value

        // Growing: the total moves by the delta, not by the new size alone.
        s.kv_set(b"k", vec![0u8; 300], 0);
        assert_eq!(s.kv_total_bytes(), 301);

        // Shrinking works the other way.
        s.kv_set(b"k", b"v".to_vec(), 0);
        assert_eq!(s.kv_total_bytes(), 2);

        // The total stays exact across keys.
        s.kv_set(b"other", b"xy".to_vec(), 0);
        assert_eq!(s.kv_total_bytes(), 2 + 7);
    }

    #[test]
    fn kv_del_frees_quota_for_a_new_set() {
        let s = state();
        let entry = 3 + KV_MAX_VALUE_BYTES;
        fill_to_near_quota(&s);
        // No headroom for another full-cap value on a fresh key.
        assert!(!s.kv_set(b"other", vec![0u8; KV_MAX_VALUE_BYTES], 0));

        // Deleting releases the bytes, so the refused write fits now.
        assert!(s.kv_del(b"k00"));
        assert_eq!(s.kv_total_bytes(), 14 * entry);
        assert!(s.kv_set(b"other", vec![0u8; KV_MAX_VALUE_BYTES], 0));
        assert_eq!(s.kv_total_bytes(), 14 * entry + 5 + KV_MAX_VALUE_BYTES);

        // Deleting a missing key changes nothing.
        assert!(!s.kv_del(b"k00"));
        assert_eq!(s.kv_total_bytes(), 14 * entry + 5 + KV_MAX_VALUE_BYTES);
    }

    #[test]
    fn sweep_of_expired_entry_frees_quota() {
        let s = state();
        s.kv_set(b"t", vec![0u8; 4_096], 1); // expires almost immediately
        assert_eq!(s.kv_total_bytes(), 1 + 4_096);
        // While the (tiny) entry is held, a full-quota write is refused.
        assert!(!s.kv_set(b"next", vec![0u8; KV_MAX_TOTAL_BYTES], 0));

        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(s.sweep(now_ms()), 1);
        assert_eq!(s.kv_total_bytes(), 0);

        // Lazy expiry in kv_get releases the bytes just the same.
        s.kv_set(b"t2", vec![0u8; 4_096], 1);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(s.kv_get(b"t2"), None);
        assert_eq!(s.kv_total_bytes(), 0);
    }
}
