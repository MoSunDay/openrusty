//! Snapshot state-machine tests driven by the watch fixture stream:
//! apply_event sequences, replace_from_list, and Snapshot immutability.

use openrusty_k8s::model::{EventType, Ingress, K8sList, WatchEvent};
use openrusty_k8s::{apply_event, replace_from_list, ResourceMeta, Snapshot};

fn ing(ns: &str, name: &str, rv: &str) -> Ingress {
    serde_json::from_str(&format!(
        r#"{{"metadata":{{"name":"{name}","namespace":"{ns}","resourceVersion":"{rv}"}},"spec":{{"rules":[]}}}}"#
    ))
    .unwrap()
}

fn event(t: EventType, obj: Ingress) -> WatchEvent<Ingress> {
    WatchEvent {
        event_type: t,
        object: obj,
    }
}

fn add(snap: &Snapshot<Ingress>, ns: &str, name: &str, rv: &str) -> Snapshot<Ingress> {
    apply_event(snap, event(EventType::Added, ing(ns, name, rv)))
}

fn modified(snap: &Snapshot<Ingress>, ns: &str, name: &str, rv: &str) -> Snapshot<Ingress> {
    apply_event(snap, event(EventType::Modified, ing(ns, name, rv)))
}

fn deleted(snap: &Snapshot<Ingress>, ns: &str, name: &str, rv: &str) -> Snapshot<Ingress> {
    apply_event(snap, event(EventType::Deleted, ing(ns, name, rv)))
}

#[test]
fn add_modify_delete_sequence_tracks_rv() {
    let s0 = Snapshot::new();
    let s1 = add(&s0, "web", "app", "101");
    let s2 = add(&s1, "web", "added", "102");
    let s3 = modified(&s2, "web", "app", "103");
    let s4 = deleted(&s3, "web", "added", "111");

    assert_eq!(s0.resource_version(), "");
    assert_eq!(s1.resource_version(), "101");
    assert_eq!(s2.resource_version(), "102");
    assert_eq!(s3.resource_version(), "103");
    assert_eq!(s4.resource_version(), "111");

    assert_eq!(s1.len(), 1);
    assert_eq!(s2.len(), 2);
    assert_eq!(s3.len(), 2);
    assert_eq!(s4.len(), 1);
    assert!(s4.get("web", "app").is_some());
    assert!(s4.get("web", "added").is_none());
}

#[test]
fn delete_of_unknown_key_still_advances_rv() {
    let s1 = add(&Snapshot::new(), "web", "app", "101");
    let s2 = deleted(&s1, "web", "ghost", "105");
    assert_eq!(s2.resource_version(), "105");
    assert_eq!(s2.len(), 1);
}

#[test]
fn modify_replaces_previous_version_in_place() {
    let s1 = add(&Snapshot::new(), "web", "app", "101");
    let s2 = modified(&s1, "web", "app", "103");
    assert_eq!(s2.get("web", "app").unwrap().resource_version(), "103");
    assert_eq!(s2.len(), 1, "modify must not duplicate the key");
}

#[test]
fn bookmark_only_advances_rv_and_error_is_ignored() {
    let s1 = add(&Snapshot::new(), "web", "app", "101");
    let bm: Ingress = serde_json::from_str(
        r#"{"apiVersion":"networking.k8s.io/v1","kind":"Ingress","metadata":{"resourceVersion":"110"}}"#,
    )
    .unwrap();
    let s2 = apply_event(&s1, event(EventType::Bookmark, bm.clone()));
    assert_eq!(s2.resource_version(), "110");
    assert_eq!(s2.len(), 1);
    assert_eq!(
        s2.iter().collect::<Vec<_>>(),
        s1.iter().collect::<Vec<_>>(),
        "items unchanged by a bookmark"
    );
    // With no stored rv yet, a bookmark still records the fresh one.
    let s3 = apply_event(&Snapshot::new(), event(EventType::Bookmark, bm));
    assert_eq!(s3.resource_version(), "110");
    // ERROR events change nothing at all.
    let err = event(EventType::Error, ing("web", "app", "999"));
    assert_eq!(apply_event(&s2, err), s2);
}

#[test]
fn replace_from_list_clears_stale_entries() {
    // `replace_from_list` builds a fresh snapshot: anything absent from the
    // list (here "gone") simply ceases to exist afterwards.
    let list: K8sList<Ingress> = serde_json::from_str(
        r#"{"metadata":{"resourceVersion":"200"},"items":[
            {"metadata":{"name":"app","namespace":"web","resourceVersion":"199"},
             "spec":{"rules":[]}},
            {"metadata":{"name":"fresh","namespace":"other","resourceVersion":"200"},
             "spec":{"rules":[]}}
        ]}"#,
    )
    .unwrap();
    let s2 = replace_from_list(list.items, &list.metadata.resource_version);
    assert_eq!(s2.resource_version(), "200");
    assert_eq!(s2.len(), 2);
    assert!(s2.get("web", "gone").is_none(), "stale entry dropped");
    assert!(s2.get("web", "app").is_some());
    assert!(s2.get("other", "fresh").is_some());
}

#[test]
fn snapshot_is_immutable_and_sorted() {
    let s1 = add(
        &add(&Snapshot::new(), "web", "zeta", "101"),
        "web",
        "alpha",
        "102",
    );
    let before: Vec<(&str, &str)> = s1.iter().map(|(k, _)| (k.0, k.1)).collect();
    // Deriving a new snapshot never mutates the old one.
    let _s2 = deleted(&s1, "web", "zeta", "103");
    let after: Vec<(&str, &str)> = s1.iter().map(|(k, _)| (k.0, k.1)).collect();
    assert_eq!(before, after);
    assert_eq!(s1.len(), 2);
    assert_eq!(
        before,
        vec![("web", "alpha"), ("web", "zeta")],
        "iteration is ordered by (namespace, name)"
    );
}
