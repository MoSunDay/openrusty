//! The async watch loop: LIST -> replace -> watch -> debounce -> callback.
//!
//! This is the only stateful driver in the crate; every decision inside it
//! delegates to the pure pieces ([`crate::snapshot`], [`crate::model`]).
//! One cycle:
//!
//! 1. `GET path` and [`replace_from_list`] the items (full-replacement
//!    semantics, resource version from the list envelope);
//! 2. open [`Client::watch_raw`](crate::client::Client::watch_raw) pinned
//!    at that resource version and fold each newline-delimited JSON event
//!    into the snapshot with [`apply_event`](crate::snapshot::apply_event);
//! 3. debounce: the callback fires once the stream has been quiet for
//!    [`WatchOptions::debounce`], so bursts of events collapse into one
//!    snapshot hand-over;
//! 4. on 410 Gone, stream end (clean EOF or error), malformed line or
//!    transport error: reconnect, i.e. go back to step 1 with exponential
//!    backoff (100ms, x2, capped at 30s; any successful LIST resets it);
//! 5. resync: every [`WatchOptions::resync`] the cycle restarts at step 1
//!    so a silently dead (half-open) stream cannot serve a stale snapshot
//!    longer than one interval; a re-list at an unchanged resource version
//!    skips the hand-over (quiet clusters do not churn generations).
//!
//! # Stale-serve semantics (explicit, not a degradation)
//!
//! The callback always receives an immutable snapshot and it is only ever
//! replaced by a fresher one - never cleared. While the API server is
//! unreachable the loop keeps retrying and the caller keeps serving the
//! last snapshot it was handed; that is the designed contract, so the
//! gateway does not need a "config lost" mode.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use http_body_util::BodyExt;
use serde::de::DeserializeOwned;
use tokio::time::Sleep;

use crate::client::{watch_path, ByteStream, Client};
use crate::error::{K8sError, Result};
use crate::model::{parse_line, EventType, K8sList, ResourceMeta, WatchEvent};
use crate::snapshot::{apply_event, replace_from_list, Snapshot};

/// Where a watch stream must start for a given list resource version.
pub const BACKOFF_BASE: Duration = Duration::from_millis(100);
/// Upper bound of the reconnect backoff.
pub const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// Default debounce window (nginx-style reload pacing).
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(200);
/// Default periodic re-LIST bound (the staleness watchdog; see
/// [`WatchOptions::resync`]).
pub const DEFAULT_RESYNC: Duration = Duration::from_secs(30);

/// Async face of the API server: exactly what [`watch_loop`] needs.
///
/// [`Client`] implements it. Tests substitute a plain-TCP fake (see
/// `tests/watch_loop_fake.rs`) because the HTTPS-only client would need a
/// full TLS server harness.
pub trait WatchSource: Send + Sync {
    fn fetch(
        &self,
        path: &str,
    ) -> Pin<Box<dyn Future<Output = Result<hyper::Response<ByteStream>>> + Send + '_>>;
}

impl WatchSource for Client {
    fn fetch(
        &self,
        path: &str,
    ) -> Pin<Box<dyn Future<Output = Result<hyper::Response<ByteStream>>> + Send + '_>> {
        // `path` is cloned so the future's lifetime is tied to `&self`
        // only (call sites hold both for the loop's duration anyway).
        let path = path.to_string();
        Box::pin(async move { self.get(&path).await })
    }
}

/// Tunables of the watch loop. `ingress_class` is carried for the render
/// step the callback performs (see `render::routes`); the loop itself does
/// not filter.
#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// Merge window: the callback fires once no new event arrived for this
    /// long (bounded: the window opens when the first change lands).
    pub debounce: Duration,
    /// Ingress class the caller renders routes for.
    pub ingress_class: String,
    /// Staleness watchdog: restart the LIST→WATCH cycle this often so a
    /// silently dead (half-open) stream can never serve a stale snapshot
    /// longer than one interval. A re-LIST with an unchanged resource
    /// version skips the hand-over, so quiet clusters do not churn
    /// generations. `None` disables it (only errors re-list).
    pub resync: Option<Duration>,
}

impl WatchOptions {
    pub fn new(ingress_class: impl Into<String>) -> Self {
        Self {
            debounce: DEFAULT_DEBOUNCE,
            ingress_class: ingress_class.into(),
            resync: Some(DEFAULT_RESYNC),
        }
    }
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self::new("")
    }
}

