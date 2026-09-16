//! Log sink selection: stdout (default) or an append-mode file that
//! SIGUSR1 can reopen after external rotation, nginx style.
//!
//! The logrotate contract: rotation renames the file out from under the
//! running gateway; an admin (or logrotate's `postrotate`) then sends
//! SIGUSR1 and [`reopen`] re-opens the same path with create+append, so
//! nothing is truncated and the old file can be compressed once the
//! descriptor is closed.

use openrusty_core::config::ServerConfig;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};

/// Shared handle to the open log file: the non-blocking writer thread and
/// the SIGUSR1 reopen task both go through the same [`Mutex`]-guarded
/// [`File`], so swapping the descriptor is visible to in-flight writes.
#[derive(Debug)]
pub struct ReopenableWriter {
    path: PathBuf,
    file: Arc<Mutex<File>>,
}

/// Clones share the same underlying file descriptor slot.
impl Clone for ReopenableWriter {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            file: Arc::clone(&self.file),
        }
    }
}

/// Locks the file slot, treating a poisoned lock (a panicked writer) as
/// recoverable: losing logs beats losing the gateway.
fn locked(mutex: &Mutex<File>) -> std::sync::MutexGuard<'_, File> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Opens (or creates) `path` for append.
pub fn open(path: &Path) -> io::Result<ReopenableWriter> {
    Ok(ReopenableWriter {
        path: path.to_path_buf(),
        file: Arc::new(Mutex::new(open_file(path)?)),
    })
}

/// Bare create+append open shared by [`open`] and [`reopen`].
fn open_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

impl Write for ReopenableWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        locked(&self.file).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        locked(&self.file).flush()
    }
}

/// Re-opens [`ReopenableWriter::path`] and swaps the descriptor in place;
/// dropping the old [`File`] closes it so the rotated file can be
/// compressed. The new file is opened create+append, so a reopen without
/// an external rename never truncates history.
pub fn reopen(writer: &ReopenableWriter) -> io::Result<()> {
    let fresh = open_file(&writer.path)?;
    *locked(&writer.file) = fresh;
    tracing::info!(path = %writer.path.display(), "log file reopened");
    Ok(())
}

/// Builds the process-wide tracing sink from `server.log_file`.
///
/// `None` keeps today's stdout behavior byte-for-byte (journald captures
/// it); `Some(path)` opens the file eagerly - io errors propagate so the
/// caller can fail fast before tracing is up - and returns a
/// [`ReopenableWriter`] handle for the SIGUSR1 task.
pub fn init(
    server: &ServerConfig,
) -> io::Result<(NonBlocking, WorkerGuard, Option<ReopenableWriter>)> {
    match &server.log_file {
        None => {
            let (writer, guard) = tracing_appender::non_blocking(std::io::stdout());
            Ok((writer, guard, None))
        }
        Some(path) => {
            let writer = open(Path::new(path))?;
            let handle = writer.clone();
            let (non_blocking, guard) = tracing_appender::non_blocking(writer);
            Ok((non_blocking, guard, Some(handle)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TmpDir;
    use std::fs;

    fn append(w: &mut ReopenableWriter, line: &str) {
        w.write_all(line.as_bytes()).unwrap();
        w.flush().unwrap();
    }

    /// Rotation contract: rename + reopen splits history across files and
    /// truncates nothing.
    #[test]
    fn reopen_after_rename_writes_to_new_file() {
        let tmp = TmpDir::new("logrotate");
        let path = tmp.0.join("gw.log");
        let mut w = open(&path).unwrap();
        append(&mut w, "first\n");
        fs::rename(&path, tmp.0.join("rotated.log")).unwrap();

        reopen(&w).unwrap();
        append(&mut w, "second\n");

        assert_eq!(
            fs::read_to_string(tmp.0.join("rotated.log")).unwrap(),
            "first\n"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "second\n");
    }

    /// Reopen without a rename appends; existing content survives.
    #[test]
    fn reopen_without_rename_appends() {
        let tmp = TmpDir::new("logappend");
        let path = tmp.0.join("gw.log");
        let mut w = open(&path).unwrap();
        append(&mut w, "a\n");
        reopen(&w).unwrap();
        append(&mut w, "b\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), "a\nb\n");
    }

    /// Fail-fast contract: an unopenable path surfaces at boot, not
    /// silently at first log line.
    #[test]
    fn open_missing_dir_is_err() {
        let tmp = TmpDir::new("logmissing");
        assert!(open(&tmp.0.join("no-such-dir/gw.log")).is_err());
    }
}
