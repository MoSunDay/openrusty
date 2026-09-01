//! `networking.k8s.io/v1` Ingress, trimmed to what route rendering needs.

use serde::Deserialize;

use super::{ObjectMeta, ResourceMeta};

/// One Ingress object. `apiVersion`, `kind` and `status` are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Ingress {
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: IngressSpec,
}

impl ResourceMeta for Ingress {
    fn namespace(&self) -> &str {
        &self.metadata.namespace
    }
    fn name(&self) -> &str {
        &self.metadata.name
    }
    fn resource_version(&self) -> &str {
        &self.metadata.resource_version
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct IngressSpec {
    /// Class selector; the renderer collects only Ingresses whose class
    /// equals the configured one. `None` (field absent) never matches.
    #[serde(rename = "ingressClassName", default)]
    pub ingress_class_name: Option<String>,
    /// Catch-all backend for requests matching no rule.
    #[serde(rename = "defaultBackend", default)]
    pub default_backend: Option<IngressBackend>,
    /// TLS sections binding hosts to secrets.
    #[serde(default)]
    pub tls: Vec<IngressTls>,
    #[serde(default)]
    pub rules: Vec<IngressRule>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct IngressTls {
    /// Host names this secret serves; empty means the secret applies to
    /// the default host (which the renderer has no key for and skips).
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(rename = "secretName", default)]
    pub secret_name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct IngressRule {
    /// `None` = catch-all rule for every host.
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub http: Option<HttpIngressRuleValue>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct HttpIngressRuleValue {
    #[serde(default)]
    pub paths: Vec<HttpIngressPath>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct HttpIngressPath {
    /// Request path; always starts with `/` in valid objects.
    #[serde(default)]
    pub path: String,
    /// k8s requires `pathType` on v1 objects; missing degrades to
    /// `ImplementationSpecific` (prefix semantics, as nginx `location /`).
    #[serde(rename = "pathType", default)]
    pub path_type: PathType,
    pub backend: IngressBackend,
}

/// k8s `pathType`. Wire values are PascalCase.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum PathType {
    #[default]
    ImplementationSpecific,
    Prefix,
    Exact,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct IngressBackend {
    /// Service backend; `resource` backends are not modeled and render as
    /// an error.
    #[serde(default)]
    pub service: Option<IngressServiceBackend>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct IngressServiceBackend {
    pub name: String,
    #[serde(default)]
    pub port: Option<ServiceBackendPort>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ServiceBackendPort {
    /// Named service port; the renderer needs the numeric port to dial the
    /// ClusterIP directly, so named ports are a render error in v1.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub number: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_types_are_pascal_case_on_the_wire() {
        for (wire, expected) in [
            ("Exact", PathType::Exact),
            ("Prefix", PathType::Prefix),
            ("ImplementationSpecific", PathType::ImplementationSpecific),
        ] {
            let path: HttpIngressPath = serde_json::from_str(&format!(
                r#"{{"path":"/p","pathType":"{wire}","backend":{{}}}}"#
            ))
            .unwrap();
            assert_eq!(path.path_type, expected);
        }
    }

    #[test]
    fn missing_path_type_defaults_to_implementation_specific() {
        let path: HttpIngressPath = serde_json::from_str(
            r#"{"path":"/p","backend":{"service":{"name":"svc","port":{"number":80}}}}"#,
        )
        .unwrap();
        assert_eq!(path.path_type, PathType::ImplementationSpecific);
        assert_eq!(path.backend.service.as_ref().unwrap().name, "svc");
    }
}
