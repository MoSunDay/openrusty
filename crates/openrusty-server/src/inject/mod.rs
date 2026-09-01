//! `openrusty inject`: static sidecar injection for Kubernetes workloads.
//!
//! linkerd-aligned CLI shape: a workload manifest goes in on stdin, the
//! manifest carrying the openrusty sidecar pieces comes out on stdout.
//! Like [`crate::init`] this is a one-shot parameter-plane tool: no cluster
//! access, no kubeconfig, no gateway config file, no serving runtime.
//!
//! # Input contract (v1)
//!
//! - exactly one YAML document; multi-document input is rejected (exit 1).
//! - `kind` must be `Pod`, or a workload with a pod template:
//!   `Deployment`/`ReplicaSet`/`StatefulSet`/`DaemonSet`/`Job` inject at
//!   `spec.template.spec`, `CronJob` at `spec.jobTemplate.spec.template.spec`,
//!   `Pod` at `spec`. Any other kind is rejected (exit 1).
//!
//! # Annotation surface (`config.openrusty.io/` prefix)
//!
//! Read from the pod template annotations of workloads (workload-level
//! `metadata.annotations` are merged underneath, template entries win), and
//! from `metadata.annotations` for bare Pods. Parsed into [`InjectParams`].
//!
//! | annotation           | meaning                              | default   |
//! |----------------------|--------------------------------------|-----------|
//! | `inject`             | `enabled` / `disabled`               | `enabled` |
//! | `skip-inbound-ports` | inbound ports left unredirected      | none      |
//! | `opaque-ports`       | recorded as `OPENRUSTY_OPAQUE_PORTS` | none      |
//! | `proxy-uid`          | sidecar UID (iptables owner match)   | `511`     |
//! | `proxy-log-level`    | `trace..error`                       | `warn`    |
//! | `egress-mode`        | `direct` / `gateway` / `deny`        | `direct`  |
//! | `egress-gateway`     | gateway `host:port` (gateway mode)   | none      |
//!
//! `disabled` passes the input through byte-for-byte (no output docs added).
//! Unknown `config.openrusty.io/*` annotations warn on stderr and never
//! fail (forward compatibility). Bad values of *known* annotations fail:
//! a typo would silently inject the wrong rules otherwise.
//!
//! # What gets injected (three pieces; see [`render`])
//!
//! 1. initContainer `openrusty-init` - iptables redirection, privileged.
//! 2. sidecar `openrusty-proxy` - runs as the proxy UID, ports
//!    4143/4140/4191, config from a ConfigMap volume mount.
//! 3. `ConfigMap <name>-openrusty-config` - the sidecar's minimal TOML.
//!
//! `opaque-ports` is env passthrough only in v1: it does not change the
//! iptables surface (a later version adds per-port handling).
//!
//! Output is two YAML documents (workload, then ConfigMap); key order may
//! differ from the input (the parsed tree is re-rendered), which kubectl
//! and Helm apply identically.

mod config;
mod render;
#[cfg(test)]
mod tests;

use openrusty_core::config::EgressMode;
use config::render_config_map;
use render::{metadata_of, pod_spec, pod_template_metadata};
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::io::{Read, Write};

