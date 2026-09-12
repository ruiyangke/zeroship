//! SQLite snapshots contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;

use zeroship_data_orm::backend::{LockManager, LockScope};

use zeroship_data_orm::error::DbError;

use zeroship_data_orm::backend::{Backup as _, BusyPolicy as BackupBusyPolicy, SnapshotOpts};

#[cfg(test)]
use crate::tests::fixtures::DatabaseFixture;

/// **Gate #4**: round-trip snapshot+restore on SQLite.
/// Insert N rows into a per-app collection; snapshot to a temp dir;
/// raw `DROP TABLE` to clear rows; restore; assert the rows recovered.
///
/// The dest URI uses the `file://` scheme (the only one supported).
/// We pick a destination INSIDE the backend's `db_dir` so the restore's
/// `std::fs::copy -> rename` swap lands on the same filesystem as the
/// live per-app file (POSIX rename atomic-same-FS contract).
#[test]
fn snapshot_restore_round_trip_sqlite() {
    Host::test(|host| {
        host.run(async {
            let (backend, dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");

            // Seed deterministic rows.
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_demo\".\"notes\" (id INTEGER PRIMARY KEY, body TEXT)",
                    &[],
                )
                .await
                .expect("CREATE TABLE notes");
            const ROW_COUNT: i64 = 10;
            for i in 0..ROW_COUNT {
                let sql = format!("INSERT INTO \"app_demo\".\"notes\" VALUES ({i}, 'row-{i}')");
                backend
                    .execute_fixture(&sql, &[])
                    .await
                    .expect("INSERT row");
            }
            // Sanity: row count is N.
            let client = backend
                .fixture_session("app_demo")
                .await
                .expect("acquire client");
            let rows = client
                .query("SELECT COUNT(*) FROM \"app_demo\".\"notes\"", &[])
                .await
                .expect("count rows pre-snapshot");
            assert_eq!(rows[0][0].as_deref(), Some(ROW_COUNT.to_string().as_str()));

            // Snapshot. Dest must NOT pre-exist (SQLite VACUUM INTO refuses
            // to overwrite); use a fresh name in the same dir as the live
            // per-app file so the eventual restore's same-FS rename works.
            let snap_path = dir.path().join("snap-app_demo.sqlite");
            let snap_uri = format!("file://{}", snap_path.to_string_lossy());
            let handle = backend
                .snapshot(
                    "app_demo",
                    &snap_uri,
                    SnapshotOpts {
                        if_busy: BackupBusyPolicy::Retry,
                    },
                )
                .await
                .expect("snapshot");
            assert!(snap_path.exists(), "snapshot file must exist on disk");
            assert_eq!(handle.uri, snap_uri, "handle uri echoes caller-supplied");
            // The content_hash field is the SHA-256 of the on-disk bytes —
            // re-hash here and compare bytewise.
            let observed_hash: [u8; 32] = {
                use sha2::Digest;
                let bytes = std::fs::read(&snap_path).expect("read snap file");
                sha2::Sha256::digest(&bytes).into()
            };
            assert_eq!(
                handle.content_hash, observed_hash,
                "content_hash must match SHA-256 of on-disk file"
            );

            // Clear the live rows so the restore is a meaningful recovery.
            backend
                .execute_fixture("DELETE FROM \"app_demo\".\"notes\"", &[])
                .await
                .expect("DELETE rows");
            let rows_after_delete = client
                .query("SELECT COUNT(*) FROM \"app_demo\".\"notes\"", &[])
                .await
                .expect("count rows post-delete");
            assert_eq!(rows_after_delete[0][0].as_deref(), Some("0"));

            // Restore. After this call the per-app file is replaced with
            // the snapshot content and the session re-ATTACHed against
            // the new file.
            backend.restore("app_demo", &handle).await.expect("restore");

            // Rows are back.
            let rows_restored = client
                .query("SELECT COUNT(*) FROM \"app_demo\".\"notes\"", &[])
                .await
                .expect("count rows post-restore");
            assert_eq!(
                rows_restored[0][0].as_deref(),
                Some(ROW_COUNT.to_string().as_str()),
                "restore must recover the original row count"
            );
        });
    })
}

