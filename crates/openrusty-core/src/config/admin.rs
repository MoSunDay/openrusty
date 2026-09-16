//! Optional management-plane authentication (`[admin]` section).

use serde::Deserialize;

/// `[admin]` — management-plane hardening knobs. The section is optional
/// and empty by default: without a token every `/openrusty/*` endpoint
/// stays open and loopback / listener-role isolation remains the sole
/// control (the historical default).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    /// Shared secret for `/openrusty/*` endpoints except `ready`/`live`
    /// (those stay open for LB / k8s probes). Clients present it as
    /// `Authorization: Bearer <token>` or `X-OpenRusty-Token: <token>`.
    /// A rotated value takes effect on the next config reload.
    #[serde(default)]
    pub token: Option<String>,
}

impl AdminConfig {
    /// The active secret, if any: blank/whitespace-only values disable
    /// auth so a placeholder `token = ""` behaves like no token.
    pub fn effective_token(&self) -> Option<&str> {
        self.token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_section_deserializes_to_default() {
        let cfg: AdminConfig = toml::from_str("").unwrap();
        assert_eq!(cfg, AdminConfig::default());
        assert_eq!(cfg.effective_token(), None);
    }

    #[test]
    fn token_parses() {
        let cfg: AdminConfig = toml::from_str("token = \"s\"").unwrap();
        assert_eq!(cfg.effective_token(), Some("s"));
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = toml::from_str::<AdminConfig>("whoops = 1").unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn blank_tokens_disable_auth() {
        let empty: AdminConfig = toml::from_str("token = \"\"").unwrap();
        assert_eq!(empty.effective_token(), None);
        let blank: AdminConfig = toml::from_str("token = \"   \"").unwrap();
        assert_eq!(blank.effective_token(), None);
    }
}
