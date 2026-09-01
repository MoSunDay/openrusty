# Fixtures

All values in these fixtures are synthetic dummies for unit tests only:

- tokens are literally `dummy-token-for-tests` / `dummy-exec-token`;
- `*-data` fields are base64 of obviously invalid placeholder PEM blocks
  (`Zm9v...` = "foo..." payloads);
- `test-ca.crt` is a throwaway self-signed CA generated locally for TLS
  config-construction tests only. Its private key was discarded at
  generation time and was never committed. It trusts nothing and authenticates
  nothing; do not reuse it anywhere.

## Ingress / Secret / watch fixtures (M2.b)

Same rules: everything is synthetic. `secret-tls.json` carries base64 of
obviously fake PEM blocks (`dummy-cert-for-tests` / `dummy-key-for-tests`);
hosts are `app.example.com`, services are `example-svc` / `second-svc` /
`fallback-svc` in namespace `web`. `list-ingresses.json` and
`watch-events.jsonl` are pre-recorded-shaped LIST bodies and watch stream
lines for the state-machine and watch-loop tests; `ingress-minimal.json`
intentionally omits `ingressClassName` and `pathType` to cover the
"no class never matches" and "missing pathType degrades to
ImplementationSpecific" rules.
