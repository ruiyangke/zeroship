//! `safety.require_rls` reaches the DECLARATIVE deploy path, not just the IR one.
//!
//! The obligation's final-state check lives in `check_ir_data_security_policy`, and
//! that function has one non-test caller: `IrAuthor::lower_guarded_with_op_spans`.
//! `plan_declarative` never builds a `MigrationIr` - it diffs two snapshots into
//! `Vec<Migration>` (SQL text) and lints that text - so a table created through the
//! declarative path used to reach live Postgres with row level security off, however
//! the obligation was scoped.
//!
//! The declarative model carries no RLS intent: `SchemaSnapshot` records RLS only as
//! `RoleSnapshot.bypass_rls`, nothing per table, so no diff of it can ever author the
//! `ALTER TABLE ... ENABLE ROW LEVEL SECURITY` that would discharge the obligation. A
//! covered CREATE is therefore refused rather than planned.
//!
//! The four arms below share one live database, one descriptor set and one project
//! schema. What changes between them is the charter's obligation and whether the diff
//! creates anything:
//!
//! 1. obligation over the schema the diff creates into -> REFUSED, naming the table;
//! 2. CONTROL, one variable different - the same obligation scoped to another schema
//!    -> plans;
//! 3. the obligating charter over an alter-only and then a no-op diff -> both plan,
//!    because the refusal is about a CREATE, not about the charter's presence;
//! 4. a charter that never mentions `require_rls` -> unaffected.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`; skips cleanly when unset.

mod support;

use std::collections::HashMap;

use support::PgDevSession;

use zero_migrate::render::declarative::DeclarativeError;
use zero_migrate::{
    desired_snapshot, effective_policy_from_charter_toml, snapshot_schema, Approval,
    CollectionDescriptor, DeclarativeAuthor, EffectivePolicy, ExecutorConfig, FieldDescriptor,
    GuardConfig, MigrationEngine, PostgresBackend, SqlDialect,
};

/// The grants every arm needs to plan and apply into its own per-run schema, with
/// `require` supplying whatever obligation the arm is measuring.
fn charter(schema: &str, require: &str) -> EffectivePolicy {
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
key = "schema.rename"
value = true
scope = {{ include = [{schema:?}] }}

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
{require}"#
    );
    effective_policy_from_charter_toml(&toml).expect("the arm's charter composes")
}

/// The charter with no `safety.require_rls` rule at all - the shape every other
/// live-PG declarative test runs under.
fn charter_without_obligation(schema: &str) -> EffectivePolicy {
    charter(schema, "")
}

/// The charter obligating RLS over `obligated_schema`, which an arm points either at
/// its own project schema or at a schema the diff never touches.
fn charter_obligating(schema: &str, obligated_schema: &str) -> EffectivePolicy {
    charter(
        schema,
        &format!(
            r#"
[[require]]
key = "safety.require_rls"
value = true
scope = {{ include = [{obligated_schema:?}] }}
"#
        ),
    )
}

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}_{n}")
}

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(
        format!("prj_{tok}"),
        format!("proj_{tok}"),
        charter_without_obligation(&format!("proj_{tok}")),
    );
    c.pg.meta_schema = format!("meta_{tok}");
    c
}

async fn ensure_project_schema(session: &PgDevSession, cfg: &ExecutorConfig) {
    use zero_migrate::driver::SqlSession;
    session
        .batch(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            cfg.project_schema
        ))
        .await
        .expect("create project schema");
}

