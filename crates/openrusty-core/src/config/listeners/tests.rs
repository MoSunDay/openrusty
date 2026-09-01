//! Unit tests for [`super`]: `server.listeners` parsing and validation.
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
    let cfg =
        with_listener("[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:4143\"\n");
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
