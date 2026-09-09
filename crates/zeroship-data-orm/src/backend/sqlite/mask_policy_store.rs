//! Where SQLite keeps an app's mask policy across restarts.
//!
//! PostgreSQL needs no equivalent: its policy is cached in the thread context
//! and re-installed by `installSchema` on every boot. SQLite runs where there
//! may be no such re-install, so the policy is written beside the app files.
//!
//! **This lived in `crud/mask_policy.rs` until 2026-09-02**, which put a sidecar
//! path, a file-lock registry, an atomic tmp-and-rename and a JSON merge inside
//! an engine module. Neither census saw it: the signature census looks for
//! foreign-CRATE markers and `SqliteBackend` is a crate-internal path, and the
//! direction census only flags upward edges while ENGINE -> SQLITE is downward.
//! Both instruments were right; the code was simply in the wrong tier.
//!
//! **The seam is `zeroship_data_sql::value::Value`, deliberately.** A store that took
//! `MaskPolicy` would name an engine type from the vendor tier - the upward
//! edge that the `auth::bootstrap` re-exports were deleted for. It costs
//! nothing to avoid: the persisted form was always JSON, so the caller converts
//! and this module stores what it is given.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use zeroship_data_orm::error::DbError;
use zeroship_data_sql::value::Value;

use super::SqliteBackend;

/// SQLite sidecar file path. Lives next to the per-app
/// SQLite files at `<db_dir>/mask_policies.json`. Single global file
/// keyed by `app_id` — mirrors the in-process structure most closely
/// and avoids per-app I/O multipliers (a 50-app worker would otherwise
/// open 50 files at startup).
fn policy_path(sq: &SqliteBackend) -> PathBuf {
    sq.db_dir().join("mask_policies.json")
}

type PolicyFileLock = &'static Mutex<()>;

fn policy_file_lock(path: &Path) -> Result<std::sync::MutexGuard<'static, ()>, DbError> {
    static POLICY_FILE_LOCKS: OnceLock<Mutex<HashMap<PathBuf, PolicyFileLock>>> = OnceLock::new();
    let locks = POLICY_FILE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let lock: PolicyFileLock = {
        let mut locks = locks
            .lock()
            .map_err(|_| DbError::internal("mask_policies.json: global lock registry poisoned"))?;
        *locks
            .entry(path.to_path_buf())
            .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))))
    };
    lock.lock()
        .map_err(|_| DbError::internal("mask_policies.json: per-file lock poisoned"))
}

/// SQLite atomic write. Strategy:
///
/// 1. Read existing `<dir>/mask_policies.json` (treat ENOENT as empty
///    `{}`).
/// 2. Merge: insert / overwrite the app's entry.
/// 3. Serialise the merged map.
/// 4. Write to `<dir>/mask_policies.json.tmp`.
/// 5. Atomic rename to `<dir>/mask_policies.json`. POSIX rename is
///    atomic on the same filesystem; on crash mid-write the original
///    file survives untouched.
///
/// The blocking file I/O runs on compio's blocking pool, NOT on the
/// compio event loop thread. A per-file process-local mutex
/// serialises concurrent writers so two `setMaskPolicy` calls cannot
/// both read the same old JSON, then race their rewrites.
///
/// # Errors
///
/// Any I/O or serialisation failure along the five steps above.
pub async fn persist(sq: &SqliteBackend, app_id: &str, policy_json: &Value) -> Result<(), DbError> {
    let path = policy_path(sq);
    let app_id = app_id.to_string();
    let policy_json = policy_json.clone();
    compio::runtime::spawn_blocking(move || persist_blocking(path, app_id, policy_json))
        .await
        .map_err(|_| {
            DbError::internal("mask_policies.json: persist spawn_blocking task panicked")
        })?
}