async fn drop_schemas(session: &PgDevSession, cfg: &ExecutorConfig) {
    use zero_migrate::driver::SqlSession;
    let _ = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

fn guard_cfg(policy: &EffectivePolicy) -> GuardConfig {
    GuardConfig::from_policy(policy.clone(), SqlDialect::Postgres)
}

fn author_for(cfg: &ExecutorConfig) -> DeclarativeAuthor {
    DeclarativeAuthor::new(cfg.project_schema.clone(), "app_test")
}

/// A one-field collection descriptor.
fn descriptor(name: &str, field: &str, ty: &str, required: bool) -> CollectionDescriptor {
    CollectionDescriptor {
        name: name.into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: field.into(),
            ty: ty.into(),
            required,
            ..Default::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    }
}

/// A two-field descriptor, for the arm that needs an ADD COLUMN against a live table.
fn descriptor_two_fields(name: &str) -> CollectionDescriptor {
    CollectionDescriptor {
        name: name.into(),
        owner_app: "app_test".into(),
        fields: vec![
            FieldDescriptor {
                name: "title".into(),
                ty: "string".into(),
                required: true,
                ..Default::default()
            },
            FieldDescriptor {
                name: "subtitle".into(),
                ty: "string".into(),
                required: false,
                ..Default::default()
            },
        ],
        indexes: vec![],
        runtime_options: Default::default(),
    }
}

// -- arm 1: the obligation covers the schema the diff creates into ---------------

/// A declarative deploy that CREATEs `widgets` under a charter obligating
/// `require_rls` over that very schema must be REFUSED, and the refusal must name
/// the table so an operator knows which one to move.
#[compio::test]
async fn require_rls_over_the_created_schema_refuses_the_declarative_create() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let policy = charter_obligating(&cfg.project_schema, &cfg.project_schema);
    let engine = MigrationEngine::new();
    let author = author_for(&cfg);

    let desc = descriptor("widgets", "title", "string", true);
    let desired = desired_snapshot(&cfg.project_schema, std::slice::from_ref(&desc), &policy)
        .expect("desired_snapshot");
    let live_empty = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (empty)");
    assert!(
        !live_empty.tables.contains_key("widgets"),
        "the arm measures a CREATE, so the table must be absent from live"
    );

    let refusal = engine
        .plan_declarative(
            &desired,
            &live_empty,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&policy),
            &policy,
        )
        .err();

    drop_schemas(&session, &cfg).await;

    let err = refusal.expect(
        "a declarative CREATE inside a require_rls obligation must be refused: the \
         declarative diff carries no RLS transition that could discharge it",
    );
    match &err {
        DeclarativeError::RequireRlsUnsatisfiable { schema, tables } => {
            assert_eq!(schema, &cfg.project_schema, "the schema the create targets");
            assert_eq!(
                tables,
                &vec!["widgets".to_string()],
                "the refusal must name the offending table"
            );
        }
        other => panic!("expected RequireRlsUnsatisfiable, got {other:?}"),
    }
    let msg = err.to_string();
    println!("operator sees: {msg}");
    for expected in [
        "safety.require_rls",
        "widgets",
        "IR migration path",
        "narrow the obligation's scope",
    ] {
        assert!(
            msg.contains(expected),
            "the refusal must tell the operator about {expected:?}, got: {msg}"
        );
    }
}

// -- the obligation resolves at each table, not at the diff ----------------------

/// Two tables created into the same schema by one diff, with the obligation scoped
/// to only one of them: the refusal names that one and only that one. This is what
/// "resolve at the concrete table" buys over "refuse whenever the charter obligates
/// anything".
#[compio::test]
async fn require_rls_names_only_the_covered_table_of_a_multi_table_create() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let policy = charter(
        &cfg.project_schema,
        &format!(
            r#"
[[require]]
key = "safety.require_rls"
value = true
scope = {{ include = [{:?}] }}
"#,
            format!("{}.widgets", cfg.project_schema)
        ),
    );
    let engine = MigrationEngine::new();
    let author = author_for(&cfg);

    let descs = vec![
        descriptor("widgets", "title", "string", true),
        descriptor("gadgets", "title", "string", true),
    ];
    let desired = desired_snapshot(&cfg.project_schema, &descs, &policy).expect("desired_snapshot");
    let live_empty = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (empty)");

    let refusal = engine
        .plan_declarative(
            &desired,
            &live_empty,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&policy),
            &policy,
        )
        .err();

    drop_schemas(&session, &cfg).await;

    let err = refusal.expect("the covered table's create must be refused");
    match err {
        DeclarativeError::RequireRlsUnsatisfiable { tables, .. } => assert_eq!(
            tables,
            vec!["widgets".to_string()],
            "only the covered table belongs in the refusal; gadgets is outside the scope"
        ),
        other => panic!("expected RequireRlsUnsatisfiable, got {other:?}"),
    }
}

// -- arm 2: CONTROL, the same obligation scoped to a different schema ------------

/// One variable different from arm 1: the obligation names a schema this diff never
/// touches. The identical CREATE must still plan.
#[compio::test]
async fn require_rls_over_another_schema_still_plans_the_create() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let elsewhere = format!("other_{tok}");
    let policy = charter_obligating(&cfg.project_schema, &elsewhere);
    let engine = MigrationEngine::new();
    let author = author_for(&cfg);

    let desc = descriptor("widgets", "title", "string", true);
    let desired = desired_snapshot(&cfg.project_schema, std::slice::from_ref(&desc), &policy)
        .expect("desired_snapshot");
    let live_empty = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (empty)");

    let planned = engine.plan_declarative(
        &desired,
        &live_empty,
        &HashMap::new(),
        &author,
        &[],
        &guard_cfg(&policy),
        &policy,
    );

    drop_schemas(&session, &cfg).await;

    let plan = planned.expect(
        "an obligation scoped to a schema this diff never touches must not refuse the diff",
    );
    assert!(
        !plan.plain.items.is_empty(),
        "the uncovered CREATE must still be planned"
    );
}