/// Subcommand name under the `openrusty` binary.
pub const SUBCOMMAND: &str = "inject";
/// Annotation prefix (linkerd uses `config.linkerd.io/`).
pub const PREFIX: &str = "config.openrusty.io/";
pub const INIT_NAME: &str = "openrusty-init";
pub const SIDECAR_NAME: &str = "openrusty-proxy";
/// ConfigMap volume name and mount point of the sidecar config.
pub const CONFIG_VOLUME: &str = "openrusty-config";
pub const CONFIG_MOUNT: &str = "/etc/openrusty";
pub const CONFIG_PATH: &str = "/etc/openrusty/openrusty.toml";
/// Three-port convention: inbound / outbound / admin.
pub const INBOUND_PORT: u16 = 4143;
pub const OUTBOUND_PORT: u16 = 4140;
pub const ADMIN_PORT: u16 = 4191;
/// Port the init rules must never hijack: the admin listener itself.
pub const ADMIN_IGNORE_PORT: u16 = ADMIN_PORT;
pub const DEFAULT_PROXY_UID: u32 = 511;
pub const DEFAULT_LOG_LEVEL: &str = "warn";
pub const IMAGE: &str = "ghcr.io/openrusty/openrusty:0.0.0-placeholder";
/// Sidecar env carrying the `opaque-ports` annotation (v1: passthrough
/// only, no rule-surface change - see the module docs).
pub const OPAQUE_PORTS_ENV: &str = "OPENRUSTY_OPAQUE_PORTS";
/// `[plugins] dir` of every injected config, deliberately pointing at a
/// path that does not exist in the container: `PluginRegistry::bootstrap`
/// treats a missing/empty plugin dir as "no plugins" (warn + generation 0)
/// and never fails, so the sidecar boots without any plugins volume.
pub const PLUGINS_DIR: &str = "/dev/null-plugins";
pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
pub const MANAGED_BY: &str = "openrusty-inject";

/// Parsed annotation surface (pure data; rendering is a pure function).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InjectParams {
    pub enabled: bool,
    pub skip_inbound_ports: Vec<u16>,
    pub opaque_ports: Vec<u16>,
    pub proxy_uid: u32,
    pub log_level: String,
    pub egress_mode: EgressMode,
    pub egress_gateway: String,
}

impl Default for InjectParams {
    fn default() -> Self {
        InjectParams {
            enabled: true,
            skip_inbound_ports: Vec::new(),
            opaque_ports: Vec::new(),
            proxy_uid: DEFAULT_PROXY_UID,
            log_level: DEFAULT_LOG_LEVEL.to_string(),
            egress_mode: EgressMode::Direct,
            egress_gateway: String::new(),
        }
    }
}

/// Process entry: reads the workload from `input`, writes the injected
/// YAML (or the untouched passthrough) to `output`. Returns the exit code.
pub fn run_cli(mut input: impl Read, mut output: impl Write) -> i32 {
    let mut raw = String::new();
    if input.read_to_string(&mut raw).is_err() {
        eprintln!("openrusty {SUBCOMMAND}: stdin is not valid UTF-8");
        return 1;
    }
    match run(&raw) {
        Ok(rendered) => {
            if write!(output, "{rendered}").is_err() {
                eprintln!("openrusty {SUBCOMMAND}: cannot write to stdout");
                return 1;
            }
            0
        }
        Err(e) => {
            eprintln!("openrusty {SUBCOMMAND}: {e}");
            1
        }
    }
}

/// Pure core of the command: manifest text in, injected manifest out.
pub fn run(raw: &str) -> Result<String, String> {
    if raw.trim().is_empty() {
        return Err("empty input: expected exactly one workload YAML document on stdin".into());
    }
    let mut doc: Value = serde_yaml::from_str(raw).map_err(|e| {
        if e.to_string().contains("more than one document") {
            "multi-document input is not supported in v1: pipe exactly one \
             workload document (split with `yq` or `sed` first)"
                .to_string()
        } else {
            format!("invalid YAML: {e}")
        }
    })?;
    let (params, warnings) = collect_annotations(&doc)?;
    for w in &warnings {
        eprintln!("openrusty {SUBCOMMAND}: warning: {w}");
    }
    if !params.enabled {
        // Explicit opt-out: byte-for-byte passthrough, nothing injected.
        return Ok(raw.to_string());
    }
    let name = render::workload_name(&doc)?;
    let cm_name = format!("{name}-openrusty-config");
    let pod_spec = pod_spec(&mut doc)?;
    render::inject_pod_spec(pod_spec, &params, &cm_name)?;
    let config_map = render_config_map(&name, render::namespace_of(&doc), &params)?;
    let workload_yaml = serde_yaml::to_string(&doc)
        .map_err(|e| format!("cannot re-render the workload: {e}"))?;
    let cm_yaml = serde_yaml::to_string(&config_map)
        .map_err(|e| format!("cannot render the config ConfigMap: {e}"))?;
    Ok(format!("{}\n---\n{}\n", workload_yaml.trim_end(), cm_yaml.trim_end()))
}

