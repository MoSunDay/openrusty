# OpenRusty K8s forms: sidecar, ingress, egress

One binary, three deployment forms. A single `openrusty` process is shaped
purely by its TOML config into:

- a **sidecar** that transparently intercepts a pod's inbound and outbound
  TCP in front of iptables REDIRECT rules;
- an **ingress gateway** that watches cluster `Ingress` objects and TLS
  `Secret`s and renders routes at runtime;
- an **egress gateway** (or an egress policy inside a sidecar) that applies
  a tri-mode policy to intercepted outbound connections.

The forms compose: an ingress gateway is a non-transparent inbound listener
plus `[ingress]`; a sidecar is transparent data listeners plus an optional
`[egress]` policy. Nothing in the forms changes the plugin ABI - HTTP
connections intercepted anywhere run the same 8-phase pipeline (see
[docs/wasm-abi.md](./wasm-abi.md)).

| form | what it does | config surface | sockets (convention) |
|------|--------------|----------------|----------------------|
| sidecar | intercepts local workload traffic, in and out | `[[server.listeners]]` (`transparent = true`), `[egress]` | inbound 4143, outbound 4140, admin 4191 |
| ingress | adopts `networking.k8s.io/v1` Ingresses + TLS Secrets | `[ingress]`, `tls = true` listeners | edge data (e.g. 8443), admin 4191 |
| egress | policy for intercepted outbound traffic | `[egress]` (`direct` / `gateway` / `deny`) | outbound 4140, admin 4191 |

## Listeners and ports

### `[[server.listeners]]`

Role-scoped sockets under `[server.listeners]` (linkerd-style split):

| field | applies to | meaning | default |
|-------|-----------|---------|---------|
| `role` | all | `inbound` / `outbound` / `admin` | required |
| `listen` | all | socket address to bind | required |
| `http1_only` | inbound, outbound | skip the h2c preface sniff; plain HTTP/1.1 only (admin always speaks both) | `false` |
| `transparent` | inbound, outbound | a REDIRECT/DNAT/TPROXY rule sits in front; run the intercept path | `false` |
| `detect_timeout_ms` | when `transparent` | total protocol-sniff budget; a silent peer is opaque | `3000` |
| `tls` | inbound, admin | terminate TLS, route by SNI (outbound parses but ignores it) | `false` |
| `tls_cert`, `tls_key` | when `tls` | static PEM pair, read once at boot; both or neither | none |

Validation rules (fail at config load):

- at most one listener per role; no duplicated address;
- `transparent` and `tls` are mutually exclusive on data-plane roles (v1
  has no TLS-over-transparent);
- `detect_timeout_ms` must be `> 0` whenever `transparent = true`;
- `tls = true` needs a certificate source: a static pair, `[ingress]`
  enabled, or both;
- `egress.gateway` must be set and resolvable when
  `egress.mode = "gateway"`.

### Derived default

With **no** `[[server.listeners]]`, one `inbound` listener is derived from
`server.listen` (+ `server.http1_only`): the historical single-socket
shape, never transparent, admin routes included. With listeners written,
they are the only authority and `server.listen` is dead config (a warning
is logged once at boot). When an `admin` listener exists, the
`/openrusty/*` plane lives **only** on that socket and is detached from
all data ports.

### Port conventions

| port | role | notes |
|------|------|-------|
| 4143 | inbound | REDIRECT target for pod-local serving; egress-gateway data port |
| 4140 | outbound | REDIRECT target for pod egress |
| 4191 | admin | `/openrusty/*`; always exempt from inbound redirection |
| 443 | - | exempt from outbound redirection by default (v1 egress cannot carry TLS) |

## Sidecar: transparent interception

Per connection on a `transparent = true` listener:

1. **Original destination.** `SO_ORIGINAL_DST` recovers the pre-NAT
   destination recorded by conntrack.
2. **Loop guard.** The recovered destination is compared against *every*
   listener port (all roles). A hit is refused and counted - the gateway
   must never tunnel into itself.
3. **Protocol detection** (inbound; outbound sees step 4 first).
4. **Dispatch** by role and sniffed protocol.

