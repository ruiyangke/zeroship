//! The consistent-hash ring positions, defined ONCE so control and the gateway
//! cannot order the same fleet differently.
//!
//! WHY THIS IS IN CORE RATHER THAN THE GATEWAY. Two parties walk this ring for
//! two different reasons. Control walks it to COMPUTE an app's eligible worker
//! set. The gateway walks it to ROUTE. If each carried its own ordering the two
//! would disagree, and the disagreement would not look like a security failure -
//! it would look like a routing bug, because the gateway would leave the
//! eligible set on almost every dispatch and control would refuse a caller that
//! was doing exactly what the gateway told it to do. One function, consumed by
//! both, is what makes "the same ring" a fact rather than a convention.
//!
//! THE WORKER SIDE IS THE CONTROL-MINTED RING KEY, NOT THE URL. The gateway used
//! to derive vnode positions by hashing the worker's configured URL
//! (`format!("{url}-vnode-{i}")`), which made the ring an artifact of operator
//! configuration rather than of platform state. Two consequences followed, and
//! both are why this moved: control could not compute the same order at all,
//! because it does not hold the gateway's `--worker-urls`; and a registrant able
//! to influence its own address could GRIND that address until it landed beside
//! a target app. Control mints `ring_key` from its own CSPRNG and freezes it for
//! the row's life, so neither is reachable.
//!
//! THE APP SIDE WAS ALREADY SHARED and is unchanged: `app_derivation::ring_key`
//! returns the app id's embedded bits. That function's own comment explains why
//! it exists rather than hashing `app_id.as_bytes()`, and nothing here weakens
//! it.

/// Virtual nodes per worker. More vnodes spread an app's placement more evenly
/// across the fleet; the value is a smoothness knob, not a wire constant, but it
/// MUST be identical in every party that builds the ring or the orders diverge.
/// It lives here for exactly that reason.
pub const VNODES_PER_WORKER: usize = 150;

#[inline]
fn hash_bytes(data: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(data)
}

/// Where one of a worker's virtual nodes sits on the ring.
///
/// The input is the worker's control-minted `ring_key` and the vnode ordinal.
/// Callers build their own index (a `BTreeMap` in the gateway, a sorted vector
/// in control) but every position in both comes from here.
#[must_use]
pub fn vnode_position(ring_key: &[u8], vnode: usize) -> u64 {
    // The ordinal is appended in a fixed byte encoding rather than through
    // `format!`, so the position cannot shift with a Display impl or a locale.
    let mut buf = Vec::with_capacity(ring_key.len() + 9);
    buf.extend_from_slice(ring_key);
    buf.push(b'-');
    buf.extend_from_slice(&(vnode as u64).to_be_bytes());
    hash_bytes(&buf)
}

/// Where an app starts its walk around the ring.
///
/// The argument is `app_derivation::ring_key(app_id)`'s output, not the app id's
/// printed form. Taking bytes here rather than an `AppId` keeps this module free
/// of the id types and makes the caller name the derivation explicitly.
#[must_use]
pub fn app_position(app_ring_key: &[u8]) -> u64 {
    hash_bytes(app_ring_key)
}

/// One worker as the ring sees it: who it is, where it sits, how to reach it.
///
/// This is the roster control publishes and the gateway builds its ring from.
/// `url` is derived by control from the enrolment connection and frozen by a
/// trigger, so it is not a value the registrant chose.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkerRingEntry {
    /// The `wkr_…` typed id of the enrolled instance.
    pub instance_id: String,
    /// Control's minted ring key. Opaque bytes; only its position matters.
    #[serde(with = "crate::worker_ring::ring_key_bytes")]
    pub ring_key: Vec<u8>,
    /// `http://host:port`, built by control from the derived advertise address.
    pub url: String,
}

/// Base64 for the ring key on the wire, because a JSON array of numbers is both
/// larger and easy to mistake for something with arithmetic meaning.
pub mod ring_key_bytes {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    use serde::{Deserialize as _, Deserializer, Serializer};

