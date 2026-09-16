//! Locks for `openrusty inject`: annotation parsing, the rendered sidecar
//! TOML (through the real `load_config`), and end-to-end injection of the
//! shared fixture (`tests/fixtures/inject/deployment.yaml`, also consumed
//! by `scripts/chart-lint.sh` for chart consistency).

use super::*;
use openrusty_core::config::{load_config, EgressMode, ListenerRole};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Repo-root fixture, shared with `scripts/chart-lint.sh`.
fn fixture() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/inject/deployment.yaml");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("fixture {} unreadable: {e}", path.display()))
}

fn params_from(pairs: &[(&str, &str)]) -> Result<InjectParams, String> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    parse_annotations(&map)
}

#[test]
fn defaults_when_no_annotations() {
    assert_eq!(params_from(&[]).unwrap(), InjectParams::default());
}

#[test]
fn inject_disabled_is_explicit() {
    assert!(
        !params_from(&[(&format!("{PREFIX}inject"), "disabled")])
            .unwrap()
            .enabled
    );
    assert!(
        params_from(&[(&format!("{PREFIX}inject"), "enabled")])
            .unwrap()
            .enabled
    );
    let err = params_from(&[(&format!("{PREFIX}inject"), "Enabled")]).unwrap_err();
    assert!(err.contains("enabled|disabled"), "got: {err}");
}

#[test]
fn skip_and_opaque_ports_parse() {
    let p = params_from(&[
        (&format!("{PREFIX}skip-inbound-ports"), "9090, 15002,9090"),
        (&format!("{PREFIX}opaque-ports"), "443,8443"),
    ])
    .unwrap();
    assert_eq!(p.skip_inbound_ports, vec![9090, 15002]);
    assert_eq!(p.opaque_ports, vec![443, 8443]);
    let err = params_from(&[(&format!("{PREFIX}skip-inbound-ports"), "9090,x")]).unwrap_err();
    assert!(err.contains("skip-inbound-ports"), "got: {err}");
}

#[test]
fn uid_log_level_and_egress_parse() {
    let p = params_from(&[
        (&format!("{PREFIX}proxy-uid"), "2102"),
        (&format!("{PREFIX}proxy-log-level"), "info"),
        (&format!("{PREFIX}egress-mode"), "gateway"),
        (&format!("{PREFIX}egress-gateway"), "egress.demo.svc:4140"),
    ])
    .unwrap();
    assert_eq!(p.proxy_uid, 2102);
    assert_eq!(p.log_level, "info");
    assert_eq!(p.egress_mode, EgressMode::Gateway);
    assert_eq!(p.egress_gateway, "egress.demo.svc:4140");

    assert_eq!(
        params_from(&[(&format!("{PREFIX}egress-mode"), "deny")])
            .unwrap()
            .egress_mode,
        EgressMode::Deny
    );
    let err = params_from(&[(&format!("{PREFIX}egress-mode"), "gateway")]).unwrap_err();
    assert!(err.contains("egress-gateway is required"), "got: {err}");
    let err = params_from(&[(&format!("{PREFIX}proxy-uid"), "nobody")]).unwrap_err();
    assert!(err.contains("proxy-uid"), "got: {err}");
    let err = params_from(&[(&format!("{PREFIX}proxy-log-level"), "loud")]).unwrap_err();
    assert!(err.contains("proxy-log-level"), "got: {err}");
    let err = params_from(&[(&format!("{PREFIX}egress-mode"), "forward")]).unwrap_err();
    assert!(err.contains("direct|gateway|deny"), "got: {err}");
}

#[test]
fn app_port_parses_into_params() {
    let p = params_from(&[(&format!("{PREFIX}app-port"), "8080")]).unwrap();
    assert_eq!(p.app_port, Some(8080));
    assert_eq!(InjectParams::default().app_port, None);
    let err = params_from(&[(&format!("{PREFIX}app-port"), "http")]).unwrap_err();
    assert!(
        err.contains("app-port") && err.contains("port number"),
        "got: {err}"
    );
}

