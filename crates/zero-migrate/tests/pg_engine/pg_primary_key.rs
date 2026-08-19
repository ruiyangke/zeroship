//! Live PostgreSQL coverage for explicit primary-key add/replace/drop.
//!
//! Gated behind `ZERO_MIGRATE_TEST_PG_URL`; DB-free runs skip cleanly. These
//! tests drive the public `PostgresBackend<PgDevSession>` seam, not a native
//! client-only implementation.

use crate::support;

use std::sync::atomic::{AtomicU64, Ordering};

use crate::support::PgDevSession;
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::AlterPrimaryKeyAction;
use zero_migrate::{
    AlterPrimaryKeyStep, ApplyError, Approval, ApprovalScope, Checksum, ChecksumInput,
    ExecutorConfig, Migration, MigrationBackend, MigrationFlags, MigrationId, PostgresBackend,
};

fn token() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    format!("{}_{}_{}", std::process::id(), nanos, sequence)
}

fn cfg_for(token: &str) -> ExecutorConfig {
    let mut cfg = ExecutorConfig::new(
        format!("pk_project_{token}"),
        format!("pk_app_{token}"),
        support::no_inject(&format!("pk_app_{token}")),
    );
    cfg.pg.meta_schema = format!("pk_meta_{token}");
    cfg
}

// Hands back the guard rather than dropping the schemas itself, so a panic in the
// caller still removes them. The explicit `cleanup` below stays as the happy path.
async fn setup<'a>(session: &'a PgDevSession, cfg: &ExecutorConfig) -> support::SchemaGuard<'a> {
    let guard = support::SchemaGuard::arm(
        session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!("CREATE SCHEMA \"{}\"", cfg.project_schema))
        .await
        .expect("create primary-key test schema");
    PostgresBackend::new_generic(session)
        .ensure_journal(cfg)
        .await
        .expect("create primary-key test journal");
    guard
}

async fn cleanup(session: &PgDevSession, cfg: &ExecutorConfig) {
    let _ = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; \
             DROP SCHEMA IF EXISTS \"{}\" CASCADE",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

fn step(table: &str, action: AlterPrimaryKeyAction) -> AlterPrimaryKeyStep {
    let destructive = !matches!(action, AlterPrimaryKeyAction::Add { .. });
    let flags = MigrationFlags {
        destructive,
        requires_approval: destructive,
        ..MigrationFlags::default()
    };
    let up = format!("-- structured PostgreSQL primary-key operation: {action:?}");
    let checksum = Checksum::of(&ChecksumInput {
        up: &up,
        down: None,
        flags: &flags,
        owner_app: "app_test",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    AlterPrimaryKeyStep {
        migration: Migration {
            version: MigrationId::generate(),
            name: format!("alter primary key {table}"),
            checksum,
            up,
            down: None,
            flags,
            owner_app: "app_test".into(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: None,
            effect: None,
        },
        schema: String::new(), // filled from ExecutorConfig by `run`
        table: table.into(),
        action,
    }
}

async fn run(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    mut step: AlterPrimaryKeyStep,
) -> Result<bool, ApplyError> {
    step.schema.clone_from(&cfg.project_schema);
    let backend = PostgresBackend::new_generic(session);
    backend.acquire_project_lock(cfg).await?;
    let result = backend
        .alter_primary_key(
            cfg,
            &step,
            Approval::Approved,
            &ApprovalScope::All,
            "pg-primary-key-test",
        )
        .await;
    backend.release_project_lock(cfg).await?;
    result
}

async fn primary_key_columns(
    session: &PgDevSession,
    schema: &str,
    table: &str,
) -> Option<Vec<String>> {
    let rows = session
        .query(
            "SELECT COALESCE(array_agg(att.attname::text ORDER BY key.ordinality),
                             ARRAY[]::text[]) AS columns
             FROM pg_catalog.pg_constraint con
             CROSS JOIN LATERAL unnest(con.conkey)
               WITH ORDINALITY AS key(attnum, ordinality)
             JOIN pg_catalog.pg_attribute att
               ON att.attrelid = con.conrelid AND att.attnum = key.attnum
             JOIN pg_catalog.pg_class tbl ON tbl.oid = con.conrelid
             JOIN pg_catalog.pg_namespace ns ON ns.oid = tbl.relnamespace
             WHERE ns.nspname = $1 AND tbl.relname = $2 AND con.contype = 'p'
             GROUP BY con.oid",
            &[schema.into(), table.into()],
        )
        .await
        .expect("read primary-key columns");
    rows.first()
        .map(|row| row.try_get("columns").expect("decode primary-key columns"))
}

async fn is_identity(session: &PgDevSession, schema: &str, table: &str, column: &str) -> bool {
    session
        .query_one(
            "SELECT (att.attidentity <> '') AS is_identity
             FROM pg_catalog.pg_attribute att
             JOIN pg_catalog.pg_class tbl ON tbl.oid = att.attrelid
             JOIN pg_catalog.pg_namespace ns ON ns.oid = tbl.relnamespace
             WHERE ns.nspname = $1 AND tbl.relname = $2 AND att.attname = $3",
            &[schema.into(), table.into(), column.into()],
        )
        .await
        .expect("read identity facet")
        .try_get("is_identity")
        .expect("decode identity facet")
}

#[compio::test]
async fn add_installs_an_exact_candidate_on_a_table_without_a_primary_key() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    let _schema_guard = setup(&session, &cfg).await;
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".items (
               tenant_id bigint NOT NULL,
               item_id bigint NOT NULL
             );
             CREATE UNIQUE INDEX items_candidate
               ON \"{}\".items (tenant_id, item_id)",
            cfg.project_schema, cfg.project_schema
        ))
        .await
        .expect("create no-primary-key fixture");

    let ran = run(
        &session,
        &cfg,
        step(
            "items",
            AlterPrimaryKeyAction::Add {
                columns: vec!["tenant_id".into(), "item_id".into()],
            },
        ),
    )
    .await
    .expect("add primary key");
    assert!(ran);
    assert_eq!(
        primary_key_columns(&session, &cfg.project_schema, "items").await,
        Some(vec!["tenant_id".into(), "item_id".into()])
    );
    cleanup(&session, &cfg).await;
}

