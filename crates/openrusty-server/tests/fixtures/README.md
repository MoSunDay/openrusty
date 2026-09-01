# TLS termination fixtures (M2.c2)

All values in these fixtures are synthetic dummies for unit and e2e tests
only. The CA and leaf key material was generated locally with openssl and
authenticates nothing: `echo.example.com` is not a real zone, the CA is
self-signed, and nothing in these files is a credential for any real
system. Do not reuse them anywhere.

Files:

- `ca.crt` / `ca.key` - throwaway self-signed CA
  (`CN=openrusty-dummy-test-ca, O=openrusty`), 2048-bit RSA;
- `server.crt` / `server.key` - leaf certificate for
  `CN=echo.example.com` with SAN `DNS:echo.example.com,DNS:localhost`,
  signed by that CA (the SNI-map fixture identity);
- `server-rotated.crt` / `server-rotated.key` - renewed leaf for the
  same name, used as the rotation-test's "after" identity;
- `fallback.crt` / `fallback.key` - wildcard leaf
  (`DNS:*.example.com`), the static default/fallback fixture pair: a
  default cert has to validate for names that are not known up front.

Regenerate locally with (the committed copies are the canonical dummies;
openssl needs to be on PATH only when regenerating):

```sh
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.crt \
  -days 36500 -subj "/CN=openrusty-dummy-test-ca/O=openrusty" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign"
openssl req -newkey rsa:2048 -nodes -keyout server.key -out server.csr \
  -subj "/CN=echo.example.com/O=openrusty"
printf "subjectAltName=DNS:echo.example.com,DNS:localhost\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n" > server.ext
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -out server.crt -days 36500 -extfile server.ext
rm -f server.csr server.ext ca.srl
```

Same ground rules as `crates/openrusty-k8s/tests/fixtures/README.md`:
everything here is synthetic. These files never leave the test target.
