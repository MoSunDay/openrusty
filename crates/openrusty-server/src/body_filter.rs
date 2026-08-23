//! Streaming response body that runs the body_filter phase per chunk and
//! the log phase exactly once when the stream ends, errors, or is dropped
//! (client disconnect mid-stream).

use axum::body::HttpBody;
use bytes::Bytes;
use hyper::body::Frame;
use hyper::body::Incoming;
use openrusty_core::phase::Phase;
use openrusty_wasm::RequestSession;
use std::error::Error as StdError;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

/// Body error type accepted by `axum::body::Body::new`.
pub type BoxError = Box<dyn StdError + Send + Sync>;

/// Wraps the upstream `Incoming` stream; every data chunk is pushed through
/// the plugin body_filter phase (observe-only: the chunk itself is yielded
/// unchanged). The session is shared behind a Mutex because the body outlives
/// the handler that created it.
pub struct FilteredBody {
    inner: Option<Incoming>,
    session: Arc<Mutex<RequestSession>>,
    status: u16,
    peer: String,
    started: Instant,
    bytes: u64,
    /// True once the final body_filter call (empty chunk, last=true) ran.
    last_sent: bool,
    /// True once the log phase ran; guards against double runs.
    log_done: bool,
}

impl FilteredBody {
    pub fn new(
        session: Arc<Mutex<RequestSession>>,
        inner: Incoming,
        status: u16,
        peer: String,
    ) -> Self {
        FilteredBody {
            inner: Some(inner),
            session,
            status,
            peer,
            started: Instant::now(),
            bytes: 0,
            last_sent: false,
            log_done: false,
        }
    }
}

/// Push one chunk through body_filter. Locks are held only for the
/// (timeout-bounded) synchronous wasm call.
fn run_filter(session: &Mutex<RequestSession>, chunk: Bytes, last: bool) {
    if let Ok(mut s) = session.lock() {
        s.set_body_chunk(chunk, last);
        s.run_phase(Phase::BodyFilter);
    }
}

/// Run the log phase and emit the access-log line.
fn run_log(session: &Mutex<RequestSession>, status: u16, peer: &str, started: Instant, bytes: u64) {
    if let Ok(mut s) = session.lock() {
        s.run_phase(Phase::Log);
    }
    tracing::info!(
        status,
        peer = %peer,
        bytes,
        ms = started.elapsed().as_millis() as u64,
        "request finished"
    );
}

impl HttpBody for FilteredBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        // All fields are Unpin, so plain field access is fine.
        let this = self.as_mut().get_mut();
        let Some(inner) = this.inner.as_mut() else {
            return Poll::Ready(None);
        };
        match Pin::new(inner).poll_frame(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.bytes += data.len() as u64;
                    run_filter(&this.session, data.clone(), false);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => {
                // Stream error: finalize as if the body ended here.
                this.inner = None;
                if !this.last_sent {
                    this.last_sent = true;
                    run_filter(&this.session, Bytes::new(), true);
                }
                if !this.log_done {
                    this.log_done = true;
                    run_log(
                        &this.session,
                        this.status,
                        &this.peer,
                        this.started,
                        this.bytes,
                    );
                }
                Poll::Ready(Some(Err(Box::new(e))))
            }
            Poll::Ready(None) => {
                this.inner = None;
                if !this.last_sent {
                    this.last_sent = true;
                    run_filter(&this.session, Bytes::new(), true);
                }
                if !this.log_done {
                    this.log_done = true;
                    run_log(
                        &this.session,
                        this.status,
                        &this.peer,
                        this.started,
                        this.bytes,
                    );
                }
                Poll::Ready(None)
            }
        }
    }
}

impl Drop for FilteredBody {
    fn drop(&mut self) {
        // Covers client disconnect mid-stream: the body is dropped before
        // the final chunk. Running wasm here is acceptable: it is bounded
        // by the plugin timeout.
        if !self.last_sent {
            self.last_sent = true;
            run_filter(&self.session, Bytes::new(), true);
        }
        if !self.log_done {
            self.log_done = true;
            run_log(
                &self.session,
                self.status,
                &self.peer,
                self.started,
                self.bytes,
            );
        }
    }
}
