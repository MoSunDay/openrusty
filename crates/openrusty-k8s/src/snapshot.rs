//! The watch state machine: an immutable snapshot plus a pure event fold.
//!
//! [`Snapshot`] is a plain value: a `BTreeMap` keyed by `(namespace, name)`
//! and the resource version it is consistent at. [`apply_event`] never
//! mutates its input - it clones and returns a new snapshot - so callers
//! (the watch loop, tests) can hold a snapshot for as long as they like
//! while events keep flowing.
//!
//! # Stale-serve semantics (explicit, not a fallback)
//!
//! Snapshots are only ever *replaced*, never cleared. While the API server
//! is unreachable the watch loop simply stops producing new snapshots and
//! the caller keeps serving the last known-good one; nothing expires it.
//! An empty snapshot can only be produced by a successful list/watch that
//! really observed zero objects, never by a failed refresh.

use std::collections::BTreeMap;

use crate::model::{EventType, ResourceMeta, WatchEvent};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot<T> {
    items: BTreeMap<(String, String), T>,
    resource_version: String,
}

impl<T> Snapshot<T> {
    pub fn new() -> Self {
        Self {
            items: BTreeMap::new(),
            resource_version: String::new(),
        }
    }

    /// Resource version this snapshot is consistent at (empty before the
    /// first successful list).
    pub fn resource_version(&self) -> &str {
        &self.resource_version
    }

    pub fn get(&self, namespace: &str, name: &str) -> Option<&T> {
        self.items.get(&(namespace.to_string(), name.to_string()))
    }

