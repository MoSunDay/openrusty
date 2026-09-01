//! `core/v1` Secret, trimmed to TLS material.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{ObjectMeta, ResourceMeta};

/// `type` value of TLS secrets: PEM `tls.crt` + `tls.key`.
pub const TLS_TYPE: &str = "kubernetes.io/tls";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Secret {
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(rename = "type", default)]
    pub type_: Option<String>,
    /// Base64-encoded values keyed by data key.
    #[serde(default)]
    pub data: BTreeMap<String, String>,
}

impl ResourceMeta for Secret {
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

impl Secret {
    /// TLS material of a `kubernetes.io/tls` secret, still base64
    /// (decoding is the renderer's job with field-level error naming).
    pub fn tls_material(&self) -> Option<(&str, &str)> {
        if self.type_.as_deref() != Some(TLS_TYPE) {
            return None;
        }
        Some((
            self.data.get("tls.crt")?.as_str(),
            self.data.get("tls.key")?.as_str(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_material_requires_type_and_both_keys() {
        let empty: Secret = serde_json::from_str(r#"{"data":{}}"#).unwrap();
        assert!(empty.tls_material().is_none());

        let tls: Secret = serde_json::from_str(
            r#"{"type":"kubernetes.io/tls","data":{"tls.crt":"Yw==","tls.key":"aw=="}}"#,
        )
        .unwrap();
        assert_eq!(tls.tls_material(), Some(("Yw==", "aw==")));
    }
}
