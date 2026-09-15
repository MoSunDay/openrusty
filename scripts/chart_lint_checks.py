#!/usr/bin/env python3
"""Shape assertions for `scripts/chart-lint.sh`.

Reads the six helm-template renders (defaults / demo / nodeport /
rbac.create=false / rbac.clusterWide=true / explicit watchNamespaces)
plus the `openrusty inject` output of the shared
fixture and asserts the chart/inject contract: component shape, ports,
proxy UID, init-container flags, ConfigMaps, the watch-plane RBAC, and
the inject-CLI vs chart sidecar consistency. Every check prints PASS/FAIL;
the script exits non-zero when any check fails.
"""

import sys

import yaml

INBOUND, OUTBOUND, ADMIN = 4143, 4140, 4191
PROXY_UID = 511
WATCH_RULES = [
    {"apiGroups": [""], "resources": ["secrets"],
     "verbs": ["get", "list", "watch"]},
    {"apiGroups": ["networking.k8s.io"], "resources": ["ingresses"],
     "verbs": ["get", "list", "watch"]},
]
PASS = 0
FAIL = 0


def check(name, cond, detail=""):
    global PASS, FAIL
    if cond:
        PASS += 1
        print(f"PASS: {name}")
    else:
        FAIL += 1
        print(f"FAIL: {name} {detail}")


def docs(path):
    with open(path) as fh:
        return [d for d in yaml.safe_load_all(fh) if d]


def one(docs_, kind, suffix):
    hits = [d for d in docs_ if d.get("kind") == kind
            and str(d.get("metadata", {}).get("name", "")).endswith(suffix)]
    return hits[0] if hits else None


def container(spec, name):
    for c in spec.get("containers", []):
        if c.get("name") == name:
            return c
    return None


def port_numbers(container_):
    return sorted(p["containerPort"] for p in container_.get("ports", []))


def flag_value(command, flag):
    return command[command.index(flag) + 1] if flag in command else None


def toml_text(configmap):
    return configmap["data"]["openrusty.toml"]