    /// # Errors
    /// Propagates the serializer's own failure.
    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    /// # Errors
    /// Fails when the field is not valid base64.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        STANDARD
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

/// The ring walk: roster indices in the order this app reaches them, each worker
/// appearing at most once.
///
/// Both parties get their order from here. Control keeps the first few as the
/// eligible set; the gateway walks the whole thing and takes the first entry
/// that is both eligible and under its load cap.
///
/// An empty roster yields an empty walk. That is not "route anywhere" and no
/// caller may read it as such: with no workers there is no eligible set, and a
/// dispatch has to fail rather than fall back to a fleet that is not there.
#[must_use]
pub fn ring_walk(app_ring_key: &[u8], workers: &[WorkerRingEntry]) -> Vec<usize> {
    if workers.is_empty() {
        return Vec::new();
    }
    let mut positions: Vec<(u64, usize)> = Vec::with_capacity(workers.len() * VNODES_PER_WORKER);
    for (idx, worker) in workers.iter().enumerate() {
        for vnode in 0..VNODES_PER_WORKER {
            positions.push((vnode_position(&worker.ring_key, vnode), idx));
        }
    }
    positions.sort_unstable();

    let start = app_position(app_ring_key);
    let split = positions.partition_point(|(pos, _)| *pos < start);

    let mut seen = vec![false; workers.len()];
    let mut order = Vec::with_capacity(workers.len());
    for (_, idx) in positions[split..].iter().chain(positions[..split].iter()) {
        if !seen[*idx] {
            seen[*idx] = true;
            order.push(*idx);
            if order.len() == workers.len() {
                break;
            }
        }
    }
    order
}

/// The app's eligible worker set: the CHWBL primary plus `successors` ring
/// successors, as instance ids.
///
/// `successors` bounds how far a dispatch may spill, so it is a capacity
/// parameter that is ALSO a security parameter: it is exactly the width of the
/// set an attacker would have to land inside. It is a caller-supplied config
/// value rather than a constant here for that reason.
///
/// The set is returned in ring order, and callers must preserve it: the gateway
/// prefers earlier entries, so re-sorting this would silently change placement.
#[must_use]
pub fn eligible_set(
    app_ring_key: &[u8],
    workers: &[WorkerRingEntry],
    successors: usize,
) -> Vec<String> {
    ring_walk(app_ring_key, workers)
        .into_iter()
        .take(successors.saturating_add(1))
        .map(|idx| workers[idx].instance_id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster(n: usize) -> Vec<WorkerRingEntry> {
        (0..n)
            .map(|i| WorkerRingEntry {
                instance_id: format!("wkr_{i:022}"),
                // Distinct keys, deliberately NOT derived from the url, so a
                // test that accidentally depended on url ordering would fail.
                ring_key: vec![u8::try_from(i).expect("small roster"); 32],
                url: format!("http://10.0.0.{i}:8080"),
            })
            .collect()
    }

    /// The whole point of the module: one app, one roster, one order, no matter
    /// who asks. If control and the gateway ever call this separately they get
    /// the same answer.
    #[test]
    fn the_walk_is_deterministic_for_the_same_inputs() {
        let workers = roster(5);
        let app = [7u8; 16];
        assert_eq!(ring_walk(&app, &workers), ring_walk(&app, &workers));
    }

    /// Every worker appears exactly once: the walk is a permutation, so a
    /// saturated prefix can always fall through to the rest of the ring.
    #[test]
    fn the_walk_is_a_permutation_of_the_roster() {
        let workers = roster(6);
        let mut order = ring_walk(&[3u8; 16], &workers);
        assert_eq!(order.len(), workers.len());
        order.sort_unstable();
        order.dedup();
        assert_eq!(order.len(), workers.len(), "no worker appears twice");
    }

    /// Different apps do not all pile onto one worker. Without this the ring
    /// would be "consistent" and useless.
    #[test]
    fn different_apps_do_not_all_start_at_the_same_worker() {
        let workers = roster(5);
        let firsts: std::collections::HashSet<usize> = (0u8..40)
            .map(|i| ring_walk(&[i; 16], &workers)[0])
            .collect();
        assert!(
            firsts.len() > 1,
            "every app started at the same worker, so the ring is not spreading"
        );
    }

    /// The eligible set is the primary plus exactly `successors` more, in ring
    /// order, and it is a PREFIX of the walk rather than an arbitrary subset.
    #[test]
    fn the_eligible_set_is_the_ring_ordered_prefix() {
        let workers = roster(5);
        let app = [11u8; 16];
        let walk = ring_walk(&app, &workers);
        let set = eligible_set(&app, &workers, 2);
        assert_eq!(set.len(), 3, "primary plus two successors");
        let expected: Vec<String> = walk
            .iter()
            .take(3)
            .map(|i| workers[*i].instance_id.clone())
            .collect();
        assert_eq!(set, expected);
    }

    /// A successor count at or beyond the fleet size yields the whole fleet and
    /// never panics or repeats - the saturating add is what keeps `usize::MAX`
    /// from wrapping to zero and silently producing an EMPTY set, which would
    /// be a fence that denies everything.
    #[test]
    fn an_oversized_successor_count_yields_the_whole_fleet_once() {
        let workers = roster(4);
        let set = eligible_set(&[5u8; 16], &workers, usize::MAX);
        assert_eq!(set.len(), 4);
        let unique: std::collections::HashSet<&String> = set.iter().collect();
        assert_eq!(unique.len(), 4);
    }

    /// An empty roster is an empty set, NOT an unrestricted one. A caller that
    /// read this as "no restriction" would turn an empty fleet into a bypass.
    #[test]
    fn an_empty_roster_yields_an_empty_set() {
        assert!(ring_walk(&[1u8; 16], &[]).is_empty());
        assert!(eligible_set(&[1u8; 16], &[], 3).is_empty());
    }

    /// The ring key, not the url, decides position. Changing only the url must
    /// leave the order alone - otherwise the gateway's configuration would still
    /// be steering placement and control could not reproduce it.
    #[test]
    fn the_url_does_not_influence_the_order() {
        let workers = roster(5);
        let mut relabelled = workers.clone();
        for (i, w) in relabelled.iter_mut().enumerate() {
            w.url = format!("http://192.168.5.{}:9999", 20 - i);
        }
        let app = [9u8; 16];
        assert_eq!(ring_walk(&app, &workers), ring_walk(&app, &relabelled));
    }
}
