//! systemd socket-activation fd inheritance (the sd_listen_fds(3)
//! protocol).
//!
//! When the gateway starts with `LISTEN_FDS`/`LISTEN_PID` set - a systemd
//! `openrusty.socket` unit, or a future exec-based USR2 binary upgrade
//! where the child re-execs itself with the same `LISTEN_*` convention and
//! calls the same [`adopt`] path - [`adopt`] claims the inherited listener
//! fds instead of binding: the sockets are matched to the configured
//! `[[server.listeners]]` addresses by local port, reordered into config
//! order, and handed to tokio. A set of inherited fds that does not match
//! the configuration is a hard error, mirroring the fail-fast bind
//! contract: the gateway would otherwise serve on ports nobody configured.
//!
//! The logic is split so tests never touch the real environment (parallel
//! test threads would race on `std::env`) nor depend on fd 3 being live:
//! [`parse_activation`] reads a closure instead of the process env,
//! [`match_ports`] is a pure index computation over ports, and
//! [`adopt_fds`] takes explicit fd numbers (the systemd path is always
//! `3..3+n`).

use std::io::{Error, ErrorKind};
use std::net::SocketAddr;
use std::os::fd::{FromRawFd, RawFd};

/// Parsed `LISTEN_*` environment: how many fds were passed (fds `3..3+n`
/// per the protocol) and, when systemd also set `LISTEN_FDNAMES`, one name
/// per fd (`None` when unnamed, or when the name list is shorter than the
/// fd count - the fd count is the truth, the names are decoration).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    pub fds: usize,
    pub names: Vec<Option<String>>,
}

/// Parse the activation environment against one expected pid. `None`
/// means "not activated": `LISTEN_FDS` missing, `"0"` or unparseable, or
/// `LISTEN_PID` absent/unparseable/not our pid (the sockets were meant
/// for a different process - including a parent that forgot to unset
/// them).
fn parse_activation(vars: &dyn Fn(&str) -> Option<String>, pid: u32) -> Option<Activation> {
    let fds: usize = vars("LISTEN_FDS")?.trim().parse().ok()?;
    if fds == 0 {
        return None;
    }
    let listen_pid = vars("LISTEN_PID").and_then(|v| v.trim().parse::<u32>().ok());
    if listen_pid != Some(pid) {
        return None;
    }
    // `LISTEN_FDNAMES` is optional, colon-separated, aligned with the fds
    // by position; pad (unnamed) or truncate to the fd count.
    let mut names: Vec<Option<String>> = match vars("LISTEN_FDNAMES") {
        Some(raw) => raw
            .split(':')
            .map(|n| {
                if n.is_empty() {
                    None
                } else {
                    Some(n.to_owned())
                }
            })
            .collect(),
        None => Vec::new(),
    };
    names.resize(fds, None);
    names.truncate(fds);
    Some(Activation { fds, names })
}

/// Whether this process was socket-activated, per the real environment.
pub fn activated() -> Option<Activation> {
    parse_activation(&|k| std::env::var(k).ok(), std::process::id())
}

/// Pure port matching: for each expected listener (in order) the index of
/// the inherited fd whose local port equals the expected port. Inherited
/// fds are positional - index `i` is fd `3 + i`, the sd_listen_fds(3)
/// convention - so returned indices double as fd numbers for diagnostics.
/// Every inherited fd must be consumed by exactly one expected listener
/// and vice versa; a missing port, a leftover fd or duplicates that leave
/// either side unmatched fail with both sides printed.
fn match_ports(
    inherited: &[u16],
    names: &[Option<String>],
    expected: &[SocketAddr],
) -> Result<Vec<usize>, String> {
    let mismatch = || mismatch_message(inherited, names, expected);
    let mut used = vec![false; inherited.len()];
    let mut order = Vec::with_capacity(expected.len());
    for addr in expected {
        let port = addr.port();
        match (0..inherited.len()).find(|&i| !used[i] && inherited[i] == port) {
            Some(i) => {
                used[i] = true;
                order.push(i);
            }
            None => return Err(mismatch()),
        }
    }
    if used.iter().any(|u| !*u) {
        return Err(mismatch());
    }
    Ok(order)
}

