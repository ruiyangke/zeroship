//! Prove the platform-schema migrate path on the PUBLISHED
//! engine: every `db/migrations-ts/*.ts` file AUTHORS (zeroship-runtime V8 + the
//! standalone v1 recorder) and LOWERS (fail-closed load gate + Platform-guarded
//! lower) cleanly on the standalone (v1) engine.
//!
//! The author+lower half needs NO database and NO privileged executor seam, so it
//! runs UNCONDITIONALLY — it is the maximal claim the published engine's PUBLIC API
//! lets a monorepo consumer prove today.
//!
//! The APPLY half runs on the published engine's PUBLIC token-gated Platform seam
//! (`ExecutorConfig::platform`). `apply_all_platform_migrations_to_fresh_db`
//! applies every migration to a FRESH scratch database on :5440 and
//! INDEPENDENTLY asserts (via a second connection) that the expected platform
//! schemas/tables/RLS-policy/function+trigger/grants landed. Gated on
//! a test database (set `PG_TEST_URL`; skips clean when unset).

#[cfg(feature = "platform-cli")]
// `DB_APPLY_LOCK` is held across the awaits of every live-PG test below, on
// purpose, and this is the one topology in which that is not the bug the lint
// describes.
//
// The lint fires when a `MutexGuard` outlives an await point, because on a
// multi-task executor the holder can yield to a task that wants the same lock
// and neither can proceed. Neither half of that applies here:
//
//   - `#[compio::test]` expands to a plain `#[test]` fn that builds its OWN
//     `compio::runtime::Runtime` and `block_on`s the body (compio-macros
//     test_fn.rs). One runtime per test, one test per harness thread, and the
//     future is driven to completion on the thread that created it - a
//     `MutexGuard` is `!Send`, so it could not be otherwise.
//   - No OTHER task in that runtime takes this lock. The only contenders are
//     the harness's other test threads, which is exactly what the lock is for:
//     these tests `CREATE ROLE` in the cluster-global `pg_authid` catalog and
//     PG aborts one of two concurrent creators with "tuple concurrently
//     updated" (see DB_APPLY_LOCK's own comment). Serializing THREADS is the
//     point, and blocking a thread that has nothing else to run costs nothing.
//
// `concurrent_migrates_creating_the_same_roles_both_succeed` is the case worth
// checking against this: it holds the guard and spawns a second OS thread with
// its own runtime, but that thread never takes the lock, so there is no cycle.
//
// The suppression is module-scope with this reason rather than eleven
// attributes, and clippy.toml's "only permitted local suppressions" rule is
// about `clippy::disallowed_methods` and the raw-environment boundary, not this
// lint.
#[allow(clippy::await_holding_lock)]
mod platform_cli {
    use std::path::{Path, PathBuf};

    use std::sync::Mutex;

    use zero_migrate::driver::SqlSession;
    use zeroship_migrate_adapter::platform::{
        author_and_lower_all, run_platform_migrations, PlatformMigrateConfig, PlatformMigrateError,
        PLATFORM_MIGRATION_LEDGER_TABLE,
    };
    use zeroship_migrate_adapter::platform::cluster_lock::DEFAULT_CLUSTER_LOCK_DATABASE;
    use zeroship_migrate_adapter::CompioPgSession;

    /// Serialize the live-PG apply tests. Some provision a scratch DB and create
    /// the platform's CLUSTER-GLOBAL roles (`CREATE ROLE zeroship_control`, …); run in
    /// parallel they race on the shared `pg_authid` catalog and PG aborts one with
    /// `tuple concurrently updated`. `cargo test` runs test fns on multiple OS threads
    /// by default, so this process-wide lock (not `--test-threads=1`) keeps the DB
    /// apply tests from overlapping regardless of the caller's thread count. The
    /// DB-free author+lower test does not take it.
    static DB_APPLY_LOCK: Mutex<()> = Mutex::new(());

