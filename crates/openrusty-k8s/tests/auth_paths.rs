//! In-cluster and exec credential paths, exercised without a cluster.

use std::path::Path;

use openrusty_k8s::auth::exec::{self, ExecConfig};
use openrusty_k8s::auth::in_cluster;
use openrusty_k8s::auth::Credentials;

#[test]
fn in_cluster_reads_injected_root() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("token"), "dummy-in-cluster-token\n").unwrap();
    std::fs::write(
        dir.path().join("ca.crt"),
        b"-----BEGIN CERTIFICATE-----\ndummy\n",
    )
    .unwrap();

    let (cluster, credentials) =
        in_cluster::load_with_root(dir.path(), "10.96.0.1", "443").unwrap();
    assert_eq!(cluster.endpoint.to_string(), "https://10.96.0.1:443/");
    assert_eq!(cluster.ca, b"-----BEGIN CERTIFICATE-----\ndummy\n");
    assert_eq!(
        credentials,
        Credentials::Bearer("dummy-in-cluster-token".to_string())
    );
}

#[test]
fn in_cluster_reports_missing_mount_pieces() {
    let dir = tempfile::tempdir().unwrap();
    let err = in_cluster::load_with_root(dir.path(), "10.96.0.1", "443").unwrap_err();
    assert!(err.to_string().contains("token"), "{err}");

    std::fs::write(dir.path().join("token"), "dummy").unwrap();
    let err = in_cluster::load_with_root(dir.path(), "10.96.0.1", "443").unwrap_err();
    assert!(err.to_string().contains("ca.crt"), "{err}");

    std::fs::write(dir.path().join("ca.crt"), b"ca").unwrap();
    let err = in_cluster::load_with_root(dir.path(), "", "443").unwrap_err();
    assert!(err.to_string().contains("KUBERNETES_SERVICE_HOST"), "{err}");
}

#[test]
fn in_cluster_endpoint_brackets_ipv6() {
    assert_eq!(
        in_cluster::endpoint("fd00::10", "6443")
            .unwrap()
            .to_string(),
        "https://[fd00::10]:6443/"
    );
}

#[test]
fn default_service_account_path_is_standard() {
    assert_eq!(
        in_cluster::SERVICE_ACCOUNT_DIR,
        "/var/run/secrets/kubernetes.io/serviceaccount"
    );
}

#[test]
fn exec_plugin_token_is_extracted() {
    let exec_config = ExecConfig {
        command: "echo".to_string(),
        args: vec![r#"{"apiVersion":"client.authentication.k8s.io/v1","status":{"token":"dummy-exec-token"}}"#.to_string()],
        env: vec![],
        api_version: Some("client.authentication.k8s.io/v1".to_string()),
    };
    assert_eq!(exec::run(&exec_config).unwrap(), "dummy-exec-token");
}

#[test]
fn exec_plugin_env_entries_are_passed_through() {
    // `sh -c` reads the injected env var; if it is missing the JSON would
    // be empty and the token extraction would fail.
    let exec_config = ExecConfig {
        command: "sh".to_string(),
        args: vec![
            "-c".to_string(),
            r#"printf '{"apiVersion":"v1","status":{"token":"%s"}}' "$EXEC_FIXTURE_TOKEN""#
                .to_string(),
        ],
        env: vec![openrusty_k8s::auth::exec::ExecEnvVar {
            name: "EXEC_FIXTURE_TOKEN".to_string(),
            value: "dummy-env-token".to_string(),
        }],
        api_version: None,
    };
    assert_eq!(exec::run(&exec_config).unwrap(), "dummy-env-token");
}

#[test]
fn exec_plugin_failure_maps_to_exec_error() {
    let exec_config = ExecConfig {
        command: "sh".to_string(),
        args: vec![
            "-c".to_string(),
            "echo plugin exploded >&2; exit 7".to_string(),
        ],
        env: vec![],
        api_version: None,
    };
    let err = exec::run(&exec_config).unwrap_err();
    let openrusty_k8s::K8sError::Exec { command, message } = err else {
        panic!("expected Exec error, got {err:?}");
    };
    assert_eq!(command, "sh");
    assert!(message.contains("7"), "{message}");
    assert!(message.contains("plugin exploded"), "{message}");
}

#[test]
fn exec_plugin_non_json_stdout_maps_to_exec_error() {
    let exec_config = ExecConfig {
        command: "echo".to_string(),
        args: vec!["definitely not json".to_string()],
        env: vec![],
        api_version: None,
    };
    let err = exec::run(&exec_config).unwrap_err();
    assert!(err.to_string().contains("ExecCredential"), "{err}");
}

#[test]
fn load_without_any_source_errors_not_panics() {
    // Explicit path that does not exist must be a named error (and must
    // not silently fall through to the home directory or in-cluster).
    let err = openrusty_k8s::load(Some(Path::new("/nonexistent/kubeconfig"))).unwrap_err();
    assert!(err.to_string().contains("/nonexistent/kubeconfig"), "{err}");
}