#[compio::test]
async fn replace_accepts_only_exact_order_and_supports_single_composite_round_trip() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    let _schema_guard = setup(&session, &cfg).await;
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".items (
               id bigint NOT NULL PRIMARY KEY,
               tenant_id bigint NOT NULL,
               item_id bigint NOT NULL
             );
             CREATE UNIQUE INDEX items_id_candidate ON \"{}\".items (id);
             CREATE UNIQUE INDEX items_composite_candidate
               ON \"{}\".items (tenant_id, item_id)",
            cfg.project_schema, cfg.project_schema, cfg.project_schema
        ))
        .await
        .expect("create replacement fixture");

    let mismatch = run(
        &session,
        &cfg,
        step(
            "items",
            AlterPrimaryKeyAction::Replace {
                expected_columns: vec!["tenant_id".into(), "id".into()],
                columns: vec!["tenant_id".into(), "item_id".into()],
                drop_identity_from: None,
            },
        ),
    )
    .await
    .expect_err("wrong expectedColumns must be refused");
    assert!(mismatch.to_string().contains("order is significant"));
    assert_eq!(
        primary_key_columns(&session, &cfg.project_schema, "items").await,
        Some(vec!["id".into()])
    );

    run(
        &session,
        &cfg,
        step(
            "items",
            AlterPrimaryKeyAction::Replace {
                expected_columns: vec!["id".into()],
                columns: vec!["tenant_id".into(), "item_id".into()],
                drop_identity_from: None,
            },
        ),
    )
    .await
    .expect("single to composite replacement");
    assert_eq!(
        primary_key_columns(&session, &cfg.project_schema, "items").await,
        Some(vec!["tenant_id".into(), "item_id".into()])
    );

    let reversed = run(
        &session,
        &cfg,
        step(
            "items",
            AlterPrimaryKeyAction::Replace {
                expected_columns: vec!["item_id".into(), "tenant_id".into()],
                columns: vec!["id".into()],
                drop_identity_from: None,
            },
        ),
    )
    .await
    .expect_err("reversed composite expectedColumns must be refused");
    assert!(reversed.to_string().contains("order is significant"));

    run(
        &session,
        &cfg,
        step(
            "items",
            AlterPrimaryKeyAction::Replace {
                expected_columns: vec!["tenant_id".into(), "item_id".into()],
                columns: vec!["id".into()],
                drop_identity_from: None,
            },
        ),
    )
    .await
    .expect("composite to single replacement");
    assert_eq!(
        primary_key_columns(&session, &cfg.project_schema, "items").await,
        Some(vec!["id".into()])
    );
    cleanup(&session, &cfg).await;
}