    /// Every `.ts` filename in `db/migrations-ts`, in the order the runner
    /// applies them.
    ///
    /// THIS REPLACED A MAINTAINED CONSTANT (`PLATFORM_MIGRATION_FILES: usize =
    /// 34`) whose problem was that it was derived by counting the very files it
    /// then checked. A commit deleting one migration and adding another left 34
    /// reading 34, so the one corpus change worth catching was the one it could
    /// not see. Measured: with the constant, delete
    /// `20260817000500_drop_net_policy_catalog.ts` and add any replacement and
    /// every count assertion still passes.
    ///
    /// The two jobs the constant was doing are now split, and neither is a
    /// count that maintains itself:
    ///
    ///   - "the ENGINE dropped a file the directory has" is asserted by NAME in
    ///     `all_platform_migrations_author_and_lower_on_standalone_engine`,
    ///     comparing the engine's enumeration against this one. Two independent
    ///     enumerations disagreeing names the file; two counts agreeing proves
    ///     nothing.
    ///   - "a file that a deployed database applied went missing" is asserted
    ///     by `released_platform_migrations_all_exist_in_the_corpus`, whose
    ///     authority is production's journal rather than a number somebody
    ///     typed. That is what catches the delete-one-add-one above, and it
    ///     names the file.
    ///
    /// What neither covers: deleting a migration NO deployed database has
    /// applied. Nothing in the repo knows such a file was ever meant to exist,
    /// and the old constant only "caught" it by being edited in the same
    /// commit.
    fn platform_migration_filenames() -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(migrations_dir())
            .expect("read platform migration corpus")
            .map(|entry| entry.expect("read platform migration entry").file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".ts"))
            .collect();
        names.sort();
        names
    }

    /// How many files `db/migrations-ts` holds.
    fn platform_migration_files() -> usize {
        platform_migration_filenames().len()
    }

    const DURABLE_WORKFLOW_JOURNAL_TABLES: [&str; 10] = [
        "app_deploys",
        "workflow_runs",
        "workflow_blobs",
        "workflow_signal_keys",
        "workflow_broadcasts",
        "workflow_steps",
        "workflow_signals",
        "workflow_subscriptions",
        "workflow_schedules",
        "workflow_rollout_config",
    ];

    /// The repo-root `db/migrations-ts` directory (the crate is two levels below).
    fn migrations_dir() -> PathBuf {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        manifest
            .ancestors()
            .nth(2)
            .expect("repo root two levels above the crate")
            .join("db")
            .join("migrations-ts")
    }

    fn copy_migration_corpus(destination: &Path) -> Result<(), String> {
        for entry in std::fs::read_dir(migrations_dir())
            .map_err(|e| format!("read platform migration corpus: {e}"))?
        {
            let entry = entry.map_err(|e| format!("read platform migration entry: {e}"))?;
            let source = entry.path();
            if source.extension().and_then(|ext| ext.to_str()) != Some("ts") {
                continue;
            }
            let filename = source
                .file_name()
                .ok_or_else(|| format!("migration path has no filename: {}", source.display()))?;
            std::fs::copy(&source, destination.join(filename))
                .map_err(|e| format!("copy {}: {e}", source.display()))?;
        }
        Ok(())
    }

    /// Copy exactly the NAMED migration files into `destination`.
    ///
    /// By name rather than by position, so a corpus whose filename order is not
    /// the deploy order cannot make the caller seed the wrong set. A name that
    /// is not in the corpus is an error rather than a silently smaller copy --
    /// the caller is reconstructing a deployed database's state and a missing
    /// file changes what it is reconstructing.
    fn copy_migration_files(destination: &Path, names: &[&str]) -> Result<(), String> {
        let dir = migrations_dir();
        for name in names {
            let source = dir.join(name);
            std::fs::copy(&source, destination.join(name))
                .map_err(|e| format!("copy {}: {e}", source.display()))?;
        }
        Ok(())
    }

    fn copy_migration_prefix(destination: &Path, count: usize) -> Result<(), String> {
        let mut sources = std::fs::read_dir(migrations_dir())
            .map_err(|e| format!("read platform migration corpus: {e}"))?
            .map(|entry| {
                entry
                    .map(|entry| entry.path())
                    .map_err(|e| format!("read platform migration entry: {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        sources.retain(|path| path.extension().and_then(|ext| ext.to_str()) == Some("ts"));
        sources.sort();
        if sources.len() < count {
            return Err(format!(
                "platform migration corpus has {} files, cannot copy a prefix of {count}",
                sources.len()
            ));
        }
        for source in sources.into_iter().take(count) {
            let filename = source
                .file_name()
                .ok_or_else(|| format!("migration path has no filename: {}", source.display()))?;
            std::fs::copy(&source, destination.join(filename))
                .map_err(|e| format!("copy {}: {e}", source.display()))?;
        }
        Ok(())
    }

    fn write_migration(directory: &Path, filename: &str, source: &str) -> Result<PathBuf, String> {
        let path = directory.join(filename);
        std::fs::write(&path, source)
            .map_err(|e| format!("write temporary migration {}: {e}", path.display()))?;
        Ok(path)
    }

    fn pg_url() -> Option<String> {
        zeroship_core::config::test_database_url_opt()
    }

    /// DB-FREE proof: EVERY platform `.ts` migration authors (V8, v1 recorder) +
    /// lowers (Platform-guarded, fail-closed load gate) on the standalone engine.
    /// This is the whole DSL/op-support proof — if any file used an op the
    /// published v1 engine could not author or lower, this fails with the exact
    /// file + op.
    #[test]
    fn all_platform_migrations_author_and_lower_on_standalone_engine() {
        let dir = migrations_dir();
        let lowered = author_and_lower_all(&dir, "zeroship")
            .expect("every platform .ts must author + lower on the published v1 engine");

        // Identity, not arity -- see `platform_migration_filenames`. The engine
        // enumerated `lowered`; `platform_migration_filenames` reads the
        // directory. A file the engine silently dropped is named here.
        let lowered_names: Vec<String> = lowered.iter().map(|(f, _)| f.clone()).collect();
        let mut sorted_lowered = lowered_names.clone();
        sorted_lowered.sort();
        assert_eq!(
            sorted_lowered,
            platform_migration_filenames(),
            "the engine's enumeration of db/migrations-ts does not match the \
             directory's; engine saw {:?}",
            lowered_names
        );

        // The first file (schema/roles/extensions/domains/sequences) must lower to a
        // non-empty plan — proof the widened Platform ops (CREATE SCHEMA/ROLE/DOMAIN/
        // EXTENSION/SEQUENCE) authored + lowered, not silently dropped.
        let (first_name, first) = &lowered[0];
        assert!(
            first_name.contains("schema_roles_extensions"),
            "first file is schema_roles_extensions, got {first_name}"
        );
        assert!(
            !first.plan.steps.is_empty(),
            "schema_roles_extensions must lower to real plan steps"
        );

        // The functions/triggers file must lower its createFunction + trigger +
        // comment ops to a non-empty plan.
        let funcs = lowered
            .iter()
            .find(|(f, _)| f.contains("functions_triggers_comments"))
            .expect("functions_triggers_comments present");
        assert!(
            !funcs.1.plan.steps.is_empty(),
            "functions/triggers/comments must lower to real plan steps"
        );

        // The RLS policies file must lower its setRls + policy ops.
        let rls = lowered
            .iter()
            .find(|(f, _)| f.contains("policies_rls"))
            .expect("policies_rls present");
        assert!(
            !rls.1.plan.steps.is_empty(),
            "RLS policies must lower to real plan steps"
        );

        // The grants file must lower its grant/revoke/dropFunction ops.
        let grants = lowered
            .iter()
            .find(|(f, _)| f.contains("grants"))
            .expect("grants present");
        assert!(
            !grants.1.plan.steps.is_empty(),
            "grants/revokes must lower to real plan steps"
        );
    }

    /// Rewrite a DSN's database name to `new_db` (path segment after the host/port).
    /// Handles the common `postgres://user:pass@host:port/dbname[?params]` shape.
    fn dsn_with_db(url: &str, new_db: &str) -> String {
        let (head, rest) = match url.split_once("://") {
            Some((scheme, after)) => (format!("{scheme}://"), after),
            None => (String::new(), url),
        };
        // Split off any query string; re-attach after swapping the db segment.
        let (authority_and_path, query) = match rest.split_once('?') {
            Some((a, q)) => (a, Some(q)),
            None => (rest, None),
        };
        let authority = match authority_and_path.split_once('/') {
            Some((auth, _db)) => auth,
            None => authority_and_path,
        };
        let mut out = format!("{head}{authority}/{new_db}");
        if let Some(q) = query {
            out.push('?');
            out.push_str(q);
        }
        out
    }

    /// An admin session on the server's maintenance DB (`postgres`), for
    /// CREATE/DROP DATABASE of the scratch DB. Derived from the test DSN by
    /// swapping its database segment.
    async fn admin_session(url: &str) -> CompioPgSession {
        let admin_dsn = dsn_with_db(url, "postgres");
        CompioPgSession::connect(&admin_dsn)
            .await
            .expect("connect admin session (postgres maintenance DB)")
    }

    struct ScratchDatabase {
        name: String,
        dsn: String,
    }

    async fn create_scratch_database(url: &str, prefix: &str) -> Result<ScratchDatabase, String> {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| format!("read system clock: {e}"))?
            .as_nanos();
        let name = format!("{prefix}_{suffix}");
        let dsn = dsn_with_db(url, &name);
        let admin = admin_session(url).await;
        let _ = admin
            .batch(&format!("DROP DATABASE IF EXISTS \"{name}\""))
            .await;
        admin
            .batch(&format!("CREATE DATABASE \"{name}\""))
            .await
            .map_err(|e| format!("CREATE DATABASE {name}: {e}"))?;
        Ok(ScratchDatabase { name, dsn })
    }

    async fn drop_scratch_database(url: &str, scratch: &ScratchDatabase) -> Result<(), String> {
        let admin = admin_session(url).await;
        let _ = admin
            .batch(&format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE datname = '{}' AND pid <> pg_backend_pid()",
                scratch.name
            ))
            .await;
        admin
            .batch(&format!("DROP DATABASE IF EXISTS \"{}\"", scratch.name))
            .await
            .map_err(|e| format!("DROP DATABASE {}: {e}", scratch.name))
    }

    /// A single scalar-bool probe over the seam (the independent `psql`-equivalent
    /// assertion path — a SECOND connection, distinct from the migrate run).
    async fn scalar_bool(session: &CompioPgSession, sql: &str) -> bool {
        let row = session
            .client()
            .query_one(sql, &[])
            .await
            .unwrap_or_else(|e| panic!("probe `{sql}` failed: {e}"));
        row.try_get::<_, bool>(0).expect("decode bool probe")
    }

    /// The count of COMPLETED journal rows in the platform migration journal
    /// (`zeroship_migrations.schema_migrations`) — the idempotency witness. A
    /// re-runnable one-shot must leave this UNCHANGED across a re-run (no new rows,
    /// no duplicates).
    async fn journal_completed_count(session: &CompioPgSession) -> i64 {
        let row = session
            .client()
            .query_one(
                "SELECT count(*)::bigint FROM zeroship_migrations.schema_migrations \
                 WHERE phase = 'completed'",
                &[],
            )
            .await
            .expect("count platform journal rows");
        row.try_get::<_, i64>(0).expect("decode journal count")
    }

    async fn ledger_row_count(session: &CompioPgSession) -> i64 {
        let row = session
            .client()
            .query_one(
                &format!(
                    "SELECT count(*)::bigint FROM zeroship_migrations.{}",
                    PLATFORM_MIGRATION_LEDGER_TABLE
                ),
                &[],
            )
            .await
            .expect("count platform file ledger rows");
        row.try_get::<_, i64>(0)
            .expect("decode platform file ledger count")
    }

    async fn ledger_has_file(session: &CompioPgSession, filename: &str) -> bool {
        let row = session
            .client()
            .query_one(
                &format!(
                    "SELECT EXISTS (SELECT 1 FROM zeroship_migrations.{} \
                     WHERE filename = $1)",
                    PLATFORM_MIGRATION_LEDGER_TABLE
                ),
                &[&filename],
            )
            .await
            .expect("probe platform file ledger row");
        row.try_get::<_, bool>(0)
            .expect("decode platform file ledger probe")
    }

    async fn table_exists(session: &CompioPgSession, table: &str) -> bool {
        let row = session
            .client()
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
                 WHERE table_schema = 'zeroship' AND table_name = $1)",
                &[&table],
            )
            .await
            .expect("probe platform table");
        row.try_get::<_, bool>(0)
            .expect("decode platform table probe")
    }

    async fn column_exists(session: &CompioPgSession, table: &str, column: &str) -> bool {
        let row = session
            .client()
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
                 WHERE table_schema = 'zeroship' AND table_name = $1 AND column_name = $2)",
                &[&table, &column],
            )
            .await
            .expect("probe platform table column");
        row.try_get::<_, bool>(0)
            .expect("decode platform table column probe")
    }

    async fn foreign_key_targets(
        session: &CompioPgSession,
        constraint: &str,
        child_table: &str,
        parent_table: &str,
    ) -> bool {
        let row = session
            .client()
            .query_one(
                "SELECT EXISTS (\
                     SELECT 1 FROM pg_constraint fk \
                     JOIN pg_class child ON child.oid = fk.conrelid \
                     JOIN pg_namespace child_ns ON child_ns.oid = child.relnamespace \
                     JOIN pg_class parent ON parent.oid = fk.confrelid \
                     JOIN pg_namespace parent_ns ON parent_ns.oid = parent.relnamespace \
                     WHERE fk.contype = 'f' AND fk.conname = $1 \
                       AND child_ns.nspname = 'zeroship' AND child.relname = $2 \
                       AND parent_ns.nspname = 'zeroship' AND parent.relname = $3\
                 )",
                &[&constraint, &child_table, &parent_table],
            )
            .await
            .expect("probe platform foreign key");
        row.try_get::<_, bool>(0)
            .expect("decode platform foreign-key probe")
    }

    /// PROVE the apply path end to end: apply EVERY platform migration to a FRESH
    /// scratch database on :5440 via the real `run_platform_migrations` path
    /// (zeroship-runtime V8 author → zero-migrate Platform lower+apply over the
    /// native compio seam), then INDEPENDENTLY assert (a SECOND connection) that the
    /// expected platform schema + tables + an RLS policy + a function/trigger + the
    /// grants landed. Gated on a test database (set `PG_TEST_URL`; skips clean when unset).
    /// The scratch DB is created and dropped by this test.
    #[compio::test]
    async fn apply_all_platform_migrations_to_fresh_db() {
        let Some(url) = pg_url() else {
            zeroship_test_support::skip(
                "skipping the full-apply proof: no test database (set PG_TEST_URL \
                 to a DSN on :5440 to run)"
            );
            return;
        };
        // Serialize against the sibling DB-apply test (shared cluster-global roles).
        let _serial = DB_APPLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // A unique scratch DB name per run (PG identifiers: lowercase, no dashes).
        let scratch = format!(
            "zs_stage4a_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let scratch_dsn = dsn_with_db(&url, &scratch);

        // ── provision the FRESH scratch DB via the admin (maintenance) session ──
        {
            let admin = admin_session(&url).await;
            let _ = admin
                .batch(&format!("DROP DATABASE IF EXISTS \"{scratch}\""))
                .await;
            admin
                .batch(&format!("CREATE DATABASE \"{scratch}\""))
                .await
                .unwrap_or_else(|e| panic!("CREATE DATABASE scratch failed: {e}"));
        }

        // Run the migrate + assertions inside a closure so we always DROP the
        // scratch DB afterwards, even on a panic-free early failure return.
        let result = run_and_assert(&scratch_dsn).await;

        // ── teardown: drop the scratch DB (terminate any lingering backends) ──
        {
            let admin = admin_session(&url).await;
            let _ = admin
                .batch(&format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                     WHERE datname = '{scratch}' AND pid <> pg_backend_pid()"
                ))
                .await;
            admin
                .batch(&format!("DROP DATABASE IF EXISTS \"{scratch}\""))
                .await
                .expect("DROP DATABASE scratch");
        }

        result.expect("full apply + assertions must pass");
    }

    /// Apply every migration to `scratch_dsn` and independently assert the schema.
    /// Returns `Ok(())` on full success; the caller drops the scratch DB regardless.
    async fn run_and_assert(scratch_dsn: &str) -> Result<(), String> {
        // ── APPLY: the real platform-migrate path (V8 author → Platform apply) ──
        let cfg = PlatformMigrateConfig {
            database_url: scratch_dsn.to_string(),
            migrations_dir: migrations_dir(),
            project_schema: "zeroship".to_string(),
            project_id: "zeroship".to_string(),
            cluster_lock_database: DEFAULT_CLUSTER_LOCK_DATABASE.to_string(),
        };
        let report = run_platform_migrations(&cfg)
            .await
            .map_err(|e| format!("run_platform_migrations failed: {e}"))?;
        let expected_files = platform_migration_files();
        if report.files != expected_files {
            return Err(format!("expected {expected_files} files, saw {}", report.files));
        }
        if report.applied.is_empty() {
            return Err("no migrations were applied".to_string());
        }

        // ── ASSERT: a SECOND connection independently verifies the schema ──
        let probe = CompioPgSession::connect(scratch_dsn)
            .await
            .map_err(|e| format!("connect assertion probe: {e}"))?;

        // (1) the primary platform schema exists.
        if !scalar_bool(
            &probe,
            "SELECT EXISTS (SELECT 1 FROM information_schema.schemata \
             WHERE schema_name = 'zeroship')",
        )
        .await
        {
            return Err("schema 'zeroship' missing".to_string());
        }

        // (1a) no index name is at or past PostgreSQL's 63-byte identifier limit.
        //
        // Verified against the PostgreSQL this suite runs on. Two over-long index
        // names created with `IF NOT EXISTS` both report success, emit only a
        // NOTICE, and leave ONE index behind. The constraint spelling of the same
        // collision errors instead, so it is the index spelling that fails
        // silently. A single over-long name of either spelling is truncated with
        // only a NOTICE, leaving the authored name and the catalog name disagreeing.
        //
        // This reads REALIZED names rather than authored ops, because the authored
        // name is exactly what stops corresponding to reality once it truncates.
        //
        // The two arms below guard different failures, verified against the
        // PostgreSQL this suite runs on:
        //
        //   - Index names, and the names of UNIQUE and PRIMARY KEY constraints,
        //     share ONE per-schema relation namespace. A truncated name on one
        //     table can therefore collide with a truncated name on another.
        //   - CHECK and FOREIGN KEY names live only in pg_constraint and are unique
        //     per TABLE. They cannot collide with an index, and two tables may hold
        //     the same name happily, so for these the risk is purely that the
        //     authored name and the catalog name diverge.
        //
        // Divergence alone is enough to matter, and this is observed rather than
        // reasoned: the engine maintainers ran it against a live PostgreSQL. A
        // guarded drop probes the AUTHORED 64-byte name, the catalog holds the
        // truncated 63-byte one, the probe concludes the object is already gone,
        // the executor skips the statement, and the journal records it Completed.
        // The constraint is still in the database and the history says it was
        // removed.
        //
        // Their load-time gate now bounds authored identifiers, but a caller
        // reaching the lower seam directly bypasses it and their probe still
        // reproduces after that fix. So this check is not scaffolding waiting on
        // an upstream release; it is the thing that actually catches this, and it
        // should stay.
        //
        // A stored name of exactly 63 bytes is already indistinguishable from a
        // truncated one.
        if !scalar_bool(
            &probe,
            // octet_length, NOT length. PostgreSQL's budget is 63 BYTES while
            // `length` counts CHARACTERS, so a name carrying any multi-byte
            // character reads as shorter than it is: 61 'a' plus one two-byte
            // character is 62 characters and 63 bytes, and a character-counting
            // predicate waves it through.
            //
            // The clip itself lands on a character boundary - a trailing
            // multi-byte character that does not fit is dropped whole rather than
            // split - so a truncated name can come back at 62 bytes. That only
            // matters for a check that derives the truncated spelling; this one
            // asks whether the budget was reached at all, which the byte count
            // answers directly.
            "SELECT NOT EXISTS ( \
               SELECT 1 FROM pg_indexes \
                WHERE schemaname = 'zeroship' AND octet_length(indexname) >= 63 \
               UNION ALL \
               SELECT 1 FROM pg_constraint c \
                 JOIN pg_namespace n ON n.oid = c.connamespace \
                WHERE n.nspname = 'zeroship' AND octet_length(c.conname) >= 63 \
             )",
        )
        .await
        {
            return Err(
                "an index or constraint name in schema 'zeroship' reached \
                 PostgreSQL's 63-byte identifier limit and may have been silently \
                 truncated"
                    .to_string(),
            );
        }

        // (2) key tables from each conceptual domain (control/auth/billing/sandbox)
        //     all live in the `zeroship` schema.
        for table in [
            "app_audit",           // control
            "app_user_identities", // auth
            "app_spend_limit",     // billing
            "deleted_sandboxes",   // sandbox
        ] {
            let sql = format!(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
                 WHERE table_schema = 'zeroship' AND table_name = '{table}')"
            );
            if !scalar_bool(&probe, &sql).await {
                return Err(format!("table zeroship.{table} missing"));
            }
        }

        // (3) at least one RLS policy (tenant_isolation) landed.
        if !scalar_bool(
            &probe,
            "SELECT EXISTS (SELECT 1 FROM pg_policies \
             WHERE schemaname = 'zeroship' AND policyname = 'tenant_isolation')",
        )
        .await
        {
            return Err("RLS policy 'tenant_isolation' missing".to_string());
        }

        // (4) a function AND a trigger landed (functions_triggers_comments file).
        if !scalar_bool(
            &probe,
            "SELECT EXISTS (SELECT 1 FROM pg_proc p \
             JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = 'zeroship' AND p.proname = 'app_audit_block_tamper')",
        )
        .await
        {
            return Err("function zeroship.app_audit_block_tamper missing".to_string());
        }
        if !scalar_bool(
            &probe,
            "SELECT EXISTS (SELECT 1 FROM pg_trigger t \
             JOIN pg_class c ON c.oid = t.tgrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'zeroship' AND c.relname = 'app_audit' \
               AND t.tgname = 'app_audit_block_delete')",
        )
        .await
        {
            return Err("trigger app_audit_block_delete on zeroship.app_audit missing".to_string());
        }

        // (5) the grants landed — a platform role holds a privilege on a platform
        //     table (grants file). `zeroship_control` gets table privileges.
        if !scalar_bool(
            &probe,
            "SELECT EXISTS (SELECT 1 FROM information_schema.role_table_grants \
             WHERE table_schema = 'zeroship' AND grantee = 'zeroship_control')",
        )
        .await
        {
            return Err("no table grants to role 'zeroship_control'".to_string());
        }
        if !scalar_bool(
            &probe,
            "SELECT has_table_privilege( \
                       'zeroship_auth', \
                       'zeroship.app_user_identities', \
                       'INSERT' \
                    ) \
                AND NOT has_table_privilege( \
                       'zeroship_worker', \
                       'zeroship.app_user_identities', \
                       'INSERT' \
                    )",
        )
        .await
        {
            return Err("app_user_identities INSERT privilege boundary is wrong".to_string());
        }
        if !scalar_bool(
            &probe,
            "SELECT has_table_privilege( \
                       'zeroship_auth', \
                       'zeroship.app_oauth_clients', \
                       'SELECT' \
                    ) \
                AND NOT has_table_privilege( \
                       'zeroship_worker', \
                       'zeroship.app_oauth_clients', \
                       'SELECT' \
                    )",
        )
        .await
        {
            return Err("app_oauth_clients SELECT privilege boundary is wrong".to_string());
        }
        if !scalar_bool(
            &probe,
            "SELECT EXISTS (SELECT 1 FROM pg_indexes \
             WHERE schemaname = 'zeroship' \
               AND tablename = 'app_user_identities' \
               AND indexname = 'app_user_identities_global_user_id_idx')",
        )
        .await
        {
            return Err("app_user_identities global-user lookup index is missing".to_string());
        }
        if !scalar_bool(
            &probe,
            "SELECT EXISTS (SELECT 1 FROM pg_indexes \
             WHERE schemaname = 'zeroship' \
               AND tablename = 'users' \
               AND indexname = 'auth_users_non_authenticating_idx')",
        )
        .await
        {
            return Err("non-authenticating user feed index is missing".to_string());
        }
        // The seeded `pairwise_sub` is DERIVED, not a literal: it is the value
        // the gateway would project for this (user, client), and the stored
        // subject is immutable once written, so a made-up literal is the value
        // no credential for this pair can ever carry. This probe only reads the
        // row across RLS today; deriving it keeps that true if it ever does more.
        let lifecycle_pws = zeroship_core::auth::derive_pairwise(
            &zeroship_core::crypto::derive_key("platform-migrate-test-salt"),
            "10000000-0000-0000-0000-000000000001",
            "https://oac_lifecycle_feed.zeroship.localhost",
        );
        probe
            .batch(&format!(
                "INSERT INTO zeroship.users (id, email, name) VALUES \
                   ('10000000-0000-0000-0000-000000000001', \
                    'lifecycle-feed@zeroship.test', 'Lifecycle Feed'); \
                 INSERT INTO zeroship.oauth_clients \
                   (client_id, client_name, redirect_uris, scopes) VALUES \
                   ('oac_lifecycle_feed', 'Lifecycle Feed', ARRAY[]::text[], ARRAY[]::text[]); \
                 INSERT INTO zeroship.app_user_identities \
                   (app_client_id, global_user_id, pairwise_sub) VALUES \
                   ('oac_lifecycle_feed', \
                    '10000000-0000-0000-0000-000000000001', '{lifecycle_pws}')"
            ))
            .await
            .map_err(|e| format!("seed lifecycle feed: {e}"))?;
        probe
            .batch("SET ROLE zeroship_control")
            .await
            .map_err(|e| format!("assume control role: {e}"))?;
        let control_sees_mapping = scalar_bool(
            &probe,
            "SELECT count(*) = 1 FROM zeroship.app_user_identities \
             WHERE global_user_id = '10000000-0000-0000-0000-000000000001'",
        )
        .await;
        probe
            .batch("RESET ROLE")
            .await
            .map_err(|e| format!("reset control role: {e}"))?;
        if !control_sees_mapping {
            return Err("control lifecycle feed cannot cross identity RLS".to_string());
        }

        // (6) The app-code worker login is a replication consumer, never a
        // platform-schema writer. Check effective table, column, sequence, and
        // schema authority so role membership cannot hide a write grant.
        if !scalar_bool(
            &probe,
            "SELECT NOT rolsuper AND NOT rolcreaterole AND NOT rolcreatedb \
                    AND rolreplication AND rolbypassrls \
               FROM pg_roles WHERE rolname = 'zeroship_worker'",
        )
        .await
        {
            return Err("zeroship_worker has unsafe role attributes".to_string());
        }
        if !scalar_bool(
            &probe,
            "SELECT pg_has_role('zeroship_worker', 'zeroship_workflow_owner', 'MEMBER') \
                    AND NOT has_schema_privilege('zeroship_worker', 'zeroship', 'CREATE') \
                    AND NOT has_database_privilege('zeroship_worker', current_database(), 'CREATE')",
        )
        .await
        {
            return Err("zeroship_worker has unsafe ownership or schema authority".to_string());
        }
        if !scalar_bool(
            &probe,
            "SELECT NOT EXISTS ( \
                 SELECT 1 \
                   FROM pg_class relation \
                   JOIN pg_namespace namespace ON namespace.oid = relation.relnamespace \
                   CROSS JOIN (VALUES ('INSERT'), ('UPDATE'), ('DELETE'), ('TRUNCATE'), \
                                      ('REFERENCES'), ('TRIGGER')) privilege(name) \
                  WHERE namespace.nspname = 'zeroship' \
                    AND relation.relkind IN ('r', 'p', 'v', 'm', 'f') \
                    AND has_table_privilege('zeroship_worker', relation.oid, privilege.name) \
               ) AND NOT EXISTS ( \
                 SELECT 1 \
                   FROM pg_class relation \
                   JOIN pg_namespace namespace ON namespace.oid = relation.relnamespace \
                   CROSS JOIN (VALUES ('INSERT'), ('UPDATE'), ('REFERENCES')) privilege(name) \
                  WHERE namespace.nspname = 'zeroship' \
                    AND relation.relkind IN ('r', 'p', 'v', 'm', 'f') \
                    AND has_any_column_privilege('zeroship_worker', relation.oid, privilege.name) \
               ) AND NOT EXISTS ( \
                 SELECT 1 \
                   FROM pg_class relation \
                   JOIN pg_namespace namespace ON namespace.oid = relation.relnamespace \
                   CROSS JOIN (VALUES ('USAGE'), ('UPDATE')) privilege(name) \
                  WHERE namespace.nspname = 'zeroship' \
                    AND relation.relkind = 'S' \
                    AND has_sequence_privilege('zeroship_worker', relation.oid, privilege.name) \
               )",
        )
        .await
        {
            return Err("zeroship_worker can write a platform relation".to_string());
        }
        if !scalar_bool(
            &probe,
            "SELECT NOT has_table_privilege('zeroship_worker', 'zeroship.device_grants', 'INSERT') \
                    AND NOT has_column_privilege('zeroship_worker', 'zeroship.apps', 'api_key', 'SELECT') \
                    AND has_column_privilege('zeroship_worker', 'zeroship.apps', 'id', 'SELECT') \
                    AND has_column_privilege('zeroship_worker', 'zeroship.plans', 'runtime_limits_json', 'SELECT') \
                    AND has_column_privilege('zeroship_worker', 'zeroship.app_deploys', 'manifest_json', 'SELECT')",
        )
        .await
        {
            return Err("zeroship_worker column grants exceed the workflow projection".to_string());
        }

        // (6b) Control reaches the per-app workflow journals the same way the
        // worker does, and by the same bounded route. It PROVISIONS them
        // (workflow_instance_api, cron::workflow_schedules -> PgStore::provision,
        // whose first statement is `SET ROLE zeroship_workflow_owner`) and sweeps
        // them, so membership is required - table privileges cannot stand in for
        // SET ROLE, and a migration cannot name an `app_<uuid>` schema that does
        // not exist yet. Without this, every control workflow cron failed with
        // `permission denied for schema app_<uuid>`.
        //
        // The second half is the bound: the role control gains owns nothing but
        // the journals. If it ever acquires CREATE in `zeroship` or on the
        // database, this membership stops being narrow and this check fails.
        if !scalar_bool(
            &probe,
            "SELECT pg_has_role('zeroship_control', 'zeroship_workflow_owner', 'MEMBER') \
                    AND NOT has_schema_privilege('zeroship_workflow_owner', 'zeroship', 'CREATE') \
                    AND NOT has_database_privilege('zeroship_workflow_owner', current_database(), 'CREATE')",
        )
        .await
        {
            return Err(
                "zeroship_control cannot reach per-app workflow journals, or the owner role it \
                 reaches them through is no longer narrow"
                    .to_string(),
            );
        }

        // (7) The service-assertion replay store is a SECOND trust zone, and it
        // is bounded. The worker holds write grants on
        // `service_authn.service_assertion_replay` because a callee that
        // verifies a service assertion must record its `jti`, and the worker is
        // a callee (docs/proposals/2026-08-16-service-identity.md:180, where the
        // gateway calls `worker POST /dispatch/{app}`). That is precisely the
        // grant check (6) refuses, which is why the table sits in its own schema
        // rather than as a named exception to a blanket rule.
        //
        // A second schema only helps while it stays one table. The platform
        // charter now allowlists `service_authn` for CREATE TABLE, so without
        // this a later platform migration could put real platform state there
        // and grant the worker writes on it - the same collision one namespace
        // over, with nothing in the way. So: the zone holds exactly the replay
        // table, and no service role can add to it.
        if !scalar_bool(
            &probe,
            "SELECT ( \
                 SELECT count(*) \
                   FROM pg_class relation \
                   JOIN pg_namespace namespace ON namespace.oid = relation.relnamespace \
                  WHERE namespace.nspname = 'service_authn' \
                    AND relation.relkind IN ('r', 'p', 'v', 'm', 'f', 'S') \
               ) = 1 \
               AND EXISTS ( \
                 SELECT 1 \
                   FROM pg_class relation \
                   JOIN pg_namespace namespace ON namespace.oid = relation.relnamespace \
                  WHERE namespace.nspname = 'service_authn' \
                    AND relation.relname = 'service_assertion_replay' \
                    AND relation.relkind = 'r' \
               ) \
               AND NOT EXISTS ( \
                 SELECT 1 \
                   FROM (VALUES ('zeroship_control'), ('zeroship_gateway'), \
                                ('zeroship_worker'), ('zeroship_auth'), \
                                ('zeroship_app')) grantee(name) \
                  WHERE has_schema_privilege(grantee.name, 'service_authn', 'CREATE') \
               )",
        )
        .await
        {
            return Err("the service_authn zone is not exactly the replay table".to_string());
        }

        // And the zone's grants are the verifying services and nobody else.
        // `zeroship_app` is the creator-app login: it reaches the `zeroship`
        // schema (grants file) and runs no verifier, so it is the control that
        // makes the four positives mean "granted to verifiers" rather than
        // "granted to whoever asked".
        if !scalar_bool(
            &probe,
            "SELECT has_table_privilege('zeroship_worker', \
                        'service_authn.service_assertion_replay', 'INSERT') \
                AND has_table_privilege('zeroship_gateway', \
                        'service_authn.service_assertion_replay', 'INSERT') \
                AND has_table_privilege('zeroship_control', \
                        'service_authn.service_assertion_replay', 'INSERT') \
                AND has_table_privilege('zeroship_auth', \
                        'service_authn.service_assertion_replay', 'INSERT') \
                AND NOT has_table_privilege('zeroship_app', \
                        'service_authn.service_assertion_replay', 'SELECT')",
        )
        .await
        {
            return Err("replay-table grants do not match the verifying services".to_string());
        }

        Ok(())
    }

    /// Exercise the real ordered runner across a catalog refresh and verify that
    /// the later workflow migration can still validate foreign-key value formats
    /// authored by an earlier file. The environment gate matches the other live-PG
    /// coverage in this suite.
    #[compio::test]
    async fn ordered_runner_retains_authored_fk_formats_across_catalog_refresh() {
        let Some(url) = pg_url() else {
            zeroship_test_support::skip(
                "skipping logical-column retention regression: no test database (set PG_TEST_URL \
                 to a DSN on :5440 to run)"
            );
            return;
        };
        let _serial = DB_APPLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let scratch = format!(
            "zs_logical_columns_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let scratch_dsn = dsn_with_db(&url, &scratch);

        {
            let admin = admin_session(&url).await;
            let _ = admin
                .batch(&format!("DROP DATABASE IF EXISTS \"{scratch}\""))
                .await;
            admin
                .batch(&format!("CREATE DATABASE \"{scratch}\""))
                .await
                .unwrap_or_else(|e| panic!("CREATE DATABASE scratch failed: {e}"));
        }

        let result = run_logical_column_retention_assertions(&scratch_dsn).await;

        {
            let admin = admin_session(&url).await;
            let _ = admin
                .batch(&format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                     WHERE datname = '{scratch}' AND pid <> pg_backend_pid()"
                ))
                .await;
            admin
                .batch(&format!("DROP DATABASE IF EXISTS \"{scratch}\""))
                .await
                .expect("DROP DATABASE scratch");
        }

        result.expect("ordered runner must retain authored foreign-key formats");
    }

    async fn run_logical_column_retention_assertions(scratch_dsn: &str) -> Result<(), String> {
        let cfg = PlatformMigrateConfig {
            database_url: scratch_dsn.to_string(),
            migrations_dir: migrations_dir(),
            project_schema: "zeroship".to_string(),
            project_id: "zeroship".to_string(),
            cluster_lock_database: DEFAULT_CLUSTER_LOCK_DATABASE.to_string(),
        };
        let report = run_platform_migrations(&cfg)
            .await
            .map_err(|e| format!("run_platform_migrations failed: {e}"))?;
        // The corpus size, not a literal. This read `!= 12`, which was true at
        // abd1e70d7 when `db/migrations-ts` held twelve files and went stale as
        // eleven more landed; it then read a maintained constant, which went
        // stale the same way one delete-plus-add later. It now reads the
        // directory.
        let expected_files = platform_migration_files();
        if report.files != expected_files {
            return Err(format!("expected {expected_files} files, saw {}", report.files));
        }

        let probe = CompioPgSession::connect(scratch_dsn)
            .await
            .map_err(|e| format!("connect assertion probe: {e}"))?;
        let mut missing = Vec::new();
        for table in DURABLE_WORKFLOW_JOURNAL_TABLES {
            let sql = format!(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
                 WHERE table_schema = 'zeroship' AND table_name = '{table}')"
            );
            if !scalar_bool(&probe, &sql).await {
                missing.push(table);
            }
        }
        if !missing.is_empty() {
            return Err(format!(
                "durable-workflow journal tables missing after apply: {}",
                missing.join(", ")
            ));
        }

        Ok(())
    }

    /// PROVE the platform-migrate one-shot is IDEMPOTENT (re-runnable). On a FRESH
    /// scratch DB: run 1 applies every file's migrations (N applied, 0 skipped); run
    /// 2 over the SAME already-migrated DB re-lowers the identical `.ts` set and must
    /// skip EVERY migration (0 applied, N skipped) and exit clean — the journal row
    /// count is UNCHANGED (no duplicate rows) and no "already exists" error surfaces.
    ///
    /// This is the regression guard for the filename-derived STABLE version fix: the
    /// engine's already-applied skip keys on the journal version, so a re-run only
    /// skips when re-lowering reproduces byte-identical, order-preserving versions.
    /// Before the fix, additive DDL got a fresh RANDOM `MigrationId::generate()` per
    /// lowering, so run 2 re-executed everything and failed (e.g. `type
    /// "account_state" already exists`). Gated on a test database (see `PG_TEST_URL`).
    #[compio::test]
    async fn platform_migrate_is_idempotent_on_rerun() {
        let Some(url) = pg_url() else {
            zeroship_test_support::skip(
                "skipping idempotency proof: no test database (set PG_TEST_URL \
                 to a DSN on :5440 to run)"
            );
            return;
        };
        // Serialize against the sibling DB-apply test (shared cluster-global roles).
        let _serial = DB_APPLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let scratch = format!(
            "zs_stage4a_idem_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let scratch_dsn = dsn_with_db(&url, &scratch);

        {
            let admin = admin_session(&url).await;
            let _ = admin
                .batch(&format!("DROP DATABASE IF EXISTS \"{scratch}\""))
                .await;
            admin
                .batch(&format!("CREATE DATABASE \"{scratch}\""))
                .await
                .unwrap_or_else(|e| panic!("CREATE DATABASE scratch failed: {e}"));
        }

        let result = run_idempotency_assertions(&scratch_dsn).await;

        {
            let admin = admin_session(&url).await;
            let _ = admin
                .batch(&format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                     WHERE datname = '{scratch}' AND pid <> pg_backend_pid()"
                ))
                .await;
            admin
                .batch(&format!("DROP DATABASE IF EXISTS \"{scratch}\""))
                .await
                .expect("DROP DATABASE scratch");
        }

        result.expect("platform-migrate idempotency assertions must pass");
    }

    /// Run the migrate one-shot twice against `scratch_dsn` and assert the
    /// idempotency contract. Returns `Ok(())` on success; the caller drops the DB.
    async fn run_idempotency_assertions(scratch_dsn: &str) -> Result<(), String> {
        let cfg = PlatformMigrateConfig {
            database_url: scratch_dsn.to_string(),
            migrations_dir: migrations_dir(),
            project_schema: "zeroship".to_string(),
            project_id: "zeroship".to_string(),
            cluster_lock_database: DEFAULT_CLUSTER_LOCK_DATABASE.to_string(),
        };

        // ── RUN 1: fresh DB — everything applies, nothing skips ──
        let run1 = run_platform_migrations(&cfg)
            .await
            .map_err(|e| format!("run 1 failed: {e}"))?;
        if run1.applied.is_empty() {
            return Err("run 1 applied nothing".to_string());
        }
        if !run1.skipped.is_empty() {
            return Err(format!(
                "run 1 (fresh DB) reported {} skipped — the per-file report over-counts \
                 already-applied rows across files",
                run1.skipped.len()
            ));
        }

        // Journal row count after the first apply (the idempotency baseline).
        let probe = CompioPgSession::connect(scratch_dsn)
            .await
            .map_err(|e| format!("connect journal probe: {e}"))?;
        let rows_after_run1 = journal_completed_count(&probe).await;
        if rows_after_run1 as usize != run1.applied.len() {
            return Err(format!(
                "run 1 applied {} but journal holds {} completed rows",
                run1.applied.len(),
                rows_after_run1
            ));
        }

        // ── RUN 2: same already-migrated DB — nothing applies, everything skips ──
        let run2 = run_platform_migrations(&cfg)
            .await
            .map_err(|e| format!("run 2 (idempotent re-run) failed: {e}"))?;
        if !run2.applied.is_empty() {
            return Err(format!(
                "run 2 re-applied {} migration(s) — the re-run is NOT idempotent \
                 (stable-version skip did not match)",
                run2.applied.len()
            ));
        }
        if run2.skipped.len() != run1.applied.len() {
            return Err(format!(
                "run 2 skipped {} but run 1 applied {} — the skip set must be exactly \
                 run 1's applied set",
                run2.skipped.len(),
                run1.applied.len()
            ));
        }

        // Journal row count is UNCHANGED — no duplicate rows appended by run 2.
        let rows_after_run2 = journal_completed_count(&probe).await;
        if rows_after_run2 != rows_after_run1 {
            return Err(format!(
                "journal grew from {rows_after_run1} to {rows_after_run2} rows across the \
                 idempotent re-run (duplicate journal rows)"
            ));
        }

        // No duplicate versions in the journal (each logical migration once).
        let distinct: i64 = probe
            .client()
            .query_one(
                "SELECT count(DISTINCT version)::bigint \
                 FROM zeroship_migrations.schema_migrations WHERE phase = 'completed'",
                &[],
            )
            .await
            .map_err(|e| format!("count distinct versions: {e}"))?
            .try_get::<_, i64>(0)
            .map_err(|e| format!("decode distinct count: {e}"))?;
        if distinct != rows_after_run2 {
            return Err(format!(
                "journal has {rows_after_run2} completed rows but only {distinct} distinct \
                 versions — duplicate version rows present"
            ));
        }

        Ok(())
    }

    /// The probe file's name, which must sort AFTER every committed migration.
    ///
    /// It stands for a migration added today, and the runner derives a
    /// migration's stable version from its ORDINAL in the sorted set. A name
    /// that lands mid-corpus shifts the version of every file after it, so run 2
    /// checks the journal's checksum for a version against a DIFFERENT file's
    /// body: the failure is checksum drift, which says nothing about ordering.
    ///
    /// MEASURED, on a live DSN before this was changed: the previous name
    /// `20260710000100_append_probe.ts` had ten committed files sorting after it
    /// (every `202608*`, the first landing 2026-08-11), and run 2 failed with
    /// `checksum drift on mig_0000595bcDNs774MyYTiwC`.
    ///
    /// A far-future date rather than today's, so it does not have to move every
    /// time a migration lands. [`assert_probe_sorts_last`] is what makes the
    /// requirement fail loudly instead of as drift if it ever stops holding.
    const APPEND_FILENAME: &str = "29991231000000_append_probe.ts";
    const APPEND_SOURCE: &str = r#"
