//! Resurrected live-Postgres DECLARATIVE-deploy scenarios.
//!
//! The desired-vs-live diff → DDL path: given a set of `CollectionDescriptor`s (the
//! `registerModel` shape the SDK emits), `desired_snapshot` compiles the desired schema,
//! `snapshot_schema` introspects the live one, and `MigrationEngine::plan_declarative` +
//! `apply_declarative` deploy the diff — the whole flow driven through the shipped
//! `PostgresBackend<PgDevSession>` over the `driver::SqlSession` seam against real PG.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`; skips cleanly when unset.

mod support;

use std::collections::HashMap;

use support::PgDevSession;

use zero_migrate::{
    desired_snapshot, diff_snapshots, snapshot_schema, Approval, CollectionDescriptor,
    DeclarativeAuthor, EffectivePolicy, ExecutorConfig, FieldDescriptor, GuardConfig,
    MigrationEngine, PostgresBackend, RenameHint, SqlDialect,
};

/// The charter every stage of the deploy runs under, scoped to this run's project
/// schema and carrying no inject rule.
///
/// Both halves matter. Each test isolates itself behind a per-run schema, so the
/// grants have to name that schema or apply refuses every statement as
/// `CrossSchema`. And the charter must leave the schema uninjected: a declarative
/// deploy reaches the database as rendered DDL, which `gate_raw_create` denies
/// outright wherever an inject rule covers the target, because injection cannot
/// rewrite raw text. This is the same policy `guard_cfg` builds, so the guard and
/// the executor now agree on one charter instead of two.
fn effective_policy(cfg: &ExecutorConfig) -> EffectivePolicy {
    support::no_inject(&cfg.project_schema)
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
        support::no_inject(&format!("proj_{tok}")),
    );
    c.pg.meta_schema = format!("meta_{tok}");
    c
}

/// Create the project schema and hand back the guard that removes it, and the meta
/// schema apply creates later, when the test leaves scope. The DROP rides the guard
/// rather than a trailing statement so a failing assertion cannot abandon them.
#[must_use = "the guard drops the schemas when it falls out of scope"]
async fn ensure_project_schema<'a>(
    session: &'a PgDevSession,
    cfg: &ExecutorConfig,
) -> support::SchemaGuard<'a> {
    use zero_migrate::driver::SqlSession;
    let guard = support::SchemaGuard::arm(
        session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            cfg.project_schema
        ))
        .await
        .expect("create project schema");
    guard
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

fn guard_cfg(cfg: &ExecutorConfig) -> GuardConfig {
    GuardConfig::from_policy(
        support::no_inject(&cfg.project_schema),
        SqlDialect::Postgres,
    )
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

async fn table_exists(session: &PgDevSession, schema: &str, table: &str) -> bool {
    use zero_migrate::driver::SqlSession;
    let row = session
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_schema = $1 AND table_name = $2) AS present",
            &[schema.into(), table.into()],
        )
        .await
        .expect("table_exists probe");
    row.try_get::<_, bool>("present").expect("decode present")
}

/// A declarative deploy of a fresh collection creates the table, and a
/// re-introspection of live PG then round-trips to the desired snapshot with ZERO
/// structural drift - the type-fidelity proof, over the shipped seam.
#[compio::test]
async fn declarative_deploy_creates_table_and_round_trips_with_zero_drift() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    let _schemas = ensure_project_schema(&session, &cfg).await;

    let engine = MigrationEngine::new();
    let author = author_for(&cfg);

    // Desired: one collection `widgets` with a required `title` string field.
    let desc = descriptor("widgets", "title", "string", true);
    let desired = desired_snapshot(
        &cfg.project_schema,
        std::slice::from_ref(&desc),
        &effective_policy(&cfg),
    )
    .expect("desired_snapshot");

    // Live: empty (the table does not exist yet).
    let live_empty = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (empty)");
    assert!(
        !table_exists(&session, &cfg.project_schema, "widgets").await,
        "the table does not exist before the declarative deploy"
    );

    // Plan the desired-vs-live diff (no rename hints → additive create), then deploy
    // it through the shipped PostgresBackend over the seam.
    let plan = engine
        .plan_declarative(
            &desired,
            &live_empty,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&cfg),
            &effective_policy(&cfg),
        )
        .expect("plan_declarative");
    assert!(plan.renames.is_empty(), "an additive create has no renames");
    let backend = PostgresBackend::new_generic(&session);
    engine
        .apply_declarative(
            &plan,
            &effective_policy(&cfg),
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
        )
        .await
        .expect("apply_declarative create widgets");

    assert!(
        table_exists(&session, &cfg.project_schema, "widgets").await,
        "the declarative deploy created the table against real PG"
    );

    // Re-introspect live and diff against desired: the freshly-created table must
    // round-trip to the desired snapshot with ZERO structural drift.
    let live_after = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (after)");
    let drift = diff_snapshots(&desired.snapshot, &live_after);
    assert!(
        drift.is_clean(),
        "the created table must round-trip to desired with zero drift: \
         missing={:?} unexpected={:?} altered={:?}",
        drift.missing_objects,
        drift.unexpected_objects,
        drift.altered_objects,
    );

    // Idempotent re-deploy: desired == live now, so the diff is empty (no-op).
    let live_now = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (now)");
    let plan2 = engine
        .plan_declarative(
            &desired,
            &live_now,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&cfg),
            &effective_policy(&cfg),
        )
        .expect("plan_declarative 2");
    assert!(
        plan2.plain.items.is_empty(),
        "a second declarative deploy of the same desired schema is a no-op: {:?}",
        plan2.plain.items.len()
    );

    drop_schemas(&session, &cfg).await;
}

