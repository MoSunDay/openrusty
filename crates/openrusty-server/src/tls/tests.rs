//! TLS termination tests: source classification, ALPN mapping over real
//! rustls handshakes, and listener-level e2e drills for both certificate
//! sources (static pair; ingress-rendered SNI table).
//!
//! Certificates come from the committed dummy fixtures
//! (`tests/fixtures/`, see their README); no external command is run.

use super::*;
use crate::h2c::ProtoMode;
use crate::listeners;
use crate::state::AppState;
use crate::testutil::{boot_state, fixture_path, free_port, spawn_echo_upstream, TmpDir};
use openrusty_core::config::{effective_listeners, ListenerConfig, ListenerRole};
use openrusty_k8s::render::TlsPair;
use rustls::pki_types::ServerName;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

const CERT_PEM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/server.crt");
const KEY_PEM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/server.key");
const FALLBACK_PEM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fallback.crt");
const FALLBACK_KEY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fallback.key");
const HOST: &str = "echo.example.com";
const TIMEOUT: Duration = Duration::from_secs(5);

/// rustls client trusting only the fixture CA (ALPN set per call).
fn client_config() -> rustls::ClientConfig {
    let ca = std::fs::read_to_string(fixture_path("ca.crt")).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut ca.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth()
}

/// Handshake with the listener, trusting the fixture CA. Err = the
/// handshake itself failed (that IS the assertion for SNI misses with
/// no fallback).
async fn connect_tls(
    port: u16,
    sni: &str,
    alpn: &[&[u8]],
) -> Result<TlsStream<TcpStream>, String> {
    let mut cfg = client_config();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let connector = TlsConnector::from(Arc::new(cfg));
    let sock = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .unwrap();
    let name = ServerName::try_from(sni.to_string()).unwrap();
    tokio::time::timeout(TIMEOUT, connector.connect(name, sock))
        .await
        .expect("handshake timed out")
        .map_err(|e| e.to_string())
}

fn alpn_of(tls: &TlsStream<TcpStream>) -> Option<Vec<u8>> {
    tls.get_ref().1.alpn_protocol().map(|p| p.to_vec())
}

/// First DER cert of a fixture PEM (identity of the served chain).
fn fixture_leaf_der(pem_path: &str) -> Vec<u8> {
    let pem = std::fs::read_to_string(pem_path).unwrap();
    let mut cursor = pem.as_bytes();
    let der = rustls_pemfile::certs(&mut cursor)
        .next()
        .unwrap()
        .unwrap();
    der.as_ref().to_vec()
}

/// The leaf the server actually presented on this handshake.
fn served_leaf(tls: &TlsStream<TcpStream>) -> Vec<u8> {
    tls.get_ref().1.peer_certificates().unwrap()[0]
        .as_ref()
        .to_vec()
}

/// HTTP/1.1 GET over the established TLS stream (connection: close
/// makes read-to-end the whole response).
async fn http_get(mut tls: TlsStream<TcpStream>, path: &str) -> String {
    let req = format!("GET {path} HTTP/1.1\r\nhost: {HOST}\r\nconnection: close\r\n\r\n");
    tls.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(TIMEOUT, tls.read_to_end(&mut buf))
        .await
        .expect("response read timed out")
        .unwrap();
    String::from_utf8_lossy(&buf).to_string()
}

/// Boot a gateway state + effective listeners from a generated config:
/// one catch-all route to an upstream on `upstream` (which may not be
/// listening - admin-plane probes never reach it), plus one TLS
/// listener block and optionally `[ingress] enabled`.
fn boot_with(upstream: u16, listener_block: &str, ingress: bool) -> (Arc<AppState>, Vec<ListenerConfig>) {
    let dir = TmpDir::new("tls");
    dir.write_config(&format!(
        r#"
[server]
listen = "127.0.0.1:0"

[plugins]
dir = "{plugins}"

[[upstreams]]
name = "u"
  [[upstreams.peers]]
  addr = "127.0.0.1:{upstream}"

[[routes]]
path_prefix = "/"
upstream = "u"

{listener_block}
{ingress_section}"#,
        plugins = dir.plugins_dir().display(),
        listener_block = listener_block,
        ingress_section = if ingress {
            "\n[ingress]\nenabled = true".to_string()
        } else {
            String::new()
        },
    ));
    let state = boot_state(&dir);
    let cfg = openrusty_core::load_config(&dir.config_path()).unwrap();
    (state, effective_listeners(&cfg))
}

fn static_listener(port: u16) -> String {
    // The static source is the *fallback default*: a wildcard leaf, the
    // realistic shape for a cert that must validate on SNI misses too.
    format!(
        "[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:{port}\"\ntls = true\ntls_cert = \"{FALLBACK_PEM}\"\ntls_key = \"{FALLBACK_KEY}\"\n"
    )
}