#[compio::test]
async fn drop_and_replace_require_declared_identity_removal_and_drop_identity_transactionally() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    let _schema_guard = setup(&session, &cfg).await;
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".replace_identity (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               external_id bigint NOT NULL UNIQUE
             );
             CREATE TABLE \"{}\".drop_plain (
               id bigint NOT NULL PRIMARY KEY
             );
             CREATE TABLE \"{}\".drop_identity (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY
             );
             CREATE TABLE \"{}\".composite_identity (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               tenant_id bigint NOT NULL,
               UNIQUE (tenant_id, id)
             )",
            cfg.project_schema, cfg.project_schema, cfg.project_schema, cfg.project_schema
        ))
        .await
        .expect("create identity transition fixtures");

    let replace_refusal = run(
        &session,
        &cfg,
        step(
            "replace_identity",
            AlterPrimaryKeyAction::Replace {
                expected_columns: vec!["id".into()],
                columns: vec!["external_id".into()],
                drop_identity_from: None,
            },
        ),
    )
    .await
    .expect_err("identity replacement without declaration must fail");
    assert!(replace_refusal.to_string().contains("dropIdentityFrom"));
    assert!(is_identity(&session, &cfg.project_schema, "replace_identity", "id").await);

    run(
        &session,
        &cfg,
        step(
            "replace_identity",
            AlterPrimaryKeyAction::Replace {
                expected_columns: vec!["id".into()],
                columns: vec!["external_id".into()],
                drop_identity_from: Some(vec!["id".into()]),
            },
        ),
    )
    .await
    .expect("declared DROP IDENTITY replacement");
    assert!(!is_identity(&session, &cfg.project_schema, "replace_identity", "id").await);

    run(
        &session,
        &cfg,
        step(
            "drop_plain",
            AlterPrimaryKeyAction::Drop {
                expected_columns: vec!["id".into()],
                drop_identity_from: None,
            },
        ),
    )
    .await
    .expect("drop non-generated primary key without identity transition");
    assert_eq!(
        primary_key_columns(&session, &cfg.project_schema, "drop_plain").await,
        None
    );

    let drop_refusal = run(
        &session,
        &cfg,
        step(
            "drop_identity",
            AlterPrimaryKeyAction::Drop {
                expected_columns: vec!["id".into()],
                drop_identity_from: None,
            },
        ),
    )
    .await
    .expect_err("identity drop without declaration must fail");
    assert!(drop_refusal.to_string().contains("dropIdentityFrom"));

    run(
        &session,
        &cfg,
        step(
            "drop_identity",
            AlterPrimaryKeyAction::Drop {
                expected_columns: vec!["id".into()],
                drop_identity_from: Some(vec!["id".into()]),
            },
        ),
    )
    .await
    .expect("drop primary key and identity together");
    assert_eq!(
        primary_key_columns(&session, &cfg.project_schema, "drop_identity").await,
        None
    );
    assert!(!is_identity(&session, &cfg.project_schema, "drop_identity", "id").await);

    run(
        &session,
        &cfg,
        step(
            "composite_identity",
            AlterPrimaryKeyAction::Replace {
                expected_columns: vec!["id".into()],
                columns: vec!["tenant_id".into(), "id".into()],
                drop_identity_from: None,
            },
        ),
    )
    .await
    .expect("PostgreSQL identity may remain in a composite primary key");
    assert_eq!(
        primary_key_columns(&session, &cfg.project_schema, "composite_identity").await,
        Some(vec!["tenant_id".into(), "id".into()])
    );
    assert!(is_identity(&session, &cfg.project_schema, "composite_identity", "id").await);
    cleanup(&session, &cfg).await;
}

