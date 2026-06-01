//! Shared wrapper-token revocation: the spec §8.5 cross-node PER-APP family
//! marker.
//!
//! **`token_revocations`** is the SOLE wrapper-token revocation primitive
//! keyed on `(client_id, sub)` with `sub` stored as TEXT, so it holds the
//! wrapper's `pws_…` pairwise subject (and, on the raw-Hydra arm, the per-app
//! `pws_` derived from the global UUID). The wrapper / Bearer / DPoP arms
//! (§1.3 c-wrap / c-hydra) reject a token when a row exists for its
//! `(client_id, pws_)` with `revoked_after > token.iat`. Per-app scoping means
//! revoking a user on app A leaves their tokens on app B valid.
//!
//! There is no longer a global UUID-keyed subject denylist: every wrapper /
//! raw-Hydra access token is minted under a per-app `oac_…` client and carries
//! (or projects to) a per-app `pws_`, so the per-app family marker is the only
//! key any reader uses. The previous `wrapper_revoked_subjects` denylist was
//! write-only dead code after the per-app cutover (Batch A M2) and is removed —
//! pre-launch, no back-compat (AGENTS.md), so the table and its helpers are
//! deleted rather than left orphaned.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use compio_postgres::{Client, Error};

pub const WRAPPER_REVOCATION_RETENTION_HOURS: i32 = 24;

// ─── Per-app family marker (spec §8.5 `auth.token_revocations`) ──────────
//
// The SOLE cross-node revocation mechanism. Keyed on `(client_id, sub)`
// — one row per token family per app — so signout (which cannot enumerate
// the live `jti`s) can reject every already-minted token for that family
// on every node. `sub` is TEXT and holds the per-app `pws_…` pairwise
// subject every wrapper / raw-Hydra access token carries (or projects to);
// the per-app scoping is exactly what a global UUID-keyed denylist could
// never express for the browser wrapper path.

/// Upsert the per-app family marker for `(client_id, sub)`, stamping
/// `revoked_after = NOW()`. Any token in this family with `iat < NOW()` is
/// rejected from here on. Per-app: a marker for app A's `client_id` does
/// NOT affect app B.
pub async fn revoke_family(db: &Client, client_id: &str, sub: &str) -> Result<u64, Error> {
    db.execute(
        "INSERT INTO zeroship.token_revocations (client_id, sub, revoked_after) \
         VALUES ($1, $2, NOW()) \
         ON CONFLICT (client_id, sub) DO UPDATE SET revoked_after = EXCLUDED.revoked_after",
        &[&client_id, &sub],
    )
    .await
}

/// The family's LATEST `revoked_after`, as epoch-SECONDS, for `(client_id,
/// sub)` — `None` when no marker row exists for the family.
///
/// This is the SOLE source of truth the readers consult: the per-request
/// "is this token revoked?" decision is `revoked_after > iat`, computed
/// LOCALLY by the caller against the value this returns. Returning the
/// marker instant (rather than a precomputed bool) is what lets the gateway
/// CACHE the result across cookies in the same family with different `iat`s
/// (R1d): the bool depends on `iat`, the marker does not.
///
/// `EXTRACT(EPOCH …)` yields `double precision` (sub-second). We `ceil()` to
/// whole seconds so the cached `i64 > iat` decision is IDENTICAL to the old
/// direct `revoked_after > to_timestamp(iat)` comparison for the whole-second
/// `iat`s tokens carry. A token `iat` is a whole second; `to_timestamp(iat)`
/// is exactly that second boundary, so a marker written ANYWHERE inside the
/// SAME second as `iat` (the "revoke immediately after mint" case) is `>`
/// it — `ceil` rounds such a marker UP to `iat + 1`, preserving the `>`.
/// `floor` would collapse it to `iat` and wrongly read "not revoked".
pub async fn revoked_after_for(
    db: &Client,
    client_id: &str,
    sub: &str,
) -> Result<Option<i64>, Error> {
    let row = db
        .query_one(
            "SELECT EXTRACT(EPOCH FROM MAX(revoked_after))::double precision AS ra \
             FROM zeroship.token_revocations \
             WHERE client_id = $1 AND sub = $2",
            &[&client_id, &sub],
        )
        .await?;
    // `MAX(...)` over zero rows is SQL NULL → `ra` is NULL → `Option::None`.
    let ra: Option<f64> = row.get("ra");
    Ok(ra.map(|secs| secs.ceil() as i64))
}

