//! Snapshot and restore fixtures exercised by this backend's tests.
use super::PostgresBackend;
use crate::error::DbError;

impl zeroship_data_orm::storage::Backup for PostgresBackend {
    async fn snapshot(
        &self,
        app_id: &str,
        dest_uri: &str,
        opts: zeroship_data_orm::capability::SnapshotOpts,
    ) -> Result<zeroship_data_orm::capability::SnapshotHandle, DbError> {
        backup_pg::snapshot_impl(self, app_id, dest_uri, opts).await
    }

    async fn restore(
        &self,
        app_id: &str,
        snapshot: &zeroship_data_orm::capability::SnapshotHandle,
    ) -> Result<(), DbError> {
        backup_pg::restore_impl(self, app_id, snapshot).await
    }
}

/// Inner module so the helpers stay grouped and the surrounding file
/// keeps the "thin trait facade + per-capability impl block" shape.
/// `pub(super)` so the trait methods above can call in; everything
/// else stays private.
#[cfg(test)]
mod backup_pg {
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::PostgresBackend;
    use crate::backend::postgres::PgLockManager;
    use crate::backend::postgres::lock_guard::LockGuard;
    use zeroship_data_orm::capability::SNAPSHOT_RESTORE_LOCK_TAG;
    use zeroship_data_orm::capability::{BusyPolicy, LockScope, SnapshotHandle, SnapshotOpts};
    use zeroship_data_orm::error::DbError;

    /// Resolve a file URI or bare filesystem path for a snapshot artifact.
    fn parse_dest_path(dest_uri: &str) -> Result<PathBuf, DbError> {
        if let Some(rest) = dest_uri.strip_prefix("file://") {
            // RFC 8089: `file:///abs/path` — the empty authority leaves
            // `rest` starting with `/`. We accept both `file:///x` and
            // `file://x` here since callers in tests sometimes elide
            // the empty authority.
            Ok(PathBuf::from(rest))
        } else if dest_uri.starts_with("s3://") || dest_uri.starts_with("https://") {
            Err(DbError::Configuration {
                code: "backup_dest_uri_unsupported",
                message: format!(
                    "snapshot destination URI {dest_uri:?} uses an unsupported scheme; \
                     snapshots require a file URI or bare filesystem path"
                ),
                hint: Some("use `file:///abs/path/to/snapshot.dump`".to_string()),
            })
        } else {
            // Treat anything else as a bare filesystem path so the
            // operator can pass either form. `file://` is the documented
            // shape per `SnapshotHandle::uri` rustdoc.
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
    /// Reads in 64 KiB chunks; runs entirely on the compio thread (no
    /// `spawn_blocking`) — the snapshot path is operator-driven and not
    /// hot enough to warrant offloading. The std-fs blocking reads
    /// dominate only for multi-GiB snapshots, at which point the whole
    /// op is already gated by pg_dump latency.
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

    /// Classify a `pg_dump` / `pg_restore` failure by stderr substring
    /// match. The set of patterns is intentionally narrow — only the
    /// shapes the SDK needs to branch on get a coded variant; the rest
    /// pass through as `Internal` with the raw stderr in the message
    /// so the operator can read it from the log.
    fn classify_pg_tool_failure(op: &'static str, stderr: &str, if_busy: BusyPolicy) -> DbError {
        // `pg_dump: error: connection to server ... failed: …` —
        // transient infra issue. When the caller asked for Retry we
        // mark it retryable; otherwise propagate the same code so the
        // SDK can branch on `.code === "backup_busy"`.
        if stderr.contains("connection to server")
            || stderr.contains("connection to database failed")
            || stderr.contains("could not connect to server")
        {
            return DbError::Coded {
                code: "backup_busy".to_string(),
                message: format!(
                    "{op} could not reach Postgres: {}",
                    stderr.trim().lines().next().unwrap_or(stderr.trim())
                ),
                hint: match if_busy {
                    BusyPolicy::Retry => Some(
                        "transient — retry after the database accepts connections again"
                            .to_string(),
                    ),
                    BusyPolicy::Abort => None,
                },
            };
        }
        // `pg_dump: error: relation "<app>.<table>" does not exist` /
        // schema not found.
        if stderr.contains("does not exist") || stderr.contains("no matching schemas were found") {
            return DbError::Configuration {
                code: "snapshot_app_unknown",
                message: format!(
                    "{op}: source app schema is missing — {}",
                    stderr.trim().lines().next().unwrap_or(stderr.trim())
                ),
                hint: Some(
                    "verify the app_id matches a schema that exists on this database".to_string(),
                ),
            };
        }
        // Everything else: Internal with the raw stderr so the
        // operator can debug. We keep the message bounded so a verbose
        // `pg_restore --verbose` dump doesn't flood the JS console.
        let mut truncated = stderr.trim().to_string();
        if truncated.len() > 4096 {
            truncated.truncate(4096);
            truncated.push_str("\n…[truncated]");
        }
        DbError::Internal {
            message: format!("{op} failed: {truncated}"),
        }
    }

    /// Run a `pg_dump` / `pg_restore` subprocess on a blocking worker
    /// and return its output. The connection URL is passed via the
    /// `--dbname=` long arg so it doesn't show up in `ps` output
    /// (modern pg tools mask the password component, but we still
    /// prefer the explicit form).
    async fn run_pg_tool(
        binary: &'static str,
        args: Vec<String>,
    ) -> Result<std::process::Output, std::io::Error> {
        compio::runtime::spawn_blocking(move || {
            std::process::Command::new(binary).args(&args).output()
        })
        .await
        .map_err(|_| std::io::Error::other(format!("{binary}: spawn_blocking task panicked")))?
    }

    pub(super) async fn snapshot_impl(
        backend: &PostgresBackend,
        app_id: &str,
        dest_uri: &str,
        opts: SnapshotOpts,
    ) -> Result<SnapshotHandle, DbError> {
        // Pre-flight: hold the per-app snapshot/restore lock for the duration
        // of pg_dump so another backup operation cannot replace the database
        // while we capture it. Contention surfaces as
        // `migration_in_progress` regardless of `opts.if_busy` — the
        // SDK branches on `.code` and the caller chooses to retry.
        let lock_client = backend.acquire_pooled_client_for_lock().await?;
        let scope = LockScope::GlobalApp {
            app_id: app_id.to_string(),
            name: SNAPSHOT_RESTORE_LOCK_TAG.to_string(),
        };
        let guard = match LockGuard::acquire(backend, lock_client, &scope).await {
            Ok(g) => g,
            Err(DbError::LockContention { message }) => {
                return Err(DbError::Coded {
                    code: "migration_in_progress".to_string(),
                    message: format!(
                        "snapshot: another deploy / migration is in progress for app {app_id:?}: \
                         {message}"
                    ),
                    hint: Some(
                        "retry the snapshot once the in-flight snapshot / restore completes"
                            .to_string(),
                    ),
                });
            }
            Err(other) => return Err(other),
        };

        // Parse destination + ensure parent dir exists.
        let dest_path = parse_dest_path(dest_uri)?;
        if let Some(parent) = dest_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| DbError::Internal {
                    message: format!("snapshot: create parent dir {parent:?} failed: {e}"),
                })?;
            }
        }

