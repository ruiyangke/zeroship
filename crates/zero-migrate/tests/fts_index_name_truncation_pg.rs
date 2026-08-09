//! A collection whose FTS index name overflows NAMEDATALEN no longer churns that
//! index CREATE/DROP on every deploy.
//!
//! PostgreSQL caps identifiers at 63 bytes and truncates anything longer at CREATE with
//! only a NOTICE. The FTS index name is derived as `<collection>__fts_idx`, so a
//! collection name over 54 bytes produces a name the server cannot hold. The desired
//! snapshot kept the authored spelling, live introspection read the truncated one, and
//! the index diff keys on NAME - so every re-deploy saw the authored name missing and
//! the truncated name unexpected, and emitted a CREATE (a no-op, the truncated relation
//! already exists) plus a DROP (which really removes the index). The pair nets to
//! deleting the full-text index, and the DROP is not gated: `render_drop_index` stamps
//! `destructive_flags()` only when the index is UNIQUE, and an FTS GIN index is not.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`; skips cleanly when unset.

mod support;

use std::collections::HashMap;

use support::PgDevSession;

use zero_migrate::{
    desired_snapshot, snapshot_schema, Approval, CollectionDescriptor, DeclarativeAuthor,
    EffectivePolicy, ExecutorConfig, FieldDescriptor, GuardConfig, MigrationEngine,
    PostgresBackend, SqlDialect,
};

/// PostgreSQL's NAMEDATALEN-derived identifier bound, in bytes.
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

fn guard_cfg(cfg: &ExecutorConfig) -> GuardConfig {
    GuardConfig::from_policy(
        support::no_inject(&cfg.project_schema),
        SqlDialect::Postgres,
    )
}

fn author_for(cfg: &ExecutorConfig) -> DeclarativeAuthor {
    DeclarativeAuthor::new(cfg.project_schema.clone(), "app_test")
}

/// A collection with one `.fts()` text field, named so that `<name>__fts_idx` overflows
/// NAMEDATALEN.
fn fts_descriptor(name: &str) -> CollectionDescriptor {
    CollectionDescriptor {
        name: name.into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "body".into(),
            ty: "string".into(),
            required: true,
            fts: true,
            ..Default::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    }
}

/// The GIN index names the catalog actually holds on `schema.table`.
async fn gin_index_names(session: &PgDevSession, schema: &str, table: &str) -> Vec<String> {
    use zero_migrate::driver::SqlSession;
    session
        .query(
            "SELECT ic.relname AS name FROM pg_index x \
             JOIN pg_class ic ON ic.oid = x.indexrelid \
             JOIN pg_class c ON c.oid = x.indrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_am am ON am.oid = ic.relam \
             WHERE n.nspname = $1 AND c.relname = $2 AND am.amname = 'gin' \
             ORDER BY ic.relname",
            &[schema.into(), table.into()],
        )
        .await
        .expect("read pg_index")
        .iter()
        .map(|row| row.try_get::<_, String>("name").expect("decode name"))
        .collect()
}

/// A deployed collection whose FTS index name overflows NAMEDATALEN re-diffs CLEAN.
///
/// Asserted end to end against a live server: deploy the collection, re-introspect, and
/// plan again. The second plan must be empty. Before the fix it carried a CREATE INDEX
/// naming the 66-byte authored spelling and a DROP INDEX naming the 63-byte spelling the
/// server truncated to - a pair that nets to deleting the full-text index, ungated.
#[compio::test]
async fn an_over_long_fts_index_name_re_diffs_clean() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    // 57 bytes of collection name: the table name itself fits, but the derived
    // `<name>__fts_idx` is 57 + 9 = 66 bytes, three over what the server can hold.
    let collection = format!("zz_fts_{}", "c".repeat(50));
    assert_eq!(
        collection.len(),
        57,
        "the collection name must fit in NAMEDATALEN"
    );
    let authored_idx = format!("{collection}__fts_idx");
    assert_eq!(
        authored_idx.len(),
        MAX + 3,
        "the derived FTS index name must overflow NAMEDATALEN"
    );
    let truncated_idx: String = authored_idx.chars().take(MAX).collect();

    let engine = MigrationEngine::new();
    let author = author_for(&cfg);
    let desc = fts_descriptor(&collection);
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
        .expect("apply_declarative create the collection");

    // The catalog holds exactly one GIN index, spelled as PostgreSQL's own truncation
    // of the authored name. Pinning the spelling is what proves the remedy MIMICS the
    // server rather than renaming: a hash-tail cap would put a different name here and
    // rename an index that already exists on live databases.
    let live_names = gin_index_names(&session, &cfg.project_schema, &collection).await;
    assert_eq!(
        live_names,
        vec![truncated_idx.clone()],
        "the deploy leaves one GIN index under the server's own truncated spelling"
    );

    // Re-introspect and plan again. Desired and live describe the same database, so
    // the second plan is a no-op.
    let live_after = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot live (after)");
    let plan2 = engine
        .plan_declarative(
            &desired,
            &live_after,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(&cfg),
            &effective_policy(&cfg),
        )
        .expect("plan_declarative 2");
    let statements: Vec<String> = plan2
        .plain
        .items
        .iter()
        .map(|i| i.migration.up.clone())
        .collect();
    assert!(
        plan2.plain.items.is_empty(),
        "re-deploying the same collection must be a no-op; the live GIN index is \
         {live_names:?}, the authored name is {authored_idx:?} ({} bytes) and the \
         server's own spelling is {truncated_idx:?}, and the plan carried: {statements:#?}",
        authored_idx.len(),
    );

    drop_schemas(&session, &cfg).await;
}