#[compio::test]
async fn inbound_fk_refuses_missing_or_stale_alternate_and_accepts_prebound_alternate() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    let _schema_guard = setup(&session, &cfg).await;
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".unsafe_parent (
               id bigint NOT NULL PRIMARY KEY
             );
             CREATE TABLE \"{}\".unsafe_child (
               parent_id bigint REFERENCES \"{}\".unsafe_parent(id)
             )",
            cfg.project_schema, cfg.project_schema, cfg.project_schema
        ))
        .await
        .expect("create unsafe inbound-FK fixture");

    let no_alternate = run(
        &session,
        &cfg,
        step(
            "unsafe_parent",
            AlterPrimaryKeyAction::Drop {
                expected_columns: vec!["id".into()],
                drop_identity_from: None,
            },
        ),
    )
    .await
    .expect_err("drop must refuse an inbound FK with no alternate");
    assert!(no_alternate
        .to_string()
        .contains("no exact alternate unique key"));

    session
        .batch(&format!(
            "CREATE UNIQUE INDEX unsafe_parent_id_alternate
               ON \"{}\".unsafe_parent(id)",
            cfg.project_schema
        ))
        .await
        .expect("add late alternate");
    let stale_binding = run(
        &session,
        &cfg,
        step(
            "unsafe_parent",
            AlterPrimaryKeyAction::Drop {
                expected_columns: vec!["id".into()],
                drop_identity_from: None,
            },
        ),
    )
    .await
    .expect_err("late alternate does not repoint PostgreSQL conindid");
    assert!(stale_binding.to_string().contains("physically bound"));
    assert_eq!(
        primary_key_columns(&session, &cfg.project_schema, "unsafe_parent").await,
        Some(vec!["id".into()])
    );

    // Bind the FK while the alternate unique index is the only referenced key,
    // then add a PK afterward. Its `conindid` remains the alternate index, so the
    // structured drop is safe and does not migrate/recreate the FK.
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".safe_parent (id bigint NOT NULL);
             CREATE UNIQUE INDEX safe_parent_id_alternate
               ON \"{}\".safe_parent(id);
             CREATE TABLE \"{}\".safe_child (
               parent_id bigint REFERENCES \"{}\".safe_parent(id)
             );
             ALTER TABLE \"{}\".safe_parent ADD PRIMARY KEY (id)",
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema
        ))
        .await
        .expect("create prebound alternate fixture");
    run(
        &session,
        &cfg,
        step(
            "safe_parent",
            AlterPrimaryKeyAction::Drop {
                expected_columns: vec!["id".into()],
                drop_identity_from: None,
            },
        ),
    )
    .await
    .expect("drop with FK prebound to exact alternate");
    assert_eq!(
        primary_key_columns(&session, &cfg.project_schema, "safe_parent").await,
        None
    );
    session
        .batch(&format!(
            "INSERT INTO \"{}\".safe_parent(id) VALUES (7);
             INSERT INTO \"{}\".safe_child(parent_id) VALUES (7)",
            cfg.project_schema, cfg.project_schema
        ))
        .await
        .expect("inbound FK remains valid after primary-key drop");

    session
        .batch(&format!(
            "CREATE TABLE \"{}\".unsafe_replace_parent (
               id bigint NOT NULL PRIMARY KEY,
               next_id bigint NOT NULL UNIQUE
             );
             CREATE TABLE \"{}\".unsafe_replace_child (
               parent_id bigint REFERENCES \"{}\".unsafe_replace_parent(id)
             )",
            cfg.project_schema, cfg.project_schema, cfg.project_schema
        ))
        .await
        .expect("create unsafe inbound-FK replacement fixture");
    let unsafe_replace = run(
        &session,
        &cfg,
        step(
            "unsafe_replace_parent",
            AlterPrimaryKeyAction::Replace {
                expected_columns: vec!["id".into()],
                columns: vec!["next_id".into()],
                drop_identity_from: None,
            },
        ),
    )
    .await
    .expect_err("replace must not remove the inbound FK's only referenced key");
    assert!(unsafe_replace
        .to_string()
        .contains("no exact alternate unique key"));
    assert_eq!(
        primary_key_columns(&session, &cfg.project_schema, "unsafe_replace_parent").await,
        Some(vec!["id".into()])
    );

    // As in the safe drop fixture, bind the FK to the exact alternate before
    // installing the old PK. Replacing that PK must leave the FK untouched and
    // valid while promoting the separately staged target candidate.
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".safe_replace_parent (
               id bigint NOT NULL,
               next_id bigint NOT NULL UNIQUE
             );
             CREATE UNIQUE INDEX safe_replace_id_alternate
               ON \"{}\".safe_replace_parent(id);
             CREATE TABLE \"{}\".safe_replace_child (
               parent_id bigint REFERENCES \"{}\".safe_replace_parent(id)
             );
             ALTER TABLE \"{}\".safe_replace_parent ADD PRIMARY KEY (id)",
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema,
            cfg.project_schema
        ))
        .await
        .expect("create prebound alternate replacement fixture");
    run(
        &session,
        &cfg,
        step(
            "safe_replace_parent",
            AlterPrimaryKeyAction::Replace {
                expected_columns: vec!["id".into()],
                columns: vec!["next_id".into()],
                drop_identity_from: None,
            },
        ),
    )
    .await
    .expect("replace with FK prebound to exact alternate");
    assert_eq!(
        primary_key_columns(&session, &cfg.project_schema, "safe_replace_parent").await,
        Some(vec!["next_id".into()])
    );
    session
        .batch(&format!(
            "INSERT INTO \"{}\".safe_replace_parent(id, next_id) VALUES (8, 80);
             INSERT INTO \"{}\".safe_replace_child(parent_id) VALUES (8)",
            cfg.project_schema, cfg.project_schema
        ))
        .await
        .expect("inbound FK remains valid after primary-key replacement");
    cleanup(&session, &cfg).await;
}