import { table, t } from "@zeroship/migrate";

export const name = "append_probe";

export function up() {
  table("platform_append_probe", { schema: "zeroship" }).create({
    columns: {
      id: t.bigInt().notNull(),
      app_id: t.uuid().notNull(),
    },
    primaryKey: ["id"],
  });
  table("platform_append_probe", { schema: "zeroship" })
    .foreignKey("platform_append_probe_app_id_fkey")
    .add({
      columns: ["app_id"],
      references: { table: "apps", columns: ["id"], schema: "zeroship" },
      onDelete: "cascade",
    });
}

export function down() {}
"#;

    const APPEND_MIGRATION_STEPS: usize = 2;

    const APPLIED_CORPUS_PREFIX_FILES: usize = 9;

    const PARTIAL_FILENAME: &str = "20260806000100_partial_resume.ts";
    const PARTIAL_SOURCE: &str = r#"
import { raw } from "@zeroship/migrate";

export const name = "partial_resume";

export function up() {
  raw({
    reason: "Add the first independently journaled column.",
    sql: "ALTER TABLE zeroship.resume_a ADD COLUMN applied boolean",
  });
  raw({
    reason: "Add the second independently journaled column.",
    sql: "ALTER TABLE zeroship.resume_b ADD COLUMN applied boolean",
  });
}

