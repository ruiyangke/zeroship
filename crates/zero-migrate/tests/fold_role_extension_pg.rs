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

/// Role ATTRIBUTES, not just role existence.
///
/// The test above creates a bare-named role and proves the oracle notices when it
/// is dropped. That exercises the existence half of the comparison and none of the
/// attribute half: `diff_role_attrs` compares `login`, `superuser`, `create_db`,
/// `create_role` and the rest, and produces `altered` entries — a path nothing has
/// ever driven against a live server.
///
/// Those fields are authorable (`createRole` carries `login` / `createDb` /
/// `createRole` / `bypassRls`; `superuser` is denied at render in every profile),
/// so this is a fold the oracle can genuinely check rather than a blind spot.
///
/// The second arm is the load-bearing one. A round-trip alone would pass on a fold
/// that recorded a role as nothing but a name and on a snapshot that read the same
/// — both sides defaulting identically and agreeing about nothing. Altering ONE
/// attribute out of band and requiring the oracle to name that field is what shows
/// the attributes are really being compared.
#[compio::test]
async fn role_attributes_round_trip_and_drift_is_named() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token("schema");
    let role = token("rolattr").to_lowercase();
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
            "name": "role_attributes",
            "owner_app": OWNER,
            "ops": [
                {
                    "op": "createRole",
                    "name": role,
                    "login": true,
                    "createDb": true,
                    "createRole": true,
                    "bypassRls": false
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

        // Non-vacuity: the fold must have recorded the attributes, not just a name.
        let folded = expected
            .roles
            .get(&role)
            .ok_or_else(|| format!("the fold must record the role: {:?}", expected.roles.keys()))?;
        if !(folded.login && folded.create_db && folded.create_role) {
            return Err(format!(
                "the fold must carry the authored attributes, got login={} create_db={} create_role={}",
                folded.login, folded.create_db, folded.create_role
            ));
        }

        let actual = snapshot_schema(&session, &cfg.project_schema)
            .await
            .map_err(|error| format!("snapshot: {error}"))?;
        let drift = diff_snapshots(&expected, &actual);
        assert!(
            drift.is_clean(),
            "an attributed role must round-trip: missing={:?} altered={:?}",
            drift.missing_objects,
            drift.altered_objects
        );

        // The arm that shows the attributes are compared at all. One attribute,
        // changed out of band; the oracle must name that field and only it.
        session
            .batch(&format!("ALTER ROLE {} NOCREATEDB", quote_ident(&role)))
            .await
            .map_err(|error| format!("alter the role out of band: {error}"))?;
        let after = snapshot_schema(&session, &cfg.project_schema)
            .await
            .map_err(|error| format!("snapshot after the alter: {error}"))?;
        let drifted = diff_snapshots(&expected, &after);
        assert!(
            drifted.missing_objects.is_empty(),
            "the role still exists, so nothing may be reported missing: {:?}",
            drifted.missing_objects
        );
        let fields: Vec<&str> = drifted
            .altered_objects
            .iter()
            .filter(|entry| entry.object == format!("role {role}"))
            .map(|entry| entry.field.as_str())
            .collect();
        assert_eq!(
            fields,
            vec!["create_db"],
            "the oracle must name the one attribute that moved: {:?}",
            drifted.altered_objects
        );
        Ok(())
    }
    .await;

    let _ = session
        .batch(&format!("DROP ROLE IF EXISTS {}", quote_ident(&role)))
        .await;
    session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE; DROP SCHEMA IF EXISTS {} CASCADE",
            quote_ident(&cfg.project_schema),
            quote_ident(&cfg.pg.meta_schema)
        ))
        .await
        .expect("drop the test schemas");
    work.expect("fold an attributed role against live PostgreSQL");
}

