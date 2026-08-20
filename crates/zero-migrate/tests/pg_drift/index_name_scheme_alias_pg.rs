//! An index the DATA PLANE created no longer churns CREATE/DROP against the same
//! index the declarative author derived a different name for.
//!
//! Two derivations of the same `(table, columns, unique)` triple disagree above 60
//! bytes. The data plane names indexes through `schema::query::index_name`, which
//! swaps the tail of any natural name over 60 bytes for an 8-char base32 hash. The
//! declarative author names them through `plan::author::cap_ident_name`, which keeps
//! a natural name verbatim through 63 bytes and only then applies its own 10-hex
//! tail. So a natural `<table>_<col>_key` of 61 to 63 bytes yields two different
//! live-legal names for one index, and the index diff keys on NAME: the author's
//! spelling is missing from live and the data plane's spelling is unexpected, so a
//! CREATE and a DROP go out for an index that is already exactly right.
//!
//! The arms only mean something together: arm A is the defect, arm B is the
//! population that works today and must keep working, arm C is the author-supplied
//! rename the fix must NOT swallow, and arm D is the ownership check, which decides
//! "structurally changed" with strict table equality and so had to learn the same
//! pairing or it refused a non-owner over an index the differ had already accepted.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`; skips cleanly when unset.

use crate::support;

use std::collections::HashMap;

use crate::support::PgDevSession;

use zero_migrate::{
    desired_snapshot, diff_snapshots, diff_snapshots_with_index_aliases, snapshot_schema,
    AcceptedIndexAlias, Approval, CollectionDescriptor, DeclarativeAuthor, EffectivePolicy,
    ExecutorConfig, FieldDescriptor, GuardConfig, IndexDescriptor, MigrationEngine,
    PostgresBackend, SqlDialect,
};

/// PostgreSQL's NAMEDATALEN-derived identifier bound, in bytes. Both derivations
/// stay under it, so neither name in this test is server-truncated - the divergence
/// under test is the two schemes disagreeing, not PostgreSQL clipping either one.
const MAX: usize = 63;

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
        .expect("clock")
        .as_nanos();
    format!("{pid}_{nanos}_{n}")
}

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(
        format!("prj_{tok}"),
        format!("proj_{tok}"),
        support::no_inject(&format!("proj_{tok}")),
    );
    c.confinement.meta_schema = format!("meta_{tok}");
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
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
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
            cfg.project_schema, cfg.confinement.meta_schema
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

/// The table name every arm uses. 12 bytes, so a 44-byte field name puts the
/// natural `<table>_<field>_key` at exactly 61 bytes - inside the 61..=63 window
/// where the two derivations disagree and neither is server-truncated.
const TABLE: &str = "zz_idx_alias";

/// The 44-byte field whose `unique: true` facet produces the derived index.
fn long_unique_field() -> String {
    let f = format!("f{}", "a".repeat(43));
    assert_eq!(f.len(), 44, "the field name must be 44 bytes");
    f
}

/// A collection with one `unique: true` field, which the declarative author models
/// as a unique index named through `cap_ident_name`.
fn unique_descriptor(table: &str, field: &str) -> CollectionDescriptor {
    CollectionDescriptor {
        name: table.into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: field.into(),
            ty: "string".into(),
            required: true,
            unique: true,
            ..Default::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    }
}

/// A collection with one plain field and ONE author-supplied index name.
fn named_index_descriptor(table: &str, field: &str, index: &str) -> CollectionDescriptor {
    CollectionDescriptor {
        name: table.into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: field.into(),
            ty: "string".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![IndexDescriptor {
            name: index.into(),
            columns: vec![field.into()],
            unique: false,
        }],
        runtime_options: Default::default(),
    }
}

/// Every index name the catalog holds on `schema.table`, excluding the PK's implicit
/// index (created by the PRIMARY KEY clause, never by a standalone CREATE INDEX).
async fn index_names(session: &PgDevSession, schema: &str, table: &str) -> Vec<String> {
    use zero_migrate::driver::SqlSession;
    session
        .query(
            "SELECT ic.relname AS name FROM pg_index x \
             JOIN pg_class ic ON ic.oid = x.indexrelid \
             JOIN pg_class c ON c.oid = x.indrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND NOT x.indisprimary \
             ORDER BY ic.relname",
            &[schema.into(), table.into()],
        )
        .await
        .expect("read pg_index")
        .iter()
        .map(|row| row.try_get::<_, String>("name").expect("decode name"))
        .collect()
}