export function down() {}
"#;

    const EDITED_FILENAME: &str = "20260806000200_edited_source.ts";
    const EDITED_SOURCE: &str = r#"
import { raw } from "@zeroship/migrate";

export const name = "edited_source";

export function up() {
  raw({
    reason: "Create the checksum edit probe.",
    sql: "CREATE TABLE zeroship.edited_source_probe (id bigint PRIMARY KEY)",
  });
}

export function down() {}
"#;

    fn test_config(database_url: &str, migrations_dir: &Path) -> PlatformMigrateConfig {
        PlatformMigrateConfig {
            database_url: database_url.to_string(),
            migrations_dir: migrations_dir.to_path_buf(),
            project_schema: "zeroship".to_string(),
            project_id: "zeroship".to_string(),
            cluster_lock_database: DEFAULT_CLUSTER_LOCK_DATABASE.to_string(),
        }
    }

    #[compio::test]
    async fn platform_migrate_applies_only_newly_appended_file() {
        let Some(url) = pg_url() else {
            zeroship_test_support::skip(
                "skipping appended-file proof: no test database (set PG_TEST_URL \
                 to a DSN on :5440 to run)"
            );
            return;
        };
        let _serial = DB_APPLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let corpus = tempfile::tempdir().expect("create temporary migration corpus");
        copy_migration_corpus(corpus.path()).expect("copy platform migration corpus");
        let scratch = create_scratch_database(&url, "zs_ledger_append")
            .await
            .expect("create appended-file scratch database");

        let result = run_appended_file_assertions(&scratch.dsn, corpus.path()).await;
        drop_scratch_database(&url, &scratch)
            .await
            .expect("drop appended-file scratch database");
        result.expect("only the newly appended migration file must apply");
    }

    /// Fail with the real reason if [`APPEND_FILENAME`] stops sorting last.
    ///
    /// This test says "appended", and the runner only treats a file as appended
    /// when it sorts after every other. When that stopped being true the suite
    /// still failed - but as `checksum drift`, which reads as a corrupted
    /// journal and sent the reader to the migration bodies. This is the same
    /// requirement stated where it can be acted on.
    fn assert_probe_sorts_last(corpus: &Path) -> Result<(), String> {
        let mut existing: Vec<String> = std::fs::read_dir(corpus)
            .map_err(|e| format!("read the temporary corpus: {e}"))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".ts"))
            .collect();
        existing.sort();
        match existing.last() {
            Some(last) if last.as_str() < APPEND_FILENAME => Ok(()),
            Some(last) => Err(format!(
                "{APPEND_FILENAME} no longer sorts after the whole corpus (last is {last}), so it \
                 would renumber the files after it instead of appending; move it past {last}"
            )),
            None => Err("the temporary corpus is empty".to_string()),
        }
    }

    async fn run_appended_file_assertions(scratch_dsn: &str, corpus: &Path) -> Result<(), String> {
        let cfg = test_config(scratch_dsn, corpus);
        let run1 = run_platform_migrations(&cfg)
            .await
            .map_err(|e| format!("initial corpus apply failed: {e}"))?;
        let expected_files = platform_migration_files();
        if run1.files != expected_files {
            return Err(format!(
                "initial corpus reported {} files, expected {expected_files}",
                run1.files
            ));
        }
        if run1.applied.is_empty() || !run1.skipped.is_empty() {
            return Err(format!(
                "initial corpus reported {} applied and {} skipped",
                run1.applied.len(),
                run1.skipped.len()
            ));
        }

        let probe = CompioPgSession::connect(scratch_dsn)
            .await
            .map_err(|e| format!("connect appended-file probe: {e}"))?;
        let rows_before_append = journal_completed_count(&probe).await;
        assert_probe_sorts_last(corpus)?;
        write_migration(corpus, APPEND_FILENAME, APPEND_SOURCE)?;

        let run2 = run_platform_migrations(&cfg)
            .await
            .map_err(|e| format!("appended corpus apply failed: {e}"))?;
        if run2.files != expected_files + 1 {
            return Err(format!(
                "appended corpus reported {} files, expected {}",
                run2.files,
                expected_files + 1
            ));
        }
        if run2.applied.len() != APPEND_MIGRATION_STEPS {
            return Err(format!(
                "appended corpus applied {} steps, expected {APPEND_MIGRATION_STEPS}: {:?}",
                run2.applied.len(),
                run2.applied
            ));
        }
        if run2.skipped.len() != run1.applied.len() {
            return Err(format!(
                "appended corpus skipped {} prior steps, expected {}",
                run2.skipped.len(),
                run1.applied.len()
            ));
        }
        let rows_after_append = journal_completed_count(&probe).await;
        if rows_after_append != rows_before_append + APPEND_MIGRATION_STEPS as i64 {
            return Err(format!(
                "journal grew from {rows_before_append} to {rows_after_append}, expected \
                 {APPEND_MIGRATION_STEPS} rows"
            ));
        }
        if !table_exists(&probe, "platform_append_probe").await {
            return Err("newly appended migration did not create its probe table".to_string());
        }
        if !foreign_key_targets(
            &probe,
            "platform_append_probe_app_id_fkey",
            "platform_append_probe",
            "apps",
        )
        .await
        {
            return Err(
                "newly appended migration did not create its foreign key to zeroship.apps"
                    .to_string(),
            );
        }
        let ledger_rows = ledger_row_count(&probe).await;
        if ledger_rows != (expected_files + 1) as i64 {
            return Err(format!(
                "file ledger contains {ledger_rows} rows, expected {}",
                expected_files + 1
            ));
        }
        Ok(())
    }

    #[compio::test]
    async fn platform_migrate_resumes_partially_applied_corpus() {
        let Some(url) = pg_url() else {
            zeroship_test_support::skip(
                "skipping corpus-prefix resume proof: no test database (set PG_TEST_URL \
                 to a DSN on :5440 to run)"
            );
            return;
        };
        let _serial = DB_APPLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let corpus = tempfile::tempdir().expect("create temporary migration corpus");
        copy_migration_prefix(corpus.path(), APPLIED_CORPUS_PREFIX_FILES)
            .expect("copy platform migration prefix");
        let scratch = create_scratch_database(&url, "zs_ledger_corpus_prefix")
            .await
            .expect("create corpus-prefix scratch database");

        let result = run_corpus_prefix_resume_assertions(&scratch.dsn, corpus.path()).await;
        drop_scratch_database(&url, &scratch)
            .await
            .expect("drop corpus-prefix scratch database");
        result.expect("the complete corpus must resume after an applied prefix");
    }

    async fn run_corpus_prefix_resume_assertions(
        scratch_dsn: &str,
        corpus: &Path,
    ) -> Result<(), String> {
        let cfg = test_config(scratch_dsn, corpus);
        let prefix = run_platform_migrations(&cfg)
            .await
            .map_err(|e| format!("migration prefix apply failed: {e}"))?;
        if prefix.files != APPLIED_CORPUS_PREFIX_FILES
            || prefix.applied.is_empty()
            || !prefix.skipped.is_empty()
        {
            return Err(format!(
                "migration prefix reported files={}, applied={}, skipped={}",
                prefix.files,
                prefix.applied.len(),
                prefix.skipped.len()
            ));
        }

        let probe = CompioPgSession::connect(scratch_dsn)
            .await
            .map_err(|e| format!("connect corpus-prefix probe: {e}"))?;
        let rows_after_prefix = journal_completed_count(&probe).await;
        if rows_after_prefix != prefix.applied.len() as i64 {
            return Err(format!(
                "migration prefix applied {} steps but journal has {rows_after_prefix}",
                prefix.applied.len()
            ));
        }
        if ledger_row_count(&probe).await != APPLIED_CORPUS_PREFIX_FILES as i64 {
            return Err("migration prefix did not record every completed file".to_string());
        }

        probe
            .batch(&format!(
                "DROP TABLE zeroship_migrations.{PLATFORM_MIGRATION_LEDGER_TABLE}"
            ))
            .await
            .map_err(|e| format!("remove file ledger before corpus resume: {e}"))?;
        copy_migration_corpus(corpus)?;

        let resumed = run_platform_migrations(&cfg)
            .await
            .map_err(|e| format!("complete corpus resume failed: {e}"))?;
        let expected_files = platform_migration_files();
        if resumed.files != expected_files {
            return Err(format!(
                "complete corpus reported {} files, expected {expected_files}",
                resumed.files
            ));
        }
        if resumed.applied.is_empty() {
            return Err("complete corpus did not apply any remaining migration".to_string());
        }
        if resumed.skipped.len() != prefix.applied.len() {
            return Err(format!(
                "complete corpus skipped {} prior steps, expected {}",
                resumed.skipped.len(),
                prefix.applied.len()
            ));
        }
        let rows_after_resume = journal_completed_count(&probe).await;
        if rows_after_resume != rows_after_prefix + resumed.applied.len() as i64 {
            return Err(format!(
                "journal grew from {rows_after_prefix} to {rows_after_resume}, but resume \
                 applied {} steps",
                resumed.applied.len()
            ));
        }
        if ledger_row_count(&probe).await != expected_files as i64 {
            return Err("complete corpus did not record every migration file".to_string());
        }
        for table in DURABLE_WORKFLOW_JOURNAL_TABLES {
            if !table_exists(&probe, table).await {
                return Err(format!(
                    "durable-workflow table zeroship.{table} is missing"
                ));
            }
        }
        for column in ["line_kind", "correction_dedup_key"] {
            if !column_exists(&probe, "invoice_lines", column).await {
                return Err(format!(
                    "billing correction column invoice_lines.{column} is missing"
                ));
            }
        }
        if table_exists(&probe, "metering_exports").await {
            return Err("metering_exports still exists after complete corpus resume".to_string());
        }
        Ok(())
    }

    #[compio::test]
    async fn platform_migrate_resumes_a_partially_applied_file() {
        let Some(url) = pg_url() else {
            zeroship_test_support::skip(
                "skipping partial-file proof: no test database (set PG_TEST_URL \
                 to a DSN on :5440 to run)"
            );
            return;
        };
        let _serial = DB_APPLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let corpus = tempfile::tempdir().expect("create temporary migration corpus");
        write_migration(corpus.path(), PARTIAL_FILENAME, PARTIAL_SOURCE)
            .expect("write partial-file migration");
        let scratch = create_scratch_database(&url, "zs_ledger_partial")
            .await
            .expect("create partial-file scratch database");

        let result = run_partial_file_assertions(&scratch.dsn, corpus.path()).await;
        drop_scratch_database(&url, &scratch)
            .await
            .expect("drop partial-file scratch database");
        result.expect("a rerun must complete the remaining file step");
    }

    async fn run_partial_file_assertions(scratch_dsn: &str, corpus: &Path) -> Result<(), String> {
        let setup = CompioPgSession::connect(scratch_dsn)
            .await
            .map_err(|e| format!("connect partial-file setup: {e}"))?;
        setup
            .batch(
                "CREATE SCHEMA zeroship; \
                 CREATE TABLE zeroship.resume_a (id bigint PRIMARY KEY); \
                 CREATE TABLE zeroship.resume_b (id bigint PRIMARY KEY)",
            )
            .await
            .map_err(|e| format!("create partial-file probe tables: {e}"))?;

        let blocker = CompioPgSession::connect(scratch_dsn)
            .await
            .map_err(|e| format!("connect partial-file blocker: {e}"))?;
        blocker
            .batch("BEGIN; LOCK TABLE zeroship.resume_b IN ACCESS SHARE MODE")
            .await
            .map_err(|e| format!("lock second probe table: {e}"))?;

        let cfg = test_config(scratch_dsn, corpus);
        let first_run = run_platform_migrations(&cfg).await;
        blocker
            .batch("ROLLBACK")
            .await
            .map_err(|e| format!("release second probe table: {e}"))?;
        match first_run {
            Err(PlatformMigrateError::Apply { file, .. }) if file == PARTIAL_FILENAME => {}
            Err(other) => {
                return Err(format!(
                    "partial-file run failed through the wrong error path: {other}"
                ));
            }
            Ok(report) => {
                return Err(format!(
                    "partial-file run unexpectedly succeeded with {} applied steps",
                    report.applied.len()
                ));
            }
        }

        let probe = CompioPgSession::connect(scratch_dsn)
            .await
            .map_err(|e| format!("connect partial-file probe: {e}"))?;
        if !column_exists(&probe, "resume_a", "applied").await {
            return Err("the first file step did not commit before interruption".to_string());
        }
        if column_exists(&probe, "resume_b", "applied").await {
            return Err("the blocked file step unexpectedly committed".to_string());
        }
        if journal_completed_count(&probe).await != 1 {
            return Err("partial-file journal must contain exactly one completed step".to_string());
        }
        if ledger_has_file(&probe, PARTIAL_FILENAME).await {
            return Err("partial file was recorded complete after a failed step".to_string());
        }

        let rerun = run_platform_migrations(&cfg)
            .await
            .map_err(|e| format!("partial-file rerun failed: {e}"))?;
        if rerun.files != 1 || rerun.applied.len() != 1 || rerun.skipped.len() != 1 {
            return Err(format!(
                "partial-file rerun reported files={}, applied={}, skipped={}",
                rerun.files,
                rerun.applied.len(),
                rerun.skipped.len()
            ));
        }
        if !column_exists(&probe, "resume_b", "applied").await {
            return Err("partial-file rerun did not complete the remaining step".to_string());
        }
        if journal_completed_count(&probe).await != 2 {
            return Err("partial-file journal must contain both completed steps".to_string());
        }
        if !ledger_has_file(&probe, PARTIAL_FILENAME).await {
            return Err("completed partial file is missing from the ledger".to_string());
        }
        Ok(())
    }

    #[compio::test]
    async fn platform_migrate_rejects_an_edited_applied_file() {
        let Some(url) = pg_url() else {
            zeroship_test_support::skip(
                "skipping edited-file proof: no test database (set PG_TEST_URL \
                 to a DSN on :5440 to run)"
            );
            return;
        };
        let _serial = DB_APPLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let corpus = tempfile::tempdir().expect("create temporary migration corpus");
        let migration_path = write_migration(corpus.path(), EDITED_FILENAME, EDITED_SOURCE)
            .expect("write edited-file migration");
        let scratch = create_scratch_database(&url, "zs_ledger_edited")
            .await
            .expect("create edited-file scratch database");

        let result = run_edited_file_assertions(&scratch.dsn, corpus.path(), &migration_path).await;
        drop_scratch_database(&url, &scratch)
            .await
            .expect("drop edited-file scratch database");
        result.expect("an applied file edit must fail with a checksum mismatch");
    }

    async fn run_edited_file_assertions(
        scratch_dsn: &str,
        corpus: &Path,
        migration_path: &Path,
    ) -> Result<(), String> {
        let cfg = test_config(scratch_dsn, corpus);
        let run1 = run_platform_migrations(&cfg)
            .await
            .map_err(|e| format!("initial edited-file apply failed: {e}"))?;
        if run1.files != 1 || run1.applied.len() != 1 || !run1.skipped.is_empty() {
            return Err(format!(
                "initial edited-file apply reported files={}, applied={}, skipped={}",
                run1.files,
                run1.applied.len(),
                run1.skipped.len()
            ));
        }

        let probe = CompioPgSession::connect(scratch_dsn)
            .await
            .map_err(|e| format!("connect edited-file probe: {e}"))?;
        let rows_before_edit = journal_completed_count(&probe).await;
        let edited_source = format!("{EDITED_SOURCE}\nexport function broken(\n");
        std::fs::write(migration_path, edited_source)
            .map_err(|e| format!("edit applied migration source: {e}"))?;

        let error = run_platform_migrations(&cfg)
            .await
            .expect_err("edited migration source must be rejected");
        let message = error.to_string();
        match &error {
            PlatformMigrateError::ChecksumMismatch {
                file,
                applied_checksum,
                current_checksum,
            } => {
                if file != EDITED_FILENAME {
                    return Err(format!(
                        "checksum error named {file}, expected {EDITED_FILENAME}"
                    ));
                }
                if applied_checksum == current_checksum
                    || applied_checksum.len() != 64
                    || current_checksum.len() != 64
                {
                    return Err(format!(
                        "checksum error carried invalid digests: applied={applied_checksum}, \
                         current={current_checksum}"
                    ));
                }
            }
            other => {
                return Err(format!(
                    "edited migration failed through the wrong error path: {other}"
                ));
            }
        }
        if !message.contains(EDITED_FILENAME) || !message.contains("checksum") {
            return Err(format!("checksum error was not clear: {message}"));
        }
        if journal_completed_count(&probe).await != rows_before_edit {
            return Err("edited-file rejection changed the migration journal".to_string());
        }
        if ledger_row_count(&probe).await != 1 {
            return Err("edited-file rejection changed the file ledger".to_string());
        }
        if !table_exists(&probe, "edited_source_probe").await {
            return Err("edited-file probe table disappeared".to_string());
        }
        Ok(())
    }

    const COLLIDE_FIRST_FILENAME: &str = "20260101000000_collision_first.ts";
    const COLLIDE_FIRST_SOURCE: &str = r#"