/// `dropOwnedBy` against a live server, with a bystander that must survive.
///
/// This is the most destructive verb in the vocabulary — `DROP OWNED BY <role>`
/// removes every object the named role owns, across the whole database. It is
/// rendered and validated offline (`vendor.rs` refuses an empty list and refuses
/// the reserved `PUBLIC`, both fail-closed), and it appears in the envelope,
/// support-matrix and faithfulness tests. None of those APPLY it. Nothing had ever
/// watched it run.
///
/// THE BYSTANDER IS THE ASSERTION. Confirming the owned table disappeared would
/// pass equally on an implementation that dropped everything in the schema, which
/// is the failure this op is shaped to cause. So a second table, identical except
/// for its owner, sits beside it and has to still be there afterwards.
///
/// What this does NOT claim: that the engine confines the blast radius to the
/// project schema. It cannot and does not — `DROP OWNED BY` is role-scoped by
/// PostgreSQL's design, and `docs/security-model.md` delegates that bound to the
/// database itself ("use a dedicated, non-login migrator role with only the
/// project-schema permissions required"). The engine's part is to render the
/// statement faithfully and refuse the two footguns; the operator's part is the
/// migrator role. This pins the engine's half.
#[compio::test]
async fn drop_owned_by_removes_the_role_s_objects_and_spares_everyone_else_s() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token("schema");
    let owner_role = token("owned").to_lowercase();
    let bystander_role = token("bystnd").to_lowercase();
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

        // Two roles, two tables, identical but for their owner.
        let project = quote_ident(&cfg.project_schema);
        session
            .batch(&format!(
                "CREATE ROLE {owner} NOLOGIN; \
                 CREATE ROLE {bystander} NOLOGIN; \
                 CREATE TABLE {project}.owned_rows (id bigint PRIMARY KEY); \
                 CREATE TABLE {project}.bystander_rows (id bigint PRIMARY KEY); \
                 ALTER TABLE {project}.owned_rows OWNER TO {owner}; \
                 ALTER TABLE {project}.bystander_rows OWNER TO {bystander}",
                owner = quote_ident(&owner_role),
                bystander = quote_ident(&bystander_role),
            ))
            .await
            .map_err(|error| format!("seed the two owned tables: {error}"))?;

        let doc = serde_json::json!({
            "ir_version": 1,
            "name": "drop_owned",
            "owner_app": OWNER,
            "ops": [ { "op": "dropOwnedBy", "roles": [owner_role] } ]
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

        let remaining = session
            .query(
                "SELECT c.relname AS name FROM pg_class c
                   JOIN pg_namespace n ON n.oid = c.relnamespace
                  WHERE n.nspname = $1 AND c.relkind = 'r'
                  ORDER BY c.relname",
                &[(&cfg.project_schema).into()],
            )
            .await
            .map_err(|error| format!("list the surviving tables: {error}"))?;
        let names: Vec<String> = remaining
            .iter()
            .map(|row| row.try_get::<_, String>("name").expect("decode relname"))
            .collect();

        assert_eq!(
            names,
            vec!["bystander_rows".to_string()],
            "the owned table must be gone and the bystander's must remain"
        );
        Ok(())
    }
    .await;

    let _ = session
        .batch(&format!(
            "DROP OWNED BY {}, {} CASCADE",
            quote_ident(&owner_role),
            quote_ident(&bystander_role)
        ))
        .await;
    let _ = session
        .batch(&format!(
            "DROP ROLE IF EXISTS {}; DROP ROLE IF EXISTS {}",
            quote_ident(&owner_role),
            quote_ident(&bystander_role)
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
    work.expect("drop owned by against live PostgreSQL");
}

/// `dropRole` against a live server: the ordinary case, the refusal, and the
/// `ifExists` no-op.
///
/// Like `dropOwnedBy`, this op existed only in the offline envelope,
/// support-matrix and faithfulness tests. Nothing applied it.
///
/// The refusal arm is the one worth having. PostgreSQL will not drop a role that
/// still owns objects, and F470 already established that a raw catalog error
/// reaching an operator is a filed defect when the message says nothing useful
/// (`duplicate key value violates unique constraint "pg_type_typname_nsp_index"`).
/// So this measures WHAT the operator is told, not merely that the drop failed:
/// the message has to name the role, or an on-call engineer has a failing deploy
/// and no subject.
///
/// It also asserts the role and its table BOTH survive the refusal. A drop that
/// failed partway - role gone, objects orphaned to a missing owner - would be far
/// worse than the refusal, and asserting only the error would not notice.
///
/// The `ifExists` arm is the liveness half: re-running a settled migration must
/// not fail because the role it dropped is already gone.
#[compio::test]
async fn drop_role_succeeds_refuses_while_owning_and_no_ops_under_if_exists() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token("schema");
    let plain_role = token("plain").to_lowercase();
    let owning_role = token("owning").to_lowercase();
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

        let project = quote_ident(&cfg.project_schema);
        session
            .batch(&format!(
                "CREATE ROLE {plain} NOLOGIN; \
                 CREATE ROLE {owning} NOLOGIN; \
                 CREATE TABLE {project}.owning_rows (id bigint PRIMARY KEY); \
                 ALTER TABLE {project}.owning_rows OWNER TO {owning}",
                plain = quote_ident(&plain_role),
                owning = quote_ident(&owning_role),
            ))
            .await
            .map_err(|error| format!("seed the roles: {error}"))?;

        let apply_ops = |case: &str, ops: serde_json::Value| {
            let case = case.to_string();
            let policy = policy.clone();
            let cfg = cfg.clone();
            let backend = &backend;
            async move {
                let doc = serde_json::json!({
                    "ir_version": 1,
                    "name": case,
                    "owner_app": OWNER,
                    "ops": ops
                })
                .to_string();
                let author =
                    IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, &policy);
                let guard_cfg = GuardConfig::from_policy(policy.clone(), SqlDialect::Postgres);
                let base = fold_ops(&[], SqlDialect::Postgres, &cfg.project_schema, &policy)
                    .map_err(|error| format!("fold base: {error}"))?;
                let live = LiveSchema::from_catalog_snapshot(base, OWNER);
                let artifact = author
                    .load_and_lower_guarded(&doc, OWNER, &BTreeMap::new(), &live, &guard_cfg)
                    .map_err(|error| format!("lower: {error}"))?;
                MigrationEngine::new()
                    .apply_plan(
                        &artifact.plan.steps,
                        Approval::Approved,
                        backend,
                        &cfg,
                        OWNER,
                        LockMode::Acquire,
                    )
                    .await
                    .map_err(|error| format!("{error}"))
            }
        };

        let role_exists = |name: String| {
            let session = &session;
            async move {
                let row = session
                    .query_one(
                        "SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1) AS present",
                        &[name.as_str().into()],
                    )
                    .await
                    .expect("probe pg_roles");
                row.try_get::<_, bool>("present").expect("decode present")
            }
        };

        // 1. The ordinary case: a role owning nothing drops.
        apply_ops(
            "drop_plain",
            serde_json::json!([{ "op": "dropRole", "name": plain_role }]),
        )
        .await
        .map_err(|error| format!("dropping an unowned role must succeed: {error}"))?;
        if role_exists(plain_role.clone()).await {
            return Err("the unowned role must be gone".to_string());
        }

        // 2. The refusal: PostgreSQL will not drop a role that still owns objects.
        //    What matters is that the operator is told WHICH role.
        let refusal = apply_ops(
            "drop_owning",
            serde_json::json!([{ "op": "dropRole", "name": owning_role }]),
        )
        .await
        .expect_err("dropping a role that owns objects must fail");
        if !refusal.contains(&owning_role) {
            return Err(format!(
                "the failure must name the role an operator has to act on, got: {refusal}"
            ));
        }
        // Nothing half-done: the role and the table it owns both survive.
        if !role_exists(owning_role.clone()).await {
            return Err("the refused drop must leave the role in place".to_string());
        }
        let table_there: bool = session
            .query_one(
                "SELECT EXISTS (
                     SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                      WHERE n.nspname = $1 AND c.relname = 'owning_rows'
                 ) AS present",
                &[(&cfg.project_schema).into()],
            )
            .await
            .map_err(|error| format!("probe the owned table: {error}"))?
            .try_get::<_, bool>("present")
            .map_err(|error| format!("decode present: {error}"))?;
        if !table_there {
            return Err("the refused drop must leave the owned table in place".to_string());
        }

        // 3. Liveness: a settled migration re-supplied must not fail because the
        //    role it dropped is already gone.
        apply_ops(
            "drop_absent",
            serde_json::json!([{ "op": "dropRole", "name": plain_role, "ifExists": true }]),
        )
        .await
        .map_err(|error| format!("ifExists on an absent role must be a no-op: {error}"))?;
        Ok(())
    }
    .await;

    let _ = session
        .batch(&format!(
            "DROP OWNED BY {} CASCADE",
            quote_ident(&owning_role)
        ))
        .await;
    let _ = session
        .batch(&format!(
            "DROP ROLE IF EXISTS {}; DROP ROLE IF EXISTS {}",
            quote_ident(&plain_role),
            quote_ident(&owning_role)
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
    work.expect("drop role against live PostgreSQL");
}
