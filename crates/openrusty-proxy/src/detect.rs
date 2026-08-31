//! Raw protocol detection for transparently intercepted connections.
//!
//! In the sidecar deployment the gateway listener sits behind iptables /
//! nftables `REDIRECT` rules (`orig_dst` recovers the pre-NAT address).
//! Such a socket carries an arbitrary application byte stream the gateway
//! never saw a request for, so the first job is to sniff which protocol the
//! peer is speaking. Following the semantics of linkerd2-proxy's
//! `http/detect`, a bounded prefix of the stream is read and prefix-matched
//! against the HTTP preambles:
//!
//! - A stream starting with `PRI *` (the leading bytes of the 24-byte
//!   HTTP/2 client preface `PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n`) is
//!   [`Protocol::H2`].
//! - A stream starting with an HTTP/1 request-line method token followed
//!   by SP is [`Protocol::H1`].
//! - Anything else is [`Protocol::Opaque`]: TLS handshakes, SSH banners,
//!   database wire protocols, arbitrary TCP.
//! - A peer that stays silent past `timeout` is [`Protocol::Opaque`]: a
//!   quiet TCP service is opaque by definition, and the empty consumed
//!   prefix forwards cleanly.
//! - An EOF before the prefix can settle - whether nothing or a truncated
//!   prefix arrived - is `io::ErrorKind::UnexpectedEof`; the caller decides
//!   to close.
//!
//! Deliberate choices, locked by tests:
//!
//! - H2 needs only `PRI *`, not the full 24-byte preface. `PRI ` alone is
//!   ambiguous, so a fifth byte is read; anything but `*` (e.g. `PRI X`)
//!   rules h2 out and the stream is opaque. Once `*` is seen no further
//!   byte can change the verdict, and waiting for the whole preface would
//!   only delay detection of well-formed h2 clients.
//! - EOF after a partial prefix is an error, not `Opaque`, matching
//!   linkerd2-proxy (which fails `UnexpectedEof` even when some bytes
//!   arrived). A truncated prefix can never complete a valid request, so
//!   guessing opaque would only forward a stream the peer abandoned.
//! - `timeout` is one total budget from the first call, not per read
//!   (linkerd arms a single deadline for the whole preface). On expiry the
//!   verdict is `Opaque` together with whatever bytes already arrived, so
//!   a stalled half-preface is tunneled rather than dropped.
//! - Matching is byte-exact - methods are case-sensitive tokens - and only
//!   the nginx-aligned method set below counts as HTTP/1; an unrecognized
//!   leading token is opaque by design.
//!
//! The primitive is pure stateless I/O: one function over a borrowed
//! [`TcpStream`], every outcome reported through the return value, and
//! every byte read preserved in the returned [`Bytes`] for the caller to
//! re-inject ahead of the live stream - sniffing never consumes a byte.

use std::io;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

/// Leading bytes of the HTTP/2 client preface that suffice to identify it.
///
/// The full preface is `PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n`; only `PRI *`
/// (five bytes) is matched, so `PRI X` never classifies as h2.
const H2_PREFIX: &[u8] = b"PRI *";

/// Request-line method tokens recognized as HTTP/1, case-sensitively.
const H1_METHODS: [&[u8]; 9] = [
    b"GET", b"POST", b"PUT", b"DELETE", b"HEAD", b"OPTIONS", b"PATCH", b"CONNECT", b"TRACE",
];

/// Read granularity of the probe. Large enough that any method token plus
/// its SP arrives in one syscall; every byte read beyond the discriminating
/// prefix stays in the returned buffer, so over-reading is lossless.
const READ_CHUNK: usize = 16;

/// The protocol sniffed from a connection's first bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// HTTP/1.x request line; feed the stream into the h1 pipeline.
    H1,
    /// HTTP/2 client preface; feed the stream into the h2 path.
    H2,
    /// Neither HTTP flavor; tunnel the bytes through untouched.
    Opaque,
}

/// Outcome of prefix-matching the bytes read so far. Pure: total over every
/// input, no I/O.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    /// The prefix settles the protocol.
    Complete(Protocol),
    /// The prefix could still grow into `PRI *` or a method token.
    Incomplete,
}

/// Sniffs the protocol of an inbound connection, consuming the smallest
/// prefix that decides it.
///
/// Every byte read is returned alongside the verdict - including the empty
/// buffer after a timeout - so the caller can re-inject the prefix ahead of
/// the live stream and nothing is lost. `timeout` bounds the whole probe;
/// a peer silent past it yields [`Protocol::Opaque`]. An EOF before the
/// prefix can settle (no bytes at all, or a truncated prefix) yields
/// `UnexpectedEof`, per linkerd2-proxy semantics.
pub async fn detect(stream: &mut TcpStream, timeout: Duration) -> io::Result<(Protocol, Bytes)> {
    let mut buf = BytesMut::with_capacity(READ_CHUNK * 2);
    // One deadline for the entire probe (linkerd semantics): a partially
    // read prefix that stalls turns opaque just like a fully silent peer.
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        // `read` is cancel-safe, so taking the timeout branch below can
        // never lose bytes that arrived between polls.
        let read = stream.read(&mut chunk);
        tokio::select! {
            _ = deadline.as_mut() => {
                return Ok((Protocol::Opaque, buf.freeze()));
            }
            res = read => {
                let n = res?;
                if n == 0 {
                    // EOF with the verdict still open: either nothing was
                    // sent at all, or the prefix was cut mid-flight.
                    let reason = if buf.is_empty() {
                        "peer closed before sending any bytes"
                    } else {
                        "peer closed with the protocol prefix incomplete"
                    };
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, reason));
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Decision::Complete(protocol) = classify(&buf) {
                    return Ok((protocol, buf.freeze()));
                }
            }
        }
    }
}