        // pg_dump invocation. `--no-owner --no-privileges` makes the
        // dump restore-portable across operator roles; `--format=custom`
        // is the input format `pg_restore` consumes.
        let url = backend.url().to_string();
        let app_id_owned = app_id.to_string();
        let dest_path_str = dest_path.to_string_lossy().into_owned();
        let args = vec![
            format!("--dbname={url}"),
            format!("--schema={app_id_owned}"),
            "--format=custom".to_string(),
            "--no-owner".to_string(),
            "--no-privileges".to_string(),
            format!("--file={dest_path_str}"),
        ];

        let output = match run_pg_tool("pg_dump", args).await {
            Ok(o) => o,
            Err(e) => {
                // Best-effort lock release on Err. Drop logs a leak
                // notice on failure; the session-scoped lock auto-
                // releases when the pool recycles the connection.
                let _ = guard.release().await;
                return Err(DbError::Configuration {
                    code: "pg_dump_unavailable",
                    message: format!("pg_dump spawn failed: {e}"),
                    hint: Some(
                        "ensure `pg_dump` is on PATH in the deployment environment \
                         (matches the server major version)"
                            .to_string(),
                    ),
                });
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let _ = guard.release().await;
            // Cleanup partial file so a re-run doesn't see stale bytes.
            let _ = std::fs::remove_file(&dest_path);
            return Err(classify_pg_tool_failure("pg_dump", &stderr, opts.if_busy));
        }

        // Compute the content hash over the persisted file.
        let content_hash = match sha256_file(&dest_path) {
            Ok(h) => h,
            Err(e) => {
                let _ = guard.release().await;
                let _ = std::fs::remove_file(&dest_path);
                return Err(DbError::Internal {
                    message: format!("snapshot: SHA-256 of {dest_path_str:?} failed: {e}"),
                });
            }
        };

        // Release the lock now that the dump is committed to disk.
        // From this point onwards concurrent deploys can proceed; the
        // returned handle is enough for a later restore to verify
        // integrity independent of the live schema.
        let _ = guard.release().await;

        Ok(SnapshotHandle {
            uri: dest_uri.to_string(),
            content_hash,
            created_at_ms: now_ms(),
        })
    }