Failure semantics differ by role: on **inbound** an unrecoverable original
destination degrades to the plain (non-transparent) HTTP pipeline -
transparency is best-effort, service is not. On **outbound** the original
destination *is* the dial target, so without it the connection is closed.

### Loop guard is fail-fast by design

The guard runs **only** on `transparent = true` listeners. A transparent
listener without a REDIRECT in front of it reports its own address as the
"original" destination of every connection, so the guard rejects *all*
connections loudly instead of silently proxying a misconfigured socket.
If every connection to a transparent listener is refused, look for the
missing/mis-aimed REDIRECT rule, not for a routing bug.

### Protocol detection

A bounded prefix of the stream is sniffed; sniffing never consumes a byte
(the prefix is re-injected ahead of the live socket):

| verdict | when | consequence |
|---------|------|-------------|
| `H1` | request line starts with an HTTP/1 method token | normal pipeline, HTTP/1 forced |
| `H2` | stream starts with `PRI *` (5 bytes of the h2 preface) | normal pipeline, HTTP/2 forced |
| `Opaque` | anything else, or silence past `detect_timeout_ms` | raw TCP tunnel to the original destination |
| truncated prefix (EOF) | peer gave up mid-preface | connection closed (`UnexpectedEof`), never guessed |

The timeout is one total budget from the first read; on expiry the
connection is treated as opaque *with* the bytes already read. The
opaque tunnel is a plaintext byte splice: **it does not traverse the wasm
phases**; plugins only see intercepted HTTP.

## Egress tri-mode

`[egress]` steers intercepted **outbound** connections only (inbound and
admin listeners never consult it). Everything defaults to `direct`, so a
config without the section behaves exactly as before.

| `mode` | sniff | destination | disposition |
|--------|-------|-------------|-------------|
| `direct` (default) | none | any | tunnel verbatim to the original destination |
| `deny` | none | any | refuse (fail closed) |
| `gateway` | not yet | any | sniff first, then: |
| `gateway` | H1/H2 | port != 443 | forward the byte stream to `[egress].gateway` |
| `gateway` | H1/H2 | port == 443 | refuse (`TlsPort`) |
| `gateway` | Opaque | any | refuse (`Opaque`) |

Gateway-mode rationale: the gateway hop is plaintext TCP, so an opaque
stream (a TLS ClientHello included) would be an undecryptable byte blob
the gateway could only re-tunnel - the sidecar refuses it instead of
pretending to proxy it, and a TLS destination on the plaintext hop is out
of v1 scope (the iptables-init default already exempts 443).

`gateway` is a `host:port` address, never a URL, and carries no
credentials. Forwarding is byte-for-byte: no header rewrite, no Host
mangling, the sniffed prefix re-injected. A gateway dial that fails or
exceeds 3 s closes the intercepted connection (fail close).

```toml
[egress]
mode = "direct"                 # direct | gateway | deny
# gateway = "egress-gw.mesh.svc:8180"   # required when mode = "gateway"
```

## Ingress

`[ingress]` turns the gateway into a controller for `networking.k8s.io/v1`
Ingresses whose `spec.ingressClassName` matches. Ingress is an
*enhancement*, never a dependency: if credentials or the apiserver are
unavailable the gateway logs the failure and keeps serving its static
config.

| field | meaning | default |
|-------|---------|---------|
| `enabled` | run the watch plane | `false` |
| `ingress_class` | class this gateway adopts (empty/absent class never matches) | `"openrusty"` |
| `kubeconfig` | explicit kubeconfig path; empty = `$KUBECONFIG` -> `~/.kube/config` -> in-cluster service account | `""` |
| `namespaces` | namespaces to watch; empty = cluster-wide (still RBAC-bounded) | `[]` |

### Watch semantics

- One list/watch loop per resource path: Ingresses, and Secrets filtered
  to `type=kubernetes.io/tls`; cluster-wide, or one namespaced loop per
  configured namespace.
- Each cycle: `LIST` (full-replacement snapshot) -> `WATCH` pinned at the
  list `resourceVersion` -> fold events -> hand over once the stream has
  been quiet for a 200 ms debounce.