/// Whether the `(client_id, sub)` family was revoked AFTER `iat` — i.e. a
/// row exists with `revoked_after > to_timestamp(iat)`. A token whose `iat`
/// predates the marker is rejected; one minted after the marker is fine.
///
/// Delegates to [`revoked_after_for`] so the marker query lives in ONE place;
/// the `> iat` comparison is the only thing this layer adds.
pub async fn is_family_revoked_since(
    db: &Client,
    client_id: &str,
    sub: &str,
    iat: i64,
) -> Result<bool, Error> {
    Ok(family_revoked_at(revoked_after_for(db, client_id, sub).await?, iat))
}

/// The local revocation decision: a family marked `revoked_after` (epoch
/// seconds) rejects a token whose `iat` predates the marker. `None` (no
/// marker) is never revoked. This is the pure function the cache hot path
/// evaluates on a hit — keep it side-effect-free so the cached marker can be
/// re-judged against any `iat`.
#[must_use]
pub fn family_revoked_at(revoked_after: Option<i64>, iat: i64) -> bool {
    revoked_after.is_some_and(|ra| ra > iat)
}

/// Sweep family markers older than the retention window. Markers only need
/// to outlive the longest-lived token whose `iat` could predate them; the
/// 24 h retention is comfortably beyond the 10-min wrapper / 1 h raw-Hydra
/// TTLs.
pub async fn sweep_expired_families(db: &Client) -> Result<u64, Error> {
    db.execute(
        "DELETE FROM zeroship.token_revocations \
         WHERE revoked_after < NOW() - make_interval(hours => $1)",
        &[&WRAPPER_REVOCATION_RETENTION_HOURS],
    )
    .await
}

// ─── R1d: short-TTL read-through cache for the per-request marker read ────
//
// The cookie hot path (and the Bearer / DPoP-introspect arms) verifies the
// token LOCALLY, then runs ONE per-request DB read — the family-marker
// revocation gate. R1d caches that read so the steady-state (no-revocation)
// request is fully DB-free on a cache hit.
//
// What is cached: the family's LATEST `revoked_after` as `Option<i64>`
// epoch-seconds (see [`revoked_after_for`]) — NOT a precomputed bool. The
// per-request decision is re-evaluated LOCALLY via [`family_revoked_at`]
// against each cookie's own `iat`, so one cache entry serves every cookie in
// the family regardless of their `iat`s.
//
// NEGATIVE CACHING is the whole point: the common case (no marker, `None`) is
// cached for the TTL — without it every request still hits the DB.
//
// Staleness bound: a CROSS-NODE revocation (control grant-revoke, back-channel
// logout on a sibling gateway) takes effect within `<= TTL` on a cache-warm
// node. That bounded window is acceptable for the `<= 15-min` session cookies.
// A SAME-NODE writer ([`RevocationCache::invalidate`] from `/signout` and any
// gateway-side family revoke) busts the entry immediately, so same-node
// signout needs no TTL wait; cross-node writers rely on the TTL backstop.
//
// No tokio: a `std::Mutex<HashMap>` with sweep-on-insert and a capacity cap,
// the same shape as [`crate::dpop::JtiCache`]. The instance lives on
// `GateState` as an `Arc` and is threaded into the resolve arms like the
// other shared caches.

/// Default cache TTL. Short on purpose: it is the upper bound on how long a
/// cross-node revocation can be unseen by a cache-warm node. 5 s is small
/// relative to the `<= 15-min` session-cookie lifetime.
pub const REVOCATION_CACHE_TTL_SECS: u64 = 5;

/// Default maximum number of `(client_id, sub)` entries. Caps the key space so
/// a churn of distinct families can't grow the map unbounded. At the limit,
/// inserts evict the oldest-cached entry after sweeping expired ones.
pub const REVOCATION_CACHE_MAX_ENTRIES: usize = 100_000;

