//! Snapshot and restore fixtures exercised by this backend's tests.
use super::SqliteBackend;
use crate::error::DbError;

impl zeroship_data_orm::storage::Backup for SqliteBackend {
    async fn snapshot(
        &self,
        app_id: &str,
        dest_uri: &str,
        opts: zeroship_data_orm::capability::SnapshotOpts,
    ) -> Result<zeroship_data_orm::capability::SnapshotHandle, DbError> {
        backup_sqlite::snapshot_impl(self, app_id, dest_uri, opts).await
    }

    async fn restore(
        &self,
        app_id: &str,
        snapshot: &zeroship_data_orm::capability::SnapshotHandle,
    ) -> Result<(), DbError> {
        backup_sqlite::restore_impl(self, app_id, snapshot).await
    }
}

#[cfg(test)]
mod backup_sqlite {
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::SqliteBackend;
    use zeroship_data_orm::capability::{
        BusyPolicy, LockScope, SNAPSHOT_RESTORE_LOCK_TAG, SnapshotHandle, SnapshotOpts,
    };
    use zeroship_data_orm::error::DbError;

    /// Resolve a file URI or bare filesystem path for a snapshot artifact.
    fn parse_dest_path(dest_uri: &str) -> Result<PathBuf, DbError> {
        if let Some(rest) = dest_uri.strip_prefix("file://") {
            Ok(PathBuf::from(rest))
        } else if dest_uri.starts_with("s3://") || dest_uri.starts_with("https://") {
            Err(DbError::Configuration {
                code: "backup_dest_uri_unsupported",
                message: format!(
                    "snapshot destination URI {dest_uri:?} uses an unsupported scheme; \
                     snapshots require a file URI or bare filesystem path"
                ),
                hint: Some("use `file:///abs/path/to/snapshot.sqlite`".to_string()),
            })
        } else {
            Ok(PathBuf::from(dest_uri))
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Stream `path` through SHA-256 and return the 32-byte digest.
    /// Reads in 64 KiB chunks — same shape as the PG arm.
    fn sha256_file(path: &Path) -> Result<[u8; 32], std::io::Error> {
        use sha2::Digest;
        let mut file = std::fs::File::open(path)?;
        let mut hasher = sha2::Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hasher.finalize().into())
    }

    /// Acquire the per-app snapshot/restore advisory lock through the
    /// in-process registry. Returns the `(key1, key2)` pair so the
    /// caller can release it symmetrically. On contention emits the
    /// typed `migration_in_progress` Coded error (mirrors the PG arm).
    async fn acquire_snapshot_restore_lock(
        backend: &SqliteBackend,
        app_id: &str,
        op: &'static str,
    ) -> Result<(String, String), DbError> {
        let scope = LockScope::GlobalApp {
            app_id: app_id.to_string(),
            name: SNAPSHOT_RESTORE_LOCK_TAG.to_string(),
        };
        let (k1, k2) = scope.to_keys();
        // Use a bounded poll matching the PG arm's
        // `try_acquire_with_backoff` schedule (5 attempts, 0/50/200/500/1000ms,
        // ~1.75s budget).
        const SCHEDULE: &[u64] = &[0, 50, 200, 500, 1000];
        for &pre_wait in SCHEDULE {
            if pre_wait > 0 {
                compio::time::sleep(std::time::Duration::from_millis(pre_wait)).await;
            }
            if backend.lock_registry.try_acquire((k1.clone(), k2.clone())) {
                return Ok((k1, k2));
            }
        }
        Err(DbError::Coded {
            code: "migration_in_progress".to_string(),
            message: format!(
                "{op}: another snapshot / restore is in progress for app {app_id:?} \
                 (in-process backup lock held; 5-attempt bounded retry exhausted)"
            ),
            hint: Some(
                "retry the operation once the in-flight snapshot / restore completes".to_string(),
            ),
        })
    }

    /// Drop the lock acquired by [`acquire_snapshot_restore_lock`].
    /// Infallible at the registry layer — unheld slots emit a
    /// `tracing::warn` no-op. Matches the contract of every other
    /// `release_advisory_lock` site.
    fn release_snapshot_restore_lock(backend: &SqliteBackend, k1: String, k2: String) {
        backend.lock_registry.release((k1, k2));
    }

    /// Classify a session-level `DbError` from `VACUUM INTO` as the
    /// `SQLITE_BUSY`-equivalent retryable error. SQLite's
    /// `busy_timeout=5000` PRAGMA absorbs most contention internally;
    /// surfacing here means a schema-change race or checkpointer
    /// holding the exclusive lock past the timeout. The
    /// `error::from_sqlite` classifier maps `SQLITE_BUSY` to
    /// `DbError::LockContention` (per the existing classifier shape).
    fn is_busy_error(e: &DbError) -> bool {
        matches!(e, DbError::LockContention { .. })
    }

    pub(super) async fn snapshot_impl(
        backend: &SqliteBackend,
        app_id: &str,
        dest_uri: &str,
        opts: SnapshotOpts,
    ) -> Result<SnapshotHandle, DbError> {
        // 1. Hold the per-app snapshot/restore lock for the whole snapshot.
        let (k1, k2) = acquire_snapshot_restore_lock(backend, app_id, "snapshot").await?;

        // 2. Parse + prepare destination.
        let dest_path = match parse_dest_path(dest_uri) {
            Ok(p) => p,
            Err(e) => {
                release_snapshot_restore_lock(backend, k1, k2);
                return Err(e);
            }
        };
        if let Some(parent) = dest_path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    release_snapshot_restore_lock(backend, k1, k2);
                    return Err(DbError::Internal {
                        message: format!("snapshot: create parent dir {parent:?} failed: {e}"),
                    });
                }
            }
        }
        // Refuse if the dest path already exists — SQLite refuses to
        // VACUUM INTO an existing file. We surface this as a typed
        // Configuration error so the operator's tool gets a clean
        // signal rather than the raw rusqlite "output file already
        // exists" message.
        if dest_path.exists() {
            release_snapshot_restore_lock(backend, k1, k2);
            return Err(DbError::Configuration {
                code: "backup_dest_exists",
                message: format!(
                    "snapshot: destination {dest_path:?} already exists — SQLite \
                     VACUUM INTO refuses to overwrite existing files"
                ),
                hint: Some(
                    "remove the existing file or choose a different destination path \
                     before re-running the snapshot"
                        .to_string(),
                ),
            });
        }

        let dest_path_str = dest_path.to_string_lossy().into_owned();

        // 3. Issue VACUUM INTO via the session actor. The actor body
        //    `run_vacuum_into` constructs the literal-quoted SQL and
        //    runs `VACUUM "<app>" INTO '<dest>'` against the per-app
        //    ATTACH alias on the control connection (which is where
        //    the attach_app_file path attached the per-app file).
        //
        //    Busy-policy retry: 3 attempts at 0/100/500ms when
        //    `opts.if_busy == Retry`. SQLite's bootstrap PRAGMA
        //    `busy_timeout=5000` absorbs most contention internally so
        //    a surfaced LockContention here is rare; the retry caps
        //    additional wait at ~0.6s on top of the PRAGMA budget.
        let attempts: &[u64] = match opts.if_busy {
            BusyPolicy::Abort => &[0],
            BusyPolicy::Retry => &[0, 100, 500],
        };
        let mut last_err: Option<DbError> = None;
        for &pre_wait in attempts {
            if pre_wait > 0 {
                compio::time::sleep(std::time::Duration::from_millis(pre_wait)).await;
            }
            match backend
                .session
                .vacuum_into(Some(app_id), &dest_path_str)
                .await
            {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(e) if is_busy_error(&e) => {
                    last_err = Some(e);
                    continue;
                }
                Err(e) => {
                    release_snapshot_restore_lock(backend, k1, k2);
                    // Clean up a partial dest file so a re-run doesn't
                    // see stale bytes.
                    let _ = std::fs::remove_file(&dest_path);
                    return Err(e);
                }
            }
        }
        if let Some(e) = last_err {
            // Exhausted retries (or Abort with one failed attempt) on
            // a SQLITE_BUSY-equivalent. Translate to the typed
            // `backup_busy` Coded code the SDK can branch on.
            release_snapshot_restore_lock(backend, k1, k2);
            let _ = std::fs::remove_file(&dest_path);
            return Err(DbError::Coded {
                code: "backup_busy".to_string(),
                message: format!(
                    "snapshot: SQLite busy after {} attempt(s) — {e}",
                    attempts.len()
                ),
                hint: match opts.if_busy {
                    BusyPolicy::Retry => Some(
                        "transient — re-run the snapshot once the contending \
                         schema-change / checkpointer releases"
                            .to_string(),
                    ),
                    BusyPolicy::Abort => Some(
                        "pass SnapshotOpts { if_busy: BusyPolicy::Retry } to absorb \
                         transient busy events with a bounded backoff"
                            .to_string(),
                    ),
                },
            });
        }

        // 4. Compute the content hash over the persisted file. Runs
        //    blocking std-fs reads on the compio thread (same as the
        //    PG arm); the snapshot path is operator-driven and not hot
        //    enough to warrant spawn_blocking.
        let content_hash = match sha256_file(&dest_path) {
            Ok(h) => h,
            Err(e) => {
                release_snapshot_restore_lock(backend, k1, k2);
                let _ = std::fs::remove_file(&dest_path);
                return Err(DbError::Internal {
                    message: format!("snapshot: SHA-256 of {dest_path_str:?} failed: {e}"),
                });
            }
        };

        // 5. Release the lock now that the snapshot is committed to
        //    disk. From here on another backup operation can proceed;
        //    the SnapshotHandle's content_hash pins integrity for the
        //    eventual restore.
        release_snapshot_restore_lock(backend, k1, k2);

        Ok(SnapshotHandle {
            uri: dest_uri.to_string(),
            content_hash,
            created_at_ms: now_ms(),
        })
    }

    pub(super) async fn restore_impl(
        backend: &SqliteBackend,
        app_id: &str,
        snapshot: &SnapshotHandle,
    ) -> Result<(), DbError> {
        // 1. Hold the per-app snapshot/restore lock for the whole restore so
        //    another backup operation cannot race the DETACH/rename/ATTACH sequence.
        let (k1, k2) = acquire_snapshot_restore_lock(backend, app_id, "restore").await?;

        // 2. Resolve the snapshot URI to an on-disk path. SQLite
        //    supports file:// only.
        let src_path = match parse_dest_path(&snapshot.uri) {
            Ok(p) => p,
            Err(e) => {
                release_snapshot_restore_lock(backend, k1, k2);
                return Err(e);
            }
        };

        // 3. Re-hash and verify integrity BEFORE touching the live
        //    DB. A mismatch means the snapshot was tampered with or
        //    truncated; refuse before any DETACH/rename.
        match sha256_file(&src_path) {
            Ok(observed) if observed == snapshot.content_hash => { /* ok */ }
            Ok(_) => {
                release_snapshot_restore_lock(backend, k1, k2);
                return Err(DbError::Coded {
                    code: "snapshot_hash_mismatch".to_string(),
                    message: format!(
                        "restore: SHA-256 of {src_path:?} does not match the \
                         SnapshotHandle's recorded hash — snapshot is corrupt or \
                         this is the wrong file"
                    ),
                    hint: Some(
                        "re-fetch the snapshot from the original source; do NOT \
                         run restore against a file whose hash has drifted"
                            .to_string(),
                    ),
                });
            }
            Err(e) => {
                release_snapshot_restore_lock(backend, k1, k2);
                return Err(DbError::Internal {
                    message: format!("restore: SHA-256 of {src_path:?} failed: {e}"),
                });
            }
        }

        // 4. Copy the snapshot into a sibling temp file beside the
        //    live per-app DB so the atomic rename happens on the same
        //    filesystem. POSIX `rename` is atomic only when both paths
        //    are on one FS; the plan documents this caveat as the
        //    operator contract. A direct `rename(src_path → live)`
        //    would consume the operator-supplied snapshot file (which
        //    they may want to keep) AND fail across filesystems.
        let live_path = backend.db_dir.join(format!("zs-{app_id}.sqlite"));
        let temp_path = backend
            .db_dir
            .join(format!("zs-{app_id}.sqlite.restore-tmp"));
        // Best-effort cleanup of a stale tmp from a prior crashed run.
        let _ = std::fs::remove_file(&temp_path);
        if let Err(e) = std::fs::copy(&src_path, &temp_path) {
            release_snapshot_restore_lock(backend, k1, k2);
            return Err(DbError::Internal {
                message: format!(
                    "restore: std::fs::copy({src_path:?} -> {temp_path:?}) failed: {e}; \
                     ensure the snapshot destination shares a filesystem with the live \
                     per-app DB directory ({:?})",
                    backend.db_dir
                ),
            });
        }

        // 5. Signal the session actor to DETACH the current live
        //    file, atomically rename temp → live, and re-ATTACH the
        //    alias against the new content. The actor's
        //    `run_reattach_file` runs the three steps on the worker
        //    thread; the CDC hook triplet on the control connection
        //    stays armed across the swap because hooks are bound to
        //    `sqlite3*`, not to an attached DB.
        let temp_path_str = temp_path.to_string_lossy().into_owned();
        let live_path_str = live_path.to_string_lossy().into_owned();
        if let Err(e) = backend
            .session
            .reattach_file(app_id, &temp_path_str, &live_path_str)
            .await
        {
            release_snapshot_restore_lock(backend, k1, k2);
            // Best-effort: leave the temp file in place so the
            // operator can inspect it; do NOT delete on error.
            return Err(e);
        }

        // 6. Restore complete. Release the lock.
        release_snapshot_restore_lock(backend, k1, k2);
        Ok(())
    }

}
