//! Kubernetes API object models, trimmed to the serde subset the gateway
//! actually consumes.
//!
//! The API server returns objects with dozens of fields we never read
//! (`uid`, `creationTimestamp`, `status`, ...). Every struct here therefore
//! keeps unknown fields (serde default behaviour, no `deny_unknown_fields`)
//! and makes optional everything the API may omit, so real-world payloads
//! and hand-written fixtures both deserialize. What the watch state machine
//! needs beyond the objects themselves lives here too: the generic
//! [`K8sList`] envelope, the [`WatchEvent`] stream item and the
//! [`parse_line`] helper that turns one newline-delimited JSON line into
//! that event.

pub mod ingress;
pub mod secret;

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::error::{K8sError, Result};

pub use ingress::{
    HttpIngressPath, HttpIngressRuleValue, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec, IngressTls, PathType, ServiceBackendPort,
};
pub use secret::{Secret, TLS_TYPE};

/// `metadata` shared by every object we watch. Only the fields the watch
/// state machine and the renderer read are modeled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ObjectMeta {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(rename = "resourceVersion", default)]
    pub resource_version: String,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}

/// `metadata` of a list envelope; carries the list resource version the
/// watch loop resumes from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ListMeta {
    #[serde(rename = "resourceVersion", default)]
    pub resource_version: String,
}

/// Generic `List` envelope (`IngressList`, `SecretList`, ...): items plus
/// the list resource version. No `Default`: `T` needs none.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(bound(deserialize = "T: serde::de::Deserialize<'de>"))]
pub struct K8sList<T> {
    #[serde(default)]
    pub metadata: ListMeta,
    #[serde(default)]
    pub items: Vec<T>,
}

/// The `(namespace, name, resourceVersion)` triple the watch state machine
/// keys and orders objects by; implemented by every model we watch.
pub trait ResourceMeta {
    fn namespace(&self) -> &str;
    fn name(&self) -> &str;
    /// Empty string when the object carries no resource version.
    fn resource_version(&self) -> &str;
}

/// `type` of a watch stream item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum EventType {
    Added,
    Modified,
    Deleted,
    Bookmark,
    Error,
}

/// One newline-delimited JSON item of a watch response body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WatchEvent<T> {
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub object: T,
}

/// Deserialize one watch-stream line into a typed event (pure).
///
/// The stream is newline-delimited JSON; a line that is not valid JSON or
/// does not match `WatchEvent<T>` is an error, which the watch loop treats
/// as stream corruption and answers with a full re-list.
pub fn parse_line<T: DeserializeOwned>(line: &str) -> Result<WatchEvent<T>> {
    serde_json::from_str(line.trim()).map_err(K8sError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq, Deserialize)]
    struct Probe {
        #[serde(default)]
        marker: String,
    }

    impl ResourceMeta for Probe {
        fn namespace(&self) -> &str {
            "default"
        }
        fn name(&self) -> &str {
            "probe"
        }
        fn resource_version(&self) -> &str {
            ""
        }
    }

    #[test]
    fn event_types_are_uppercase_on_the_wire() {
        for (wire, expected) in [
            ("ADDED", EventType::Added),
            ("MODIFIED", EventType::Modified),
            ("DELETED", EventType::Deleted),
            ("BOOKMARK", EventType::Bookmark),
            ("ERROR", EventType::Error),
        ] {
            let line = format!(r#"{{"type":"{wire}","object":{{}}}}"#);
            let event: WatchEvent<Probe> = parse_line(&line).unwrap();
            assert_eq!(event.event_type, expected);
        }
    }

    #[test]
    fn parse_line_rejects_garbage() {
        let err = parse_line::<Probe>("not json at all").unwrap_err();
        assert!(err.to_string().contains("json decode failed"), "{err}");
    }

    #[test]
    fn list_envelope_tolerates_unknown_fields() {
        let list: K8sList<Probe> = serde_json::from_str(
            r#"{"apiVersion":"v1","kind":"ProbeList","metadata":{"resourceVersion":"7","selfLink":"/x"},
                "items":[{"marker":"a"}],"other":1}"#,
        )
        .unwrap();
        assert_eq!(list.metadata.resource_version, "7");
        assert_eq!(list.items.len(), 1);
    }
}
