//! redb backend — single-process persistent embedded KV (dev + self-host).
//!
//! Pure-Rust embedded store ([`redb`]). One read-write `Database` takes an
//! **exclusive file lock**, so this backend is single-process by design:
//! it's the self-host / single-worker-process tier. Multi-process fleets
//! stay on the Redis backend (shared, network-backed). This is not a
//! limitation to work around — it's the tier boundary.
//!
//! Conforms to the canonical `incr` contract (overflow → error,
//! non-numeric → error, preserve existing TTL on increment) and the full
//! expanded surface (`set_if_absent` / `expire` / `ttl` / `persist` /
//! paginated `list`), byte-for-byte interchangeable with the InMemory
//! backend's cursor contract.
//!
//! ## Storage layout
//!
//! One table keyed by the scoped key (`{<app_id>}:<key>`, via [`scope`]),
//! value `(payload, expires_at_ms)` — redb has a built-in `Value` impl for
//! `(&str, Option<u64>)`, so no custom codec is needed. `expires_at_ms` is
//! an **absolute** UNIX-epoch millisecond deadline (see [`now_ms`]); it
//! must be wall-clock so a TTL survives a process restart (unlike the
//! InMemory backend's `Instant`, which is monotonic and process-local).
//!
//! ## Atomicity
//!
//! redb is single-writer / MVCC-reader, so a read-modify-write inside one
//! write transaction is serialized against every other writer — `incr` and
//! `set_if_absent` need no CAS loop. Default `Durability::Immediate` means
//! a committed write is fsynced and survives a crash.
//!
//! ## Blocking I/O
//!
//! redb is synchronous/blocking. We call it directly inside the async fns
//! (the same posture as InMemory's blocking-under-lock) rather than
//! offloading to a thread — zero tokio, and the single-process tier isn't
//! throughput-critical. A `compio::dispatcher::Dispatcher` offload is the
//! documented hook if an fsync stall is ever measured; it is NOT this
//! commit.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableError};

use super::{scope, Backend, TtlState};
use crate::error::KvError;

/// The single KV table. Key = scoped key (`{app}:key`); value =
/// `(payload, expires_at_ms)` where `expires_at_ms` is an absolute
/// UNIX-epoch millisecond deadline, or `None` for no expiry.
const KV: TableDefinition<&str, (&str, Option<u64>)> = TableDefinition::new("kv");

/// Current wall-clock time in UNIX-epoch milliseconds. Absolute (not
/// monotonic) so persisted TTL deadlines remain comparable across process
/// restarts.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// `true` if an `expires_at_ms` deadline has passed relative to `now`.
fn is_expired(expires_at: Option<u64>, now: u64) -> bool {
    expires_at.is_some_and(|at| now >= at)
}

/// Map a redb `open_table` failure on a **read** path. The table is
/// created lazily by the first write, so a fresh database (never written
/// to) has no `kv` table yet — `TableDoesNotExist` there is not an error,
/// it just means "empty keyspace". Any other `TableError` is a real
/// backend fault.
fn is_missing_table(err: &TableError) -> bool {
    matches!(err, TableError::TableDoesNotExist(_))
}

pub struct RedbBackend {
    db: Arc<Database>,
}

// `redb::Database` doesn't implement `Debug`, so derive can't apply here;
// a manual impl keeps `Backend: std::fmt::Debug` satisfied.
impl std::fmt::Debug for RedbBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedbBackend").finish_non_exhaustive()
    }
}

impl RedbBackend {
    /// Open (or create) a redb database at `path`. Takes an exclusive file
    /// lock for the process lifetime — see the module docs. Open/create
    /// failures map to [`KvError::Connection`] (the file is the backend's
    /// "transport"); a missing parent dir or a corrupt file surfaces here.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, KvError> {
        let path = path.as_ref();
        let db = Database::create(path).map_err(|e| {
            KvError::connection(format!("kv: redb open '{}': {e}", path.display()))
        })?;
        Ok(Self { db: Arc::new(db) })
    }
}

