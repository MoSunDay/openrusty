//! End-to-end `watch_loop` drive against a plain-TCP fake apiserver.
//!
//! `Client` is HTTPS-only, so instead of a full TLS harness the test plugs a
//! tiny [`WatchSource`] (pooled hyper legacy client over plain HTTP on
//! `127.0.0.1`) into the unmodified loop and scripts the server responses by
//! global request order:
//!
//! | # | request      | scripted response                                   |
//! |---|--------------|-----------------------------------------------------|
//! | 0 | LIST         | 200, rv=100, 1 item                                 |
//! | 1 | WATCH rv=100 | ADDED rv=101 + MODIFIED rv=102 at +5ms, EOF at +150 |
//! | 2 | LIST         | 200, rv=102, 2 items                                |
//! | 3 | WATCH rv=102 | BOOKMARK rv=110 at +5ms, EOF at +150                |
//! | 4 | LIST         | 200, rv=110, 2 items                                |
//! | 5 | WATCH rv=110 | BOOKMARK, body held open forever                    |
//!
//! Timing is chosen so every phase is deterministic against the loop's
//! 60ms debounce and 100ms/200ms reconnect backoff:
//!
//! - each watch answers frames well inside its LIST's still-open debounce
//!   window (+5ms << 60ms), so the burst merges with the list snapshot and
//!   the bookmark folds into the pending window's rv;
//! - each scripted stream stays open past that window (EOF at +150ms) and
//!   the next LIST only happens after the 100ms/200ms backoff, so every
//!   window fires exactly once before it could be replaced;
//! - the held watch stays quiet forever: no fourth generation.
//!
//! Expected callbacks: (rv=102, 2 items), (rv=110, 2 items), (rv=110,
//! 2 items); final stats lists=3, reconnects=2, last_rv="110".

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::{Body, Frame, SizeHint};
use hyper::service::service_fn;
use hyper::{Request, Response, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as LegacyClient;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;
use tokio::time::Sleep;

use openrusty_k8s::client::ByteStream;
use openrusty_k8s::error::{K8sError, Result};
use openrusty_k8s::model::Ingress;
use openrusty_k8s::snapshot::Snapshot;
use openrusty_k8s::watch::{watch_loop, WatchOptions, WatchSource, WatchStats};

const PATH: &str = "/apis/networking.k8s.io/v1/ingresses";
/// Below the loop's reconnect backoff (100ms+): a window always fires
/// before the re-list that would otherwise replace it.
const DEBOUNCE: Duration = Duration::from_millis(60);
/// Watch frames are answered this fast so they land inside the still-open
/// debounce window of the LIST that preceded them (see module docs).
const WATCH_DELAY: Duration = Duration::from_millis(5);
/// Watch streams stay open this long past their frames, then end.
const STREAM_OPEN: Duration = Duration::from_millis(150);
/// Long quiet beat proving the held watch produces no further callbacks.
const HOLD_QUIET: Duration = Duration::from_millis(250);
const TEST_TIMEOUT: Duration = Duration::from_secs(15);

const LIST_RV_100: &str = r#"{"metadata":{"resourceVersion":"100"},"items":[{"metadata":{"name":"app","namespace":"web","resourceVersion":"100"},"spec":{"ingressClassName":"openrusty","rules":[{"host":"app.example.com","http":{"paths":[{"path":"/","pathType":"Prefix","backend":{"service":{"name":"example-svc","port":{"number":8000}}}}]}}]}}]}"#;

const EVENT_ADDED_101: &str = r#"{"type":"ADDED","object":{"metadata":{"name":"second","namespace":"web","resourceVersion":"101"},"spec":{"ingressClassName":"openrusty","rules":[{"host":"second.example.com","http":{"paths":[{"path":"/","pathType":"Prefix","backend":{"service":{"name":"second-svc","port":{"number":8100}}}}]}}]}}}"#;

const EVENT_MODIFIED_102: &str = r#"{"type":"MODIFIED","object":{"metadata":{"name":"app","namespace":"web","resourceVersion":"102"},"spec":{"ingressClassName":"openrusty","rules":[{"host":"app.example.com","http":{"paths":[{"path":"/v2","pathType":"Prefix","backend":{"service":{"name":"example-svc","port":{"number":8001}}}}]}}]}}}"#;

const LIST_RV_102: &str = r#"{"metadata":{"resourceVersion":"102"},"items":[{"metadata":{"name":"app","namespace":"web","resourceVersion":"102"},"spec":{"ingressClassName":"openrusty","rules":[{"host":"app.example.com","http":{"paths":[{"path":"/v2","pathType":"Prefix","backend":{"service":{"name":"example-svc","port":{"number":8001}}}}]}}]}},{"metadata":{"name":"second","namespace":"web","resourceVersion":"101"},"spec":{"ingressClassName":"openrusty","rules":[{"host":"second.example.com","http":{"paths":[{"path":"/","pathType":"Prefix","backend":{"service":{"name":"second-svc","port":{"number":8100}}}}]}}]}}]}"#;

const EVENT_BOOKMARK_110: &str = r#"{"type":"BOOKMARK","object":{"apiVersion":"networking.k8s.io/v1","kind":"Ingress","metadata":{"resourceVersion":"110"}}}"#;

/// Same items as the rv=102 list, but the envelope carries the rv the
/// bookmark handed out; a re-list must never rewind past it.
const LIST_RV_110: &str = r#"{"metadata":{"resourceVersion":"110"},"items":[{"metadata":{"name":"app","namespace":"web","resourceVersion":"110"},"spec":{"ingressClassName":"openrusty","rules":[{"host":"app.example.com","http":{"paths":[{"path":"/v2","pathType":"Prefix","backend":{"service":{"name":"example-svc","port":{"number":8001}}}}]}}]}},{"metadata":{"name":"second","namespace":"web","resourceVersion":"101"},"spec":{"ingressClassName":"openrusty","rules":[{"host":"second.example.com","http":{"paths":[{"path":"/","pathType":"Prefix","backend":{"service":{"name":"second-svc","port":{"number":8100}}}}]}}]}}]}"#;

fn list_response(body: &str) -> Response<FakeBody> {
    scripted_response(
        vec![(Duration::ZERO, Bytes::from(body.to_string()))],
        None,
        false,
    )
}

/// Scripted response body: each frame is released at its scheduled instant
/// (driven by a real `Sleep` so polls resume), then the body either holds
/// open forever or ends at `end_at`.
struct FakeBody {
    frames: VecDeque<(Instant, Bytes)>,
    end_at: Option<Instant>,
    hold: bool,
    timer: Option<Pin<Box<Sleep>>>,
}

impl FakeBody {
    /// Polls the pending timer for `at`; true once the instant is reached.
    fn sleep_until(&mut self, at: Instant, cx: &mut Context<'_>) -> bool {
        if self
            .timer
            .as_ref()
            .is_some_and(|timer| timer.deadline() != at.into())
        {
            self.timer = None;
        }
        let timer = self
            .timer
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(at.into())));
        match timer.as_mut().poll(cx) {
            Poll::Ready(()) => {
                self.timer = None;
                true
            }
            Poll::Pending => false,
        }
    }
}