/// Render both sides of a failed match: the configured addresses vs the
/// inherited fds (fd number, name when systemd gave one, local port).
fn mismatch_message(
    inherited: &[u16],
    names: &[Option<String>],
    expected: &[SocketAddr],
) -> String {
    let expected = expected
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let received = inherited
        .iter()
        .enumerate()
        .map(|(i, port)| match names.get(i).and_then(|n| n.as_deref()) {
            Some(name) => format!("fd{}({})={}", 3 + i, name, port),
            None => format!("fd{}={}", 3 + i, port),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("socket activation mismatch: expected [{expected}], received [{received}]")
}

/// Take ownership of the given fds, port-match them to `expected`, and
/// convert them to tokio listeners in expected order. Every fd is claimed
/// with `from_raw_fd` up front - ownership is taken immediately, so every
/// error path below drops the already-adopted listeners and closes their
/// fds instead of leaking them. A fd whose local address cannot be read
/// (e.g. a unix-socket fd systemd passed where a TCP listener was
/// expected) is named explicitly; `std::net::SocketAddr` can only be
/// IPv4/IPv6, so a readable address already implies an internet-family
/// socket.
fn adopt_fds(
    fds: &[RawFd],
    names: &[Option<String>],
    expected: &[SocketAddr],
) -> std::io::Result<Vec<tokio::net::TcpListener>> {
    // SAFETY: the caller (systemd or the exec parent) handed us ownership
    // of these fds; wrapping each in a TcpListener makes the Rust ownership
    // model close them on every path out of this function.
    let adopted: Vec<std::net::TcpListener> = fds
        .iter()
        .map(|&fd| unsafe { std::net::TcpListener::from_raw_fd(fd) })
        .collect();
    let ports: Vec<u16> = adopted
        .iter()
        .zip(fds)
        .map(|(l, fd)| {
            l.local_addr().map(|a| a.port()).map_err(|_| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "inherited fd {fd} is not an IPv4/IPv6 TCP listener \
                         (local address unavailable: unix or datagram socket?)"
                    ),
                )
            })
        })
        .collect::<std::io::Result<_>>()?;
    let order = match_ports(&ports, names, expected)
        .map_err(|msg| Error::new(ErrorKind::InvalidInput, msg))?;
    // Reorder into config order, then hand each socket to tokio: accept
    // needs a non-blocking socket registered with the reactor.
    let mut sockets: Vec<Option<std::net::TcpListener>> = adopted.into_iter().map(Some).collect();
    let mut listeners = Vec::with_capacity(order.len());
    for i in order {
        let l = sockets[i]
            .take()
            .expect("match_ports returns a permutation");
        l.set_nonblocking(true)?;
        listeners.push(tokio::net::TcpListener::from_std(l)?);
    }
    Ok(listeners)
}

