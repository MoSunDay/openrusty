//! Original-destination lookup for transparently redirected connections.
//!
//! When the gateway runs as a transparent sidecar inside a pod, iptables or
//! nftables rules (`REDIRECT`, or the DNAT/TPROXY family) hijack application
//! traffic into the gateway listener. netfilter conntrack keeps the pre-NAT
//! destination of such a connection on the socket, and the kernel hands it
//! back through:
//!
//! - IPv4: `getsockopt(SOL_IP, SO_ORIGINAL_DST)` (option number `80`)
//! - IPv6: `getsockopt(SOL_IPV6, IP6T_SO_ORIGINAL_DST)` (option number `80`)
//!
//! The kernel answers by looking up conntrack's original-direction tuple,
//! so the reported address is only meaningful for connections netfilter
//! actually redirected. When conntrack holds no information for the socket
//! at all (module not loaded, untracked socket, no remote endpoint) the
//! syscall fails with an errno (`ENOENT` with the module loaded,
//! `ENOPROTOOPT` without it). Note that a merely *tracked* but un-NATed
//! connection does not fail: its original tuple equals the real
//! destination, so the real destination is reported. This module surfaces
//! the underlying `io::Error` unchanged, and callers must treat any error
//! as "not transparent" and fall back to explicitly configured upstreams.
//!
//! The primitive is a single pure function over a borrowed [`TcpStream`]:
//! the socket's local address family selects which option to query, no
//! state is kept, and every outcome is reported through the return value.

use std::io;
use std::net::SocketAddr;

use socket2::{SockAddr, SockRef};
use tokio::net::TcpStream;

/// Returns the original (pre-NAT) destination address of `stream`.
///
/// Dispatches on the address family of the socket's *local* address, which
/// conntrack leaves as the post-redirect (gateway) address: an IPv4 local
/// address queries `SO_ORIGINAL_DST` under `SOL_IP`, an IPv6 local address
/// queries `IP6T_SO_ORIGINAL_DST` under `SOL_IPV6`.
///
/// The reported address is only meaningful for connections that netfilter
/// redirected into the gateway (`REDIRECT`/`DNAT`); on a merely tracked
/// but un-NATed connection it degenerates to the real destination, and
/// when conntrack holds no information for the socket the underlying
/// `getsockopt` `io::Error` is returned unchanged.
pub fn original_dst(stream: &TcpStream) -> io::Result<SocketAddr> {
    let socket = SockRef::from(stream);
    match socket.local_addr()?.as_socket() {
        Some(SocketAddr::V4(_)) => original_dst_addr(socket.original_dst_v4()),
        Some(SocketAddr::V6(_)) => original_dst_addr(socket.original_dst_v6()),
        // TCP sockets are v4 or v6 today; be explicit instead of guessing.
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "orig_dst: unsupported local address family",
        )),
    }
}

/// Narrows socket2's [`SockAddr`] (which also covers non-IP families) down
/// to an IP [`SocketAddr`], keeping the public signature in `std` terms.
fn original_dst_addr(addr: io::Result<SockAddr>) -> io::Result<SocketAddr> {
    addr?.as_socket().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "orig_dst: original destination is not an IP address",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::io::{FromRawFd, IntoRawFd};

    use socket2::{Domain, SockAddr, Socket, Type};
    use tokio::net::{TcpListener, TcpStream};

    /// Error path, locked deterministically: a bound but never connected
    /// stream has no remote endpoint, hence no conntrack tuple, so the
    /// kernel must fail the getsockopt instead of inventing an address
    /// (`ENOENT` with conntrack loaded, `ENOPROTOOPT` without it). The
    /// success value under a real REDIRECT needs netns + iptables and is
    /// covered by the netns integration script (M1.8).
    #[tokio::test]
    async fn unconnected_socket_has_no_original_dst() {
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).expect("create socket");
        let any_v4 = SockAddr::from("127.0.0.1:0".parse::<SocketAddr>().unwrap());
        socket.bind(&any_v4).expect("bind socket");
        socket.set_nonblocking(true).expect("set nonblocking");

        // Hand the fd to a tokio stream; no I/O happens below, the stream
        // is only a borrowable handle for the getsockopt probe.
        let std_stream = unsafe { std::net::TcpStream::from_raw_fd(socket.into_raw_fd()) };
        let client = TcpStream::from_std(std_stream).expect("wrap stream");

        let err = original_dst(&client).expect_err("socket without conntrack tuple must fail");
        assert!(
            err.raw_os_error().is_some(),
            "expected a getsockopt errno, got: {err}"
        );
    }

    /// An ordinary loopback connection was never REDIRECT/DNATed, so the
    /// answer must be coherent with reality: either the kernel reports
    /// failure (conntrack does not track this connection at all), or it
    /// reports the original-direction tuple, which for an un-NATed
    /// connection equals the real destination. Any other address would be
    /// a false original dst and misroute transparent traffic.
    #[tokio::test]
    async fn plain_connection_reports_no_nat_or_real_destination() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let dst = listener.local_addr().expect("listener local addr");

        let client = TcpStream::connect(dst)
            .await
            .expect("connect to loopback listener");
        let _server = listener.accept().await.expect("accept loopback");

        assert!(client.local_addr().expect("client local addr").is_ipv4());
        match original_dst(&client) {
            Err(err) => assert!(
                err.raw_os_error().is_some(),
                "expected a getsockopt errno, got: {err}"
            ),
            Ok(got) => assert_eq!(
                got, dst,
                "un-NATed connection must report its real destination"
            ),
        }
    }

    /// Same coherence contract over IPv6, exercising the
    /// `IP6T_SO_ORIGINAL_DST` dispatch branch. Skipped on hosts without
    /// an IPv6 loopback.
    #[tokio::test]
    async fn plain_ipv6_connection_reports_no_nat_or_real_destination() {
        let listener = match TcpListener::bind("[::1]:0").await {
            Ok(listener) => listener,
            Err(err) if err.kind() == io::ErrorKind::AddrNotAvailable => return,
            Err(err) => panic!("bind ipv6 loopback listener: {err}"),
        };
        let dst = listener.local_addr().expect("listener local addr");

        let client = TcpStream::connect(dst)
            .await
            .expect("connect to loopback listener");
        let _server = listener.accept().await.expect("accept loopback");

        assert!(client.local_addr().expect("client local addr").is_ipv6());
        match original_dst(&client) {
            Err(err) => assert!(
                err.raw_os_error().is_some(),
                "expected a getsockopt errno, got: {err}"
            ),
            Ok(got) => assert_eq!(
                got, dst,
                "un-NATed connection must report its real destination"
            ),
        }
    }
}