- `410 Gone`, stream end, malformed line or transport error: re-list with
  exponential backoff (100 ms, x2, capped at 30 s; any successful LIST
  resets it). A second 200 ms window folds Ingress and Secret hand-overs
  into one render+apply.
- **Stale-serve is the contract**: the apply side always holds an
  immutable snapshot that is only ever replaced by a fresher one - never
  cleared. While the apiserver is unreachable the gateway keeps serving
  the last snapshot; there is no "config lost" mode.
- A successful apply publishes the new routes atomically at the *current*
  plugin registry generation - a route swap never recompiles plugins.

### Conflict policy

Rendered routes merge with the static TOML routes under an explicit key:
`(host, path_prefix, exact)`. A key claimed twice - inside either input or
across the two - is a hard error: the whole apply is rejected and the
previously applied config stays authoritative. The same applies to any
render error (missing or non-TLS Secret, named service port, resource
backend, path not starting with `/`) and to two TLS sections claiming the
same host with different Secrets.

### TLS termination and SNI

A `tls = true` listener wraps a shared SNI resolver. Certificate sources
combine freely:

- **ingress** (dynamic): every adopted Ingress TLS section contributes its
  `kubernetes.io/tls` Secret; the host -> certificate map is republished
  on every successful apply (hot rotation). rustls clones the selected
  key into each session, so rotation never disturbs in-flight
  connections - only new handshakes see the new material.
- **static** (boot-only): `tls_cert` + `tls_key`, read once while binding.

When both exist, ingress wins for every SNI name it serves and the static
pair is the fallback for SNI misses. With no static pair, an SNI miss
fails the handshake (logged per connection); the listener keeps serving.

### Routes and upstreams

- Ingress rules render into gateway routes: `pathType: Exact` becomes an
  exact route, everything else a prefix route; the rule `host` becomes the
  route's host constraint (host class wins before longest-prefix path
  matching; an exact hit wins outright within its host class).
- Backends render into one upstream per unique `(namespace, service,
  port)`, named `ing-{ns}-{svc}-{port}`, dialing the ClusterIP DNS name
  `svc.ns.svc.cluster.local:port` directly (kube-proxy owns per-pod load
  balancing behind it).
- Rendered upstreams disable passive health (`max_fails = 0`): with a
  single ClusterIP endpoint per service, a few 5xx must not mark the whole
  service down.

## Deployment surface

### `openrusty iptables-init`

One-shot installer of the nat-table REDIRECT rules in front of the
transparent listeners (linkerd `proxy-init` parameter surface). Custom
chains `OPENRUSTY_IN` (hooked from PREROUTING) and `OPENRUSTY_OUT` (from
OUTPUT); every rule is tagged with the `openrusty-init` comment.

| flag | meaning | default |
|------|---------|---------|
| `--proxy-uid <UID>` | proxy UID, exempted on OUTPUT (**first rule** of the OUT chain - the proxy must never loop into itself) | required |
| `--inbound-port <PORT>` | inbound REDIRECT target | `4143` |
| `--outbound-port <PORT>` | outbound REDIRECT target | `4140` |
| `--ignore-inbound-ports <LIST>` | inbound ports RETURNed before REDIRECT | `4191` |
| `--ignore-outbound-ports <LIST>` | outbound ports RETURNed before REDIRECT | `443` |
| `--skip-subnets <LIST>` | CIDRs RETURNed before the outbound REDIRECT | none |
| `--backend <auto\|iptables>` | command flavour probe | `auto` |
| `--dry-run` | print the plan to stdout, execute nothing | off |

Idempotence: chains are created if missing, flushed and repopulated on
every run; the PREROUTING/OUTPUT hook jumps are installed only when an
existence check (`iptables -C`) fails - two consecutive runs leave an
identical `iptables-save`. Order inside `OPENRUSTY_OUT` is load-bearing:
owner RETURN -> ignore-outbound-ports RETURN -> skip-subnets RETURN ->
full-port REDIRECT. Preflight self-checks run before any mutation:
conntrack availability, the iptables command, and a write/delete REDIRECT
probe on a scratch chain.