#[async_trait::async_trait(?Send)]
impl Backend for RedbBackend {
    async fn get(&self, app_id: &str, key: &str) -> Result<Option<String>, KvError> {
        let scoped = scope(app_id, key);
        let now = now_ms();
        let txn = self
            .db
            .begin_read()
            .map_err(|e| KvError::backend(format!("kv: redb get begin_read: {e}")))?;
        let table = match txn.open_table(KV) {
            Ok(t) => t,
            Err(e) if is_missing_table(&e) => return Ok(None),
            Err(e) => return Err(KvError::backend(format!("kv: redb get open_table: {e}"))),
        };
        match table
            .get(scoped.as_str())
            .map_err(|e| KvError::backend(format!("kv: redb get: {e}")))?
        {
            Some(guard) => {
                let (value, expires_at) = guard.value();
                if is_expired(expires_at, now) {
                    return Ok(None);
                }
                // Clone into an owned String before the guard / txn drops.
                Ok(Some(value.to_string()))
            }
            None => Ok(None),
        }
    }

    async fn set(
        &self,
        app_id: &str,
        key: &str,
        value: &str,
        ttl_ms: Option<u64>,
    ) -> Result<(), KvError> {
        let scoped = scope(app_id, key);
        let expires_at = ttl_ms.map(|ms| now_ms() + ms);
        let txn = self
            .db
            .begin_write()
            .map_err(|e| KvError::backend(format!("kv: redb set begin_write: {e}")))?;
        {
            let mut table = txn
                .open_table(KV)
                .map_err(|e| KvError::backend(format!("kv: redb set open_table: {e}")))?;
            table
                .insert(scoped.as_str(), (value, expires_at))
                .map_err(|e| KvError::backend(format!("kv: redb set insert: {e}")))?;
        }
        txn.commit()
            .map_err(|e| KvError::backend(format!("kv: redb set commit: {e}")))
    }

    async fn delete(&self, app_id: &str, key: &str) -> Result<bool, KvError> {
        let scoped = scope(app_id, key);
        let txn = self
            .db
            .begin_write()
            .map_err(|e| KvError::backend(format!("kv: redb delete begin_write: {e}")))?;
        let existed = {
            let mut table = txn
                .open_table(KV)
                .map_err(|e| KvError::backend(format!("kv: redb delete open_table: {e}")))?;
            let removed = table
                .remove(scoped.as_str())
                .map_err(|e| KvError::backend(format!("kv: redb delete remove: {e}")))?;
            removed.is_some()
        };
        txn.commit()
            .map_err(|e| KvError::backend(format!("kv: redb delete commit: {e}")))?;
        Ok(existed)
    }

    async fn incr(
        &self,
        app_id: &str,
        key: &str,
        delta: i64,
        ttl_ms: Option<u64>,
    ) -> Result<i64, KvError> {
        let scoped = scope(app_id, key);
        let now = now_ms();
        // One write txn — single-writer/MVCC makes the read-modify-write
        // serializable, so no CAS loop is needed.
        let txn = self
            .db
            .begin_write()
            .map_err(|e| KvError::backend(format!("kv: redb incr begin_write: {e}")))?;
        let next = {
            let mut table = txn
                .open_table(KV)
                .map_err(|e| KvError::backend(format!("kv: redb incr open_table: {e}")))?;

            // Read current; treat an expired entry as absent.
            let live: Option<(String, Option<u64>)> = match table
                .get(scoped.as_str())
                .map_err(|e| KvError::backend(format!("kv: redb incr get: {e}")))?
            {
                Some(guard) => {
                    let (value, expires_at) = guard.value();
                    if is_expired(expires_at, now) {
                        None
                    } else {
                        Some((value.to_string(), expires_at))
                    }
                }
                None => None,
            };

            let (next, expires_at) = match live {
                Some((value, expires_at)) => {
                    // Existing key: parse, add (checked), preserve TTL.
                    let current = value.parse::<i64>().map_err(|_| {
                        KvError::non_numeric(format!(
                            "kv: incr on non-numeric value for key '{key}'"
                        ))
                    })?;
                    let next = current.checked_add(delta).ok_or_else(|| {
                        KvError::overflow(format!("kv: incr overflowed i64 for key '{key}'"))
                    })?;
                    (next, expires_at) // preserved
                }
                None => {
                    // Created this call: apply ttl_ms (fixed-window).
                    let next = 0_i64.checked_add(delta).ok_or_else(|| {
                        KvError::overflow(format!("kv: incr overflowed i64 for key '{key}'"))
                    })?;
                    (next, ttl_ms.map(|ms| now + ms))
                }
            };

            let next_s = next.to_string();
            table
                .insert(scoped.as_str(), (next_s.as_str(), expires_at))
                .map_err(|e| KvError::backend(format!("kv: redb incr insert: {e}")))?;
            next
        };
        txn.commit()
            .map_err(|e| KvError::backend(format!("kv: redb incr commit: {e}")))?;
        Ok(next)
    }