    pub(super) async fn restore_impl(
        backend: &PostgresBackend,
        app_id: &str,
        snapshot: &SnapshotHandle,
    ) -> Result<(), DbError> {
        // Hold the per-app snapshot/restore lock for the whole restore so
        // another backup operation cannot observe the drop-and-recreate
        // sequence.
        let lock_client = backend.acquire_pooled_client_for_lock().await?;
        let scope = LockScope::GlobalApp {
            app_id: app_id.to_string(),
            name: SNAPSHOT_RESTORE_LOCK_TAG.to_string(),
        };
        let guard = match LockGuard::acquire(backend, lock_client, &scope).await {
            Ok(g) => g,
            Err(DbError::LockContention { message }) => {
                return Err(DbError::Coded {
                    code: "migration_in_progress".to_string(),
                    message: format!(
                        "restore: another deploy / migration is in progress for app {app_id:?}: \
                         {message}"
                    ),
                    hint: Some(
                        "retry the restore once the in-flight snapshot / restore completes"
                            .to_string(),
                    ),
                });
            }
            Err(other) => return Err(other),
        };

        // Resolve the on-disk path; only file:// is supported.
        let src_path = match parse_dest_path(&snapshot.uri) {
            Ok(p) => p,
            Err(e) => {
                let _ = guard.release().await;
                return Err(e);
            }
        };

        // Verify the content hash matches what `snapshot` recorded.
        // A mismatch means either the file was truncated/corrupted in
        // transit or the operator pointed us at the wrong dump. Refuse
        // before touching the live schema.
        match sha256_file(&src_path) {
            Ok(observed) if observed == snapshot.content_hash => { /* ok */ }
            Ok(_) => {
                let _ = guard.release().await;
                return Err(DbError::Coded {
                    code: "snapshot_hash_mismatch".to_string(),
                    message: format!(
                        "restore: SHA-256 of {:?} does not match the SnapshotHandle's \
                         recorded hash — snapshot is corrupt or this is the wrong file",
                        src_path
                    ),
                    hint: Some(
                        "re-fetch the snapshot from the original source; do NOT run \
                         restore against a dump whose hash has drifted"
                            .to_string(),
                    ),
                });
            }
            Err(e) => {
                let _ = guard.release().await;
                return Err(DbError::Internal {
                    message: format!("restore: SHA-256 of {src_path:?} failed: {e}"),
                });
            }
        }

        // Drop the live schema so pg_restore can rebuild it from
        // the dump's TOC. This is a simplification of the
        // load-bearing safety step: a full `swap_schema_atomic`
        // SECURITY DEFINER function is the hardened replacement. The
        // simple sequence is destructive — if pg_restore fails after the
        // DROP, the schema is gone and the operator has to re-
        // restore. The lock above keeps concurrent deploys out of
        // the window; the snapshot hash above keeps wrong dumps out.
        //
        // We DROP but do NOT pre-CREATE the schema: pg_dump's custom
        // format emits its own `CREATE SCHEMA "<app_id>"` statement
        // in the TOC, and pre-creating would trip pg_restore with
        // `schema … already exists`. The CASCADE drop kills the
        // schema's tables / indexes / sequences; pg_restore rebuilds
        // the full graph (schema + objects).
        //
        // Run via the pool (not the locked client) so a SQL error
        // doesn't drop the lock. We use raw quoted identifiers; app
        // ids reaching this surface are platform-controlled (typed_id
        // entity prefixes), not user input.
        let drop_sql = format!(r#"DROP SCHEMA IF EXISTS "{app_id}" CASCADE"#);
        let empty: Vec<&str> = Vec::new();
        if let Err(e) = backend.pool().query_text_params(&drop_sql, &empty).await {
            let _ = guard.release().await;
            return Err(DbError::Internal {
                message: format!(
                    "restore: DROP SCHEMA failed: {}",
                    crate::backend::postgres::pg_error::classify(&e)
                ),
            });
        }

        // pg_restore. The dump's TOC includes a `CREATE SCHEMA` so
        // we don't pre-create. `--no-owner --no-privileges` matches
        // the dump-side flags so role-rewriting doesn't trip.
        let url = backend.url().to_string();
        let src_path_str = src_path.to_string_lossy().into_owned();
        let args = vec![
            format!("--dbname={url}"),
            "--no-owner".to_string(),
            "--no-privileges".to_string(),
            "--exit-on-error".to_string(),
            src_path_str.clone(),
        ];

        let output = match run_pg_tool("pg_restore", args).await {
            Ok(o) => o,
            Err(e) => {
                let _ = guard.release().await;
                return Err(DbError::Configuration {
                    code: "pg_restore_unavailable",
                    message: format!("pg_restore spawn failed: {e}"),
                    hint: Some(
                        "ensure `pg_restore` is on PATH in the deployment environment \
                         (matches the server major version)"
                            .to_string(),
                    ),
                });
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let _ = guard.release().await;
            // The schema is empty at this point — the operator needs
            // to know that the partial restore left no data.
            let base = classify_pg_tool_failure("pg_restore", &stderr, BusyPolicy::Abort);
            return Err(match base {
                DbError::Internal { message } => DbError::Coded {
                    code: "restore_failed".to_string(),
                    message: format!(
                        "{message}\n\
                         NOTE: schema {app_id:?} is EMPTY after partial restore — \
                         operator must re-run restore to reconstruct state"
                    ),
                    hint: Some("re-run restore from the verified snapshot".to_string()),
                },
                other => other,
            });
        }

        let _ = guard.release().await;
        Ok(())
    }
}