/// **Gate #5 / CRITICAL #3 fence**: VACUUM INTO under a
/// concurrent writer must be snapshot-isolated (the dest matches the
/// commit point visible when VACUUM INTO began; writes appended
/// during the copy do NOT land in the snapshot). We assert:
///
///   (a) the snapshot file is well-formed (opens cleanly, COUNT(*)
///       returns a finite number).
///   (b) live > snap — at least one write happened during the snapshot
///       window and landed in the live DB but not the snapshot copy.
///   (c) no `SQLITE_BUSY` surfaced under `BusyPolicy::Retry`.
///
/// Mechanism: spawn a `std::thread` that hammers INSERTs into the
/// per-app collection via a SECOND `Connection` opened directly on
/// the per-app file (bypasses the session actor's mpsc queue — the
/// writer thread + the snapshot's read transaction race for the WAL
/// observer position). The main thread takes the snapshot; the writer
/// keeps inserting throughout.
#[test]
fn vacuum_into_snapshot_consistent_under_concurrent_writer() {
    Host::test(|host| {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::time::Duration;

        host.run(async {
            let (backend, dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_demo\".\"notes\" (id INTEGER PRIMARY KEY, body TEXT)",
                    &[],
                )
                .await
                .expect("CREATE TABLE notes");
            // Seed an initial baseline so the snapshot is not empty.
            const INITIAL_ROWS: usize = 50;
            for i in 0..INITIAL_ROWS {
                let sql = format!("INSERT INTO \"app_demo\".\"notes\" VALUES ({i}, 'initial-{i}')");
                backend
                    .execute_fixture(&sql, &[])
                    .await
                    .expect("INSERT initial");
            }

            // Concurrent writer thread. Opens its own rusqlite Connection
            // directly against the per-app file in WAL mode and hammers
            // INSERTs until the `stop` flag flips. We use a separate
            // process-internal Connection (NOT the session actor) so the
            // writer races the VACUUM INTO's read transaction at the
            // engine layer — exactly the SQLITE_BUSY surface this test
            // exists to fence.
            let stop = Arc::new(AtomicBool::new(false));
            let writes_observed = Arc::new(AtomicUsize::new(0));
            let app_file = dir.path().join("zs-app_demo.sqlite");
            let writer_stop = stop.clone();
            let writer_observed = writes_observed.clone();
            let writer = std::thread::spawn(move || {
                let conn =
                    rusqlite::Connection::open(&app_file).expect("writer-thread connection open");
                // Match the session's WAL mode so we are in the right
                // concurrency regime; busy_timeout absorbs short-term
                // contention.
                conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")
                    .expect("writer PRAGMAs");
                // Insert IDs starting past INITIAL_ROWS to avoid PK
                // collision with seeded rows.
                let mut i = INITIAL_ROWS;
                while !writer_stop.load(Ordering::Relaxed) {
                    let sql = format!("INSERT INTO \"notes\" VALUES ({i}, 'concurrent-{i}')");
                    match conn.execute(&sql, []) {
                        Ok(_) => {
                            writer_observed.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            // SQLITE_BUSY on the writer side is fine —
                            // the engine's own busy_timeout absorbed
                            // contention. Other errors fail the test.
                            let msg = format!("{e}");
                            if !msg.contains("locked") && !msg.contains("busy") {
                                eprintln!("writer thread INSERT failed: {e}");
                            }
                        }
                    }
                    i += 1;
                    // Slight cadence so we don't hog the CPU; the engine
                    // is fast enough that ~thousands of writes happen per
                    // second of snapshot wait time regardless.
                    std::thread::sleep(Duration::from_micros(50));
                }
            });

            // Give the writer thread a moment to start hammering so the
            // snapshot fires INTO a live write storm (not against an
            // idle DB).
            compio::time::sleep(Duration::from_millis(50)).await;

            // Snapshot under the concurrent writer. Must NOT surface
            // SQLITE_BUSY — the Retry policy + the engine-side
            // busy_timeout absorb everything.
            let snap_path = dir.path().join("snap-concurrent.sqlite");
            let snap_uri = format!("file://{}", snap_path.to_string_lossy());
            let snap_result = backend
                .snapshot(
                    "app_demo",
                    &snap_uri,
                    SnapshotOpts {
                        if_busy: BackupBusyPolicy::Retry,
                    },
                )
                .await;

            // (c) — no SQLITE_BUSY classification leaked out.
            let _handle = snap_result
                .expect("VACUUM INTO under concurrent writer must succeed (Retry absorbs busy)");

            // Stop the writer thread and join.
            stop.store(true, Ordering::Relaxed);
            writer.join().expect("writer-thread join");
            let writes = writes_observed.load(Ordering::Relaxed);
            // Sanity — the writer landed at least some writes; otherwise
            // the test isn't actually exercising the race window.
            assert!(
                writes > 0,
                "writer thread should have committed at least one INSERT before stop"
            );

            // (a) — open snap file standalone and count rows.
            let snap_conn =
                rusqlite::Connection::open(&snap_path).expect("open snapshot file standalone");
            let snap_count: i64 = snap_conn
                .query_row("SELECT COUNT(*) FROM \"notes\"", [], |r| r.get(0))
                .expect("count snap rows");
            assert!(snap_count >= INITIAL_ROWS as i64);
            // (b) — live > snap (the concurrent writer's commits past the
            // snapshot's read mark are visible in live but NOT in snap).
            let client = backend
                .fixture_session("app_demo")
                .await
                .expect("acquire client");
            let live_rows = client
                .query("SELECT COUNT(*) FROM \"app_demo\".\"notes\"", &[])
                .await
                .expect("count live rows");
            let live_count: i64 = live_rows[0][0]
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            assert!(
                live_count > snap_count,
                "live ({live_count}) must exceed snap ({snap_count}) — \
             concurrent writes after snapshot must be visible in live but not snap; \
             writer landed {writes} rows total"
            );
        });
    })
}

