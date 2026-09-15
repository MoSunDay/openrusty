//! YAML-tree surgery and TOML rendering behind `openrusty inject`.
//!
//! Split from [`super`] to keep both files small. Everything here is pure:
//! `serde_yaml::Value` in, `Value`/`String` out. The workload tree is
//! mutated in place by inserting the three pieces (init container, sidecar,
//! config volume) - every other field is passed through untouched.

use super::{
    ignore_inbound_ports, InjectParams, ADMIN_PORT, CONFIG_MOUNT, CONFIG_PATH, CONFIG_VOLUME,
    INBOUND_PORT, INIT_NAME, OUTBOUND_PORT, OPAQUE_PORTS_ENV, SIDECAR_NAME,
};
use serde::Serialize;
use serde_yaml::Value;

/// Capabilities the init container needs to program iptables: NET_ADMIN
/// (rule writes) + NET_RAW (socket matchers), nothing more.
const INIT_CAPS: &[&str] = &["NET_ADMIN", "NET_RAW"];

/// Pod-spec location per kind (v1 surface).
const POD: &[&str] = &["spec"];
const POD_TEMPLATE: &[&str] = &["spec", "template", "spec"];
const CRONJOB_POD: &[&str] = &["spec", "jobTemplate", "spec", "template", "spec"];

// ---------------------------------------------------------------------------
// Tree accessors
// ---------------------------------------------------------------------------

/// `metadata` of the workload root (None when absent).
pub fn metadata_of(doc: &Value) -> Option<&Value> {
    doc.get("metadata")
}

/// `spec.template.metadata` of a pod-template workload (None for pods).
pub fn pod_template_metadata(doc: &Value) -> Option<&Value> {
    doc.get("spec")?.get("template")?.get("metadata")
}

/// Annotations of one metadata object, as `(key, value)` pairs.
pub fn annotations(meta: &Value) -> Vec<(String, &Value)> {
    let Some(map) = meta.get("annotations").and_then(Value::as_mapping) else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(k, v)| k.as_str().map(|k| (k.to_string(), v)))
        .collect()
}

/// YAML scalars in their annotation-string form (k8s requires strings, but
/// an unquoted `proxy-uid: 511` should still work).
pub fn scalar_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// `metadata.name`, required for the ConfigMap name.
pub fn workload_name(doc: &Value) -> Result<String, String> {
    metadata_of(doc)
        .and_then(|m| m.get("name"))
        .and_then(scalar_string)
        .ok_or_else(|| "workload has no metadata.name (cannot name the config ConfigMap)".into())
}

/// `metadata.namespace`, optional.
pub fn namespace_of(doc: &Value) -> Option<String> {
    metadata_of(doc)
        .and_then(|m| m.get("namespace"))
        .and_then(scalar_string)
}

/// Resolves the pod spec (`&mut`) for the document kind, rejecting kinds
/// openrusty does not inject into.
pub fn pod_spec(doc: &mut Value) -> Result<&mut Value, String> {
    let kind = doc
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "document has no kind".to_string())?
        .to_string();
    let path: &[&str] = match kind.as_str() {
        "Pod" => POD,
        "Deployment" | "ReplicaSet" | "StatefulSet" | "DaemonSet" | "Job" => POD_TEMPLATE,
        "CronJob" => CRONJOB_POD,
        other => {
            return Err(format!(
                "kind {other:?} is not injectable in v1: only Pod and \
                 pod-template workloads (Deployment/ReplicaSet/StatefulSet/\
                 DaemonSet/Job/CronJob) are supported"
            ))
        }
    };
    let spec = get_mut(doc, path).ok_or_else(|| {
        format!(
            "no pod spec at .{}: is this a valid {kind} manifest?",
            path.join(".")
        )
    })?;
    if spec.get("containers").and_then(Value::as_sequence).is_none() {
        return Err(format!("pod spec at .{} has no containers", path.join(".")));
    }
    Ok(spec)
}

/// Walks `path` through mappings; any missing hop aborts with None.
fn get_mut<'a>(value: &'a mut Value, path: &[&str]) -> Option<&'a mut Value> {
    let mut current = value;
    for key in path {
        current = current.get_mut(*key)?;
    }
    Some(current)
}

// ---------------------------------------------------------------------------
// Injection
// ---------------------------------------------------------------------------

