//! Unit tests for [`super`]: kubeconfig loading and exec-credential wiring.
use super::*;
use crate::auth::ExecConfig;
use std::fs;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn endpoint() -> Uri {
    Uri::from_static("https://apiserver.example.invalid:6443")
}

#[test]
fn watch_path_pins_resource_version_and_bookmarks() {
    assert_eq!(
        watch_path("/api/v1/ingresses", "4215"),
        "/api/v1/ingresses?watch=1&allowWatchBookmarks=true&resourceVersion=4215"
    );
    assert_eq!(
        watch_path("/api/v1/ingresses", ""),
        "/api/v1/ingresses?watch=1&allowWatchBookmarks=true"
    );
}

/// A path that already carries a query (the TLS fieldSelector on the
/// secrets watch) must be joined with `&`, never a second `?`.
#[test]
fn watch_path_joins_an_existing_query() {
    let path = "/api/v1/secrets?fieldSelector=type%3Dkubernetes.io%2Ftls";
    assert_eq!(
        watch_path(path, "7"),
        format!("{path}&watch=1&allowWatchBookmarks=true&resourceVersion=7")
    );
    assert_eq!(
        watch_path(path, ""),
        format!("{path}&watch=1&allowWatchBookmarks=true")
    );
}

#[test]
fn request_uri_joins_endpoint_and_path() {
    let uri = request_uri(&endpoint(), "api/v1/namespaces/default/secrets").unwrap();
    assert_eq!(
        uri.to_string(),
        "https://apiserver.example.invalid:6443/api/v1/namespaces/default/secrets"
    );
    let uri = request_uri(&endpoint(), "/x?watch=1").unwrap();
    assert_eq!(
        uri.to_string(),
        "https://apiserver.example.invalid:6443/x?watch=1"
    );
}

#[test]
fn request_uri_rejects_authorityless_endpoint() {
    let bare = Uri::from_static("/only/a/path");
    assert!(request_uri(&bare, "/x").is_err());
}

#[test]
fn tls_config_accepts_valid_ca_and_rejects_garbage() {
    let ca = fs::read(fixture("test-ca.crt")).unwrap();
    assert!(build_tls_config(&ca, None).is_ok());

    let broken = fs::read(fixture("broken-ca.crt")).unwrap();
    let err = build_tls_config(&broken, None).unwrap_err();
    assert!(matches!(err, K8sError::Tls(_)), "{err}");
}

#[test]
fn tls_config_rejects_broken_client_cert_with_valid_ca() {
    let ca = fs::read(fixture("test-ca.crt")).unwrap();
    let err = build_tls_config(&ca, Some((b"not a pem".as_slice(), b"".as_slice()))).unwrap_err();
    assert!(matches!(err, K8sError::Tls(_)), "{err}");
}

#[test]
fn client_resolves_bearer_credentials_without_network() {
    let cluster = Cluster {
        endpoint: endpoint(),
        ca: fs::read(fixture("test-ca.crt")).unwrap(),
    };
    let client = Client::new(
        &cluster,
        &Credentials::Bearer("dummy-token-for-tests".to_string()),
    )
    .unwrap();
    let request = client
        .request_builder(client.uri_for("/api/v1/x").unwrap())
        .body(Full::new(Bytes::new()))
        .unwrap();
    assert_eq!(
        request.headers().get(AUTHORIZATION).unwrap(),
        "Bearer dummy-token-for-tests"
    );
    assert_eq!(
        request.headers().get(hyper::header::ACCEPT).unwrap(),
        "application/json"
    );
}

#[test]
fn client_cert_credentials_feed_tls_layer() {
    let cluster = Cluster {
        endpoint: endpoint(),
        ca: vec![],
    };
    // Dummy (invalid) PEM: construction must fail at the TLS layer,
    // proving the client-cert branch feeds material into rustls
    // instead of the Authorization header.
    let err = Client::new(
        &cluster,
        &Credentials::ClientCert {
            cert: b"not a pem".to_vec(),
            key: b"not a pem".to_vec(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, K8sError::Tls(_)), "{err}");
}

#[test]
fn exec_credentials_are_resolved_at_construction() {
    let cluster = Cluster {
        endpoint: endpoint(),
        ca: fs::read(fixture("test-ca.crt")).unwrap(),
    };
    let credentials = Credentials::ExecToken(ExecConfig {
        command: "echo".to_string(),
        args: vec![r#"{"apiVersion":"client.authentication.k8s.io/v1","status":{"token":"dummy-exec-token"}}"#.to_string()],
        env: vec![],
        api_version: Some("client.authentication.k8s.io/v1".to_string()),
    });
    let client = Client::new(&cluster, &credentials).unwrap();
    let request = client
        .request_builder(client.uri_for("/api/v1/x").unwrap())
        .body(Full::new(Bytes::new()))
        .unwrap();
    assert_eq!(
        request.headers().get(AUTHORIZATION).unwrap(),
        "Bearer dummy-exec-token"
    );
}

#[test]
fn bearer_header_rejects_control_bytes() {
    assert!(bearer_header("tok\nrefresh").is_err());
}
