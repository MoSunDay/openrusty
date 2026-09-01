//! exec-based credential plugin support.
//!
//! A kubeconfig user may declare an external command that prints an
//! ExecCredential JSON document on stdout:
//!
//! ```json
//! {"apiVersion":"client.authentication.k8s.io/v1","status":{"token":"..."}}
//! ```
//!
//! [`ExecConfig`] is carried inside [`Credentials::ExecToken`](super::Credentials)
//! and executed lazily: [`run`] spawns the command and extracts the token.
//! Scope notes for this milestone:
//!
//! - the plugin's stdout is parsed as JSON only; `clientCertificateData` in
//!   the exec output is not consumed yet;
//! - `apiVersion` is accepted but not enforced (both `v1` and `v1beta1`
//!   share the same `status.token` shape);
//! - `KUBERNETES_EXEC_INFO` is not injected and interactive/refresh flows
//!   are out of scope; the token is resolved once when the client is built.

use std::collections::HashMap;
use std::process::Command;

use serde::Deserialize;

use crate::error::{K8sError, Result};

/// An exec credential plugin declaration (kubeconfig `user.exec` field).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ExecConfig {
    /// Command to run, resolved through `PATH` like `kubectl` does.
    pub command: String,
    /// Arguments passed verbatim to [`ExecConfig::command`].
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment entries for the plugin process.
    #[serde(default)]
    pub env: Vec<ExecEnvVar>,
    /// Plugin API version, e.g. `client.authentication.k8s.io/v1`.
    #[serde(rename = "apiVersion")]
    pub api_version: Option<String>,
}

/// One `name`/`value` pair of [`ExecConfig::env`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ExecEnvVar {
    /// Environment variable name.
    pub name: String,
    /// Environment variable value.
    pub value: String,
}

/// Minimal shape of the plugin's stdout document (only `status.token` is
/// consumed; everything else is ignored).
#[derive(Debug, Deserialize)]
struct ExecCredential {
    /// `status` object holding the token.
    #[serde(default)]
    status: Option<ExecCredentialStatus>,
}

/// `status` subset of the ExecCredential document.
#[derive(Debug, Deserialize)]
struct ExecCredentialStatus {
    /// Bearer token handed to the apiserver.
    #[serde(default)]
    token: Option<String>,
}

/// Run the plugin and return the bearer token from its stdout.
///
/// Pure with respect to the cluster: the process environment seen by the
/// plugin is the gateway's, plus the configured [`ExecConfig::env`] entries.
pub fn run(exec: &ExecConfig) -> Result<String> {
    let output = Command::new(&exec.command)
        .args(&exec.args)
        .envs(env_map(exec))
        .output()
        .map_err(|e| K8sError::Exec {
            command: exec.command.clone(),
            message: format!("failed to spawn: {e}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(K8sError::Exec {
            command: exec.command.clone(),
            message: format!(
                "exit status {:?}, stderr: {}",
                output.status.code(),
                stderr.trim()
            ),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    token_from_output(&stdout).map_err(|e| match e {
        K8sError::Exec { message, .. } => K8sError::Exec {
            command: exec.command.clone(),
            message,
        },
        other => other,
    })
}

/// Environment for the plugin process: the configured entries only, so the
/// child inherits nothing surprising beyond the implicit OS defaults.
fn env_map(exec: &ExecConfig) -> HashMap<String, String> {
    exec.env
        .iter()
        .map(|var| (var.name.clone(), var.value.clone()))
        .collect()
}

/// Parse an ExecCredential document and extract `status.token` (pure).
///
/// Public so tests can exercise the parser without spawning processes.
pub fn token_from_output(stdout: &str) -> Result<String> {
    let credential: ExecCredential = serde_json::from_str(stdout).map_err(|e| K8sError::Exec {
        command: "<unavailable>".to_string(),
        message: format!("stdout is not a valid ExecCredential document: {e}"),
    })?;

    let token = credential
        .status
        .and_then(|status| status.token)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| K8sError::Exec {
            command: "<unavailable>".to_string(),
            message: "no token in exec output".to_string(),
        })?;

    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_token_from_v1_document() {
        let doc = r#"{"apiVersion":"client.authentication.k8s.io/v1","status":{"token":"tok-1"}}"#;
        assert_eq!(token_from_output(doc).unwrap(), "tok-1");
    }

    #[test]
    fn extracts_token_from_v1beta1_document() {
        let doc =
            r#"{"apiVersion":"client.authentication.k8s.io/v1beta1","status":{"token":"tok-2"}}"#;
        assert_eq!(token_from_output(doc).unwrap(), "tok-2");
    }

    #[test]
    fn rejects_non_json_stdout() {
        let err = token_from_output("not json").unwrap_err();
        assert!(err.to_string().contains("ExecCredential"), "{err}");
    }

    #[test]
    fn rejects_document_without_token() {
        let err = token_from_output(r#"{"apiVersion":"v1","status":{}}"#).unwrap_err();
        assert!(err.to_string().contains("no token"), "{err}");
        let err = token_from_output(r#"{"apiVersion":"v1"}"#).unwrap_err();
        assert!(err.to_string().contains("no token"), "{err}");
    }

    #[test]
    fn rejects_empty_token() {
        let err = token_from_output(r#"{"status":{"token":""}}"#).unwrap_err();
        assert!(err.to_string().contains("no token"), "{err}");
    }

    #[test]
    fn run_propagates_command_failure_as_exec_error() {
        let exec = ExecConfig {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "echo boom >&2; exit 3".to_string()],
            env: vec![],
            api_version: None,
        };
        let err = run(&exec).unwrap_err();
        let K8sError::Exec { command, message } = err else {
            panic!("expected Exec error, got {err:?}");
        };
        assert_eq!(command, "sh");
        assert!(message.contains("3"), "{message}");
        assert!(message.contains("boom"), "{message}");
    }
}
