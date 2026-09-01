//! Role-scoped listeners: `[[server.listeners]]` parsing and validation.
//!
//! Split out of `config.rs` to keep both files small; the parent module
//! re-exports [`ListenerRole`], [`ListenerConfig`] and
//! [`effective_listeners`], so `openrusty_core::config::*` paths are
//! unchanged.
//!
//! Transparency fields: `transparent` declares that an iptables/nftables
//! REDIRECT (or DNAT/TPROXY) rule sits in front of the socket, so a
//! connection's pre-NAT destination is recoverable and the gateway must
//! split by protocol instead of assuming HTTP. `detect_timeout_ms` bounds
//! that sniff on a transparent inbound. Both are meaningful only for
//! inbound/outbound: an **admin listener parses but ignores them** (the
//! management plane is never a REDIRECT target), kept permissive so the
//! same listener block can be templated across roles.
//!
//! TLS fields (`tls`, `tls_cert`, `tls_key`) follow the same shape: they
//! terminate TLS on `inbound`/`admin` sockets, an **outbound listener
//! parses but ignores them**, and validation requires a certificate
//! source (static pair, or `[ingress]`-rendered secrets) before the
//! switch is allowed to do anything.

use serde::Deserialize;
use std::collections::HashSet;
use std::net::SocketAddr;

use super::{Config, ConfigError};

/// Role of a listener socket, linkerd-aligned sidecar split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListenerRole {
    /// Data plane serving local workload traffic.
    Inbound,
    /// Data plane serving egress traffic.
    Outbound,
    /// Management plane: `/openrusty/*` routes only.
    Admin,
}

impl ListenerRole {
    /// Stable lowercase name, used in errors and log lines.
    pub fn as_str(&self) -> &'static str {
        match self {
            ListenerRole::Inbound => "inbound",
            ListenerRole::Outbound => "outbound",
            ListenerRole::Admin => "admin",
        }
    }
}

/// One role-scoped socket under `[[server.listeners]]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerConfig {
    pub role: ListenerRole,
    /// Address to bind for this role.
    pub listen: SocketAddr,
    /// Serve plain HTTP/1.1 only (skip the h2c preface sniffing). Only
    /// meaningful for inbound/outbound; an admin listener ignores it and
    /// always speaks both protocols.
    #[serde(default)]
    pub http1_only: bool,
    /// Declares that this socket sits behind a REDIRECT-style rule and must
    /// run the transparent intercept path (orig-dst recovery, loop guard,
    /// protocol detection, opaque tunneling). Only meaningful for
    /// inbound/outbound; an admin listener ignores it.
    #[serde(default)]
    pub transparent: bool,
    /// Total protocol-sniff budget for a transparent inbound connection
    /// (milliseconds): a peer silent past it is treated as opaque and
    /// tunneled. Only meaningful when `transparent` is true.
    #[serde(default = "default_detect_timeout_ms")]
    pub detect_timeout_ms: u64,
    /// Listener-level TLS termination: accept TLS on this socket, route by
    /// SNI, and serve HTTP/1.1 + h2 over the decrypted stream (ALPN picks
    /// the protocol, with the usual h2c sniffing as the no-ALPN fallback).
    /// Meaningful for `inbound` and `admin`; an **outbound listener parses
    /// but ignores it** (egress termination is not v1 scope), kept
    /// permissive so the same listener block can be templated across
    /// roles. Mutually exclusive with `transparent` on data-plane roles
    /// (v1 does not do TLS-over-transparent).
    #[serde(default)]
    pub tls: bool,
    /// Static certificate chain (PEM). Optional: when `[ingress]` is
    /// enabled a listener may run on rendered secrets alone. If both this
    /// source and ingress are configured, **ingress wins for every SNI
    /// name it serves and the static pair is the fallback default** for
    /// SNI misses (with no static pair a miss fails the handshake).
    #[serde(default)]
    pub tls_cert: Option<std::path::PathBuf>,
    /// Static private key (PEM) matching `tls_cert`. Read once at boot;
    /// reloads never re-read static material (ingress-driven rotation is
    /// the dynamic path).
    #[serde(default)]
    pub tls_key: Option<std::path::PathBuf>,
}