fn ingress_listener(port: u16) -> String {
    format!(
        "[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:{port}\"\ntls = true\n"
    )
}

#[test]
fn classification_and_role_scope() {
    let mk = |role, cert: Option<&str>, key: Option<&str>| ListenerConfig {
        role,
        listen: SocketAddr::from(([127, 0, 0, 1], 4143)),
        http1_only: false,
        transparent: false,
        detect_timeout_ms: 3_000,
        tls: true,
        tls_cert: cert.map(std::path::PathBuf::from),
        tls_key: key.map(std::path::PathBuf::from),
    };

    let static_only = mk(ListenerRole::Inbound, Some("/c.pem"), Some("/k.pem"));
    assert_eq!(
        TlsSources::classify(&static_only, false),
        TlsSources::Static {
            cert: "/c.pem".into(),
            key: "/k.pem".into()
        }
    );
    assert_eq!(
        TlsSources::classify(&static_only, true),
        TlsSources::StaticWithIngress {
            cert: "/c.pem".into(),
            key: "/k.pem".into()
        },
        "ingress priority: the static pair demotes to the SNI-miss fallback"
    );
    assert_eq!(
        TlsSources::classify(&mk(ListenerRole::Inbound, None, None), true),
        TlsSources::Ingress
    );

    // Role scope: outbound parses but ignores `tls`, inbound/admin serve it.
    assert!(!uses_tls(&mk(ListenerRole::Outbound, None, None)));
    assert!(uses_tls(&mk(ListenerRole::Inbound, None, None)));
    assert!(uses_tls(&mk(ListenerRole::Admin, None, None)));
}

/// ALPN -> ProtoMode mapping over real handshakes on in-memory IO:
/// h2 forces HTTP/2, http/1.1 is Auto, and no ALPN extension yields
/// None so the caller can apply the listener default.
#[tokio::test]
async fn alpn_picks_the_protocol_mode() {
    let plan = TlsPlan {
        resolver: Arc::new(DynamicCertResolver::new()),
        sources: TlsSources::Static {
            cert: CERT_PEM.into(),
            key: KEY_PEM.into(),
        },
    };
    let config = boot(&plan, false).unwrap();

    for (client_alpn, expect_mode, expect_wire) in [
        (&b"h2"[..], Some(ProtoMode::ForceHttp2), Some(&b"h2"[..])),
        (
            &b"http/1.1"[..],
            Some(ProtoMode::Auto),
            Some(&b"http/1.1"[..]),
        ),
    ] {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let cfg = config.clone();
        let (res_server, res_client) = tokio::join!(
            accept_tls(server, cfg),
            async move {
                let mut ccfg = client_config();
                ccfg.alpn_protocols = vec![client_alpn.to_vec()];
                TlsConnector::from(Arc::new(ccfg))
                    .connect(ServerName::try_from(HOST.to_string()).unwrap(), client)
                    .await
            }
        );
        let (_, mode) = res_server.unwrap();
        assert_eq!(mode, expect_mode, "server mode for ALPN {client_alpn:?}");
        assert_eq!(
            res_client.unwrap().get_ref().1.alpn_protocol(),
            expect_wire,
            "negotiated wire token for ALPN {client_alpn:?}"
        );
    }

    // No ALPN extension at all -> None (listener default applies).
    let (client, server) = tokio::io::duplex(64 * 1024);
    let cfg = config.clone();
    let (res_server, res_client) = tokio::join!(
        accept_tls(server, cfg),
        async move {
            TlsConnector::from(Arc::new(client_config()))
                .connect(ServerName::try_from(HOST.to_string()).unwrap(), client)
                .await
        }
    );
    let (_, mode) = res_server.unwrap();
    assert_eq!(mode, None);
    assert_eq!(res_client.unwrap().get_ref().1.alpn_protocol(), None);
}