    async fn set_if_absent(
        &self,
        app_id: &str,
        key: &str,
        value: &str,
        ttl_ms: Option<u64>,
    ) -> Result<bool, KvError> {
        let scoped = scope(app_id, key);
        let now = now_ms();
        let txn = self
            .db
            .begin_write()
            .map_err(|e| KvError::backend(format!("kv: redb setIfAbsent begin_write: {e}")))?;
        let stored = {
            let mut table = txn.open_table(KV).map_err(|e| {
                KvError::backend(format!("kv: redb setIfAbsent open_table: {e}"))
            })?;
            // An expired entry counts as absent.
            let occupied = match table
                .get(scoped.as_str())
                .map_err(|e| KvError::backend(format!("kv: redb setIfAbsent get: {e}")))?
            {
                Some(guard) => {
                    let (_, expires_at) = guard.value();
                    !is_expired(expires_at, now)
                }
                None => false,
            };
            if occupied {
                false
            } else {
                let expires_at = ttl_ms.map(|ms| now + ms);
                table
                    .insert(scoped.as_str(), (value, expires_at))
                    .map_err(|e| {
                        KvError::backend(format!("kv: redb setIfAbsent insert: {e}"))
                    })?;
                true
            }
        };
        txn.commit()
            .map_err(|e| KvError::backend(format!("kv: redb setIfAbsent commit: {e}")))?;
        Ok(stored)
    }

    async fn expire(&self, app_id: &str, key: &str, ttl_ms: u64) -> Result<bool, KvError> {
        let scoped = scope(app_id, key);
        let now = now_ms();
        let txn = self
            .db
            .begin_write()
            .map_err(|e| KvError::backend(format!("kv: redb expire begin_write: {e}")))?;
        let updated = {
            let mut table = txn
                .open_table(KV)
                .map_err(|e| KvError::backend(format!("kv: redb expire open_table: {e}")))?;
            // Present-and-unexpired → set the new deadline; otherwise false.
            let payload: Option<String> = match table
                .get(scoped.as_str())
                .map_err(|e| KvError::backend(format!("kv: redb expire get: {e}")))?
            {
                Some(guard) => {
                    let (value, expires_at) = guard.value();
                    if is_expired(expires_at, now) {
                        None
                    } else {
                        Some(value.to_string())
                    }
                }
                None => None,
            };
            match payload {
                Some(value) => {
                    table
                        .insert(scoped.as_str(), (value.as_str(), Some(now + ttl_ms)))
                        .map_err(|e| {
                            KvError::backend(format!("kv: redb expire insert: {e}"))
                        })?;
                    true
                }
                None => false,
            }
        };
        txn.commit()
            .map_err(|e| KvError::backend(format!("kv: redb expire commit: {e}")))?;
        Ok(updated)
    }

    async fn ttl(&self, app_id: &str, key: &str) -> Result<TtlState, KvError> {
        let scoped = scope(app_id, key);
        let now = now_ms();
        let txn = self
            .db
            .begin_read()
            .map_err(|e| KvError::backend(format!("kv: redb ttl begin_read: {e}")))?;
        let table = match txn.open_table(KV) {
            Ok(t) => t,
            Err(e) if is_missing_table(&e) => return Ok(TtlState::Missing),
            Err(e) => return Err(KvError::backend(format!("kv: redb ttl open_table: {e}"))),
        };
        match table
            .get(scoped.as_str())
            .map_err(|e| KvError::backend(format!("kv: redb ttl: {e}")))?
        {
            Some(guard) => {
                let (_, expires_at) = guard.value();
                match expires_at {
                    None => Ok(TtlState::NoExpiry),
                    Some(at) if now >= at => Ok(TtlState::Missing),
                    Some(at) => Ok(TtlState::ExpiresInMs(at - now)),
                }
            }
            None => Ok(TtlState::Missing),
        }
    }

    async fn persist(&self, app_id: &str, key: &str) -> Result<bool, KvError> {
        let scoped = scope(app_id, key);
        let now = now_ms();
        let txn = self
            .db
            .begin_write()
            .map_err(|e| KvError::backend(format!("kv: redb persist begin_write: {e}")))?;
        let updated = {
            let mut table = txn
                .open_table(KV)
                .map_err(|e| KvError::backend(format!("kv: redb persist open_table: {e}")))?;
            // Present-and-unexpired with a TTL → clear it; else false.
            let payload: Option<String> = match table
                .get(scoped.as_str())
                .map_err(|e| KvError::backend(format!("kv: redb persist get: {e}")))?
            {
                Some(guard) => {
                    let (value, expires_at) = guard.value();
                    if is_expired(expires_at, now) || expires_at.is_none() {
                        None
                    } else {
                        Some(value.to_string())
                    }
                }
                None => None,
            };
            match payload {
                Some(value) => {
                    table
                        .insert(scoped.as_str(), (value.as_str(), None))
                        .map_err(|e| {
                            KvError::backend(format!("kv: redb persist insert: {e}"))
                        })?;
                    true
                }
                None => false,
            }
        };
        txn.commit()
            .map_err(|e| KvError::backend(format!("kv: redb persist commit: {e}")))?;
        Ok(updated)
    }