fn persist_blocking(path: PathBuf, app_id: String, policy_json: Value) -> Result<(), DbError> {
    use std::fs;
    let _file_guard = policy_file_lock(&path)?;
    let tmp = path.with_extension("json.tmp");

    // 1. Read existing.
    let existing: Value = match fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).map_err(|e| {
            DbError::internal(format!(
                "mask_policies.json: parse existing file at {}: {e}",
                path.display()
            ))
        })?,
        Ok(_) => Value::Object(zeroship_data_sql::value::Map::new()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Value::Object(zeroship_data_sql::value::Map::new())
        }
        Err(e) => {
            return Err(DbError::internal(format!(
                "mask_policies.json: read {}: {e}",
                path.display()
            )));
        }
    };
    let mut obj = match existing {
        Value::Object(o) => o,
        _ => zeroship_data_sql::value::Map::new(),
    };

    // 2. Merge.
    obj.insert(app_id, policy_json);
    let merged = Value::Object(obj);
    let serialised = serde_json::to_string_pretty(&merged)
        .map_err(|e| DbError::internal(format!("mask_policies.json: serialise: {e}")))?;

    // 3 + 4. Write tmp.
    fs::write(&tmp, &serialised).map_err(|e| {
        DbError::internal(format!(
            "mask_policies.json: write tmp {}: {e}",
            tmp.display()
        ))
    })?;

    // 5. Atomic rename.
    fs::rename(&tmp, &path).map_err(|e| {
        DbError::internal(format!(
            "mask_policies.json: rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        ))
    })?;
    Ok(())
}

/// Load the stored policy JSON for a single app from the sidecar file.
///
/// `None` when the file is absent or has no entry for the app.
///
/// # Errors
///
/// A read that is not `NotFound`, or a file that is not parseable JSON.
pub async fn load(sq: &SqliteBackend, app_id: &str) -> Result<Option<Value>, DbError> {
    let path = policy_path(sq);
    let app_id = app_id.to_string();
    compio::runtime::spawn_blocking(move || load_blocking(path, app_id))
        .await
        .map_err(|_| DbError::internal("mask_policies.json: load spawn_blocking task panicked"))?
}

fn load_blocking(path: PathBuf, app_id: String) -> Result<Option<Value>, DbError> {
    use std::fs;
    let _file_guard = policy_file_lock(&path)?;
    let text = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(DbError::internal(format!(
                "mask_policies.json: read {}: {e}",
                path.display()
            )));
        }
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| {
        DbError::internal(format!("mask_policies.json: parse {}: {e}", path.display()))
    })?;
    Ok(v.get(app_id.as_str()).cloned())
}

#[cfg(test)]
mod tests {
    use super::{load, load_blocking, persist, persist_blocking};
    use zeroship_data_sql::value;
    use zeroship_data_sql::value::Value;

    /// Round-trip through the async pair, off the event-loop thread.
    ///
    /// Moved here from `crud/mask_policy.rs` with the code it covers, and now
    /// asserts on the stored JSON rather than on a `MaskPolicy`. That is the
    /// point of the new seam: this module never learns what the policy means.
    #[test]
    fn stored_policy_json_survives_a_round_trip() {
        compio::runtime::Runtime::new()
            .expect("compio runtime")
            .block_on(async {
                let dir = tempfile::tempdir().expect("create tempdir");
                let backend = crate::backend::sqlite::SqliteBackend::new(
                    dir.path().to_path_buf(),
                    std::sync::Arc::new(crate::backend::sqlite::NullChangeSink),
                    zeroship_data_orm::encryption::LocalKeySource::env_var(),
                )
                .expect("open sqlite backend");
                let stored = value!({ "admin": ["public", "pii"], "support": ["public"] });

                persist(&backend, "app_async", &stored)
                    .await
                    .expect("persist sqlite policy");
                let loaded = load(&backend, "app_async")
                    .await
                    .expect("load sqlite policy")
                    .expect("policy entry");

                assert_eq!(loaded, stored);
            });
    }

    /// **Two writers must not lose each other's entry.**
    ///
    /// Each reads the whole file, merges its app in and rewrites; without the
    /// per-file lock both can read the same old JSON and the second rename
    /// wins, silently dropping the first app's policy. The assertion is that
    /// BOTH keys survive, which is exactly what last-writer-wins breaks.
    #[test]
    fn the_sidecar_serialises_concurrent_writers() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("mask_policies.json");
        let policy_a = value!({ "admin": ["pii", "spi"] });
        let policy_b = value!({ "support": ["public"] });

        std::thread::scope(|scope| {
            let path_a = path.clone();
            let path_b = path.clone();
            let write_a =
                scope.spawn(move || persist_blocking(path_a, "app_a".to_string(), policy_a));
            let write_b =
                scope.spawn(move || persist_blocking(path_b, "app_b".to_string(), policy_b));

            write_a
                .join()
                .expect("writer A thread")
                .expect("writer A persist");
            write_b
                .join()
                .expect("writer B thread")
                .expect("writer B persist");
        });

        let raw = std::fs::read_to_string(&path).expect("read sidecar");
        let parsed: Value = serde_json::from_str(&raw).expect("parse sidecar json");
        assert!(
            parsed.get("app_a").is_some() && parsed.get("app_b").is_some(),
            "serialized sidecar must retain both app entries: {raw}"
        );

        let loaded_a = load_blocking(path.clone(), "app_a".to_string())
            .expect("load app_a")
            .expect("app_a entry");
        let loaded_b = load_blocking(path, "app_b".to_string())
            .expect("load app_b")
            .expect("app_b entry");
        assert_eq!(loaded_a, value!({ "admin": ["pii", "spi"] }));
        assert_eq!(loaded_b, value!({ "support": ["public"] }));
    }
}
