# Fixtures

All values in these fixtures are synthetic dummies for unit tests only:

- tokens are literally `dummy-token-for-tests` / `dummy-exec-token`;
- `*-data` fields are base64 of obviously invalid placeholder PEM blocks
  (`Zm9v...` = "foo..." payloads);
- `test-ca.crt` is a throwaway self-signed CA generated locally for TLS
  config-construction tests only. Its private key was discarded at
  generation time and was never committed. It trusts nothing and authenticates
  nothing; do not reuse it anywhere.