#[derive(Debug, Clone, Copy)]
struct CacheEntry {
    /// The family's latest `revoked_after` (epoch seconds), or `None` for a
    /// negatively-cached family (no marker). The per-request `> iat` decision
    /// is computed locally from this on every hit.
    revoked_after: Option<i64>,
    /// When this entry was loaded — used for the TTL expiry check and for
    /// oldest-entry eviction at capacity.
    cached_at: Instant,
}

/// Bounded, short-TTL, read-through cache of per-app family-marker revocation
/// state, keyed on `(client_id, sub)`.
///
/// Process-local (one per gateway worker process). Stores the family's latest
/// `revoked_after` so the hot path can re-judge `> iat` locally on a hit
/// without a DB round-trip. See the module-level R1d notes for the staleness
/// bound and the negative-caching rationale.
#[derive(Debug)]
pub struct RevocationCache {
    inner: Mutex<HashMap<(String, String), CacheEntry>>,
    ttl_secs: u64,
    max_entries: usize,
}

impl RevocationCache {
    /// Build a cache with the default TTL ([`REVOCATION_CACHE_TTL_SECS`]) and
    /// capacity ([`REVOCATION_CACHE_MAX_ENTRIES`]).
    #[must_use]
    pub fn new() -> Self {
        Self::with_ttl_and_capacity(REVOCATION_CACHE_TTL_SECS, REVOCATION_CACHE_MAX_ENTRIES)
    }

    /// Build a cache with an explicit TTL (seconds) and entry cap. Tests inject
    /// a tiny TTL (e.g. `0`) to exercise the staleness / fail-closed-after-TTL
    /// paths deterministically.
    #[must_use]
    pub fn with_ttl_and_capacity(ttl_secs: u64, max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl_secs,
            max_entries: max_entries.max(1),
        }
    }

    /// The configured TTL in seconds (the cross-node staleness bound).
    #[must_use]
    pub const fn ttl_secs(&self) -> u64 {
        self.ttl_secs
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), CacheEntry>> {
        self.inner.lock().unwrap_or_else(|poisoned| {
            tracing::error!("revocation cache mutex poisoned; recovering cache");
            poisoned.into_inner()
        })
    }

    fn is_fresh(&self, entry: &CacheEntry, now: Instant) -> bool {
        now.duration_since(entry.cached_at).as_secs() < self.ttl_secs
    }

    /// Look the family up. On a FRESH hit, returns `Some(revoked_after)` (the
    /// cached marker, possibly `None` for a negatively-cached family); the
    /// caller computes the `> iat` decision via [`family_revoked_at`]. On a
    /// miss (no entry or expired), returns `None` — the caller must then load
    /// from the DB and [`store`](Self::store) the result (failing CLOSED if the
    /// DB read errors).
    #[must_use]
    pub fn get(&self, client_id: &str, sub: &str, now: Instant) -> Option<Option<i64>> {
        let guard = self.lock_inner();
        // Borrow-only lookup; expired entries are swept on the next `store`.
        guard
            .get(&(client_id.to_string(), sub.to_string()))
            .filter(|e| self.is_fresh(e, now))
            .map(|e| e.revoked_after)
    }

    /// Insert/refresh the family's `revoked_after` with `cached_at = now`.
    /// Sweeps expired entries first; if still at capacity, evicts the
    /// oldest-cached entry. Negative results (`revoked_after == None`) are
    /// stored exactly like positive ones — negative caching is mandatory for
    /// the DB-avoidance win.
    pub fn store(&self, client_id: &str, sub: &str, revoked_after: Option<i64>, now: Instant) {
        let mut guard = self.lock_inner();
        // 1. Sweep expired entries (sweep-on-insert; no background task).
        guard.retain(|_, e| now.duration_since(e.cached_at).as_secs() < self.ttl_secs);
        let key = (client_id.to_string(), sub.to_string());
        // 2. Capacity guard: if inserting a NEW key would exceed the cap,
        //    evict the oldest-cached entry.
        if !guard.contains_key(&key) && guard.len() >= self.max_entries {
            if let Some(victim) = guard
                .iter()
                .min_by_key(|(_, e)| e.cached_at)
                .map(|(k, _)| k.clone())
            {
                guard.remove(&victim);
            }
        }
        guard.insert(key, CacheEntry { revoked_after, cached_at: now });
    }

    /// SAME-NODE write-side bust: drop the cached entry for `(client_id, sub)`
    /// so the next read reloads from the DB. Called by the gateway when IT
    /// writes a revocation (`/signout` → [`revoke_family`], back-channel
    /// logout) so same-node signout takes effect IMMEDIATELY (no TTL wait).
    pub fn invalidate(&self, client_id: &str, sub: &str) {
        self.lock_inner()
            .remove(&(client_id.to_string(), sub.to_string()));
    }

    /// Live entry count (post-last-sweep is only guaranteed after a `store`).
    /// Useful for metrics/tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock_inner().len()
    }

    /// Whether the cache currently holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock_inner().is_empty()
    }
}