#[test]
fn app_port_renders_pod_local_upstream_and_catch_all_route() {
    // With the annotation: the app upstream (127.0.0.1:<port>) plus the
    // catch-all route land between [egress] and [ingress] - and the
    // rendered TOML still boots through the real load_config.
    let p = InjectParams {
        app_port: Some(8080),
        ..InjectParams::default()
    };
    let src = config::render_config_toml(&p).expect("renders");
    assert!(
        src.contains("name = \"app\"\n"),
        "upstream named app: {src}"
    );
    assert!(
        src.contains("addr = \"127.0.0.1:8080\"\n"),
        "pod-local peer: {src}"
    );
    assert!(
        src.contains("path_prefix = \"/\"\nupstream = \"app\"\n"),
        "catch-all route: {src}"
    );
    let egress = src.find("[egress]").expect("egress section");
    let app = src.find("[[upstreams]]").expect("upstreams section");
    let ingress = src.find("[ingress]").expect("ingress section");
    assert!(
        egress < app && app < ingress,
        "block sits between egress and ingress"
    );
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "openrusty-inject-appport-{}.toml",
        std::process::id()
    ));
    std::fs::write(&path, &src).unwrap();
    let cfg = load_config(&path).unwrap_or_else(|e| panic!("app-port config boots: {e}"));
    let _ = std::fs::remove_file(&path);
    assert_eq!(cfg.upstreams.len(), 1);
    assert_eq!(cfg.upstreams[0].name, "app");
    assert_eq!(cfg.upstreams[0].peers[0].addr.to_string(), "127.0.0.1:8080");
    assert_eq!(cfg.routes.len(), 1);
    assert_eq!(cfg.routes[0].path_prefix, "/");
    assert_eq!(cfg.routes[0].upstream, "app");
}

#[test]
fn no_app_port_renders_no_upstreams_or_routes() {
    // Without the annotation today's shape is preserved: no upstreams,
    // no routes - the router stays untouched.
    let src = config::render_config_toml(&InjectParams::default()).expect("renders");
    assert!(!src.contains("[[upstreams]]"), "no upstreams block: {src}");
    assert!(!src.contains("[[routes]]"), "no routes block: {src}");
}

#[test]
fn ignore_inbound_ports_unions_admin_and_sorts() {
    let p = params_from(&[(&format!("{PREFIX}skip-inbound-ports"), "15002,9090")]).unwrap();
    assert_eq!(ignore_inbound_ports(&p), "4191,9090,15002");
    assert_eq!(ignore_inbound_ports(&InjectParams::default()), "4191");
    let p = params_from(&[(&format!("{PREFIX}skip-inbound-ports"), "4191")]).unwrap();
    assert_eq!(ignore_inbound_ports(&p), "4191");
}

#[test]
fn rendered_toml_loads_through_real_load_config() {
    // The critical lock: whatever the renderer emits must boot. Parse +
    // validate through the exact production entry point.
    let dir = std::env::temp_dir();
    for (mutate, check) in [
        (InjectParams::default(), "direct"),
        (
            InjectParams {
                egress_mode: EgressMode::Gateway,
                egress_gateway: "127.0.0.1:4140".into(),
                ..InjectParams::default()
            },
            "gateway",
        ),
        (
            InjectParams {
                egress_mode: EgressMode::Deny,
                ..InjectParams::default()
            },
            "deny",
        ),
    ] {
        let src = config::render_config_toml(&mutate).expect("renders");
        let path = dir.join(format!(
            "openrusty-inject-test-{}-{check}.toml",
            std::process::id()
        ));
        std::fs::write(&path, &src).unwrap();
        let cfg = load_config(&path).unwrap_or_else(|e| panic!("{check}: {e}"));
        let _ = std::fs::remove_file(&path);
        let listeners = openrusty_core::effective_listeners(&cfg);
        assert_eq!(listeners.len(), 3, "{check}: {src}");
        assert_eq!(listeners[0].role, ListenerRole::Inbound);
        assert_eq!(listeners[0].listen.port(), INBOUND_PORT);
        assert!(listeners[0].transparent);
        assert_eq!(listeners[1].role, ListenerRole::Outbound);
        assert_eq!(listeners[1].listen.port(), OUTBOUND_PORT);
        assert!(listeners[1].transparent);
        assert_eq!(listeners[2].role, ListenerRole::Admin);
        assert_eq!(listeners[2].listen.port(), ADMIN_PORT);
        assert!(!listeners[2].transparent);
        let expect = match check {
            "direct" => EgressMode::Direct,
            "gateway" => EgressMode::Gateway,
            _ => EgressMode::Deny,
        };
        assert_eq!(cfg.egress.mode, expect, "{check}: {src}");
        assert_eq!(cfg.plugins.dir, PLUGINS_DIR);
        assert!(!cfg.ingress.enabled);
        assert!(cfg.routes.is_empty() && cfg.upstreams.is_empty());
    }
}