/// Inherit the listener fds when this process is socket-activated.
/// `Ok(None)` = not activated; the caller binds instead. When activated,
/// fds `3..3+n` are adopted and returned in `expected` order (one per
/// configured address). Any surprise - wrong count, wrong ports, a
/// non-TCP fd - is a fail-fast error, exactly like a failed bind.
pub fn adopt(expected: &[SocketAddr]) -> std::io::Result<Option<Vec<tokio::net::TcpListener>>> {
    let Some(activation) = activated() else {
        return Ok(None);
    };
    let fds: Vec<RawFd> = (3..3 + activation.fds as RawFd).collect();
    adopt_fds(&fds, &activation.names, expected).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    fn activate(pairs: &[(&str, &str)], pid: u32) -> Option<Activation> {
        let get = |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| (*v).to_string())
        };
        parse_activation(&get, pid)
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[test]
    fn parse_activation_accepts_matching_pid_and_names() {
        let a = activate(
            &[
                ("LISTEN_FDS", "2"),
                ("LISTEN_PID", "42"),
                ("LISTEN_FDNAMES", "inbound:admin"),
            ],
            42,
        )
        .unwrap();
        assert_eq!(
            a,
            Activation {
                fds: 2,
                names: vec![Some("inbound".into()), Some("admin".into())]
            }
        );
    }

    #[test]
    fn parse_activation_rejects_foreign_missing_or_zero() {
        // PID belongs to another process: the sockets are not ours.
        assert!(activate(&[("LISTEN_FDS", "1"), ("LISTEN_PID", "41")], 42).is_none());
        // LISTEN_PID missing or unparseable.
        assert!(activate(&[("LISTEN_FDS", "1")], 42).is_none());
        assert!(activate(&[("LISTEN_FDS", "1"), ("LISTEN_PID", "self")], 42).is_none());
        // LISTEN_FDS missing, zero, or not a number.
        assert!(activate(&[("LISTEN_PID", "42")], 42).is_none());
        assert!(activate(&[("LISTEN_FDS", "0"), ("LISTEN_PID", "42")], 42).is_none());
        assert!(activate(&[("LISTEN_FDS", "two"), ("LISTEN_PID", "42")], 42).is_none());
    }

    #[test]
    fn parse_activation_pads_short_name_lists() {
        // One name for two fds: the second fd is unnamed.
        let a = activate(
            &[
                ("LISTEN_FDS", "2"),
                ("LISTEN_PID", "7"),
                ("LISTEN_FDNAMES", "inbound"),
            ],
            7,
        )
        .unwrap();
        assert_eq!(a.names, vec![Some("inbound".to_string()), None]);
        // No LISTEN_FDNAMES at all: every fd unnamed.
        let b = activate(&[("LISTEN_FDS", "1"), ("LISTEN_PID", "7")], 7).unwrap();
        assert_eq!(b.names, vec![None]);
    }

    #[test]
    fn match_ports_exact_and_reordered() {
        let none: Vec<Option<String>> = Vec::new();
        assert_eq!(
            match_ports(&[8080, 8081], &none, &[addr(8080), addr(8081)]),
            Ok(vec![0, 1])
        );
        // systemd order differs from config order: indices reorder.
        assert_eq!(
            match_ports(&[8081, 8080], &none, &[addr(8080), addr(8081)]),
            Ok(vec![1, 0])
        );
    }

    #[test]
    fn match_ports_rejects_leftover_missing_and_duplicates() {
        let none: Vec<Option<String>> = Vec::new();
        // Leftover inherited fd nobody configured.
        let err = match_ports(&[8080, 8090], &none, &[addr(8080)]).unwrap_err();
        assert_eq!(
            err,
            "socket activation mismatch: expected [127.0.0.1:8080], received [fd3=8080, fd4=8090]"
        );
        // Configured listener with no inherited fd for it.
        let err = match_ports(&[8090], &none, &[addr(8080), addr(8090)]).unwrap_err();
        assert!(
            err.contains("expected [127.0.0.1:8080, 127.0.0.1:8090]"),
            "{err}"
        );
        assert!(err.contains("received [fd3=8090]"), "{err}");
        // Duplicate inherited port against one configured listener.
        assert!(match_ports(&[8080, 8080], &none, &[addr(8080)]).is_err());
        // Duplicate expected port against one inherited fd.
        assert!(match_ports(&[8080], &none, &[addr(8080), addr(8080)]).is_err());
        // Names decorate the received side when present.
        let named = vec![Some("inbound".to_string())];
        let err = match_ports(&[8080], &named, &[addr(8090)]).unwrap_err();
        assert!(err.contains("received [fd3(inbound)=8080]"), "{err}");
    }

    /// The one real-fd test: no environment, no assumed fd numbers. A pair
    /// of listeners is bound on ephemeral ports; `try_clone` hands copies
    /// to `adopt_fds` (from_raw_fd takes ownership, the originals must stay
    /// valid) and the adoption must return them in expected order, accept a
    /// real connection, and fail fast with the mismatch message otherwise.
    #[test]
    fn adopt_fds_adopts_real_listeners_and_reports_mismatch() {
        let a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let wrong = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr_a = a.local_addr().unwrap();
        let addr_b = b.local_addr().unwrap();
        let addr_wrong = wrong.local_addr().unwrap();

        // Mismatch path: the clone's fd is adopted, matched, rejected and
        // closed; the original stays alive in `wrong`. Handing an fd to
        // adopt_fds transfers its ownership, so the clone wrapper must be
        // forgotten - dropping it would close the fd a second time (std
        // aborts on that).
        let wrong_clone = wrong.try_clone().unwrap();
        let fd_wrong = wrong_clone.as_raw_fd();
        std::mem::forget(wrong_clone);
        let err = adopt_fds(&[fd_wrong], &[], &[addr_a]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains(&format!(
                "socket activation mismatch: expected [{addr_a}], received [fd3={}]",
                addr_wrong.port()
            )),
            "{err}"
        );

        // Happy path: fds passed out of config order come back reordered.
        // Same ownership rule: the clones are forgotten, the adopted
        // listeners own their fds from here on.
        let ca = a.try_clone().unwrap();
        let cb = b.try_clone().unwrap();
        let fd_ca = ca.as_raw_fd();
        let fd_cb = cb.as_raw_fd();
        std::mem::forget((ca, cb));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let adopted = adopt_fds(&[fd_cb, fd_ca], &[], &[addr_a, addr_b]).unwrap();
            assert_eq!(adopted.len(), 2);
            assert_eq!(adopted[0].local_addr().unwrap(), addr_a);
            assert_eq!(adopted[1].local_addr().unwrap(), addr_b);
            // Liveness: a connection dialed to the adopted socket is
            // accepted through the inherited fd.
            let dial = tokio::spawn(tokio::net::TcpStream::connect(addr_a));
            let (_conn, peer) = adopted[0].accept().await.unwrap();
            let dialer = dial.await.unwrap().unwrap();
            assert_eq!(peer.port(), dialer.local_addr().unwrap().port());
        });
    }
}