/// Deploy `descs` against a live database and return the post-apply live snapshot.
async fn deploy(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    engine: &MigrationEngine,
    descs: &[CollectionDescriptor],
) -> zero_migrate::SchemaSnapshot {
    let author = author_for(cfg);
    let desired = desired_snapshot(&cfg.project_schema, descs, &effective_policy(cfg))
        .expect("desired_snapshot");
    let live = snapshot_schema(session, &cfg.project_schema)
        .await
        .expect("snapshot live");
    let plan = engine
        .plan_declarative(
            &desired,
            &live,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(cfg),
            &effective_policy(cfg),
        )
        .expect("plan_declarative");
    let backend = PostgresBackend::new_generic(session);
    engine
        .apply_declarative(
            &plan,
            &effective_policy(cfg),
            Approval::Approved,
            &backend,
            cfg,
            "app_test",
        )
        .await
        .expect("apply_declarative");
    snapshot_schema(session, &cfg.project_schema)
        .await
        .expect("snapshot live after deploy")
}

/// ARM A - the defect. An index the DATA PLANE created re-diffs CLEAN against the
/// declarative author's differently-derived name for the same index.
///
/// The live index is created under `schema::query::index_name`, byte-for-byte what
/// the out-of-repo data plane's `registerModel` writes. The desired snapshot derives
/// the same index through `cap_ident_name`. Both names are live-legal and under
/// NAMEDATALEN; they simply disagree. Nothing about the index itself differs, so the
/// plan must be empty and drift must be clean.
#[compio::test]
async fn a_data_plane_named_index_re_diffs_clean() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    let _schemas = ensure_project_schema(&session, &cfg).await;

    let field = long_unique_field();
    let natural = format!("{TABLE}_{field}_key");
    assert_eq!(
        natural.len(),
        61,
        "the natural name must land in the 61..=63 window where the schemes disagree"
    );

    // The data plane's spelling, from the REAL function the data plane calls.
    let data_plane_name = zero_migrate::schema::query::index_name(TABLE, &[field.as_str()], true);
    assert_ne!(
        data_plane_name, natural,
        "above 60 bytes the data plane replaces the tail with its base32 hash"
    );
    assert!(
        data_plane_name.len() <= MAX,
        "the data plane's name must be live-legal, not server-truncated"
    );

    let engine = MigrationEngine::new();
    let desc = unique_descriptor(TABLE, &field);

    // Deploy once so the table and its columns exist and match desired. The engine
    // creates the index under ITS derivation; that is population B, not the one
    // under test.
    deploy(&session, &cfg, &engine, std::slice::from_ref(&desc)).await;
    let engine_named = index_names(&session, &cfg.project_schema, TABLE).await;
    assert_eq!(
        engine_named,
        vec![natural.clone()],
        "the engine names the index through cap_ident_name, which keeps 61 bytes verbatim"
    );

    // Now REPLACE it with the byte-identical index under the data plane's spelling.
    // This is the live shape a project gets when `registerModel` built the index
    // before any migration ran.
    {
        use zero_migrate::driver::SqlSession;
        session
            .batch(&format!(
                "DROP INDEX \"{}\".\"{}\"; \
                 CREATE UNIQUE INDEX \"{}\" ON \"{}\".\"{}\" (\"{}\");",
                cfg.project_schema, natural, data_plane_name, cfg.project_schema, TABLE, field
            ))
            .await
            .expect("recreate the index under the data plane's name");
    }
    let live_names = index_names(&session, &cfg.project_schema, TABLE).await;
    assert_eq!(
        live_names,
        vec![data_plane_name.clone()],
        "the live database now holds exactly the data plane's spelling"
    );

    // Re-plan. Desired and live describe the same index, spelled two ways.
    let author = author_for(&cfg);
    let desired = desired_snapshot(
        &cfg.project_schema,
        std::slice::from_ref(&desc),
        &effective_policy(&cfg),
    )
    .expect("desired_snapshot");
    let live_after = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (after)");
    let plan = engine
        .plan_declarative(
            &desired,
            &live_after,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&cfg),
            &effective_policy(&cfg),
        )
        .expect("plan_declarative");
    let statements: Vec<String> = plan
        .plain
        .items
        .iter()
        .map(|i| i.migration.up.clone())
        .collect();

    // The same pairing must reach drift, or dry-run reports the accepted index as
    // BOTH missing and unexpected while the migration plan says there is nothing to
    // do. Both surfaces are asserted together so a failure reports each one.
    let drift = diff_snapshots_with_index_aliases(
        &desired.snapshot,
        &live_after,
        &desired.derived_index_aliases,
    );
    assert!(
        plan.plain.items.is_empty() && drift.is_clean(),
        "an index the data plane named must not churn: the live name is \
         {data_plane_name:?} ({} bytes), the authored name is {natural:?} ({} bytes).\n\
         plan carried: {statements:#?}\ndrift carried: {drift:#?}",
        data_plane_name.len(),
        natural.len(),
    );

    // The acceptance is REPORTED, not silent: without this the plan is
    // indistinguishable from one that stopped managing the index altogether.
    assert_eq!(
        plan.accepted_index_aliases,
        vec![AcceptedIndexAlias {
            table: TABLE.to_string(),
            desired_name: natural.clone(),
            live_name: data_plane_name.clone(),
        }],
        "the plan must surface the alias it accepted"
    );

    // The alias is PROVENANCE the caller opts into, not a global weakening of the
    // comparator: name-only diffing still sees two different names.
    assert!(
        !diff_snapshots(&desired.snapshot, &live_after).is_clean(),
        "diff_snapshots without the alias map must stay name-only"
    );

    drop_schemas(&session, &cfg).await;
}

