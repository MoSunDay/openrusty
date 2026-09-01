//! Render TLS material: map host -> (cert PEM, key PEM) from the Ingress
//! TLS sections and the Secret snapshot.
//!
//! The host keys come from the *Ingress* (`spec.tls[].hosts`), not from
//! the secrets - a Secret alone does not know which hosts it serves. That
//! is why this function takes both snapshots. Hosts are normalized to
//! lowercase (the gateway compares host names case-insensitively).

use std::collections::BTreeMap;

use base64::Engine as _;

use crate::model::{Ingress, Secret, TLS_TYPE};
use crate::render::{normalize_host, RenderError};
use crate::snapshot::Snapshot;

/// Decoded certificate + key PEM pair for one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsPair {
    pub cert_pem: String,
    pub key_pem: String,
}

/// Build the host -> TLS material map for class-matching Ingresses.
///
/// - only `kubernetes.io/tls` secrets carry TLS material; other secrets in
///   the snapshot are simply never looked at (skipped, not errors);
/// - a referenced secret that is missing or has the wrong type is a hard
///   error: rendering must not silently drop HTTPS for a host;
/// - a TLS section with no hosts maps nothing (there is no host key for
///   it);
/// - two sections claiming the same host with different secrets is a
///   conflict (hard error), consistent with the route merge policy.
pub fn render_tls(
    ingresses: &Snapshot<Ingress>,
    secrets: &Snapshot<Secret>,
    ingress_class: &str,
) -> Result<BTreeMap<String, TlsPair>, RenderError> {
    let mut out: BTreeMap<String, TlsPair> = BTreeMap::new();
    // host -> description of the TLS section that first claimed it
    let mut claims: BTreeMap<String, String> = BTreeMap::new();
    for ((ns, name), ing) in ingresses.iter() {
        if ing.spec.ingress_class_name.as_deref() != Some(ingress_class) {
            continue;
        }
        let owner = format!("ingress {ns}/{name}");
        for tls in &ing.spec.tls {
            let Some(secret_name) = &tls.secret_name else {
                continue;
            };
            let secret =
                secrets
                    .get(ns, secret_name)
                    .ok_or_else(|| RenderError::SecretMissing {
                        owner: owner.clone(),
                        ns: ns.to_string(),
                        name: secret_name.clone(),
                    })?;
            let pair = decode_tls_secret(&owner, secret)?;
            for host in &tls.hosts {
                let key = normalize_host(host);
                if key.is_empty() {
                    continue;
                }
                if let Some(existing) = out.get(&key) {
                    if *existing != pair {
                        return Err(RenderError::TlsHostConflict {
                            host: key.clone(),
                            a: claims.get(&key).cloned().unwrap_or_default(),
                            b: format!("{owner} (secret {ns}/{secret_name})"),
                        });
                    }
                } else {
                    claims.insert(key.clone(), format!("{owner} (secret {ns}/{secret_name})"));
                }
                out.insert(key, pair.clone());
            }
        }
    }
    Ok(out)
}

/// Decode one `kubernetes.io/tls` secret into PEM strings (pure).
fn decode_tls_secret(owner: &str, secret: &Secret) -> Result<TlsPair, RenderError> {
    let (ns, name) = (&secret.metadata.namespace, &secret.metadata.name);
    let fail = |field: &str, reason: String| RenderError::SecretPem {
        owner: owner.to_string(),
        ns: ns.clone(),
        name: name.clone(),
        field: field.to_string(),
        reason,
    };
    // Referencing a non-TLS secret is a configuration error on the
    // Ingress; unrelated non-TLS secrets in the snapshot never get here.
    if secret.type_.as_deref() != Some(TLS_TYPE) {
        return Err(RenderError::SecretNotTls {
            owner: owner.to_string(),
            ns: ns.clone(),
            name: name.clone(),
            ty: secret.type_.clone().unwrap_or_default(),
        });
    }
    let cert = decode_field(secret, "tls.crt")
        .map_err(|reason| fail("tls.crt", reason))?
        .ok_or_else(|| fail("tls.crt", "missing data key".to_string()))?;
    let key = decode_field(secret, "tls.key")
        .map_err(|reason| fail("tls.key", reason))?
        .ok_or_else(|| fail("tls.key", "missing data key".to_string()))?;
    for (field, pem) in [("tls.crt", &cert), ("tls.key", &key)] {
        if pem.trim().is_empty() {
            return Err(fail(field, "PEM is empty".to_string()));
        }
    }
    Ok(TlsPair {
        cert_pem: cert,
        key_pem: key,
    })
}