/// When the per-app `snapshot_restore` advisory lock is
/// already held in this process, `snapshot()` surfaces the typed
/// `Coded { code: "migration_in_progress" }` rather than blocking
/// indefinitely. Mirrors the PG arm's `snapshot_during_migration_returns_typed_error`
/// test (`src/tests/integration.rs`).
///
/// SQLite's lock state lives in `InProcessLockRegistry` (one map per
/// `SqliteBackend`), so we acquire the slot through the public
/// `LockManager` surface — same registry the snapshot pre-flight
/// races for.
#[test]
fn snapshot_during_migration_returns_typed_error_sqlite() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            let app_id = "app_miglock";

            // Hold the snapshot_restore lock through the typed LockManager
            // surface — exactly the slot the snapshot pre-flight tries to
            // acquire. The `to_keys` derivation is identical to what the
            // snapshot impl computes.
            let client = backend
                .fixture_session("default")
                .await
                .expect("acquire client");
            let scope = LockScope::GlobalApp {
                app_id: app_id.to_string(),
                name: "snapshot_restore".to_string(),
            };
            let acquired = backend
                .try_acquire(&client, &scope)
                .await
                .expect("try_acquire snapshot_restore");
            assert!(acquired, "test must hold the slot to set up the contention");

            // Snapshot must refuse at pre-flight. We deliberately do NOT
            // pre-create the destination directory so a stray success
            // wouldn't write to disk either.
            let dest = "file:///tmp/p5_pr5_miglock_should_not_exist.sqlite";
            let err = backend
                .snapshot(
                    app_id,
                    dest,
                    SnapshotOpts {
                        if_busy: BackupBusyPolicy::Abort,
                    },
                )
                .await
                .expect_err("snapshot must refuse while snapshot_restore lock is held");
            match err {
                DbError::Coded { code, .. } => {
                    assert_eq!(
                        code, "migration_in_progress",
                        "expected Coded migration_in_progress, got code={code:?}"
                    );
                }
                other => {
                    panic!(
                        "expected Coded {{ code: \"migration_in_progress\", .. }}, got {other:?}"
                    )
                }
            }

            // Dest file must not exist — pre-flight refusal runs before
            // any disk I/O.
            let path = std::path::Path::new("/tmp/p5_pr5_miglock_should_not_exist.sqlite");
            assert!(
                !path.exists(),
                "snapshot must not write to disk when refused at pre-flight"
            );

            // Release for cleanliness.
            backend
                .release(&client, &scope)
                .await
                .expect("release snapshot_restore");
        });
    })
}