### `openrusty inject`

Static sidecar injection (see [docs/inject.md](./inject.md) for the full
contract): one workload YAML document in on stdin, the injected manifest
plus a ConfigMap out on stdout. Annotation surface
(`config.openrusty.io/` prefix; pod template wins over workload
metadata):

| annotation | meaning | default |
|------------|---------|---------|
| `inject` | `enabled` / `disabled` (explicit opt-out passes the input through untouched) | `enabled` |
| `skip-inbound-ports` | inbound ports left unredirected (always unioned with 4191) | none |
| `opaque-ports` | recorded in the `OPENRUSTY_OPAQUE_PORTS` env only - **v1 passthrough, no rule change** | none |
| `proxy-uid` | sidecar UID and iptables owner-match exemption | `511` |
| `proxy-log-level` | `trace`/`debug`/`info`/`warn`/`error` | `warn` |
| `egress-mode` | `direct` / `gateway` / `deny` | `direct` |
| `egress-gateway` | gateway `host:port`, required in gateway mode | none |

Three pieces are injected: the `openrusty-init` initContainer (running
`iptables-init` with the flags above), the `openrusty-proxy` sidecar
(`runAsUser: <proxy-uid>`, ports 4143/4140/4191, config mounted from the
`<name>-openrusty-config` ConfigMap at `/etc/openrusty/openrusty.toml`),
and that ConfigMap - a minimal validated config with transparent inbound +
outbound listeners, an admin listener, `[egress]` from the annotations,
`[ingress] enabled = false`, and `[plugins] dir = "/dev/null-plugins"`
(a missing plugin dir means "no plugins", so no plugins volume is needed).
v1 input contract: exactly one YAML document, injectable kinds only
(Pod + pod-template workloads), re-injection refused.

### Helm chart (`deploy/charts/openrusty`)

| piece | content |
|-------|---------|
| `ingress` | Deployment (default 2 replicas) + `LoadBalancer` Service on plain-HTTP data port 8443 (`tls = false`; edge TLS stays with the fronting LB in v1) + admin 4191; `[ingress] enabled = true` rendered into an in-chart ConfigMap; `/openrusty/ready` + `/openrusty/live` probes |
| `egress-gateway` | Deployment (pause container + statically baked `openrusty-proxy` sidecar, transparent inbound 4143, `[egress] mode = "deny"` fail-closed until gateway policy lands) + Service exposing data 4143 and admin/probe 4191; identity annotations `config.openrusty.io/egress-gateway: "true"` and `inject: "disabled"` (re-inject can never double-inject) |
| `demo` | shared fixture workload with its injection result written out statically; `demo.enabled=false` by default |

`scripts/chart-lint.sh` helm-templates three releases and asserts the
rendered sidecar matches the `openrusty inject` CLI output (ports, UID,
init flags, mounts, TOML body). No cluster or kubeconfig involved.

## Lifecycle

One shutdown signal, one three-phase sequence. `SIGTERM`/`SIGINT` and
`POST /openrusty/shutdown` are strictly equivalent - both flip the same
watch flag, and every path converges on the same runner:

1. flag flips: every accept loop (plain, TLS, transparent) stops and
   closes its socket; accepted connections drain through hyper's graceful
   shutdown; established opaque tunnels keep serving until their peers
   close or the grace expires;
2. bounded wait until every accept task ended and the in-flight counter
   reached zero (polled every 10 ms);
3. summary log (drained vs. force-closed, drain wall-clock); exit 0 either
   way - an expired grace only adds a warn.

| endpoint | serving | draining |
|----------|---------|----------|
| `GET /openrusty/ready` | `200 {"status":"ready"}` | `503 {"status":"draining"}` |
| `GET /openrusty/live` | `200 {"status":"live"}` | `200 {"status":"live"}` (orchestrators must not restart a draining proxy) |

`server.shutdown_grace_ms` bounds phase 2 (default `5000`). The
`/openrusty/*` routes live only on the `admin` listener when one exists.