    async fn list(
        &self,
        app_id: &str,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<String>, Option<String>), KvError> {
        let scoped_prefix = scope(app_id, prefix);
        let strip = format!("{{{app_id}}}:");
        let now = now_ms();

        let txn = self
            .db
            .begin_read()
            .map_err(|e| KvError::backend(format!("kv: redb list begin_read: {e}")))?;
        let table = match txn.open_table(KV) {
            Ok(t) => t,
            Err(e) if is_missing_table(&e) => return Ok((Vec::new(), None)),
            Err(e) => return Err(KvError::backend(format!("kv: redb list open_table: {e}"))),
        };

        // Ordered range from the scoped prefix; take-while the key still
        // shares the prefix. Skip expired entries (lazy TTL — we don't
        // reap on a read txn). The `cursor` is the last *unscoped* key
        // returned previously, so we resume strictly after it.
        let mut page: Vec<String> = Vec::new();
        let mut last_scoped: Option<String> = None;
        let mut any_remaining = false;

        let range = table
            .range(scoped_prefix.as_str()..)
            .map_err(|e| KvError::backend(format!("kv: redb list range: {e}")))?;

        for entry in range {
            let (key_guard, val_guard) = entry
                .map_err(|e| KvError::backend(format!("kv: redb list iter: {e}")))?;
            let scoped_key = key_guard.value();
            // Ordered iteration: the first key past the prefix ends it.
            if !scoped_key.starts_with(&scoped_prefix) {
                break;
            }
            let (_, expires_at) = val_guard.value();
            if is_expired(expires_at, now) {
                continue;
            }
            let unscoped = match scoped_key.strip_prefix(&strip) {
                Some(u) => u.to_string(),
                None => continue,
            };
            // Resume strictly after the cursor (exclusive lower bound).
            if let Some(c) = cursor {
                if unscoped.as_str() <= c {
                    continue;
                }
            }
            if page.len() == limit {
                // We already have a full page; this extra match means more
                // keys remain beyond the page boundary.
                any_remaining = true;
                break;
            }
            last_scoped = Some(unscoped.clone());
            page.push(unscoped);
        }

        // A next cursor is returned only when we filled the page AND saw at
        // least one more matching key — mirrors InMemory exactly.
        let next_cursor = if page.len() == limit && any_remaining {
            last_scoped
        } else {
            None
        };
        Ok((page, next_cursor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const APP: &str = "test-app";

    /// Open a fresh redb backend rooted in a temp dir. The `TempDir` is
    /// returned so the caller keeps it alive for the test's duration.
    fn backend() -> (RedbBackend, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kv.redb");
        let b = RedbBackend::open(&path).expect("open redb");
        (b, dir)
    }

    #[compio::test]
    async fn get_set_delete_roundtrip() {
        let (b, _dir) = backend();
        assert!(b.get(APP, "k").await.unwrap().is_none());
        b.set(APP, "k", "v", None).await.unwrap();
        assert_eq!(b.get(APP, "k").await.unwrap().as_deref(), Some("v"));
        assert!(b.delete(APP, "k").await.unwrap());
        assert!(!b.delete(APP, "k").await.unwrap());
        assert!(b.get(APP, "k").await.unwrap().is_none());
    }

    #[compio::test]
    async fn set_overwrites_value_and_ttl() {
        let (b, _dir) = backend();
        b.set(APP, "k", "first", Some(100_000)).await.unwrap();
        b.set(APP, "k", "second", None).await.unwrap();
        assert_eq!(b.get(APP, "k").await.unwrap().as_deref(), Some("second"));
        assert_eq!(b.ttl(APP, "k").await.unwrap(), TtlState::NoExpiry);
    }

    #[compio::test]
    async fn empty_value_is_allowed() {
        let (b, _dir) = backend();
        b.set(APP, "k", "", None).await.unwrap();
        assert_eq!(b.get(APP, "k").await.unwrap().as_deref(), Some(""));
    }

    #[compio::test]
    async fn persists_across_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kv.redb");
        {
            let b = RedbBackend::open(&path).unwrap();
            b.set(APP, "k", "durable", None).await.unwrap();
            // Drop releases the exclusive file lock.
        }
        let b2 = RedbBackend::open(&path).unwrap();
        assert_eq!(b2.get(APP, "k").await.unwrap().as_deref(), Some("durable"));
    }

    #[compio::test]
    async fn incr_creates_and_accumulates() {
        let (b, _dir) = backend();
        assert_eq!(b.incr(APP, "c", 5, None).await.unwrap(), 5);
        assert_eq!(b.incr(APP, "c", -3, None).await.unwrap(), 2);
        assert_eq!(b.incr(APP, "c", 1, None).await.unwrap(), 3);
    }

    #[compio::test]
    async fn incr_on_seeded_numeric_value() {
        let (b, _dir) = backend();
        b.set(APP, "c", "100", None).await.unwrap();
        assert_eq!(b.incr(APP, "c", 5, None).await.unwrap(), 105);
    }

    #[compio::test]
    async fn incr_non_numeric_is_error() {
        let (b, _dir) = backend();
        b.set(APP, "c", "notanumber", None).await.unwrap();
        match b.incr(APP, "c", 1, None).await {
            Err(KvError::NonNumeric { .. }) => {}
            other => panic!("expected NonNumeric, got {other:?}"),
        }
    }

    #[compio::test]
    async fn incr_overflow_is_error() {
        let (b, _dir) = backend();
        b.set(APP, "c", &i64::MAX.to_string(), None).await.unwrap();
        match b.incr(APP, "c", 1, None).await {
            Err(KvError::Overflow { .. }) => {}
            other => panic!("expected Overflow, got {other:?}"),
        }
    }

    #[compio::test]
    async fn incr_preserves_existing_ttl() {
        let (b, _dir) = backend();
        // Create with a TTL via incr (key created this call).
        assert_eq!(b.incr(APP, "c", 1, Some(100_000)).await.unwrap(), 1);
        let before = b.ttl(APP, "c").await.unwrap();
        assert!(matches!(before, TtlState::ExpiresInMs(_)));
        // Subsequent incr must NOT reset/clear the TTL.
        b.incr(APP, "c", 1, Some(50)).await.unwrap();
        let after = b.ttl(APP, "c").await.unwrap();
        assert!(
            matches!(after, TtlState::ExpiresInMs(ms) if ms > 1000),
            "TTL should be preserved (~100s), got {after:?}"
        );
    }

    #[compio::test]
    async fn incr_ttl_only_applies_on_create() {
        let (b, _dir) = backend();
        // Existing key with no TTL.
        b.set(APP, "c", "10", None).await.unwrap();
        b.incr(APP, "c", 1, Some(100_000)).await.unwrap();
        // incr's ttl_ms must NOT apply to a pre-existing key.
        assert_eq!(b.ttl(APP, "c").await.unwrap(), TtlState::NoExpiry);
    }

    #[compio::test]
    async fn set_if_absent_stores_only_when_absent() {
        let (b, _dir) = backend();
        assert!(b.set_if_absent(APP, "lock", "1", None).await.unwrap());
        assert!(!b.set_if_absent(APP, "lock", "2", None).await.unwrap());
        assert_eq!(b.get(APP, "lock").await.unwrap().as_deref(), Some("1"));
    }

    #[compio::test]
    async fn set_if_absent_treats_expired_as_absent() {
        let (b, _dir) = backend();
        b.set(APP, "lock", "old", Some(1)).await.unwrap();
        compio::time::sleep(Duration::from_millis(10)).await;
        assert!(b.set_if_absent(APP, "lock", "new", None).await.unwrap());
        assert_eq!(b.get(APP, "lock").await.unwrap().as_deref(), Some("new"));
    }

    #[compio::test]
    async fn expire_sets_ttl_and_reports_missing() {
        let (b, _dir) = backend();
        assert!(!b.expire(APP, "k", 1000).await.unwrap()); // missing
        b.set(APP, "k", "v", None).await.unwrap();
        assert!(b.expire(APP, "k", 100_000).await.unwrap());
        assert!(matches!(b.ttl(APP, "k").await.unwrap(), TtlState::ExpiresInMs(_)));
    }

    #[compio::test]
    async fn ttl_distinguishes_missing_no_expiry_and_expiring() {
        let (b, _dir) = backend();
        assert_eq!(b.ttl(APP, "k").await.unwrap(), TtlState::Missing);
        b.set(APP, "k", "v", None).await.unwrap();
        assert_eq!(b.ttl(APP, "k").await.unwrap(), TtlState::NoExpiry);
        b.set(APP, "k", "v", Some(100_000)).await.unwrap();
        assert!(matches!(b.ttl(APP, "k").await.unwrap(), TtlState::ExpiresInMs(_)));
    }

    #[compio::test]
    async fn persist_removes_ttl() {
        let (b, _dir) = backend();
        b.set(APP, "k", "v", Some(100_000)).await.unwrap();
        assert!(b.persist(APP, "k").await.unwrap()); // had a TTL
        assert_eq!(b.ttl(APP, "k").await.unwrap(), TtlState::NoExpiry);
        assert!(!b.persist(APP, "k").await.unwrap()); // already no TTL
        assert!(!b.persist(APP, "missing").await.unwrap()); // missing
    }

    #[compio::test]
    async fn expired_keys_are_treated_as_absent_on_access() {
        let (b, _dir) = backend();
        b.set(APP, "k", "v", Some(1)).await.unwrap();
        compio::time::sleep(Duration::from_millis(10)).await;
        assert!(b.get(APP, "k").await.unwrap().is_none());
        assert_eq!(b.ttl(APP, "k").await.unwrap(), TtlState::Missing);
    }

    #[compio::test]
    async fn list_filters_by_prefix_and_strips_scope() {
        let (b, _dir) = backend();
        for k in ["user:1", "user:2", "post:1"] {
            b.set(APP, k, "x", None).await.unwrap();
        }
        let (mut users, cursor) = b.list(APP, "user:", None, 100).await.unwrap();
        users.sort();
        assert_eq!(users, vec!["user:1", "user:2"]);
        assert!(cursor.is_none());
    }

    #[compio::test]
    async fn list_skips_expired() {
        let (b, _dir) = backend();
        b.set(APP, "a", "x", None).await.unwrap();
        b.set(APP, "b", "x", Some(1)).await.unwrap();
        compio::time::sleep(Duration::from_millis(10)).await;
        let (mut keys, _) = b.list(APP, "", None, 100).await.unwrap();
        keys.sort();
        assert_eq!(keys, vec!["a"]);
    }

    #[compio::test]
    async fn list_isolates_apps() {
        let (b, _dir) = backend();
        b.set("app-a", "k", "a", None).await.unwrap();
        b.set("app-b", "k", "b", None).await.unwrap();
        let (a_keys, _) = b.list("app-a", "", None, 100).await.unwrap();
        let (b_keys, _) = b.list("app-b", "", None, 100).await.unwrap();
        assert_eq!(a_keys, vec!["k"]);
        assert_eq!(b_keys, vec!["k"]);
    }

    #[compio::test]
    async fn list_paginates_with_cursor() {
        let (b, _dir) = backend();
        for i in 0..5 {
            b.set(APP, &format!("k{i}"), "x", None).await.unwrap();
        }
        // Page 1: limit 2.
        let (page1, c1) = b.list(APP, "k", None, 2).await.unwrap();
        assert_eq!(page1, vec!["k0", "k1"]);
        let c1 = c1.expect("more pages remain");

        // Page 2.
        let (page2, c2) = b.list(APP, "k", Some(&c1), 2).await.unwrap();
        assert_eq!(page2, vec!["k2", "k3"]);
        let c2 = c2.expect("more pages remain");

        // Page 3 (final).
        let (page3, c3) = b.list(APP, "k", Some(&c2), 2).await.unwrap();
        assert_eq!(page3, vec!["k4"]);
        assert!(c3.is_none(), "last page must return cursor None");
    }

    #[compio::test]
    async fn list_full_page_at_exact_boundary_ends() {
        let (b, _dir) = backend();
        for i in 0..2 {
            b.set(APP, &format!("k{i}"), "x", None).await.unwrap();
        }
        // A page that exactly consumes all keys must report no more.
        let (page, cursor) = b.list(APP, "k", None, 2).await.unwrap();
        assert_eq!(page, vec!["k0", "k1"]);
        assert!(cursor.is_none());
    }

    #[compio::test]
    async fn list_empty_returns_empty() {
        let (b, _dir) = backend();
        let (keys, cursor) = b.list(APP, "none:", None, 100).await.unwrap();
        assert!(keys.is_empty());
        assert!(cursor.is_none());
    }
}