import { raw } from "@zeroship/migrate";

export const name = "collision_first";

export function up() {
  raw({
    reason: "Create the first ordinal-collision probe.",
    sql: "CREATE TABLE zeroship.collision_first_probe (id bigint PRIMARY KEY)",
  });
}

export function down() {}
"#;

    const COLLIDE_LAST_FILENAME: &str = "20260103000000_collision_last.ts";
    const COLLIDE_LAST_SOURCE: &str = r#"
import { raw } from "@zeroship/migrate";

export const name = "collision_last";

export function up() {
  raw({
    reason: "Create the second ordinal-collision probe.",
    sql: "CREATE TABLE zeroship.collision_last_probe (id bigint PRIMARY KEY)",
  });
}

export function down() {}
"#;

    /// The ONE variable this test moves. Both names carry
    /// [`COLLIDE_INSERT_SOURCE`] byte for byte; only the sort position differs.
    const COLLIDE_MIDDLE_FILENAME: &str = "20260102000000_collision_insert.ts";
    const COLLIDE_APPENDED_FILENAME: &str = "20260104000000_collision_insert.ts";
    const COLLIDE_INSERT_SOURCE: &str = r#"
import { raw } from "@zeroship/migrate";

export const name = "collision_insert";

export function up() {
  raw({
    reason: "Create the inserted-file probe.",
    sql: "CREATE TABLE zeroship.collision_insert_probe (id bigint PRIMARY KEY)",
  });
}

