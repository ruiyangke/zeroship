//! Phase F Stage 4a — prove the platform-schema migrate path on the PUBLISHED
//! engine: every `db/migrations-ts/*.ts` file AUTHORS (zeroship-runtime V8 + the
//! standalone v1 recorder) and LOWERS (fail-closed load gate + Platform-guarded
//! lower) cleanly on the standalone (v1) engine.
//!
//! The author+lower half needs NO database and NO privileged executor seam, so it
//! runs UNCONDITIONALLY — it is the maximal claim the published engine's PUBLIC API
//! lets a monorepo consumer prove today.
//!
//! The APPLY half is BLOCKED by a genuine engine gap: the published engine exposes
//! no public seam to build a Platform-trust `ExecutorConfig`
//! (`ExecutorConfig::platform` is `#[cfg(test)] pub(crate)`), so the reachable
//! Confined executor guard denies platform DDL (CREATE SCHEMA / roles / grants /
//! cross-schema / functions). The `apply_blocked_by_confined_executor_seam` test
//! documents + pins that exact blocker (gated on a DB URL).

#[cfg(feature = "platform-cli")]
mod platform_cli {
    use std::path::PathBuf;

    use zeroship_migrate_adapter::platform::{
        author_and_lower_all, run_platform_migrations, PlatformMigrateConfig, PlatformMigrateError,
    };

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

    /// PIN the Stage 4a engine gap: with only the PUBLIC engine API (the reachable
    /// Confined `ExecutorConfig::new`), applying the platform migrations fail-closes
    /// at the first platform-DDL op. Gated on `ZERO_MIGRATE_TEST_PG_URL` (skips
    /// clean when unset). This documents the blocker; it is NOT a hack around it.
    #[compio::test]
    async fn apply_blocked_by_confined_executor_seam() {
        let Some(url) = pg_url() else {
            eprintln!(
                "skipping Stage 4a apply-gap pin: ZERO_MIGRATE_TEST_PG_URL unset \
                 (set it to a scratch DSN on :5440 to run)"
            );
            return;
        };

        let cfg = PlatformMigrateConfig {
            database_url: url,
            migrations_dir: migrations_dir(),
            project_schema: "zeroship".to_string(),
            project_id: "zeroship".to_string(),
        };

        let err = run_platform_migrations(&cfg)
            .await
            .expect_err("apply must fail-closed under the reachable Confined executor");

        match err {
            PlatformMigrateError::Apply { file, message } => {
                assert!(
                    file.contains("schema_roles_extensions"),
                    "the block must hit the FIRST platform file, got {file}"
                );
                assert!(
                    message.contains("denied by guard")
                        || message.to_lowercase().contains("dangerous"),
                    "the block must be a guard denial (Confined executor), got: {message}"
                );
            }
            other => panic!("expected an Apply guard denial, got: {other}"),
        }
    }
}
