//! Loopback guard for transparently intercepted connections.
//!
//! Under iptables/nftables `REDIRECT` a hijacked connection carries the
//! pre-NAT destination in conntrack ([`orig_dst`]). If that destination is
//! the gateway's *own* listener port, forwarding it again would feed the
//! gateway's traffic back into itself - a forwarding loop. [`is_loopback`]
//! is the cheap refusal test for exactly that case.
//!
//! # Arming semantics (a known trap)
//!
//! **Enable this guard only on listeners with `transparent = true`.** On a
//! plain (non-transparent) listener netfilter conntrack still answers
//! successfully for tracked but un-NATed connections, and the value it
//! reports is then simply the *real* destination - which, for clients
//! connecting to the gateway, is the gateway's own port. Arming the guard
//! in that shape would misclassify every normal connection as a loop and
//! reject them all. With transparency the listener is necessarily behind a
//! REDIRECT rule, so an original destination equal to an own port can only
//! be a hijacked connection meant for the gateway itself: refuse it.
//!
//! # Port-only comparison
//!
//! Only the port is compared, never the address. After NAT the recovered
//! original address may legitimately be a ClusterIP, a NodeIP, or loopback
//! depending on hairpinning and service routing, so no address is a stable
//! discriminator; the port is. A different address on a guarded port is
//! still a loop and must be reported.

use std::net::SocketAddr;

/// Returns `true` when `original_dst` points at one of `own_ports`.
///
/// A `true` verdict means the connection must be rejected: tunnelling it
/// onward would recurse into the gateway. See the module docs for when the
/// guard may be armed (transparent listeners only) and why the check is
/// port-only.
pub fn is_loopback(original_dst: SocketAddr, own_ports: &[u16]) -> bool {
    own_ports.contains(&original_dst.port())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn v4(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 244, 1, 7)), port)
    }

    #[test]
    fn port_hit_is_a_loop() {
        assert!(is_loopback(v4(15006), &[8080, 15006]));
    }

    #[test]
    fn port_miss_is_not_a_loop() {
        assert!(!is_loopback(v4(9000), &[8080, 15006]));
    }

    #[test]
    fn empty_port_set_never_loops() {
        assert!(!is_loopback(v4(8080), &[]));
    }

    /// Only the port is compared: the same address on an unguarded port is
    /// a legitimate original destination, not a loop.
    #[test]
    fn same_address_different_port_is_not_a_loop() {
        assert!(!is_loopback(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9000),
            &[8080]
        ));
    }

    /// The address is ignored on purpose: post-NAT the original address can
    /// be ClusterIP, NodeIP, or loopback, so only the port is a stable
    /// discriminator - any address on a guarded port is a loop.
    #[test]
    fn different_address_same_port_is_a_loop() {
        assert!(is_loopback(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
            &[8080]
        ));
    }

    #[test]
    fn ipv6_destination_is_matched_by_port() {
        let dst = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 15006);
        assert!(is_loopback(dst, &[15006]));
    }
}