fn default_detect_timeout_ms() -> u64 {
    3_000
}

/// Effective listener set for the gateway (pure derivation).
///
/// `[[server.listeners]]` entries, when present, are the only authority: they are
/// returned verbatim and `server.listen` plays no further role. With no
/// `[[server.listeners]]` written, a single `inbound` listener is derived from
/// `server.listen` + `server.http1_only`, which reproduces the historical
/// single-socket behaviour exactly (including admin routes on that socket).
/// The derived listener is never transparent: the single-socket shape predates
/// interception and stays plain HTTP.
pub fn effective_listeners(cfg: &Config) -> Vec<ListenerConfig> {
    if !cfg.server.listeners.is_empty() {
        return cfg.server.listeners.clone();
    }
    vec![ListenerConfig {
        role: ListenerRole::Inbound,
        listen: cfg.server.listen,
        http1_only: cfg.server.http1_only,
        transparent: false,
        detect_timeout_ms: default_detect_timeout_ms(),
        tls: false,
        tls_cert: None,
        tls_key: None,
    }]
}

/// Listener slice of configuration validation; called from `config::validate`.
pub(super) fn validate_listeners(cfg: &Config) -> Result<(), ConfigError> {
    let bad = |m: &str| ConfigError::Invalid(m.to_string());
    // Multi-listener mode: [[server.listeners]] must not bind the same role or the
    // same address twice. There is intentionally no cross-check against
    // `server.listen`: when listeners are present, `server.listen` is dead
    // config (the gateway logs a warning at boot).
    if cfg.server.listeners.is_empty() {
        return Ok(());
    }
    let mut roles = HashSet::new();
    let mut addrs = HashSet::new();
    for (i, l) in cfg.server.listeners.iter().enumerate() {
        if !roles.insert(l.role) {
            return Err(bad(&format!(
                "listeners[{}] repeats role {}; one listener per role",
                i,
                l.role.as_str()
            )));
        }
        if !addrs.insert(l.listen) {
            return Err(bad(&format!(
                "listeners[{}] repeats address {}; addresses must be unique",
                i, l.listen
            )));
        }
        // A zero sniff budget would classify every transparent inbound as
        // opaque before a single byte could arrive, silently tunneling all
        // HTTP; reject it like the other zero timeouts.
        if l.transparent && l.detect_timeout_ms == 0 {
            return Err(bad(&format!(
                "listeners[{}] detect_timeout_ms must be > 0 when transparent is true",
                i
            )));
        }
        // TLS material rules. An outbound listener parses but ignores
        // `tls` (egress termination is not v1 scope), so it is exempt.
        if l.role == ListenerRole::Outbound {
            continue;
        }
        // v1 has no TLS-over-transparent: termination happens on the
        // clear-text face of the intercept, not the REDIRECTed one. The
        // admin listener ignores `transparent` outright, so no conflict
        // can arise there.
        if l.role != ListenerRole::Admin && l.transparent && l.tls {
            return Err(bad(&format!(
                "listeners[{}] transparent and tls are mutually exclusive (v1 has no TLS-over-transparent)",
                i
            )));
        }
        if !l.tls {
            // Stray material without the switch is almost certainly a
            // typo; reject instead of silently ignoring it.
            if l.tls_cert.is_some() || l.tls_key.is_some() {
                return Err(bad(&format!(
                    "listeners[{}] tls_cert/tls_key require tls = true",
                    i
                )));
            }
            continue;
        }
        let has_static = l.tls_cert.is_some() && l.tls_key.is_some();
        let partial = l.tls_cert.is_some() != l.tls_key.is_some();
        if !has_static && !cfg.ingress.enabled {
            if partial {
                return Err(bad(&format!(
                    "listeners[{}] tls = true needs BOTH tls_cert and tls_key, got only one",
                    i
                )));
            }
            return Err(bad(&format!(
                "listeners[{}] tls = true needs a certificate source: static tls_cert + tls_key, or [ingress] enabled (rendered secrets)",
                i
            )));
        }
        // has_static && ingress.enabled is valid on purpose: ingress owns
        // the SNI map (its rendered secrets win), the static pair is the
        // fallback default for SNI misses.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal parseable config around one listener block (`[plugins]` is
    /// required because `validate` checks `plugins.timeout_ms`, which only
    /// gets its non-zero default from the TOML section).
    fn with_listener(block: &str) -> Config {
        toml::from_str(&format!(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\n[plugins]\ndir = \"build/plugins\"\n\n{block}"
        ))
        .unwrap()
    }

    #[test]
    fn defaults_are_plain_and_three_second_sniff() {
        let cfg = with_listener(
            "[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:4143\"\n",
        );
        let l = &cfg.server.listeners[0];
        assert!(!l.transparent);
        assert!(!l.http1_only);
        assert!(!l.tls);
        assert!(l.tls_cert.is_none());
        assert!(l.tls_key.is_none());
        assert_eq!(l.detect_timeout_ms, 3_000);
        super::super::validate(&cfg).unwrap();
    }

    #[test]
    fn transparent_fields_parse_verbatim() {
        let cfg = with_listener(
            "[[server.listeners]]\nrole = \"inbound\"\nlisten = \"0.0.0.0:4143\"\ntransparent = true\ndetect_timeout_ms = 250\n",
        );
        let l = &cfg.server.listeners[0];
        assert!(l.transparent);
        assert_eq!(l.detect_timeout_ms, 250);
        super::super::validate(&cfg).unwrap();
    }

    #[test]
    fn admin_listener_parses_but_ignores_transparent_fields() {
        // Allowed on purpose (templatable listener blocks); validate must not
        // reject, and the assembly layer is what ignores the flag.
        let cfg = with_listener(
            "[[server.listeners]]\nrole = \"admin\"\nlisten = \"127.0.0.1:4191\"\ntransparent = true\ndetect_timeout_ms = 100\n",
        );
        assert!(cfg.server.listeners[0].transparent);
        super::super::validate(&cfg).unwrap();
    }

    #[test]
    fn rejects_zero_detect_timeout_on_transparent_listener() {
        let cfg = with_listener(
            "[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:4143\"\ntransparent = true\ndetect_timeout_ms = 0\n",
        );
        let err = super::super::validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("detect_timeout_ms"),
            "unexpected error: {err}"
        );
        // Without transparency the budget is inert, so 0 stays acceptable.
        let cfg = with_listener(
            "[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:4143\"\ndetect_timeout_ms = 0\n",
        );
        super::super::validate(&cfg).unwrap();
    }

    #[test]
    fn tls_accepts_static_or_ingress_or_both() {
        // Static source only (ingress off).
        let cfg = with_listener(
            "[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:4143\"\ntls = true\ntls_cert = \"/tmp/tls/cert.pem\"\ntls_key = \"/tmp/tls/key.pem\"\n",
        );
        let l = &cfg.server.listeners[0];
        assert!(l.tls);
        assert_eq!(
            l.tls_cert.as_deref(),
            Some(std::path::Path::new("/tmp/tls/cert.pem"))
        );
        super::super::validate(&cfg).unwrap();

        // Dynamic source only (ingress on, no static material).
        let cfg = with_listener(
            "[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:4143\"\ntls = true\n\n[ingress]\nenabled = true\n",
        );
        assert!(cfg.server.listeners[0].tls_cert.is_none());
        super::super::validate(&cfg).unwrap();

        // Both sources: valid on purpose. Priority is ingress-wins with
        // the static pair as the SNI-miss fallback (assembly-level rule).
        let cfg = with_listener(
            "[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:4143\"\ntls = true\ntls_cert = \"/tmp/tls/cert.pem\"\ntls_key = \"/tmp/tls/key.pem\"\n\n[ingress]\nenabled = true\n",
        );
        super::super::validate(&cfg).unwrap();
    }

    #[test]
    fn rejects_tls_without_any_certificate_source() {
        let cfg = with_listener(
            "[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:4143\"\ntls = true\n",
        );
        let err = super::super::validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("certificate source"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_tls_with_only_one_static_file() {
        let cfg = with_listener(
            "[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:4143\"\ntls = true\ntls_cert = \"/tmp/tls/cert.pem\"\n",
        );
        let err = super::super::validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("BOTH tls_cert and tls_key"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_tls_over_transparent_intercept() {
        let cfg = with_listener(
            "[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:4143\"\ntransparent = true\ntls = true\ntls_cert = \"/c.pem\"\ntls_key = \"/k.pem\"\n",
        );
        let err = super::super::validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("mutually exclusive"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_stray_tls_material_without_the_switch() {
        let cfg = with_listener(
            "[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:4143\"\ntls_cert = \"/c.pem\"\n",
        );
        let err = super::super::validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("require tls = true"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn outbound_listener_parses_but_ignores_tls() {
        // Templatable like the transparent fields: no material needed, no
        // rejection - the assembly layer simply never terminates TLS there.
        let cfg = with_listener(
            "[[server.listeners]]\nrole = \"outbound\"\nlisten = \"127.0.0.1:4140\"\ntls = true\n",
        );
        super::super::validate(&cfg).unwrap();
    }

    #[test]
    fn empty_listeners_derive_inbound_from_server_listen() {
        // http1_only must propagate into the derived listener too.
        let cfg: Config = toml::from_str(
            "[server]\nlisten = \"127.0.0.1:18080\"\nhttp1_only = true\n\n[plugins]\ndir = \"build/plugins\"\n",
        )
        .unwrap();
        super::super::validate(&cfg).unwrap();
        let ls = effective_listeners(&cfg);
        assert_eq!(ls.len(), 1);
        assert_eq!(ls[0].role, ListenerRole::Inbound);
        assert_eq!(ls[0].listen, "127.0.0.1:18080".parse().unwrap());
        assert!(ls[0].http1_only);
        // The derived single-socket shape is never transparent.
        assert!(!ls[0].transparent);
    }

    #[test]
    fn explicit_listeners_are_authoritative_and_verbatim() {
        let cfg = with_listener(
            "[[server.listeners]]\nrole = \"inbound\"\nlisten = \"0.0.0.0:4143\"\n\n[[server.listeners]]\nrole = \"outbound\"\nlisten = \"127.0.0.1:4140\"\nhttp1_only = true\n\n[[server.listeners]]\nrole = \"admin\"\nlisten = \"127.0.0.1:4191\"\n",
        );
        super::super::validate(&cfg).unwrap();
        let ls = effective_listeners(&cfg);
        let roles: Vec<_> = ls.iter().map(|l| l.role).collect();
        assert_eq!(
            roles,
            [
                ListenerRole::Inbound,
                ListenerRole::Outbound,
                ListenerRole::Admin
            ]
        );
        // server.listen is dead config here; listeners win verbatim.
        assert_eq!(ls[0].listen, "0.0.0.0:4143".parse().unwrap());
        assert!(ls[1].http1_only);
        assert!(!ls[2].http1_only);
    }

    #[test]
    fn rejects_duplicate_listener_role() {
        let cfg = with_listener(
            "[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:4143\"\n\n[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:4144\"\n",
        );
        let err = super::super::validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("repeats role inbound"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_duplicate_listener_address() {
        let cfg = with_listener(
            "[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:4143\"\n\n[[server.listeners]]\nrole = \"admin\"\nlisten = \"127.0.0.1:4143\"\n",
        );
        let err = super::super::validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("repeats address 127.0.0.1:4143"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_unknown_listener_role_and_fields() {
        // Unknown role names are rejected by the serde enum.
        assert!(toml::from_str::<Config>(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[server.listeners]]\nrole = \"east-west\"\nlisten = \"127.0.0.1:4143\"\n"
        )
        .is_err());
        // Unknown fields are rejected by deny_unknown_fields.
        assert!(toml::from_str::<Config>(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[server.listeners]]\nrole = \"admin\"\nlisten = \"127.0.0.1:4191\"\nnope = 1\n"
        )
        .is_err());
    }
}