def assert_shared(defaults, demo, nodeport, rbacoff, wide, watchns, injected):
    # --- default release: ingress + egress-gateway, no demo ---
    cm = one(defaults, "ConfigMap", "-ingress-config")
    check("default: ingress ConfigMap rendered", cm is not None)
    if cm:
        body = toml_text(cm)
        check("default: [ingress] enabled with openrusty class",
              'enabled = true' in body and 'ingress_class = "openrusty"' in body)
        check("default: edge listener plain (tls = false)",
              "tls = false" in body)
        check("default: [ingress] watch scoped to the release namespace",
              'namespaces = ["default"]' in body,
              body.split("\n[ingress]")[-1][:80] if "[ingress]" in body else "no [ingress]")

    dep = one(defaults, "Deployment", "-ingress")
    check("default: ingress Deployment rendered", dep is not None)
    if dep:
        check("default: ingress replicas=2", dep["spec"]["replicas"] == 2)
        gw = container(dep["spec"]["template"]["spec"], "openrusty")
        check("default: ingress container ports 8443+4191",
              gw is not None and port_numbers(gw) == [4191, 8443],
              str(port_numbers(gw) if gw else "missing"))
        check("default: ingress config volume mounted",
              gw is not None and gw["volumeMounts"][0]["mountPath"] == "/etc/openrusty")

    # --- watch-plane RBAC: SA + namespaced least-privilege Role, bound ---
    sa = one(defaults, "ServiceAccount", "-ingress")
    role = one(defaults, "Role", "-ingress")
    binding = one(defaults, "RoleBinding", "-ingress")
    check("default: SA + Role + RoleBinding with the exact watch rules",
          sa is not None and role is not None and binding is not None
          and role["rules"] == WATCH_RULES
          and binding["roleRef"]["kind"] == "Role"
          and binding["roleRef"]["name"] == role["metadata"]["name"]
          and binding["subjects"][0]["kind"] == "ServiceAccount"
          and binding["subjects"][0]["name"] == sa["metadata"]["name"]
          and dep is not None
          and dep["spec"]["template"]["spec"].get("serviceAccountName")
          == sa["metadata"]["name"],
          str(role["rules"] if role else "no Role"))

    # --- rbac.create=false: the identity plane is rendered nowhere ---
    roff = one(rbacoff, "Deployment", "-ingress")
    check("rbac.create=false: no SA/Role/RoleBinding rendered",
          one(rbacoff, "ServiceAccount", "") is None
          and one(rbacoff, "Role", "") is None
          and one(rbacoff, "RoleBinding", "") is None
          and roff is not None
          and "serviceAccountName" not in roff["spec"]["template"]["spec"])

    # --- rbac.clusterWide: cluster-scoped watch plane, TOML stays wide ---
    crole = one(wide, "ClusterRole", "-ingress")
    cbinding = one(wide, "ClusterRoleBinding", "-ingress")
    wcm = one(wide, "ConfigMap", "-ingress-config")
    check("clusterWide: ClusterRole + ClusterRoleBinding, no namespaced Role",
          crole is not None and cbinding is not None
          and crole["rules"] == WATCH_RULES
          and one(wide, "Role", "-ingress") is None
          and one(wide, "RoleBinding", "-ingress") is None
          and cbinding["roleRef"]["kind"] == "ClusterRole"
          and cbinding["subjects"][0]["namespace"] == "default",
          "missing cluster-scope objects" if crole is None else "shape drift")
    check("clusterWide: TOML leaves [ingress].namespaces unset (cluster-wide)",
          wcm is not None and "namespaces =" not in toml_text(wcm))

    # --- explicit watchNamespaces: rendered verbatim into the TOML ---
    ncm = one(watchns, "ConfigMap", "-ingress-config")
    check("watchNamespaces: explicit list rendered into the TOML",
          ncm is not None and 'namespaces = ["alpha", "beta"' in toml_text(ncm),
          toml_text(ncm).split("\n[ingress]")[-1][:80] if ncm else "no ConfigMap")

    svc = one(defaults, "Service", "-ingress")
    check("default: ingress Service LoadBalancer on 8443",
          svc is not None and svc["spec"]["type"] == "LoadBalancer"
          and [p["port"] for p in svc["spec"]["ports"]] == [8443])

    eg = one(defaults, "Deployment", "-egress-gateway")
    check("default: egress-gateway Deployment rendered", eg is not None)
    if eg:
        spec = eg["spec"]["template"]["spec"]
        names = [c["name"] for c in spec["containers"]]
        check("default: egress-gateway = pause + openrusty-proxy",
              names == ["pause", "openrusty-proxy"], str(names))
        ann = eg["spec"]["template"]["metadata"]["annotations"]
        check("default: egress-gateway identity annotations",
              ann.get("config.openrusty.io/egress-gateway") == "true"
              and ann.get("config.openrusty.io/inject") == "disabled")
        sidecar = container(spec, "openrusty-proxy")
        check("default: egress-gateway sidecar uid + ports",
              sidecar is not None
              and sidecar["securityContext"]["runAsUser"] == PROXY_UID
              and port_numbers(sidecar) == sorted([INBOUND, ADMIN]))
    esvc = one(defaults, "Service", "-egress-gateway")
    check("default: egress-gateway Service exposes both ports",
          esvc is not None
          and sorted(p["port"] for p in esvc["spec"]["ports"]) == sorted([INBOUND, ADMIN]))
    check("default: demo disabled", one(defaults, "Deployment", "-echo") is None)

    # --- nodeport release: service type override reached the template ---
    nsvc = one(nodeport, "Service", "-ingress")
    check("nodeport: ingress Service type=NodePort",
          nsvc is not None and nsvc["spec"]["type"] == "NodePort")

    # --- demo release: the static injection result ---
    ddep = one(demo, "Deployment", "-echo")
    dcm = one(demo, "ConfigMap", "-echo-openrusty-config")
    check("demo: workload + config ConfigMap rendered",
          ddep is not None and dcm is not None)
    if not (ddep and dcm):
        return

    # --- inject CLI consistency (fixture vs chart sidecar) ---
    workload = one(injected, "Deployment", "-echo") or one(injected, "Deployment", "")
    icm = one(injected, "ConfigMap", "-openrusty-config")
    check("inject: two docs, configmap named after workload",
          workload is not None and icm is not None
          and icm["metadata"]["name"]
          == f"{workload['metadata']['name']}-openrusty-config")
    if not workload:
        return
    ispec = workload["spec"]["template"]["spec"]
    dspec = ddep["spec"]["template"]["spec"]
    iinit = ispec["initContainers"][0]
    dinit = dspec["initContainers"][0]
    check("inject: init command matches the chart byte-for-byte",
          iinit["command"] == dinit["command"]
          and iinit["securityContext"] == dinit["securityContext"],
          f"{iinit['command']} vs {dinit['command']}")
    check("inject: init flags encode the fixture annotations",
          flag_value(dinit["command"], "--proxy-uid") == str(PROXY_UID)
          and flag_value(dinit["command"], "--inbound-port") == str(INBOUND)
          and flag_value(dinit["command"], "--outbound-port") == str(OUTBOUND)
          and flag_value(dinit["command"], "--ignore-inbound-ports")
          == "4191,9090,15002")
    iside = container(ispec, "openrusty-proxy")
    dside = container(dspec, "openrusty-proxy")
    faces = ("image", "args", "env", "ports", "securityContext", "volumeMounts")
    check("inject: sidecar faces match the chart",
          iside is not None and dside is not None
          and all(iside.get(f) == dside.get(f) for f in faces),
          str({f: (iside or {}).get(f) for f in faces}))
    check("inject: sidecar uid + three ports + config mount",
          iside["securityContext"]["runAsUser"] == PROXY_UID
          and port_numbers(iside) == sorted([INBOUND, OUTBOUND, ADMIN])
          and iside["volumeMounts"][0]["mountPath"] == "/etc/openrusty")
    check("inject: demo sidecar runs the same three ports",
          port_numbers(dside) == port_numbers(iside))
    check("inject: rendered TOML matches the chart ConfigMap",
          icm["data"]["openrusty.toml"] == toml_text(dcm))
    check("inject: transparent inbound/outbound + admin in TOML",
          toml_text(icm).count("transparent = true") == 2
          and f'listen = "0.0.0.0:{ADMIN}"' in toml_text(icm))


def main(argv):
    global PASS, FAIL
    paths = dict(zip(("defaults", "demo", "nodeport", "rbacoff", "wide",
                      "watchns", "injected"), argv))
    assert_shared(docs(paths["defaults"]), docs(paths["demo"]),
                  docs(paths["nodeport"]), docs(paths["rbacoff"]),
                  docs(paths["wide"]), docs(paths["watchns"]),
                  docs(paths["injected"]))
    print(f"chart-lint: {PASS} passed, {FAIL} failed")
    return 1 if FAIL else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