/// Counters handed to the callback next to the snapshot. The callback sees
/// a consistent, immutable view: fields only grow, `generation` is the
/// 1-based ordinal of the very callback being invoked.
#[derive(Debug, Clone, Default)]
pub struct WatchStats {
    /// Number of `on_snapshot` invocations so far (starts at 1).
    pub generation: u64,
    /// Successful LIST requests.
    pub lists: u64,
    /// Watch streams that ended abnormally (EOF, error, non-2xx open).
    pub reconnects: u64,
    /// When the last successful LIST completed.
    pub last_success: Option<Instant>,
    /// Resource version of the snapshot being handed over.
    pub last_rv: String,
}

/// Double the backoff, capped.
fn grow_backoff(d: Duration) -> Duration {
    d.saturating_mul(2).min(BACKOFF_CAP)
}

/// Watch `path` forever, handing every debounced snapshot to
/// `on_snapshot`. Never returns under its own power: every failure is
/// retried with backoff, and quiet clusters just park in the watch stream.
/// Terminate it by dropping the future (or aborting its task); the last
/// snapshot stays valid with the caller (stale-serve).
pub async fn watch_loop<T, F, S>(
    source: &S,
    path: &str,
    opts: WatchOptions,
    mut on_snapshot: F,
) -> !
where
    T: ResourceMeta + DeserializeOwned + Clone,
    F: FnMut(&Snapshot<T>, &WatchStats),
    S: WatchSource + ?Sized,
{
    let mut backoff = BACKOFF_BASE;
    let mut stats = WatchStats::default();
    let mut snap: Snapshot<T>;
    let mut restart_rv: Option<String> = None;
    'outer: loop {
        // ---- LIST phase (retried with backoff until it succeeds) ----
        let list: K8sList<T> = loop {
            match source.fetch(path).await {
                Ok(resp) if resp.status().is_success() => {
                    let bytes = read_body(resp.into_body()).await;
                    match bytes.and_then(|b| {
                        serde_json::from_slice::<K8sList<T>>(&b).map_err(K8sError::from)
                    }) {
                        Ok(list) => break list,
                        Err(e) => {
                            tracing::warn!(path, error = %e, "list body parse failed; retrying")
                        }
                    }
                }
                Ok(resp) => {
                    tracing::warn!(path, status = %resp.status(), "list request failed; retrying")
                }
                Err(e) => tracing::warn!(path, error = %e, "list request errored; retrying"),
            }
            tokio::time::sleep(backoff).await;
            backoff = grow_backoff(backoff);
        };
        backoff = BACKOFF_BASE;
        stats.lists += 1;
        stats.last_success = Some(Instant::now());
        snap = replace_from_list(list.items, &list.metadata.resource_version);
        stats.last_rv = snap.resource_version().to_string();
        // Hand the fresh snapshot over after a quiet window; events racing
        // in while it is open collapse into the same hand-over.
        // A resync restart re-lists at the rv it left; an unchanged rv means
        // nothing was missed, so no hand-over (no generation churn). Error
        // restarts always hand over.
        let mut dirty = restart_rv
            .take()
            .as_deref()
            .map_or(true, |rv| snap.resource_version() != rv);
        let mut deadline: Option<Pin<Box<Sleep>>> =
            Some(Box::pin(tokio::time::sleep(opts.debounce)));

        // ---- WATCH phase ----
        let mut body: Option<ByteStream> = None;
        let mut resync_at: Option<Pin<Box<Sleep>>> =
            opts.resync.map(|d| Box::pin(tokio::time::sleep(d)));
        let mut buf: Vec<u8> = Vec::new();
        loop {
            if body.is_none() {
                match source
                    .fetch(&watch_path(path, snap.resource_version()))
                    .await
                {
                    Ok(resp) if resp.status().is_success() => body = Some(resp.into_body()),
                    Ok(resp) if resp.status() == 410 => {
                        // Resource version expired: only a re-list heals.
                        tracing::info!(path, "watch returned 410 Gone; re-listing");
                        stats.reconnects += 1;
                        tokio::time::sleep(backoff).await;
                        backoff = grow_backoff(backoff);
                        continue 'outer;
                    }
                    Ok(resp) => {
                        tracing::warn!(path, status = %resp.status(), "watch open failed; re-listing");
                        stats.reconnects += 1;
                        tokio::time::sleep(backoff).await;
                        backoff = grow_backoff(backoff);
                        continue 'outer;
                    }
                    Err(e) => {
                        tracing::warn!(path, error = %e, "watch open errored; re-listing");
                        stats.reconnects += 1;
                        tokio::time::sleep(backoff).await;
                        backoff = grow_backoff(backoff);
                        continue 'outer;
                    }
                }
            }

            let mut reconnect = false;
            tokio::select! {
                frame = async { body.as_mut().expect("body guarded above").frame().await }, if body.is_some() => {
                    match frame {
                        Some(Ok(f)) => {
                            if let Some(data) = f.data_ref() {
                                buf.extend_from_slice(data);
                            }
                            match take_lines(&mut buf) {
                                Ok(lines) => {
                                    for line in lines {
                                        if !apply_line(&mut snap, &mut dirty, &mut deadline, &opts, line) {
                                            reconnect = true;
                                            break;
                                        }
                                    }
                                }
                                Err(()) => {
                                    tracing::warn!(path, "watch stream carried a non-UTF-8 line; re-listing");
                                    reconnect = true;
                                }
                            }
                        }
                        Some(Err(e)) => {
                            tracing::warn!(path, error = %e, "watch stream errored; re-listing");
                            reconnect = true;
                        }
                        None => {
                            tracing::debug!(path, "watch stream ended; re-listing");
                            reconnect = true;
                        }
                    }
                }
                _ = async { deadline.as_mut().expect("deadline guarded").as_mut().await }, if deadline.is_some() => {
                    if dirty {
                        stats.generation += 1;
                        stats.last_rv = snap.resource_version().to_string();
                        on_snapshot(&snap, &stats);
                        dirty = false;
                    }
                    deadline = None;
                }
                _ = async { resync_at.as_mut().expect("resync guarded").as_mut().await }, if resync_at.is_some() => {
                    tracing::debug!(path, "resync tick; re-listing");
                    restart_rv = Some(snap.resource_version().to_string());
                    continue 'outer;
                }
            }
            if reconnect {
                stats.reconnects += 1;
                tokio::time::sleep(backoff).await;
                backoff = grow_backoff(backoff);
                continue 'outer;
            }
        }
    }
}