/// A second declarative deploy that ADDS a column to an existing table diffs to an
/// additive `ALTER TABLE … ADD COLUMN` and applies cleanly (desired ⊃ live).
#[compio::test]
async fn declarative_add_column_diff_applies() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    let _schemas = ensure_project_schema(&session, &cfg).await;

    let engine = MigrationEngine::new();
    let author = author_for(&cfg);

    // Deploy v1: widgets(title).
    let v1_desc = descriptor("widgets", "title", "string", true);
    let desired_v1 = desired_snapshot(
        &cfg.project_schema,
        std::slice::from_ref(&v1_desc),
        &effective_policy(&cfg),
    )
    .expect("desired v1");
    let live0 = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("live0");
    let plan1 = engine
        .plan_declarative(
            &desired_v1,
            &live0,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&cfg),
            &effective_policy(&cfg),
        )
        .expect("plan v1");
    let backend = PostgresBackend::new_generic(&session);
    engine
        .apply_declarative(
            &plan1,
            &effective_policy(&cfg),
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
        )
        .await
        .expect("apply v1");

    // Deploy v2: widgets(title, subtitle) — an added nullable column.
    let v2_desc = CollectionDescriptor {
        name: "widgets".into(),
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
    };
    let desired_v2 = desired_snapshot(
        &cfg.project_schema,
        std::slice::from_ref(&v2_desc),
        &effective_policy(&cfg),
    )
    .expect("desired v2");
    let live1 = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("live1");
    let plan2 = engine
        .plan_declarative(
            &desired_v2,
            &live1,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&cfg),
            &effective_policy(&cfg),
        )
        .expect("plan v2");
    assert!(
        !plan2.plain.items.is_empty(),
        "the add-column diff produces a non-empty plain plan"
    );
    let backend2 = PostgresBackend::new_generic(&session);
    engine
        .apply_declarative(
            &plan2,
            &effective_policy(&cfg),
            Approval::Approved,
            &backend2,
            &cfg,
            "app_test",
        )
        .await
        .expect("apply v2 add-column");

    // The new column now exists, and live round-trips to desired_v2 with zero drift.
    let live2 = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("live2");
    let table = live2.tables.get("widgets").expect("widgets table present");
    assert!(
        table.columns.iter().any(|c| c.name == "subtitle"),
        "the added column is live after the additive declarative deploy"
    );
    let drift = diff_snapshots(&desired_v2.snapshot, &live2);
    assert!(
        drift.is_clean(),
        "after the add-column deploy, live round-trips to desired_v2 with zero drift: \
         missing={:?} unexpected={:?} altered={:?}",
        drift.missing_objects,
        drift.unexpected_objects,
        drift.altered_objects,
    );

    drop_schemas(&session, &cfg).await;
}