export function down() {}
"#;

    /// A migration inserted MID-CORPUS is refused by NAME, and the refusal names
    /// the applied file whose journal versions it would have taken.
    ///
    /// WHY THIS IS NOT COVERED ELSEWHERE. Two guards already police the same
    /// mistake before a deploy -- `unreleased_migrations_sort_after_every_
    /// released_one` here in this file, and `released_ledger_misordered` in
    /// `deploy/scripts/deploy-remote.sh`. Both compare filenames against ONE
    /// deployment's journal snapshot, so neither can rule on a second cluster
    /// further behind, on a run that does not go through `deploy-remote.sh`, or
    /// on a developer's own database. This one is keyed to the database being
    /// migrated and reads its live journal, so it has no snapshot to go stale.
    ///
    /// WHAT WAS WRONG BEFORE. The runner still aborted -- the ordinal derivation
    /// makes a mid-corpus insert claim a version the journal holds -- but it
    /// aborted with the engine's `ChecksumDrift` on an opaque `mig_...` id,
    /// wrapped in `Apply { file }` naming the file BEING APPLIED. That is the
    /// file that moved into the slot, not the one that owns it, so the operator
    /// was pointed at the wrong migration and told nothing about ordering.
    /// `20260819000000_app_egress_rules.ts` hit this for real on 2026-08-20.
    ///
    /// THE CONTROL IS PART OF THE TEST. The third run re-writes the SAME source
    /// bytes under a filename that sorts LAST and requires it to apply. Without
    /// it a refusal that fired on every new file would pass just as well.
    #[compio::test]
    async fn platform_migrate_refuses_a_mid_corpus_migration_by_name() {
        let Some(url) = pg_url() else {
            zeroship_test_support::skip(
                "skipping mid-corpus insert proof: no test database (set PG_TEST_URL \
                 to a DSN on :5440 to run)"
            );
            return;
        };
        let _serial = DB_APPLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let corpus = tempfile::tempdir().expect("create temporary migration corpus");
        write_migration(corpus.path(), COLLIDE_FIRST_FILENAME, COLLIDE_FIRST_SOURCE)
            .expect("write first collision migration");
        write_migration(corpus.path(), COLLIDE_LAST_FILENAME, COLLIDE_LAST_SOURCE)
            .expect("write last collision migration");
        let scratch = create_scratch_database(&url, "zs_ledger_collide")
            .await
            .expect("create mid-corpus scratch database");

        let result = run_mid_corpus_assertions(&scratch.dsn, corpus.path()).await;
        drop_scratch_database(&url, &scratch)
            .await
            .expect("drop mid-corpus scratch database");
        result.expect("a mid-corpus migration must be refused by name, and the same file renamed to sort last must apply");
    }

    async fn run_mid_corpus_assertions(scratch_dsn: &str, corpus: &Path) -> Result<(), String> {
        let cfg = test_config(scratch_dsn, corpus);
        let run1 = run_platform_migrations(&cfg)
            .await
            .map_err(|e| format!("initial two-file apply failed: {e}"))?;
        if run1.files != 2 || run1.applied.len() != 2 {
            return Err(format!(
                "initial two-file apply reported files={}, applied={}",
                run1.files,
                run1.applied.len()
            ));
        }

        let probe = CompioPgSession::connect(scratch_dsn)
            .await
            .map_err(|e| format!("connect mid-corpus probe: {e}"))?;
        let journal_before = journal_completed_count(&probe).await;
        let ledger_before = ledger_row_count(&probe).await;

        // ---- the refusal: the SAME source, inserted mid-corpus ----
        let inserted = write_migration(corpus, COLLIDE_MIDDLE_FILENAME, COLLIDE_INSERT_SOURCE)?;
        if !inserted.is_file() {
            return Err(format!("staged file {} is not present", inserted.display()));
        }
        if !(COLLIDE_FIRST_FILENAME < COLLIDE_MIDDLE_FILENAME
            && COLLIDE_MIDDLE_FILENAME < COLLIDE_LAST_FILENAME)
        {
            return Err(format!(
                "{COLLIDE_MIDDLE_FILENAME} no longer sorts between {COLLIDE_FIRST_FILENAME} \
                 and {COLLIDE_LAST_FILENAME}, so this run is not testing an insert"
            ));
        }

        let error = run_platform_migrations(&cfg)
            .await
            .expect_err("a mid-corpus migration must be refused");
        match &error {
            PlatformMigrateError::VersionCollision {
                file,
                owner,
                version,
                collisions,
            } => {
                if file != COLLIDE_MIDDLE_FILENAME {
                    return Err(format!(
                        "refusal named {file} as the inserted file, expected \
                         {COLLIDE_MIDDLE_FILENAME}"
                    ));
                }
                if owner != COLLIDE_LAST_FILENAME {
                    return Err(format!(
                        "refusal named {owner} as the slot's owner, expected \
                         {COLLIDE_LAST_FILENAME} -- naming the wrong file is the whole \
                         defect this refusal exists to fix"
                    ));
                }
                if !version.starts_with("mig_") || *collisions == 0 {
                    return Err(format!(
                        "refusal carried version={version}, collisions={collisions}"
                    ));
                }
            }
            other => {
                return Err(format!(
                    "mid-corpus insert failed through the wrong error path: {other}"
                ));
            }
        }
        let message = error.to_string();
        for needle in [
            COLLIDE_MIDDLE_FILENAME,
            COLLIDE_LAST_FILENAME,
            "sorts after every applied migration",
        ] {
            if !message.contains(needle) {
                return Err(format!("refusal message omits {needle:?}: {message}"));
            }
        }
        if journal_completed_count(&probe).await != journal_before {
            return Err("the refusal ran engine work: the journal grew".to_string());
        }
        if ledger_row_count(&probe).await != ledger_before {
            return Err("the refusal wrote a ledger row".to_string());
        }
        if table_exists(&probe, "collision_insert_probe").await {
            return Err("the refused migration created its table anyway".to_string());
        }

        // ---- the control: ONE variable changes, the filename ----
        std::fs::remove_file(&inserted)
            .map_err(|e| format!("remove the mid-corpus file: {e}"))?;
        if inserted.exists() {
            return Err(format!(
                "staged file {} survived removal, so the control run is not a control",
                inserted.display()
            ));
        }
        write_migration(corpus, COLLIDE_APPENDED_FILENAME, COLLIDE_INSERT_SOURCE)?;
        if COLLIDE_APPENDED_FILENAME < COLLIDE_LAST_FILENAME {
            return Err(format!(
                "{COLLIDE_APPENDED_FILENAME} no longer sorts last, so the control is not \
                 an append"
            ));
        }

        let run3 = run_platform_migrations(&cfg).await.map_err(|e| {
            format!("the same source renamed to sort last must apply, but failed: {e}")
        })?;
        if run3.files != 3 || run3.applied.len() != 1 {
            return Err(format!(
                "control run reported files={}, applied={}",
                run3.files,
                run3.applied.len()
            ));
        }
        if !table_exists(&probe, "collision_insert_probe").await {
            return Err("the renamed migration did not create its table".to_string());
        }
        if ledger_row_count(&probe).await != ledger_before + 1 {
            return Err("the renamed migration did not record a ledger row".to_string());
        }
        Ok(())
    }

    // ── the cluster-global concurrency regression ───────────────────────────

    /// TWO migrate runs, TWO fresh databases, ONE cluster, at the same time.
    ///
    /// This is the configuration every agent brief in this repo hands out — "you
    /// get a private TEST_DB on the shared :5440 cluster" — and for the two
    /// migrations that write shared catalogs, a private database is not
    /// isolation. The migrations that create roles write `pg_authid`,
    /// `pg_db_role_setting` and `pg_auth_members`, which are cluster-global, so
    /// the per-database advisory lock `run_platform_migrations` already holds
    /// does not exclude the peer at all. This test deliberately names no
    /// migration file: which files carry that DDL has already changed once
    /// (see the header of `platform::cluster_lock`), and the whole corpus is
    /// what it runs.
    ///
    /// Before the cluster lock this aborted one of the two runs with an
    /// infrastructure error carrying no test name — `tuple concurrently
    /// updated`, or a duplicate key on `pg_authid_rolname_index` /
    /// `pg_db_role_setting_databaseid_rol_index` — which is the shape most
    /// easily misread as flakiness or as the reader's own change.
    ///
    /// A SERIAL RUN OF THIS TEST PROVES NOTHING: serial is the configuration
    /// that already worked. The overlap assertion below is therefore part of the
    /// test, not decoration — it fails if a future change quietly makes the two
    /// runs sequential, which would leave every other assertion here passing
    /// vacuously.
    #[compio::test]
    async fn concurrent_migrates_of_two_databases_on_one_cluster_both_succeed() {
        let Some(url) = pg_url() else {
            zeroship_test_support::skip(
                "skipping the concurrent-migrate proof: no test database (set PG_TEST_URL \
                 to a DSN on :5440 to run)",
            );
            return;
        };
        // Exclude the SIBLING tests in this binary; the two runs *inside* this
        // test are concurrent on purpose.
        let _serial = DB_APPLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let db_a = format!("zs_concurrent_a_{stamp}");
        let db_b = format!("zs_concurrent_b_{stamp}");

        {
            let admin = admin_session(&url).await;
            for db in [&db_a, &db_b] {
                let _ = admin.batch(&format!("DROP DATABASE IF EXISTS \"{db}\"")).await;
                admin
                    .batch(&format!("CREATE DATABASE \"{db}\""))
                    .await
                    .unwrap_or_else(|e| panic!("CREATE DATABASE {db} failed: {e}"));
            }
        }

        let outcome = run_two_concurrently(
            &url,
            &db_a,
            &db_b,
            &migrations_dir(),
            platform_migration_files(),
        )
        .await;

        {
            let admin = admin_session(&url).await;
            for db in [&db_a, &db_b] {
                let _ = admin
                    .batch(&format!(
                        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                         WHERE datname = '{db}' AND pid <> pg_backend_pid()"
                    ))
                    .await;
                let _ = admin.batch(&format!("DROP DATABASE IF EXISTS \"{db}\"")).await;
            }
        }

        outcome.expect("two concurrent platform migrates on one cluster must both succeed");
    }

    /// The SENSITIVE half of the concurrency proof: the same two-runs-one-cluster
    /// shape, over a one-file corpus that creates roles nothing else has.
    ///
    /// WHY THIS EXISTS ALONGSIDE THE TEST ABOVE. The real-corpus version is
    /// faithful but blunt: MEASURED 2026-08-20 on :5440, it failed 1 run in 3
    /// with the cluster lock disabled, against 0 in 3 with it enabled. Two
    /// things blunt it. The two runs must drift into phase across 28 unrelated
    /// files before they reach the roles file, so whether their shared-catalog
    /// statements overlap is luck; and every platform role already exists on any
    /// cluster a suite has ever run against, so the engine's `ifNotExists` probe
    /// turns most of the op into a no-op and there is less left to collide.
    ///
    /// This version measured 3 failures in 5 per attempt under the same
    /// mutation, and 0 in 5 with the lock enabled; the round loop below turns
    /// that per-attempt 3-in-5 into a per-run near-certainty.
    ///
    /// This version removes both. The corpus is ONE file, so both runs reach the
    /// role statements within milliseconds of each other, and the role names are
    /// unique to this test run, so every `CREATE ROLE` and every unconditional
    /// `ALTER ROLE … SET search_path` genuinely executes on both sides.
    ///
    /// It is not a weaker test for being synthetic: the lock is driven by the
    /// LOWERED SQL, not by a filename, so this exercises the same classifier and
    /// the same bracket the platform corpus does. What is synthetic is only the
    /// precondition -- which is the one thing a shared cluster will not let the
    /// real corpus establish, since dropping the live platform roles would break
    /// every other suite using the server.
    #[compio::test]
    async fn concurrent_migrates_creating_the_same_roles_both_succeed() {
        let Some(url) = pg_url() else {
            zeroship_test_support::skip(
                "skipping the concurrent-role proof: no test database (set PG_TEST_URL \
                 to a DSN on :5440 to run)",
            );
            return;
        };
        let _serial = DB_APPLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // ROUNDS, because one collision attempt is not a test.
        //
        // MEASURED 2026-08-20 on :5440 with the cluster lock disabled, one
        // attempt per invocation: 3 failures in 5. With it enabled: 0 in 5. So a
        // single attempt would let the defect back in about two times in five.
        // Rounds are independent attempts, so five of them miss only if all five
        // do: ~0.4^5, about one percent. Each round costs roughly a second.
        //
        // Stopping at the first red keeps a genuine regression fast to see; the
        // round number is reported so a rare-vs-immediate failure is
        // distinguishable in the output.
        const ROUNDS: usize = 5;

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut failure: Option<String> = None;

        for round in 0..ROUNDS {
            // Fresh role names PER ROUND. Reusing them would leave round 2
            // onwards finding the roles already present, and the engine's
            // `ifNotExists` probe would skip the very statements under test --
            // every round after the first would pass without proving anything.
            //
            // Short enough to stay inside PostgreSQL's 63-byte identifier limit.
            let role_prefix = format!("zsrace{}r{round}", stamp % 1_000_000_000);
            let roles: Vec<String> = (0..8).map(|i| format!("{role_prefix}_{i}")).collect();

            let corpus = tempfile::tempdir().expect("create temporary race corpus");
            std::fs::write(
                corpus.path().join("20260101000000_race_probe.ts"),
                race_corpus_source(&roles),
            )
            .expect("write the race corpus");

            let db_a = format!("zs_rolerace_a_{stamp}_{round}");
            let db_b = format!("zs_rolerace_b_{stamp}_{round}");
            {
                let admin = admin_session(&url).await;
                for db in [&db_a, &db_b] {
                    let _ = admin.batch(&format!("DROP DATABASE IF EXISTS \"{db}\"")).await;
                    admin
                        .batch(&format!("CREATE DATABASE \"{db}\""))
                        .await
                        .unwrap_or_else(|e| panic!("CREATE DATABASE {db} failed: {e}"));
                }
            }

            let outcome = run_two_concurrently(&url, &db_a, &db_b, corpus.path(), 1).await;

            // Teardown runs before the assertion so a red round still leaves the
            // cluster clean. The roles are cluster-global: leaking one leaks it
            // into every other suite on this server.
            {
                let admin = admin_session(&url).await;
                for db in [&db_a, &db_b] {
                    let _ = admin
                        .batch(&format!(
                            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                             WHERE datname = '{db}' AND pid <> pg_backend_pid()"
                        ))
                        .await;
                    let _ = admin.batch(&format!("DROP DATABASE IF EXISTS \"{db}\"")).await;
                }
                for role in &roles {
                    let _ = admin.batch(&format!("DROP ROLE IF EXISTS \"{role}\"")).await;
                }
            }

            if let Err(error) = outcome {
                failure = Some(format!("round {round} of {ROUNDS}: {error}"));
                break;
            }
        }

        assert!(
            failure.is_none(),
            "two concurrent migrates creating the same roles must both succeed: {}",
            failure.unwrap_or_default()
        );
    }

    /// A one-file corpus whose whole content is role creation.
    ///
    /// `setSearchPath` is the load-bearing option, not decoration: the renderer
    /// emits it as a SEPARATE `ALTER ROLE … SET search_path` statement that
    /// `ifNotExists` does NOT guard, so it writes the cluster-global
    /// `pg_db_role_setting` unconditionally. That is the statement that produces
    /// `tuple concurrently updated`, as opposed to the duplicate-key abort the
    /// guarded `CREATE ROLE` produces.
    fn race_corpus_source(roles: &[String]) -> String {
        let mut out = String::from(
            "import { role } from \"@zeroship/migrate\";\n\n\
             export const name = \"race_probe\";\n\n\
             export function up() {\n",
        );
        for role in roles {
            out.push_str(&format!(
                "  role(\"{role}\").create({{ login: false, setSearchPath: [\"public\"], \
                 ifNotExists: true }});\n"
            ));
        }
        out.push_str("}\n\nexport function down() {}\n");
        out
    }

    /// Drive both migrates so they are genuinely in flight together, and report
    /// what happened. Returns `Ok(())` only if BOTH runs completed the whole
    /// corpus AND their execution windows actually overlapped.
    async fn run_two_concurrently(
        url: &str,
        db_a: &str,
        db_b: &str,
        corpus: &Path,
        expected_files: usize,
    ) -> Result<(), String> {
        let config_for = |dsn: String| PlatformMigrateConfig {
            database_url: dsn,
            migrations_dir: corpus.to_path_buf(),
            project_schema: "zeroship".to_string(),
            project_id: "zeroship".to_string(),
            cluster_lock_database: DEFAULT_CLUSTER_LOCK_DATABASE.to_string(),
        };
        let cfg_a = config_for(dsn_with_db(url, db_a));
        let cfg_b = config_for(dsn_with_db(url, db_b));

        // Run A on its OWN OS thread with its OWN compio runtime, released
        // together with B by a barrier.
        //
        // THIS USED TO BE `compio::runtime::spawn` -- two tasks cooperating on
        // one thread -- and that measured only 1 failure in 3 with the lock
        // disabled, on BOTH corpora. The reason is that the halves of a migrate
        // run that take real time are synchronous: V8 authoring and lowering
        // contain no await points, so a single-threaded pair does not interleave
        // there at all. B authored its whole corpus, and only when it finally
        // awaited the database did A begin authoring. The two runs met only
        // inside the short apply, which is why the collision was luck.
        //
        // Separate threads remove that: both runs author in parallel and reach
        // their first shared-catalog statement together, which is the condition
        // the test is supposed to create.
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let barrier_a = std::sync::Arc::clone(&barrier);
        let thread_a = std::thread::spawn(move || {
            compio::runtime::Runtime::new()
                .expect("build run A's compio runtime")
                .block_on(async move {
                    barrier_a.wait();
                    let started = std::time::Instant::now();
                    let result = run_platform_migrations(&cfg_a)
                        .await
                        .map_err(|e| e.to_string());
                    (started, std::time::Instant::now(), result)
                })
        });

        barrier.wait();
        let started_b = std::time::Instant::now();
        let result_b = run_platform_migrations(&cfg_b)
            .await
            .map_err(|e| e.to_string());
        let ended_b = std::time::Instant::now();

        let (spawned_start_a, ended_a, result_a) = thread_a
            .join()
            .map_err(|_| "run A panicked; see the captured output above".to_string())?;

        // Report BOTH outcomes. Reporting only the first failure would hide the
        // case where the peer failed differently, and the two runs fail in
        // different ways depending on which one lost the race.
        let mut failures = Vec::new();
        match &result_a {
            Ok(report) if report.files != expected_files => failures.push(format!(
                "run A applied {} files, expected {expected_files}",
                report.files
            )),
            Ok(_) => {}
            Err(e) => failures.push(format!("run A failed: {e}")),
        }
        match &result_b {
            Ok(report) if report.files != expected_files => failures.push(format!(
                "run B applied {} files, expected {expected_files}",
                report.files
            )),
            Ok(_) => {}
            Err(e) => failures.push(format!("run B failed: {e}")),
        }
        if !failures.is_empty() {
            return Err(failures.join(" | "));
        }

        // The anti-vacuity check. If A finished before B started (or vice
        // versa), the runs were serial and every assertion above passed for a
        // reason that has nothing to do with the lock under test.
        if ended_a <= started_b || ended_b <= spawned_start_a {
            return Err(format!(
                "the two runs did NOT overlap, so this test proved nothing about \
                 concurrency: A ran for {:?}, B ran for {:?}",
                ended_a.duration_since(spawned_start_a),
                ended_b.duration_since(started_b),
            ));
        }

        Ok(())
    }

    // ===================================================================
    // RELEASED-BYTES GUARD
    // ===================================================================

    /// Repo-root path of the checked-in journal snapshot.
    const RELEASED_LEDGER_PATH: &str = "db/released_migrations.tsv";

    /// The checksums a DEPLOYED database has already recorded for the migration
    /// files it applied, read from `db/released_migrations.tsv` -- a verbatim
    /// dump of that database's `zeroship_migrations.platform_migration_files`.
    ///
    /// WHY A FILE AND NOT THE `[(&str, &str); 21]` THAT USED TO BE HERE. The
    /// array was correct the day it was written: 21 files applied, 21 entries.
    /// A deploy then applied thirteen more and nothing re-derived it, so its
    /// coverage was bound to WHEN IT WAS WRITTEN rather than to WHAT IS
    /// APPLIED, and it reported the identical green either way. Twelve of the
    /// thirteen uncovered files were freely editable; one of them was in fact
    /// edited on a branch and nothing fired.
    ///
    /// A file fixes only half of that -- it is still a copy, and a copy can
    /// still go stale. What removes the drift is that
    /// `deploy/scripts/deploy-remote.sh` REWRITES this file from the journal it
    /// just wrote and exits non-zero when that produced a diff, so the deploy
    /// that freezes new files is the same act that records them. See
    /// `released_platform_migrations_cover_every_deployed_file` for what that
    /// still does not cover.
    ///
    fn released_platform_migrations() -> Vec<(String, String)> {
        let path = migrations_dir()
            .parent()
            .expect("db/migrations-ts has a parent")
            .parent()
            .expect("repo root above db/")
            .join(RELEASED_LEDGER_PATH);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        text.lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                let (filename, checksum) = line.split_once('\t').unwrap_or_else(|| {
                    panic!("{RELEASED_LEDGER_PATH}: not <filename>TAB<sha256>: {line:?}")
                });
                (filename.to_string(), checksum.to_string())
            })
            .collect()
    }

    // WHY THIS GUARD EXISTS. The runner hashes each file's source bytes and
    // refuses any file whose hash no longer matches what the journal recorded
    // (`platform.rs`, `PlatformMigrateError::ChecksumMismatch`). The guard is
    // correct and it cannot self-heal: once a deployed database has journalled a
    // file, EDITING that file bricks every future migrate run against it, and
    // the only repair is to restore the released bytes and re-land the change as
    // a new file.
    //
    // Nothing detected that. Four commits on 2026-08-16 edited five
    // already-applied files in place; every test passed, because every test
    // applies the corpus to an EMPTY database, where the journal is written from
    // the same bytes it is later checked against and so always agrees. The
    // failure needs a journal written from DIFFERENT bytes, which only a
    // deployed database had. The checked-in snapshot is that database's half
    // of the comparison, brought into the repo so the check can run without one.

    /// A released migration file's bytes must never change.
    ///
    /// DB-FREE and therefore always runs. This is the check that would have
    /// failed on 2026-08-16 the moment the first of the five edits was made,
    /// naming the file, instead of surfacing three months later as
    /// `service "migrate" didn't complete successfully` on a production roll.
    ///
    /// WHAT THIS DOES NOT CATCH. Exactly one thing is asserted: that the files
    /// in `db/released_migrations.tsv` still hash to the recorded values. It
    /// does NOT check that the deltas removed from those files were re-landed
    /// anywhere, that the corpus still produces the intended schema, or that a
    /// file absent from the snapshot is unedited -- and the last of those is
    /// not an oversight but the design: a file no deployed database has
    /// journalled is not frozen, and freezing it would contradict the
    /// pre-launch stance for schema that has never shipped. A commit that
    /// reverted files and dropped their changes on the floor also passes; the
    /// end-state equivalence that rules that out is asserted by the other tests
    /// in this module, not here.
    #[test]
    fn released_platform_migrations_keep_their_released_bytes() {
        let dir = migrations_dir();
        let mut drifted = Vec::new();
        for (filename, released) in released_platform_migrations() {
            let path = dir.join(&filename);
            let source = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("read released migration {}: {e}", path.display()));
            let current = zero_migrate::manifest_entry::sha256_hex(&source);
            if current != released {
                drifted.push(format!(
                    "  {filename}\n    released {released}\n    current  {current}"
                ));
            }
        }
        assert!(
            drifted.is_empty(),
            "{} released migration file(s) were edited after they were applied to a \
             deployed database. That database's journal still holds the released \
             checksum, so its next migrate run refuses with ChecksumMismatch and \
             cannot recover. Restore the released bytes and re-land the change as a \
             NEW file:\n{}",
            drifted.len(),
            drifted.join("\n")
        );
    }

    /// Every file the snapshot names must still be in `db/migrations-ts`.
    ///
    /// Deleting an applied migration is the same defect as editing one: the
    /// runner's next pass over that database still expects it and refuses.
    /// Without this the bytes guard above would panic on the failed read with
    /// no explanation of why the file mattered.
    ///
    /// This is also what catches the corpus change the deleted
    /// `PLATFORM_MIGRATION_FILES` constant could not see. Delete one applied
    /// migration and add another and the file count is unchanged, so a count
    /// agrees with itself; the snapshot is an EXTERNAL list of names and names
    /// the missing one.
    ///
    /// This is containment only. Ordering is a SEPARATE and equally hard
    /// requirement, asserted by
    /// `unreleased_migrations_sort_after_every_released_one` below.
    ///
    /// WHAT THIS DOES NOT CATCH: whether the snapshot is CURRENT. A repo cannot
    /// read production's journal, so nothing here can tell a snapshot of 34
    /// applied files from one of 21 while thirteen more are live. That is the
    /// deploy's job -- see
    /// `released_platform_migrations_cover_every_deployed_file`.
    #[test]
    fn released_platform_migrations_all_exist_in_the_corpus() {
        let corpus = platform_migration_filenames();
        let released = released_platform_migrations();
        let missing: Vec<&str> = released
            .iter()
            .map(|(filename, _)| filename.as_str())
            .filter(|filename| !corpus.iter().any(|have| have == filename))
            .collect();
        assert!(
            missing.is_empty(),
            "{RELEASED_LEDGER_PATH} names {} migration(s) that db/migrations-ts no longer \
             carries: {missing:?}. A deployed database has journalled them, so its next \
             migrate run expects them and refuses without them. Restore the files; if the \
             change they carry must go, land the removal as a NEW migration.",
            missing.len()
        );
    }

    /// A migration that is not yet deployed must sort AFTER every one that is.
    ///
    /// WHY THIS IS NOT A STYLE RULE. `platform.rs` stamps every lowered step
    /// with a version derived from the file's ORDINAL in sorted-filename order
    /// (`restamp_stable_versions`, `file_ordinal * FILE_VERSION_STRIDE +
    /// step_index`); the filename itself reaches the id only through an error
    /// label. So a file inserted mid-corpus takes the version a LATER file
    /// already owns, and every file after it shifts up one. The deployed
    /// database's journal still holds the old mapping, so the next migrate run
    /// compares a recorded checksum against a DIFFERENT file's body and aborts
    /// with `ChecksumDrift` -- naming the new file, which is not the one that
    /// changed, and saying nothing about ordering.
    ///
    /// MEASURED, and this is the whole reason the test exists. On 2026-08-20
    /// `20260819000000_app_egress_rules.ts` landed on main while the deployed
    /// journal ended at `20260820000000_control_workflow_journal_access.ts`.
    /// That journal (`zeroship_migrations.schema_migrations`, 668 rows) records
    ///
    ///     mig_0000E9Uuwao9JYwWBWok52 -> 299299267e22b313e3036804b0a14014...
    ///
    /// as its highest version, which is ordinal 33 -- the last file's only
    /// step. In the merged corpus ordinal 33 is `app_egress_rules`, whose body
    /// hashes to 6ac171d3..., so the next deploy to that host would have
    /// aborted. Nothing else in the tree could see it: the file is new, so no
    /// ledger row covers its bytes, and its own content is perfectly fine.
    ///
    /// THE FIX IS ALWAYS TO RENAME the undeployed file so it sorts last. It is
    /// not in any journal yet, so renaming it costs nothing -- which is exactly
    /// why this must fail before the deploy rather than during it.
    ///
    /// WHAT THIS DOES NOT CATCH. Two applied files cannot swap order, so this
    /// only ever polices the undeployed tail. It also says nothing about a file
    /// that sorts last but is otherwise wrong, and -- like every check here --
    /// it measures against ONE deployment's journal.
    #[test]
    fn unreleased_migrations_sort_after_every_released_one() {
        let corpus = platform_migration_filenames();
        let released = released_platform_migrations();
        let Some(newest_released) = released.iter().map(|(f, _)| f.as_str()).max() else {
            return;
        };
        let offenders: Vec<&String> = corpus
            .iter()
            .filter(|name| !released.iter().any(|(f, _)| f == *name))
            .filter(|name| name.as_str() < newest_released)
            .collect();
        assert!(
            offenders.is_empty(),
            "these migrations are NOT yet applied anywhere but sort BEFORE \
             {newest_released}, which is: {offenders:?}\n\
             platform.rs derives each file's journal version from its ordinal in \
             sorted-filename order, so inserting one mid-corpus takes a version the \
             deployed journal already recorded for a different file. The next \
             migrate run against that database aborts with ChecksumDrift naming \
             the NEW file, which is not the one that changed.\n\
             Rename the file(s) above so they sort after {newest_released}. They \
             are in no journal yet, so a rename costs nothing."
        );
    }

    /// The DEPLOY is what keeps the snapshot honest; this test only says so.
    ///
    /// THE DESIGN, and why the alternatives lose. The snapshot cannot verify
    /// its own coverage: only a deployed database knows which files it has
    /// journalled, and a test has no route to one. Three shapes were weighed.
    ///
    ///   Recompute each entry from the tree at land time. REJECTED, and it is
    ///   the trap rather than the obvious answer: a checksum derived from the
    ///   file is updated BY the edit, so the guard would endorse precisely the
    ///   change it exists to refuse. Worse for an agent than for a human -- the
    ///   failure names the file and the "fix" that makes it green is the
    ///   catastrophic one.
    ///
    ///   Refresh by hand from a documented command. REJECTED alone: it is
    ///   exactly today's defect with a shorter procedure. Nobody re-ran the
    ///   array for thirteen files and nobody would re-run a command either.
    ///
    ///   Have the deploy rewrite it. CHOSEN. `deploy-remote.sh` is the only
    ///   place that both knows the journal and runs every time the journal
    ///   changes, so coverage tracks deploys by construction. It writes the
    ///   file into the operator's own checkout -- not into the deploy path, and
    ///   not into the remote host -- and exits non-zero when that produced a
    ///   diff, so a deploy from a tree with a stale snapshot ends in a failure
    ///   naming the files to commit rather than in silence.
    ///
    /// WHAT THIS STILL DOES NOT CATCH, and it is worth naming because the green
    /// reads stronger than it is:
    ///
    ///   - A file edited BEFORE it was ever deployed is invisible to every
    ///     check here. It is not in the snapshot, so no checksum covers it.
    ///     That is intended -- it is not frozen yet -- but it means "green"
    ///     never meant "no migration was edited".
    ///   - A deploy to a SECOND database. The snapshot is one deployment's
    ///     journal. A different cluster, further behind, can hold checksums
    ///     nothing here has ever seen.
    ///   - An operator who ignores the deploy's non-zero exit. Nothing can
    ///     write the commit for them.
    #[test]
    fn released_platform_migrations_cover_every_deployed_file() {
        let released = released_platform_migrations();
        assert!(
            !released.is_empty(),
            "{RELEASED_LEDGER_PATH} is empty; the released-bytes guard would then \
             assert nothing at all and still report green"
        );
        for (filename, checksum) in &released {
            assert_eq!(
                checksum.len(),
                64,
                "{RELEASED_LEDGER_PATH}: {filename} has a {}-char checksum, not a sha256",
                checksum.len()
            );
            assert!(
                checksum.chars().all(|c| c.is_ascii_hexdigit()),
                "{RELEASED_LEDGER_PATH}: {filename} checksum is not hex: {checksum}"
            );
        }
    }

    /// The same guard, driven through the real runner against a real database
    /// whose ledger carries the RELEASED checksums rather than the corpus's own.
    ///
    /// The precondition a fresh-database run can never reproduce is seeded
    /// explicitly: apply the released prefix, then overwrite the completion
    /// ledger with the checksums the deployed database actually recorded, then
    /// run the WHOLE corpus the way a deploy does. On the corpus as it stood on
    /// 2026-08-19 this stops on the first drifted file with
    /// `migration file 20260702000100_schema_roles_extensions.ts was edited after
    /// it was applied`.
    ///
    /// WHAT THIS DOES NOT CATCH. It proves the checksum GATE is passed and the
    /// remaining files apply on top of a released-prefix database. It says
    /// nothing about whether that database ends up shaped like a fresh one -- it
    /// asserts no schema, no grant and no row. It also seeds only the ledger, so
    /// a deployed database that additionally carries objects no migration created
    /// is outside what this covers.
    #[compio::test]
    async fn platform_migrate_accepts_a_ledger_of_released_checksums() {
        let Some(url) = pg_url() else {
            zeroship_test_support::skip(
                "skipping released-ledger proof: no test database (set PG_TEST_URL \
                 to a DSN to run)"
            );
            return;
        };
        let _serial = DB_APPLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let corpus = tempfile::tempdir().expect("create temporary migration corpus");
        let scratch = create_scratch_database(&url, "zs_released_ledger")
            .await
            .expect("create released-ledger scratch database");

        let result = run_released_ledger_assertions(&scratch.dsn, corpus.path()).await;
        drop_scratch_database(&url, &scratch)
            .await
            .expect("drop released-ledger scratch database");
        result.expect("the corpus must apply over a ledger of released checksums");
    }

    async fn run_released_ledger_assertions(
        scratch_dsn: &str,
        corpus: &Path,
    ) -> Result<(), String> {
        // Seed the corpus with EXACTLY the files the snapshot names, by name.
        //
        // This used to copy the oldest N files and assert the snapshot was that
        // prefix. Both halves were wrong for the same reason: a migration can
        // land with an earlier timestamp than one already deployed
        // (`20260819000000_app_egress_rules` did, after `20260820000000_...`
        // was applied), and then "the oldest N" and "what the journal holds"
        // are different sets. Copying by name cannot drift that way, and it
        // removes an ordering assumption the runner never made.
        let released = released_platform_migrations();
        let names: Vec<&str> = released
            .iter()
            .map(|(filename, _)| filename.as_str())
            .collect();
        copy_migration_files(corpus, &names)?;
        let cfg = test_config(scratch_dsn, corpus);
        run_platform_migrations(&cfg)
            .await
            .map_err(|e| format!("released prefix apply failed: {e}"))?;

        // SEED THE PRODUCTION FACT. Up to here the ledger holds the corpus's own
        // checksums, which trivially agree with it -- that agreement is exactly
        // why a fresh-database test cannot see this bug. Replacing them with the
        // checksums a deployed database recorded is the whole experiment.
        let probe = CompioPgSession::connect(scratch_dsn)
            .await
            .map_err(|e| format!("connect released-ledger probe: {e}"))?;
        for (filename, checksum) in &released {
            probe
                .batch(&format!(
                    "UPDATE zeroship_migrations.{PLATFORM_MIGRATION_LEDGER_TABLE} \
                     SET checksum = '{checksum}' WHERE filename = '{filename}'"
                ))
                .await
                .map_err(|e| format!("seed released checksum for {filename}: {e}"))?;
        }
        let seeded = ledger_row_count(&probe).await;
        if seeded != released.len() as i64 {
            return Err(format!(
                "seeded ledger holds {seeded} rows, expected {}",
                released.len()
            ));
        }

        copy_migration_corpus(corpus)?;
        let run = run_platform_migrations(&cfg)
            .await
            .map_err(|e| format!("full corpus over a released ledger failed: {e}"))?;
        let expected_files = platform_migration_files();
        if run.files != expected_files {
            return Err(format!(
                "full corpus reported {} files, expected {expected_files}",
                run.files
            ));
        }
        // The load-bearing assertion is that the call above returned Ok at all:
        // every released file's ledger row now holds the DEPLOYED database's
        // checksum, so a single byte of drift would have come back as
        // ChecksumMismatch. That is the whole experiment.
        //
        // These two say the run was not vacuous. `skipped` and `applied` are
        // STEP lists, not file lists -- 34 files skip as 668 steps -- so they
        // can only be checked for emptiness, and the second is written as an
        // equivalence rather than `!applied.is_empty()`: the snapshot
        // legitimately covers the WHOLE corpus in steady state, between a deploy
        // and the next migration landing, and asserting that something applied
        // turns that healthy state into a failure.
        if run.skipped.is_empty() {
            return Err(
                "the released prefix was re-applied rather than skipped, so the seeded \
                 checksums were never the ones the runner compared against and this test \
                 proved nothing"
                    .to_string(),
            );
        }
        if run.applied.is_empty() != (released.len() == expected_files) {
            return Err(format!(
                "the corpus holds {expected_files} files of which {} are released, but the \
                 run applied {} steps; it must apply nothing when the snapshot covers the \
                 corpus and something when it does not",
                released.len(),
                run.applied.len()
            ));
        }
        Ok(())
    }
}