impl Default for RevocationCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn negative_entry_is_cached_and_serves_without_reload() {
        let cache = RevocationCache::with_ttl_and_capacity(5, 10);
        let now = Instant::now();
        // No marker for the family — negative caching is mandatory.
        cache.store("oac_a", "pws_x", None, now);
        // A fresh hit returns the cached marker (None); the caller decides
        // `> iat` locally — never revoked for a None marker.
        let hit = cache.get("oac_a", "pws_x", now).expect("fresh negative hit");
        assert_eq!(hit, None);
        assert!(!family_revoked_at(hit, 1_700_000_000));
    }

    #[test]
    fn positive_entry_is_judged_against_iat_locally() {
        let cache = RevocationCache::with_ttl_and_capacity(5, 10);
        let now = Instant::now();
        // Family revoked at epoch 1000.
        cache.store("oac_a", "pws_x", Some(1000), now);
        let hit = cache.get("oac_a", "pws_x", now).expect("fresh positive hit");
        assert_eq!(hit, Some(1000));
        // A token minted BEFORE the marker (iat=900) is revoked.
        assert!(family_revoked_at(hit, 900));
        // A token minted AFTER the marker (iat=1100) is NOT — same cached entry.
        assert!(!family_revoked_at(hit, 1100));
    }

    #[test]
    fn expired_entry_is_a_miss() {
        // TTL 0 ⇒ any age >= 0s is stale; the entry is never a fresh hit.
        let cache = RevocationCache::with_ttl_and_capacity(0, 10);
        let now = Instant::now();
        cache.store("oac_a", "pws_x", None, now);
        assert!(
            cache.get("oac_a", "pws_x", now).is_none(),
            "a TTL-0 entry must read as a miss (expired)"
        );
    }

    #[test]
    fn invalidate_forces_a_miss() {
        let cache = RevocationCache::with_ttl_and_capacity(60, 10);
        let now = Instant::now();
        cache.store("oac_a", "pws_x", None, now);
        assert!(cache.get("oac_a", "pws_x", now).is_some());
        cache.invalidate("oac_a", "pws_x");
        assert!(
            cache.get("oac_a", "pws_x", now).is_none(),
            "invalidate must drop the entry so the next read reloads"
        );
        // A DIFFERENT family is unaffected by the bust.
        cache.store("oac_a", "pws_y", None, now);
        cache.invalidate("oac_a", "pws_x");
        assert!(cache.get("oac_a", "pws_y", now).is_some());
    }

    #[test]
    fn capacity_caps_entry_count_and_evicts_oldest() {
        let cache = RevocationCache::with_ttl_and_capacity(600, 2);
        let t0 = Instant::now();
        cache.store("oac", "a", None, t0);
        cache.store("oac", "b", None, t0 + Duration::from_millis(1));
        cache.store("oac", "c", None, t0 + Duration::from_millis(2));
        assert_eq!(cache.len(), 2, "cache must not exceed max_entries");
        // The oldest (a) was evicted; b and c remain.
        assert!(cache.get("oac", "a", t0 + Duration::from_millis(2)).is_none());
        assert!(cache.get("oac", "b", t0 + Duration::from_millis(2)).is_some());
        assert!(cache.get("oac", "c", t0 + Duration::from_millis(2)).is_some());
    }

    #[test]
    fn refresh_of_existing_key_does_not_grow() {
        let cache = RevocationCache::with_ttl_and_capacity(600, 2);
        let now = Instant::now();
        cache.store("oac", "a", None, now);
        cache.store("oac", "a", Some(123), now);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("oac", "a", now), Some(Some(123)));
    }
}
