//! Fixture-driven kubeconfig loading tests (no cluster, no env access).

use std::path::PathBuf;

use openrusty_k8s::auth::kubeconfig::{parse_kubeconfig, resolve};
use openrusty_k8s::auth::load_kubeconfig_file;
use openrusty_k8s::auth::Credentials;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn token_fixture_resolves_cluster_and_bearer() {
    let (cluster, credentials) = load_kubeconfig_file(&fixture("kubeconfig-token.yaml")).unwrap();
    assert_eq!(
        cluster.endpoint.to_string(),
        "https://apiserver.example.invalid:6443/"
    );
    let expected_ca =
        "-----BEGIN CERTIFICATE-----\nZm9vQ0FEYXRhRm9yVGVzdHMK\n-----END CERTIFICATE-----\n";
    assert_eq!(cluster.ca, expected_ca.as_bytes());
    assert_eq!(
        credentials,
        Credentials::Bearer("dummy-token-for-tests".to_string())
    );
}

#[test]
fn client_cert_fixture_decodes_data_fields() {
    let (_, credentials) = load_kubeconfig_file(&fixture("kubeconfig-client-cert.yaml")).unwrap();
    let Credentials::ClientCert { cert, key } = credentials else {
        panic!("expected ClientCert, got {credentials:?}");
    };
    let cert_text = String::from_utf8(cert).unwrap();
    let key_text = String::from_utf8(key).unwrap();
    assert!(
        cert_text.starts_with("-----BEGIN CERTIFICATE-----"),
        "{cert_text}"
    );
    assert!(
        cert_text.contains("Zm9vQ2VydGlmaWNhdGVEYXRhRm9yVGVzdHMK"),
        "{cert_text}"
    );
    assert!(
        key_text.starts_with("-----BEGIN RSA PRIVATE KEY-----"),
        "{key_text}"
    );
    assert!(
        key_text.contains("Zm9vQ2xpZW50S2V5RGF0YUZvclRlc3RzCg=="),
        "{key_text}"
    );
}

#[test]
fn exec_fixture_requires_and_honors_context_override() {
    let path = fixture("kubeconfig-exec.yaml");
    let text = std::fs::read_to_string(&path).unwrap();
    let config = parse_kubeconfig(&text).unwrap();

    // No current-context in the document: resolution must fail...
    let err = resolve(&config, None).unwrap_err();
    assert!(err.to_string().contains("current-context"), "{err}");

    // ...unless the caller names a context explicitly.
    let (cluster, _) = resolve(&config, Some("ctx-b")).unwrap();
    assert_eq!(
        cluster.cluster.server,
        "https://cluster-b.example.invalid:6443"
    );
    assert!(cluster.cluster.certificate_authority_data.is_some());

    let (_, credentials) = load_kubeconfig_file_with_override(&path, "ctx-a");
    let Credentials::ExecToken(exec_config) = credentials else {
        panic!("expected ExecToken, got {credentials:?}");
    };
    assert_eq!(exec_config.command, "echo");
    assert_eq!(
        exec_config.api_version.as_deref(),
        Some("client.authentication.k8s.io/v1")
    );
    assert_eq!(exec_config.env.len(), 1);
}

/// Resolve with an explicit context: parse + `resolve(Some(ctx))`.
fn load_kubeconfig_file_with_override(
    path: &std::path::Path,
    context: &str,
) -> (openrusty_k8s::Cluster, Credentials) {
    let text = std::fs::read_to_string(path).unwrap();
    let config = parse_kubeconfig(&text).unwrap();
    let (named_cluster, named_user) = resolve(&config, Some(context)).unwrap();
    let endpoint = named_cluster.cluster.server.parse().unwrap();
    let credentials = match (&named_user.user.token, &named_user.user.exec) {
        (Some(token), _) => Credentials::Bearer(token.clone()),
        (None, Some(exec_config)) => Credentials::ExecToken(exec_config.clone()),
        _ => panic!("fixture user carries no credential this helper understands"),
    };
    (
        openrusty_k8s::Cluster {
            endpoint,
            ca: Vec::new(),
        },
        credentials,
    )
}

#[test]
fn ca_file_fixture_resolves_relative_to_kubeconfig() {
    let (cluster, _) = load_kubeconfig_file(&fixture("kubeconfig-ca-file.yaml")).unwrap();
    let ca_on_disk = std::fs::read(fixture("test-ca.crt")).unwrap();
    assert_eq!(cluster.ca, ca_on_disk);
}

#[test]
fn broken_fixtures_error_with_named_cause() {
    let err =
        load_kubeconfig_file(&fixture("kubeconfig-missing-current-context.yaml")).unwrap_err();
    assert!(err.to_string().contains("current-context"), "{err}");

    let err = load_kubeconfig_file(&fixture("kubeconfig-missing-cluster.yaml")).unwrap_err();
    assert!(
        err.to_string().contains("no-such-cluster"),
        "error should name the dangling cluster: {err}"
    );

    let err = load_kubeconfig_file(&fixture("kubeconfig-missing-user.yaml")).unwrap_err();
    assert!(
        err.to_string().contains("no-such-user"),
        "error should name the dangling user: {err}"
    );
}