/// An out-of-band ALTER that changes an attribute WITHOUT changing a name must
/// land in `altered_objects`.
///
/// `StructuralDrift` has three buckets, and the third exists because the first
/// two cannot see this case - the comment on it says so directly: "the
/// missing/unexpected name buckets cannot see these because the name still
/// matches; this bucket is the attribute-aware tamper surface".
///
/// That claim had no positive test. The bucket appeared in this file only inside
/// must-be-clean assertions, which print it when drift is unexpectedly non-empty
/// and therefore pass whether or not it can ever be populated. A bucket that is
/// only ever asserted EMPTY is indistinguishable from one that never fills.
///
/// Both attributes the surrounding comment names are exercised: a NULLABILITY
/// change and a UNIQUENESS change, the latter being the example the comment
/// itself cites.
#[compio::test]
async fn an_out_of_band_alter_lands_in_altered_objects() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    let _schemas = ensure_project_schema(&session, &cfg).await;

    let engine = MigrationEngine::new();
    let author = author_for(&cfg);

    // A collection with a required field AND a declared UNIQUE index, so the
    // uniqueness attribute has something to diverge from.
    let mut desc = descriptor("gadgets", "code", "string", true);
    desc.indexes = vec![zero_migrate::IndexDescriptor {
        name: "gadgets_code_key".into(),
        columns: vec!["code".into()],
        unique: true,
    }];
    let desired = desired_snapshot(
        &cfg.project_schema,
        std::slice::from_ref(&desc),
        &effective_policy(&cfg),
    )
    .expect("desired_snapshot");

    let live_empty = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (empty)");
    let plan = engine
        .plan_declarative(
            &desired,
            &live_empty,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&cfg),
            &effective_policy(&cfg),
        )
        .expect("plan_declarative");
    let backend = PostgresBackend::new_generic(&session);
    engine
        .apply_declarative(
            &plan,
            &effective_policy(&cfg),
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
        )
        .await
        .expect("apply_declarative create gadgets");

    // BASELINE. Without this the test cannot tell "the ALTER was detected" from
    // "this snapshot never matched in the first place".
    let live_after = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (after deploy)");
    let clean = diff_snapshots(&desired.snapshot, &live_after);
    assert!(
        clean.is_clean(),
        "baseline must be clean before tampering: missing={:?} unexpected={:?} altered={:?}",
        clean.missing_objects,
        clean.unexpected_objects,
        clean.altered_objects
    );

    // TAMPER 1: nullability, out of band.
    session
        .exec(
            &format!(
                r#"ALTER TABLE "{}"."gadgets" ALTER COLUMN "code" DROP NOT NULL"#,
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("out-of-band ALTER");

    let tampered = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (tampered)");
    let drift = diff_snapshots(&desired.snapshot, &tampered);
    assert!(
        !drift.altered_objects.is_empty(),
        "a nullability change must surface in altered_objects, not vanish: {drift:?}"
    );
    assert!(
        drift.missing_objects.is_empty() && drift.unexpected_objects.is_empty(),
        "the name still matches, so the NAME buckets must stay empty - that is the \
         whole reason the altered bucket exists: {drift:?}"
    );

    // TAMPER 2: uniqueness, the example the comment itself cites.
    session
        .exec(
            &format!(r#"DROP INDEX "{}"."gadgets_code_key""#, cfg.project_schema),
            &[],
        )
        .await
        .expect("drop the unique index");
    session
        .exec(
            &format!(
                r#"CREATE INDEX "gadgets_code_key" ON "{}"."gadgets" ("code")"#,
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("recreate it NON-unique under the same name");

    let tampered2 = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (tampered 2)");
    let drift2 = diff_snapshots(&desired.snapshot, &tampered2);
    assert!(
        !drift2.altered_objects.is_empty(),
        "a same-name index that lost its uniqueness must surface in \
         altered_objects: {drift2:?}"
    );
}

/// The two NAME buckets must be proven to fill, for the same reason the attribute
/// bucket was: `missing_objects` and `unexpected_objects` appeared across this
/// crate ONLY inside `.is_empty()` assertions. Asserting a collection empty
/// forever cannot distinguish a working detector from one that never reports.
///
/// These are the out-of-band CREATE and DROP halves of tamper detection - a table
/// made by hand outside the journal, and a deployed table removed behind the
/// engine's back.
#[compio::test]
async fn the_name_buckets_fill_on_out_of_band_create_and_drop() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    let _schemas = ensure_project_schema(&session, &cfg).await;

    let engine = MigrationEngine::new();
    let author = author_for(&cfg);
    let desc = descriptor("sprockets", "label", "string", true);
    let desired = desired_snapshot(
        &cfg.project_schema,
        std::slice::from_ref(&desc),
        &effective_policy(&cfg),
    )
    .expect("desired_snapshot");

    let live_empty = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (empty)");
    let plan = engine
        .plan_declarative(
            &desired,
            &live_empty,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&cfg),
            &effective_policy(&cfg),
        )
        .expect("plan_declarative");
    let backend = PostgresBackend::new_generic(&session);
    engine
        .apply_declarative(
            &plan,
            &effective_policy(&cfg),
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
        )
        .await
        .expect("apply_declarative create sprockets");

    // BASELINE, as in the sibling test: prove the snapshot matches before tampering.
    let live_after = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (after deploy)");
    assert!(
        diff_snapshots(&desired.snapshot, &live_after).is_clean(),
        "baseline must be clean before tampering"
    );

    // OUT-OF-BAND CREATE: a table nobody declared.
    session
        .exec(
            &format!(
                r#"CREATE TABLE "{}"."handmade" ("id" integer PRIMARY KEY)"#,
                cfg.project_schema
            ),
            &[],
        )
        .await
        .expect("out-of-band CREATE");

    let after_create = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (after create)");
    let drift = diff_snapshots(&desired.snapshot, &after_create);
    assert!(
        !drift.unexpected_objects.is_empty(),
        "a hand-made table must surface as UNEXPECTED, not vanish: {drift:?}"
    );
    assert!(
        drift.missing_objects.is_empty(),
        "nothing declared has gone missing yet, so that bucket must stay empty: {drift:?}"
    );

    // OUT-OF-BAND DROP: the declared table removed behind the engine's back.
    session
        .exec(
            &format!(r#"DROP TABLE "{}"."sprockets""#, cfg.project_schema),
            &[],
        )
        .await
        .expect("out-of-band DROP");

    let after_drop = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (after drop)");
    let drift2 = diff_snapshots(&desired.snapshot, &after_drop);
    assert!(
        !drift2.missing_objects.is_empty(),
        "a declared table dropped out of band must surface as MISSING: {drift2:?}"
    );
}

/// A rename HINT on PostgreSQL must produce an expand-contract rename, not a
/// drop-and-recreate.
///
/// `plan.renames` is the highest-stakes of the collections this session found
/// asserted only empty. Every assertion on it across the crate was
/// `.is_empty()` - and on SQLite that is CORRECT, because a rename there routes
/// to a rebuild and `run_expand` is unreachable. Those tests say so explicitly.
///
/// The consequence was that nothing anywhere proved the bucket EVER fills. The
/// `expand_contract` module has a dozen unit tests, but they author a plan
/// directly and therefore bypass the diff - the step that has to DECIDE that a
/// rename happened at all.
///
/// That decision is what stands between a rename and data loss: unrecognised, a
/// renamed column is a drop of the old one plus a create of the new, and the
/// rows in it are gone.
#[compio::test]
async fn a_rename_hint_on_postgres_produces_a_rename_not_a_drop_and_recreate() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    let _schemas = ensure_project_schema(&session, &cfg).await;

    let engine = MigrationEngine::new();
    let author = author_for(&cfg);

    // v1: deploy `contacts` with an `email` field.
    let v1 = descriptor("contacts", "email", "string", true);
    let desired1 = desired_snapshot(
        &cfg.project_schema,
        std::slice::from_ref(&v1),
        &effective_policy(&cfg),
    )
    .expect("desired v1");
    let live_empty = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (empty)");
    let plan1 = engine
        .plan_declarative(
            &desired1,
            &live_empty,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&cfg),
            &effective_policy(&cfg),
        )
        .expect("plan v1");
    let backend = PostgresBackend::new_generic(&session);
    engine
        .apply_declarative(
            &plan1,
            &effective_policy(&cfg),
            Approval::Approved,
            &backend,
            &cfg,
            "app_test",
        )
        .await
        .expect("apply v1");

    // v2: the same collection with the field renamed.
    let v2 = descriptor("contacts", "email_address", "string", true);
    let desired2 = desired_snapshot(
        &cfg.project_schema,
        std::slice::from_ref(&v2),
        &effective_policy(&cfg),
    )
    .expect("desired v2");
    let live_v1 = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (v1)");

    // CONTROL FIRST, and it is the whole point: WITHOUT the hint the diff cannot
    // know a rename happened, so it must plan a drop plus a create. If this ever
    // starts producing a rename, the hint is not what drives the decision and
    // the test below proves nothing.
    let unhinted = engine
        .plan_declarative(
            &desired2,
            &live_v1,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&cfg),
            &effective_policy(&cfg),
        )
        .expect("plan v2 without a hint");
    assert!(
        unhinted.renames.is_empty(),
        "without a hint the diff cannot infer a rename: {:?}",
        unhinted.renames
    );

    // WITH the hint: the same desired-vs-live pair must now plan a rename.
    let hints = vec![RenameHint {
        table: "contacts".into(),
        from: "email".into(),
        to: "email_address".into(),
    }];
    let hinted = engine
        .plan_declarative(
            &desired2,
            &live_v1,
            &HashMap::new(),
            &author,
            &hints,
            &guard_cfg(&cfg),
            &effective_policy(&cfg),
        )
        .expect("plan v2 with a hint");

    assert_eq!(
        hinted.renames.len(),
        1,
        "a rename hint must produce exactly one expand-contract rename: {:?}",
        hinted.renames
    );
}
