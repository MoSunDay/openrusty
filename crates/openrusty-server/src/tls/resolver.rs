//! SNI-driven certificate selection: the shared [`DynamicCertResolver`]
//! every TLS-terminating listener serves from, plus the one mutation
//! entry point used by the ingress apply loop.
//!
//! Purely read-side lookups: `resolve` maps the client hello's server
//! name (lowercased) through an `ArcSwap`-held `BTreeMap`, falls back to
//! a static default pair, and returns `None` (rustls fails the
//! handshake) when neither matches. Writes replace the whole map via
//! `rcu`, so readers always see one consistent table.
//!
//! Rotation never disturbs established connections: rustls clones the
//! `Arc<CertifiedKey>` into each negotiated session, so swapping the map
//! (or the fallback) only affects handshakes that start afterwards.
//! [`tests::in_flight_selection_survives_a_table_swap`] locks that
//! semantics in.

use arc_swap::ArcSwap;
use arc_swap::ArcSwapOption;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::collections::BTreeMap;
use std::sync::Arc;

use openrusty_k8s::render::TlsPair;

/// Host -> certificate map plus optional static default, read on every
/// TLS handshake. All TLS-terminating listeners share one instance, so
/// an ingress apply (or a boot-time static seed) is visible to all of
/// them at once.
pub struct DynamicCertResolver {
    /// SNI name (lowercase) -> material. Replaced wholesale on every
    /// ingress apply; one bad secret skips its entry instead of blocking
    /// the rest of the table.
    map: ArcSwap<BTreeMap<String, Arc<CertifiedKey>>>,
    /// Static default used when SNI misses the map (or carries no name
    /// at all). `None` means an unresolved miss fails the handshake.
    fallback: ArcSwapOption<CertifiedKey>,
}

impl std::fmt::Debug for DynamicCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let map = self.map.load();
        f.debug_struct("DynamicCertResolver")
            .field("hosts", &map.len())
            .field("has_fallback", &self.fallback.load().is_some())
            .finish()
    }
}

impl DynamicCertResolver {
    /// An empty resolver: no SNI entries, no fallback (every handshake
    /// fails until material is published into it).
    pub fn new() -> Self {
        Self {
            map: ArcSwap::from_pointee(BTreeMap::new()),
            fallback: ArcSwapOption::empty(),
        }
    }

    /// Pure read-side selection: exact (case-insensitive) SNI match on
    /// the map, then the static fallback, then `None`. This is exactly
    /// what `ResolvesServerCert::resolve` runs per handshake; public so
    /// tests can probe the table without crafting a client hello.
    pub fn lookup(&self, server_name: Option<&str>) -> Option<Arc<CertifiedKey>> {
        let name = server_name.map(|n| n.to_ascii_lowercase());
        if let Some(name) = name.as_deref() {
            if let Some(found) = self.map.load().get(name) {
                return Some(found.clone());
            }
        }
        self.fallback.load().as_ref().map(Arc::clone)
    }

    /// Replace the whole SNI table (ingress apply path). The count of
    /// live entries afterwards is returned for logging.
    fn store_map(&self, next: BTreeMap<String, Arc<CertifiedKey>>) -> usize {
        let count = next.len();
        self.map.rcu(|_| Arc::new(next.clone()));
        count
    }

    /// Install (or replace) the static default used on SNI misses.
    fn store_fallback(&self, key: Arc<CertifiedKey>) {
        self.fallback.store(Some(key));
    }

    /// Current number of SNI entries (tests and diagnostics).
    pub fn hosts(&self) -> usize {
        self.map.load().len()
    }
}

impl Default for DynamicCertResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl ResolvesServerCert for DynamicCertResolver {
    fn resolve(&self, client_hello: ClientHello) -> Option<Arc<CertifiedKey>> {
        self.lookup(client_hello.server_name())
    }
}

/// Parse one PEM cert chain + key into rustls session material (pure).
fn certified_key(cert_pem: &str, key_pem: &str) -> Result<CertifiedKey, String> {
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("unparsable certificate PEM: {e}"))?;
    if certs.is_empty() {
        return Err("certificate chain is empty".to_string());
    }
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .map_err(|e| format!("unparsable key PEM: {e}"))?
        .ok_or_else(|| "no private key in PEM input".to_string())?;
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| format!("unsupported private key: {e}"))?;
    Ok(CertifiedKey::new(certs, signing_key))
}

/// Install the static pair as the resolver's SNI-miss fallback (boot
/// path; see [`crate::tls::boot`]).
pub(super) fn seed_fallback(resolver: &DynamicCertResolver, pair: &TlsPair) -> Result<(), String> {
    let key = certified_key(&pair.cert_pem, &pair.key_pem)?;
    resolver.store_fallback(Arc::new(key));
    Ok(())
}