// ---------------------------------------------------------------------------
// Annotation parsing
// ---------------------------------------------------------------------------

/// Merges every annotation source into one `name -> value` map (pod
/// template annotations win over workload-level ones) and parses it.
pub fn collect_annotations(doc: &Value) -> Result<(InjectParams, Vec<String>), String> {
    let mut merged = BTreeMap::new();
    let mut warnings = Vec::new();
    // Workload-level metadata first, then the pod template's (template
    // entries win on conflict); a bare Pod has only the first source.
    let mut sources: Vec<&Value> = Vec::new();
    if let Some(meta) = metadata_of(doc) {
        sources.push(meta);
    }
    if let Some(meta) = pod_template_metadata(doc) {
        sources.push(meta);
    }
    for meta in sources {
        for (k, v) in render::annotations(meta) {
            match render::scalar_string(v) {
                Some(s) => {
                    merged.insert(k, s);
                }
                None => warnings.push(format!("annotation {k} is not a scalar; ignored")),
            }
        }
    }
    parse_annotations(&merged).map(|p| (p, warnings))
}

/// Parses the merged annotation map into [`InjectParams`] (pure).
pub fn parse_annotations(annotations: &BTreeMap<String, String>) -> Result<InjectParams, String> {
    let mut p = InjectParams::default();
    for (key, value) in annotations {
        let Some(name) = key.strip_prefix(PREFIX) else {
            continue; // not ours
        };
        match name {
            "inject" => {
                p.enabled = match value.as_str() {
                    "enabled" => true,
                    "disabled" => false,
                    other => return Err(invalid(name, other, "enabled|disabled")),
                }
            }
            "skip-inbound-ports" => p.skip_inbound_ports = parse_ports(name, value)?,
            "opaque-ports" => p.opaque_ports = parse_ports(name, value)?,
            "proxy-uid" => {
                p.proxy_uid = value
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| invalid(name, value, "a numeric uid"))?
            }
            "proxy-log-level" => {
                if !matches!(value.as_str(), "trace" | "debug" | "info" | "warn" | "error") {
                    return Err(invalid(name, value, "trace|debug|info|warn|error"));
                }
                p.log_level = value.to_string();
            }
            "egress-mode" => {
                p.egress_mode = match value.as_str() {
                    "direct" => EgressMode::Direct,
                    "gateway" => EgressMode::Gateway,
                    "deny" => EgressMode::Deny,
                    other => return Err(invalid(name, other, "direct|gateway|deny")),
                }
            }
            "egress-gateway" => p.egress_gateway = value.trim().to_string(),
            other => warn_unknown(other),
        }
    }
    if p.egress_mode == EgressMode::Gateway && p.egress_gateway.is_empty() {
        return Err(format!(
            "annotation {PREFIX}egress-gateway is required when {PREFIX}egress-mode is \"gateway\""
        ));
    }
    Ok(p)
}

fn warn_unknown(name: &str) {
    eprintln!(
        "openrusty {SUBCOMMAND}: warning: unknown annotation {PREFIX}{name} \
         (newer openrusty? ignored for forward compatibility)"
    );
}

fn invalid(what: &str, got: &str, want: &str) -> String {
    format!("annotation {PREFIX}{what}: {got:?} is not one of {want}")
}

/// `4191` (admin listener) union `skip-inbound-ports`, sorted, deduped -
/// rendered into the init container's `--ignore-inbound-ports` flag.
pub fn ignore_inbound_ports(p: &InjectParams) -> String {
    let mut ports = p.skip_inbound_ports.clone();
    ports.push(ADMIN_IGNORE_PORT);
    ports.sort_unstable();
    ports.dedup();
    ports.iter().map(u16::to_string).collect::<Vec<_>>().join(",")
}

fn parse_ports(what: &str, raw: &str) -> Result<Vec<u16>, String> {
    let mut out = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(invalid(what, raw, "a comma-separated port list"));
        }
        let port = part
            .parse::<u16>()
            .map_err(|_| invalid(what, raw, "a comma-separated port list"))?;
        if !out.contains(&port) {
            out.push(port);
        }
    }
    Ok(out)
}
