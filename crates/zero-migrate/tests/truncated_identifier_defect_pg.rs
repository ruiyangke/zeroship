//! Pin the lying journal a PostgreSQL identifier truncation produces.
//!
//! THIS SUITE ASSERTS A KNOWN DEFECT THAT IS NOT YET FIXED. Every assertion below
//! describes what the engine does today, not what it should do.
//!
//! PostgreSQL caps identifiers at 63 bytes (NAMEDATALEN) and truncates anything longer
//! with only a NOTICE. The engine keeps the AUTHORED name while the catalog keeps the
//! TRUNCATED one. A guarded drop probes the AUTHORED name against the INTROSPECTED
//! snapshot, never matches the truncated catalog name, decides the work is already
//! done, skips the statement, and journals it COMPLETED. The object survives and the
//! migration history says it was removed.
//!
//! Commit ddd679d bounded every authored identifier PostgreSQL would truncate, but
//! that bound is a LOAD-time gate: `IrAuthor::lower`, `lower_steps` and `lower_plan`
//! never call `validate_ir_scoped`, so the bound narrows the entry point without
//! removing the hazard. Lowering an over-long name still reproduces the whole chain,
//! which is exactly why it is worth pinning.
//!
//! IF THIS TEST STARTS FAILING, the most likely cause is that the hazard was closed at
//! the lower seam or at the existence-probe seam. In that case INVERT the assertions to
//! assert the correct behaviour (the drop is refused, or the guard resolves against the
//! truncated catalog name and the object is actually removed). Do not delete it.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`; skips cleanly when unset.

mod support;

use support::PgDevSession;

use zero_migrate::model::ir::ExistenceGuard;
use zero_migrate::render::existence_probe::{decide, GuardVerdict};
use zero_migrate::{
    apply, snapshot_schema, Approval, ExecutorConfig, IrAuthor, LiveSchema, MigrationIr, Op, Phase,
    SqlDialect,
};

/// PostgreSQL's NAMEDATALEN-derived identifier bound, in bytes.
const MAX: usize = 63;

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

/// The UNIQUE constraint names the catalog actually holds for `schema.t`.
async fn unique_constraint_names(session: &PgDevSession, schema: &str) -> Vec<String> {
    use zero_migrate::driver::SqlSession;
    session
        .query(
            "SELECT con.conname FROM pg_constraint con \
             JOIN pg_class c ON c.oid = con.conrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = 't' AND con.contype = 'u' \
             ORDER BY con.conname",
            &[schema.into()],
        )
        .await
        .expect("read pg_constraint")
        .iter()
        .map(|row| row.try_get::<_, String>("conname").expect("decode conname"))
        .collect()
}

/// A guarded drop of a name PostgreSQL truncated reports success, journals COMPLETED,
/// and leaves the constraint in `pg_constraint`.
///
/// This is the defect, asserted end to end: the catalog holds the truncated name, the
/// guard verdict is the satisfied no-op, apply reports the migration applied, the
/// journal records phase Completed, AND the constraint is still present afterwards.
/// The last assertion is the point of the suite; the ones before it show why.
#[compio::test]
async fn a_truncated_constraint_name_makes_a_guarded_drop_journal_a_lie() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    // One byte over NAMEDATALEN, all ASCII, so bytes and characters agree and the
    // truncation is exactly the first 63 bytes.
    let authored = format!("zm_truncated_constraint_{}", "x".repeat(40));
    assert_eq!(
        authored.len(),
        MAX + 1,
        "the authored name must be one byte over the PostgreSQL cap"
    );
    let truncated: String = authored.chars().take(MAX).collect();

    session
        .batch(&format!(
            "CREATE TABLE \"{s}\".t (c text NOT NULL); \
             ALTER TABLE \"{s}\".t ADD CONSTRAINT \"{authored}\" UNIQUE (c)",
            s = cfg.project_schema
        ))
        .await
        .expect("create the table and its over-long constraint");

    // The server truncated with only a NOTICE: the catalog holds a name the authored
    // one is never equal to.
    let before = unique_constraint_names(&session, &cfg.project_schema).await;
    assert_eq!(
        before,
        vec![truncated.clone()],
        "PostgreSQL must have truncated the authored name to {MAX} bytes"
    );
    assert!(
        !before.contains(&authored),
        "the authored 64-byte name is not what the catalog holds"
    );

    let ir = MigrationIr {
        ir_version: 2,
        name: "drop_truncated_constraint".into(),
        owner_app: "app_test".into(),
        ops: vec![Op::DropConstraint {
            table: "t".into(),
            name: authored.clone(),
            schema: None,
            existence_guard: Some(ExistenceGuard::IfExists),
        }],
        flags: Default::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    };
    let author = IrAuthor::new(
        &cfg.project_schema,
        "app_test",
        SqlDialect::Postgres,
        &support::no_inject(&cfg.project_schema),
    );

    // The load-time bound from ddd679d does not run here: lower accepts the over-long
    // name and carries it verbatim into the rendered DDL.
    let migrations = author
        .lower(&ir, &LiveSchema::default())
        .expect("the guarded drop lowers despite the over-long name");
    assert_eq!(migrations.len(), 1, "one migration lowers from one op");
    assert!(
        migrations[0].up.contains(&authored),
        "the lowered DDL carries the authored name, not the truncated one: {}",
        migrations[0].up
    );

    // The verdict the executor will compute, taken against the same live catalog.
    let live = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot the live schema");
    let probe = migrations[0]
        .existence_guard
        .as_ref()
        .expect("the guarded drop carries a probe");
    let verdict = decide(probe, &live, SqlDialect::Postgres);
    assert_eq!(
        verdict,
        GuardVerdict::SatisfiedNoop,
        "the authored name does not match the truncated catalog name, so the guard \
         reads the drop as already done"
    );

    let out = apply(&session, &cfg, &migrations, Approval::Approved, "app_test")
        .await
        .expect("the guarded drop applies");
    assert_eq!(
        out.applied.len(),
        1,
        "apply reports the migration applied: {out:?}"
    );
    assert!(
        out.skipped.is_empty(),
        "the skip happens inside the migration, so nothing is reported skipped: {out:?}"
    );

    let journal = zero_migrate::applied(&session, &cfg)
        .await
        .expect("read the journal");
    assert!(
        journal
            .iter()
            .any(|e| e.version == out.applied[0] && e.phase == Phase::Completed),
        "the journal records the drop as completed: {journal:?}"
    );

    // The defect. The journal says the constraint was dropped; the catalog disagrees.
    let after = unique_constraint_names(&session, &cfg.project_schema).await;
    assert_eq!(
        after,
        vec![truncated],
        "the constraint the journal says was dropped is still in pg_constraint"
    );

    drop_schemas(&session, &cfg).await;
}