#[test]
fn e2e_fixture_deployment_gets_three_pieces() {
    let out = run(&fixture()).expect("injects");
    let mut docs = out.split("\n---\n");
    let workload: serde_yaml::Value =
        serde_yaml::from_str(docs.next().expect("workload doc")).unwrap();
    let cm: serde_yaml::Value = serde_yaml::from_str(docs.next().expect("configmap doc")).unwrap();
    assert!(docs.next().is_none(), "exactly two documents");

    let spec = &workload["spec"]["template"]["spec"];
    // a. init container: flags encode the fixture annotations.
    let init = &spec["initContainers"][0];
    assert_eq!(init["name"].as_str(), Some(INIT_NAME));
    assert_eq!(init["command"].as_sequence().unwrap().len(), 10);
    assert_eq!(init["command"][1].as_str(), Some("iptables-init"));
    let cmd: Vec<String> = init["command"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let flag = |name: &str| cmd.iter().position(|a| a == name).unwrap();
    assert_eq!(cmd[flag("--proxy-uid") + 1], "511");
    assert_eq!(cmd[flag("--inbound-port") + 1], "4143");
    assert_eq!(cmd[flag("--outbound-port") + 1], "4140");
    assert_eq!(cmd[flag("--ignore-inbound-ports") + 1], "4191,9090,15002");
    // Narrowed init privilege envelope: root + NET_ADMIN/NET_RAW, never
    // `privileged`.
    assert_eq!(init["securityContext"]["runAsUser"].as_u64(), Some(0));
    let caps: Vec<&str> = init["securityContext"]["capabilities"]["add"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(caps, vec!["NET_ADMIN", "NET_RAW"]);
    assert!(init["securityContext"].get("privileged").is_none());

    // b. sidecar: uid, three ports, config mount, opaque env passthrough.
    let sidecars: Vec<&serde_yaml::Value> = spec["containers"]
        .as_sequence()
        .unwrap()
        .iter()
        .filter(|c| c["name"].as_str() == Some(SIDECAR_NAME))
        .collect();
    assert_eq!(sidecars.len(), 1);
    let sidecar = sidecars[0];
    assert_eq!(sidecar["securityContext"]["runAsUser"].as_u64(), Some(511));
    assert_eq!(sidecar["args"][0].as_str(), Some(CONFIG_PATH));
    let ports: Vec<u16> = sidecar["ports"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|p| p["containerPort"].as_u64().unwrap() as u16)
        .collect();
    assert_eq!(ports, vec![INBOUND_PORT, OUTBOUND_PORT, ADMIN_PORT]);
    assert_eq!(sidecar["env"][0]["name"].as_str(), Some(OPAQUE_PORTS_ENV));
    assert_eq!(sidecar["env"][0]["value"].as_str(), Some("8443"));
    assert_eq!(
        sidecar["volumeMounts"][0]["mountPath"].as_str(),
        Some(CONFIG_MOUNT)
    );
    // The app container survives untouched.
    assert!(spec["containers"]
        .as_sequence()
        .unwrap()
        .iter()
        .any(|c| c["name"].as_str() == Some("echo")));
    // Config volume references the ConfigMap.
    assert_eq!(
        spec["volumes"][0]["configMap"]["name"].as_str(),
        Some("echo-openrusty-config")
    );

    // c. the ConfigMap document: name, key, and loadable TOML.
    assert_eq!(cm["kind"].as_str(), Some("ConfigMap"));
    assert_eq!(
        cm["metadata"]["name"].as_str(),
        Some("echo-openrusty-config")
    );
    assert_eq!(cm["metadata"]["namespace"].as_str(), Some("demo"));
    let toml_src = cm["data"]["openrusty.toml"].as_str().expect("toml");
    let dir = std::env::temp_dir();
    let path = dir.join(format!("openrusty-inject-e2e-{}.toml", std::process::id()));
    std::fs::write(&path, toml_src).unwrap();
    let cfg = load_config(&path).expect("injected config loads");
    let _ = std::fs::remove_file(&path);
    assert_eq!(cfg.egress.mode, EgressMode::Direct);
}

#[test]
fn disabled_annotation_passes_input_through() {
    let raw = fixture().replace(
        &format!("{PREFIX}inject: \"enabled\""),
        &format!("{PREFIX}inject: \"disabled\""),
    );
    assert_eq!(run(&raw).unwrap(), raw);
}

#[test]
fn multidoc_unsupported_kind_and_double_inject_fail() {
    // Multi-document input is a v1 hard error.
    let multi = format!("{}\n---\n{}\n", fixture(), fixture());
    let err = run(&multi).unwrap_err();
    assert!(err.contains("multi-document"), "got: {err}");
    // Non-workload kinds are rejected.
    let err = run("apiVersion: v1\nkind: Service\nmetadata:\n  name: x\n").unwrap_err();
    assert!(err.contains("not injectable"), "got: {err}");
    // Injecting twice refuses.
    let once = run(&fixture()).unwrap();
    let workload_doc = once.split("\n---\n").next().unwrap();
    let err = run(workload_doc).unwrap_err();
    assert!(err.contains("refusing to inject twice"), "got: {err}");
}

#[test]
fn bare_pod_injects_at_spec() {
    let pod = "apiVersion: v1\nkind: Pod\nmetadata:\n  name: scratch\n  annotations:\n    config.openrusty.io/proxy-uid: \"7\"\nspec:\n  containers:\n    - name: app\n      image: app\n";
    let out = run(pod).unwrap();
    let workload: serde_yaml::Value =
        serde_yaml::from_str(out.split("\n---\n").next().unwrap()).unwrap();
    let cmd: Vec<String> = workload["spec"]["initContainers"][0]["command"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let uid_at = cmd.iter().position(|a| a == "--proxy-uid").unwrap();
    assert_eq!(
        cmd[uid_at + 1],
        "7",
        "uid annotation encoded into init flags"
    );
    assert!(
        out.contains("scratch-openrusty-config"),
        "configmap named after the pod: {out}"
    );
}

#[test]
fn run_cli_exit_codes() {
    let no_args: Vec<String> = Vec::new();
    assert_eq!(run_cli(&no_args, fixture().as_bytes(), std::io::sink()), 0);
    assert_eq!(
        run_cli(&no_args, "kind: Service\n".as_bytes(), std::io::sink()),
        1
    );
    assert_eq!(run_cli(&no_args, "".as_bytes(), std::io::sink()), 1);
    assert_eq!(run_cli(&no_args, "a: [1,\n".as_bytes(), std::io::sink()), 1);
    // Bad flags exit 2 (iptables-init convention); -h prints usage, exit 0.
    let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        run_cli(&argv(&["--bogus"]), "".as_bytes(), std::io::sink()),
        2
    );
    assert_eq!(
        run_cli(&argv(&["--image"]), "".as_bytes(), std::io::sink()),
        2
    );
    assert_eq!(run_cli(&argv(&["-h"]), "".as_bytes(), std::io::sink()), 0);
}

#[test]
fn image_flag_overrides_both_containers() {
    let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    // a. --image REF: both the init container and the sidecar carry it.
    let opts = parse_cli_args(&argv(&["--image", "reg.example/openrusty:1.2.3"])).unwrap();
    assert_eq!(opts.image.as_deref(), Some("reg.example/openrusty:1.2.3"));
    let out = run_with_image(&fixture(), "reg.example/openrusty:1.2.3").unwrap();
    let workload: serde_yaml::Value =
        serde_yaml::from_str(out.split("\n---\n").next().unwrap()).unwrap();
    let spec = &workload["spec"]["template"]["spec"];
    assert_eq!(
        spec["initContainers"][0]["image"].as_str(),
        Some("reg.example/openrusty:1.2.3")
    );
    assert_eq!(
        spec["containers"]
            .as_sequence()
            .unwrap()
            .iter()
            .find(|c| c["name"].as_str() == Some(SIDECAR_NAME))
            .unwrap()["image"]
            .as_str(),
        Some("reg.example/openrusty:1.2.3")
    );
    // The `--image=REF` spelling parses identically.
    let opts = parse_cli_args(&argv(&["--image=reg.example/openrusty:1.2.3"])).unwrap();
    assert_eq!(opts.image.as_deref(), Some("reg.example/openrusty:1.2.3"));

    // b. unknown flag is a parse error.
    assert!(parse_cli_args(&argv(&["--bogus"])).is_err());

    // c. no flag: the default IMAGE const lands in both containers.
    assert!(parse_cli_args(&[]).unwrap().image.is_none());
    let out = run(&fixture()).unwrap();
    assert!(out.contains(IMAGE), "default image rendered: {IMAGE}");
}