/// Fold one watch line into the snapshot. Returns false when the line is
/// unusable (parse failure or ERROR event) and the stream must be dropped
/// in favor of a re-list.
fn apply_line<T: ResourceMeta + DeserializeOwned + Clone>(
    snap: &mut Snapshot<T>,
    dirty: &mut bool,
    deadline: &mut Option<Pin<Box<Sleep>>>,
    opts: &WatchOptions,
    line: String,
) -> bool {
    let event: WatchEvent<T> = match parse_line(&line) {
        Ok(event) => event,
        Err(e) => {
            tracing::warn!(line = %line, error = %e, "malformed watch line; re-listing");
            return false;
        }
    };
    if event.event_type == EventType::Error {
        tracing::warn!(line = %line, "watch stream reported ERROR; re-listing");
        return false;
    }
    let data_change = event.event_type != EventType::Bookmark;
    *snap = apply_event(snap, event);
    if data_change {
        *dirty = true;
        // Open (or keep) the merge window: first change starts it, later
        // ones land inside the already-open window.
        if deadline.is_none() {
            *deadline = Some(Box::pin(tokio::time::sleep(opts.debounce)));
        }
    }
    true
}

/// Split complete newline-terminated lines off the front of the buffer.
/// `Err(())` on non-UTF-8 data (stream corruption).
fn take_lines(buf: &mut Vec<u8>) -> std::result::Result<Vec<String>, ()> {
    let mut lines = Vec::new();
    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let raw: Vec<u8> = buf.drain(..=pos).collect();
        let line = std::str::from_utf8(&raw[..raw.len() - 1]).map_err(|_| ())?;
        lines.push(line.trim_end_matches('\r').to_string());
    }
    Ok(lines)
}

async fn read_body(body: ByteStream) -> Result<bytes::Bytes> {
    body.collect()
        .await
        .map(|collected| collected.to_bytes())
        .map_err(|e| K8sError::Io(std::io::Error::other(e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let mut d = BACKOFF_BASE;
        let mut steps = 0;
        while d < BACKOFF_CAP {
            d = grow_backoff(d);
            steps += 1;
            assert!(steps < 20, "must reach the cap");
        }
        assert_eq!(d, BACKOFF_CAP);
        assert_eq!(grow_backoff(BACKOFF_CAP), BACKOFF_CAP);
    }

    #[test]
    fn take_lines_splits_and_keeps_partial_tail() {
        let mut buf = b"{\"a\":1}\n{\"b\":\n".to_vec();
        let lines = take_lines(&mut buf).unwrap();
        assert_eq!(lines, vec!["{\"a\":1}".to_string(), "{\"b\":".to_string()]);
        assert!(buf.is_empty());
    }

    #[test]
    fn take_lines_rejects_non_utf8() {
        let mut buf = vec![0xff, b'\n'];
        assert!(take_lines(&mut buf).is_err());
    }
}
