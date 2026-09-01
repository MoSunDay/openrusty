//! The `[egress]` configuration section: policy for transparently
//! intercepted *outbound* (egress) connections.
//!
//! Split out of `config.rs` to keep both files small; the parent module
//! re-exports [`EgressConfig`] and [`EgressMode`], so
//! `openrusty_core::config::*` paths are unchanged.
//!
//! Everything defaults to `direct`: a config without an `[egress]` section
//! keeps the historical behaviour (egress dialed straight at the recovered
//! original destination), so existing sidecar deployments see zero change.
//! The section only steers connections on a `transparent = true` outbound
//! listener; inbound and admin listeners never consult it.
//!
//! The three modes (`crates/openrusty-server/src/egress.rs` holds the full
//! decision matrix and its runtime):
//!
//! - `direct`  - tunnel verbatim to the original destination (default).
//! - `gateway` - forward plain HTTP (sniffed H1/H2) to an upstream egress
//!   gateway (another openrusty with its own inbound routing); opaque
//!   streams and port-443 destinations are refused (v1 boundary: TLS
//!   purposes cannot be carried over the plaintext gateway hop).
//! - `deny`    - refuse every intercepted egress connection (fail closed:
//!   a locked-down sidecar allows local serving only).
//!
//! `gateway` is a `host:port` *address*, never a URL, and deliberately
//! carries no credentials - a mesh-internal hop needs none.

use serde::Deserialize;
use std::net::ToSocketAddrs;

use super::ConfigError;

/// Egress disposition for intercepted outbound connections.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressMode {
    /// Dial the recovered original destination directly (default).
    #[default]
    Direct,
    /// Forward plain HTTP to `[egress].gateway`; refuse opaque/TLS traffic.
    Gateway,
    /// Refuse every intercepted egress connection.
    Deny,
}

/// Egress policy settings (`[egress]` in the TOML config).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressConfig {
    /// Disposition of intercepted outbound connections. Default `direct`.
    #[serde(default)]
    pub mode: EgressMode,
    /// `host:port` of the upstream egress gateway; required when (and only
    /// when) `mode = "gateway"`. Validated to be non-empty and resolvable
    /// at config-load time so a broken gateway address fails fast instead
    /// of failing every connection at runtime.
    #[serde(default)]
    pub gateway: String,
}

/// Resolve the configured gateway address once. Returns `None` when the
/// address is empty or unresolvable (callers fail closed in that case);
/// validation normally rejects both shapes before this ever runs.
pub fn resolve_gateway(cfg: &EgressConfig) -> Option<std::net::SocketAddr> {
    cfg.gateway
        .to_socket_addrs()
        .ok()?
        .find(|a| a.is_ipv4() || a.is_ipv6())
}

/// Pure validation of the `[egress]` section: cross-field invariants the
/// deserializer cannot express. An all-default section is always valid
/// (`direct`), so existing configs keep loading.
pub fn validate(cfg: &EgressConfig) -> Result<(), ConfigError> {
    let bad = |m: String| ConfigError::Invalid(m);
    if cfg.mode == EgressMode::Gateway {
        if cfg.gateway.trim().is_empty() {
            return Err(bad(
                "egress.gateway must be set when egress.mode = \"gateway\" (host:port of the \
                 upstream gateway)"
                    .to_string(),
            ));
        }
        if resolve_gateway(cfg).is_none() {
            return Err(bad(format!(
                "egress.gateway must be a resolvable host:port, got {:?}",
                cfg.gateway
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const MINIMAL: &str = r#"
[server]
listen = "127.0.0.1:8080"
"#;

    #[test]
    fn section_is_optional_and_direct() {
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        assert_eq!(cfg.egress.mode, EgressMode::Direct);
        assert_eq!(cfg.egress.gateway, "");
        assert_eq!(cfg.egress, EgressConfig::default());
        validate(&cfg.egress).unwrap();
    }

    #[test]
    fn explicit_direct_and_deny_parse() {
        let direct: Config =
            toml::from_str(&format!("{MINIMAL}\n[egress]\nmode = \"direct\"")).unwrap();
        assert_eq!(direct.egress.mode, EgressMode::Direct);
        validate(&direct.egress).unwrap();

        let deny: Config =
            toml::from_str(&format!("{MINIMAL}\n[egress]\nmode = \"deny\"")).unwrap();
        assert_eq!(deny.egress.mode, EgressMode::Deny);
        validate(&deny.egress).unwrap();
    }

    #[test]
    fn gateway_mode_with_address_parses_and_validates() {
        let cfg: Config = toml::from_str(&format!(
            "{MINIMAL}\n[egress]\nmode = \"gateway\"\ngateway = \"127.0.0.1:4140\""
        ))
        .unwrap();
        assert_eq!(cfg.egress.mode, EgressMode::Gateway);
        validate(&cfg.egress).unwrap();
        assert_eq!(
            resolve_gateway(&cfg.egress),
            Some("127.0.0.1:4140".parse().unwrap())
        );
    }

    #[test]
    fn gateway_mode_requires_an_address() {
        let cfg: Config =
            toml::from_str(&format!("{MINIMAL}\n[egress]\nmode = \"gateway\"")).unwrap();
        let err = validate(&cfg.egress).unwrap_err();
        assert!(err.to_string().contains("egress.gateway"), "got: {err}");
    }

    #[test]
    fn gateway_mode_rejects_an_unresolvable_address() {
        let cfg: Config = toml::from_str(&format!(
            "{MINIMAL}\n[egress]\nmode = \"gateway\"\ngateway = \"256.256.256.256:99999\""
        ))
        .unwrap();
        let err = validate(&cfg.egress).unwrap_err();
        assert!(err.to_string().contains("egress.gateway"), "got: {err}");
    }

    /// `direct`/`deny` never consult the gateway address: an unresolvable
    /// one stays inert instead of failing an unrelated configuration.
    #[test]
    fn non_gateway_modes_ignore_the_gateway_field() {
        let cfg: Config = toml::from_str(&format!(
            "{MINIMAL}\n[egress]\nmode = \"deny\"\ngateway = \"not-a-host:1\""
        ))
        .unwrap();
        validate(&cfg.egress).unwrap();
    }

    #[test]
    fn unknown_mode_is_a_serde_error() {
        let err = toml::from_str::<Config>(&format!("{MINIMAL}\n[egress]\nmode = \"forward\""))
            .unwrap_err();
        assert!(err.to_string().contains("unknown variant"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_fields() {
        let src = format!("{MINIMAL}\n[egress]\nmode = \"deny\"\nvip = \"10.0.0.9\"");
        let err = toml::from_str::<Config>(&src).unwrap_err();
        assert!(err.to_string().contains("vip"), "got: {err}");
    }
}