/// Classifies the bytes read so far. A prefix completes as soon as its
/// protocol is decided: `PRI *` (h2), a method token followed by SP (h1),
/// a method token followed by any other byte, or any byte that can no
/// longer grow into either shape (opaque).
fn classify(buf: &[u8]) -> Decision {
    if buf.starts_with(H2_PREFIX) {
        return Decision::Complete(Protocol::H2);
    }
    // `P`, `PR`, `PRI`, `PRI ` may still grow into the h2 preface.
    if H2_PREFIX.starts_with(buf) {
        return Decision::Incomplete;
    }
    // At least five bytes led by `PRI ` whose fifth byte is not `*`: the
    // preface is ruled out, and `PRI` is no method token.
    if buf.starts_with(b"PRI ") {
        return Decision::Complete(Protocol::Opaque);
    }
    for method in H1_METHODS {
        if buf.starts_with(method) {
            if buf.len() > method.len() {
                // The byte after a method token must be SP on a request line.
                if buf[method.len()] == b' ' {
                    return Decision::Complete(Protocol::H1);
                }
                return Decision::Complete(Protocol::Opaque);
            }
            // Exact token so far; the request-line SP is still pending.
            return Decision::Incomplete;
        }
        // Not the full token yet, but it could still grow into this one.
        if method.starts_with(buf) {
            return Decision::Incomplete;
        }
    }
    Decision::Complete(Protocol::Opaque)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    /// Generous probe budget for tests that must conclude by data or EOF,
    /// never by timeout.
    const LONG: Duration = Duration::from_secs(5);

    /// Connected loopback pair: `client` writes probe bytes, `detect` runs
    /// on `server`.
    async fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("listener local addr");
        let client = TcpStream::connect(addr)
            .await
            .expect("connect to loopback listener");
        let (server, _) = listener.accept().await.expect("accept loopback");
        (client, server)
    }

    /// Boundary table for the pure matcher, including the points the module
    /// docs promise: `PRI`/`PRI ` stay open, `PRI *` is h2, `PRI X` is not.
    #[test]
    fn classify_prefixes() {
        use Decision::{Complete, Incomplete};
        let cases: &[(&[u8], Decision)] = &[
            (b"", Incomplete),
            (b"P", Incomplete),
            (b"PR", Incomplete),
            (b"PRI", Incomplete),
            (b"PRI ", Incomplete),
            (b"PRI *", Complete(Protocol::H2)),
            (b"PRI X", Complete(Protocol::Opaque)),
            (b"PRIB", Complete(Protocol::Opaque)),
            (b"G", Incomplete),
            (b"GE", Incomplete),
            (b"GET", Incomplete),
            (b"GET ", Complete(Protocol::H1)),
            (b"GETX", Complete(Protocol::Opaque)),
            (b"HEADX /", Complete(Protocol::Opaque)),
            (b"OPTIONS ", Complete(Protocol::H1)),
            (b"OPT", Incomplete),
            (b"TRACE ", Complete(Protocol::H1)),
            (b"get /", Complete(Protocol::Opaque)),
            (&[0x16], Complete(Protocol::Opaque)),
        ];
        for (prefix, expected) in cases {
            assert_eq!(&classify(prefix), expected, "prefix {prefix:?}");
        }
    }

    #[tokio::test]
    async fn h1_request_line_is_detected_with_identical_bytes() {
        let (mut client, mut server) = loopback_pair().await;
        // Exactly READ_CHUNK bytes: one read must consume it whole.
        let req: &[u8] = b"GET / HTTP/1.1\r\n";
        client.write_all(req).await.expect("write h1 request");

        let (protocol, prefix) = detect(&mut server, LONG).await.expect("detect h1");
        assert_eq!(protocol, Protocol::H1);
        assert_eq!(&prefix[..], req);
    }

    #[tokio::test]
    async fn h2_preface_is_detected() {
        let (mut client, mut server) = loopback_pair().await;
        let preface: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
        client.write_all(preface).await.expect("write h2 preface");

        let (protocol, prefix) = detect(&mut server, LONG).await.expect("detect h2");
        assert_eq!(protocol, Protocol::H2);
        assert!(
            preface.starts_with(&prefix[..]),
            "consumed bytes must be a prefix of the preface: {prefix:?}"
        );
    }

    #[tokio::test]
    async fn pri_without_star_is_not_h2() {
        let (mut client, mut server) = loopback_pair().await;
        client.write_all(b"PRI X").await.expect("write PRI X");

        let (protocol, prefix) = detect(&mut server, LONG).await.expect("detect");
        assert_eq!(protocol, Protocol::Opaque);
        assert_eq!(&prefix[..], &b"PRI X"[..]);
    }

    #[tokio::test]
    async fn tls_client_hello_is_opaque() {
        let (mut client, mut server) = loopback_pair().await;
        // TLS 1.x record header followed by a ClientHello lead-in.
        let hello: &[u8] = &[
            0x16, 0x03, 0x01, 0x00, 0xec, 0x01, 0x00, 0x00, 0xe8, 0x03, 0x03,
        ];
        client.write_all(hello).await.expect("write clienthello");

        let (protocol, prefix) = detect(&mut server, LONG).await.expect("detect tls");
        assert_eq!(protocol, Protocol::Opaque);
        assert_eq!(&prefix[..], hello);
    }

    #[tokio::test]
    async fn ssh_banner_is_opaque() {
        let (mut client, mut server) = loopback_pair().await;
        let banner: &[u8] = b"SSH-2.0-OpenSSH_9.6\r\n";
        client.write_all(banner).await.expect("write banner");

        let (protocol, prefix) = detect(&mut server, LONG).await.expect("detect ssh");
        assert_eq!(protocol, Protocol::Opaque);
        assert!(
            banner.starts_with(&prefix[..]),
            "consumed bytes must be a prefix of the banner: {prefix:?}"
        );
    }

    #[tokio::test]
    async fn silent_peer_is_opaque_after_timeout() {
        let (_client, mut server) = loopback_pair().await;
        let timeout = Duration::from_millis(50);
        let started = Instant::now();

        let (protocol, prefix) = detect(&mut server, timeout).await.expect("detect silence");
        assert_eq!(protocol, Protocol::Opaque);
        assert!(prefix.is_empty(), "nothing was sent, nothing consumed");
        let elapsed = started.elapsed();
        assert!(elapsed >= timeout, "returned early after {elapsed:?}");
        assert!(
            elapsed < Duration::from_secs(2),
            "timeout fired suspiciously late ({elapsed:?})"
        );
    }

    #[tokio::test]
    async fn eof_before_first_byte_is_unexpected_eof() {
        let (mut client, mut server) = loopback_pair().await;
        client.shutdown().await.expect("half-close without data");

        let err = detect(&mut server, LONG)
            .await
            .expect_err("silent close must be an error");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn eof_mid_prefix_is_unexpected_eof_not_opaque() {
        // Locks the linkerd-aligned choice: a truncated prefix can never
        // complete a request, so it errors instead of degrading to opaque.
        for partial in [&b"GE"[..], &b"PRI "[..]] {
            let (mut client, mut server) = loopback_pair().await;
            client.write_all(partial).await.expect("write partial");
            client.shutdown().await.expect("half-close mid-prefix");

            let err = detect(&mut server, LONG)
                .await
                .expect_err("truncated prefix must be an error");
            assert_eq!(
                err.kind(),
                io::ErrorKind::UnexpectedEof,
                "prefix {partial:?}"
            );
        }
    }

    #[tokio::test]
    async fn stalled_partial_prefix_is_opaque_with_bytes_kept() {
        let (mut client, mut server) = loopback_pair().await;
        client.write_all(b"GE").await.expect("write partial");

        let (protocol, prefix) = detect(&mut server, Duration::from_millis(50))
            .await
            .expect("detect stall");
        assert_eq!(protocol, Protocol::Opaque);
        assert_eq!(&prefix[..], &b"GE"[..]);
    }

    #[tokio::test]
    async fn detection_never_loses_h1_bytes() {
        let (mut client, mut server) = loopback_pair().await;
        let stream: &[u8] = b"POST /upload HTTP/1.1\r\nHost: gw\r\nContent-Length: 5\r\n\r\nhello";
        client.write_all(stream).await.expect("write stream");
        client.shutdown().await.expect("half-close");

        let (protocol, prefix) = detect(&mut server, LONG).await.expect("detect h1");
        assert_eq!(protocol, Protocol::H1);

        let mut rest = Vec::new();
        tokio::time::timeout(LONG, server.read_to_end(&mut rest))
            .await
            .expect("remainder never drained")
            .expect("read remainder");

        let mut whole = prefix.to_vec();
        whole.extend_from_slice(&rest);
        assert_eq!(&whole[..], stream);
    }

    #[tokio::test]
    async fn detection_never_loses_h2_frames() {
        let (mut client, mut server) = loopback_pair().await;
        // Preface plus an empty SETTINGS frame header.
        let stream: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00";
        client.write_all(stream).await.expect("write preface+frame");
        client.shutdown().await.expect("half-close");

        let (protocol, prefix) = detect(&mut server, LONG).await.expect("detect h2");
        assert_eq!(protocol, Protocol::H2);

        let mut rest = Vec::new();
        tokio::time::timeout(LONG, server.read_to_end(&mut rest))
            .await
            .expect("remainder never drained")
            .expect("read remainder");

        let mut whole = prefix.to_vec();
        whole.extend_from_slice(&rest);
        assert_eq!(&whole[..], stream);
    }
}
