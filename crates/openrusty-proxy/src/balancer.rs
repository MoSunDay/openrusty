//! Pure peer-selection algorithms.
//!
//! These functions hold no state of their own: the caller owns any mutable
//! bookkeeping (e.g. the SWRR effective-weight vector) so that balancing
//! state can live wherever per-upstream state naturally lives.

/// FNV-1a 64-bit: offset basis and prime per the Fowler-Noll-Vo spec.
/// Hand-rolled to avoid a hashing dependency for ip_hash.
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x00000100000001b3;

/// FNV-1a 64-bit hash of `data`. Deterministic across runs and platforms
/// (unlike `std::hash`, which is randomized), which is exactly what a
/// consistent ip_hash needs.
fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// nginx smooth weighted round-robin step.
///
/// `current` holds the per-peer effective weights owned by the caller; it
/// must have the same length as `weights` and start all-zero. One call:
/// 1. adds each peer's configured weight to its effective weight,
/// 2. picks the peer with the highest effective weight,
/// 3. subtracts the total weight from the chosen peer.
///
/// Returns the chosen index. Over any `sum(weights)` consecutive picks the
/// distribution matches the weights exactly, spread as evenly as possible.
pub fn swrr_next(weights: &[u32], current: &mut [i64]) -> usize {
    assert!(!weights.is_empty(), "swrr_next requires at least one peer");
    assert_eq!(
        weights.len(),
        current.len(),
        "swrr_next: weights/current length mismatch"
    );
    let mut total: i64 = 0;
    for (eff, weight) in current.iter_mut().zip(weights) {
        *eff += i64::from(*weight);
        total += i64::from(*weight);
    }
    let mut best = 0;
    for i in 1..current.len() {
        if current[i] > current[best] {
            best = i;
        }
    }
    current[best] -= total;
    best
}

/// Deterministic peer pick for `ip_hash` balancing.
///
/// Hashes the client IP string with FNV-1a and maps it into `healthy`,
/// so the same client keeps hitting the same peer as long as the healthy
/// set is stable. Returns `None` when no healthy peer exists.
pub fn ip_hash_pick(client_ip: &str, healthy: &[usize]) -> Option<usize> {
    if healthy.is_empty() {
        return None;
    }
    let hash = fnv1a64(client_ip.as_bytes());
    Some(healthy[(hash % healthy.len() as u64) as usize])
}

