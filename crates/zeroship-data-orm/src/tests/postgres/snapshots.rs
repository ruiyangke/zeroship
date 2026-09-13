//! PostgreSQL snapshots contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;

use compio_postgres::{NoTls, Pool};

use zeroship_data_orm::error::DbError;

use zeroship_data_orm::backend::{
    Backup as _, BusyPolicy as BackupBusyPolicy, LockScope, SnapshotOpts,
};

/// Refuse the run unless `tool` answers `--version` on PATH.
///
/// The callers used to `#[ignore]` themselves statically, so this refusal was
/// reachable only from a run that passed `--ignored` - and nothing in this
/// repository passes it. The attribute therefore did not defer the check, it
/// deleted the tests from every job that could have run them, which is the same
/// silent green the refusal exists to prevent.
///
/// It takes the binary NAME because restore needs `pg_restore` as well as
/// `pg_dump`, and a probe of only the first reports a machine as ready when the
/// round-trip's second half cannot run.
///
/// # Panics
///
/// When `tool` is absent, naming the packages that carry it.
fn require_pg_client_tool(tool: &str) {
    let answered = std::process::Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    assert!(
        answered,
        "`{tool}` is not on PATH, and this test requires it.\n\
         \n\
         \x20 backend: the PostgreSQL CLIENT tools, in this process's PATH\n\
         \x20 probe:   `{tool} --version` did not succeed\n\
         \n\
         This is a LOCAL binary, not the server: a reachable database does not\n\
         supply it, and the container-hosted server this suite talks to has it\n\
         inside the container where this process cannot reach it. CI installs\n\
         matching clients; local runs must also put them on PATH.\n\
         \n\
         Install the client package for your system - `postgresql-client` on\n\
         Debian and Ubuntu, `postgresql` on Fedora and Arch, `postgresql@16` in\n\
         Homebrew, or the `postgresql` package in a nix shell - then check both\n\
         binaries, because restore needs the second:\n\
         \x20 pg_dump --version\n\
         \x20 pg_restore --version\n\
         \n\
         Use client tools matching the server major version. Older dump tools\n\
         refuse newer servers, while newer dumps may emit settings an older\n\
         restore server does not recognize.\n\
         \n\
         There is no environment variable and no attribute that makes this a\n\
         skip."
    );
}

/// Fence: when the per-app `snapshot_restore` advisory
/// lock is held by another caller, `snapshot()` surfaces a typed
/// `Coded { code: "migration_in_progress" }` rather than blocking
/// indefinitely or returning an opaque LockContention. Pins the
/// pre-flight interlock the snapshot impl runs before invoking
/// `pg_dump`.
#[test]
fn snapshot_during_migration_returns_typed_error() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let backend = zeroship_data_orm::backend::PostgresBackend::new(
                pool.clone(),
                url.clone(),
                host.key_source(),
            );
            let app_id = crate::tests::fixtures::test_app_id!();
            let app_id = app_id.as_str();

            // Acquire the snapshot_restore lock on a dedicated standalone
            // connection (not a pooled client) so the lock is held for the
            // entire test without competing with the pool. The lock is
            // session-scoped, so it auto-releases when this client drops at
            // end-of-scope. We don't go through `LockGuard` because that
            // type is `pub(crate)` and unreachable from integration tests.
            let (lock_client, lock_conn) = compio_postgres::connect(&url, NoTls)
                .await
                .expect("hold-lock dedicated connect");
            let lock_conn_task = compio::runtime::spawn(async move {
                let _ = lock_conn.run().await;
            });
            // Mirror `LockScope::GlobalApp { app_id, name: "snapshot_restore" }
            // .to_keys()` exactly so the underlying `(key1, key2)` pair
            // matches what the snapshot's pre-flight will try to acquire.
            let scope = LockScope::GlobalApp {
                app_id: app_id.to_string(),
                name: "snapshot_restore".to_string(),
            };
            let (key1, key2) = scope.to_keys();
            lock_client
                .query_text_params(
                    "SELECT pg_advisory_lock(hashtext($1)::int4, hashtext($2)::int4)",
                    &[key1.as_str(), key2.as_str()],
                )
                .await
                .expect("acquire snapshot_restore lock on dedicated session");

            // Snapshot dest URI doesn't need to be real — we expect the
            // call to refuse at the pre-flight stage, before pg_dump runs.
            let dest = "file:///tmp/p5_pr4_miglock_should_not_exist.dump";
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
                other => panic!(
                    "expected Coded {{ code: \"migration_in_progress\", .. }}, got {other:?}"
                ),
            }

            // The destination file MUST NOT have been created — the
            // pre-flight refusal runs before any disk I/O.
            let path = std::path::Path::new("/tmp/p5_pr4_miglock_should_not_exist.dump");
            assert!(
                !path.exists(),
                "snapshot must not write to disk when refused at pre-flight"
            );

            // Drop the dedicated client; PG releases the session-scoped
            // advisory lock when the backend session terminates.
            drop(lock_client);
            lock_conn_task.detach();
            drop(backend);
            release_pg(host, pool).await;
        })
    })
}

