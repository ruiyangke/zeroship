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
//! `ZERO_MIGRATE_TEST_PG_URL` (skips clean when unset).

#[cfg(feature = "platform-cli")]
mod platform_cli {
    use std::path::{Path, PathBuf};

    use std::sync::Mutex;

    use zero_migrate::driver::SqlSession;
    use zeroship_migrate_adapter::platform::{
        author_and_lower_all, run_platform_migrations, PlatformMigrateConfig, PlatformMigrateError,
        PLATFORM_MIGRATION_LEDGER_TABLE,
    };
    use zeroship_migrate_adapter::CompioPgSession;

    /// Serialize the live-PG apply tests. Some provision a scratch DB and create
    /// the platform's CLUSTER-GLOBAL roles (`CREATE ROLE zeroship_control`, …); run in
    /// parallel they race on the shared `pg_authid` catalog and PG aborts one with
    /// `tuple concurrently updated`. `cargo test` runs test fns on multiple OS threads
    /// by default, so this process-wide lock (not `--test-threads=1`) keeps the DB
    /// apply tests from overlapping regardless of the caller's thread count. The
    /// DB-free author+lower test does not take it.
    static DB_APPLY_LOCK: Mutex<()> = Mutex::new(());

    /// How many files `db/migrations-ts` holds. Asserted rather than derived so
    /// that a discovery bug which silently drops a file fails loudly instead of
    /// agreeing with itself. Adding a migration updates this one constant.
    const PLATFORM_MIGRATION_FILES: usize = 12;

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
        std::env::var("ZERO_MIGRATE_TEST_PG_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
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

        assert_eq!(
            lowered.len(),
            PLATFORM_MIGRATION_FILES,
            "expected {PLATFORM_MIGRATION_FILES} platform migrations, got {}: {:?}",
            lowered.len(),
            lowered.iter().map(|(f, _)| f).collect::<Vec<_>>()
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
    /// grants landed. Gated on `ZERO_MIGRATE_TEST_PG_URL` (skips clean when unset).
    /// The scratch DB is created and dropped by this test.
    #[compio::test]
    async fn apply_all_platform_migrations_to_fresh_db() {
        let Some(url) = pg_url() else {
            eprintln!(
                "skipping the full-apply proof: ZERO_MIGRATE_TEST_PG_URL unset \
                 (set it to a DSN on :5440 to run)"
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
        };
        let report = run_platform_migrations(&cfg)
            .await
            .map_err(|e| format!("run_platform_migrations failed: {e}"))?;
        if report.files != PLATFORM_MIGRATION_FILES {
            return Err(format!(
                "expected {PLATFORM_MIGRATION_FILES} files, saw {}",
                report.files
            ));
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

        Ok(())
    }

    /// Exercise the real ordered runner across a catalog refresh and verify that
    /// the later workflow migration can still validate foreign-key value formats
    /// authored by an earlier file. The environment gate matches the other live-PG
    /// coverage in this suite.
    #[compio::test]
    async fn ordered_runner_retains_authored_fk_formats_across_catalog_refresh() {
        let Some(url) = pg_url() else {
            eprintln!(
                "skipping logical-column retention regression: ZERO_MIGRATE_TEST_PG_URL unset \
                 (set it to a DSN on :5440 to run)"
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
        };
        let report = run_platform_migrations(&cfg)
            .await
            .map_err(|e| format!("run_platform_migrations failed: {e}"))?;
        if report.files != 12 {
            return Err(format!("expected 12 files, saw {}", report.files));
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
    /// "account_state" already exists`). Gated on `ZERO_MIGRATE_TEST_PG_URL`.
    #[compio::test]
    async fn platform_migrate_is_idempotent_on_rerun() {
        let Some(url) = pg_url() else {
            eprintln!(
                "skipping idempotency proof: ZERO_MIGRATE_TEST_PG_URL unset \
                 (set it to a DSN on :5440 to run)"
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

    const APPEND_FILENAME: &str = "20260710000100_append_probe.ts";
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
        }
    }

    #[compio::test]
    async fn platform_migrate_applies_only_newly_appended_file() {
        let Some(url) = pg_url() else {
            eprintln!(
                "skipping appended-file proof: ZERO_MIGRATE_TEST_PG_URL unset \
                 (set it to a DSN on :5440 to run)"
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

    async fn run_appended_file_assertions(scratch_dsn: &str, corpus: &Path) -> Result<(), String> {
        let cfg = test_config(scratch_dsn, corpus);
        let run1 = run_platform_migrations(&cfg)
            .await
            .map_err(|e| format!("initial corpus apply failed: {e}"))?;
        if run1.files != PLATFORM_MIGRATION_FILES {
            return Err(format!(
                "initial corpus reported {} files, expected {PLATFORM_MIGRATION_FILES}",
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
        write_migration(corpus, APPEND_FILENAME, APPEND_SOURCE)?;

        let run2 = run_platform_migrations(&cfg)
            .await
            .map_err(|e| format!("appended corpus apply failed: {e}"))?;
        if run2.files != PLATFORM_MIGRATION_FILES + 1 {
            return Err(format!(
                "appended corpus reported {} files, expected {}",
                run2.files,
                PLATFORM_MIGRATION_FILES + 1
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
        if ledger_rows != (PLATFORM_MIGRATION_FILES + 1) as i64 {
            return Err(format!(
                "file ledger contains {ledger_rows} rows, expected {}",
                PLATFORM_MIGRATION_FILES + 1
            ));
        }
        Ok(())
    }

    #[compio::test]
    async fn platform_migrate_resumes_partially_applied_corpus() {
        let Some(url) = pg_url() else {
            eprintln!(
                "skipping corpus-prefix resume proof: ZERO_MIGRATE_TEST_PG_URL unset \
                 (set it to a DSN on :5440 to run)"
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
        if resumed.files != PLATFORM_MIGRATION_FILES {
            return Err(format!(
                "complete corpus reported {} files, expected {PLATFORM_MIGRATION_FILES}",
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
        if ledger_row_count(&probe).await != PLATFORM_MIGRATION_FILES as i64 {
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
            eprintln!(
                "skipping partial-file proof: ZERO_MIGRATE_TEST_PG_URL unset \
                 (set it to a DSN on :5440 to run)"
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
            eprintln!(
                "skipping edited-file proof: ZERO_MIGRATE_TEST_PG_URL unset \
                 (set it to a DSN on :5440 to run)"
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
}
