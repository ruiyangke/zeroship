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

    use zero_migrate::driver::SqlSession;
    use zeroship_migrate_adapter::platform::{
        author_and_lower_all, run_platform_migrations, PlatformMigrateConfig,
    };
    use zeroship_migrate_adapter::CompioPgSession;

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
}