/// Base64-decode one data key of a secret; `Ok(None)` when the key is
/// absent (caller decides whether that is fatal), `Err(reason)` on decode
/// or UTF-8 failure.
fn decode_field(secret: &Secret, field: &str) -> std::result::Result<Option<String>, String> {
    let Some(value) = secret.data.get(field) else {
        return Ok(None);
    };
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value.as_bytes())
        .map_err(|e| format!("bad base64: {e}"))?;
    let pem = String::from_utf8(decoded).map_err(|_| "decoded bytes are not UTF-8".to_string())?;
    Ok(Some(pem))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::parse_line;
    use crate::snapshot::replace_from_list;

    fn parse<T: serde::de::DeserializeOwned>(json: &str) -> T {
        parse_line::<T>(&format!(r#"{{"type":"ADDED","object":{json}}}"#))
            .unwrap()
            .object
    }

    const CERT_B64: &str =
        "LS0tLS1CRUdJTiBDRVJUSUZJQ0FURS0tLS0tCmR1bW15LWNlcnQKLS0tLS1FTkQgQ0VSVElGSUNBVEUtLS0tLQo=";
    const KEY_B64: &str =
        "LS0tLS1CRUdJTiBQUklWQVRFIEtFWS0tLS0tCmR1bW15LWtleQotLS0tLUVORCBQUklWQVRFIEtFWS0tLS0tCg==";

    fn tls_ingress(ns: &str, name: &str, class: &str, host: &str, secret: &str) -> Ingress {
        parse(&format!(
            r#"{{"metadata":{{"namespace":"{ns}","name":"{name}"}},"spec":{{"ingressClassName":"{class}","tls":[{{"hosts":["{host}"],"secretName":"{secret}"}}]}}}}"#
        ))
    }

    fn tls_secret(ns: &str, name: &str, ty: &str) -> Secret {
        parse(&format!(
            r#"{{"metadata":{{"namespace":"{ns}","name":"{name}"}},"type":"{ty}","data":{{"tls.crt":"{CERT_B64}","tls.key":"{KEY_B64}"}}}}"#
        ))
    }

    #[test]
    fn decodes_tls_secret_into_lowercase_hosts() {
        let ings = replace_from_list(
            vec![tls_ingress(
                "web",
                "a",
                "openrusty",
                "App.Example.COM",
                "app-tls",
            )],
            "1",
        );
        let secrets = replace_from_list(
            vec![
                tls_secret("web", "app-tls", "kubernetes.io/tls"),
                // non-TLS secrets ride along in the snapshot and are skipped
                parse::<Secret>(
                    r#"{"metadata":{"namespace":"web","name":"regcred"},"type":"kubernetes.io/dockerconfigjson","data":{".dockerconfigjson":"e30="}}"#,
                ),
            ],
            "1",
        );
        let map = render_tls(&ings, &secrets, "openrusty").unwrap();
        assert_eq!(map.len(), 1);
        let pair = map.get("app.example.com").unwrap();
        assert!(pair.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(pair.key_pem.contains("dummy-key"));
    }

    #[test]
    fn missing_secret_is_an_error() {
        let ings = replace_from_list(
            vec![tls_ingress(
                "web",
                "a",
                "openrusty",
                "app.example.com",
                "ghost",
            )],
            "1",
        );
        let secrets: Snapshot<Secret> = replace_from_list(vec![], "1");
        let err = render_tls(&ings, &secrets, "openrusty").unwrap_err();
        assert!(err.to_string().contains("secret web/ghost"), "{err}");
    }

    #[test]
    fn wrong_type_secret_is_an_error_when_referenced() {
        let ings = replace_from_list(
            vec![tls_ingress(
                "web",
                "a",
                "openrusty",
                "app.example.com",
                "opaque",
            )],
            "1",
        );
        let secrets = replace_from_list(vec![tls_secret("web", "opaque", "Opaque")], "1");
        let err = render_tls(&ings, &secrets, "openrusty").unwrap_err();
        assert!(
            err.to_string().contains("expected kubernetes.io/tls"),
            "{err}"
        );
    }

    #[test]
    fn bad_base64_names_the_field() {
        let ings = replace_from_list(
            vec![tls_ingress(
                "web",
                "a",
                "openrusty",
                "app.example.com",
                "broken",
            )],
            "1",
        );
        let secrets = replace_from_list(
            vec![parse::<Secret>(
                r#"{"metadata":{"namespace":"web","name":"broken"},"type":"kubernetes.io/tls","data":{"tls.crt":"!!!not-base64!!!","tls.key":"aw=="}}"#,
            )],
            "1",
        );
        let err = render_tls(&ings, &secrets, "openrusty").unwrap_err();
        assert!(
            err.to_string().contains("tls.crt") && err.to_string().contains("base64"),
            "{err}"
        );
    }

    #[test]
    fn empty_pem_is_rejected() {
        let ings = replace_from_list(
            vec![tls_ingress(
                "web",
                "a",
                "openrusty",
                "app.example.com",
                "blank",
            )],
            "1",
        );
        let secrets = replace_from_list(
            vec![parse::<Secret>(
                r#"{"metadata":{"namespace":"web","name":"blank"},"type":"kubernetes.io/tls","data":{"tls.crt":"CiA=","tls.key":"aw=="}}"#,
            )],
            "1",
        );
        let err = render_tls(&ings, &secrets, "openrusty").unwrap_err();
        assert!(err.to_string().contains("PEM is empty"), "{err}");
    }

    #[test]
    fn same_host_from_two_secrets_conflicts() {
        let ings = replace_from_list(
            vec![
                tls_ingress("web", "a", "openrusty", "app.example.com", "tls-a"),
                tls_ingress("other", "b", "openrusty", "app.example.com", "tls-b"),
            ],
            "1",
        );
        let secrets = replace_from_list(
            vec![
                tls_secret("web", "tls-a", "kubernetes.io/tls"),
                // different material for the same host is a real conflict
                parse::<Secret>(
                    r#"{"metadata":{"namespace":"other","name":"tls-b"},"type":"kubernetes.io/tls","data":{"tls.crt":"YQ==","tls.key":"Yg=="}}"#,
                ),
            ],
            "1",
        );
        let err = render_tls(&ings, &secrets, "openrusty").unwrap_err();
        assert!(matches!(err, RenderError::TlsHostConflict { .. }), "{err}");
    }

    #[test]
    fn same_host_with_identical_material_deduplicates() {
        let ings = replace_from_list(
            vec![
                tls_ingress("web", "a", "openrusty", "app.example.com", "app-tls"),
                tls_ingress("web", "b", "openrusty", "APP.example.COM", "app-tls"),
            ],
            "1",
        );
        let secrets =
            replace_from_list(vec![tls_secret("web", "app-tls", "kubernetes.io/tls")], "1");
        let map = render_tls(&ings, &secrets, "openrusty").unwrap();
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn other_class_ingresses_do_not_contribute() {
        let ings = replace_from_list(
            vec![tls_ingress(
                "web",
                "a",
                "nginx",
                "app.example.com",
                "app-tls",
            )],
            "1",
        );
        let secrets =
            replace_from_list(vec![tls_secret("web", "app-tls", "kubernetes.io/tls")], "1");
        let map = render_tls(&ings, &secrets, "openrusty").unwrap();
        assert!(map.is_empty());
    }
}