/// Full assembly with a static certificate source: TLS handshake,
/// ALPN http/1.1, proxied GET through the echo upstream, the admin
/// plane on the same socket, SNI-miss falls back to the static pair,
/// and HTTP/2 via ALPN.
#[tokio::test]
async fn static_tls_listener_serves_http1_h2_and_fallback() {
    let upstream = spawn_echo_upstream().await;
    let port = free_port();
    let (state, ls) = boot_with(upstream, &static_listener(port), false);
    assert!(state.tls_resolver.is_some(), "static TLS listener seeds a resolver");
    let tasks =
        listeners::spawn(listeners::mounts(&state, &ls), &state.shutdown).await
            .unwrap();

    // SNI hit + ALPN http/1.1: the proxied GET reaches the upstream.
    // There is no SNI map here, so the static wildcard fallback serves.
    let tls = connect_tls(port, HOST, &[b"http/1.1"]).await.unwrap();
    assert_eq!(alpn_of(&tls), Some(b"http/1.1".to_vec()));
    assert_eq!(
        served_leaf(&tls),
        fixture_leaf_der(FALLBACK_PEM),
        "static-only source serves the fallback for every name"
    );
    let resp = http_get(tls, "/").await;
    assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp}");
    assert!(resp.ends_with("hello"), "got: {resp}");

    // The admin plane rides the same TLS listener (no dedicated admin
    // socket in this config, so the merged router serves /openrusty/*).
    let tls = connect_tls(port, HOST, &[b"http/1.1"]).await.unwrap();
    let resp = http_get(tls, "/openrusty/ready").await;
    assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp}");

    // SNI miss WITH a static source: the static pair is the fallback.
    let tls = connect_tls(port, "unknown.example.com", &[b"http/1.1"])
        .await
        .unwrap();
    let resp = http_get(tls, "/openrusty/ready").await;
    assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp}");

    // h2 over TLS via ALPN: the server runs the prior-knowledge HTTP/2
    // path, proven by a SETTINGS frame answering the h2 preface.
    let mut tls = connect_tls(port, HOST, &[b"h2"]).await.unwrap();
    assert_eq!(alpn_of(&tls), Some(b"h2".to_vec()));
    tls.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
        .await
        .unwrap();
    tls.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]).await.unwrap(); // empty SETTINGS
    let mut frame = [0u8; 9];
    tokio::time::timeout(TIMEOUT, tls.read_exact(&mut frame))
        .await
        .expect("SETTINGS read timed out")
        .unwrap();
    assert_eq!(frame[3], 0x04, "expected an HTTP/2 SETTINGS frame, got {frame:?}");

    for t in tasks {
        t.abort();
    }
}

/// Full assembly with an ingress-only certificate source: an empty SNI
/// table (and unknown names) fail the handshake; publishing rendered
/// secrets lights the host up - the closed loop the ingress apply runs.
#[tokio::test]
async fn ingress_source_publishes_and_unknown_sni_fails() {
    let upstream = spawn_echo_upstream().await;
    let port = free_port();
    let (state, ls) = boot_with(upstream, &ingress_listener(port), true);
    let resolver = state.tls_resolver.clone().expect("tls listener -> resolver");

    let tasks =
        listeners::spawn(listeners::mounts(&state, &ls), &state.shutdown).await
            .unwrap();
    // Nothing published yet and no static fallback: miss = handshake
    // failure.
    assert!(connect_tls(port, HOST, &[]).await.is_err());

    // Simulate the ingress apply step (apply_pair calls exactly this).
    let published = update_from_tls_pairs(
        &resolver,
        &BTreeMap::from([(
            HOST.to_string(),
            TlsPair {
                cert_pem: std::fs::read_to_string(CERT_PEM).unwrap(),
                key_pem: std::fs::read_to_string(KEY_PEM).unwrap(),
            },
        )]),
    );
    assert_eq!(published, 1);

    let tls = connect_tls(port, HOST, &[b"http/1.1"]).await.unwrap();
    assert_eq!(
        served_leaf(&tls),
        fixture_leaf_der(CERT_PEM),
        "SNI hit serves the published map entry, not a fallback"
    );
    let resp = http_get(tls, "/openrusty/ready").await;
    assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp}");

    // Still no fallback: an unknown name keeps failing after publish.
    assert!(connect_tls(port, "unknown.example.com", &[]).await.is_err());

    for t in tasks {
        t.abort();
    }
}

/// The bind phase is fail-fast on static material: an unreadable file
/// aborts `spawn` before anything serves.
#[tokio::test]
async fn unreadable_static_cert_aborts_the_bind_phase() {
    let listener_block = format!(
        "[[server.listeners]]\nrole = \"inbound\"\nlisten = \"127.0.0.1:{}\"\ntls = true\ntls_cert = \"/nonexistent/openrusty-cert.pem\"\ntls_key = \"/nonexistent/openrusty-key.pem\"\n",
        free_port()
    );
    let (state, ls) = boot_with(1, &listener_block, false);
    let err = listeners::spawn(listeners::mounts(&state, &ls), &state.shutdown)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("tls_cert"), "got: {err}");
}

#[test]
fn plain_state_builds_no_tls_resolver() {
    let dir = TmpDir::new("tls-plain");
    dir.write_config(&dir.standard_config());
    let state = boot_state(&dir);
    assert!(state.tls_resolver.is_none());
}