    /// Deterministic iteration order: `(namespace, name)` sorted.
    pub fn iter(&self) -> impl Iterator<Item = ((&str, &str), &T)> {
        self.items
            .iter()
            .map(|((ns, name), item)| ((ns.as_str(), name.as_str()), item))
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Fold one watch event into a snapshot, returning the new snapshot.
///
/// - `ADDED`/`MODIFIED` insert or replace the object and advance the
///   resource version;
/// - `DELETED` removes it (no-op when already gone) and advances rv;
/// - `BOOKMARK` only advances rv - it carries no object change;
/// - `ERROR` leaves the snapshot untouched (the watch loop answers it by
///   reconnecting and re-listing).
pub fn apply_event<T: ResourceMeta + Clone>(
    snap: &Snapshot<T>,
    event: WatchEvent<T>,
) -> Snapshot<T> {
    let mut next = snap.clone();
    let advance = |snap: &mut Snapshot<T>, event: &WatchEvent<T>| {
        let rv = event.object.resource_version();
        if !rv.is_empty() {
            snap.resource_version = rv.to_string();
        }
    };
    match event.event_type {
        EventType::Added | EventType::Modified => {
            advance(&mut next, &event);
            let key = (
                event.object.namespace().to_string(),
                event.object.name().to_string(),
            );
            next.items.insert(key, event.object);
        }
        EventType::Deleted => {
            advance(&mut next, &event);
            let key = (
                event.object.namespace().to_string(),
                event.object.name().to_string(),
            );
            next.items.remove(&key);
        }
        EventType::Bookmark => advance(&mut next, &event),
        EventType::Error => {}
    }
    next
}

/// Full-replacement semantics of a re-list: the returned snapshot contains
/// exactly `items` at `resource_version`; anything the previous snapshot
/// held is dropped.
pub fn replace_from_list<T: ResourceMeta>(items: Vec<T>, resource_version: &str) -> Snapshot<T> {
    let mut snap = Snapshot {
        items: BTreeMap::new(),
        resource_version: resource_version.to_string(),
    };
    for item in items {
        let key = (item.namespace().to_string(), item.name().to_string());
        snap.items.insert(key, item);
    }
    snap
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{parse_line, ObjectMeta};
    use serde::Deserialize;

    fn event(line: &str) -> WatchEvent<IngressLike> {
        parse_line(line).unwrap()
    }

    // Minimal stand-in exercising the generic state machine without
    // pulling render concerns in; the shape matches the real models.
    #[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
    struct IngressLike {
        #[serde(default)]
        metadata: ObjectMeta,
    }

    impl ResourceMeta for IngressLike {
        fn namespace(&self) -> &str {
            &self.metadata.namespace
        }
        fn name(&self) -> &str {
            &self.metadata.name
        }
        fn resource_version(&self) -> &str {
            &self.metadata.resource_version
        }
    }

    fn snap_at(rv: &str, items: &[(&str, &str)]) -> Snapshot<IngressLike> {
        let items = items
            .iter()
            .map(|(ns, name)| IngressLike {
                metadata: ObjectMeta {
                    namespace: (*ns).to_string(),
                    name: (*name).to_string(),
                    resource_version: rv.to_string(),
                    ..ObjectMeta::default()
                },
            })
            .collect();
        replace_from_list(items, rv)
    }

    #[test]
    fn add_modify_delete_bookmark_sequence() {
        let s0 = snap_at("10", &[]);
        let s1 = apply_event(
            &s0,
            event(
                r#"{"type":"ADDED","object":{"metadata":{"namespace":"web","name":"a","resourceVersion":"11"}}}"#,
            ),
        );
        assert_eq!(s1.len(), 1);
        assert_eq!(s1.resource_version(), "11");
        assert!(s1.get("web", "a").is_some());
        // the input snapshot is never mutated
        assert_eq!(s0.len(), 0);

        let s2 = apply_event(
            &s1,
            event(
                r#"{"type":"MODIFIED","object":{"metadata":{"namespace":"web","name":"a","resourceVersion":"12"}}}"#,
            ),
        );
        assert_eq!(s2.resource_version(), "12");
        assert_eq!(s2.len(), 1, "modify replaces, not duplicates");

        let s3 = apply_event(
            &s2,
            event(
                r#"{"type":"DELETED","object":{"metadata":{"namespace":"web","name":"a","resourceVersion":"13"}}}"#,
            ),
        );
        assert_eq!(s3.len(), 0);
        assert_eq!(s3.resource_version(), "13");

        let s4 = apply_event(
            &s3,
            event(r#"{"type":"BOOKMARK","object":{"metadata":{"resourceVersion":"14"}}}"#),
        );
        assert_eq!(s4.resource_version(), "14");
        assert_eq!(s4.len(), 0, "bookmark carries no object change");
    }

    #[test]
    fn delete_of_unknown_object_advances_rv_only() {
        let s0 = snap_at("10", &[("web", "a")]);
        let s1 = apply_event(
            &s0,
            event(
                r#"{"type":"DELETED","object":{"metadata":{"namespace":"web","name":"ghost","resourceVersion":"15"}}}"#,
            ),
        );
        assert_eq!(s1.len(), 1);
        assert_eq!(s1.resource_version(), "15");
    }

    #[test]
    fn error_event_leaves_state_untouched() {
        let s0 = snap_at("10", &[("web", "a")]);
        let s1 = apply_event(&s0, event(r#"{"type":"ERROR","object":{"kind":"Status"}}"#));
        assert_eq!(s1, s0);
    }

    #[test]
    fn replace_from_list_drops_stale_items() {
        let s0 = snap_at("20", &[("web", "a"), ("web", "b"), ("other", "c")]);
        assert_eq!(s0.len(), 3);
        let items = vec![IngressLike {
            metadata: ObjectMeta {
                namespace: "web".to_string(),
                name: "a".to_string(),
                resource_version: "21".to_string(),
                ..ObjectMeta::default()
            },
        }];
        let s1 = replace_from_list(items, "21");
        assert_eq!(s1.len(), 1);
        assert_eq!(s1.resource_version(), "21");
        assert!(s1.get("web", "b").is_none());
    }

    #[test]
    fn iteration_is_deterministically_sorted() {
        let s = snap_at("1", &[("web", "b"), ("api", "a"), ("web", "a")]);
        let order: Vec<_> = s
            .iter()
            .map(|((ns, name), _)| format!("{ns}/{name}"))
            .collect();
        assert_eq!(order, vec!["api/a", "web/a", "web/b"]);
    }
}