/// ARM B - the population that works today. An index the ENGINE created on a new
/// table still round-trips clean.
///
/// `diff` hands a new table's index snapshots straight to `render_create_index` with
/// no re-derivation, so live already carries the author's spelling and there is zero
/// drift today. The fix must not buy arm A by breaking this.
#[compio::test]
async fn b_engine_named_index_still_round_trips_clean() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    let _schemas = ensure_project_schema(&session, &cfg).await;

    let field = long_unique_field();
    let natural = format!("{TABLE}_{field}_key");
    let engine = MigrationEngine::new();
    let desc = unique_descriptor(TABLE, &field);

    deploy(&session, &cfg, &engine, std::slice::from_ref(&desc)).await;
    assert_eq!(
        index_names(&session, &cfg.project_schema, TABLE).await,
        vec![natural.clone()],
        "the engine leaves its own derivation on disk"
    );

    let author = author_for(&cfg);
    let desired = desired_snapshot(
        &cfg.project_schema,
        std::slice::from_ref(&desc),
        &effective_policy(&cfg),
    )
    .expect("desired_snapshot");
    let live_after = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (after)");
    let plan = engine
        .plan_declarative(
            &desired,
            &live_after,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&cfg),
            &effective_policy(&cfg),
        )
        .expect("plan_declarative");
    let statements: Vec<String> = plan
        .plain
        .items
        .iter()
        .map(|i| i.migration.up.clone())
        .collect();
    assert!(
        plan.plain.items.is_empty(),
        "re-deploying an engine-created index must stay a no-op; plan carried: \
         {statements:#?}"
    );
    let drift = diff_snapshots(&desired.snapshot, &live_after);
    assert!(drift.is_clean(), "drift must stay clean, got {drift:#?}");
    // Population B pairs on the EXACT name. It must not be reaching clean via the
    // alias, or this arm would stop guarding the population it exists to guard.
    assert!(
        plan.accepted_index_aliases.is_empty(),
        "an engine-created index must pair on its exact name, got {:#?}",
        plan.accepted_index_aliases
    );

    drop_schemas(&session, &cfg).await;
}

