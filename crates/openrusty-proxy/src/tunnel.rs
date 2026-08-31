//! Opaque TCP passthrough for transparently intercepted connections.
//!
//! A transparently intercepted socket carries a byte stream the gateway may
//! not speak (or chose not to inspect): once [`detect`] classifies a
//! connection as opaque the only correct action is to stop interpreting
//! bytes and shovel them verbatim between the client side and the original
//! destination. The same primitive serves both transparent directions:
//!
//! - *inbound*: a hijacked connection is tunneled to the address recovered
//!   by `orig_dst`, so the gateway behaves like a piece of wire;
//! - *outbound*: the gateway dials the original destination itself and
//!   tunnels to it, so egress keeps the tuple the application aimed at.
//!
//! [`tunnel`] is a deliberately thin wrapper over
//! [`tokio::io::copy_bidirectional`]; it adds nothing but the byte counts
//! and keeps the underlying semantics, locked by tests:
//!
//! - Copying runs in both directions concurrently. EOF observed in one
//!   direction shuts down the *opposite* write half (a propagated
//!   half-close) while the reverse direction keeps copying; the future
//!   completes `Ok((a_to_b, b_to_a))` only when both directions are done.
//! - Any I/O error aborts immediately with `Err` and some data may be
//!   lost; the caller must drop both ends. A peer that vanishes mid-stream
//!   surfaces as an error, never as a silent hang or panic.

use tokio::io::{AsyncRead, AsyncWrite};

/// Shovels bytes between `a` and `b` until both directions are finished.
///
/// Returns `(bytes a -> b, bytes b -> a)` on success. See the module docs
/// for the exact completion and error semantics inherited from
/// [`tokio::io::copy_bidirectional`].
pub async fn tunnel<A, B>(mut a: A, mut b: B) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    tokio::io::copy_bidirectional(&mut a, &mut b).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

    const LONG: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn round_trip_and_counts() {
        let (gw_a, mut cli) = duplex(64);
        let (gw_b, mut srv) = duplex(64);
        let task = tokio::spawn(tunnel(gw_a, gw_b));

        cli.write_all(b"ping").await.expect("client write");
        let mut buf = [0u8; 4];
        srv.read_exact(&mut buf).await.expect("server read");
        assert_eq!(&buf, b"ping");

        srv.write_all(b"pong!").await.expect("server write");
        let mut back = [0u8; 5];
        cli.read_exact(&mut back).await.expect("client read");
        assert_eq!(&back, b"pong!");

        // Both parties drop: each direction hits EOF and the tunnel ends.
        drop(cli);
        drop(srv);
        let (a2b, b2a) = tokio::time::timeout(LONG, task)
            .await
            .expect("tunnel finished")
            .expect("join")
            .expect("tunnel ok");
        assert_eq!((a2b, b2a), (4, 5));
    }

    /// Locks the half-close contract: a `shutdown()` on one side (no more
    /// writes, still reading) must NOT end the tunnel. `copy_bidirectional`
    /// propagates the EOF by shutting down the opposite write half, so the
    /// peer observes EOF, and the reverse direction keeps flowing to a full
    /// two-sided completion.
    #[tokio::test]
    async fn half_close_keeps_reverse_direction_flowing() {
        let (gw_a, mut cli) = duplex(64);
        let (gw_b, mut srv) = duplex(64);
        let task = tokio::spawn(tunnel(gw_a, gw_b));

        cli.write_all(b"ping").await.expect("client write");
        cli.shutdown().await.expect("client half-close");

        let mut buf = [0u8; 4];
        srv.read_exact(&mut buf).await.expect("server read ping");
        assert_eq!(&buf, b"ping");
        // Half-close arrived: EOF after the drained bytes, tunnel still up.
        assert_eq!(srv.read(&mut buf).await.expect("server sees EOF"), 0);

        srv.write_all(b"pong!").await.expect("server write");
        drop(srv);

        let mut rest = Vec::new();
        cli.read_to_end(&mut rest).await.expect("client drains");
        assert_eq!(rest, b"pong!");

        let (a2b, b2a) = tokio::time::timeout(LONG, task)
            .await
            .expect("tunnel finished")
            .expect("join")
            .expect("tunnel ok");
        assert_eq!((a2b, b2a), (4, 5));
    }

    /// Both ends already gone before the tunnel starts: two immediate EOFs,
    /// a clean completion with zero bytes, not an error and not a panic.
    #[tokio::test]
    async fn both_ends_closed_yields_zero_bytes() {
        let (gw_a, cli) = duplex(64);
        let (gw_b, srv) = duplex(64);
        drop(cli);
        drop(srv);

        let res = tokio::time::timeout(LONG, tunnel(gw_a, gw_b)).await;
        assert_eq!(res.expect("tunnel finished").expect("tunnel ok"), (0, 0));
    }

    /// A peer that vanishes after the tunnel picked up data: writing into
    /// the dropped end must surface as an `Err` (data may be lost) instead
    /// of hanging or panicking. The caller's contract is to close both ends.
    #[tokio::test]
    async fn vanished_peer_errors_without_panic() {
        let (gw_a, cli) = duplex(64);
        let (gw_b, mut srv) = duplex(64);
        drop(cli);
        srv.write_all(b"late").await.expect("buffered write");

        let res = tokio::time::timeout(LONG, tunnel(gw_a, gw_b)).await;
        let err = res
            .expect("tunnel finished")
            .expect_err("write into vanished peer must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }
}
