//! Phase F Stage 4a — prove the platform-schema migrate path on the PUBLISHED
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
//! applies all 11 migrations to a FRESH scratch database on :5440 and
//! INDEPENDENTLY asserts (via a second connection) that the expected platform
//! schemas/tables/RLS-policy/function+trigger/grants landed. Gated on
//! `ZERO_MIGRATE_TEST_PG_URL` (skips clean when unset).

#[cfg(feature = "platform-cli")]
mod platform_cli {
    use std::path::PathBuf;

    use std::sync::Mutex;

    use zero_migrate::driver::SqlSession;
    use zeroship_migrate_adapter::platform::{
        author_and_lower_all, run_platform_migrations, PlatformMigrateConfig,
    };
    use zeroship_migrate_adapter::CompioPgSession;

    /// Serialize the three live-PG apply tests. Each provisions a scratch DB and creates
    /// the platform's CLUSTER-GLOBAL roles (`CREATE ROLE zeroship_control`, …); run in
    /// parallel they race on the shared `pg_authid` catalog and PG aborts one with
    /// `tuple concurrently updated`. `cargo test` runs test fns on multiple OS threads
    /// by default, so this process-wide lock (not `--test-threads=1`) keeps the DB
    /// apply tests from overlapping regardless of the caller's thread count. The
    /// DB-free author+lower test does not take it.
    static DB_APPLY_LOCK: Mutex<()> = Mutex::new(());

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

        // There are 11 platform migration files today.
        assert_eq!(
            lowered.len(),
            11,
            "expected 11 platform migrations, got {}: {:?}",
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

    /// PROVE Stage 4a end to end: apply ALL 11 platform migrations to a FRESH
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
                "skipping Stage 4a full-apply proof: ZERO_MIGRATE_TEST_PG_URL unset \
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

        result.expect("Stage 4a full apply + assertions must pass");
    }

    /// Apply all 11 migrations to `scratch_dsn` and independently assert the schema.
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
        if report.files != 11 {
            return Err(format!("expected 11 files, saw {}", report.files));
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
    /// scratch DB: run 1 applies all 11 files' migrations (N applied, 0 skipped); run
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
}