impl Body for FakeBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        loop {
            let now = Instant::now();
            if let Some((at, _)) = this.frames.front() {
                if *at <= now {
                    let (_, bytes) = this.frames.pop_front().unwrap();
                    return Poll::Ready(Some(Ok(Frame::data(bytes))));
                }
                if !this.sleep_until(*at, cx) {
                    return Poll::Pending;
                }
                continue;
            }
            if this.hold {
                return Poll::Pending;
            }
            match this.end_at {
                None => return Poll::Ready(None),
                Some(end) if end <= now => return Poll::Ready(None),
                Some(end) => {
                    if !this.sleep_until(end, cx) {
                        return Poll::Pending;
                    }
                }
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

/// What the fake apiserver has seen, shared with the assertions.
#[derive(Default)]
struct FakeState {
    served: AtomicUsize,
    log: Mutex<Vec<String>>,
}

impl FakeState {
    fn record(&self, req: &Request<hyper::body::Incoming>) {
        self.served.fetch_add(1, Ordering::SeqCst);
        self.log
            .lock()
            .unwrap()
            .push(format!("{} {}", req.method(), req.uri()));
    }
}

fn scripted_response(
    frames: Vec<(Duration, Bytes)>,
    end_at: Option<Duration>,
    hold: bool,
) -> Response<FakeBody> {
    let now = Instant::now();
    let frames = frames.into_iter().map(|(at, b)| (now + at, b)).collect();
    let end_at = end_at.map(|at| now + at);
    let mut resp = Response::new(FakeBody {
        frames,
        end_at,
        hold,
        timer: None,
    });
    resp.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    resp
}

fn watch_frames(events: &[&str]) -> Vec<(Duration, Bytes)> {
    // Split the event stream mid-JSON across chunk boundaries to exercise
    // the loop's line buffering, all released at +WATCH_DELAY.
    let bytes = Bytes::from(format!("{}\n", events.join("\n")));
    let split = bytes.len() / 2;
    vec![
        (WATCH_DELAY, bytes.slice(..split)),
        (WATCH_DELAY, bytes.slice(split..)),
    ]
}

async fn handle(
    req: Request<hyper::body::Incoming>,
    state: Arc<FakeState>,
) -> std::result::Result<Response<FakeBody>, std::convert::Infallible> {
    state.record(&req);
    let n = state.served.load(Ordering::SeqCst) - 1;
    let is_watch = req.uri().query().unwrap_or_default().contains("watch=1");
    if is_watch {
        return Ok(match n {
            1 => scripted_response(
                watch_frames(&[EVENT_ADDED_101, EVENT_MODIFIED_102]),
                Some(STREAM_OPEN),
                false,
            ),
            3 => scripted_response(
                watch_frames(&[EVENT_BOOKMARK_110]),
                Some(STREAM_OPEN),
                false,
            ),
            // The last watch yields one bookmark, then stays open forever:
            // real apiservers keep quiet streams alive indefinitely.
            _ => scripted_response(watch_frames(&[EVENT_BOOKMARK_110]), None, true),
        });
    }
    Ok(match n {
        0 => list_response(LIST_RV_100),
        2 => list_response(LIST_RV_102),
        _ => list_response(LIST_RV_110),
    })
}

async fn serve(listener: TcpListener, state: Arc<FakeState>) {
    loop {
        let (tcp, _) = match listener.accept().await {
            Ok(conn) => conn,
            Err(_) => return,
        };
        let st = state.clone();
        tokio::spawn(async move {
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    TokioIo::new(tcp),
                    service_fn(move |req| handle(req, st.clone())),
                )
                .await;
        });
    }
}

/// Plain-HTTP [`WatchSource`] for the fake: same shape as `Client`'s impl,
/// minus TLS.
#[derive(Clone)]
struct FakeSource {
    http: LegacyClient<HttpConnector, Full<Bytes>>,
    endpoint: Uri,
}

impl FakeSource {
    fn new(port: u16) -> Self {
        Self {
            http: LegacyClient::builder(TokioExecutor::new())
                .pool_idle_timeout(Duration::from_secs(5))
                .build(HttpConnector::new()),
            endpoint: format!("http://127.0.0.1:{port}").parse().unwrap(),
        }
    }
}

impl WatchSource for FakeSource {
    fn fetch(
        &self,
        path: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Response<ByteStream>>> + Send + '_>> {
        let mut parts = self.endpoint.clone().into_parts();
        parts.path_and_query = Some(path.parse().unwrap());
        let uri = Uri::from_parts(parts).unwrap();
        let request = Request::builder()
            .uri(uri)
            .header("accept", "application/json")
            .body(Full::new(Bytes::new()))
            .unwrap();
        Box::pin(async move { self.http.request(request).await.map_err(K8sError::Http) })
    }
}

/// What the callback recorded: (generation, rv, snapshot size).
#[derive(Default)]
struct Observed {
    events: Mutex<Vec<(u64, String, usize)>>,
    snapshots: Mutex<Vec<Snapshot<Ingress>>>,
}

async fn wait_for_generation(observed: &Observed, generation: u64, state: &FakeState) {
    let deadline = tokio::time::Instant::now() + TEST_TIMEOUT;
    loop {
        if observed
            .events
            .lock()
            .unwrap()
            .last()
            .is_some_and(|e| e.0 >= generation)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out at generation {}; events={:?}; server log={:?}",
            generation,
            observed.events.lock().unwrap(),
            state.log.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_loop_relists_and_reconnects_against_fake_apiserver() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(FakeState::default());
    tokio::spawn(serve(listener, state.clone()));

    let source = FakeSource::new(port);
    let observed = Arc::new(Observed::default());
    let recorder = observed.clone();
    let opts = WatchOptions {
        debounce: DEBOUNCE,
        ingress_class: "openrusty".to_string(),
    };

    tokio::select! {
        _ = watch_loop::<Ingress, _, _>(&source, PATH, opts, move |snap: &Snapshot<Ingress>,
              stats: &WatchStats| {
            recorder.snapshots.lock().unwrap().push(snap.clone());
            recorder
                .events
                .lock()
                .unwrap()
                .push((stats.generation, stats.last_rv.clone(), snap.len()));
        }) => unreachable!("watch_loop never returns on its own"),
        _ = wait_for_generation(&observed, 3, &state) => {}
    }

    // The held watch is quiet: a full quiet beat must not add a generation.
    tokio::time::sleep(HOLD_QUIET).await;

    let events = observed.events.lock().unwrap().clone();
    assert_eq!(
        events,
        vec![
            (1, "102".to_string(), 2),
            (2, "110".to_string(), 2),
            (3, "110".to_string(), 2),
        ],
        "LIST+burst coalesce, LIST+bookmark coalesce, final LIST; the held \
         watch stays quiet"
    );

    let snapshots = observed.snapshots.lock().unwrap();
    let last = snapshots.last().unwrap();
    assert_eq!(last.resource_version(), "110");
    let app = last.get("web", "app").unwrap();
    let app_rule = &app.spec.rules[0];
    assert_eq!(app_rule.host.as_deref(), Some("app.example.com"));
    let app_path = &app_rule.http.as_ref().unwrap().paths[0];
    assert_eq!(app_path.path, "/v2");
    let app_svc = app_path.backend.service.as_ref().unwrap();
    assert_eq!(app_svc.port.as_ref().unwrap().number, Some(8001));
    let second = last.get("web", "second").unwrap();
    assert_eq!(
        second.spec.rules[0].http.as_ref().unwrap().paths[0]
            .backend
            .service
            .as_ref()
            .unwrap()
            .name,
        "second-svc"
    );

    // Server saw exactly the scripted request sequence.
    let log = state.log.lock().unwrap().clone();
    assert_eq!(log.len(), 6, "unexpected extra requests: {log:?}");
    assert!(log[0].ends_with(PATH), "request 0 is the LIST: {log:?}");
    assert!(log[1].contains("watch=1") && log[1].contains("resourceVersion=100"));
    assert!(log[2].ends_with(PATH));
    assert!(log[3].contains("watch=1") && log[3].contains("resourceVersion=102"));
    assert!(log[4].ends_with(PATH));
    assert!(log[5].contains("watch=1") && log[5].contains("resourceVersion=110"));
}
