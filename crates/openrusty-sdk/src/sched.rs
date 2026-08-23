//! Pure scheduling primitives (host-unit-testable, no host imports).

/// Snapshot of one upstream peer for least-loaded selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeStat {
    /// Unhealthy nodes are never selected.
    pub healthy: bool,
    /// Currently assigned tasks (e.g. live affinity records).
    pub active: u32,
    /// Timestamp (ms) of the last scheduling onto this node. Callers pass
    /// `0` -- or [`NEVER`] -- for nodes never scheduled; both sort oldest
    /// and are therefore most preferred on ties.
    pub last_sched_ms: u64,
    /// Capacity limit on `active`; `0` = unlimited.
    pub cap: u32,
}

/// Sentinel meaning "never scheduled"; treated exactly like timestamp `0`.
pub const NEVER: u64 = u64::MAX;

/// Pick the least-loaded node index: candidates are healthy nodes under
/// capacity; the minimum by `(active, last_sched_ms)` wins; ties break to
/// the lowest index. Returns `None` when no node is eligible.
pub fn choose_least_loaded(nodes: &[NodeStat]) -> Option<usize> {
    let mut best: Option<(usize, u32, u64)> = None;
    for (idx, node) in nodes.iter().enumerate() {
        if !node.healthy {
            continue;
        }
        if node.cap != 0 && node.active >= node.cap {
            continue;
        }
        let last = if node.last_sched_ms == NEVER {
            0
        } else {
            node.last_sched_ms
        };
        match best {
            None => best = Some((idx, node.active, last)),
            Some((_, best_active, best_last)) => {
                // Strict `<` keeps the lowest index on ties.
                if (node.active, last) < (best_active, best_last) {
                    best = Some((idx, node.active, last));
                }
            }
        }
    }
    best.map(|(idx, _, _)| idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(healthy: bool, active: u32, last: u64, cap: u32) -> NodeStat {
        NodeStat {
            healthy,
            active,
            last_sched_ms: last,
            cap,
        }
    }

    #[test]
    fn lowest_active_wins() {
        let nodes = [
            node(true, 5, 0, 0),
            node(true, 2, 900, 0),
            node(true, 9, 100, 0),
        ];
        assert_eq!(choose_least_loaded(&nodes), Some(1));
    }

    #[test]
    fn ties_go_to_oldest_last_scheduled() {
        let nodes = [
            node(true, 1, 500, 0),
            node(true, 1, 100, 0),
            node(true, 1, 300, 0),
        ];
        assert_eq!(choose_least_loaded(&nodes), Some(1));
    }

    #[test]
    fn exact_ties_go_to_lowest_index() {
        let nodes = [node(true, 1, 100, 0), node(true, 1, 100, 0)];
        assert_eq!(choose_least_loaded(&nodes), Some(0));
    }

    #[test]
    fn never_scheduled_sentinel_is_most_preferred() {
        let nodes = [node(true, 0, 12345, 0), node(true, 0, NEVER, 0)];
        assert_eq!(choose_least_loaded(&nodes), Some(1));
    }

    #[test]
    fn unhealthy_nodes_are_skipped() {
        let nodes = [
            node(false, 0, 0, 0),
            node(true, 3, 0, 0),
            node(false, 1, 0, 0),
        ];
        assert_eq!(choose_least_loaded(&nodes), Some(1));
    }

    #[test]
    fn capacity_is_honored() {
        let nodes = [
            node(true, 2, 0, 2), // at cap -> ineligible
            node(true, 2, 0, 3), // under cap -> eligible
            node(true, 0, 0, 0),
        ];
        assert_eq!(choose_least_loaded(&nodes), Some(2));
    }

    #[test]
    fn none_when_all_unhealthy_or_full() {
        let nodes = [node(false, 0, 0, 0), node(true, 5, 0, 5)];
        assert_eq!(choose_least_loaded(&nodes), None);
        assert_eq!(choose_least_loaded(&[]), None);
    }
}
