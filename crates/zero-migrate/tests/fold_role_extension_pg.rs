//! `createRole` and `createExtension` through the fold oracle, against live
//! PostgreSQL.
//!
//! `fold_roundtrip_pg.rs` covers roughly thirty op kinds and not these two, and
//! the reason turned out to be worth writing down. Of the op families it omits,
//! most cannot be checked this way at all: `setTableOptions` folds into
//! `runtime_options` (soft-delete, versioning) which have no catalog
//! representation, and `createSchema` folds to a snapshot live introspection
//! cannot match, because `snapshot_schema` narrows its namespace query to one
//! name - see `fold_cross_schema_drift_pg.rs`.
//!
//! Roles and extensions are the ones that CAN. Their live queries are
//! cluster-wide, so what the fold records and what introspection returns are
//! comparable, and `diff_snapshots` has something real to say.
//!
//! Two properties, and the second is the one worth having:
//!
//!   1. applying these ops and folding the same ops agree - `is_clean()`;
//!   2. the oracle NOTICES when they stop agreeing. A clean diff on its own is
//!      equally consistent with a differ that ignores roles and extensions
//!      entirely, which is exactly what a fold oracle must not do. So each is
//!      then removed out of band and the diff must go dirty and name it.
//!
//! Without (2) this file would pass against a build where `SchemaSnapshot`
//! dropped both fields.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`.

mod support;

use std::collections::BTreeMap;

use support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    diff_snapshots, effective_policy_from_charter_toml, fold_ops, snapshot_schema, Approval,
    EffectivePolicy, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine,
    MigrationIr, PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_fold_role_extension";
/// Available on the test image, and not installed by default - so creating it is
/// a real catalog change rather than a no-op the oracle could not see.
const EXTENSION: &str = "unaccent";

fn token(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "foldre_{tag}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// The operator charter, plus the extension allowlist `createExtension` needs.
fn charter(schema: &str) -> EffectivePolicy {
    let toml = format!(
        r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = {{ include = [{schema:?}] }}

[[grant]]
key = "schema.create_table"
value = true
scope = {{ include = [{schema:?}] }}

[[grant]]
key = "access.role"
value = true
scope = "all"

[[grant]]
key = "code.extension"
value = [{EXTENSION:?}]
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#
    );
    effective_policy_from_charter_toml(&toml).expect("charter composes")
}

#[compio::test]
async fn a_role_and_an_extension_fold_to_what_live_introspection_reports() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token("proj");
    // PostgreSQL folds unquoted role names to lower case; keep the authored name
    // already lower so the comparison is about the fold and not about casing.
    let role = token("role").to_lowercase();
    let policy = charter(&schema);
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy.clone());
    let _guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!(
            "CREATE SCHEMA {}",
            quote_ident(&cfg.project_schema)
        ))
        .await
        .expect("create the project schema");

    let work: Result<(), String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure journal: {error}"))?;

        let doc = serde_json::json!({
            "ir_version": 1,
            "name": "role_and_extension",
            "owner_app": OWNER,
            "ops": [
                { "op": "createExtension", "name": EXTENSION, "ifNotExists": true },
                { "op": "createRole", "name": role },
                {
                    "op": "createTable",
                    "name": "t1",
                    "columns": [{ "name": "id", "type": "int", "nullable": false }],
                    "primaryKey": ["id"]
                }
            ]
        })
        .to_string();

        let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, &policy);
        let guard_cfg = GuardConfig::from_policy(policy.clone(), SqlDialect::Postgres);
        let base = fold_ops(&[], SqlDialect::Postgres, &cfg.project_schema, &policy)
            .map_err(|error| format!("fold the empty base: {error}"))?;
        let live = LiveSchema::from_catalog_snapshot(base, OWNER);
        let artifact = author
            .load_and_lower_guarded(&doc, OWNER, &BTreeMap::new(), &live, &guard_cfg)
            .map_err(|error| format!("lower: {error}"))?;
        MigrationEngine::new()
            .apply_plan(
                &artifact.plan.steps,
                Approval::Approved,
                &backend,
                &cfg,
                OWNER,
                LockMode::Acquire,
            )
            .await
            .map_err(|error| format!("apply: {error}"))?;

        let authored: MigrationIr =
            serde_json::from_str(&doc).map_err(|error| format!("parse the IR: {error}"))?;
        let expected = fold_ops(
            &authored.ops,
            SqlDialect::Postgres,
            &cfg.project_schema,
            &policy,
        )
        .map_err(|error| format!("fold the authored ops: {error}"))?;

        // The fold must actually carry them, or (1) below is vacuous.
        assert!(
            expected.roles.contains_key(&role),
            "the fold must record the role it created: {:?}",
            expected.roles.keys().collect::<Vec<_>>()
        );
        assert!(
            expected.extensions.contains_key(EXTENSION),
            "the fold must record the extension it created: {:?}",
            expected.extensions.keys().collect::<Vec<_>>()
        );

        // (1) Applied and folded agree.
        let actual = snapshot_schema(&session, &cfg.project_schema)
            .await
            .map_err(|error| format!("snapshot: {error}"))?;
        let drift = diff_snapshots(&expected, &actual);
        assert!(
            drift.is_clean(),
            "a role and an extension must round-trip: missing={:?} altered={:?}",
            drift.missing_objects,
            drift.altered_objects
        );

        // (2) And the oracle notices when they stop agreeing. Removed one at a
        // time, so each failure names the object it is about rather than both.
        session
            .batch(&format!("DROP ROLE {}", quote_ident(&role)))
            .await
            .map_err(|error| format!("drop the role: {error}"))?;
        let after_role = snapshot_schema(&session, &cfg.project_schema)
            .await
            .map_err(|error| format!("snapshot after dropping the role: {error}"))?;
        assert_eq!(
            diff_snapshots(&expected, &after_role).missing_objects,
            vec![format!("role {role}")],
            "dropping the role out of band must be reported"
        );

        session
            .batch(&format!("DROP EXTENSION {}", quote_ident(EXTENSION)))
            .await
            .map_err(|error| format!("drop the extension: {error}"))?;
        let after_both = snapshot_schema(&session, &cfg.project_schema)
            .await
            .map_err(|error| format!("snapshot after dropping the extension: {error}"))?;
        let mut both = diff_snapshots(&expected, &after_both).missing_objects;
        both.sort();
        assert_eq!(
            both,
            vec![format!("extension {EXTENSION}"), format!("role {role}")],
            "dropping the extension out of band must be reported too"
        );
        Ok(())
    }
    .await;

    let _ = session
        .batch(&format!("DROP ROLE IF EXISTS {}", quote_ident(&role)))
        .await;
    let _ = session
        .batch(&format!(
            "DROP EXTENSION IF EXISTS {}",
            quote_ident(EXTENSION)
        ))
        .await;
    session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE; DROP SCHEMA IF EXISTS {} CASCADE",
            quote_ident(&cfg.project_schema),
            quote_ident(&cfg.pg.meta_schema)
        ))
        .await
        .expect("drop the test schemas");
    work.expect("fold a role and an extension against live PostgreSQL");
}