/// ARM C - the hole the provenance scope exists to keep closed. An AUTHOR-SUPPLIED
/// index rename still produces a CREATE and a DROP.
///
/// `IndexDescriptor.name` is first-class and copied verbatim into the desired
/// snapshot, so renaming an index while keeping its columns is a real change the
/// differ must still act on. A shape-only match with no provenance scope would
/// silently keep the old name on disk forever.
#[compio::test]
async fn c_author_supplied_rename_still_creates_and_drops() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    let _schemas = ensure_project_schema(&session, &cfg).await;

    let field = "body";
    let old_name = "zz_alias_author_one_idx";
    let new_name = "zz_alias_author_two_idx";
    let engine = MigrationEngine::new();

    deploy(
        &session,
        &cfg,
        &engine,
        &[named_index_descriptor(TABLE, field, old_name)],
    )
    .await;
    assert_eq!(
        index_names(&session, &cfg.project_schema, TABLE).await,
        vec![old_name.to_string()],
        "the author's own index name goes on disk verbatim"
    );

    // Same table, same column, same uniqueness - only the AUTHOR'S name changed.
    let renamed = named_index_descriptor(TABLE, field, new_name);
    let author = author_for(&cfg);
    let desired = desired_snapshot(
        &cfg.project_schema,
        std::slice::from_ref(&renamed),
        &effective_policy(&cfg),
    )
    .expect("desired_snapshot");
    let live_after = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (after)");
    let plan = engine
        .plan_declarative(
            &desired,
            &live_after,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&cfg),
            &effective_policy(&cfg),
        )
        .expect("plan_declarative");
    let statements: Vec<String> = plan
        .plain
        .items
        .iter()
        .map(|i| i.migration.up.clone())
        .collect();
    assert!(
        statements.iter().any(|s| s.contains(new_name)),
        "the renamed index must be CREATEd; plan carried: {statements:#?}"
    );
    assert!(
        statements
            .iter()
            .any(|s| s.contains("DROP INDEX") && s.contains(old_name)),
        "the old index name must be DROPped; plan carried: {statements:#?}"
    );
    // The alias never applies to an author-supplied name, so nothing was accepted.
    assert!(
        plan.accepted_index_aliases.is_empty(),
        "an author-supplied rename must not be swallowed as an alias, got {:#?}",
        plan.accepted_index_aliases
    );

    drop_schemas(&session, &cfg).await;
}

/// ARM D - the ownership question. A NON-OWNER re-declaring a table whose only
/// difference from live is an alias-accepted index name must not be refused as a
/// structural modifier.
///
/// `enforce_ownership` decides "structurally changed" with strict `TableSnapshot`
/// equality, and `IndexSnapshot`'s equality compares the NAME. So the two spellings
/// of one index make the table compare unequal even when the index diff pairs them
/// and emits nothing.
#[compio::test]
async fn d_alias_accepted_no_op_does_not_trip_ownership() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    let _schemas = ensure_project_schema(&session, &cfg).await;

    let field = long_unique_field();
    let natural = format!("{TABLE}_{field}_key");
    let data_plane_name = zero_migrate::schema::query::index_name(TABLE, &[field.as_str()], true);
    let engine = MigrationEngine::new();

    let mut owner_desc = unique_descriptor(TABLE, &field);
    owner_desc.owner_app = "app_test".into();
    deploy(&session, &cfg, &engine, std::slice::from_ref(&owner_desc)).await;

    {
        use zero_migrate::driver::SqlSession;
        session
            .batch(&format!(
                "DROP INDEX \"{}\".\"{}\"; \
                 CREATE UNIQUE INDEX \"{}\" ON \"{}\".\"{}\" (\"{}\");",
                cfg.project_schema, natural, data_plane_name, cfg.project_schema, TABLE, field
            ))
            .await
            .expect("recreate the index under the data plane's name");
    }

    // Both apps declare the same shape, so the owner is the lexicographically
    // smallest declarer - `app_test`. `app_zzz` is a confirmed NON-owner.
    let mut other_desc = unique_descriptor(TABLE, &field);
    other_desc.owner_app = "app_zzz".into();
    let desired = desired_snapshot(
        &cfg.project_schema,
        &[owner_desc, other_desc],
        &effective_policy(&cfg),
    )
    .expect("desired_snapshot");
    assert_eq!(
        desired.ownership.get(TABLE).map(String::as_str),
        Some("app_test"),
        "the owner must be the other app for this to be a non-owner deploy"
    );

    let live_after = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (after)");
    let non_owner = DeclarativeAuthor::new(cfg.project_schema.clone(), "app_zzz");
    let planned = engine.plan_declarative(
        &desired,
        &live_after,
        &HashMap::new(),
        &non_owner,
        &[],
        &guard_cfg(&cfg),
        &effective_policy(&cfg),
    );

    let plan = planned.expect("a non-owner whose declaration matches live must not be refused");
    assert!(
        plan.plain.items.is_empty(),
        "the non-owner's plan must be empty"
    );

    drop_schemas(&session, &cfg).await;
}