/// Publish rendered ingress secrets into the resolver: build session
/// material per host, skip (and warn about) broken entries so one bad
/// Secret cannot take down the rest of the table, then swap the whole
/// map in one `rcu`. Returns the number of live entries published.
///
/// Reads the PEMs synchronously: the ingress apply path is a debounce
/// task off the request path, and PEM parsing is tiny next to the
/// render it follows.
pub fn update_from_tls_pairs(resolver: &DynamicCertResolver, pairs: &BTreeMap<String, TlsPair>) -> usize {
    let mut next: BTreeMap<String, Arc<CertifiedKey>> = BTreeMap::new();
    for (host, pair) in pairs {
        match certified_key(&pair.cert_pem, &pair.key_pem) {
            Ok(key) => {
                next.insert(host.to_ascii_lowercase(), Arc::new(key));
            }
            Err(e) => {
                tracing::warn!(host = %host, error = %e, "ingress TLS secret unusable; host skipped");
            }
        }
    }
    resolver.store_map(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEAF_PEM: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/server.crt"
    ));
    const LEAF_KEY: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/server.key"
    ));
    /// Renewed identity for the same host: the rotation-test's "after".
    const ROTATED_PEM: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/server-rotated.crt"
    ));
    const ROTATED_KEY: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/server-rotated.key"
    ));
    /// Not a certificate at all: deterministic junk to prove bad entries
    /// are skipped rather than fatal.
    const JUNK_PEM: &str = "not a pem block at all\n";

    fn pair(cert: &str, key: &str) -> TlsPair {
        TlsPair {
            cert_pem: cert.to_string(),
            key_pem: key.to_string(),
        }
    }

    fn single(host: &str, cert: &str, key: &str) -> BTreeMap<String, TlsPair> {
        BTreeMap::from([(host.to_string(), pair(cert, key))])
    }

    fn leaf_der(resolved: &Arc<CertifiedKey>) -> &[u8] {
        resolved.cert[0].as_ref()
    }

    #[test]
    fn update_returns_the_number_of_live_entries() {
        let resolver = DynamicCertResolver::new();
        let n = update_from_tls_pairs(&resolver, &single("echo.example.com", LEAF_PEM, LEAF_KEY));
        assert_eq!(n, 1);
        assert_eq!(resolver.hosts(), 1);
    }

    #[test]
    fn sni_hit_miss_and_fallback() {
        let resolver = DynamicCertResolver::new();
        assert!(resolver.lookup(Some("echo.example.com")).is_none());
        // No fallback either: an SNI miss must fail the handshake (None).
        assert!(resolver.lookup(Some("other.example.com")).is_none());

        update_from_tls_pairs(&resolver, &single("echo.example.com", LEAF_PEM, LEAF_KEY));
        let hit = resolver.lookup(Some("echo.example.com")).unwrap();
        assert_eq!(leaf_der(&hit), first_fixture_cert_der());

        // Still no fallback: a miss stays a miss.
        assert!(resolver.lookup(Some("other.example.com")).is_none());

        // Install the static pair as fallback; now misses resolve to it.
        seed_fallback(&resolver, &pair(LEAF_PEM, LEAF_KEY)).unwrap();
        let miss = resolver.lookup(Some("other.example.com")).unwrap();
        assert_eq!(leaf_der(&miss), first_fixture_cert_der());

        // A hello with no SNI at all also lands on the fallback.
        assert!(resolver.lookup(None).is_some());
    }

    #[test]
    fn sni_names_are_matched_case_insensitively() {
        let resolver = DynamicCertResolver::new();
        update_from_tls_pairs(&resolver, &single("Echo.Example.COM", LEAF_PEM, LEAF_KEY));
        // The stored key is normalized...
        assert!(resolver.map.load().contains_key("echo.example.com"));
        // ...and every case variant of the hello hits it.
        for probe in ["echo.example.com", "ECHO.EXAMPLE.COM", "Echo.example.com"] {
            assert!(
                resolver.lookup(Some(probe)).is_some(),
                "case variant {probe} missed"
            );
        }
    }

    /// Rotation semantics: a swap only affects handshakes that start
    /// afterwards. A selection an in-flight connection already holds (the
    /// `Arc` rustls clones into its session) keeps resolving to exactly
    /// the old material.
    #[test]
    fn in_flight_selection_survives_a_table_swap() {
        let resolver = DynamicCertResolver::new();
        update_from_tls_pairs(&resolver, &single("echo.example.com", LEAF_PEM, LEAF_KEY));
        let before = resolver.lookup(Some("echo.example.com")).unwrap();

        // Renewal: swap in the rotated identity for the same host, the
        // way a re-rendered ingress secret would.
        let renewed = update_from_tls_pairs(
            &resolver,
            &single("echo.example.com", ROTATED_PEM, ROTATED_KEY),
        );
        assert_eq!(renewed, 1);
        let after = resolver.lookup(Some("echo.example.com")).unwrap();

        // New lookups see the new table entry (a different Arc).
        assert!(!Arc::ptr_eq(&before, &after));
        // The in-flight connection's clone is untouched and still picks
        // exactly the old leaf.
        assert_eq!(leaf_der(&before), first_fixture_cert_der());
        assert_ne!(leaf_der(&before), leaf_der(&after));
    }

    #[test]
    fn one_bad_secret_does_not_sink_the_table() {
        let resolver = DynamicCertResolver::new();
        let pairs = BTreeMap::from([
            ("good.example.com".to_string(), pair(LEAF_PEM, LEAF_KEY)),
            ("junk.example.com".to_string(), pair(JUNK_PEM, LEAF_KEY)),
            (
                "empty.example.com".to_string(),
                pair("", LEAF_KEY),
            ),
        ]);
        let published = update_from_tls_pairs(&resolver, &pairs);
        assert_eq!(published, 1, "only the good entry survives");
        assert!(resolver.lookup(Some("good.example.com")).is_some());
        assert!(resolver.lookup(Some("junk.example.com")).is_none());
        assert!(resolver.lookup(Some("empty.example.com")).is_none());
    }

    /// DER of the fixture leaf's first certificate, for identity checks.
    fn first_fixture_cert_der() -> Vec<u8> {
        let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut LEAF_PEM.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        certs[0].as_ref().to_vec()
    }
}