## Observability

### `openrusty_transparent_conns_total{role,outcome}`

One counter increment per intercepted connection; the disposition is the
label:

| outcome | role | meaning |
|---------|------|---------|
| `http` | inbound | served through the pipeline (sniffed or degraded) |
| `tunnel` | inbound | opaque stream spliced to the original destination |
| `loop_rejected` | both | loop guard refused a connection aimed at our own ports |
| `no_orig_dst` | outbound | original destination unrecoverable; connection closed |
| `egress_direct` | outbound | tunneled verbatim to the original destination |
| `egress_deny` | outbound | refused (`deny` mode, opaque, or port 443 in gateway mode) |
| `egress_gateway_ok` | outbound | forwarded to the egress gateway |
| `egress_gateway_fail` | outbound | gateway dial failed or timed out (fail close) |

### Ingress node of `/openrusty/status`

When `[ingress]` is configured the status JSON carries an `ingress` node:

| field | meaning |
|-------|---------|
| `enabled` | mirrors the config switch |
| `watching` | loops actually running (client built) |
| `ingresses.generation` | 1-based ordinal of the last hand-over; `0` before the first successful LIST |
| `ingresses.last_rv` | resource version of the snapshot being served |
| `ingresses.reconnects` | watch streams that ended abnormally (410, EOF, error) |
| `ingresses.last_success_age_ms` | ms since the last successful LIST; `null` before the first |
| `secrets` | the same counter shape for the TLS Secret loop(s) |

These are scrape-time gauges, not counter series: the watch plane is
observed through the status node, while the data plane is observed through
the `openrusty_transparent_conns_total` / `openrusty_requests_total`
families on `/openrusty/metrics`.

## Security boundaries

- **Service account.** The watch plane needs read-only access to Ingresses
  and Secrets - `get`/`list`/`watch`, cluster-wide or namespaced per the
  `[ingress].namespaces` setting. Namespace-scoping the Role is the
  default posture: the gateway renders only what it can read. Reading a
  TLS Secret is reading the private key, so Secret access is the
  privilege - grant it only where TLS adoption is expected. (The chart
  does not render RBAC objects yet; they land with the real image
  channel.)
- **Credentials.** A kubeconfig path is the only credential-shaped config
  field; secrets travel via mounted files or env (`$KUBECONFIG`), never
  inside `openrusty.toml`. In-cluster mode uses the mounted service
  account token. Basic-auth kubeconfig users are parsed but rejected.
- **Init privileges.** The injected init container runs with
  `privileged: true` because iptables needs it; `NET_ADMIN` would
  suffice, and dropping to a dedicated capability is the planned
  narrowing. `--dry-run` prints the plan without touching the kernel.

## Local drills

| script | covers |
|--------|--------|
| `scripts/local-netns-test.sh` | full transparent inbound/outbound path under a real iptables REDIRECT inside a throwaway netns: orig_dst recovery, opaque byte-faithful tunnel, loop guard, iptables-init idempotence, graceful shutdown (26 checks) |
| `scripts/local-egress-test.sh` | egress tri-mode over a netns topology: direct/deny/gateway phases with the `openrusty_transparent_conns_total` contract as the assertion surface, XFF injection, opaque refusal (21 checks) |
| `scripts/chart-lint.sh` | helm-render the chart (3 releases) plus the inject-CLI vs chart sidecar consistency drill (24 checks) |
| `scripts/cluster-e2e.sh` | M4 cluster e2e, seven assertion groups: preflight (RBAC/version/image, gaps named), deploy + reach (LB with recorded NodePort fallback), routes + TLS handshake, watch resilience (stale-serve blackout), conflict red lines, removal rollback, 10-min observation window; `--preflight` runs without a cluster |

The three local drills gate their environment up front and print a
visible SKIP instead of failing on un-capable machines; no cluster is
involved. `cluster-e2e.sh` follows the same gate: without kubectl, a
kubeconfig or a reachable cluster it names every gap and SKIPs
(exit 0); with them, a hard G1 gap fails loudly (exit 1).