/// Inserts the init container, the sidecar and the config volume into the
/// pod spec; both containers carry `image`. Refuses to double-inject.
pub fn inject_pod_spec(
    spec: &mut Value,
    p: &InjectParams,
    image: &str,
    cm_name: &str,
) -> Result<(), String> {
    if container_names(spec, "initContainers").contains(&INIT_NAME.to_string())
        || container_names(spec, "containers").contains(&SIDECAR_NAME.to_string())
    {
        return Err(format!(
            "pod already carries openrusty pieces ({}/{}): refusing to inject twice",
            INIT_NAME, SIDECAR_NAME
        ));
    }
    let init = to_value(&InitContainer {
        name: INIT_NAME,
        image: image.to_string(),
        command: init_command(p),
        security_context: InitSecurity {
            run_as_user: 0,
            capabilities: InitCapabilities {
                add: INIT_CAPS.to_vec(),
            },
        },
    })?;
    let sidecar = to_value(&Sidecar {
        name: SIDECAR_NAME,
        image: image.to_string(),
        args: vec![CONFIG_PATH.to_string()],
        env: opaque_env(p),
        ports: vec![
            port("proxy", INBOUND_PORT),
            port("outbound", OUTBOUND_PORT),
            port("admin", ADMIN_PORT),
        ],
        security_context: SidecarSecurity {
            run_as_user: p.proxy_uid,
        },
        volume_mounts: vec![VolumeMount {
            name: CONFIG_VOLUME,
            mount_path: CONFIG_MOUNT,
            read_only: true,
        }],
    })?;
    let volume = to_value(&ConfigVolume {
        name: CONFIG_VOLUME,
        config_map: ConfigMapRef {
            name: cm_name.to_string(),
        },
    })?;
    append(spec, "initContainers", init)?;
    append(spec, "containers", sidecar)?;
    append(spec, "volumes", volume)
}

/// The init container command: the `iptables-init` subcommand with the
/// annotation surface encoded as flags (admin port always exempt).
pub fn init_command(p: &InjectParams) -> Vec<String> {
    vec![
        "openrusty".into(),
        "iptables-init".into(),
        "--proxy-uid".into(),
        p.proxy_uid.to_string(),
        "--inbound-port".into(),
        INBOUND_PORT.to_string(),
        "--outbound-port".into(),
        OUTBOUND_PORT.to_string(),
        "--ignore-inbound-ports".into(),
        ignore_inbound_ports(p),
    ]
}

fn opaque_env(p: &InjectParams) -> Option<Vec<EnvVar>> {
    if p.opaque_ports.is_empty() {
        return None;
    }
    let ports = p
        .opaque_ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",");
    Some(vec![EnvVar {
        name: OPAQUE_PORTS_ENV.to_string(),
        value: ports,
    }])
}

fn port(name: &str, n: u16) -> ContainerPort {
    ContainerPort {
        name: name.to_string(),
        container_port: n,
    }
}

fn container_names(spec: &Value, key: &str) -> Vec<String> {
    spec.get(key)
        .and_then(Value::as_sequence)
        .map(|list| {
            list.iter()
                .filter_map(|c| c.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Appends `item` to the list at `key`, creating an empty list if absent.
fn append(spec: &mut Value, key: &str, item: Value) -> Result<(), String> {
    let Some(map) = spec.as_mapping_mut() else {
        return Err("pod spec is not a mapping".into());
    };
    let entry = map
        .entry(serde_yaml::to_value(key).expect("static key"))
        .or_insert_with(|| Value::Sequence(Vec::new()));
    let Some(list) = entry.as_sequence_mut() else {
        return Err(format!("pod spec .{key} is not a list"));
    };
    list.push(item);
    Ok(())
}

pub(super) fn to_value<T: Serialize>(value: &T) -> Result<Value, String> {
    serde_yaml::to_value(value).map_err(|e| format!("cannot render injection piece: {e}"))
}

// --- pod spec pieces (serialize to the k8s camelCase shapes) ---

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitContainer {
    name: &'static str,
    image: String,
    command: Vec<String>,
    security_context: InitSecurity,
}

/// UID 0 + NET_ADMIN/NET_RAW: the narrow privilege envelope for nat-rule
/// programming (no `privileged: true`).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitSecurity {
    run_as_user: u32,
    capabilities: InitCapabilities,
}

#[derive(Serialize)]
struct InitCapabilities {
    add: Vec<&'static str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Sidecar {
    name: &'static str,
    image: String,
    args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<Vec<EnvVar>>,
    ports: Vec<ContainerPort>,
    security_context: SidecarSecurity,
    volume_mounts: Vec<VolumeMount>,
}

#[derive(Serialize)]
struct EnvVar {
    name: String,
    value: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ContainerPort {
    name: String,
    container_port: u16,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SidecarSecurity {
    run_as_user: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VolumeMount {
    name: &'static str,
    mount_path: &'static str,
    read_only: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigVolume {
    name: &'static str,
    config_map: ConfigMapRef,
}

#[derive(Serialize)]
struct ConfigMapRef {
    name: String,
}