// -- arm 3: the obligating charter over an alter-only and a no-op diff -----------

/// The refusal is about a CREATE, not about the charter's presence: with `widgets`
/// already live, the same obligating charter must plan an ADD COLUMN, and then plan
/// the no-op re-deploy that follows it.
#[compio::test]
async fn require_rls_admits_an_alter_only_and_a_no_op_diff() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let engine = MigrationEngine::new();
    let author = author_for(&cfg);

    // Deploy v1 under a charter with no obligation, so the table is live before the
    // obligation exists - the real order of events when an operator adds one later.
    let unobligated = charter_without_obligation(&cfg.project_schema);
    let v1 = descriptor("widgets", "title", "string", true);
    let desired_v1 = desired_snapshot(&cfg.project_schema, std::slice::from_ref(&v1), &unobligated)
        .expect("desired_snapshot v1");
    let live_empty = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (empty)");
    let plan_v1 = engine
        .plan_declarative(
            &desired_v1,
            &live_empty,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&unobligated),
            &unobligated,
        )
        .expect("plan_declarative v1");
    let backend = PostgresBackend::new_generic(&session);
    engine
        .apply_declarative(
            &plan_v1,
            &unobligated,
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
        )
        .await
        .expect("apply_declarative v1");

    // Now the obligation exists, and v2 only ADDs a column to the live table.
    let policy = charter_obligating(&cfg.project_schema, &cfg.project_schema);
    let v2 = descriptor_two_fields("widgets");
    let desired_v2 = desired_snapshot(&cfg.project_schema, std::slice::from_ref(&v2), &policy)
        .expect("desired_snapshot v2");
    let live_v1 = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (v1)");
    let mut ownership: HashMap<String, String> = HashMap::new();
    ownership.insert("widgets".to_string(), "app_test".to_string());

    let alter_only = engine.plan_declarative(
        &desired_v2,
        &live_v1,
        &ownership,
        &author,
        &[],
        &guard_cfg(&policy),
        &policy,
    );

    // And the no-op: desired v1 against live v1 creates nothing at all.
    let desired_v1_under_policy =
        desired_snapshot(&cfg.project_schema, std::slice::from_ref(&v1), &policy)
            .expect("desired_snapshot v1 under the obligating charter");
    let no_op = engine.plan_declarative(
        &desired_v1_under_policy,
        &live_v1,
        &ownership,
        &author,
        &[],
        &guard_cfg(&policy),
        &policy,
    );

    drop_schemas(&session, &cfg).await;

    let alter_only = alter_only
        .expect("an alter-only diff creates no table, so the obligation cannot refuse it");
    assert!(
        !alter_only.plain.items.is_empty(),
        "the ADD COLUMN must still be planned"
    );
    let no_op = no_op.expect("a no-op diff creates no table, so the obligation cannot refuse it");
    assert!(
        no_op.plain.items.is_empty(),
        "the re-deploy of the live shape is a no-op"
    );
}

// -- arm 4: a charter that never mentions require_rls ----------------------------

/// The charter every other declarative test runs under keeps planning its CREATE.
#[compio::test]
async fn a_charter_without_require_rls_plans_the_create() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let policy = charter_without_obligation(&cfg.project_schema);
    let engine = MigrationEngine::new();
    let author = author_for(&cfg);

    let desc = descriptor("widgets", "title", "string", true);
    let desired = desired_snapshot(&cfg.project_schema, std::slice::from_ref(&desc), &policy)
        .expect("desired_snapshot");
    let live_empty = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (empty)");

    let planned = engine.plan_declarative(
        &desired,
        &live_empty,
        &HashMap::new(),
        &author,
        &[],
        &guard_cfg(&policy),
        &policy,
    );

    drop_schemas(&session, &cfg).await;

    let plan = planned.expect("a charter with no require_rls rule refuses nothing");
    assert!(
        !plan.plain.items.is_empty(),
        "the CREATE must be planned when no obligation exists"
    );
}