/// nginx-style least_conn pick over the healthy subset.
///
/// Chooses the peer whose in-flight-to-weight ratio is smallest, compared
/// with integer cross-multiplication (`in_flight[i] * weight[b] <
/// in_flight[b] * weight[i]`) to stay float-free. Ties break to the
/// lowest index for determinism. `healthy` holds indices into the
/// parallel `in_flight`/`weights` arrays.
pub fn least_conn_pick(in_flight: &[usize], weights: &[u32], healthy: &[usize]) -> Option<usize> {
    let mut best = *healthy.first()?;
    for &i in healthy.iter().skip(1) {
        // in_flight[i]/weights[i] < in_flight[best]/weights[best], with
        // u128 cross-products so no realistic counter can overflow.
        let lhs = in_flight[i] as u128 * u128::from(weights[best]);
        let rhs = in_flight[best] as u128 * u128::from(weights[i]);
        if lhs < rhs {
            best = i;
        }
    }
    Some(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a64_known_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64(b"a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn swrr_exact_distribution_over_full_cycles() {
        let weights = [5u32, 1, 1];
        let mut current = [0i64; 3];
        let mut counts = [0u32; 3];
        // 700 picks = 100 full cycles of total weight 7.
        for _ in 0..700 {
            counts[swrr_next(&weights, &mut current)] += 1;
        }
        assert_eq!(counts, [500, 100, 100]);
        // State returns to zero after every full cycle.
        assert_eq!(current, [0, 0, 0]);
    }

    #[test]
    fn swrr_is_smooth() {
        let weights = [5u32, 1, 1];
        let mut current = [0i64; 3];
        let seq: Vec<usize> = (0..700)
            .map(|_| swrr_next(&weights, &mut current))
            .collect();
        // Classic nginx sequence for 5,1,1.
        assert_eq!(&seq[..7], &[0, 0, 1, 0, 2, 0, 0]);
        // Smoothness: peer 0 is never absent for more than 1 slot, i.e.
        // the gap between consecutive picks of peer 0 is at most 2.
        let mut last = None;
        for (pos, &pick) in seq.iter().enumerate() {
            if pick == 0 {
                if let Some(prev) = last {
                    assert!(
                        pos - prev <= 2,
                        "peer 0 gap {} > 2 at position {pos}",
                        pos - prev
                    );
                }
                last = Some(pos);
            }
        }
    }

    #[test]
    fn swrr_equal_weights_round_robin() {
        let weights = [1u32, 1, 1];
        let mut current = [0i64; 3];
        let seq: Vec<usize> = (0..6).map(|_| swrr_next(&weights, &mut current)).collect();
        assert_eq!(seq, vec![0, 1, 2, 0, 1, 2]);
    }

    #[test]
    fn ip_hash_is_stable_for_same_client() {
        let healthy = vec![0, 1, 2];
        let first = ip_hash_pick("203.0.113.9", &healthy).unwrap();
        for _ in 0..1000 {
            assert_eq!(ip_hash_pick("203.0.113.9", &healthy), Some(first));
        }
    }

    #[test]
    fn ip_hash_spreads_synthetic_clients() {
        let healthy = vec![0, 1, 2];
        let mut seen = [false; 3];
        for i in 0..1000u32 {
            let ip = format!("192.168.{}.{}", i / 250, i % 250);
            let pick = ip_hash_pick(&ip, &healthy).unwrap();
            assert!(healthy.contains(&pick));
            seen[pick] = true;
        }
        assert!(seen.iter().all(|used| *used), "all peers must be hit");
    }

    #[test]
    fn ip_hash_empty_is_none() {
        assert_eq!(ip_hash_pick("203.0.113.9", &[]), None);
    }

    #[test]
    fn least_conn_empty_is_none() {
        assert_eq!(least_conn_pick(&[0, 0, 0], &[1, 1, 1], &[]), None);
    }

    #[test]
    fn least_conn_idle_ties_break_to_lowest_index() {
        let healthy = vec![0, 1, 2];
        for _ in 0..100 {
            assert_eq!(
                least_conn_pick(&[0, 0, 0], &[1, 1, 1], &healthy),
                Some(0),
                "all-zero in-flight with equal weights must stay on index 0"
            );
        }
    }

    #[test]
    fn least_conn_avoids_busy_peers() {
        // Peer 1 carries load, peer 0 is idle: every pick must be 0.
        assert_eq!(least_conn_pick(&[0, 3, 0], &[1, 1, 1], &[0, 1, 2]), Some(0));
        // Only the busy subset: the least-loaded of {1, 2} is 2.
        assert_eq!(least_conn_pick(&[0, 3, 1], &[1, 1, 1], &[1, 2]), Some(2));
    }

    #[test]
    fn least_conn_weight_tilt() {
        // weights [1,2]: in-flight [1,2] -> ratios 1/1 vs 2/2 -> tie -> 0.
        assert_eq!(least_conn_pick(&[1, 2], &[1, 2], &[0, 1]), Some(0));
        // in-flight [2,3] -> 2/1 vs 3/2 -> peer 1 is relatively freer.
        assert_eq!(least_conn_pick(&[2, 3], &[1, 2], &[0, 1]), Some(1));
    }

    #[test]
    fn least_conn_is_deterministic() {
        let healthy = vec![0, 1, 2];
        let first = least_conn_pick(&[4, 2, 2], &[3, 1, 2], &healthy);
        for _ in 0..1000 {
            assert_eq!(least_conn_pick(&[4, 2, 2], &[3, 1, 2], &healthy), first);
        }
    }
}