/// Gate #1: round-trip snapshot+restore. Insert rows
/// into a per-app schema, snapshot to a `file://` URI, drop the
/// schema's table contents, restore, assert the rows are back.
///
/// Needs `pg_dump` AND `pg_restore` on PATH; a machine without them fails here
/// naming the package that carries them, rather than reporting a round-trip it
/// never performed.
#[test]
fn snapshot_restore_round_trip_pg() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            require_pg_client_tool("pg_dump");
            require_pg_client_tool("pg_restore");
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            // Per-app schema fresh every run.
            let app_id = crate::tests::fixtures::test_app_id!();
            let app_id = app_id.as_str();
            pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app_id}\" CASCADE"), &[])
                .await
                .unwrap();
            pool.execute(&format!("CREATE SCHEMA \"{app_id}\""), &[])
                .await
                .unwrap();
            pool.execute(
                &format!(
                    r#"CREATE TABLE "{app_id}"."notes" (
                id   INTEGER PRIMARY KEY,
                body TEXT NOT NULL
            )"#
                ),
                &[],
            )
            .await
            .unwrap();

            // Seed deterministic rows. Bind both columns as text — the
            // `$1::int` cast on the SQL side mirrors the `app_role` /
            // `users` test pattern used throughout this file.
            const ROW_COUNT: usize = 5;
            for i in 0..ROW_COUNT {
                let id_s = i.to_string();
                let body = format!("row-{i}");
                pool.query_text_params(
                    &format!(r#"INSERT INTO "{app_id}"."notes" (id, body) VALUES ($1::int, $2)"#),
                    &[id_s.as_str(), body.as_str()],
                )
                .await
                .unwrap();
            }

            let backend = zeroship_data_orm::backend::PostgresBackend::new(
                pool.clone(),
                url.clone(),
                host.key_source(),
            );

            // Snapshot to a tempdir-backed file:// URI.
            let dir = tempfile::tempdir().unwrap();
            let dest_path = dir.path().join("snapshot.dump");
            let dest_uri = format!("file://{}", dest_path.to_string_lossy());

            let handle = backend
                .snapshot(
                    app_id,
                    &dest_uri,
                    SnapshotOpts {
                        if_busy: BackupBusyPolicy::Abort,
                    },
                )
                .await
                .expect("snapshot");
            assert!(
                dest_path.exists(),
                "dump file must exist on disk after snapshot"
            );
            assert_eq!(handle.uri, dest_uri);

            // Drop-and-recreate to a clean schema (simulates data loss).
            pool.execute(&format!("DROP SCHEMA \"{app_id}\" CASCADE"), &[])
                .await
                .unwrap();
            pool.execute(&format!("CREATE SCHEMA \"{app_id}\""), &[])
                .await
                .unwrap();
            let rows = pool
                .query_text_params(
                    "SELECT 1 FROM pg_tables WHERE schemaname = $1 AND tablename = 'notes'",
                    &[app_id],
                )
                .await
                .unwrap();
            assert!(rows.is_empty(), "post-drop: notes table must be absent");

            // Restore — the impl re-drops/recreates the schema itself, then
            // runs pg_restore over the captured dump file.
            backend.restore(app_id, &handle).await.expect("restore");

            // Verify the row set is recovered. Cast id to text on the
            // server so `Row::get<String>` decodes uniformly without
            // dragging in the `query_text_params` int-decode shape.
            let rows = pool
                .query_text_params(
                    &format!(r#"SELECT id::text AS id, body FROM "{app_id}"."notes" ORDER BY id"#),
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(rows.len(), ROW_COUNT, "all rows must be recovered");
            for (i, row) in rows.iter().enumerate() {
                let id: String = row.get::<_, String>("id");
                assert_eq!(id, i.to_string());
                let body: String = row.get::<_, String>("body");
                assert_eq!(body, format!("row-{i}"));
            }

            // Cleanup so a re-run starts fresh.
            pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app_id}\" CASCADE"), &[])
                .await
                .unwrap();
            drop(backend);
            release_pg(host, pool).await;
        })
    })
}

/// Fence: the `SnapshotHandle.content_hash` returned by
/// `snapshot()` must equal the SHA-256 of the on-disk dump bytes.
/// This is the integrity contract the `restore()` path relies on —
/// any drift here would let a corrupt dump pass restore's hash
/// check.
///
/// Needs `pg_dump` on PATH, and says so by failing rather than by vanishing
/// from the run.
#[test]
fn snapshot_uri_content_hash_round_trip() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            require_pg_client_tool("pg_dump");
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let app_id = crate::tests::fixtures::test_app_id!();
            let app_id = app_id.as_str();
            pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app_id}\" CASCADE"), &[])
                .await
                .unwrap();
            pool.execute(&format!("CREATE SCHEMA \"{app_id}\""), &[])
                .await
                .unwrap();
            pool.execute(
                &format!(r#"CREATE TABLE "{app_id}"."t" (id INT PRIMARY KEY)"#),
                &[],
            )
            .await
            .unwrap();
            pool.execute(
                &format!(r#"INSERT INTO "{app_id}"."t" (id) VALUES (1), (2), (3)"#),
                &[],
            )
            .await
            .unwrap();

            let backend = zeroship_data_orm::backend::PostgresBackend::new(
                pool.clone(),
                url.clone(),
                host.key_source(),
            );
            let dir = tempfile::tempdir().unwrap();
            let dest_path = dir.path().join("hash_check.dump");
            let dest_uri = format!("file://{}", dest_path.to_string_lossy());

            let handle = backend
                .snapshot(
                    app_id,
                    &dest_uri,
                    SnapshotOpts {
                        if_busy: BackupBusyPolicy::Abort,
                    },
                )
                .await
                .expect("snapshot");

            // Recompute SHA-256 over the on-disk file via an independent
            // implementation so the assertion pins the byte format.
            use sha2::Digest;
            let bytes = std::fs::read(&dest_path).expect("read dump file");
            let observed: [u8; 32] = sha2::Sha256::digest(&bytes).into();
            assert_eq!(
                handle.content_hash, observed,
                "SnapshotHandle.content_hash must match SHA-256 of on-disk bytes"
            );

            // Cleanup.
            pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app_id}\" CASCADE"), &[])
                .await
                .unwrap();
            drop(backend);
            release_pg(host, pool).await;
        })
    })
}
