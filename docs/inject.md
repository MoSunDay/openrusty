# `openrusty inject` - static sidecar injection

`openrusty inject` turns a plain Kubernetes workload manifest into the
manifest carrying the openrusty sidecar pieces, without touching a
cluster. It is the same shape as `linkerd inject`: manifest in on stdin,
injected manifest out on stdout, everything else on stderr.

```sh
kubectl get deploy echo -o yaml | openrusty inject | kubectl apply -f -
```

The command is a pure renderer: no kubeconfig, no API server, no gateway
config file, no serving runtime (same one-shot contract as
`openrusty iptables-init`, whose rules the injected init container
installs).

## Input contract (v1)

- **Exactly one YAML document.** Multi-document input is rejected with
  exit 1; split pipelines first (`yq`, or one `kubectl get` per object).
- **Only injectable kinds.** `Pod` injects at `spec`; workloads with a pod
  template inject at `spec.template.spec` (`Deployment`, `ReplicaSet`,
  `StatefulSet`, `DaemonSet`, `Job`) or
  `spec.jobTemplate.spec.template.spec` (`CronJob`). Any other kind
  (Service, ConfigMap, CRDs, ...) is rejected with exit 1.
- An already-injected workload is refused (`openrusty-init`/`openrusty-proxy`
  already present) instead of being silently duplicated.

## Annotations (`config.openrusty.io/` prefix)

Workload pod template annotations are authoritative; workload-level
`metadata.annotations` are merged underneath (template wins). Bare Pods
read `metadata.annotations`.

| annotation                          | meaning                                        | default   |
|-------------------------------------|------------------------------------------------|-----------|
| `inject`                            | `enabled` / `disabled`                         | `enabled` |
| `skip-inbound-ports`                | inbound destination ports left unredirected    | none      |
| `opaque-ports`                      | recorded in `OPENRUSTY_OPAQUE_PORTS` (env)     | none      |
| `proxy-uid`                         | sidecar UID, iptables owner-match exemption    | `511`     |
| `proxy-log-level`                   | `trace` / `debug` / `info` / `warn` / `error`  | `warn`    |
| `egress-mode`                       | `direct` / `gateway` / `deny`                  | `direct`  |
| `egress-gateway`                    | gateway `host:port`, required in gateway mode  | none      |

- Running the command at all means "inject", so an explicit
  `inject: "disabled"` is what opts a workload out: the input is passed
  through byte-for-byte, nothing is emitted.
- `skip-inbound-ports` feeds the init rules'
  `--ignore-inbound-ports` (always unioned with `4191`, the admin
  listener, so the hijack can never loop on its own control port).
- **`opaque-ports` is a v1 boundary**: it is only recorded as the
  `OPENRUSTY_OPAQUE_PORTS` env var on the sidecar for later versions to
  act on. It does not change the iptables rule surface.
- Unknown `config.openrusty.io/*` annotations produce a stderr warning and
  are ignored (forward compatibility with newer controllers). Malformed
  values of *known* annotations fail with exit 1 - a typo must not inject
  different rules silently.

## What gets injected (three pieces)

1. **initContainer `openrusty-init`**
   `openrusty iptables-init --proxy-uid <uid> --inbound-port 4143
   --outbound-port 4140 --ignore-inbound-ports 4191[,<skip-inbound-ports>]`
   with `securityContext.privileged: true`. iptables needs the privilege
   (NET_ADMIN would suffice; v1 keeps it simple and narrowable later).
2. **sidecar `openrusty-proxy`** - `runAsUser: <proxy-uid>`, ports
   4143 (inbound) / 4140 (outbound) / 4191 (admin), config mounted from a
   ConfigMap at `/etc/openrusty/openrusty.toml` (the path is the only
   argument).
3. **ConfigMap `<name>-openrusty-config`** - key `openrusty.toml`, a
   minimal config the renderer validates before emitting: transparent
   inbound + outbound listeners, admin listener, `[egress]` from the
   annotations, `[ingress] enabled = false`, no routes/upstreams.
   `[plugins] dir` points at `/dev/null-plugins`, a path that does not
   exist in the container on purpose: the plugin registry treats a
   missing/empty plugin dir as "no plugins" (warn, generation 0) and never
   fails, so no plugins volume is needed. If `egress-mode: "gateway"`
   names a cluster-internal DNS name, `inject` cannot resolve it
   off-cluster; it warns and lets the sidecar validate at boot.

Output is always two YAML documents (workload first, then ConfigMap).
Key order inside the workload may differ from the input because the parsed
tree is re-rendered; `kubectl apply` treats both shapes identically.
