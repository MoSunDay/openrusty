//! Prefix re-injection adapter for sniffed connections.
//!
//! `proxy::detect` consumes the first bytes of a socket to classify it; the
//! HTTP pipeline must see those bytes again. [`PrefixedStream`] wraps the
//! live socket and drains the sniffed prefix ahead of it, across as many
//! polls as the reader's buffer demands, so the byte stream hyper receives
//! is exactly what the peer sent. Writes pass through untouched - the
//! prefix is read-side only.

use bytes::Bytes;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite};

/// A duplex stream with `prefix` re-injected ahead of `inner`.
pub(crate) struct PrefixedStream<S> {
    prefix: Bytes,
    pos: usize,
    inner: S,
}

impl<S> PrefixedStream<S> {
    pub(crate) fn new(prefix: Bytes, inner: S) -> Self {
        Self {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.pos < this.prefix.len() {
            let n = buf.remaining().min(this.prefix.len() - this.pos);
            buf.put_slice(&this.prefix[this.pos..this.pos + n]);
            this.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream};

    /// Stand-in for `detect`: strips the first `n` bytes off a duplex socket
    /// and hands back the stream plus the consumed prefix.
    async fn sniffed(mut sock: DuplexStream, n: usize) -> (Bytes, DuplexStream) {
        let mut head = vec![0u8; n];
        sock.read_exact(&mut head).await.unwrap();
        (Bytes::from(head), sock)
    }

    #[tokio::test]
    async fn prefix_and_rest_read_back_as_a_whole() {
        let payload = b"GET /health HTTP/1.1\r\nHost: t\r\n\r\nbody";
        let (mut peer, sock) = duplex(64);
        peer.write_all(payload).await.unwrap();
        drop(peer);
        let (prefix, sock) = sniffed(sock, 3).await;
        let mut io = PrefixedStream::new(prefix, sock);
        let mut whole = Vec::new();
        io.read_to_end(&mut whole).await.unwrap();
        assert_eq!(&whole[..], payload);
    }

    #[tokio::test]
    async fn prefix_spans_multiple_reads() {
        let (mut peer, sock) = duplex(64);
        peer.write_all(b"rest").await.unwrap();
        drop(peer);
        let (_prefix, sock) = sniffed(sock, 0).await;
        let mut io = PrefixedStream::new(Bytes::from_static(b"0123456789"), sock);
        // 4-byte reads: the 10-byte prefix must survive being split across
        // three polls before the inner stream is ever touched.
        let mut got = Vec::new();
        let mut buf = [0u8; 4];
        loop {
            let n = io.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(&got[..], b"0123456789rest");
    }

    #[tokio::test]
    async fn writes_pass_through_and_never_emit_the_prefix() {
        let (mut peer, sock) = duplex(64);
        let mut io = PrefixedStream::new(Bytes::from_static(b"GET "), sock);
        io.write_all(b"hello").await.unwrap();
        io.flush().await.unwrap();
        let mut back = [0u8; 5];
        peer.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"hello");
        // The write side carries only what was written: EOF, no prefix bytes.
        drop(io);
        let mut rest = Vec::new();
        peer.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
    }
}