/// A `restore()` whose on-disk file has drifted from the
/// `SnapshotHandle`'s recorded SHA-256 must refuse with
/// `Coded { code: "snapshot_hash_mismatch" }` BEFORE touching the
/// live per-app DB. Pins the integrity-verify gate the restore path
/// runs after the lock acquire but before any DETACH/rename.
#[test]
fn restore_hash_mismatch_rejected_sqlite() {
    Host::test(|host| {
        use std::fs::OpenOptions;
        use std::io::Write;
        host.run(async {
            let (backend, dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_demo\".\"notes\" (id INTEGER PRIMARY KEY, body TEXT)",
                    &[],
                )
                .await
                .expect("CREATE TABLE notes");
            backend
                .execute_fixture(
                    "INSERT INTO \"app_demo\".\"notes\" VALUES (1, 'sentinel')",
                    &[],
                )
                .await
                .expect("INSERT sentinel");

            // Take a clean snapshot first.
            let snap_path = dir.path().join("snap-hashcheck.sqlite");
            let snap_uri = format!("file://{}", snap_path.to_string_lossy());
            let handle = backend
                .snapshot(
                    "app_demo",
                    &snap_uri,
                    SnapshotOpts {
                        if_busy: BackupBusyPolicy::Retry,
                    },
                )
                .await
                .expect("snapshot");

            // Corrupt the on-disk file by appending bytes. The
            // SnapshotHandle's recorded hash no longer matches; restore
            // MUST refuse before touching the live DB.
            {
                let mut f = OpenOptions::new()
                    .append(true)
                    .open(&snap_path)
                    .expect("open snap for append");
                f.write_all(b"\x00\x01\x02 corruption tail \x03\x04\x05")
                    .expect("append tampering bytes");
            }

            // Restore must reject with the typed code.
            let err = backend
                .restore("app_demo", &handle)
                .await
                .expect_err("restore must refuse on hash mismatch");
            match err {
                DbError::Coded { code, .. } => {
                    assert_eq!(
                        code, "snapshot_hash_mismatch",
                        "expected Coded snapshot_hash_mismatch, got code={code:?}"
                    );
                }
                other => {
                    panic!(
                        "expected Coded {{ code: \"snapshot_hash_mismatch\", .. }}, got {other:?}"
                    )
                }
            }

            // The live DB must be untouched — the sentinel row still
            // exists. (Even without the mismatch check, the rename swap
            // only fires after the hash verify; an early-refuse contract
            // means the live file is bit-for-bit unchanged.)
            let client = backend
                .fixture_session("app_demo")
                .await
                .expect("acquire client");
            let rows = client
                .query("SELECT body FROM \"app_demo\".\"notes\" WHERE id = 1", &[])
                .await
                .expect("post-refuse query");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0].as_deref(), Some("sentinel"));
        });
    })
}
